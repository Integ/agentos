# AgentOS

一个以 Agent 为第一位的操作系统，目标是让人和 AI 更顺畅地协作：Agent 通过各种输入设备接收人的意图，通过输出设备与人沟通并展示自己的工作。

AgentOS 构建在 Linux 内核之上，在用户态自建完整的 Agent OS 层：意图总线、Agent 运行时、统一的感知/表达设备抽象，以及基于能力令牌和风险分级的权限模型。

- 设计草案：[AgentOS 架构设计草案](https://claude.ai/artifact/RzRDjHkBbJVbnyFDRSZ4BN)
- 架构概要与 P0 进度：[docs/architecture.md](docs/architecture.md)

## 仓库结构

```
crates/
  core/      核心类型：Intent、Percept、Expression、Capability，Sensor/Output trait
  policy/    策略引擎：能力授权 + 风险分级确认 + 污点追踪
  audit/     只追加审计日志（JSON Lines），可回放
  agentosd/  P0 守护进程：感知中枢 → 管家 Agent → 表达中枢
```

## 快速开始

需要 Rust 1.80+。

```sh
cargo run -p agentosd
```

然后输入意图，例如：

```
记一下 买牛奶
看看笔记
现在几点
清空笔记      # L3 不可逆操作，会先请求确认
y
```

### 用 Claude 做规划

设置 `ANTHROPIC_API_KEY` 后，`agentosd` 自动改用 Claude 规划器（默认模型 `claude-opus-5-5`）：每个意图调用一次 Messages API，所有能力作为工具提供，模型返回的工具调用按顺序成为计划步骤，仍然逐步经过策略引擎。

```sh
export ANTHROPIC_API_KEY=...
cargo run -p agentosd                    # 自动使用 Claude
cargo run -p agentosd -- --planner rule  # 强制使用离线规则规划器
```

可选环境变量：`AGENTOS_MODEL`（换模型）、`ANTHROPIC_BASE_URL`（换 API 地址）。

### 用 OpenAI 兼容接口做规划

任何兼容 OpenAI Chat Completions 的服务都可以（OpenAI、DeepSeek、通义千问、Moonshot，或本地的 Ollama、vLLM、LM Studio），需要支持 function calling。

```sh
# 云端服务
export OPENAI_API_KEY=...
export OPENAI_BASE_URL=https://api.deepseek.com/v1   # 不设则为 https://api.openai.com/v1
export OPENAI_MODEL=deepseek-chat
cargo run -p agentosd -- --planner openai

# 本地 Ollama（不需要 key）
OPENAI_BASE_URL=http://localhost:11434/v1 OPENAI_MODEL=qwen3 cargo run -p agentosd
```

规划器选择：`--planner claude|openai|rule`（或环境变量 `AGENTOS_PLANNER`）。不指定时，设置了 `ANTHROPIC_API_KEY` 就用 Claude，否则配置了 OpenAI 兼容接口就用它，都没有则用离线规则规划器。

审计日志默认写到 `agentos-audit.jsonl`，可用 `--audit <路径>` 或环境变量 `AGENTOS_AUDIT` 指定。

## 开发

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test --all
```
