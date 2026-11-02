# MCP 工具

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

## 限制

- 只支持 stdio 传输，暂不支持 HTTP/SSE 远程 server。
- 只用到工具（tools）；资源（resources）、提示（prompts）和 server 向客户端发起的请求（sampling 等）暂不支持。
- server 退出后不会自动重启，重启 agentosd 即可。
