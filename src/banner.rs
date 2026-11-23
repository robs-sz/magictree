//! The magictree mark, as a terminal draws it: the preface to `--help`, and the
//! header of the TUI.
//!
//! `docs/logo.svg` is the same mark as an image, leading the wordmark; this is
//! its three rows of half blocks, defined once so the help and the screen cannot
//! drift apart.

use std::io::IsTerminal;

/// The mark: an `m` on a five by five grid. The last row is its feet, which
/// are drawn in [`LIME`].
pub const MARK: [&str; 3] = ["█▀█▀█", "█ █ █", "▀ ▀ ▀"];

/// The lime the mark stands on, `#C8F53C`.
pub const LIME: (u8, u8, u8) = (0xc8, 0xf5, 0x3c);

/// [`LIME`]'s nearest neighbour in the 256-colour palette, for a terminal that
/// has not said it can show the exact one.
pub const LIME_INDEXED: u8 = 155;

/// What stands beside the mark's lower two rows.
pub const TAGLINE: &str = "per-worktree development stacks";

/// Whether the terminal has said it shows 24-bit colour.
pub fn truecolor() -> bool {
    std::env::var("COLORTERM").is_ok_and(|value| value == "truecolor" || value == "24bit")
}

/// The preface to the help: the mark, with the version and the tagline beside
/// it. The feet are lime on a terminal, and plain wherever colour would only
/// be escape codes in a file.
pub fn preface() -> String {
    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    render(color, truecolor())
}

fn render(color: bool, truecolor: bool) -> String {
    let feet = match (color, truecolor) {
        (false, _) => MARK[2].to_string(),
        (true, true) => {
            let (r, g, b) = LIME;
            format!("\u{1b}[38;2;{r};{g};{b}m{}\u{1b}[0m", MARK[2])
        }
        (true, false) => format!("\u{1b}[38;5;{LIME_INDEXED}m{}\u{1b}[0m", MARK[2]),
    };
    format!(
        " {}\n {}  magictree {}\n {feet}  {TAGLINE}",
        MARK[0],
        MARK[1],
        env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preface_is_the_mark_beside_the_version() {
        let plain = render(false, false);
        let lines: Vec<&str> = plain.lines().collect();
        assert_eq!(lines[0], " █▀█▀█");
        assert_eq!(
            lines[1],
            format!(" █ █ █  magictree {}", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(lines[2], " ▀ ▀ ▀  per-worktree development stacks");
        assert!(!plain.contains('\u{1b}'));
        assert!(render(true, true).contains("\u{1b}[38;2;200;245;60m▀ ▀ ▀\u{1b}[0m"));
    }
}
