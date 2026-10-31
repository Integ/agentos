# 架构概要

完整设计草案（含分层架构图和路线图）：[AgentOS 架构设计草案](https://claude.ai/artifact/RzRDjHkBbJVbnyFDRSZ4BN)

## 已确定的决策

- **底座：** Linux 内核 + 自建用户态 OS 层（2026-10-31 确定）。不自研内核，复用 Linux 驱动。

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
- [ ] LLM 规划器（Claude API，能力 spec 作为工具列表）
- [ ] 语音输入（openWakeWord + whisper.cpp）与 TTS 输出（Piper）
- [ ] MCP server 注册为能力
- [ ] 进程间通信（Unix socket + Cap'n Proto），把各中枢拆成独立服务
