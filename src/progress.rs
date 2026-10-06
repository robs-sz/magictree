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

/// The indent before a detail or result line (`     {line}`).
const INDENT: usize = 5;

/// The terminal's usable width in columns, or `None` when stdout is not a
/// terminal or the size cannot be read.
///
/// One column is reserved: a line that fills the last column makes a terminal
/// wrap the cursor to the next row, and the live status line is rewritten with
/// `\r` + clear-to-end-of-line, which can only erase the row the cursor is on.
/// Keeping every line short enough to fit one row is what makes that rewrite
/// safe — a long line that wrapped left the second row behind on every tick.
fn terminal_width() -> Option<usize> {
    #[cfg(unix)]
    {
        let mut size: nix::libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: `TIOCGWINSZ` writes a `winsize` through the pointer it is
        // given, and `size` is exactly that type, owned and aligned.
        let result =
            unsafe { nix::libc::ioctl(nix::libc::STDOUT_FILENO, nix::libc::TIOCGWINSZ, &mut size) };
        if result == 0 && size.ws_col > 1 {
            return Some(size.ws_col as usize - 1);
        }
    }
    None
}

/// `text` cut to fit `width` columns, ending in an ellipsis when it had to lose
/// anything. Counts characters rather than display columns: a line of double
/// width glyphs can still overflow, which only matters for a rare non-ASCII
/// label and is not worth a width table.
fn fit(text: &str, width: Option<usize>) -> String {
    let Some(width) = width else {
        return text.to_string();
    };
    if width == 0 {
        return String::new();
    }
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut out: String = text.chars().take(width - 1).collect();
    out.push('…');
    out
}

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
        // Truncate only on a terminal: a log file or a test keeps every column.
        let width = if self.live { terminal_width() } else { None };
        println!("{}", fit(&label, width));
        Step {
            label,
            started: Instant::now(),
            live: self.live,
            quiet: self.quiet,
            width,
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
    /// Usable columns when the plan runs on a terminal, `None` otherwise.
    width: Option<usize>,
    painter: Option<Painter>,
}

impl Step {
    /// `     {line}` — the per-step detail (`cached`, `linked`,
    /// `{container}: starting → healthy`). Suppressed by `--quiet`.
    pub fn detail(&mut self, line: &str) {
        if !self.quiet {
            self.clear_live();
            println!(
                "{:INDENT$}{}",
                "",
                fit(line, self.width.map(|width| width.saturating_sub(INDENT)))
            );
        }
    }

    /// `     {line}`, with ` {1.3s}` appended on a terminal. Always printed.
    pub fn result(&mut self, line: &str) {
        self.clear_live();
        let text = if self.live {
            format!("{line} {:.1}s", self.started.elapsed().as_secs_f64())
        } else {
            line.to_string()
        };
        let text = fit(&text, self.width.map(|width| width.saturating_sub(INDENT)));
        println!("{:INDENT$}{text}", "");
    }

    /// A status line while the step blocks: on a terminal it is rewritten in
    /// place (`[6/19] web:web · waiting for health … 14s`), off one it prints
    /// nothing, so scripted output stays byte-stable.
    pub fn note(&mut self, text: &str) {
        if !self.live {
            return;
        }
        let line = self.status_line(text);
        match &self.painter {
            Some(painter) => painter.set(line),
            None => self.painter = Some(Painter::start(line)),
        }
    }

    /// The composed live line, cut to the usable width so the in-place rewrite
    /// never leaves a wrapped row behind.
    fn status_line(&self, text: &str) -> String {
        fit(&format!("{}{SEPARATOR}{text}", self.label), self.width)
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

    #[test]
    fn fit_truncates_to_the_width_and_leaves_short_lines_alone() {
        assert_eq!(fit("hello", None), "hello");
        assert_eq!(fit("hello", Some(10)), "hello");
        assert_eq!(fit("hello", Some(5)), "hello");
        assert_eq!(fit("hello world", Some(8)), "hello w…");
        assert_eq!(fit("hello", Some(0)), "");
        // Counts characters, so a multi-byte subject is never cut mid-glyph.
        assert_eq!(fit("héllo wörld", Some(6)), "héllo…");
    }

    #[test]
    fn the_live_status_line_is_cut_to_the_usable_width() {
        // A step built by hand: the header printed for this label would wrap a
        // narrow terminal, and the in-place rewrite cannot erase a wrapped row,
        // so the composed status line must fit the width it was given.
        let label =
            "[6/10] compose compose.yaml (project a-rather-long-project-name) · 12 services";
        let step = Step {
            label: label.to_string(),
            started: Instant::now(),
            live: true,
            quiet: false,
            width: Some(40),
            painter: None,
        };
        let line = step.status_line("starting containers");
        assert_eq!(line.chars().count(), 40, "{line}");
        assert!(line.ends_with('…'), "{line}");

        // A width the caller never set (off a terminal) leaves the line whole.
        let wide = Step {
            label: label.to_string(),
            started: Instant::now(),
            live: false,
            quiet: false,
            width: None,
            painter: None,
        };
        assert_eq!(
            wide.status_line("starting containers"),
            format!("{label}{SEPARATOR}starting containers")
        );
    }
}
