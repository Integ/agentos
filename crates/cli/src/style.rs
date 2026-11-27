//! Restrained color for terminal output: a handful of states and labels, so
//! a waiting approval, a failure or an error stands out from the rest.
//!
//! Off unless standard output is a terminal, `NO_COLOR` is unset (or empty)
//! and `TERM` is not `dumb`. `--json` output never passes through here.
//!
//! Color is added only by this code, around text the caller has already made
//! safe with `render::safe_text`: data never brings its own escape sequences.
//! Colored text must not go through `safe_text` again, which would turn the
//! color codes into visible `\u{1b}` text.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use agentos_runtime::TaskState;

use crate::theme::{self, Theme};

static ENABLED: AtomicBool = AtomicBool::new(false);
/// Whether the terminal takes 24-bit color; otherwise the nearest of the
/// 256 colors nearly every terminal has.
static TRUECOLOR: AtomicBool = AtomicBool::new(false);
/// The theme's accent as `0xRRGGBB`, so the workbench follows the theme the
/// desktop uses.
static ACCENT: AtomicU32 = AtomicU32::new(0x3b82f6);

#[cfg(test)]
thread_local! {
    /// Lets a test see color without touching the process-wide switch that
    /// tests running in parallel read.
    static FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Whether color is wanted, from what the environment says.
pub fn wanted(no_color: Option<&str>, terminal: bool, term: Option<&str>) -> bool {
    terminal && no_color.is_none_or(str::is_empty) && term != Some("dumb")
}

/// Decide once, at startup, from the real environment.
pub fn init() {
    let no_color = std::env::var("NO_COLOR").ok();
    let term = std::env::var("TERM").ok();
    let terminal = std::io::IsTerminal::is_terminal(&std::io::stdout());
    ENABLED.store(
        wanted(no_color.as_deref(), terminal, term.as_deref()),
        Ordering::Relaxed,
    );
    let colorterm = std::env::var("COLORTERM").unwrap_or_default();
    TRUECOLOR.store(
        matches!(colorterm.as_str(), "truecolor" | "24bit"),
        Ordering::Relaxed,
    );
    set_theme(theme::current());
}

/// Draw accents in `theme`'s color from now on.
pub fn set_theme(theme: &Theme) {
    let (r, g, b) = theme::soft_rgb(theme);
    ACCENT.store(u32::from_be_bytes([0, r, g, b]), Ordering::Relaxed);
}

/// The escape code for a color, in 24 bits or as the nearest of the 256.
fn color_code((r, g, b): (u8, u8, u8), truecolor: bool) -> String {
    if truecolor {
        return format!("38;2;{r};{g};{b}");
    }
    let level = |c: u8| (u16::from(c) * 5 + 127) / 255;
    format!("38;5;{}", 16 + 36 * level(r) + 6 * level(g) + level(b))
}

fn accent() -> (u8, u8, u8) {
    let [_, r, g, b] = ACCENT.load(Ordering::Relaxed).to_be_bytes();
    (r, g, b)
}

fn enabled() -> bool {
    #[cfg(test)]
    if let Some(on) = FORCED.with(std::cell::Cell::get) {
        return on;
    }
    ENABLED.load(Ordering::Relaxed)
}

/// Run `f` with color on or off on this thread only.
#[cfg(test)]
pub fn with_color<T>(on: bool, f: impl FnOnce() -> T) -> T {
    FORCED.with(|forced| forced.set(Some(on)));
    let result = f();
    FORCED.with(|forced| forced.set(None));
    result
}

/// What a piece of text means, not which color it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Waits on the user: an approval, an answer, a warning.
    Attention,
    /// Went wrong.
    Bad,
    /// Went right.
    Good,
    /// Secondary: hints, what to do next.
    Quiet,
    /// A heading.
    Strong,
    /// The theme's color: the workbench's name and status line.
    Accent,
}

/// `text` in `tone`, if color is on. `text` must already be safe.
pub fn paint(text: &str, tone: Tone) -> String {
    if !enabled() || text.is_empty() {
        return text.to_string();
    }
    let code = match tone {
        Tone::Attention => "1;33".to_string(),
        Tone::Bad => "31".to_string(),
        Tone::Good => "32".to_string(),
        Tone::Quiet => "2".to_string(),
        Tone::Strong => "1".to_string(),
        Tone::Accent => format!(
            "1;{}",
            color_code(accent(), TRUECOLOR.load(Ordering::Relaxed))
        ),
    };
    format!("\x1b[{code}m{text}\x1b[0m")
}

/// A small block in `color`, to show a theme; nothing when color is off.
pub fn swatch(color: (u8, u8, u8)) -> String {
    if !enabled() {
        return String::new();
    }
    format!(
        "\x1b[{}m██\x1b[0m",
        color_code(color, TRUECOLOR.load(Ordering::Relaxed))
    )
}

/// The tone of a task state; running and cancelled tasks stay plain.
pub fn state_tone(state: TaskState) -> Option<Tone> {
    match state {
        TaskState::AwaitingConfirmation | TaskState::AwaitingInput => Some(Tone::Attention),
        TaskState::Failed | TaskState::Interrupted | TaskState::Unhandled => Some(Tone::Bad),
        TaskState::Succeeded => Some(Tone::Good),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_is_on_only_for_a_terminal_without_no_color_or_a_dumb_term() {
        assert!(wanted(None, true, Some("xterm-256color")));
        assert!(wanted(Some(""), true, None), "an empty NO_COLOR is unset");
        assert!(!wanted(Some("1"), true, Some("xterm")));
        assert!(!wanted(None, false, Some("xterm")), "piped or redirected");
        assert!(!wanted(None, true, Some("dumb")));
    }

    #[test]
    fn painting_wraps_text_only_when_color_is_on() {
        assert_eq!(with_color(false, || paint("失败", Tone::Bad)), "失败");
        assert_eq!(
            with_color(true, || paint("失败", Tone::Bad)),
            "\x1b[31m失败\x1b[0m"
        );
        assert_eq!(with_color(true, || paint("", Tone::Bad)), "");
    }

    #[test]
    fn the_accent_is_the_themes_color_in_24_bits_or_the_nearest_of_256() {
        assert_eq!(color_code((0x3b, 0x82, 0xf6), true), "38;2;59;130;246");
        // Blue lands in the 6×6×6 cube's blue corner, not on a gray.
        assert_eq!(color_code((0x3b, 0x82, 0xf6), false), "38;5;75");
        assert_eq!(color_code((0, 0, 0), false), "38;5;16");
        assert_eq!(color_code((255, 255, 255), false), "38;5;231");
        assert_eq!(with_color(false, || swatch((1, 2, 3))), "");
    }
}
