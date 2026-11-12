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
{"version":1,"id":7,"body":{"status":"ok","data":{"type":"pong","server_version":"0.3.0","revision":12}}}
```

错误响应：

```json
{"version":1,"id":7,"body":{"status":"error","error":{"code":"not_found","message":"no task task-99"}}}
```

`version` 是协议版本，与程序的语义版本号分开。v0.3 实现支持协议 `1`；未知版本得到 `unsupported_version`。协议 `1` 内的新增是向后兼容的追加：`read` 方法和 `unhandled` 状态晚于最初的 v0.3.0 加入，旧服务会以 `invalid_request` 拒绝 `read`，严格解析状态枚举的旧客户端需要升级后才能读取含 `unhandled` 的任务。字段、方法、枚举值或类型错误得到 `invalid_request`，不会交给模型重新解释。无效 JSON 若不能提取无符号整数请求编号，错误响应使用 `id: 0`。

`id` 是客户端选择的无符号整数，仅用于关联响应，**不是幂等键**。重复发送同一个 `submit` 请求编号仍可能创建第二个任务。收到完整响应前连接断开时，结果可能已经落盘；客户端必须查询任务，不能自动重发变更请求。

任务编号在线上是无符号整数（例如 `3`）；CLI 显示为 `task-3`。步骤编号在线上从 **1** 开始；持久任务中的 `confirmation.step` 和数组索引从 **0** 开始。

## 方法

| `command.method` | 其他字段 | 成功 `data.type` | 含义 |
| --- | --- | --- | --- |
| `ping` | 无 | `pong` | 程序版本和当前任务 revision |
| `submit` | `text: string` | `accepted` | 持久创建任务并排队，返回创建时的完整 `task` |
| `list` | `limit: integer`，可选 `before: integer/null`、`state: string/null` | `tasks` | 按任务 ID 倒序分页 |
| `get` | `id: integer` | `task` | 完整意图、计划、步骤、结果、状态和确认请求 |
| `cancel` | `id: integer` | `task` | 停止后续步骤，返回持久任务 |
| `approve` | `id: integer`、`step: integer`、`confirmation: string` | `approval_queued` | 校验当前具体确认请求后，将批准排入执行队列 |
| `capabilities` | 无 | `capabilities` | 已授权能力的名称、输入 schema、风险和描述 |
| `read` | `capability: string`，可选 `args: object` | `value` | 直接查询一个已授权的只读（L0）能力，不创建任务 |
| `watch` | 可选 `after: integer/null` | `watching`，随后事件 | 持续接收任务变更提示与重新同步通知 |

`accepted` 表示任务已保存并接收执行；不表示任务已经成功。`approval_queued` 同样只表示批准请求已入队，执行 worker 仍会再次检查 token、来源、期限与 grant。

### 提交与读取

```json
{"version":1,"id":1,"command":{"method":"submit","text":"记一下 买牛奶"}}
{"version":1,"id":2,"command":{"method":"get","id":1}}
{"version":1,"id":3,"command":{"method":"cancel","id":1}}
```

上面表示三条独立请求，每条都有自己的响应。提交响应的 `data` 形如 `{"type":"accepted","task":{...}}`，读取与取消形如 `{"type":"task","task":{...}}`；省略号仅用于本文解释，不是有效线上 JSON。

一个 task 包含 `id`、原始 `intent`、`state`、顺序 `steps`、`tainted`、可选 `confirmation`、`message`、`created_at_ms` 与 `updated_at_ms`。时间戳是 Unix 毫秒。状态为 `queued`、`planning`、`running`、`awaiting_confirmation`、`succeeded`、`unhandled`、`failed`、`cancelled`、`interrupted`。单个步骤包含 `capability`、`args`、`state` 与可选 `result`。

终态各自表示不同的结局，客户端不应只看“任务结束了”：

| 终态 | 含义 |
| --- | --- |
| `succeeded` | 计划的每一步都已执行成功；或规划器不需要工具，直接给出了回答（`steps` 为空，`message` 是回答） |
| `unhandled` | 规划器没有办法处理这个意图。没有执行任何步骤，也没有出错；`message` 说明当前能做什么。`steps` 必为空 |
| `failed` | 规划失败（模型不可达、拒绝或响应无效）、计划无效、被策略拒绝，或某一步执行失败 |
| `cancelled` | 被取消；已经开始的步骤可能仍产生了结果 |
| `interrupted` | 服务停止或重启时尚未完成；不会自动重复执行 |

`unhandled` 不能显示成“已完成”，也不应被脚本当作成功。

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

`read` 不创建任务，不经过规划器，不推进 revision，不产生变更事件，也不写审计记录：它和 `get`、`list` 一样，是同一系统用户对本机状态的查询，不是 Agent 的行为。因此它只对同时满足以下条件的能力开放：已授权给管家、风险为 L0 `read_only`、输出不属于不可信的外部内容（后者需要任务来承载污点）。其余能力返回 `not_readable`，必须通过 `submit` 提交意图，走规划、策略和审计。可读集合在服务启动时确定，与 `capabilities` 的结果一致。

客户端不要用 `submit` 一条固定文字来代替读取：那会在用户的任务历史和审计日志里留下一条用户没有表达过的意图，接入模型规划器时还会多一次模型调用。

### 明确批准

先 `get` 任务，展示确切能力、参数与原因。若存在确认请求，其结构包含：

```json
{"step":0,"expires_at_ms":1794300000000,"token":"opaque-token-from-task"}
```

随后明确批准对应第 1 步：

```json
{"version":1,"id":6,"command":{"method":"approve","id":1,"step":1,"confirmation":"opaque-token-from-task"}}
```

成功接收：

```json
{"version":1,"id":6,"body":{"status":"ok","data":{"type":"approval_queued","id":1,"step":1}}}
```

token 每次确认请求重新生成，客户端应将它视为不透明字符串，不能从任务编号自行构造。批准必须匹配当前任务、步骤、token、原输入来源和说话者，并在五分钟有效期内；服务端在入队和执行时各检查一次。批准不授予新能力，不能绕过撤销后的 grant。当前没有可信身份核验设备，L3 操作仍拒绝执行。

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

可靠客户端可以先建立 watch，再读取页面与 revision，并持续处理后续通知。重新连接时刷新权威页面；不要为了补齐通知去重放 `submit`、`cancel` 或 `approve`。终端工作台遵循这一原则，断线后清除已展示的批准请求，重新展示详情后才允许批准。

## 资源界限与背压

| 项目 | v0.3 限制与行为 |
| --- | --- |
| 单帧 | 最多 1,048,576 字节，不含末尾换行；接收和响应序列化均有界 |
| 意图文本 | 非空，最多 65,536 字节，按解码后的 UTF-8 字节数计算 |
| 分页 | 每页 1–50 条，任务摘要最多 120 个字符 |
| 直接读取 | 能力名 1–128 字节；单次调用最多等待 10 秒，超时返回 `capability_failed` |
| 并发连接 | 最多 64 条，含 watch；超额新连接直接关闭 |
| 执行队列 | 最多 64 个等待项，worker 串行执行；满时拒绝，不先创建任务 |
| 变更通知 | 每个 store 使用 256 条有界广播缓冲，落后触发重新同步 |
| 普通请求读入 | 每帧等待最多 60 秒；超时关闭连接 |
| 响应/事件写出 | 每帧最多等待 5 秒；不读取的客户端被断开 |
| 服务停止排空 | 停止接受和关闭客户端后，给已接收执行队列最多 5 秒 |

超长或没有换行的残帧不作为请求执行，连接关闭。有效 JSON 请求若无法生成限额内的响应，返回 `response_too_large`；任务本身仍可存在，应使用分页摘要查询，直接读取则应改用范围更小的查询。客户端收到 EOF、超时或其他传输错误，应按“变更结果可能未知”处理。

## 错误码

| `error.code` | 含义与客户端处理 |
| --- | --- |
| `invalid_request` | JSON、字段、方法或字段类型不合法；纠正协议请求 |
| `unsupported_version` | `version` 不受支持；不要以另一种格式盲目重试变更 |
| `invalid_argument` | 空/超长意图、分页数量越界、步骤从 0 开始、`read` 的参数不是对象或不被能力接受等参数问题 |
| `not_found` | 任务不存在；或 `read` 指定的能力不存在、未授权 |
| `not_readable` | `read` 指定的能力不是只读能力，或其输出属于不可信的外部内容；改为提交意图 |
| `capability_failed` | `read` 调用的能力返回了错误或超时；没有副作用，可以明确决定是否重试 |
| `queue_unavailable` | 执行/批准队列满或 worker 不可用；此次未入队，明确决定是否稍后重试 |
| `invalid_confirmation` | 未处于等待确认、步骤/token/来源不匹配，或批准过期；重新读取和展示详情 |
| `storage_unavailable` | 状态读写失败；检查本地存储与任务实际状态 |
| `audit_unavailable` | 取消已保存，但记录取消审计失败；先查询任务，不假定取消没发生 |
| `response_too_large` | 完整响应超过 1 MiB；使用精简任务页或范围更小的读取 |

帧超限、读写超时、UID 不匹配和连接数超限可能直接关闭连接，不保证存在结构化错误帧。

## 本机身份、路径与生命周期

服务端读取 Unix peer credentials，只接收与服务进程有效 UID 相同的客户端。客户端不能通过 JSON 自报 UID、设备或 speaker。服务端将来源规范为 `unix:uid:N`，说话者为 `uid:N`，所以同一个系统用户可以在工作台断开并重新连接后批准原任务。此边界适用于单用户本地服务；同 UID 的其他程序同样拥有访问权，它不提供多用户委托、应用沙箱或 L3 身份验证。

socket 文件权限为 `0600`。其父目录必须归当前用户所有，且不能被组或其他用户写入；新建父目录使用 `0700`。默认在状态目录内使用 `control.sock`。只会清理属于当前用户且确认没有活动 listener 的陈旧 socket，普通文件和 symlink 不会被覆盖；服务停止时只删除自己创建的同 inode socket。

socket 路径选择顺序为显式 `--socket`、非空 `AGENTOS_TASK_SOCKET`、状态目录下的 `control.sock`。状态目录依次由显式 `--state-dir`、非空 `AGENTOS_STATE_DIR`、绝对 `$XDG_STATE_HOME/agentos`、`$HOME/.local/state/agentos` 决定。Unix socket 路径长度受平台限制；目录过长时，使用较短的私人目录和显式 `--socket`。

客户端关闭和 stdin EOF 仅结束客户端，不结束后台服务或撤销已经接受的任务。服务在 SIGINT/SIGTERM 时停止接受请求并关闭客户端，有限排空后将未完成任务标为中断；重启仍不自动重放副作用。排队批准可能在处理前过期。一个在途能力调用即使任务已取消，也可能已经产生效果；客户端需要结合持久步骤结果和实际资源状态判断。
