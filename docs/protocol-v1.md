# AgentOS 本地控制协议 v1

此协议连接独立终端工作台/CLI 与 `agentos-taskd`。客户端只发送意图与控制请求，后台服务独立拥有任务、队列、策略和持久状态。协议类型与有界帧实现见 [`agentos-ipc`](../crates/ipc/src/lib.rs)，服务端见 [`service.rs`](../crates/taskd/src/service.rs)。

## 连接与线格式

传输使用本机 Unix stream socket，不开放 TCP 端口。每帧是一个 UTF-8 JSON 对象，后接单个换行字节 `\n`。JSON 字符串内部的换行须按 JSON 转义，不能把一个对象拆成多条物理行。

一条普通连接可以顺序发送多个请求，每个请求得到一个响应。`watch` 成功后该连接切换为只读事件流，不能继续发送请求；读操作与提交、取消、批准应使用另一条连接。客户端不得并发写入同一 stream，也不得假定响应可以乱序匹配。

请求信封：

```json
{"version":1,"id":7,"command":{"method":"ping"}}
```

响应保留原请求的 `id`：

```json
{"version":1,"id":7,"body":{"status":"ok","data":{"type":"pong","server_version":"0.4.0","revision":12}}}
```

错误响应：

```json
{"version":1,"id":7,"body":{"status":"error","error":{"code":"not_found","message":"no task task-99"}}}
```

信封还可以带一个可选的 `caller`，表示以某个已注册的外部调用方的身份发出请求；见“外部调用方”。

`version` 是协议版本，与程序的语义版本号分开。v0.3 和 v0.4 实现支持协议 `1`；未知版本得到 `unsupported_version`。协议 `1` 内的新增是向后兼容的追加，晚于最初的 v0.3.0 加入的有：`read` 方法；`unhandled` 任务状态；步骤的 `id`、`agent` 字段和 `skipped` 状态；确认请求的 `verify_identity`；`approve` 的 `pin`；`reply` 方法；`awaiting_input` 任务状态；任务的 `thread` 和 `question` 字段；`submit` 和 `list` 的 `thread`；任务摘要的 `thread`；任务的 `usage`；`preauthorize`、`preauthorizations`、`revoke_preauthorization` 方法；请求信封的 `caller`、`preauthorize` 的 `caller`，以及 `register_caller`、`callers`、`revoke_caller` 方法；`voice`、`listen`、`speech` 方法，`watch` 的 `voice`，`submit` 和 `reply` 的 `spoken`，以及语音事件 `voice_state`、`voice_level`、`transcript`、`voice_notice`（只发给声明了 `voice` 的订阅）。旧服务会以 `invalid_request` 拒绝 `read`、`reply`、预授权的三个方法、带 `pin` 的 `approve` 和带 `thread` 的 `submit`/`list`；不认识 `awaiting_input` 的客户端应把它当作尚未结束的任务；严格解析的旧客户端需要升级后才能读取含这些新字段或新取值的任务。

`id` 是客户端选择的无符号整数，仅用于关联响应，**不是幂等键**。重复发送同一个 `submit` 请求编号仍可能创建第二个任务。收到完整响应前连接断开时，结果可能已经落盘；客户端必须查询任务，不能自动重发变更请求。

任务编号在线上是无符号整数（例如 `3`）；CLI 显示为 `task-3`。步骤编号在线上从 **1** 开始；持久任务中的 `confirmation.step` 和数组索引从 **0** 开始。

## 方法

| `command.method` | 其他字段 | 成功 `data.type` | 含义 |
| --- | --- | --- | --- |
| `ping` | 无 | `pong` | 程序版本和当前任务 revision |
| `submit` | `text: string`，可选 `thread: integer`、`spoken: boolean` | `accepted` | 持久创建任务并排队，返回创建时的完整 `task`；`thread` 表示接着哪个线程说，省略则开一个新线程；`spoken` 见“语音” |
| `list` | `limit: integer`，可选 `before: integer/null`、`state: string/null`、`thread: integer` | `tasks` | 按任务 ID 倒序分页；`thread` 只列出这个线程的任务 |
| `get` | `id: integer` | `task` | 完整意图、计划、步骤、结果、状态和确认请求 |
| `cancel` | `id: integer` | `task` | 停止后续步骤，打断正在等待的模型请求和可中止的调用，返回持久任务 |
| `approve` | `id: integer`、`step: integer`、`confirmation: string`，可选 `pin: string` | `approval_queued` | 校验当前具体确认请求（不可逆步骤还要核对 PIN）后，将批准排入执行队列 |
| `reply` | `id: integer`、`step: integer`、`text: string`，可选 `spoken: boolean` | `reply_queued` | 回答任务当前正在等待的那个提问，将回答排入执行队列 |
| `preauthorize` | `capability: string`，可选 `args: array`、`other_args: boolean`、`expires_in_seconds: integer`、`pin: string` | `preauthorized` | 创建一条预授权：符合规则的调用不再逐次确认 |
| `preauthorizations` | 无 | `preauthorizations` | 仍然有效的预授权 |
| `revoke_preauthorization` | `id: string` | `preauthorization_revoked` | 撤销一条预授权，立即生效 |
| `register_caller` | `id: string`、`name: string`、`max_risk: string`，可选 `capabilities: array`、`pin: string` | `caller_registered` | 注册一个外部调用方，或整体替换它的清单 |
| `callers` | 无 | `callers` | 已注册的外部调用方 |
| `revoke_caller` | `id: string` | `caller_revoked` | 撤销调用方及它的预授权，立即生效 |
| `capabilities` | 无 | `capabilities` | 已授权能力的名称、输入 schema、风险和描述 |
| `read` | `capability: string`，可选 `args: object` | `value` | 直接查询一个已授权的只读（L0）能力，不创建任务 |
| `watch` | 可选 `after: integer/null`、`voice: boolean` | `watching`，随后事件 | 持续接收任务变更提示与重新同步通知；`voice` 为 true 时也接收语音事件 |
| `voice` | 无 | `voice` | 本机的语音识别和朗读配置及当前状态 |
| `listen` | `action: "start"/"stop"/"cancel"` | `listening` | 按住说话：开始录音、停止并识别、停止并丢弃；返回这次录音的编号 |
| `speech` | 可选 `mode: "auto"/"always"/"off"`、`stop: boolean` | `voice` | 设置何时朗读回复，或停止正在朗读的内容 |

`accepted` 表示任务已保存并接收执行；不表示任务已经成功。`approval_queued` 同样只表示批准请求已入队，执行 worker 仍会再次检查 token、来源、期限与 grant。`reply_queued` 只表示回答已入队，不表示任务拿它做了什么。

### 提交与读取

```json
{"version":1,"id":1,"command":{"method":"submit","text":"记一下 买牛奶"}}
{"version":1,"id":2,"command":{"method":"get","id":1}}
{"version":1,"id":3,"command":{"method":"cancel","id":1}}
```

上面表示三条独立请求，每条都有自己的响应。提交响应的 `data` 形如 `{"type":"accepted","task":{...}}`，读取与取消形如 `{"type":"task","task":{...}}`；省略号仅用于本文解释，不是有效线上 JSON。

一个 task 包含 `id`、所属 `thread`、原始 `intent`、`state`、顺序 `steps`、`tainted`、可选 `confirmation`、可选 `question`、`usage`、`message`、`created_at_ms` 与 `updated_at_ms`。时间戳是 Unix 毫秒。状态为 `queued`、`planning`、`running`、`awaiting_confirmation`、`awaiting_input`、`succeeded`、`unhandled`、`failed`、`cancelled`、`interrupted`。单个步骤包含 `capability`、`args`、`state`、可选 `result`，以及任务内唯一的 `id`；由子 Agent 执行的步骤带 `agent`（它的 id），管家自己的步骤没有这个字段。步骤状态为 `pending`、`running`、`succeeded`、`failed`、`skipped`。

`usage` 是任务到目前为止的消耗：`model_calls`（模型请求次数）、`unmetered_calls`（其中没有报告用量的次数）、`input_tokens`、`output_tokens` 和 `active_ms`（执行时长，不含等待人和排队的时间）。`unmetered_calls` 大于 0 时，token 数只是下限，不能当作完整用量。任务结束后 `active_ms` 还会更新一次。

规划是多轮的，所以 `steps` 会随着任务推进而增长：每一轮计划追加一组步骤。一步失败后，同一轮里排在它后面的步骤标为 `skipped`（从未开始）；规划器随后可以再规划一轮。委派给子 Agent 的步骤（`agent.<id>`）在子 Agent 工作期间保持 `running`，子 Agent 的步骤紧跟着记在它后面，所以新步骤可能插在列表中间。客户端应按 `id` 而不是下标来对应同一个步骤。

终态各自表示不同的结局，客户端不应只看“任务结束了”：

| 终态 | 含义 |
| --- | --- |
| `succeeded` | 计划的每一步都已执行成功；或规划器不需要工具，直接给出了回答（`steps` 为空，`message` 是回答） |
| `unhandled` | 规划器没有办法处理这个意图。没有执行任何步骤，也没有出错；`message` 说明当前能做什么。`steps` 必为空 |
| `failed` | 规划失败（模型不可达、拒绝或响应无效）、计划无效、被策略拒绝、某一步执行失败，或任务用完了预算（`message` 以“超出预算”开头并写明是哪一项） |
| `cancelled` | 被取消；已经开始的步骤可能仍产生了结果，被打断的步骤是 `failed` 并写明结果未知 |
| `interrupted` | 服务停止或重启时尚未完成；不会自动重复执行 |

`unhandled` 不能显示成“已完成”，也不应被脚本当作成功。

### 并发与取消

不同线程的任务互相独立地并行执行，先提交的不保证先结束。同一线程的任务按提交顺序开始：前一个还在排队或执行时，后一个保持 `queued`，不占执行名额。前一个任务挂起（等待批准或等待回答）时，后一个可以开始；被批准或回答后恢复的任务与当时正在执行的任务并行。

`cancel` 立即把任务保存为 `cancelled`，并尽快停止它正在做的事：

- 正在等待的模型请求（规划、裁判）被放弃；
- 正在执行的调用，如果能力声明自己可中止，则被放弃。这一步记为 `failed`，结果写明它在执行中被打断；有副作用的调用还会写明“是否已经生效未知”。只读能力默认可中止，沙箱插件会被终止；
- 其他调用运行到自己结束，结果照常记在这一步上，任务仍是 `cancelled`。此时这一步在一段时间内可能仍显示 `running`。

无论哪种情况，取消都不回滚已经发生的事，后续步骤保持 `pending`。

### 线程与提问

线程是一段对话：一组有先后的任务。线程用它第一个任务的编号命名，所以开启线程的任务的 `thread` 等于自己的 `id`。`submit` 带上 `thread` 就是接着这段对话说：规划器会拿到这个线程里此前任务的意图和结局。指定的线程不存在，或者不是同一来源开启的，返回 `not_found`，不创建任务。

线程里此前的某个任务读过外部内容（`tainted`）时，接着它说的任务从一开始就是 `tainted`：它拿到的上文来自用户控制之外，之后有副作用的步骤都需要批准。其他线程只会被告知那个任务发生过，不会拿到它的结果。

Agent 缺少只有用户知道的信息时可以提问。提问是任务的一步，能力名为 `ask.user`，参数是 `{"question": "..."}`。任务进入 `awaiting_input`，`question` 为 `{"step": 下标, "text": 问题}`，`message` 也是这个问题，被问的那一步保持 `pending`。

```json
{"version":1,"id":4,"command":{"method":"reply","id":1,"step":1,"text":"周三"}}
```

- `step` 是面向人的编号，从 1 开始，等于 `question.step + 1`。回答必须对上任务当前的提问、步骤和提交任务的系统用户，否则返回 `invalid_reply`，什么也不改变。问题换了，旧回答就无效。
- 回答成为那一步的结果 `{"answer": "..."}`，这一步变为 `succeeded`，任务继续。
- `awaiting_input` 不是结局，也没有期限：不回答，任务就一直等着；取消后是 `cancelled`。等待中的任务不占执行名额。
- 子 Agent 的提问同样回到用户这里，被问的步骤带着它的 `agent`。
- 服务重启后，等待回答的任务和其他未完成任务一样标为 `interrupted`。
- `reply` 是变更请求：连接中断后结果未知，客户端应查询任务，不能自动重发。界面只转交用户写下的回答，不代答。

### 外部调用方

外部调用方是用户允许提交意图的另一个程序，例如通过 MCP 接入的其他 Agent。它是一个被收窄的主体：它的任务只能使用它的清单列出、管家自己持有、并且不超过它风险上限的能力。清单默认是空的，只读能力也必须列出。

```json
{"version":1,"id":6,"command":{"method":"register_caller","id":"helper","name":"桌面助手",
  "capabilities":["notes.list","home.*"],"max_risk":"read_only"}}
{"version":1,"id":7,"caller":"helper","command":{"method":"submit","text":"看看笔记"}}
```

请求信封里的 `caller` 表示“以这个调用方的身份发出这个请求”。它只会缩小请求能做的事，所以不需要密钥：能连上 socket 的程序本来就能以用户身份提出更多。它防的是只能通过这个身份说话的程序（比如 MCP 另一端的 Agent），不防本机上以同一系统用户运行的任意程序。没有注册的 `caller` 返回 `not_found`。

带 `caller` 的请求：

- **能用的方法：** `ping`、`submit`、`list`、`get`、`cancel`、`reply`、`capabilities`、`read`。其余返回 `forbidden`：`approve`、预授权的三个方法、调用方的三个方法和 `watch`。
- **只看得到自己的任务。** `list` 只列出它提交的任务；对别人的任务 `get`、`cancel`、`reply` 返回 `not_found`；它只能接着自己开启的线程说。
- **`capabilities` 和 `read` 只覆盖清单。** 清单之外的能力对它不存在。
- **拿不到批准令牌。** 返回给它的任务里，`confirmation.token` 是空字符串。

调用方提交的任务：

- 来源是 `caller:<id>`，界面和审计里都能看出不是用户本人提交的。
- **比用户更严。** L0 自动执行；L1 及以上默认都要用户批准（用户自己的 L1 是自动执行的）。批准只能由用户做，调用方不能批准自己的请求；任务的提问 `ask.user` 可以由调用方或用户回答。
- **预授权要专门给它。** `preauthorize` 带 `caller` 时，规则只适用于这个调用方，能力必须在它的清单内；给用户自己的预授权不适用于它。
- **上下文隔离。** 规划时不会附上用户的其他任务；清单里有 `board.read` 才会附上黑板。它的任务也不会出现在用户任务的上下文里。
- **不能委派子 Agent，不能写黑板，** 即使清单里写了 `agent.*` 或 `board.*`。
- **撤销立即生效。** 调用方被撤销后，它进行中的任务的下一次调用就被拒绝，它的预授权一并撤销；重新注册同名调用方不会恢复它们。

`max_risk` 取 `read_only`、`reversible`、`external`、`irreversible`。`id` 只能用小写字母、数字、`-` 和 `_`，最长 32 个字符；`capabilities` 的每一项是能力名或以 `*` 结尾的前缀，单独一个 `*` 不被接受；最多 32 个调用方。设置了 PIN 时，注册必须带 `pin`。注册和撤销各有一条审计记录，并且是变更请求：连接中断后先用 `callers` 查询，不要重发。

### 预授权

预授权是用户事先给出的许可：某个能力的调用只要参数符合每一条规则，就不再逐次确认。它只做一件事，把“先问人”变成“放行”，并且只在这些条件下：

- 能力是 L1 或 L2。L0 本来就不问，L3 永远逐次确认并核对 PIN；对它们创建预授权返回 `not_preauthorizable`。
- 任务没有被污染。读过外部内容的任务，有副作用的步骤照旧需要批准。
- Agent 持有这个能力的授权。预授权不能把拒绝变成放行。
- 裁判没有提出异议。裁判仍可以把命中预授权的调用改成需要确认。

```json
{"version":1,"id":5,"command":{"method":"preauthorize","capability":"mail.send",
  "args":[{"field":"to","match":{"one_of":["me@example.com"]}},{"field":"body","match":"any"}],
  "expires_in_seconds":3600}}
```

`args` 里每条规则针对调用的一个顶层参数，`match` 是下列之一：

| `match` | 含义 |
| --- | --- |
| `{"equals": 值}` | 与这个值完全相同 |
| `{"one_of": [值, ...]}` | 是其中之一；参数是数组时，每个元素都必须在其中 |
| `{"prefix": "文本"}` | 是以这段文本开头的字符串。只是文本比较，路径请用 `under` |
| `{"under": "目录"}` | 是这个目录本身或其下的路径；含 `.` 或 `..` 的路径一律不符合 |
| `"any"` | 任意值；表示这个参数已被考虑并放开 |

- 规则里的参数必须出现在调用里并符合规则。调用里出现了规则没有提到的参数，就不符合；`other_args: true` 才放开未提到的参数。
- 预授权必须至少限定一个参数（`any` 不算），或者带期限。期限最长 30 天，规则最多 16 条，有效的预授权最多 64 条。
- 设置了 PIN 时，创建预授权必须带 `pin`，否则返回 `identity_required`；PIN 只用于这一次核对。
- 预授权在每次调用时重新检查：撤销或过期后，进行中的任务也不再适用。它不会批准已经在等待批准的步骤。
- 不带 `caller` 的预授权只适用于用户自己提交的任务；带 `caller` 的只适用于那个调用方，见“外部调用方”。
- 每次命中都在审计里记下所用预授权的 `id`；创建和撤销也各有一条审计记录。
- 这三个方法只能由用户的客户端调用；没有任何能力可以创建或撤销预授权，Web Shell 也不提供。
- `preauthorize` 和 `revoke_preauthorization` 是变更请求：连接中断后先用 `preauthorizations` 查询，不要重发。

### 分页与筛选

```json
{"version":1,"id":4,"command":{"method":"list","limit":2,"before":null,"state":null}}
```

响应中的任务是精简摘要，意图文本最多 120 个 Unicode 字符，不携带大体积步骤参数与结果：

```json
{"version":1,"id":4,"body":{"status":"ok","data":{"type":"tasks","tasks":[{"id":3,"text":"看看笔记","state":"succeeded","completed_steps":1,"total_steps":1,"created_at_ms":1000,"updated_at_ms":1002},{"id":2,"text":"记一下 买牛奶","state":"succeeded","completed_steps":1,"total_steps":1,"created_at_ms":900,"updated_at_ms":902}],"next_before":2,"revision":15}}}
```

当 `next_before` 非空时，以它作为下一页的 `before`；边界为严格小于，因此新提交的较大任务 ID 不会挤入已经翻到的旧页。没有更多匹配项时 `next_before` 为 `null`。例如下一页请求：

```json
{"version":1,"id":5,"command":{"method":"list","limit":2,"before":2,"state":null}}
```

`state` 是当前状态筛选。每一页及其 `revision` 在同一存储锁内读取；跨多个请求没有冻结整段历史的 snapshot。任务状态可能在翻页期间改变，筛选结果因而也可能变化。需要最新完整视图时，响应变更通知并从第一页重新读取。

### 直接读取

界面刷新自己要展示的数据不是用户的意图。`read` 直接调用一个只读能力并返回结果：

```json
{"version":1,"id":9,"command":{"method":"read","capability":"notes.list","args":{}}}
```

```json
{"version":1,"id":9,"body":{"status":"ok","data":{"type":"value","capability":"notes.list","value":{"notes":["买牛奶"],"message":"1. 买牛奶"}}}}
```

`args` 省略时为 `{}`，必须是 JSON 对象。`value` 是能力自己的返回值，结构由能力决定。

`read` 不创建任务，不经过规划器，不推进 revision，不产生变更事件，也不写审计记录：它和 `get`、`list` 一样，是同一系统用户对本机状态的查询，不是 Agent 的行为。因此它只对同时满足以下条件的能力开放：已授权给管家、风险为 L0 `read_only`、输出不属于不可信的外部内容（后者需要任务来承载污点）。其余能力返回 `not_readable`，必须通过 `submit` 提交意图，走规划、策略和审计。可读集合与 `capabilities` 的结果一致，插件被安装、停用或回滚后两者一起更新。

客户端不要用 `submit` 一条固定文字来代替读取：那会在用户的任务历史和审计日志里留下一条用户没有表达过的意图，接入模型规划器时还会多一次模型调用。

### 明确批准

先 `get` 任务，展示确切能力、参数与原因。若存在确认请求，其结构包含：

```json
{"step":0,"expires_at_ms":1794300000000,"token":"opaque-token-from-task","verify_identity":true}
```

随后明确批准对应第 1 步：

```json
{"version":1,"id":6,"command":{"method":"approve","id":1,"step":1,"confirmation":"opaque-token-from-task"}}
```

成功接收：

```json
{"version":1,"id":6,"body":{"status":"ok","data":{"type":"approval_queued","id":1,"step":1}}}
```

token 每次确认请求重新生成，客户端应将它视为不透明字符串，不能从任务编号自行构造。批准必须匹配当前任务、步骤、token、原输入来源和说话者，并在五分钟有效期内；服务端在入队和执行时各检查一次。批准不授予新能力，不能绕过撤销后的 grant。

`verify_identity` 为 `true`（缺省为 `false`）表示这一步不可逆（L3），批准必须带 `pin`：

```json
{"version":1,"id":6,"command":{"method":"approve","id":1,"step":1,"confirmation":"opaque-token-from-task","pin":"1234"}}
```

PIN 由服务端核对，不保存、不写入任务或审计；核对通过只为这个任务的这一次批准作证，一分钟内有效。没有带 `pin` 得到 `identity_required`，PIN 不对或已锁定得到 `identity_rejected`（消息说明还能试几次或要等多久），这两种情况下确认请求仍然有效，可以重试。服务没有设置 PIN 时，L3 步骤不会进入等待确认，而是直接失败。

## Revision 与实时事件

任务存储为每次成功提交的变更分配递增 `revision`，并将其和任务 snapshot 一起持久化。写失败不增加 revision、不发布事件。重启恢复若把未完成任务改为 `interrupted`，也会推进 revision。v0.2 snapshot 自动迁移后可继续分配稳定任务 ID。

```json
{"version":1,"id":8,"command":{"method":"watch","after":15}}
```

首先收到普通响应：

```json
{"version":1,"id":8,"body":{"status":"ok","data":{"type":"watching","revision":15}}}
```

随后是事件帧，事件没有请求 `id`：

```json
{"version":1,"revision":16,"kind":{"event":"task_changed","id":1}}
{"version":1,"revision":20,"kind":{"event":"resync_required","reason":"watcher fell behind; refresh authoritative task pages"}}
```

事件仅提示权威数据可能变化，不包含完整任务，也不触发任务执行。收到 `task_changed` 后可以重新读取对应 task 或当前页；收到 `resync_required` 后必须重新读取当前页和详情，不能把缺失事件解释为任务未变化。

`after` 只表达客户端已知的 revision，**不请求持久事件回放**。订阅时服务端先建立接收器，再读取当前 revision；若 `after` 不等于当前值（包括缺省），在 `watching` 后发送 `resync_required`。刚好相等则只发送后续变更。慢消费者落后超过有界通知缓冲后，同样得到重新同步通知。

可靠客户端可以先建立 watch，再读取页面与 revision，并持续处理后续通知。重新连接时刷新权威页面；不要为了补齐通知去重放 `submit`、`cancel`、`approve` 或 `reply`。终端工作台遵循这一原则，断线后清除已展示的批准请求，重新展示详情后才允许批准。

## 语音

麦克风和扬声器由服务掌管，客户端不直接录音：这样每一次使用都显示在所有声明了 `voice` 的订阅上，并写进审计日志（`device_used`：时长、音频发往哪里，不含音频和文字）。外部调用方不能使用 `voice`、`listen`、`speech`，也不能订阅语音事件。

按住时发送 `listen` 的 `start`，得到 `{"type":"listening","recording":3}`；松开时发送 `stop`（识别）或 `cancel`（丢弃），回复同一个编号。太短（不到 0.3 秒）、没有声音或被丢弃的录音不会发给识别服务。识别结果不会自己变成任务：服务把它作为事件交回去，由发起这次录音的客户端像打字一样提交或回答，并带上 `spoken: true`。`spoken` 只决定 `auto` 模式下是否朗读这个任务的回复，与身份无关。批准永远是单独的 `approve` 请求，说话不能批准任何步骤。

语音事件只发给 `voice: true` 的订阅，不认识它们的旧客户端不会收到。它们不是任务变更，`revision` 是这个订阅最近一次发出的值：

```json
{"version":1,"revision":16,"kind":{"event":"voice_state","mic":"listening","speaking":false,"speak":"auto"}}
{"version":1,"revision":16,"kind":{"event":"voice_level","level":0.42}}
{"version":1,"revision":16,"kind":{"event":"transcript","recording":3,"text":"记一下 买牛奶"}}
{"version":1,"revision":16,"kind":{"event":"voice_notice","level":"info","text":"没听清，请再说一次"}}
```

`mic` 是 `idle`、`listening` 或 `transcribing`；识别结束时先发 `transcript`，再发回到 `idle` 的 `voice_state`。`voice_level` 每秒约十次，落后的订阅会丢掉一部分。没有配置语音时 `voice` 返回 `input`、`output` 都为 false，`listen` 和 `speech` 得到 `voice_unavailable`。

## 资源界限与背压

| 项目 | v0.4 限制与行为 |
| --- | --- |
| 单帧 | 最多 1,048,576 字节，不含末尾换行；接收和响应序列化均有界 |
| 意图与回答文本 | 非空，最多 65,536 字节，按解码后的 UTF-8 字节数计算 |
| 分页 | 每页 1–50 条，任务摘要最多 120 个字符 |
| 直接读取 | 能力名 1–128 字节；单次调用最多等待 10 秒，超时返回 `capability_failed` |
| 并发连接 | 最多 64 条，含 watch；超额新连接直接关闭 |
| 执行队列 | 最多 64 个等待项；满时拒绝，不先创建任务 |
| 每任务预算 | 管家 8 轮规划；token 默认 2,000,000；执行时长默认 30 分钟。超出即 `failed`，已完成的步骤保留 |
| 同时执行 | 最多 4 个任务（含正在执行的批准）；其余在执行队列里等待。等待批准的任务不占名额 |
| 变更通知 | 每个 store 使用 256 条有界广播缓冲，落后触发重新同步 |
| 普通请求读入 | 每帧等待最多 60 秒；超时关闭连接 |
| 响应/事件写出 | 每帧最多等待 5 秒；不读取的客户端被断开 |
| 服务停止排空 | 停止接受和关闭客户端后，给已接收执行队列最多 5 秒 |

超长或没有换行的残帧不作为请求执行，连接关闭。有效 JSON 请求若无法生成限额内的响应，返回 `response_too_large`；任务本身仍可存在，应使用分页摘要查询，直接读取则应改用范围更小的查询。客户端收到 EOF、超时或其他传输错误，应按“变更结果可能未知”处理。

## 错误码

| `error.code` | 含义与客户端处理 |
| --- | --- |
| `invalid_request` | JSON、字段、方法或字段类型不合法；纠正协议请求 |
| `forbidden` | 请求带着 `caller`，而这个方法只属于用户本人（批准、预授权、调用方管理、`watch`、语音） |
| `unsupported_version` | `version` 不受支持；不要以另一种格式盲目重试变更 |
| `invalid_argument` | 空/超长意图、分页数量越界、步骤从 0 开始、`read` 的参数不是对象或不被能力接受等参数问题 |
| `not_found` | 任务不存在，或对这个调用方不可见；`submit` 指定的线程不存在或不属于这个来源；`read` 指定的能力不存在、未授权；`caller` 或预授权编号不存在 |
| `not_preauthorizable` | 这个能力是只读的（本来就不需要确认），或不可逆（每次都要确认并核对 PIN） |
| `not_readable` | `read` 指定的能力不是只读能力，或其输出属于不可信的外部内容；改为提交意图 |
| `capability_failed` | `read` 调用的能力返回了错误或超时；没有副作用，可以明确决定是否重试 |
| `queue_unavailable` | 执行/批准/回答队列满或 worker 不可用；此次未入队，明确决定是否稍后重试 |
| `invalid_reply` | 任务没有在等回答，或步骤、来源与当前的提问不匹配；重新读取任务 |
| `invalid_confirmation` | 未处于等待确认、步骤/token/来源不匹配，或批准过期；重新读取和展示详情 |
| `identity_required` | 这一步不可逆，批准时必须带 `pin`，确认请求仍有效；或设置了 PIN 而创建预授权时没有带 |
| `identity_rejected` | PIN 不对或已锁定；确认请求仍有效，按消息提示重试 |
| `identity_unavailable` | 服务没有配置身份核验，无法批准这一步 |
| `voice_unavailable` | 本机没有配置语音识别或朗读 |
| `busy` | 麦克风正在录音，这次 `start` 没有开始新的录音 |
| `not_listening` | 现在没有在录音，`stop`/`cancel` 没有可停止的 |
| `voice_failed` | 无法启动录音程序；消息里写明原因 |
| `storage_unavailable` | 状态读写失败；检查本地存储与任务实际状态 |
| `audit_unavailable` | 取消已保存，但记录取消审计失败；先查询任务，不假定取消没发生 |
| `response_too_large` | 完整响应超过 1 MiB；使用精简任务页或范围更小的读取 |

帧超限、读写超时、UID 不匹配和连接数超限可能直接关闭连接，不保证存在结构化错误帧。

## 本机身份、路径与生命周期

服务端读取 Unix peer credentials，只接收与服务进程有效 UID 相同的客户端。客户端不能通过 JSON 自报 UID、设备或 speaker。服务端将来源规范为 `unix:uid:N`，说话者为 `uid:N`，所以同一个系统用户可以在工作台断开并重新连接后批准原任务。此边界适用于单用户本地服务；同 UID 的其他程序同样拥有访问权，它不提供多用户委托或应用沙箱；L3 的身份由 PIN 另行证明。

socket 文件权限为 `0600`。其父目录必须归当前用户所有，且不能被组或其他用户写入；新建父目录使用 `0700`。默认在状态目录内使用 `control.sock`。只会清理属于当前用户且确认没有活动 listener 的陈旧 socket，普通文件和 symlink 不会被覆盖；服务停止时只删除自己创建的同 inode socket。

socket 路径选择顺序为显式 `--socket`、非空 `AGENTOS_TASK_SOCKET`、状态目录下的 `control.sock`。状态目录依次由显式 `--state-dir`、非空 `AGENTOS_STATE_DIR`、绝对 `$XDG_STATE_HOME/agentos`、`$HOME/.local/state/agentos` 决定。Unix socket 路径长度受平台限制；目录过长时，使用较短的私人目录和显式 `--socket`。

客户端关闭和 stdin EOF 仅结束客户端，不结束后台服务或撤销已经接受的任务。服务在 SIGINT/SIGTERM 时停止接受请求并关闭客户端，有限排空后将未完成任务标为中断；重启仍不自动重放副作用。排队批准可能在处理前过期。一个在途能力调用即使任务已取消，也可能已经产生效果；客户端需要结合持久步骤结果和实际资源状态判断。
