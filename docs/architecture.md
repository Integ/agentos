# 架构概要

完整设计草案（含分层架构图和路线图）：[AgentOS 架构设计草案](https://claude.ai/artifact/RzRDjHkBbJVbnyFDRSZ4BN)

## 已确定的决策

- **底座：** Linux 内核 + 自建用户态 OS 层（2026-10-31 确定）。不自研内核，复用 Linux 驱动。
- **首个目标平台：** 桌面工作站，64 位 x86 PC。
- **模型：** 以云端为主（Claude API），同时兼容 OpenAI 格式的大模型 API（可接其他云服务或本地 Ollama/vLLM）；本地小模型做唤醒词、语音转写等辅助。
- **语言：** 核心 Rust；SDK 后续提供 Python / TypeScript。
- **用户：** 暂时只支持单用户。

## 分层

| 层 | 职责 | P0 中的对应 |
| --- | --- | --- |
| 交互层 | 感知中枢（输入 → 意图）、表达中枢（输出路由）、Agent 工作台 | `perception.rs`、`expression.rs` |
| Agent 层 | 管家 Agent、应用 Agent、Agent 监管器 | `butler.rs` |
| 核心服务 | 意图总线、能力注册表、策略与权限引擎、上下文与记忆、模型路由、审计日志 | `registry.rs`、`agentos-policy`、`agentos-audit` |
| 设备抽象层 | `Sensor` / `Output` trait，背后是 PipeWire、V4L2、Wayland、Matter 等 | `agentos-core`、`devices.rs` |
| Linux 内核 | 驱动、cgroups、namespaces、seccomp、Landlock | — |

## 风险分级

| 级别 | 例子 | 默认策略 |
| --- | --- | --- |
| L0 `ReadOnly` | 读日历、看时间 | 自动执行 |
| L1 `Reversible` | 开灯、写草稿 | 自动执行并记录；上下文受污染时需确认 |
| L2 `External` | 发邮件、下单 | 每次确认，可预授权 |
| L3 `Irreversible` | 删除数据、开门锁 | 确认 + 身份核验，不可预授权 |

## P0 范围

- [x] 核心类型：Intent、Percept、Expression、Capability、Sensor/Output trait
- [x] 策略引擎：能力授权 + 风险分级 + 污点标记
- [x] 只追加审计日志（JSON Lines），可回放
- [x] 管家 Agent：计划 → 策略 → 执行 → 汇报，支持中途确认
- [x] 文本输入 / 控制台输出
- [x] LLM 规划器（Claude API，能力 spec 作为工具列表；单次调用生成计划，尚不支持依赖上一步结果的多轮规划）
- [x] OpenAI 兼容规划器（Chat Completions + function calling）
- [x] 自我进化 MVP：按用户需求生成能力插件，沙箱自测，审阅后 L3 确认安装，版本化回滚（见 [self-evolution.md](self-evolution.md)）
- [x] 语音输入输出：按住说话 + 朗读回复，云端（OpenAI 兼容）或本机命令（whisper.cpp / Piper）后端，麦克风使用全程可见并审计（见 [voice.md](voice.md)）
- [ ] 本机唤醒词（openWakeWord）与连续对话
- [ ] MCP server 注册为能力
- [ ] 进程间通信（Unix socket + Cap'n Proto），把各中枢拆成独立服务
