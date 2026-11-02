//! Crash reporting. A panic anywhere in agentosd is a bug that may have left
//! a task half-done, so the daemon does not limp on: the panic hook prints a
//! report with a backtrace to stderr (which the image sends to the journal,
//! `journalctl -t agentosd`), saves it as `crash.txt` in the data directory,
//! and exits. `agentos-daemon` then starts a fresh daemon, which picks up the
//! report and records it in the audit log.

use std::path::{Path, PathBuf};

/// Exit status after a panic (the same one Rust uses for a panicking main).
pub const EXIT_PANIC: i32 = 101;

fn crash_file(home: &Path) -> PathBuf {
    home.join("crash.txt")
}

/// Text of a crash report.
pub fn report(location: Option<String>, message: &str, thread: &str, backtrace: &str) -> String {
    let when = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%:z");
    format!(
        "agentosd {} 崩溃：{message}（位置 {}，线程 {thread}，时间 {when}）\n{backtrace}\n",
        env!("CARGO_PKG_VERSION"),
        location.as_deref().unwrap_or("未知"),
    )
}

/// Install the panic hook. Call once, early in `main`.
pub fn install(home: &Path) {
    let file = crash_file(home);
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "（无信息）".into());
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()));
        let thread = std::thread::current()
            .name()
            .unwrap_or("未命名")
            .to_string();
        let bt = std::backtrace::Backtrace::force_capture().to_string();
        let text = report(location, &message, &thread, &bt);
        eprintln!("{text}");
        let _ = std::fs::write(&file, &text);
        std::process::exit(EXIT_PANIC);
    }));
}

/// The previous run's crash report, if there is one: returns its first line
/// and moves the full report to `crash.prev.txt`, so it is reported once.
pub fn take_last(home: &Path) -> Option<String> {
    let file = crash_file(home);
    let text = std::fs::read_to_string(&file).ok()?;
    let _ = std::fs::rename(&file, home.join("crash.prev.txt"));
    Some(text.lines().next().unwrap_or("").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_names_message_and_location() {
        let r = report(Some("src/x.rs:7".into()), "boom", "main", "0: frame");
        let first = r.lines().next().unwrap();
        assert!(first.contains("boom") && first.contains("src/x.rs:7") && first.contains("main"));
        assert!(r.contains("0: frame"));
    }

    #[test]
    fn last_crash_is_reported_once() {
        let home = std::env::temp_dir().join(format!("agentos-crash-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        assert_eq!(take_last(&home), None);
        std::fs::write(
            home.join("crash.txt"),
            "agentosd 0.1.0 崩溃：boom\nbacktrace\n",
        )
        .unwrap();
        assert_eq!(
            take_last(&home).as_deref(),
            Some("agentosd 0.1.0 崩溃：boom")
        );
        assert_eq!(take_last(&home), None);
        assert!(std::fs::read_to_string(home.join("crash.prev.txt"))
            .unwrap()
            .contains("backtrace"));
        let _ = std::fs::remove_dir_all(home);
    }
}
