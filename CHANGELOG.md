# 更新日志

## 未发布

- **任务存储不再每一步都重写全部历史**：此前任务的每次改动都要把所有任务重新写一遍并落盘，历史越长每一步越慢，并行的任务都在这里排队。现在改动只追加到状态目录的 `tasks.journal`，一行一个任务；日志攒到和任务数差不多多时并入 `tasks.json`，启动和正常退出时也会并入。崩溃时写了一半的最后一行会被丢弃（那次改动从未确认过）；其他读不回来的内容会让服务拒绝启动，两个文件原样保留。
- **修复：读外部内容的调用失败时，任务没有被标为受污染**。MCP 工具的错误内容、插件的 stderr 会作为失败信息交给规划器，网页或外部工具可以故意失败，把文字送进规划器而不触发“之后的副作用都要确认”。现在这类能力无论成功失败都会污染任务。`agentosd` 同样修复。
- **兼容性**：任务快照升到 schema 5，格式本身没变；旧程序读不到日志里的改动，提高版本号是为了让它拒绝启动，而不是悄悄丢掉这些改动。升级后不能带着状态目录退回旧版本。

## v0.4.0

让 Agent 成为一等公民：`agentos-taskd` 上的 Agent 可以同时做多件事、接着上一句话说、缺信息时发问、知道自己花了多少，并在你事先划定的范围内自主行动；别的 Agent 也可以通过 MCP 来用它。设计见 [docs/v0.4-design.md](docs/v0.4-design.md)。

- **任务并行执行**：默认同时 4 个，一个任务在等模型时不再挡住其他任务。同一段对话里的任务仍按提交顺序开始。
- **取消立即生效**：取消不再等模型请求返回；只读调用和沙箱插件的在途调用也会被放弃，这一步记为失败并写明结果未知，审计里以 `CapabilityInterrupted` 收尾。其他调用照旧运行到自己结束。
- **会话线程**：每个任务属于一个线程。`agentos submit --thread task-N` 接着一段对话说，规划器会拿到这段对话里此前任务的意图和结局；终端工作台里每句话默认接着同一段对话，`/new` 另起一段；原生桌面连接任务服务时，一次运行是一段对话。接着说的任务如果拿到了读过外部内容的任务的结果，从一开始就按受污染处理。
- **Agent 可以提问**：缺少只有你知道的信息时，Agent 调用 `ask.user`，任务进入新状态 `awaiting_input`（“等待回答”），不占执行名额。用 `agentos reply task-N 步骤 <回答>`、工作台的 `/reply`、Web Shell 任务详情里的输入框回答；原生桌面上你输入的下一句话就是回答。`submit --wait` 新增退出码 6。
- **用量和预算**：每次模型请求的模型、输入输出 token 和耗时写入审计并累计到任务上，任务详情里多一行“用量”；接口没有报告的用量记为未知，不记为 0。每个任务有规划轮数、token（默认 200 万）和执行时长（默认 30 分钟，不含等你的时间）三项预算，用 `AGENTOS_TASK_TOKENS`、`AGENTOS_TASK_SECONDS` 调整。
- **系统提示带上时间和设备**：每一轮规划都由服务附上当前时间、时区、主机名、系统和意图来源。主机名会随提示发给你配置的模型服务。
- **预授权**：`agentos preauthorize <能力> …` 事先许可一类调用，之后符合规则的 L1/L2 调用不再逐次确认。规则针对参数（等于、属于列表、文本前缀、在目录内、任意），可带期限；没写到的参数默认不允许出现。不适用于 L3，不适用于读过外部内容的任务；撤销（`agentos revoke pre-N`）立即生效，每次命中都记入审计。设置了 PIN 时创建需要 PIN。
- **AgentOS 作为 MCP server**：`agentos caller add <id> --name … --allow …` 注册一个外部调用方，`agentos mcp --caller <id>` 以它的身份在标准输入输出上提供 MCP 服务。调用方只能使用清单里的能力（默认什么都没有），只看得到自己的任务，有副作用的步骤默认都要你批准，它自己不能批准。见 [docs/mcp.md](docs/mcp.md#反过来agentos-作为-mcp-server)。
- **镜像和更新包包含任务服务**：`agentos-taskd`、`agentos` 命令和 `agentos-web-shell` 进入镜像和 `agentos-update` 更新包。任务服务作为桌面用户的 systemd 用户服务（`agentos-taskd.service`）随登录启动，读取 `/etc/agentos/agentos.env`，状态在 `~/.local/state/agentos`；在终端里直接用 `agentos`。开机自检和在线更新的自检都会启动它并提交一条意图，开机自检还会经 `agentos mcp` 读一次能力。设置助理设置或清除 PIN 时两个守护进程一起改。图形桌面默认仍连接 `agentosd`，在 `agentos.env` 里设 `AGENTOS_DESKTOP=taskd` 可改为连接任务服务（没有按住说话的语音）。从 v0.2.x 在线更新上来时，任务服务在下次登录后才开始运行。
- **行为变化：用完规划轮数的任务记为失败**。此前管家连续执行 8 轮后以“先停在这里”结束并记为已完成；现在是“超出预算：规划轮数”，已完成的步骤保留。
- **行为变化：其他对话不再拿到受污染任务的结果**。此前“最近任务”的摘要会把读过外部内容的任务的结果放进下一个任务的提示里；现在只告诉规划器那个任务发生过。
- 修复：MCP 请求被放弃后，等待表里的条目不再留到响应到达为止。
- 修复：在 umask 为 002 的桌面会话里（Debian、Ubuntu 的默认设置），`agentos-taskd` 新建的状态目录同组可写，随后它拒绝把 socket 放进这个目录，无法启动。现在新建的状态目录总是只有本人可访问。已有的、他人可写的目录仍然拒绝，不自动改权限；错误信息会写明是哪个目录、该怎么处理。

让结果可信：任务的结局、时间和任务历史都如实反映发生了什么。

- **“没听懂”不再记成“已完成”**：规划器没有办法处理的意图现在以独立的结局结束。`agentos-taskd` 新增任务终态 `unhandled`（CLI 显示“未能处理”，Web Shell 显示“没有听懂”）；`agentosd` 的任务新增 `unhandled` 和 `failed`，`done` 事件带上 `state`。规划器直接给出回答的任务仍是成功。
- **规划失败是错误，不是回复**：`agentosd` 的 Claude 和 OpenAI 兼容规划器在接口不可达、被拒绝或响应无效时，任务记为失败并以错误报告；此前会被当成普通回复、任务记为完成。子 Agent 规划失败或无法处理时，交还给管家的结果标记为失败。
- **`agentos-taskd` 的 `clock.now` 返回本机时区的日期和时间**，带星期和 UTC 偏移；此前只返回 UTC 时刻。结果里保留 `unix_ms`。
- **协议 v1 新增 `read`**：直接查询已授权的只读（L0）能力，不创建任务、不经过规划器。有副作用的能力和输出不可信的能力会被拒绝。新增错误码 `not_readable`、`capability_failed`。见 [docs/protocol-v1.md](docs/protocol-v1.md#直接读取)。
- **Web Shell 不再替用户提交意图**：读取后台笔记改走 `read`。此前连接后台、打开“记忆”空间或笔记变化后，界面会自动提交一条“看看笔记”任务，留在任务历史和审计日志里，接入模型规划器时还会多一次模型调用。
- **CLI**：`agentos submit --wait [--timeout 秒]` 等到任务结束再返回，退出码区分成功、未成功、等待批准和超时；`agentos read <能力>` 直接读取；`tasks --state unhandled` 可筛选；`capabilities` 的风险等级写作 L0–L3。
- **`--help` 和 `--version`**：`agentosd`、`agentos-shell`、`agentos-taskd`、`agentos-web-shell` 都支持。此前 `agentosd --help` 会直接启动守护进程，`agentos-shell --help` 会直接打开桌面窗口。`agentosd` 和 `agentos-shell` 遇到不认识的参数现在报错退出，不再忽略。

合并成一套系统：`agentos-taskd` 成为唯一的任务内核，原来只有 `agentosd` 才有的能力都迁了进来。

- **任务内核支持多轮规划**：每轮的结果交还给规划器，由它决定下一轮或回答，最多 8 轮；步骤逐轮追加到任务上。一步失败后同轮后续步骤标为 `skipped`，规划器可以换个办法；最后一轮有调用没成功的任务记为失败。
- **迁入任务服务**：子 Agent 委派（步骤记在同一个任务里并标上 Agent）、共享黑板、跨任务上下文、裁判（`AGENTOS_JUDGE=1`）、污点传播、MCP（状态目录的 `mcp.json`）、自我进化插件、Home Assistant、L3 的 PIN（`agentos-taskd --set-pin`）。
- **L3 不再一律拒绝**：设置 PIN 后，不可逆的步骤进入等待批准，批准时必须带 PIN（`agentos approve … --pin-stdin`，或在 Web Shell、原生桌面的批准面板里输入）；没有设置 PIN 时仍然拒绝。
- **一份审计日志实现**：同步落盘、单一写入者，再加哈希链（`agentos-taskd --verify-audit`）。只容忍崩溃造成的最后一行不完整；其他损坏或改动会让守护进程把原文件改名保留、另开新日志并说明原因。两个守护进程都用它。
- **共享 crate**：`agentos-planner`（规划契约、规则规划器、Claude 与 OpenAI 兼容规划器，合并了两边的实现：多轮接口加上更严格的响应解析）和 `agentos-services`（能力注册表、子 Agent、黑板、裁判、PIN、MCP、插件、智能家居）。`agentosd` 和 `agentos-taskd` 里重复的文件已删除。
- **原生桌面可以连接任务服务**：`agentos-shell --taskd` 通过协议 v1 工作。这个模式下暂时没有本机语音。
- **修复：原生桌面的确认丢失任务编号和 PIN**。桌面壳转发确认时只保留了“同意/拒绝”，所以设置 PIN 后 L3 操作在图形界面里无法确认，多个任务等待时回答总是落到最新的那个。
- **规则规划器的行为变化（`agentosd`）**：只认完整的命令。“看看我的笔记”“把笔记清空”这类包含关键词的句子不再匹配，需要说“看看笔记”“清空笔记”。
- 文档：两份架构说明合并为 [docs/architecture.md](docs/architecture.md)，新增 [AGENTS.md](AGENTS.md)。

**尚未完成**：图形桌面默认仍连接 `agentosd`，因为按住说话的本机语音还由它承载；见[架构](docs/architecture.md#尚未合并的部分)。

**兼容性**：协议 v1 新增了 `read` 方法、`unhandled` 任务状态、步骤的 `id` / `agent` 字段和 `skipped` 状态、确认请求的 `verify_identity`、`approve` 的 `pin`。严格解析的旧客户端需要升级；任务快照升到 schema 3，旧快照自动读取，新快照不保证旧程序可读。`agentos-taskd` 的任务在一步失败后不再立即结束，而是把失败交还给规划器。

v0.4.0 在协议 v1 上又追加了：`reply`、`preauthorize`、`preauthorizations`、`revoke_preauthorization`、`register_caller`、`callers`、`revoke_caller` 方法；请求信封的 `caller`；`awaiting_input` 任务状态；任务的 `thread`、`question`、`usage` 字段；`submit` 和 `list` 的 `thread`；错误码 `invalid_reply`、`not_preauthorizable`、`forbidden`。不认识 `awaiting_input` 的客户端应把它当作尚未结束的任务。任务快照升到 schema 4，旧快照自动读取，新快照旧程序读不了，所以升级后不能带着状态目录退回旧版本。审计日志新增了事件类型，旧记录照常可读。

## v0.3.0

- 新增 `agentos-taskd` 持久任务服务、协议 v1、独立 `agentos` CLI 和终端工作台。
- 新增 `agentos-web-shell`：基于真实任务上下文生成受限布局，支持 WebRTC 连续语音与随时打断。
- 保留 `agentosd` / `agentos-shell` 的多轮规划、MCP、插件、智能家居、PIN、哈希链审计、设置助理和镜像更新。新旧入口使用独立状态目录与协议。
- IPC 无字段命令同样严格拒绝未知字段，新增全命令校验、线格式兼容与真实 socket 回归测试。
- 最低 Rust 版本更新为 1.88。

## v0.2.2（2026-11-02）

这一版主要是真机测试（Intel N100 小主机、笔记本）发现的问题修复，外加图形设置助理。没有 v0.2.1，版本号直接从 v0.2.0 跳到 v0.2.2。

**从 v0.2.0 升级：** `sudo agentos-update` 会更新 AgentOS 自己的文件（桌面、设置助理、会话脚本、sway 配置），但不会安装新的系统软件包。显卡固件和输入法候选窗口这两个修复靠新软件包，在线升级拿不到。建议重新写入镜像；不想重装的话，在线升级后再运行（Ubuntu 镜像不需要第一条）：

```
sudo apt install firmware-intel-graphics firmware-intel-misc firmware-nvidia-graphics firmware-mediatek firmware-sof-signed intel-microcode amd64-microcode
sudo apt install fcitx5-frontend-gtk3 fbterm
```

### 改进

- **全新的设置助理**：首次进入桌面时不再弹出终端里的对话框，而是一个 Liquid Glass 风格的图形设置助理：一页一件事，用鼠标点选；Wi‑Fi 列表点一下就能加入；OpenAI 兼容接口带 DeepSeek、通义千问、Ollama 预设；Home Assistant 可以当场测试连接；输入错误就地提示；已保存的密钥留空即保持不变，手动加在 env 文件里的设置会保留。系统菜单「网络和 AI 设置…」打开同一个界面。文本模式仍用原来的向导。见 [docs/shell.md](docs/shell.md#设置助理)。（#30）
- **笔记本触摸板轻触点击**：默认开启，单指点按是左键、双指右键、三指中键。（#31）

### 修复

- **Debian 镜像在 Intel 核显（如 N100）上进不了桌面**：Debian 13 把显卡固件拆成了单独的包，镜像里缺 i915 GuC 固件，sway 启动失败。现在镜像带上 Intel、NVIDIA、MediaTek 固件、声卡 SOF 固件和 CPU 微码。（#29）
- **桌面起不来时多一次机会**：先等显卡设备就绪；GPU 渲染失败时用 CPU 渲染再试一次；失败原因写入日志（`journalctl -t agentos-sway`）。（#29）
- **文本模式中文显示成方块**：文本模式改在 fbterm 里运行，中文正常显示。（#29）
- **拼音输入法不显示候选字**：补上 fcitx5 的 GTK 输入模块。（#29）

## v0.2.0（2026-11-02）

**从 v0.1.0 升级：请重新写入镜像。** v0.1.0 镜像里 `sudo` 会要密码（账户没有密码），无法以 root 运行 `agentos-update`。从 v0.2.0 开始可以用 `sudo agentos-update` 在线升级。详见 [docs/update.md](docs/update.md)。

### 新功能

- **超级管家**：管家把任务拆开派给子 Agent；任务即进程，有检查点、可恢复；共享黑板；可选的模型裁判（`AGENTOS_JUDGE=1`）在执行有影响的步骤前复核。见 [docs/agents.md](docs/agents.md)。（#16）
- **Home Assistant 智能家居**：`home.list` / `home.state` / `home.control` / `home.run` / `home.secure`，开锁、开车库门等按 L3 处理，每次确认。见 [docs/smart-home.md](docs/smart-home.md)。（#20）
- **MCP 能力**：`~/.local/share/agentos/mcp.json` 里配置的 MCP 服务器，其工具自动注册为能力，按风险分级、输出视为不可信。见 [docs/mcp.md](docs/mcp.md)。（#21）
- **L3 PIN**：设置 PIN 后，不可逆操作确认时要输入 PIN（`agentosd --set-pin`，或设置向导）。PIN 不写入审计日志。（#18、#22）
- **设置向导**：新增 Home Assistant 连接和 PIN 两步。（#22）
- **Caps Lock 按键说话**：按住 Caps Lock 说话，键盘指示灯作为录音灯。（#19）
- **在线更新（OTA）**：`agentos-update` 从 GitHub Release 下载更新包，校验 sha256、先用新版本自检再替换，可 `--rollback`；系统菜单里有「检查更新…」。（#25）

### 安全与稳健性

- **污点传播**：网络插件、未沙箱插件和 MCP 工具的输出会把本次任务标记为不可信，之后有影响的操作一律要确认。（#18）
- **审计日志哈希链**：每条记录带上一条的 SHA-256，`agentosd --verify-audit` 检查是否被改动；v0.1.0 的旧日志照常读取。（#23）
- **插件资源限制**：内存 1 GiB、文件 64 MiB、CPU 10 秒、打开文件 256 个。（#24）
- **崩溃日志**：panic 报告写入 journal 和 `crash.txt`，下次启动在活动面板里显示；守护进程反复崩溃时退避重启；journal 跨重启保留。（#26）
- 笔记重启后保留、`clock.now` 用本地时间、审计日志容忍写了一半的行、socket 单行 64 KiB 上限。（#17）
- 规划器对 408/429/5xx/529 自动重试。（#18）
- CI 加入 cargo-deny：已知漏洞、许可证和依赖来源检查，每周跑一次。（#27）

### 修复

- 镜像里 `sudo` 要密码：根文件系统不再沿用 CI 的 uid，构建时检查 `agent` 能免密 sudo。（#15）

### 界面

- 玻璃风格终端和设置向导配色。（#19）

### 尚未在真机上验证

- Caps Lock 按键说话和指示灯、真实麦克风和扬声器
- 连接真实的 Home Assistant 和真实的 MCP 服务器
- 从一个真实发布版本在线升级到下一个版本（目前只有 CI 里的模拟测试）

## v0.1.0（2026-11-01）

第一个预览版：管家核心、Claude 和 OpenAI 兼容规划器、可启动安装镜像（Debian 13 / Ubuntu 24.04）、自我进化插件、玻璃风格桌面、语音交互。
