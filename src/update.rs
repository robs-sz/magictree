//! Keeping the installed magictree current.
//!
//! `magictree update` replaces this binary with the release published for the
//! platform it was built for, and every other command ends by naming a release
//! that has landed since it was installed. Both read the same answer, from the
//! same place: the GitHub release for this repository, whose binary assets are
//! `magictree-<version>-<target>.tar.gz`.

use crate::config::Config;
use crate::paths::Paths;
use anyhow::{anyhow, bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Releases are published from here.
const REPO: &str = "robs-sz/magictree";

/// The triple this binary was built for, baked in by `build.rs`. Every release
/// artifact is named after it, so an update can only ever install the build
/// belonging to the platform that asked.
const TARGET: &str = env!("MAGICTREE_TARGET");

/// The version this binary is, which is what a notice compares against.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a check that found a release is trusted: at most one request a
/// day, and a release is named the day after it lands.
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a check that reached nothing is trusted, so a machine with no
/// network asks rarely instead of on every command. Short, because a link that
/// was down is a link that came back.
const RETRY_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// The check runs on the way out of a command the user asked for, so it gives
/// up quickly: an unreachable GitHub costs a notice, never a wait.
const CHECK_TIMEOUT: Duration = Duration::from_secs(2);

/// `update` was asked for by name, so it may wait longer for its answer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A release artifact is a couple of megabytes.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// The latest release GitHub reports, and the assets it carries.
#[derive(Debug, Deserialize)]
pub struct Release {
    /// The tag the release is published under, `v0.1.3`.
    pub tag_name: String,
    /// The release page, which errors point at.
    pub html_url: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    /// `sha256:<hex>` where the API reports one; releases published before
    /// that field existed carry none, and are installed unverified.
    digest: Option<String>,
}

impl Release {
    /// The version the tag names: `v0.1.3` is `0.1.3`.
    pub fn version(&self) -> &str {
        self.tag_name.strip_prefix('v').unwrap_or(&self.tag_name)
    }

    /// The name of the artifact this platform installs from.
    pub fn artifact(&self) -> Result<&str> {
        self.asset().map(|asset| asset.name.as_str())
    }

    fn asset(&self) -> Result<&Asset> {
        let suffix = format!("-{TARGET}.tar.gz");
        self.assets
            .iter()
            .find(|asset| asset.name.ends_with(&suffix))
            .ok_or_else(|| {
                anyhow!(
                    "release {} carries no build for {TARGET}\n\n{}",
                    self.tag_name,
                    self.html_url
                )
            })
    }
}

/// Ask GitHub for the latest release.
pub fn latest() -> Result<Release> {
    latest_within(REQUEST_TIMEOUT)
}

fn latest_within(timeout: Duration) -> Result<Release> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body = agent(timeout)
        .get(&url)
        .set("Accept", "application/vnd.github+json")
        .call()
        .with_context(|| format!("asking {REPO} for its latest release"))?
        .into_string()
        .with_context(|| format!("reading the answer from {url}"))?;
    serde_json::from_str(&body).with_context(|| format!("parsing the release from {url}"))
}

/// Replace this binary with the build the release carries, and say where it
/// went.
///
/// The download is staged next to the binary that is running and moved into
/// place only once it has proved that it starts and reports the version it
/// should, so an interrupted, wrong or truncated update leaves the installed
/// magictree exactly as it was.
pub fn install(release: &Release) -> Result<PathBuf> {
    let asset = release.asset()?;
    let exe = running_binary()?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("{} has no directory to install into", exe.display()))?;
    let pid = std::process::id();
    let archive = dir.join(format!(".magictree-{pid}.tar.gz"));
    let staging = dir.join(format!(".magictree-{pid}"));
    let staged = staging.join("magictree");
    let result = (|| {
        download(
            &asset.browser_download_url,
            &archive,
            asset.digest.as_deref(),
        )?;
        unpack(&archive, &staging)?;
        verify_runs(&staged, release.version())?;
        swap(&staged, &exe)
    })();
    let _ = std::fs::remove_file(&archive);
    let _ = std::fs::remove_dir_all(&staging);
    result.map(|()| exe)
}

/// The file that is running, with any symlink resolved: an update replaces the
/// binary itself, never a link someone else put there.
pub fn running_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the running magictree")?;
    Ok(exe.canonicalize().unwrap_or(exe))
}

/// Fetch the artifact. When the release publishes a digest, the bytes are
/// hashed as they are written, so a download that is not the published one
/// fails before anything is unpacked.
fn download(url: &str, to: &Path, digest: Option<&str>) -> Result<()> {
    let response = agent(DOWNLOAD_TIMEOUT)
        .get(url)
        .call()
        .with_context(|| format!("downloading {url}"))?;
    let mut file = File::create(to).map_err(|error| write_error(to, &error))?;
    let mut reader = response.into_reader();
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .with_context(|| format!("reading {url}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])
            .map_err(|error| write_error(to, &error))?;
    }
    file.flush().map_err(|error| write_error(to, &error))?;
    if let Some(published) = digest.and_then(|digest| digest.strip_prefix("sha256:")) {
        let downloaded = format!("{:x}", hasher.finalize());
        ensure!(
            downloaded.eq_ignore_ascii_case(published),
            "the {TARGET} build does not match the digest the release publishes\n\n  published  \
             {published}\n  downloaded {downloaded}"
        );
    }
    Ok(())
}

/// Unpack the release into a staging directory. The archive holds one member,
/// the binary itself, and it is the only thing taken from it.
fn unpack(archive: &Path, staging: &Path) -> Result<()> {
    std::fs::create_dir_all(staging).map_err(|error| write_error(staging, &error))?;
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(staging)
        .arg("magictree")
        .status()
        .context("running tar, which unpacks the release")?;
    ensure!(
        status.success(),
        "tar could not unpack {}",
        archive.display()
    );
    ensure!(
        staging.join("magictree").is_file(),
        "{} carries no magictree binary",
        archive.display()
    );
    Ok(())
}

/// Run the staged binary before it replaces anything: a build for another
/// architecture, or a file that is not a program at all, fails here rather
/// than after the working magictree has been overwritten.
fn verify_runs(binary: &Path, version: &str) -> Result<()> {
    let output = match Command::new(binary).arg("--version").output() {
        Ok(output) => output,
        Err(error) => {
            bail!("the downloaded build will not run ({error}); keeping the installed magictree")
        }
    };
    let reported = String::from_utf8_lossy(&output.stdout);
    let reported = reported.split_whitespace().next_back().unwrap_or("");
    ensure!(
        output.status.success() && reported == version,
        "the downloaded build reports {reported:?} instead of {version}; keeping the installed \
         magictree"
    );
    Ok(())
}

/// Move the staged binary over the running one. A rename inside the directory
/// is atomic, and the process that is running the old file keeps running it.
fn swap(staged: &Path, exe: &Path) -> Result<()> {
    std::fs::rename(staged, exe).map_err(|error| write_error(exe, &error))
}

/// Mention a newer release, at most once a day, on stderr.
///
/// This runs after a command has finished, so it can change nothing about what
/// that command did: the notice is the last line, stdout — the part a script
/// reads — is untouched, and every failure here is silence.
pub fn notice() {
    if disabled() {
        return;
    }
    let Ok(paths) = Paths::new() else {
        return;
    };
    let Ok(config) = Config::load(&paths) else {
        return;
    };
    if !config.check_for_updates {
        return;
    }
    let Some(version) = checked_version(&paths) else {
        return;
    };
    if is_newer(&version, VERSION) {
        eprintln!("magictree {version} is available (this is {VERSION}); run `magictree update`");
    }
}

/// Record what `update` learned, so the next command does not ask GitHub for
/// an answer the user has just seen.
pub fn remember(version: &str) {
    let Ok(paths) = Paths::new() else {
        return;
    };
    let check = Check {
        checked_at: now(),
        version: Some(version.to_string()),
    };
    write_check(&paths.update_check_file(), &check);
}

/// The environment switch, for a machine that never reaches GitHub and for
/// anything that has no business asking: set to anything but `0`, it is off.
fn disabled() -> bool {
    std::env::var_os("MAGICTREE_NO_UPDATE_CHECK").is_some_and(|value| value != "0")
}

/// The latest version: from the cache while the last check is still trusted,
/// from GitHub once it is not. `None` whenever there is nothing certain to say.
fn checked_version(paths: &Paths) -> Option<String> {
    let path = paths.update_check_file();
    if let Some(cached) = read_check(&path) {
        if fresh(&cached) {
            return cached.version;
        }
    }
    let version = latest_within(CHECK_TIMEOUT)
        .ok()
        .map(|release| release.version().to_string());
    let check = Check {
        checked_at: now(),
        version: version.clone(),
    };
    write_check(&path, &check);
    version
}

/// What the last check found, kept so the next command does not ask again.
#[derive(Debug, Serialize, Deserialize)]
struct Check {
    /// Unix seconds when the check ran, whether or not it found anything.
    checked_at: u64,
    /// The version the latest release carried, when one was found at all.
    version: Option<String>,
}

fn read_check(path: &Path) -> Option<Check> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Best effort: a cache that cannot be written only costs another check.
fn write_check(path: &Path, check: &Check) {
    let Some(dir) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    if let Ok(raw) = serde_json::to_string(check) {
        let _ = std::fs::write(path, raw);
    }
}

/// Whether the last check is still worth trusting. A check that found a
/// release is good for a day; one that found nothing is worth retrying in half
/// an hour. A timestamp from the future is a clock that moved, not an answer.
fn fresh(check: &Check) -> bool {
    let now = now();
    if check.checked_at > now {
        return false;
    }
    let interval = match check.version {
        Some(_) => CHECK_INTERVAL,
        None => RETRY_INTERVAL,
    };
    now - check.checked_at < interval.as_secs()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Whether `candidate` is a later release than `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (triple(candidate), triple(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

/// `0.1.3` as a comparable triple. Anything else — a version that is not one,
/// an answer that is a branch name — is no answer, and a notice is only worth
/// printing when the answer is certain.
fn triple(version: &str) -> Option<(u64, u64, u64)> {
    let head = version.trim();
    let head = head.strip_prefix('v').unwrap_or(head);
    let head = head.split(['-', '+']).next()?;
    let mut parts = head.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// An HTTP client for one call, with the timeouts this module works in.
fn agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .user_agent(concat!("magictree/", env!("CARGO_PKG_VERSION")))
        .timeout_connect(Duration::from_secs(3))
        .timeout(timeout)
        .build()
}

/// Where an update writes is where the binary lives, and that directory often
/// belongs to another user; name it and what to do about it instead of leaving
/// a bare "permission denied".
fn write_error(path: &Path, error: &std::io::Error) -> anyhow::Error {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        anyhow!(
            "writing {}: permission denied\n\nmagictree is installed in a directory this user \
             cannot write to; run `sudo magictree update`, or keep magictree somewhere you own \
             (for example ~/.local/bin)",
            path.display()
        )
    } else {
        anyhow!("writing {}: {error}", path.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_later_release_is_newer() {
        assert!(is_newer("0.1.3", "0.1.2"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
    }

    #[test]
    fn the_same_release_or_an_earlier_one_is_not() {
        // An installed build that is ahead of the latest release — a checkout
        // built by hand — is left alone rather than announced as out of date.
        assert!(!is_newer("0.1.2", "0.1.2"));
        assert!(!is_newer("0.1.1", "0.1.2"));
        assert!(!is_newer("0.0.0", "0.1.2"));
    }

    #[test]
    fn a_tag_compares_as_the_version_it_names() {
        assert!(is_newer("v0.1.3", "0.1.2"));
        assert!(is_newer("0.1.3", "v0.1.2"));
    }

    #[test]
    fn anything_that_is_not_a_version_is_never_newer() {
        assert!(!is_newer("latest", "0.1.2"));
        assert!(!is_newer("0.1", "0.1.2"));
        assert!(!is_newer("0.1.2.3", "0.1.2"));
        assert!(!is_newer("", "0.1.2"));
        assert!(!is_newer("0.1.3", "dev"));
    }

    #[test]
    fn a_check_that_found_a_release_is_trusted_for_a_day() {
        let found = |ago: u64| Check {
            checked_at: now() - ago,
            version: Some("0.1.3".to_string()),
        };
        assert!(fresh(&found(0)));
        assert!(fresh(&found(CHECK_INTERVAL.as_secs() - 1)));
        assert!(!fresh(&found(CHECK_INTERVAL.as_secs())));
    }

    #[test]
    fn a_check_that_found_nothing_is_retried_sooner() {
        let missed = |ago: u64| Check {
            checked_at: now() - ago,
            version: None,
        };
        assert!(fresh(&missed(0)));
        assert!(fresh(&missed(RETRY_INTERVAL.as_secs() - 1)));
        assert!(!fresh(&missed(RETRY_INTERVAL.as_secs())));
    }

    #[test]
    fn a_clock_that_moved_backwards_does_not_park_the_check() {
        let ahead = Check {
            checked_at: now() + 60,
            version: Some("0.1.3".to_string()),
        };
        assert!(!fresh(&ahead));
    }

    #[test]
    fn a_fresh_check_is_answered_without_asking_github() {
        // The daily budget is the whole point: a command that finds a recent
        // answer must read it and leave it, not write a new one.
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = Paths {
            state_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        };
        let file = paths.update_check_file();
        write_check(
            &file,
            &Check {
                checked_at: now(),
                version: Some("0.1.2".to_string()),
            },
        );
        let before = std::fs::metadata(&file)
            .and_then(|meta| meta.modified())
            .expect("the cache was written");

        assert_eq!(checked_version(&paths).as_deref(), Some("0.1.2"));

        let after = std::fs::metadata(&file)
            .and_then(|meta| meta.modified())
            .expect("the cache is still there");
        assert_eq!(before, after, "a fresh check was asked again");
    }
}
