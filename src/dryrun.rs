use crate::ctx::Ctx;
use crate::manifest::{Expose, NodeKind, RunStep, Runtime};
use crate::ports;
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::Path;

/// One expose line in the dry-run plan: container-side target plus any fixed
/// host port the manifest demands for it.
struct PreviewMapping {
    target: Option<u16>,
    fixed_host: Option<u16>,
}

/// One compose service in the dry-run plan: service name, exposure, and the
/// port mappings that would replace whatever the repository declares.
struct PlannedComposeService {
    name: String,
    expose: Expose,
    mappings: Vec<PreviewMapping>,
}

/// A compose file plus its project, with the services it would start.
type PlannedComposeGroup = BTreeMap<String, Vec<PlannedComposeService>>;

/// A host service in the dry-run plan: name, command, any fixed host port, and
/// the variable that will carry the allocated port.
struct PlannedHostService {
    name: String,
    command: String,
    fixed_host: Option<u16>,
    has_health: bool,
    port_variable: Option<String>,
}
pub fn up(ctx: &Ctx, selection: &[usize], mode: Option<ports::PortMode>) -> Result<()> {
    println!(
        "dry run: nothing below is executed or written\n\nworktree root  {}",
        ctx.repo.worktree_root.display()
    );
    println!("repo key       {}", ctx.repo.key());
    println!(
        "worktree id    {} (slug {})",
        ctx.repo.worktree_id(),
        ctx.slug
    );
    println!(
        "port range     {}..{} stride {} (no ports are claimed in a dry run)",
        ctx.config.port_range_start, ctx.config.port_range_end, ctx.config.port_stride
    );
    println!("state dir      {}", ctx.paths.state_dir.display());
    println!("runtime dir    {}/", ctx.runtime_dir.display());
    println!(
        "ports          {}",
        match mode {
            Some(ports::PortMode::Declared) =>
                "declared: the ports the manifest declares, as the primary checkout uses them",
            Some(ports::PortMode::Block) =>
                "generated: every port from this worktree's block, ignoring declared ports",
            None => "the ones this worktree defaults to",
        }
    );

    println!("\n# environment (ports marked <allocated> are chosen on the first real up)");
    let plan = ctx.preview_env(mode)?;
    for (key, value) in &plan.vars {
        println!("{key}={value}");
    }

    let targets = ctx.bootstrap_targets(selection);
    if targets
        .iter()
        .any(|(_, _, steps)| !steps.sync.is_empty() || !steps.run.is_empty())
    {
        println!("\n# bootstrap");
    }
    for (dir, _app, steps) in &targets {
        if steps.sync.is_empty() && steps.run.is_empty() {
            continue;
        }
        println!("({})", display_relative(&ctx.repo.worktree_root, dir));
        let prefix = dir
            .strip_prefix(&ctx.repo.worktree_root)
            .unwrap_or(Path::new(""));
        for path in &steps.sync {
            let source = ctx.repo.main_worktree_root().join(prefix).join(path);
            let destination = dir.join(path);
            if destination.exists() {
                println!("  sync  {path}: already present, would skip");
            } else if source.exists() && !ctx.repo.is_main_worktree() {
                println!("  sync  {path}: symlink from {}", source.display());
            } else if source.exists() {
                println!("  sync  {path}: present in this worktree, would skip");
            } else {
                println!("  sync  {path}: missing everywhere, would report and continue");
            }
        }
        for step in &steps.run {
            print_step(step);
        }
    }

    let mut groups: PlannedComposeGroup = BTreeMap::new();
    let mut host: Vec<PlannedHostService> = Vec::new();
    let mut jobs: Vec<(String, String)> = Vec::new();
    let requests = ctx.port_requests(selection);

    for &index in selection {
        let node = &ctx.nodes[index];
        match &node.kind {
            NodeKind::Job(job) => jobs.push((node.qual(), job.run.clone())),
            NodeKind::Service(service) => match node.runtime() {
                Some(Runtime::Compose) => {
                    let reference = service.compose.as_ref().expect("validated reference");
                    let key = format!(
                        "{} (project {})",
                        display_relative(
                            &ctx.repo.worktree_root,
                            &node.manifest_dir.join(&reference.file)
                        ),
                        ctx.project_for(node)
                    );
                    let mut mappings = Vec::new();
                    let declared = service.ports();
                    let multiple = declared.len() > 1;
                    for port in &declared {
                        let name = if multiple {
                            port.name.as_deref().filter(|name| *name != node.id)
                        } else {
                            None
                        };
                        let request = requests
                            .iter()
                            .find(|request| request.name == ctx.port_key(node, name));
                        mappings.push(PreviewMapping {
                            target: port.target,
                            fixed_host: request
                                .and_then(|request| request.require.or(request.prefer)),
                        });
                    }
                    groups.entry(key).or_default().push(PlannedComposeService {
                        name: reference.service.clone(),
                        expose: service.expose,
                        mappings,
                    });
                }
                _ => {
                    let declared = service.ports();
                    let fixed = declared
                        .iter()
                        .find_map(|port| port.require.or(port.prefer));
                    let variable = declared
                        .iter()
                        .find_map(|port| port.env.clone())
                        .unwrap_or_else(|| "PORT".to_string());
                    host.push(PlannedHostService {
                        name: node.qual(),
                        command: node.command().unwrap_or_default(),
                        fixed_host: fixed,
                        has_health: service.health.is_some(),
                        port_variable: Some(variable),
                    });
                }
            },
        }
    }

    if !groups.is_empty() {
        println!("\n# compose");
        for (file, services) in &groups {
            println!("{file}");
            println!(
                "  would write {}/override-<hash>.yml and run:",
                ctx.runtime_dir.display()
            );
            for service in services {
                match service.expose {
                    Expose::None => {
                        println!("    {}: ports: !reset [] (no host port)", service.name)
                    }
                    Expose::Port => {
                        println!("    {}: ports: !override", service.name);
                        for mapping in &service.mappings {
                            let host_port = match mapping.fixed_host {
                                Some(port) => format!("{port} (fixed)"),
                                None => "<allocated>".to_string(),
                            };
                            let target = match mapping.target {
                                Some(port) => port.to_string(),
                                None => "<port.target>".to_string(),
                            };
                            println!("      - 127.0.0.1:{host_port}:{target}");
                        }
                    }
                }
            }
            println!("  docker compose -f <compose file> -f <override> -p <project> up -d");
        }
    }

    if !host.is_empty() {
        println!("\n# host processes");
        for service in &host {
            println!("{}", service.name);
            println!("  command  {}", service.command);
            println!(
                "  port     {}",
                match service.fixed_host {
                    Some(port) => format!("{port} when free, otherwise an allocated port"),
                    None => "allocated on first real up".to_string(),
                }
            );
            if let Some(variable) = &service.port_variable {
                println!("  via      {variable} (the process must read this variable)");
            }
            println!(
                "  health   {}",
                if service.has_health {
                    "probe declared"
                } else {
                    "none"
                }
            );
            println!(
                "  would write {}/run/{}.pid, {}/log/{}.log",
                ctx.runtime_dir.display(),
                service.name.replace(':', "_"),
                ctx.runtime_dir.display(),
                service.name.replace(':', "_")
            );
        }
    }

    if !jobs.is_empty() {
        println!("\n# jobs");
        for (name, command) in &jobs {
            println!("{name}  {command}");
        }
    }

    println!("\n# start order");
    for (position, &index) in selection.iter().enumerate() {
        println!("{}. {}", position + 1, ctx.nodes[index].qual());
    }

    if targets.iter().any(|(_, _, steps)| !steps.after.is_empty()) {
        println!("\n# after (once every service above is healthy)");
        for (dir, _app, steps) in &targets {
            if steps.after.is_empty() {
                continue;
            }
            println!("({})", display_relative(&ctx.repo.worktree_root, dir));
            for step in &steps.after {
                print_step(step);
            }
        }
    }

    println!("\n# would also");
    println!("- write {}/env", ctx.runtime_dir.display());
    println!(
        "- claim a port block under {}/blocks",
        ctx.paths.state_dir.display()
    );
    Ok(())
}

/// One bootstrap step in the plan, with what decides whether it runs.
fn print_step(step: &RunStep) {
    let mut marker = if step.inputs().is_empty() {
        "always runs".to_string()
    } else {
        format!("skipped when unchanged: {}", step.inputs().join(", "))
    };
    if step.asks() {
        marker.push_str(", asks before running");
    }
    println!("  run   {}   ({marker})", step.command());
}

fn display_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|value| {
            let text = value.display().to_string();
            if text.is_empty() {
                ".".to_string()
            } else {
                text
            }
        })
        .unwrap_or_else(|_| path.display().to_string())
}
