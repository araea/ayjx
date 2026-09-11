# Satori 接入说明

适配层实现 [Satori v1 协议](https://satori.js.org/zh-CN/)，不绑定特定实现端。

## 连接

ayjx 连接实现端的 `/v1/events` WebSocket，并在连接建立后 10 秒内发送 `IDENTIFY`。收到 `READY` 后从首个 login 建立 `BotStatus`，再触发插件的 connected 生命周期。

- `EVENT`：记下 `sn`，转为框架内部的规范化事件后进入插件流水线
- `PING`：本端每 10 秒发出，实现端回 `PONG`；收到反向 `PING` 也回 `PONG`
- `META`：刷新代理路由列表
- 连接关闭或失败：从 3 秒开始指数退避，最长 60 秒

重连时的 `IDENTIFY` 带上最后收到的事件 `sn`，实现端据此补推断线期间的事件；首次连接不带 `sn`，表示新会话。

实现端地址同时作为 HTTP API 根地址。配置接受 `http(s)://`，兼容 `ws(s)://` 与带 `/v1/events` 的完整事件地址，启动时统一规范化。

## 鉴权

token 取值顺序：环境变量 `AYJX_SATORI_TOKEN`、`config.toml` 中 Satori bot 的 `access_token`，两者均为空时不鉴权。同一 token 用于 WebSocket 的 `IDENTIFY.body.token` 与 HTTP 的 `Authorization: Bearer ...`。HTTP 请求另带 `Satori-Platform` 与 `Satori-User-ID`，取值由 `READY` 返回的 login 决定。

## 资源链接

消息元素里的 `src` 不一定能直接下载。适配层按协议的[资源链接](https://satori.js.org/zh-CN/advanced/resource.html)规范，为插件解析出可直接 GET 的 `url`：

- `internal:` 链接（平台内部资源、`upload.create` 的产物）走实现端的 `/v1/proxy/{url}`，该路由不需要鉴权头
- 命中 `READY` / `META` 下发的 `proxy_urls` 前缀的平台链接同样走代理路由，这类链接通常有防盗链或时效
- 其余 http(s) 链接直连
- `data:`、`file:` 与本地路径没有下载地址，不写入 `url`

原始 `src` 保留在消息段的 `file` 字段，便于原样回传给实现端。

## API 映射

| 框架能力 | Satori / satori-qq 方法 |
| --- | --- |
| 发送群聊或私聊消息 | `message.create` |
| 获取引用消息 | `message.get` |
| 撤回消息 | `message.delete` |
| 群列表 | `guild.list` |
| 群成员资料 | `guild.member.get` |
| 添加或取消表态 | `reaction.create` / `reaction.delete` |
| 发送文件 | `upload.create` multipart + `message.create` `<file>` |
| 群头衔 | `internal/special_title` |
| 资料卡点赞 | `internal/like` |
| 合并转发读取 | `internal/get_forward` |
| 资源代理 | `GET /v1/proxy/{url}` |

HTTP RPC 自带同步响应：需要把后续引用与已发送消息关联时，可直接读取 `message.create` 返回的消息 ID，无需 WebSocket echo 匹配器。`guild.list` 等标准分页列表跟随响应里的 `next` 令牌翻页。

## 事件规范化

插件通过 `Context::as_message()` 使用稳定的内部消息视图。适配器从原始 Satori event 提取频道、群、用户、成员角色、时间戳与消息 ID，并把 `message.content` 元素串解析为内部消息链；完整的原始 event 存放在规范化事件的 `_satori` 字段。

非消息事件保留 `satori_type`，并对撤回、成员变化、申请、禁言与戳一戳提供统一字段。全局群黑白名单在事件进入插件流水线前执行。

## 消息元素

文本内容按协议只转义 `<`、`>`、`&`，属性额外转义 `"`。撇号不转义，协议的转义表里没有 `&apos;`。

协议规定「文本内容前后包含换行符的连续空白会被忽略」，因此文本段首尾的换行用 `<br/>` 送出，段内换行保持原样，否则 `@某人\n[图片]` 这类排版会被吃掉。

双向转换覆盖：

- 文本与 XML 实体转义
- `<at>`、`<quote>`、`<emoji>`
- `<img>`、`<audio>`、`<video>`、`<file>`
- `<json>`、`<mface>`、`<poke>`、骰子与猜拳
- `<message forward>` 合并转发与自定义节点

Satori 与 QQ NT 的消息 ID 可能超出 32 位范围，适配层与插件 API 一律使用 `i64`，引用 ID 在元素串中保持十进制字符串。

## 实现端

当前适配目标是本机 `satori-qq`：默认地址 `http://127.0.0.1:3001`，平台名 `red`，适配器名 `satori-qq`，支持范围以该仓库的 `docs/SATORI_SUPPORT.md` 为准。适配器只注册 `satori` 与本地测试用的 `console`，不含 OneBot 接入。

迁移后的逐插件检查结果见[插件兼容性审计](SATORI_PLUGIN_AUDIT.md)。

## 复读时效

复读状态在收到事件时同步更新，发送与其他插件仍异步执行，避免入库、媒体处理或任务调度改变接力顺序。指令、被交互等待器消费的输入与不可复读消息也会打断接力。

`[repeater].max_delay_ms` 默认 `3000`：优先按 `message.created_at`，缺失时按事件 `timestamp` 计算有效期，没有时间戳时从本地接收时间计算。过期消息打断接力，不参与历史补发后的复读计数。`0` 按 `1ms` 处理，不表示无限等待。

准备发送与全部 `BeforeSend` 钩子执行后都会检查接力是否仍有效。对 satori-qq，复读的 `message.create` 还附带非标准扩展：

```json
{"satori_qq": {"if_latest_message_id": "触发消息的 ID", "expires_at": 1788870003000}}
```

更新后的实现端会在出站队列等待结束后、媒体转换后及每次交给 QQ 内核前检查：同一频道出现任何更新的消息，或超过 Unix 毫秒截止时间，就返回空数组并放弃发送。普通插件发送不携带该条件，仍按原有队列与限速执行。

完整保护需要同时更新 ayjx 与 satori-qq。其他实现端或旧版 satori-qq 只有框架发送前的保护；HTTP 请求发出后，框架无法撤销其内部排队。即使双方都更新，已经交给 QQ 内核的消息仍可能受 QQ 网络或媒体上传耗时影响，这时无法保证显示顺序。
