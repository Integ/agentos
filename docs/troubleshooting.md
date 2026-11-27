# 排障

出问题时先跑一条命令，它会告诉你哪里不对、该怎么办；下面是它覆盖不到、或者需要多解释几句的常见情况。

## 先运行 `agentos doctor`

```sh
agentos doctor                         # 默认状态目录（镜像里就是这个）
agentos --state-dir /tmp/agentos-dev doctor
agentos --json doctor                  # 同样的内容，给脚本或贴进 issue
```

它做三件事：

1. 连接任务服务，确认终端里的 `agentos` 和任务服务是同一个版本；
2. 读取任务服务对自己的报告（只读能力 `system.status`）：规划器、沙箱、PIN、文件工作区、每个 MCP server 有没有起来、上次有没有崩溃；
3. 连不上时，看本机能看到的东西：socket 在不在、是不是任务服务自己的，状态目录里有没有还没被处理的崩溃报告。

每个问题后面都有“怎么办”。退出码：`0` 没有需要处理的问题，`1` 连不上任务服务，`3` 任务服务在运行但有需要处理的问题。

在工作台、桌面或微信里对管家说“诊断一下”或“系统状态”，读到的是同一份报告；接了模型规划器时，问“为什么插件每次都要我确认”这类问题，管家也会先读它。

报告只在本机生成，不会发送到任何地方。贴进 issue 之前看一眼里面的路径、接口地址和主机名。

**重启任务服务**：镜像里运行 `systemctl --user restart agentos-taskd`（改了 `/etc/agentos/agentos.env` 之后也要这样）；自己运行的，停止后用同样的参数再启动。

## 常见问题

### 任务服务没有运行，或者连不上

- 镜像里：`systemctl --user status agentos-taskd` 看状态，`journalctl --user -u agentos-taskd`（或 `journalctl -t agentos-taskd`）看日志。它随登录启动，崩溃后自动重启。
- 自己运行的：`agentos-taskd` 和 `agentos` 要用同一个状态目录（`--state-dir` 或 `AGENTOS_STATE_DIR`），否则客户端找不到 socket。
- 自定义 `--socket` 时，它的父目录必须归你所有、别人不可写，不能直接放在 `/tmp` 根目录；路径也不能超过系统对 Unix socket 的长度限制。
- 服务起不来、日志里说某个状态文件“已损坏，读不出来”：它不会自动修复或覆盖那个文件（可能是你唯一的一份数据）。修好它，或者把它改名移走（例如加上 `.bad` 后缀）再启动，那一部分会从空白开始。

### 说什么都是“未能处理”

多半在用离线规则规划器，它只认识“记一下 …”“看看笔记”“现在几点”“诊断一下”这类固定说法，`agentos doctor` 的“规划器”一行会写明。设置 `ANTHROPIC_API_KEY`，或 `OPENAI_MODEL` 加 `OPENAI_API_KEY` / `OPENAI_BASE_URL`，然后重启任务服务；镜像里在「网络和 AI 设置…」里填。

反过来，环境里有这些密钥时任务服务会自动用模型规划器、发起真实请求。不想这样就用 `--planner rule`。

### 不可逆的操作总是被拒绝

L3 操作（删除数据、安装插件等）在没有设置 PIN 时一律拒绝。运行 `agentos-taskd --set-pin` 设置，正在运行的任务服务下一次核对时就会用上它，不需要重启；`--clear-pin` 也一样立即生效。

PIN 连续输错 5 次锁定 5 分钟，这期间换一个新 PIN 也不会解除锁定。换 PIN 后，用旧 PIN 刚批准、还没执行的步骤要重新批准。

PIN 文件坏了（`agentos doctor` 会把它列为错误）时，任务服务继续要求它最后读到的那个 PIN，不会当作没有 PIN。修好它，或者运行 `agentos-taskd --clear-pin` 再 `--set-pin` 重新设置（`--set-pin` 会先读旧文件，读不出来就停下）。

### 插件每次调用都要确认，MCP server 没有启动

两者都和沙箱有关。插件和 MCP server 在 bubblewrap 沙箱里运行；没有可用的沙箱时，插件每次调用都要你确认，MCP server 只有在 `mcp.json` 里写了 `"sandbox": false` 才会启动（那样它有你的全部权限）。Linux 上装上 bubblewrap（`sudo apt install bubblewrap`）后重启任务服务；macOS 上没有这个沙箱。

MCP server 在沙箱里还是起不来时，`agentos doctor` 会列出是哪一个，具体原因在任务服务的日志里。常见的是：

- 命令装在主目录里（nvm 装的 `npx`、uv 装的 `uvx`），要把它所在的目录写进 `sandbox.read`；
- server 要读写的目录没有写进 `sandbox.read` / `sandbox.write`；
- `sandbox` 规则写错，这个 server 不会启动。

另外，没有写 `"trusted": true` 的 MCP server，它的工具风险至少是 L2，每次调用都要确认，这是有意的。见 [mcp.md](mcp.md#沙箱)。

### 读过网页之后，什么都要确认

这是污点追踪：任务读过网页、邮件、MCP 工具等外部内容后，之后有影响的步骤都要你确认，因为那些内容里可能藏着指令。预授权也不适用于这样的任务。确认面板会写明“任务读过不可信的外部内容”，核对收件人、地址和内容再批准；不对就“只拒绝这一步”，任务会换别的办法继续。见[提示注入评测](injection-eval.md)。

### 任务显示“已中断”

任务服务重启时（包括崩溃后自动重启），没做完的任务标为“已中断”，不会自动重做：其中可能有已经生效的步骤。看一眼任务详情和实际结果，需要的话重新说一遍。

### 提交后连接断了，不知道成功没有

客户端不会自动重发提交、取消或批准：结果未知时重发可能做两遍。先用 `agentos tasks` 或工作台查一下，再决定要不要重新提交。

### 任务服务崩溃了

崩溃报告（带调用栈）写在状态目录的 `crash.txt` 和日志里。任务服务下次启动时把它改名为 `crash.prev.txt`，记一条审计，桌面的“最近活动”里也会显示；`agentos doctor` 在服务还没起来时显示 `crash.txt` 的第一行，起来之后显示上次崩溃的摘要。报告 bug 时把 `crash.prev.txt` 和 `agentos doctor` 的输出一起附上。

### 审计日志

`agentos-taskd --verify-audit` 校验哈希链，有记录被改过、删过或插入过时退出码为 1。任务服务启动时发现日志无法继续写入（损坏或被改动），会把原文件改名保留、开始新的日志，`agentos doctor` 会把这件事列为错误：保留那个文件，用 `agentos-taskd --verify-audit --audit <那个文件>` 查看哪一行断开。

### 终端里的 `agentos` 和任务服务版本不一致

更新之后旧的任务服务还在运行时会这样。重启任务服务；镜像里在线更新后重新登录一次，桌面才换成新版本。

### 任务因为“超出预算”失败

每个任务有规划轮数、token 和执行时长的预算，单次能力调用最多 15 分钟。`AGENTOS_TASK_TOKENS`、`AGENTOS_TASK_SECONDS`、`AGENTOS_CALL_SECONDS` 可以调整（`0` 表示不限制），改完重启任务服务。当前的预算在 `agentos doctor` 里。
