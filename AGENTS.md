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
- `agentos-services` 里插件沙箱的测试需要 Linux 和 bubblewrap。在 macOS 上有 7 个测试会因为无法启动插件而失败，这是环境原因；其余测试必须通过。
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
| 终端、浏览器、原生桌面 | `crates/cli/`、`crates/web-shell/`、`crates/shell/` |
| 按住说话、朗读、崩溃报告、旧数据迁移 | `crates/taskd/src/voice.rs`、`crash.rs`、`migrate.rs` |

只有一个守护进程 `agentos-taskd`。旧的 `agentosd` 已删除；镜像里的 `image/rootfs/usr/local/bin/agentosd` 只是给 v0.4 及更早版本的 `agentos-update` 自检用的垫片，不要往里加功能，下一个版本删除它。

## 不能破坏的约定

- **能力不做权限判断。** 每次调用都由管家经过策略引擎和审计日志。新能力要如实声明风险等级；输出来自用户控制之外的（网页、邮件、外部工具）要让 `untrusted_output()` 返回 `true`。
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
