# 智能家居（Home Assistant）

AgentOS 通过 [Home Assistant](https://www.home-assistant.io/) 控制家里的设备。HA 已经接好了 Zigbee、Matter、Wi-Fi 插座、空调、门锁等，管家只需要和它的 REST API 对话，不必自己实现每一种协议。

## 配置

1. 在 HA 里打开 **个人资料 → 安全 → 长期访问令牌**，创建一个令牌。建议给 AgentOS 单独建一个 HA 用户。
2. 设置环境变量（装机镜像里写进 `/etc/agentos/agentos.env`，然后 `pkill -x agentosd` 让它重新加载）：

```bash
HASS_URL=http://homeassistant.local:8123
HASS_TOKEN=eyJhbGciOi...
```

启动时会显示 `智能家居：Home Assistant（…）`。没配置时这些能力不会出现。

## 能力和风险等级

| 能力 | 等级 | 能做什么 |
| --- | --- | --- |
| `home.list` | L0 | 列出设备和状态，可按类型（`light`、`climate`…）或名称筛选 |
| `home.state` | L0 | 查一个设备的状态（温度、亮度、位置…） |
| `home.control` | L1 | 灯、开关、插座、风扇、空调、加湿器、音箱、扫地机、窗帘/百叶窗；锁门、布防 |
| `home.run` | L2 | 运行 HA 里的脚本、场景、自动化、按钮（它们可能做任何事，每次都要确认） |
| `home.secure` | L3 | 开锁、撤防、开关车库门/大门/门窗（确认 + PIN） |

几条保护：

- **按设备判断等级，不能降级。** 每次操作前先查实体的 `device_class`：窗帘、百叶窗（`blind` `curtain` `shade` `awning` `shutter`）算 L1；车库门、大门、门窗，以及没有标注类型的 cover，一律算 L3。模型如果想用 `home.control` 开车库门，会被拒绝并提示改用 `home.secure`。
- **只操作一个实体。** 目标永远是参数里的那一个 `entity_id`；`data` 只接受固定的参数（亮度、色温、温度、模式、位置、音量等），`area_id` 之类能扩大范围的字段会被拒绝。
- **只开放列出的服务。** `homeassistant.restart`、`shell_command` 等不在表里的服务一律不支持。
- **返回给模型的只有名称、状态和少量常用属性**，不会把实体的全部属性（可能来自外部数据源）交给规划器。

## 用法

接上 Claude 或 OpenAI 兼容规划器后，直接说“把客厅灯调到 40%”“书房空调设成 26 度”“大门锁了吗”。管家通常会先用 `home.list` 找到实体，再调用相应能力。规则规划器只认“家里设备”“智能家居”，用来列出设备。

## 以后

- 订阅 HA 的 WebSocket 事件，把门铃、人体传感器等状态变化当作感知事件送给管家。
- 在设置向导里加一步填写 HA 地址和令牌。
- 让 `home.list` 按区域（房间）分组。
