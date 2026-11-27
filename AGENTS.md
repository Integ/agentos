# AGENTS.md

给在这个仓库里写代码的 Agent（和人）。架构见 [docs/architecture.md](docs/architecture.md)，协议见 [docs/protocol-v1.md](docs/protocol-v1.md)；这里只写动手之前需要知道的事。

## 检查

提交前这四条都要通过，CI 跑的也是它们：

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
node --test crates/web-shell/assets/*.test.mjs
```

- 最低 Rust 版本是 1.88，CI 会用它编译；不要用更新版本才有的标准库 API。
- 改了依赖要提交 `Cargo.lock`，CI 用 `--locked`。
- `agentos-services` 里插件沙箱和 MCP 沙箱的测试需要 Linux 和 bubblewrap。在 macOS 上有 10 个测试（`plugin::tests` 6 个、`plugin_store::tests` 4 个）会因为无法启动插件而失败（设置资源上限时报 `Invalid argument`），这是环境原因。真正启动 MCP 沙箱的测试在没有 bubblewrap 时会跳过，但在设置了 `CI` 的环境里必须能跑。其余测试必须通过。
- `agentos-shell` 在 Linux 上需要 `libwebkit2gtk-4.1-dev` 和 `libgtk-3-dev`。
- 用临时状态目录运行，不要动 `~/.local/state/agentos`：`cargo run -p agentos-taskd -- --planner rule --state-dir /tmp/agentos-dev`。自定义 socket 的父目录必须归自己所有且不可被他人写入。
- 运行守护进程前确认环境里没有不想用的 `ANTHROPIC_API_KEY` / `OPENAI_API_KEY`；有密钥时会自动选用模型规划器并发起真实请求。

## 东西放在哪里

| 要做的事 | 位置 |
| --- | --- |
| 新能力（工具） | `crates/services/src/`，实现 `Capability`，在 `crates/taskd/src/main.rs` 里注册并授权 |
| 规划器、提示词、规则规划器的句式 | `crates/planner/` |
| 任务状态、步骤、持久化规则 | `crates/runtime/` |
| 协议方法、帧、错误码 | `crates/ipc/`，服务端在 `crates/taskd/src/service.rs`，同时更新 `docs/protocol-v1.md` |
| 管家循环：规划、策略、委派、批准 | `crates/taskd/src/butler.rs` |
| 审计事件 | `crates/audit/` |
| 任务服务对自己的报告（`system.status`）和 `agentos doctor` | `crates/taskd/src/status.rs`（启动时在 `main.rs` 里收集）、`crates/cli/src/doctor.rs`，共享的结构在 `crates/ipc/src/status.rs`；报告里只放任务服务自己的话，外部程序说的只进日志 |
| 终端、浏览器、原生桌面 | `crates/cli/`、`crates/web-shell/`、`crates/shell/`；`agentos` 的新命令同时写进 `crates/cli/src/help.rs` 的表（分组帮助、`<命令> --help` 和 `--json help` 都读它），终端颜色只经过 `crates/cli/src/style.rs` |
| 按住说话、朗读、崩溃报告、旧数据迁移 | `crates/taskd/src/voice.rs`、`crash.rs`、`migrate.rs` |
| 定时任务 | 存储和能力在 `crates/services/src/schedule.rs`，到期提交在 `crates/taskd/src/service.rs` |
| 不写镜像的独立安装 | `install.sh`（用户级：`~/.local/bin`、`~/.config/agentos/agentos.env`、systemd 用户服务），说明在 `docs/install-standalone.md`；永远不碰状态目录。CI 用 `tests/install-standalone.sh` 在真实的 systemd 用户实例上测它，这个测试会装进当前用户的主目录，只在 `CI=true` 时运行 |
| 网页、文件工作区 | `crates/services/src/web.rs`、`files.rs` |
| 微信通道 | `crates/cli/src/wechat.rs`（协议客户端，和工作台一样） |
| 自我改进：体检报告、回放评测、学习与自动采用 | `crates/taskd/src/improve.rs`、`replay.rs`、`learn.rs`、`learned.rs` |
| 提示注入评测：场景、替身能力、无污点对照、报告 | `crates/eval/`，说明在 `docs/injection-eval.md`；无污点对照只能在这里构造，不要给产品加关闭污点的开关 |

只有一个守护进程 `agentos-taskd`。旧的 `agentosd` 已删除；镜像里的 `image/rootfs/usr/local/bin/agentosd` 只是给 v0.4 及更早版本的 `agentos-update` 自检用的垫片，不要往里加功能，下一个版本删除它。

## 当前重点与冻结

v0.4.0 之后，开发集中在任务内核本身：
- 安全：沙箱、调用时限、污点；
- 能力：定时任务、文件、网页、聊天渠道，全部经过策略引擎和审计；
- 不装镜像也能单独安装。

下面这些部分已冻结：只修 bug 和安全问题，必要时跟着协议做适配，不加新功能。

| 冻结的部分 | 位置 |
| --- | --- |
| Web Shell | `crates/web-shell/` |
| 原生桌面和设置助理 | `crates/shell/`、`image/rootfs/usr/local/bin/agentos-setup` |
| 按住说话、朗读、录音指示灯 | `crates/taskd/src/voice.rs`、`mic_led.rs` |
| 智能家居 | `crates/services/src/homeassistant.rs` |
| 自我改进：体检、回放、学习。不做研究草案的第 4–6 阶段（插件自修复、提示与配置优化、代码补丁） | `crates/taskd/src/improve.rs`、`replay.rs`、`learn.rs`、`learned.rs` |
| 镜像：只随版本发布，修启动和更新问题，不加新的系统功能 | `image/` |

终端的 `agentos` 命令和工作台是内核的参考客户端，不在冻结范围内。确实需要给冻结的部分加功能时，先在 issue 里说明理由，不要顺手加。

## 不能破坏的约定

- **能力不做权限判断。** 每次调用都由管家经过策略引擎和审计日志。新能力要如实声明风险等级；输出来自用户控制之外的（网页、邮件、外部工具）要让 `untrusted_output()` 返回 `true`。
- **别人写的代码在沙箱里跑。** 插件和 MCP server 在 bubblewrap 里运行，看不到状态目录和控制 socket，否则它们能改写策略文件，或者自己批准自己引出的步骤。没有可用的沙箱时不能照常运行：插件每次调用都要人确认；MCP server 只有用户在 `mcp.json` 里写了 `"sandbox": false` 才启动。新增会启动外部程序的能力也照此办理。
- **每个调用都有时限。** 能中止的调用超时就放弃；不能中止的调用转到后台运行完，不能让它一直占着执行名额，它最后的结果写进审计。
- **结局如实。** `succeeded` 只用于真的做成了或回答了。没有办法处理是 `unhandled`，规划失败是 `failed`。不要为了让界面好看把失败写成成功。
- **界面不替用户说话。** 展示数据用协议的 `read`，不要提交一条固定文字的意图。
- **变更请求不自动重发。** 提交、取消、批准在连接中断后结果未知；客户端只能查询，不能重试。
- **批准绑定具体请求。** 任务、步骤、令牌、系统用户、五分钟期限，L3 再加 PIN。PIN 不写进日志、任务、审计或命令行参数。
- **先落盘再执行。** 能力调用前，审计的开始标记和任务的运行状态都必须已经持久化；持久化失败就停止，不继续执行。
- **审计日志只追加。** 不改写、不删除；新事件加到 `AuditEvent` 里，旧记录必须还能读。
- **协议严格。** 请求里的未知字段一律拒绝。新增字段要有默认值，新增状态和方法要写进协议文档的兼容性说明。
- **一份实现。** 几个程序都需要的东西（协议、审计、规划器、能力）放进共享 crate，不要复制文件。

## 写法

- 面向用户的文字用中文；代码、注释和提交信息用英文。
- 注释写“为什么”，尤其是安全和持久化上的理由；不要写改动经过。
- 测试用行为命名（`an_unhandled_intent_is_never_shown_as_completed`），断言旁边写明它守护的是哪条约定。
- 模型相关的行为用脚本化的规划器测试，不依赖真实接口。
