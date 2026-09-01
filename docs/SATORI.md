# Satori 接入说明

## 连接模型

`ayjx` 连接 Satori v1 实现端的 `/v1/events` WebSocket，并在连接建立后于
10 秒内发送 `IDENTIFY`。收到 `READY` 后，适配器从首个 login 建立
`BotStatus`，再触发插件的 connected 生命周期。事件流支持：

- `EVENT`：转为框架内部的规范化事件后进入插件流水线；
- `PING`：立即回复 `PONG`；
- 连接关闭或失败：从 3 秒开始指数退避，最长 60 秒。

实现端地址同时作为 HTTP API 根地址。配置接受 `http(s)://`，也兼容传入
`ws(s)://` 或带 `/v1/events` 的完整事件地址，启动时会统一规范化。

## 鉴权

token 的优先级如下：

1. 环境变量 `AYJX_SATORI_TOKEN`；
2. `config.toml` 中 Satori bot 的 `access_token`；
3. 两者均为空时不鉴权。

同一个 token 会用于 WebSocket `IDENTIFY.body.token` 和 HTTP
`Authorization: Bearer ...`。HTTP 请求同时携带 `Satori-Platform` 与
`Satori-User-ID`，由 `READY` 返回的 login 决定路由。

## API 映射

| 框架能力 | Satori / satori-qq 方法 |
| --- | --- |
| 发送群聊/私聊消息 | `message.create` |
| 获取引用消息 | `message.get` |
| 撤回消息 | `message.delete` |
| 群列表 | `guild.list` |
| 群成员资料 | `guild.member.get` |
| 添加/取消表态 | `reaction.create` / `reaction.delete` |
| 发送文件 | `upload.create` multipart + `message.create` `<file>` |
| 群头衔 | `internal/special_title` |
| 资料卡点赞 | `internal/like` |
| 合并转发读取 | `internal/get_forward` |

HTTP RPC 自带同步响应，因此需要保证“先图后文”时可直接等待
`message.create` 返回，不再需要 WebSocket echo 匹配器。

## 事件规范化

插件仍通过 `Context::as_message()` 使用稳定的内部消息视图。适配器从原始
Satori event 提取频道、群、用户、成员角色、时间戳和消息 ID，并把
`message.content` 元素串解析为内部消息链。完整的原始 Satori event 保存在
规范化事件的 `_satori` 字段中，便于调试和后续扩展。

非消息事件会保留 `satori_type`，并对撤回、成员变化、申请、禁言与戳一戳
提供统一字段。全局群黑白名单在事件进入插件流水线前执行。

## 消息元素

双向转换覆盖：

- 文本与 XML 实体转义；
- `<at>`、`<quote>`、`<emoji>`；
- `<img>`、`<audio>`、`<video>`、`<file>`；
- `<json>`、`<mface>`、`<poke>`、骰子和猜拳；
- `<message forward>` 合并转发与自定义节点。

Satori/QQ NT 的消息 ID 可能超过 32 位范围，适配层和插件 API 均使用
`i64`，引用 ID 在元素串中保持十进制字符串。

## 本机实现端

当前适配目标是本机 `satori-qq`，其默认配置为
`http://127.0.0.1:3001`，平台名 `red`，适配器名 `satori-qq`。支持范围以
该仓库的 `docs/SATORI_SUPPORT.md` 为准。
