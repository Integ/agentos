//! One-time move of the old daemon's data into the task service.
//!
//! `agentosd` kept its data in `~/.local/share/agentos` (or `AGENTOS_HOME`,
//! or `$XDG_DATA_HOME/agentos`); the task service keeps its own in the state
//! directory. Most files are the same format, since both used the shared
//! crates, and are copied as they are. Notes are not: agentosd stored a bare
//! list, the task service a versioned notebook, so they are converted.
//!
//! Nothing that exists in the state directory is overwritten, and the old
//! directory is never changed: a person can always look at what was there,
//! and running an older version against it still works. A marker records
//! that the move happened and what it did, so it happens once. agentosd's
//! task history and audit log stay where they are: the tasks are all over
//! and in another format, and the audit log can still be verified in place.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::builtin::MAX_NOTE_CHARS;

/// Written in the state directory once the move is done.
pub const MARKER: &str = ".migrated-from-agentosd";

/// What the move did, one line per item, for the log and the marker.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub copied: Vec<String>,
    pub converted: Vec<String>,
    /// Left where it was, with the reason.
    pub skipped: Vec<String>,
}

impl Report {
    pub fn is_empty(&self) -> bool {
        self.copied.is_empty() && self.converted.is_empty() && self.skipped.is_empty()
    }

    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        lines.extend(self.copied.iter().map(|item| format!("已复制 {item}")));
        lines.extend(self.converted.iter().map(|item| format!("已转换 {item}")));
        lines.extend(self.skipped.iter().map(|item| format!("未迁移 {item}")));
        lines
    }
}

/// Where agentosd kept its data, by the rules it used.
pub fn legacy_dir(var: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let set = |name: &str| var(name).filter(|value| !value.is_empty());
    if let Some(home) = set("AGENTOS_HOME") {
        return Some(home.into());
    }
    if let Some(data) = set("XDG_DATA_HOME") {
        return Some(Path::new(&data).join("agentos"));
    }
    set("HOME").map(|home| Path::new(&home).join(".local/share/agentos"))
}

/// Move what agentosd left in `legacy` into `state`, once. `None` when there
/// is nothing to do: no old directory, or the move already happened. On an
/// error no marker is written, so the next start tries again; whatever was
/// already copied is then skipped as existing.
pub fn from_agentosd(legacy: &Path, state: &Path) -> io::Result<Option<Report>> {
    if !legacy.is_dir() || state.join(MARKER).exists() {
        return Ok(None);
    }
    agentos_runtime::create_private_dir(state)?;
    let mut report = Report::default();
    for name in ["pin.json", "blackboard.json", "mcp.json"] {
        let (from, to) = (legacy.join(name), state.join(name));
        if !from.exists() {
            continue;
        }
        if to.exists() {
            report.skipped.push(format!("{name}（任务服务里已有）"));
            continue;
        }
        if !fs::symlink_metadata(&from)?.is_file() {
            report.skipped.push(format!("{name}（不是普通文件）"));
            continue;
        }
        copy_file(&from, &to)?;
        report.copied.push(name.into());
    }
    for name in ["agents", "evolve"] {
        let (from, to) = (legacy.join(name), state.join(name));
        if !from.exists() {
            continue;
        }
        if to.exists() {
            report.skipped.push(format!("{name}/（任务服务里已有）"));
            continue;
        }
        if !fs::symlink_metadata(&from)?.is_dir() {
            report.skipped.push(format!("{name}/（不是目录）"));
            continue;
        }
        copy_dir(&from, &to, name, &mut report)?;
        report.copied.push(format!("{name}/"));
    }
    convert_notes(legacy, state, &mut report)?;
    write_marker(state, &report)?;
    Ok(Some(report))
}

fn convert_notes(legacy: &Path, state: &Path, report: &mut Report) -> io::Result<()> {
    let (from, to) = (legacy.join("notes.json"), state.join("notes.json"));
    let bytes = match fs::read(&from) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if to.exists() {
        report
            .skipped
            .push("notes.json（任务服务里已有笔记）".into());
        return Ok(());
    }
    let Ok(notes) = serde_json::from_slice::<Vec<String>>(&bytes) else {
        report.skipped.push("notes.json（格式无法识别）".into());
        return Ok(());
    };
    // The task service refuses a notebook with such notes, and would then not
    // start; dropping some of them quietly would lose them. Leave all of them
    // in the old file, which stays readable, and say so.
    if notes
        .iter()
        .any(|note| note.trim().is_empty() || note.chars().count() > MAX_NOTE_CHARS)
    {
        report
            .skipped
            .push("notes.json（有空白或过长的笔记，原文件保留）".into());
        return Ok(());
    }
    let count = notes.len();
    let data = json!({"version": 1, "notes": notes, "undo": null});
    let bytes = serde_json::to_vec_pretty(&data).map_err(io::Error::other)?;
    write_new(&to, &bytes, 0o600)?;
    report
        .converted
        .push(format!("notes.json（{count} 条笔记）"));
    Ok(())
}

/// Copy a tree of plain files and directories. Links and other kinds are
/// left behind and reported: following a link could copy something outside
/// the old directory into the state directory.
fn copy_dir(from: &Path, to: &Path, shown: &str, report: &mut Report) -> io::Result<()> {
    create_dir(to)?;
    let mut entries: Vec<_> = fs::read_dir(from)?.collect::<io::Result<_>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let kind = entry.file_type()?;
        let name = entry.file_name();
        let shown = format!("{shown}/{}", name.to_string_lossy());
        let target = to.join(&name);
        if kind.is_dir() {
            copy_dir(&entry.path(), &target, &shown, report)?;
        } else if kind.is_file() {
            copy_file(&entry.path(), &target)?;
        } else {
            report.skipped.push(format!("{shown}（不是普通文件）"));
        }
    }
    sync_dir(to)
}

fn copy_file(from: &Path, to: &Path) -> io::Result<()> {
    let bytes = fs::read(from)?;
    // Keep an executable bit (a plugin's script); nobody else may read it.
    let mode = file_mode(from)? & 0o700 | 0o600;
    write_new(to, &bytes, mode)
}

#[cfg(unix)]
fn file_mode(path: &Path) -> io::Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    Ok(fs::metadata(path)?.permissions().mode())
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> io::Result<u32> {
    Ok(0o600)
}

/// Write a file that must not exist yet, durably, with `mode`.
fn write_new(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    if let Some(parent) = path.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

fn create_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path)
    }
}

fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

/// Last, so a move cut short is tried again on the next start.
fn write_marker(state: &Path, report: &Report) -> io::Result<()> {
    let mut text = String::from("从 agentosd 迁移的数据（旧目录未改动）：\n");
    for line in report.lines() {
        text.push_str(&line);
        text.push('\n');
    }
    if report.is_empty() {
        text.push_str("没有需要迁移的数据\n");
    }
    write_new(&state.join(MARKER), text.as_bytes(), 0o600)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "agentos-migrate-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn legacy(dir: &TempDir) -> PathBuf {
        let old = dir.0.join("share");
        fs::create_dir_all(old.join("evolve/plugins/weather")).unwrap();
        fs::write(old.join("notes.json"), r#"["买牛奶","周三开会"]"#).unwrap();
        fs::write(old.join("pin.json"), r#"{"salt":"s","hash":"h"}"#).unwrap();
        fs::write(old.join("blackboard.json"), "{}").unwrap();
        fs::write(old.join("evolve/plugins/weather/main.py"), "print(1)").unwrap();
        old
    }

    #[test]
    fn notes_are_converted_and_the_rest_is_copied_once_leaving_the_old_directory_alone() {
        let dir = TempDir::new();
        let old = legacy(&dir);
        let before = fs::read(old.join("notes.json")).unwrap();
        let state = dir.0.join("state");
        let report = from_agentosd(&old, &state).unwrap().unwrap();
        assert_eq!(
            report.copied,
            vec!["pin.json", "blackboard.json", "evolve/"]
        );
        assert_eq!(report.converted, vec!["notes.json（2 条笔记）"]);
        assert!(report.skipped.is_empty());
        let notes: serde_json::Value =
            serde_json::from_slice(&fs::read(state.join("notes.json")).unwrap()).unwrap();
        assert_eq!(
            notes,
            json!({"version": 1, "notes": ["买牛奶", "周三开会"], "undo": null})
        );
        assert_eq!(
            fs::read_to_string(state.join("evolve/plugins/weather/main.py")).unwrap(),
            "print(1)"
        );
        // Guards "the old directory is never changed".
        assert_eq!(fs::read(old.join("notes.json")).unwrap(), before);
        // Once: the marker stops a second move, even of new old data.
        assert!(fs::read_to_string(state.join(MARKER))
            .unwrap()
            .contains("已转换 notes.json"));
        fs::write(old.join("mcp.json"), "{}").unwrap();
        assert_eq!(from_agentosd(&old, &state).unwrap(), None);
        assert!(!state.join("mcp.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn what_the_task_service_already_has_is_never_overwritten_and_copies_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let old = legacy(&dir);
        let state = dir.0.join("state");
        fs::create_dir_all(&state).unwrap();
        let mine = r#"{"version":1,"notes":["新的"],"undo":null}"#;
        fs::write(state.join("notes.json"), mine).unwrap();
        fs::write(state.join("pin.json"), "mine").unwrap();
        let report = from_agentosd(&old, &state).unwrap().unwrap();
        assert_eq!(fs::read_to_string(state.join("notes.json")).unwrap(), mine);
        assert_eq!(fs::read_to_string(state.join("pin.json")).unwrap(), "mine");
        assert_eq!(report.skipped.len(), 2);
        let mode = |p: &str| fs::metadata(state.join(p)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode("blackboard.json"), 0o600);
        assert_eq!(mode("evolve"), 0o700);
    }

    #[test]
    fn notes_the_task_service_would_refuse_stay_in_the_old_file() {
        let dir = TempDir::new();
        let old = legacy(&dir);
        fs::write(old.join("notes.json"), r#"["好的", "   "]"#).unwrap();
        let state = dir.0.join("state");
        let report = from_agentosd(&old, &state).unwrap().unwrap();
        assert!(!state.join("notes.json").exists());
        assert!(report.skipped[0].starts_with("notes.json"));
    }

    #[cfg(unix)]
    #[test]
    fn links_are_not_followed_out_of_the_old_directory() {
        let dir = TempDir::new();
        let old = legacy(&dir);
        fs::write(dir.0.join("secret"), "outside").unwrap();
        std::os::unix::fs::symlink(dir.0.join("secret"), old.join("evolve/plugins/link")).unwrap();
        let state = dir.0.join("state");
        let report = from_agentosd(&old, &state).unwrap().unwrap();
        assert!(!state.join("evolve/plugins/link").exists());
        assert_eq!(report.skipped, vec!["evolve/plugins/link（不是普通文件）"]);
    }

    #[test]
    fn nothing_happens_without_an_old_directory() {
        let dir = TempDir::new();
        let state = dir.0.join("state");
        assert_eq!(from_agentosd(&dir.0.join("none"), &state).unwrap(), None);
        assert!(!state.exists());
    }

    #[test]
    fn the_old_directory_is_found_the_way_agentosd_found_it() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(
            legacy_dir(env(&[("AGENTOS_HOME", "/h"), ("HOME", "/u")])),
            Some("/h".into())
        );
        assert_eq!(
            legacy_dir(env(&[("XDG_DATA_HOME", "/d"), ("HOME", "/u")])),
            Some("/d/agentos".into())
        );
        assert_eq!(
            legacy_dir(env(&[("HOME", "/u")])),
            Some("/u/.local/share/agentos".into())
        );
        assert_eq!(legacy_dir(env(&[])), None);
    }
}
