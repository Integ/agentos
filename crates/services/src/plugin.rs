//! Plugin capabilities: capabilities that live outside the daemon binary, as a
//! small script plus a manifest. This is the unit of self-evolution — AgentOS
//! grows new abilities by writing plugins, never by patching its own core.
//!
//! Protocol: the script receives the call arguments as one JSON object on
//! stdin and prints one JSON object on stdout (with a `message` string for the
//! human). A non-zero exit status is a failure; stderr is the reason. So is
//! output whose `error` field is set: generated code often reports bad input
//! that way and exits 0, and a task must never call that a success.
//!
//! Every run happens in a sandbox (bubblewrap when available): read-only
//! system, no home directory, no network unless the manifest asks for it, and
//! the plugin's private data directory only if it asks for storage.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use agentos_core::{Capability, CapabilityError, CapabilityName, CapabilitySpec, RiskLevel};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// Interpreters a plugin may use, with the entry file each one runs.
pub const RUNTIMES: &[(&str, &str)] = &[("python3", "main.py"), ("sh", "main.sh")];

const PATH: &str = "/usr/local/bin:/usr/bin:/bin";
const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_OUTPUT: u64 = 64 * 1024;

/// Per-process resource limits applied to every plugin run (and inherited
/// by anything it spawns). They are set in the child between fork and exec,
/// so they hold in both sandbox modes.
pub mod limits {
    /// Address space: enough for Python/Node plus real work, not enough to
    /// push the desktop into swap.
    pub const ADDRESS_SPACE: u64 = 1024 * 1024 * 1024;
    /// Largest single file a plugin may write.
    pub const FILE_SIZE: u64 = 64 * 1024 * 1024;
    /// CPU seconds (all threads together); the wall-clock timeout is 10 s.
    pub const CPU_SECONDS: u64 = 10;
    /// Open file descriptors.
    pub const OPEN_FILES: u64 = 256;
}

/// Apply [`limits`] to the child. RLIMIT_NPROC is deliberately not used: it
/// counts every process of the real uid, so it would trip over the user's
/// own desktop. Fork bombs are contained by the PID namespace (bwrap) and
/// the wall-clock timeout, which kills the whole sandbox.
fn apply_limits(cmd: &mut Command) {
    // SAFETY: the closure runs between fork and exec and only calls
    // setrlimit, which is async-signal-safe; it allocates nothing.
    unsafe {
        cmd.pre_exec(|| {
            let set = |res, v: u64| {
                let lim = libc::rlimit {
                    rlim_cur: v as libc::rlim_t,
                    rlim_max: v as libc::rlim_t,
                };
                if libc::setrlimit(res, &lim) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            };
            set(libc::RLIMIT_AS, limits::ADDRESS_SPACE)?;
            set(libc::RLIMIT_FSIZE, limits::FILE_SIZE)?;
            set(libc::RLIMIT_CPU, limits::CPU_SECONDS)?;
            set(libc::RLIMIT_NOFILE, limits::OPEN_FILES)?;
            set(libc::RLIMIT_CORE, 0)?;
            Ok(())
        });
    }
}

/// What a plugin may touch beyond computing on its input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permissions {
    /// May open network connections.
    #[serde(default)]
    pub network: bool,
    /// Gets a private, persistent, writable directory (`$AGENTOS_DATA`).
    #[serde(default)]
    pub storage: bool,
}

/// A self-test the plugin must pass before it can be installed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestCase {
    pub args: Value,
    /// The `message` field of the output must contain this text.
    #[serde(default)]
    pub expect_contains: Option<String>,
    /// The call must fail (bad input is refused, not answered). Then
    /// `expect_contains` is looked for in the failure reason instead.
    /// Omitted when false, so manifests written before it hash the same.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub expect_failure: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: CapabilityName,
    pub description: String,
    /// Risk the author declares. The system may raise it, never lower it.
    pub risk: RiskLevel,
    pub input_schema: Value,
    #[serde(default)]
    pub permissions: Permissions,
    pub runtime: String,
    /// Human-readable explanation of what the code does.
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub tests: Vec<TestCase>,
    /// The human request this plugin was written for.
    #[serde(default)]
    pub need: String,
}

impl PluginManifest {
    pub fn entry_file(&self) -> Option<&'static str> {
        RUNTIMES
            .iter()
            .find(|(r, _)| *r == self.runtime)
            .map(|(_, f)| *f)
    }
}

/// How plugin processes are isolated; shared with MCP servers.
pub use crate::sandbox::SandboxMode;

/// The risk the policy engine will actually use: the declared risk, raised to
/// what the permissions imply. Without a sandbox nothing is contained, so
/// every call needs confirmation.
pub fn effective_risk(m: &PluginManifest, sandbox: SandboxMode) -> RiskLevel {
    let mut floor = RiskLevel::ReadOnly;
    if m.permissions.storage {
        floor = floor.max(RiskLevel::Reversible);
    }
    if m.permissions.network || sandbox == SandboxMode::None {
        floor = floor.max(RiskLevel::External);
    }
    m.risk.max(floor)
}

pub fn risk_label(r: RiskLevel) -> &'static str {
    match r {
        RiskLevel::ReadOnly => "L0 只读",
        RiskLevel::Reversible => "L1 可撤销",
        RiskLevel::External => "L2 对外/需确认",
        RiskLevel::Irreversible => "L3 不可撤销",
    }
}

fn bwrap_command(code_dir: &Path, data_dir: Option<&Path>, net: bool) -> Command {
    let mut c = crate::sandbox::bwrap_base(net);
    c.arg("--ro-bind").arg(code_dir).arg("/plugin");
    c.args([
        "--clearenv",
        "--setenv",
        "PATH",
        PATH,
        "--setenv",
        "HOME",
        "/tmp",
    ]);
    c.args([
        "--setenv",
        "LANG",
        "C.UTF-8",
        "--setenv",
        "PYTHONDONTWRITEBYTECODE",
        "1",
    ]);
    if let Some(d) = data_dir {
        c.arg("--bind").arg(d).arg("/data");
        c.args(["--setenv", "AGENTOS_DATA", "/data"]);
    }
    c.args(["--chdir", "/plugin", "--"]);
    c
}

/// Run a plugin once with `args`. `code_dir` holds the manifest's entry file.
pub async fn run_plugin(
    sandbox: SandboxMode,
    m: &PluginManifest,
    code_dir: &Path,
    data_dir: Option<&Path>,
    args: &Value,
) -> Result<Value, CapabilityError> {
    let entry = m
        .entry_file()
        .ok_or_else(|| CapabilityError::Failed(format!("不支持的运行时 {}", m.runtime)))?;
    let data_dir = if m.permissions.storage {
        data_dir
    } else {
        None
    };
    let mut cmd = match sandbox {
        SandboxMode::Bubblewrap => {
            let mut c = bwrap_command(code_dir, data_dir, m.permissions.network);
            c.arg(&m.runtime).arg(format!("/plugin/{entry}"));
            c
        }
        SandboxMode::None => {
            let mut c = Command::new(&m.runtime);
            c.arg(code_dir.join(entry))
                .env_clear()
                .env("PATH", PATH)
                .env("LANG", "C.UTF-8")
                .env("PYTHONDONTWRITEBYTECODE", "1")
                .env("HOME", std::env::temp_dir())
                .current_dir(code_dir);
            if let Some(d) = data_dir {
                c.env("AGENTOS_DATA", d);
            }
            c
        }
    };
    apply_limits(&mut cmd);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| CapabilityError::Failed(format!("无法启动插件：{e}")))?;
    let input = serde_json::to_vec(args).unwrap_or_default();
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take().map(|s| s.take(MAX_OUTPUT + 1));
    let mut stderr = child.stderr.take().map(|s| s.take(MAX_OUTPUT));

    let io = async {
        let write = async {
            if let Some(mut s) = stdin.take() {
                // A plugin that ignores its input closes the pipe early; fine.
                let _ = s.write_all(&input).await;
            }
        };
        let read_out = async {
            let mut buf = Vec::new();
            if let Some(s) = stdout.as_mut() {
                let _ = s.read_to_end(&mut buf).await;
            }
            buf
        };
        let read_err = async {
            let mut buf = Vec::new();
            if let Some(s) = stderr.as_mut() {
                let _ = s.read_to_end(&mut buf).await;
            }
            buf
        };
        let ((), out, err) = tokio::join!(write, read_out, read_err);
        let status = child.wait().await;
        (status, out, err)
    };
    let (status, out, err) = tokio::time::timeout(TIMEOUT, io)
        .await
        .map_err(|_| CapabilityError::Failed(format!("插件超时（{} 秒）", TIMEOUT.as_secs())))?;

    let status = status.map_err(|e| CapabilityError::Failed(format!("插件进程异常：{e}")))?;
    if out.len() as u64 > MAX_OUTPUT {
        return Err(CapabilityError::Failed("插件输出过大".into()));
    }
    if !status.success() {
        let err = String::from_utf8_lossy(&err);
        let tail: String = err
            .trim()
            .chars()
            .rev()
            .take(800)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        return Err(CapabilityError::Failed(format!(
            "插件退出码 {}：{tail}",
            status.code().map_or("signal".into(), |c| c.to_string())
        )));
    }
    let mut value: Value = serde_json::from_slice(&out).map_err(|e| {
        CapabilityError::Failed(format!(
            "插件输出不是 JSON（{e}）：{}",
            String::from_utf8_lossy(&out)
                .chars()
                .take(300)
                .collect::<String>()
        ))
    })?;
    let Some(obj) = value.as_object_mut() else {
        return Err(CapabilityError::Failed("插件输出必须是 JSON 对象".into()));
    };
    // Only trusted, built-in capabilities may chain further steps.
    obj.remove("follow_up");
    if obj
        .get("error")
        .is_some_and(|e| !e.is_null() && *e != Value::Bool(false))
    {
        let reason = obj
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| obj["error"].to_string());
        return Err(CapabilityError::Failed(format!("插件报告了错误：{reason}")));
    }
    Ok(value)
}

/// An installed plugin, callable like any other capability.
pub struct PluginCapability {
    spec: CapabilitySpec,
    manifest: PluginManifest,
    version: u32,
    code_dir: PathBuf,
    data_dir: PathBuf,
    sandbox: SandboxMode,
}

impl PluginCapability {
    pub fn new(
        manifest: PluginManifest,
        version: u32,
        code_dir: PathBuf,
        data_dir: PathBuf,
        sandbox: SandboxMode,
    ) -> Self {
        let spec = CapabilitySpec {
            name: manifest.name.clone(),
            // Said where the model reads it when choosing and when reading
            // the result: a general rule in the system prompt was not enough
            // for models to doubt a plugin's plainly wrong answer.
            description: format!(
                "{}（AgentOS 自己编写的插件 v{version}，没有经过人工验证：结果和常识明显不符时不要照搬，告诉用户并提出修复）",
                manifest.description
            ),
            risk: effective_risk(&manifest, sandbox),
            input_schema: manifest.input_schema.clone(),
        };
        Self {
            spec,
            manifest,
            version,
            code_dir,
            data_dir,
            sandbox,
        }
    }

    pub fn version(&self) -> u32 {
        self.version
    }
}

#[async_trait]
impl Capability for PluginCapability {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    /// A plugin that can reach the network (or runs unsandboxed and so can
    /// reach anything) may return content an attacker wrote.
    fn untrusted_output(&self) -> bool {
        self.manifest.permissions.network || self.sandbox == SandboxMode::None
    }

    /// The plugin process is killed when its call is dropped, so cancelling
    /// a task stops the plugin instead of waiting out its timeout.
    fn abortable(&self) -> bool {
        true
    }

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        if self.manifest.permissions.storage {
            std::fs::create_dir_all(&self.data_dir)
                .map_err(|e| CapabilityError::Failed(format!("无法创建插件数据目录：{e}")))?;
        }
        run_plugin(
            self.sandbox,
            &self.manifest,
            &self.code_dir,
            Some(&self.data_dir),
            &args,
        )
        .await
        .map_err(|error| match error {
            // A failing plugin can be repaired, unlike most failures; say so
            // in the result the planner reads next, so the user hears it.
            CapabilityError::Failed(reason) => CapabilityError::Failed(format!(
                "{reason}\n（{} v{} 是 AgentOS 自己编写的插件，可以修复：告诉用户它出错了，并提出交给能力工匠 Agent 修复，或用 evolve.rollback 切回旧版本。）",
                self.manifest.name, self.version
            )),
            other => other,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub fn manifest(name: &str, runtime: &str) -> PluginManifest {
        PluginManifest {
            name: CapabilityName::new(name),
            description: "测试插件".into(),
            risk: RiskLevel::ReadOnly,
            input_schema: json!({"type": "object", "properties": {}}),
            permissions: Permissions::default(),
            runtime: runtime.into(),
            summary: String::new(),
            tests: vec![],
            need: String::new(),
        }
    }

    pub fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agentos-{tag}-{}-{}",
            std::process::id(),
            agentos_core::now_ms()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn permissions_raise_risk_never_lower_it() {
        let mut m = manifest("x.y", "sh");
        assert_eq!(
            effective_risk(&m, SandboxMode::Bubblewrap),
            RiskLevel::ReadOnly
        );
        assert_eq!(effective_risk(&m, SandboxMode::None), RiskLevel::External);
        m.permissions.storage = true;
        assert_eq!(
            effective_risk(&m, SandboxMode::Bubblewrap),
            RiskLevel::Reversible
        );
        m.permissions.network = true;
        assert_eq!(
            effective_risk(&m, SandboxMode::Bubblewrap),
            RiskLevel::External
        );
        m.risk = RiskLevel::Irreversible;
        assert_eq!(
            effective_risk(&m, SandboxMode::Bubblewrap),
            RiskLevel::Irreversible
        );
    }

    #[tokio::test]
    async fn runs_script_with_json_in_and_out() {
        let dir = temp_dir("plugin-run");
        std::fs::write(
            dir.join("main.sh"),
            "read line\ncase \"$line\" in *'\"a\":1'*) r=ok;; *) r=bad;; esac\n\
             printf '{\"message\":\"got %s\",\"follow_up\":{\"capability\":\"notes.clear\"}}' $r\n",
        )
        .unwrap();
        let m = manifest("t.echo", "sh");
        let v = run_plugin(SandboxMode::detect(), &m, &dir, None, &json!({"a": 1}))
            .await
            .unwrap();
        assert_eq!(v["message"], "got ok");
        assert!(v.get("follow_up").is_none(), "plugins may not chain steps");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn output_that_reports_an_error_is_a_failure_even_with_exit_0() {
        // Guards honest outcomes: "invalid input" printed with exit 0 must
        // not make the step, and the task, a success.
        let dir = temp_dir("error-field");
        std::fs::write(
            dir.join("main.sh"),
            "printf '{\"error\":\"invalid\",\"message\":\"输入无效\"}'\n",
        )
        .unwrap();
        let err = run_plugin(
            SandboxMode::detect(),
            &manifest("t.err", "sh"),
            &dir,
            None,
            &json!({}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("输入无效"), "{err}");
        std::fs::write(
            dir.join("main.sh"),
            "printf '{\"error\":null,\"message\":\"ok\"}'\n",
        )
        .unwrap();
        let ok = run_plugin(
            SandboxMode::detect(),
            &manifest("t.err", "sh"),
            &dir,
            None,
            &json!({}),
        )
        .await
        .unwrap();
        assert_eq!(ok["message"], "ok");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn failure_reports_stderr() {
        let dir = temp_dir("plugin-fail");
        std::fs::write(dir.join("main.sh"), "echo boom >&2\nexit 3\n").unwrap();
        let m = manifest("t.fail", "sh");
        let e = run_plugin(SandboxMode::detect(), &m, &dir, None, &json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("3") && e.contains("boom"), "{e}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn memory_limit_stops_runaway_allocation() {
        let dir = temp_dir("plugin-mem");
        std::fs::write(
            dir.join("main.py"),
            "import json\ntry:\n    b = bytearray(2 * 1024 ** 3)\n    print(json.dumps({'got': len(b)}))\nexcept MemoryError:\n    print(json.dumps({'oom': True}))\n",
        )
        .unwrap();
        let m = manifest("t.mem", "python3");
        let out = run_plugin(SandboxMode::detect(), &m, &dir, None, &json!({}))
            .await
            .unwrap();
        assert_eq!(out, json!({"oom": true}));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn file_size_limit_stops_huge_writes() {
        let dir = temp_dir("plugin-fsize");
        let data = temp_dir("plugin-fsize-data");
        std::fs::write(
            dir.join("main.py"),
            "import json, os, signal\nsignal.signal(signal.SIGXFSZ, signal.SIG_IGN)\np = os.path.join(os.environ['AGENTOS_DATA'], 'big')\ntry:\n    with open(p, 'wb') as f:\n        f.write(b'x' * (65 * 1024 * 1024))\n    print(json.dumps({'wrote': True}))\nexcept OSError as e:\n    print(json.dumps({'refused': e.errno}))\n",
        )
        .unwrap();
        let mut m = manifest("t.fsize", "python3");
        m.permissions.storage = true;
        let out = run_plugin(SandboxMode::detect(), &m, &dir, Some(&data), &json!({}))
            .await
            .unwrap();
        assert!(out.get("refused").is_some(), "{out}");
        let size = std::fs::metadata(data.join("big"))
            .map(|m| m.len())
            .unwrap_or(0);
        assert!(size <= limits::FILE_SIZE, "{size}");
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(data);
    }

    #[tokio::test]
    async fn core_dumps_off_and_fd_cap() {
        let dir = temp_dir("plugin-core");
        std::fs::write(
            dir.join("main.py"),
            "import json, resource\nprint(json.dumps({'core': resource.getrlimit(resource.RLIMIT_CORE)[0], 'nofile': resource.getrlimit(resource.RLIMIT_NOFILE)[0]}))\n",
        )
        .unwrap();
        let m = manifest("t.core", "python3");
        let out = run_plugin(SandboxMode::detect(), &m, &dir, None, &json!({}))
            .await
            .unwrap();
        assert_eq!(out, json!({"core": 0, "nofile": limits::OPEN_FILES}));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn sandbox_hides_home_and_blocks_writes() {
        if SandboxMode::detect() != SandboxMode::Bubblewrap {
            eprintln!("bubblewrap unavailable; skipping");
            return;
        }
        let dir = temp_dir("plugin-sandbox");
        std::fs::write(
            dir.join("main.sh"),
            "if touch /plugin/x 2>/dev/null; then w=yes; else w=no; fi\n\
             if [ -e /root ] || [ -e /home/claude ]; then h=yes; else h=no; fi\n\
             printf '{\"message\":\"write=%s home=%s\"}' $w $h\n",
        )
        .unwrap();
        let m = manifest("t.box", "sh");
        let v = run_plugin(SandboxMode::Bubblewrap, &m, &dir, None, &json!({}))
            .await
            .unwrap();
        assert_eq!(v["message"], "write=no home=no");
        let _ = std::fs::remove_dir_all(dir);
    }
}
