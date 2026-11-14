# 管家：调度子 Agent 的 super agent

管家是你唯一的对话对象，也是其他 Agent 的调度者。简单的事它自己做；属于某个领域的多步任务，它交给专门的子 Agent，再把结果汇总给你。

它不再绑定在"一次对话"上，而是一个持续存在的实体：每个意图是一个独立的任务，多个任务可以同时推进、互不干扰，所有任务和共享记忆都存在磁盘上。这一切由任务服务 `agentos-taskd` 提供。

设计讨论：[管家调度多 Agent 设计](https://claude.ai/artifact/53iWC7jjzJ1syP4MPfb2g8)

## OS 概念 → Agent

思路来自 [pi-durable](https://github.com/earendil-works/pi/blob/main/packages/durable/README.md)（可恢复的 Agent 运行时）、[jev](https://github.com/disler/durable-pi)（执行前的快速裁判）和黑板（blackboard）架构。

| 操作系统 | AgentOS | 代码 |
| --- | --- | --- |
| 进程 / PID | 任务：每个意图一个编号、状态和步骤 | `agentos-runtime` |
| 进程表、`ps`、`kill` | `agentos tasks`、`agentos cancel task-3`，以及各个界面 | `crates/cli/` |
| 调度器 | 有界的 worker 池并行推进任务；等确认或等回答的任务不占 worker | `crates/taskd/src/service.rs` |
| 程序映像 | Agent 清单（能力、风险上限、提示词） | `agentos-services` 的 `agents.rs` |
| 调用栈 / 子进程 | 委派：子 Agent 的帧压在同一个任务的栈上 | `crates/taskd/src/butler.rs` |
| 系统调用 + 权限 | 能力调用，全部经过策略引擎；子 Agent 的授权只收窄 | `agentos-policy` |
| 安全模块（LSM） | 裁判：只能把"放行"收紧为"需要确认" | `agentos-services` 的 `judge.rs` |
| 磁盘 + 日志 | 任务和每一步先落盘再执行；重启后未完成的任务标为中断，不重做 | `agentos-runtime` |
| 共享内存 / IPC | 黑板：所有任务和 Agent 共用、持久、带版本号 | `agentos-services` 的 `blackboard.rs` |
| 内核日志 | 只追加审计日志 | `agentos-audit` |

## 任务

- 每个意图开一个任务，编号持续递增（重启后也不重复）。
- 任务中的某一步要你确认时，任务挂起（"等待确认"），你可以继续说别的事，新的任务照常执行。缺少只有你知道的信息时，Agent 会提问，任务进入"等待回答"。
- 批准绑定具体的任务、步骤和令牌：在图形桌面或 Web Shell 的确认面板里点，在工作台里用 `/approve task-N 步骤`，或 `agentos approve`。说话不能批准。
- `agentos tasks` 列出任务，`agentos cancel task-3` 取消一个任务；界面里也可以。子 Agent 的步骤记在同一个任务里、委派那一步之后，并标上它的 id。
- 任务结束时记录的是结局，而不只是“结束了”：`succeeded`（做了事或直接回答了）、`unhandled`（管家没有办法做这件事，什么也没执行）、`failed`（规划出错、某一步失败或用完预算）、`cancelled`（已取消）、`interrupted`（服务重启时还没完成）。子 Agent 规划失败或无法处理时，交还给管家的结果标记为失败。

## 持久化与重启

任务保存在状态目录（默认 `~/.local/state/agentos`）的 `tasks.json` 和 `tasks.journal` 里，全部历史都保留。能力调用遵循"先记下要做、再执行、再记下结果"：审计的开始标记和任务的运行状态落盘之后，能力才拿到控制权。

服务停止或意外退出后重新启动时，所有没结束的任务（包括等待确认、等待回答的）标为"已中断"，不会接着做：中断时正在执行的那一步可能已经生效，也可能没有，服务不猜。需要的话重新说一遍，管家会从现状出发。

## 黑板

状态目录里的 `blackboard.json` 是一份所有任务和 Agent 共享的 JSON 文档：

- `board.read`（L0）：按路径读取，例如 `trip/city`，留空读全部；
- `board.write`（L1）：一次一个操作：`set`、`delete`、`increment`、`append`；带上读到的 `rev` 时，如果黑板已被别人改过，写入会被拒绝，不会覆盖；
- 上限 256 KB，每次写入都会落盘。

子 Agent 只有在清单里声明了 `board.*` 才能读写黑板（内置的两个都声明了）。

## 跨会话上下文

管家每次规划时，系统提示里会附上同一段对话里此前的任务（最近 12 个，意图和结局）、其他对话里最近结束的 8 个任务、正在等待的任务，以及黑板内容（截断到 1500 字符）；子 Agent 会看到黑板内容。读过外部内容的任务只说它发生过，不附结果。所以说"帮我准备行李"时，管家知道你前面说过"下周去杭州"，重启之后也一样。

## 裁判

设置 `AGENTOS_JUDGE=1` 后，策略引擎直接放行、不会问你的步骤（L1 及以上），执行前会先让模型审一遍：放在当前任务里是否合理，会不会删除或覆盖数据、泄露隐私、花钱、对外发送。

- 裁判只能收紧：它有疑问时，这一步改成向你确认，并附上裁判的理由；它不能放行策略引擎拒绝或要求确认的步骤。
- 只读步骤不审。裁判调用失败时按策略引擎的决定执行，并写入审计日志。
- 裁判用的是规划器同一个模型；每一步多一次模型调用。

## 多轮规划

管家不再"一次出完整个计划"，而是循环执行：

1. 规划器根据你的意图（和之前几轮的结果）给出这一轮要调用的工具；
2. 每个调用先过策略引擎，再执行；
3. 结果交还给规划器，由它决定继续调用还是给出回答。

管家每个任务最多 8 轮，子 Agent 默认 8 轮（清单里可改，最多 32）。某一步失败或被策略拒绝时，这一轮剩下的步骤不再执行，失败原因交还给规划器，由它换个办法或者如实说明。你在确认时选择取消，则整个任务中止。

Claude 和 OpenAI 兼容规划器会把前几轮原样回放给模型（`tool_use`/`tool_result`，或 `tool_calls`/`tool` 消息）。规则规划器只规划一轮。

## 委派

每个已安装的 Agent 在管家的规划器里表现为一个工具 `agent__<id>`（能力名 `agent.<id>`），参数是 `task`。调用它就是把任务交给这个 Agent：

- 子 Agent 拿到自己的系统提示词、任务和**收窄后**的能力列表，跑同样的多轮循环；
- 它最后的一句话作为这次调用的结果交还给管家；
- 离线使用规则规划器时，可以用 `@<id> 任务` 直接委派，例如 `@notes 清空笔记`。

## 权限

- **只收窄，不放大。** 子 Agent 能用的能力 = 清单里声明的 ∩ 管家自己持有的 ∩ 风险不超过清单的 `max_risk`。委派开始时，策略引擎里这个 Agent 的授权会被替换成恰好这些。
- **确认统一回到你这里。** 子 Agent 碰到要确认的步骤，整个调用栈暂停，由管家向你提问并标明是哪个 Agent 请求的；L3 仍然不能用语音确认。
- **不能再委派。** 子 Agent 看不到 `agent.*` 工具，硬调也会被拒绝，委派深度最多一层。
- **污染标记会传递。** 子 Agent 的上下文被外部内容污染后，它交回的结果也会让管家后续步骤按污染处理。
- **全程审计。** 新增 `delegated`（谁把什么任务交给谁、给了哪些能力）和 `agent_finished` 两种审计事件；每次能力调用都记录实际执行的 Agent。

## 内置 Agent

| id | 名字 | 能力 | 最高风险 |
| --- | --- | --- | --- |
| `notes` | 笔记 Agent | `notes.*`、`board.*`、`clock.now` | L3（清空仍需你确认） |
| `craftsman` | 能力工匠 Agent | `evolve.*`、`board.*` | L3（安装插件仍需你确认） |

## 自定义 Agent

把 JSON 清单放进状态目录的 `agents/`（默认 `~/.local/state/agentos/agents/`），重启 `agentos-taskd` 生效（`systemctl --user restart agentos-taskd`）。与内置 Agent 同 id 的清单会替换内置的。

```json
{
  "id": "files",
  "name": "文件整理 Agent",
  "description": "整理下载目录里的文件",
  "capabilities": ["fs.*", "clock.now"],
  "max_risk": "L1",
  "instructions": "移动文件前先列出目录。",
  "max_rounds": 8
}
```

- `capabilities`：能力名，或以 `*` 结尾的前缀；插件生成的能力也可以写进来。
- `max_risk`：`L0`–`L3`（或 `read_only` / `reversible` / `external` / `irreversible`），默认 `L1`。
- 系统里还没有的能力（比如上面的 `fs.*`）会被忽略，等能力存在（例如通过自我进化学会）后自动可用。

## 尚未实现

- 定时 / 主动任务（不需要你开口就按计划运行）；
- 子 Agent 使用不同的模型；
- 子 Agent 作为沙箱子进程或 MCP / 远程 Agent 运行，以及监管器的超时和资源限额。
