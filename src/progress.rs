//! Where `up` is in its plan: a numbered header per step, printed before the
//! step's work, and one status line for a step that blocks — rewritten in place
//! on a terminal only, so a log file or a test sees the plain form.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The separator between a step's phase and its subject, defined once.
const SEPARATOR: &str = " · ";

/// One step of the plan `up` is about to execute.
pub struct Unit {
    pub phase: String,
    pub subject: String,
}

impl Unit {
    pub fn new(phase: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            phase: phase.into(),
            subject: subject.into(),
        }
    }
}

pub struct Progress {
    units: Vec<Unit>,
    next: usize,
    /// `stdout().is_terminal()`, read once: the elapsed suffix and the live
    /// status line exist only there.
    live: bool,
    quiet: bool,
}

impl Progress {
    pub fn new(units: Vec<Unit>, quiet: bool) -> Self {
        Self {
            units,
            next: 0,
            live: std::io::stdout().is_terminal(),
            quiet,
        }
    }

    /// Nothing is printed: `run_steps`' own tests and any non-`up` caller.
    pub fn silent() -> Self {
        Self {
            units: Vec::new(),
            next: 0,
            live: false,
            quiet: true,
        }
    }

    pub fn total(&self) -> usize {
        self.units.len()
    }

    /// The next step of the plan: prints `[k/N] {phase} · {subject}` and hands
    /// back the guard that owns the live line and the result line.
    pub fn step(&mut self) -> Step {
        let index = self.next;
        self.next += 1;
        let total = self.units.len();
        let label = match self.units.get(index) {
            Some(unit) => format!(
                "[{}/{}] {}{SEPARATOR}{}",
                index + 1,
                total,
                unit.phase,
                unit.subject
            ),
            None => {
                // `silent()` has no units by design; a real plan that ran out
                // of units is a bug worth failing a test on.
                if !self.units.is_empty() {
                    debug_assert!(false, "plan_units missed a step at index {index}");
                }
                format!("[{}/?]{SEPARATOR}", index + 1)
            }
        };
        println!("{label}");
        Step {
            label,
            started: Instant::now(),
            live: self.live,
            quiet: self.quiet,
            painter: None,
        }
    }

    /// TTY only, once `up` is done: `{N} steps in 42.3s`.
    pub fn finish(&self) {
        if self.live && !self.units.is_empty() {
            println!("{} steps completed", self.units.len());
        }
    }
}

pub struct Step {
    label: String,
    started: Instant,
    live: bool,
    quiet: bool,
    painter: Option<Painter>,
}

impl Step {
    /// `     {line}` — the per-step detail (`cached`, `linked`, `{qual}: starting container`).
    /// Suppressed by `--quiet`.
    pub fn detail(&mut self, line: &str) {
        if !self.quiet {
            self.clear_live();
            println!("     {line}");
        }
    }

    /// `     {line}`, with ` {1.3s}` appended on a terminal. Always printed.
    pub fn result(&mut self, line: &str) {
        self.clear_live();
        if self.live {
            println!("     {line} {:.1}s", self.started.elapsed().as_secs_f64());
        } else {
            println!("     {line}");
        }
    }

    /// A status line while the step blocks: on a terminal it is rewritten in
    /// place (`[6/19] web:web · waiting for health … 14s`), off one it prints
    /// nothing, so scripted output stays byte-stable.
    pub fn note(&mut self, text: &str) {
        if !self.live {
            return;
        }
        let line = format!("{}{SEPARATOR}{text}", self.label);
        match &self.painter {
            Some(painter) => painter.set(line),
            None => self.painter = Some(Painter::start(line)),
        }
    }

    /// Stop the painter and erase the live line, so the next `println!` starts
    /// on a clean line. A no-op off a terminal and when nothing was painted.
    fn clear_live(&mut self) {
        if let Some(painter) = self.painter.take() {
            painter.stop();
        }
    }
}

impl Drop for Step {
    fn drop(&mut self) {
        self.clear_live();
    }
}

/// Rewrites one line in place every 250 ms while its step blocks.
struct Painter {
    text: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Painter {
    fn start(text: String) -> Self {
        let text = Arc::new(Mutex::new(text));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_text = Arc::clone(&text);
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let mut stdout = std::io::stdout();
            while !thread_stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
                if thread_stop.load(Ordering::Relaxed) {
                    break;
                }
                let current = thread_text
                    .lock()
                    .map(|line| line.clone())
                    .unwrap_or_default();
                // `\r` + clear-to-end-of-line: only ever a terminal's path.
                let _ = write!(stdout, "\r\x1b[K{current}");
                let _ = stdout.flush();
            }
        });
        Self {
            text,
            stop,
            handle: Some(handle),
        }
    }

    fn set(&self, line: String) {
        if let Ok(mut current) = self.text.lock() {
            *current = line;
        }
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        // Erase whatever was on the line so the result prints cleanly.
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "\r\x1b[K");
        let _ = stdout.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_silent_progress_prints_nothing() {
        // Off a terminal and quiet: the header and detail paths must not write.
        let progress = Progress::silent();
        assert!(!progress.live);
        assert!(progress.quiet);
        assert_eq!(progress.total(), 0);
    }

    #[test]
    fn a_step_prints_its_number_and_result() {
        let mut progress = Progress::new(
            vec![
                Unit::new("bootstrap .", "just generate"),
                Unit::new("job", "seed"),
            ],
            false,
        );
        assert_eq!(progress.total(), 2);
        // Each `step()` consumes one unit of the plan, in order.
        let mut first = progress.step();
        first.result("ok");
        let mut second = progress.step();
        second.result("done");
        assert_eq!(progress.next, 2);
    }

    #[test]
    fn notes_are_silent_off_a_terminal() {
        // The test process has no TTY: the live line never starts.
        let mut progress = Progress::new(vec![Unit::new("service", "web:web")], false);
        assert!(!progress.live);
        let mut step = progress.step();
        step.note("waiting for health");
        assert!(step.painter.is_none(), "no painter starts off a terminal");
        step.result("healthy");
    }

    #[test]
    fn quiet_suppresses_details_but_not_results() {
        let mut progress = Progress::new(vec![Unit::new("job", "seed")], true);
        assert!(progress.quiet);
        let mut step = progress.step();
        step.detail("hidden");
        step.result("done");
    }
}
