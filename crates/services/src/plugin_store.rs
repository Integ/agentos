//! Versioned plugin store with staging, activation and rollback.
//!
//! On-disk layout under the AgentOS home directory:
//!
//! ```text
//! plugins/<name>/versions/<n>/manifest.json   immutable once written
//! plugins/<name>/versions/<n>/main.py
//! plugins/<name>/current                       active version number (absent = disabled)
//! plugins/<name>/data/                         private storage, kept across versions
//! staging/<draft>/                             scratch copy used for self-tests
//! ```
//!
//! Staged drafts are kept in memory, so what the human approves is exactly
//! what was tested; nothing on disk can be swapped in between.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agentos_core::durable::{sync_dir, write_atomic};
use agentos_core::{now_ms, Capability, CapabilityName, RiskLevel};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::plugin::{effective_risk, run_plugin, PluginCapability, PluginManifest, SandboxMode};

/// A generated plugin: manifest plus the entry file's source.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginDraft {
    pub manifest: PluginManifest,
    pub source: String,
}

impl PluginDraft {
    /// SHA-256 over the manifest and the source; recorded in the audit log.
    pub fn digest(&self) -> String {
        let mut h = Sha256::new();
        h.update(serde_json::to_vec(&self.manifest).unwrap_or_default());
        h.update([0]);
        h.update(self.source.as_bytes());
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TestOutcome {
    pub args: Value,
    pub passed: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginInfo {
    pub name: CapabilityName,
    pub versions: Vec<u32>,
    pub current: Option<u32>,
    pub description: String,
    pub risk: RiskLevel,
    /// Why the plugin cannot run although it is meant to, if it cannot.
    /// A damaged plugin is still listed: hiding it would leave the user and
    /// the agent believing it was never installed, when an earlier version
    /// on disk could be rolled back to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallReport {
    pub name: CapabilityName,
    pub version: u32,
    pub previous: Option<u32>,
    pub digest: String,
}

pub struct PluginManager {
    root: PathBuf,
    sandbox: SandboxMode,
    run_tests_unsandboxed: bool,
    generation: AtomicU64,
    staged: Mutex<BTreeMap<String, PluginDraft>>,
    reserved: Mutex<BTreeSet<String>>,
    next_draft: AtomicU64,
}

/// Namespaces plugins may never claim.
pub const RESERVED_NAMESPACES: &[&str] = &["evolve", "system", "policy", "audit"];

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

impl PluginManager {
    pub fn open(root: impl Into<PathBuf>, sandbox: SandboxMode) -> io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(root.join("plugins"))?;
        let staging = root.join("staging");
        // Drafts do not survive a restart; neither does their scratch space.
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)?;
        Ok(Self {
            root,
            sandbox,
            run_tests_unsandboxed: false,
            generation: AtomicU64::new(1),
            staged: Mutex::default(),
            reserved: Mutex::default(),
            next_draft: AtomicU64::new(1),
        })
    }

    /// Without a sandbox, self-tests would run generated code with the
    /// daemon's privileges before the human has reviewed it, so they are
    /// skipped unless explicitly allowed.
    pub fn allow_unsandboxed_tests(mut self, yes: bool) -> Self {
        self.run_tests_unsandboxed = yes;
        self
    }

    pub fn sandbox(&self) -> SandboxMode {
        self.sandbox
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Bumped whenever the set of active plugins changes.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    fn bump(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Names that plugins may not take (the built-in capabilities).
    pub fn reserve(&self, names: impl IntoIterator<Item = CapabilityName>) {
        let mut r = self.reserved.lock().unwrap_or_else(|p| p.into_inner());
        r.extend(names.into_iter().map(|n| n.0));
    }

    pub fn is_reserved(&self, name: &str) -> bool {
        let ns = name.split('.').next().unwrap_or("");
        RESERVED_NAMESPACES.contains(&ns)
            || self
                .reserved
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains(name)
    }

    fn plugin_dir(&self, name: &str) -> io::Result<PathBuf> {
        if !valid_name(name) {
            return Err(invalid(format!("非法的插件名：{name}")));
        }
        Ok(self.root.join("plugins").join(name))
    }

    fn versions(&self, name: &str) -> io::Result<Vec<u32>> {
        let dir = self.plugin_dir(name)?.join("versions");
        let mut v: Vec<u32> = match std::fs::read_dir(&dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
                .collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => vec![],
            Err(e) => return Err(e),
        };
        v.sort_unstable();
        Ok(v)
    }

    fn current(&self, name: &str) -> io::Result<Option<u32>> {
        match std::fs::read_to_string(self.plugin_dir(name)?.join("current")) {
            Ok(s) => Ok(s.trim().parse().ok()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn set_current(&self, name: &str, version: Option<u32>) -> io::Result<()> {
        let path = self.plugin_dir(name)?.join("current");
        match version {
            Some(v) => write_atomic(&path, v.to_string().as_bytes())?,
            None => match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            },
        }
        self.bump();
        Ok(())
    }

    fn read_version(&self, name: &str, v: u32) -> io::Result<PluginDraft> {
        let dir = self.plugin_dir(name)?.join("versions").join(v.to_string());
        let manifest: PluginManifest =
            serde_json::from_slice(&std::fs::read(dir.join("manifest.json"))?)
                .map_err(io::Error::other)?;
        let entry = manifest.entry_file().ok_or_else(|| invalid("未知运行时"))?;
        let source = std::fs::read_to_string(dir.join(entry))?;
        Ok(PluginDraft { manifest, source })
    }

    fn names(&self) -> io::Result<Vec<String>> {
        let mut names: Vec<String> = std::fs::read_dir(self.root.join("plugins"))?
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| valid_name(n))
            .collect();
        names.sort();
        Ok(names)
    }

    /// The active version of an installed plugin, if any.
    pub fn active_draft(&self, name: &str) -> Option<(u32, PluginDraft)> {
        let v = self.current(name).ok()??;
        Some((v, self.read_version(name, v).ok()?))
    }

    pub fn next_version(&self, name: &str) -> u32 {
        self.versions(name)
            .ok()
            .and_then(|v| v.last().copied())
            .unwrap_or(0)
            + 1
    }

    /// Every active plugin as a capability.
    pub fn active(&self) -> Vec<Arc<dyn Capability>> {
        let mut out: Vec<Arc<dyn Capability>> = Vec::new();
        for name in self.names().unwrap_or_default() {
            if self.is_reserved(&name) {
                continue;
            }
            let Some((v, draft)) = self.active_draft(&name) else {
                continue;
            };
            let dir = self.root.join("plugins").join(&name);
            out.push(Arc::new(PluginCapability::new(
                draft.manifest,
                v,
                dir.join("versions").join(v.to_string()),
                dir.join("data"),
                self.sandbox,
            )));
        }
        out
    }

    pub fn list(&self) -> Vec<PluginInfo> {
        self.names()
            .unwrap_or_default()
            .into_iter()
            .map(|name| self.inspect(&name))
            .collect()
    }

    /// What state one installed plugin is in, including when it is damaged.
    fn inspect(&self, name: &str) -> PluginInfo {
        let versions = self.versions(name).unwrap_or_default();
        let readable = |v: u32| self.read_version(name, v).ok();
        let raw_current = self
            .plugin_dir(name)
            .ok()
            .map(|dir| std::fs::read_to_string(dir.join("current")));
        let (current, problem) = match raw_current {
            None | Some(Err(_)) if versions.is_empty() => (None, Some("没有任何版本".to_string())),
            Some(Err(e)) if e.kind() == io::ErrorKind::NotFound => (None, None),
            None | Some(Err(_)) => (None, Some("读不出当前版本记录".to_string())),
            Some(Ok(text)) => match text.trim().parse::<u32>() {
                Err(_) => (None, Some(format!("当前版本记录无效（{:?}）", text.trim()))),
                Ok(v) if readable(v).is_none() => (
                    None,
                    Some(format!("当前版本 v{v} 不存在或已损坏，没有加载")),
                ),
                Ok(v) => (Some(v), None),
            },
        };
        // Describe it by the version that runs, else the newest one that can
        // still be read; a fallback a rollback could switch to is named.
        let shown = current
            .and_then(readable)
            .or_else(|| versions.iter().rev().find_map(|v| readable(*v)));
        let problem = problem.map(
            |problem| match self.rollback_target(name, &versions, None) {
                Some(v) => format!("{problem}；可以用 evolve.rollback 切回 v{v}"),
                None => format!("{problem}；没有能用的旧版本，需要重新编写"),
            },
        );
        PluginInfo {
            name: CapabilityName::new(name),
            versions: versions.clone(),
            current,
            risk: shown.as_ref().map_or(RiskLevel::Irreversible, |d| {
                effective_risk(&d.manifest, self.sandbox)
            }),
            description: shown.map_or_else(
                || "（没有能读出的版本）".to_string(),
                |d| d.manifest.description,
            ),
            problem,
        }
    }

    /// One line per installed plugin that is damaged, for the service to
    /// report when it starts.
    pub fn problems(&self) -> Vec<String> {
        self.list()
            .into_iter()
            .filter_map(|p| Some(format!("插件 {}：{}", p.name, p.problem?)))
            .collect()
    }

    /// The newest readable version below `below` (all versions when the
    /// current one is missing or damaged).
    fn rollback_target(&self, name: &str, versions: &[u32], below: Option<u32>) -> Option<u32> {
        let below = below.unwrap_or(u32::MAX);
        versions
            .iter()
            .rev()
            .copied()
            .filter(|v| *v < below)
            .find(|v| self.read_version(name, *v).is_ok())
    }

    /// Run the draft's self-tests in a scratch directory. `None` means the
    /// tests were skipped because there is no sandbox.
    pub async fn test_draft(&self, draft: &PluginDraft) -> io::Result<Option<Vec<TestOutcome>>> {
        if self.sandbox == SandboxMode::None && !self.run_tests_unsandboxed {
            return Ok(None);
        }
        let entry = draft
            .manifest
            .entry_file()
            .ok_or_else(|| invalid("未知运行时"))?;
        let scratch = self.root.join("staging").join(format!(
            "test-{}-{}",
            now_ms(),
            self.next_draft.fetch_add(1, Ordering::SeqCst)
        ));
        let code = scratch.join("code");
        let data = scratch.join("data");
        std::fs::create_dir_all(&code)?;
        std::fs::create_dir_all(&data)?;
        std::fs::write(code.join(entry), &draft.source)?;
        let mut outcomes = Vec::new();
        for t in &draft.manifest.tests {
            let r = run_plugin(self.sandbox, &draft.manifest, &code, Some(&data), &t.args).await;
            let (passed, detail) = match r {
                Err(e) if t.expect_failure => {
                    let reason = e.to_string();
                    match &t.expect_contains {
                        Some(want) if !reason.contains(want.as_str()) => {
                            (false, format!("失败原因应包含「{want}」，实际：{reason}"))
                        }
                        _ => (true, reason),
                    }
                }
                Err(e) => (false, e.to_string()),
                Ok(v) if t.expect_failure => (
                    false,
                    format!("这个用例应该失败（无效输入要以非零状态退出），实际成功了：{v}"),
                ),
                Ok(v) => match v.get("message").and_then(Value::as_str) {
                    None => (false, format!("输出缺少 message 字段：{v}")),
                    Some(msg) => match &t.expect_contains {
                        Some(want) if !msg.contains(want.as_str()) => {
                            (false, format!("message 应包含「{want}」，实际：{msg}"))
                        }
                        _ => (true, msg.to_string()),
                    },
                },
            };
            outcomes.push(TestOutcome {
                args: t.args.clone(),
                passed,
                detail,
            });
        }
        let _ = std::fs::remove_dir_all(&scratch);
        Ok(Some(outcomes))
    }

    /// Hold a validated, tested draft until the human decides. Returns its id.
    pub fn stage(&self, draft: PluginDraft) -> String {
        let id = format!(
            "d{}-{}",
            now_ms() % 1_000_000,
            self.next_draft.fetch_add(1, Ordering::SeqCst)
        );
        self.staged
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.clone(), draft);
        id
    }

    pub fn staged(&self, id: &str) -> Option<PluginDraft> {
        self.staged
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .cloned()
    }

    /// Install a staged draft as the next version and make it active.
    pub fn install(&self, id: &str) -> io::Result<InstallReport> {
        let draft = self
            .staged
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id)
            .ok_or_else(|| invalid(format!("没有待安装的草案 {id}（草案在重启后失效）")))?;
        let name = draft.manifest.name.as_str().to_string();
        if self.is_reserved(&name) {
            return Err(invalid(format!("{name} 是保留名称")));
        }
        let entry = draft
            .manifest
            .entry_file()
            .ok_or_else(|| invalid("未知运行时"))?;
        let previous = self.current(&name)?;
        let version = self.next_version(&name);
        let vdir = self
            .plugin_dir(&name)?
            .join("versions")
            .join(version.to_string());
        // Write into a temp dir, then rename, so a crash never leaves a half version.
        let tmp = vdir.with_extension("partial");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp)?;
        write_atomic(
            &tmp.join("manifest.json"),
            &serde_json::to_vec_pretty(&draft.manifest).map_err(io::Error::other)?,
        )?;
        write_atomic(&tmp.join(entry), draft.source.as_bytes())?;
        std::fs::rename(&tmp, &vdir)?;
        // `current` must never point at a version the disk may not have kept:
        // make the new version directory, and the plugin's own directory on a
        // first install, durable before switching to it.
        let versions = vdir.parent().unwrap_or(&vdir);
        sync_dir(versions)?;
        sync_dir(&self.root.join("plugins"))?;
        self.set_current(&name, Some(version))?;
        Ok(InstallReport {
            name: draft.manifest.name.clone(),
            version,
            previous,
            digest: draft.digest(),
        })
    }

    /// Activate an earlier version (default: the one before the current).
    pub fn rollback(&self, name: &str, to: Option<u32>) -> io::Result<(Option<u32>, u32)> {
        let versions = self.versions(name)?;
        if versions.is_empty() {
            return Err(invalid(format!("没有安装过插件 {name}")));
        }
        let current = self.current(name)?;
        // A current version that cannot be read is no floor: rolling back
        // from it is how a damaged plugin is repaired.
        let floor = current.filter(|v| self.read_version(name, *v).is_ok());
        let target = match to {
            Some(v) if versions.contains(&v) => {
                self.read_version(name, v)
                    .map_err(|e| invalid(format!("{name} 的 v{v} 已损坏，不能切换过去：{e}")))?;
                v
            }
            Some(v) => return Err(invalid(format!("{name} 没有 v{v}"))),
            None => self
                .rollback_target(name, &versions, floor)
                .ok_or_else(|| invalid(format!("{name} 没有能用的更早版本")))?,
        };
        self.set_current(name, Some(target))?;
        Ok((current, target))
    }

    /// Deactivate a plugin. Its versions and data are kept.
    pub fn disable(&self, name: &str) -> io::Result<Option<u32>> {
        let current = self.current(name)?;
        if current.is_none() {
            return Err(invalid(format!("{name} 没有在启用状态")));
        }
        self.set_current(name, None)?;
        Ok(current)
    }
}

/// `namespace.verb`: lowercase ASCII segments joined by dots.
pub fn valid_name(name: &str) -> bool {
    let segs: Vec<&str> = name.split('.').collect();
    name.len() <= 64
        && segs.len() >= 2
        && segs.iter().all(|s| {
            s.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                && s.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::tests::{manifest, temp_dir};
    use crate::plugin::TestCase;
    use serde_json::json;

    fn draft(name: &str, greeting: &str) -> PluginDraft {
        let mut m = manifest(name, "sh");
        m.tests = vec![TestCase {
            args: json!({}),
            expect_contains: Some(greeting.into()),
            expect_failure: false,
        }];
        PluginDraft {
            manifest: m,
            source: format!("printf '{{\"message\":\"{greeting}\"}}'\n"),
        }
    }

    fn pm(tag: &str) -> PluginManager {
        PluginManager::open(temp_dir(tag), SandboxMode::detect())
            .unwrap()
            .allow_unsandboxed_tests(true)
    }

    #[test]
    fn names_are_validated() {
        assert!(valid_name("weather.today"));
        assert!(valid_name("a.b_c.d2"));
        assert!(!valid_name("weather"));
        assert!(!valid_name("../etc.passwd"));
        assert!(!valid_name("Weather.today"));
        assert!(!valid_name("a..b"));
    }

    #[tokio::test]
    async fn install_rollback_disable() {
        let pm = pm("store");
        let g0 = pm.generation();

        let t = pm
            .test_draft(&draft("hi.say", "你好"))
            .await
            .unwrap()
            .unwrap();
        assert!(t.iter().all(|o| o.passed), "{t:?}");

        let id = pm.stage(draft("hi.say", "你好"));
        let r = pm.install(&id).unwrap();
        assert_eq!((r.version, r.previous), (1, None));
        assert!(pm.generation() > g0);
        assert!(pm.install(&id).is_err(), "a draft installs once");

        let id = pm.stage(draft("hi.say", "hello"));
        let r = pm.install(&id).unwrap();
        assert_eq!((r.version, r.previous), (2, Some(1)));

        let active = pm.active();
        assert_eq!(active.len(), 1);
        let out = active[0].invoke(json!({})).await.unwrap();
        assert_eq!(out["message"], "hello");

        assert_eq!(pm.rollback("hi.say", None).unwrap(), (Some(2), 1));
        let out = pm.active()[0].invoke(json!({})).await.unwrap();
        assert_eq!(out["message"], "你好");

        pm.disable("hi.say").unwrap();
        assert!(pm.active().is_empty());
        let info = pm.list();
        assert_eq!(info[0].versions, vec![1, 2]);
        assert_eq!(info[0].current, None);
        let _ = std::fs::remove_dir_all(pm.root());
    }

    #[tokio::test]
    async fn a_damaged_plugin_is_listed_with_its_problem_and_rolls_back_to_a_working_version() {
        // Guards repairability: a plugin whose current version cannot be
        // loaded is not silently forgotten, so a rollback can still save it.
        let pm = pm("store-damaged");
        for greeting in ["你好", "hello"] {
            let id = pm.stage(draft("hi.say", greeting));
            pm.install(&id).unwrap();
        }
        let current = pm.root().join("plugins").join("hi.say").join("current");
        std::fs::write(&current, "7").unwrap();
        assert!(pm.active().is_empty());
        let info = pm.list();
        assert_eq!(info.len(), 1);
        assert_eq!(info[0].current, None);
        let problem = info[0].problem.as_deref().unwrap();
        assert!(
            problem.contains("v7") && problem.contains("切回 v2"),
            "{problem}"
        );
        assert_eq!(pm.problems().len(), 1);

        std::fs::write(&current, "garbage").unwrap();
        assert!(pm.list()[0].problem.is_some());

        // v2 is damaged too: rollback skips it and lands on v1.
        std::fs::write(&current, "7").unwrap();
        std::fs::remove_file(pm.root().join("plugins/hi.say/versions/2/manifest.json")).unwrap();
        assert_eq!(pm.rollback("hi.say", None).unwrap(), (Some(7), 1));
        assert!(pm.rollback("hi.say", Some(2)).is_err());
        let out = pm.active()[0].invoke(json!({})).await.unwrap();
        assert_eq!(out["message"], "你好");
        assert!(pm.list()[0].problem.is_none());
        assert!(pm.problems().is_empty());
        let _ = std::fs::remove_dir_all(pm.root());
    }

    #[test]
    fn an_installed_version_is_complete_and_leaves_no_temporary_files() {
        // Guards the crash-safety of install: `current` only ever names a
        // fully written version, and nothing half-written stays around for
        // the next start to read.
        let pm = pm("store-durable");
        let id = pm.stage(draft("hi.say", "你好"));
        assert_eq!(pm.install(&id).unwrap().version, 1);

        let dir = pm.root().join("plugins").join("hi.say");
        let listing = |path: &Path| {
            let mut names: Vec<String> = std::fs::read_dir(path)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        };
        assert_eq!(listing(&dir), vec!["current", "versions"]);
        assert_eq!(listing(&dir.join("versions")), vec!["1"]);
        assert_eq!(
            listing(&dir.join("versions").join("1")),
            vec!["main.sh", "manifest.json"]
        );
        let (version, read_back) = pm.active_draft("hi.say").unwrap();
        assert_eq!(version, 1);
        assert_eq!(read_back.source, draft("hi.say", "你好").source);
        let _ = std::fs::remove_dir_all(pm.root());
    }

    #[tokio::test]
    async fn failing_test_is_reported() {
        let pm = pm("store-fail");
        let mut d = draft("hi.say", "你好");
        d.manifest.tests[0].expect_contains = Some("再见".into());
        let t = pm.test_draft(&d).await.unwrap().unwrap();
        assert!(!t[0].passed);
        assert!(t[0].detail.contains("再见"));
        let _ = std::fs::remove_dir_all(pm.root());
    }

    #[tokio::test]
    async fn a_test_that_expects_failure_passes_only_when_the_plugin_refuses() {
        // Guards the self-test of error handling: a plugin that "answers"
        // bad input with exit 0 does not pass a case that expects refusal.
        let pm = pm("store-expect-failure");
        let mut d = draft("hi.say", "你好");
        d.manifest.tests[0].expect_failure = true;
        d.manifest.tests[0].expect_contains = Some("无效".into());
        let t = pm.test_draft(&d).await.unwrap().unwrap();
        assert!(!t[0].passed, "{:?}", t[0]);
        d.source = "echo 输入无效 >&2\nexit 2\n".into();
        let t = pm.test_draft(&d).await.unwrap().unwrap();
        assert!(t[0].passed, "{:?}", t[0]);
        // Old manifests, without the field, hash as before.
        let plain = draft("hi.say", "你好");
        let text = serde_json::to_string(&plain.manifest).unwrap();
        assert!(!text.contains("expect_failure"));
        let _ = std::fs::remove_dir_all(pm.root());
    }

    #[test]
    fn reserved_names_cannot_install() {
        let pm = pm("store-reserved");
        pm.reserve([CapabilityName::new("notes.clear")]);
        assert!(pm.is_reserved("evolve.anything"));
        let id = pm.stage(draft("notes.clear", "x"));
        assert!(pm.install(&id).is_err());
        let _ = std::fs::remove_dir_all(pm.root());
    }
}
