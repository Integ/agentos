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

use std::sync::atomic::{AtomicBool, Ordering};

use agentos_runtime::TaskState;

static ENABLED: AtomicBool = AtomicBool::new(false);

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
}

/// `text` in `tone`, if color is on. `text` must already be safe.
pub fn paint(text: &str, tone: Tone) -> String {
    if !enabled() || text.is_empty() {
        return text.to_string();
    }
    let code = match tone {
        Tone::Attention => "1;33",
        Tone::Bad => "31",
        Tone::Good => "32",
        Tone::Quiet => "2",
        Tone::Strong => "1",
    };
    format!("\x1b[{code}m{text}\x1b[0m")
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
}
