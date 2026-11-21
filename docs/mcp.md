# MCP 工具

任何通过 stdio 通信的 [MCP](https://modelcontextprotocol.io) server 都可以把自己的工具借给管家。每个工具注册成一个能力，名字是 `mcp.<server>.<工具>`，和其他能力一样经过策略引擎、审计和确认。

## 配置

在任务服务的状态目录（默认 `~/.local/state/agentos`，装机镜像里是 agent 用户的同一位置）放一个 `mcp.json`，格式和 Claude Desktop 的 `mcpServers` 相同，另外可以加 AgentOS 自己的风险和沙箱设置：

```json
{
  "mcpServers": {
    "fs": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/home/agent/work"],
      "risk": "L1",
      "tools": { "read_text_file": "L0", "list_directory": "L0" },
      "trusted": true,
      "sandbox": { "write": ["/home/agent/work"] }
    },
    "fetch": {
      "command": "uvx",
      "args": ["mcp-server-fetch"],
      "risk": "L0",
      "sandbox": { "read": ["~/.local/bin"] }
    }
  }
}
```

重启任务服务（`systemctl --user restart agentos-taskd`）后，它的日志（`journalctl --user -u agentos-taskd`）会显示每个 server 注册了几个工具、是否在沙箱里，或者为什么没有启动。

| 字段 | 含义 |
| --- | --- |
| `command` `args` | 启动命令 |
| `env` | 额外的环境变量 |
| `risk` | 这个 server 工具的默认风险等级 `L0`–`L3`；不写就是 **L2**（每次都要确认） |
| `tools` | 按工具名单独设定风险等级 |
| `trusted` | 结果来自你信任的来源；不写就当作不可信 |
| `sandbox` | 沙箱设置；不写就在沙箱里运行，见[沙箱](#沙箱) |
| `disabled` | 暂时停用 |

## 信任从哪里来

信任只来自 `mcp.json`，不来自 server 自己的声明：

- **风险等级由你定。** server 的 `readOnlyHint` 不会降低等级；`destructiveHint` 只会把等级抬到 L3。没设置时默认 L2。
- **结果默认不可信。** 不可信工具的结果会让任务带上污点，之后有副作用的步骤（L1 及以上）都要你确认，防止网页、邮件里的提示注入借工具动手。只读本地文件之类的 server 可以设 `"trusted": true`。
- **环境变量清空。** server 只拿到 `PATH` `HOME` `LANG` 等基本变量和 `env` 里写的，不会继承规划器的 API Key。`env` 通过环境变量交给 server，不出现在命令行上（本机其他用户能看到命令行）。
- **server 在沙箱里运行**，看不到你的主目录和 AgentOS 自己的文件，见下文。沙箱不能替你判断 server 拿着你给它的令牌会做什么，所以仍然只添加你信任的 server。

## 沙箱

MCP server 默认在 bubblewrap 沙箱里运行，能看到的只有这些：

- 系统的程序和库（`/usr`、`/bin`、`/lib`、`/opt` 等），只读；
- 一个属于它自己的私有主目录，放在你的主目录的位置上。这个目录实际存在状态目录的 `mcp/<server>/` 里，重启后还在，所以 `npx`、`uvx` 下载的包不用每次重下；
- `sandbox.read` 列出的路径（只读）和 `sandbox.write` 列出的路径（可写）；
- 网络，除非写了 `"network": false`。

AgentOS 自己的东西永远不会出现在沙箱里，包括状态目录（任务记录、笔记、PIN、预授权、审计日志、控制 socket）和旧版 agentosd 的数据目录。就算你把整个主目录交给它（`"read": ["~"]`），这些目录在沙箱里也是空的；把状态目录里的路径直接写进规则，则会被拒绝。原因是 server 以你的系统用户运行：不进沙箱的话，它可以改写策略文件，或者连上控制 socket，替你批准它自己的输出引出的那一步。

| `sandbox` 的写法 | 效果 |
| --- | --- |
| 不写，或写 `true` | 在沙箱里，可以联网，不额外开放路径 |
| `{"read": [...], "write": [...], "network": false}` | 在沙箱里，开放这些路径；路径写绝对路径或以 `~/` 开头。三项都可以省略 |
| `false` | 不进沙箱，以你的全部权限运行 |

规则写错时（字段名拼错、路径不存在、写了相对路径或整个根目录），这个 server 不会启动，日志里写明原因。写错的规则不会被悄悄放宽或收窄。

常见情况：

- **命令装在主目录里**（例如 nvm 装的 `npx`、uv 装的 `uvx`）。沙箱里看不到你的主目录，日志会写“命令 … 在沙箱里看不到”。把它所在的目录加进 `read`，例如 `"read": ["~/.nvm"]` 或 `"read": ["~/.local/bin"]`。
- **要读写文件的 server**（例如 `server-filesystem`）。参数里给它的目录也要写进 `write`（只读就写进 `read`），否则沙箱里没有这个目录。
- **用 Docker 启动的 server**。它需要 Docker 的 socket，而能用这个 socket 就等于有 root 权限，沙箱也就没有意义。这类 server 写 `"sandbox": false`，交给 Docker 隔离。

**没有可用的沙箱时**（macOS 开发机、没装 bubblewrap，或者内核禁止了非特权的用户命名空间），默认设置的 server 不会启动，日志里会说明。只有你在它的配置里写了 `"sandbox": false`，它才会以你的全部权限运行。

沙箱挡不住的：

- 开着网络时，server 和主机共用网络命名空间，能连到本机监听的端口和抽象 Unix socket（例如 Xwayland 的 X11 socket）。Web Shell 的本机接口要会话令牌，它拿不到。
- 沙箱只管它能碰到哪些文件和进程，管不了它用你给的令牌在外面做什么。
- 不限制 CPU 和内存。

## 反过来：AgentOS 作为 MCP server

别的 Agent 也可以把 AgentOS 当作工具来用。先注册一个外部调用方，写明它能用什么（默认什么都不给，只读能力也要列出）：

```sh
agentos caller add desk-helper --name 桌面助手 --allow notes.list --allow clock.now
agentos callers
agentos caller remove desk-helper
```

然后在那个 Agent 的 MCP 配置里把 AgentOS 加为一个 stdio server：

```json
{
  "mcpServers": {
    "agentos": { "command": "agentos", "args": ["mcp", "--caller", "desk-helper"] }
  }
}
```

它得到七个工具：`submit_intent`、`get_task`、`list_tasks`、`cancel_task`、`reply_to_task`、`list_capabilities`、`read_capability`。

- **它只能做清单里的事。** 提交的意图由管家规划，但规划器只拿得到清单里的能力。
- **有副作用的操作要你批准。** 它的任务里，L1 及以上的步骤默认都停下来等你在 AgentOS 里批准；它没有批准用的工具，也拿不到批准令牌。想让某类操作不必每次问，给它单独建预授权：`agentos preauthorize notes.append --caller desk-helper --prefix 'text=[助手]' --expires 7d`。
- **它看不到你的任务。** 只能查看和取消自己提交的任务，规划时也拿不到你其他任务的内容。
- **撤销立即生效。** `agentos caller remove` 之后，它进行中的任务也不再能调用任何能力。
- **它提交的文字不是你的指示。** 界面上这些任务标着“外部调用方”，审计里的来源是 `caller:<id>`。

`--allow` 可以重复，支持以 `*` 结尾的前缀（例如 `home.*`）；`--max-risk L0|L1|L2` 设置它最高能碰到的风险等级，默认 L0。再次 `caller add` 同一个 id 会整体替换清单。设置了 PIN 时加 `--pin-stdin`。

这个身份防的是 MCP 另一端的 Agent，不是本机上的任意程序：以你的系统用户运行的程序本来就能直接连接任务服务。

## 限制

- 只支持 stdio 传输，暂不支持 HTTP/SSE 远程 server。
- 只用到工具（tools）；资源（resources）、提示（prompts）和 server 向客户端发起的请求（sampling 等）暂不支持。
- server 退出后不会自动重启，重启 `agentos-taskd` 即可。
