# MCP 工具

> **在任务服务上。** `agentos-taskd` 读取状态目录里的 `mcp.json`（默认 `~/.local/state/agentos/mcp.json`），格式和行为与下文相同；修改后重启 `agentos-taskd`。下文的数据目录指旧的 `agentosd`。

任何通过 stdio 通信的 [MCP](https://modelcontextprotocol.io) server 都可以把自己的工具借给管家。每个工具注册成一个能力，名字是 `mcp.<server>.<工具>`，和其他能力一样经过策略引擎、审计和确认。

## 配置

在数据目录（默认 `~/.local/share/agentos`，装机镜像里是 agent 用户的同一位置）放一个 `mcp.json`，格式和 Claude Desktop 的 `mcpServers` 相同，另外可以加 AgentOS 自己的风险设置：

```json
{
  "mcpServers": {
    "fs": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/home/agent/work"],
      "risk": "L1",
      "tools": { "read_text_file": "L0", "list_directory": "L0" },
      "trusted": true
    },
    "fetch": {
      "command": "uvx",
      "args": ["mcp-server-fetch"],
      "risk": "L0"
    }
  }
}
```

重启 agentosd 后，控制台会显示每个 server 注册了几个工具。

| 字段 | 含义 |
| --- | --- |
| `command` `args` | 启动命令 |
| `env` | 额外的环境变量 |
| `risk` | 这个 server 工具的默认风险等级 `L0`–`L3`；不写就是 **L2**（每次都要确认） |
| `tools` | 按工具名单独设定风险等级 |
| `trusted` | 结果来自你信任的来源；不写就当作不可信 |
| `disabled` | 暂时停用 |

## 信任从哪里来

信任只来自 `mcp.json`，不来自 server 自己的声明：

- **风险等级由你定。** server 的 `readOnlyHint` 不会降低等级；`destructiveHint` 只会把等级抬到 L3。没设置时默认 L2。
- **结果默认不可信。** 不可信工具的结果会让任务带上污点，之后有副作用的步骤（L1 及以上）都要你确认，防止网页、邮件里的提示注入借工具动手。只读本地文件之类的 server 可以设 `"trusted": true`。
- **环境变量清空。** server 只拿到 `PATH` `HOME` `LANG` 等基本变量和 `env` 里写的，不会继承规划器的 API Key。
- **server 本身没有沙箱**，以桌面用户的身份运行。只添加你信任的 server。

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
- server 退出后不会自动重启，重启 agentosd 即可。
