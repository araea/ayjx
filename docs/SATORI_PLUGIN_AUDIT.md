# Satori 插件兼容性审计

基线：协议迁移提交 `0146e91`。对全部 22 个注册插件逐项检查事件字段、消息元素、
引用消息、主动 API 与发送拦截链，并与本机 `satori-qq` 只读联调。

| 插件 | 依赖路径 | 结果 |
| --- | --- | --- |
| `meta_filter` | 非消息事件 | Satori 协议帧不进入插件链，业务事件正常放行 |
| `logger` | 收发消息字段 | 群聊/私聊区分、发送包与富文本日志正常 |
| `recorder` | 消息段、用户/群字段 | 入站与 `BeforeSend` 记录正常，商城表情键已规范化 |
| `media` | 当前/引用消息媒体 URL | `message.get` 与资源地址正常 |
| `sticker` | 引用图片、可选撤回 | 引用 ID、图片读取与 `message.delete` 正常 |
| `group_title` | 自身成员角色、内部 API | `guild.member.get` 与 `internal/special_title` 映射正常 |
| `ping` | 回复、合并转发 | `<quote>` 与 `<message forward>` 正常 |
| `recall` | 引用与当前消息撤回 | 同频道 `message.delete` 正常 |
| `echo` | 原样消息段 | 元素双向转换正常 |
| `repeater` | 消息指纹比较、发送前观察 | 图片取资源 ID 比较，换链接的同图仍判定为复读 |
| `wordcloud` | 私聊/群聊作用域、数据库 | 私聊不再产生伪 `group_id = 0` |
| `stats` | 群列表、定时多群发送 | `guild.list` 跟随 `next` 翻页，同步发送顺序正常 |
| `card` | 引用图片、文件发送 | 引用图片正常，文件先 `upload.create` 再发送 |
| `gif` | 当前/引用图片、转发节点 | URL 提取与合并转发正常 |
| `image_split` | 引用图片、批量图片发送 | 引用 ID 与批量发送正常 |
| `ciyi` | 群消息、引用、图片 | 指令与 base64 图片发送正常 |
| `webshot` | 文本 URL、引用回复 | 规范化文本段与回复正常 |
| `oai` | 引用消息、图像、表态、文件 | `message.get`、reaction 与 multipart 上传正常 |
| `ai_news` | 主动推送、合并转发 | HTTP 响应保证先图后文，重连不重复注册任务 |
| `settings` | 指令与配置持久化 | 不受协议迁移影响 |
| `help` | 回复、图片 | `<quote>` 与 base64 图片发送正常 |
| `restart` | 回复、进程生命周期 | 走 `on_init`，不受 connected 去重影响 |

## 审计中的修正

对照 [Satori 官方文档](https://satori.js.org/zh-CN/)逐条核对后改动的部分：

- **心跳方向**：协议规定 `PING` 由应用每 10 秒发出、实现端回 `PONG`。原来只被动
  回 `PONG`，连到会做心跳超时的实现端上会被断开；
- **会话恢复**：重连的 `IDENTIFY` 带上最后收到的事件 `sn`。实测本机实现端缓冲区
  里有 283 条待补事件，此前这些全部静默丢弃；
- **资源链接**：`internal:` 链接与命中 `proxy_urls` 的平台链接改走
  `/v1/proxy/{url}`，插件因此能取到原本下载不了的资源；
- **分页列表**：`guild.list` 跟随 `next` 令牌翻页，群数量超过单页上限时不漏群；
- **事件归属**：`self_id` 优先取事件自带的 `login.user.id`，多登录场景才准确；
- **转义规则**：协议转义表只有 `" & < >`。全量转义会把 `don&apos;t` 这种半成品
  原样发进 QQ 消息，因为实现端的反转义不认 `&apos;`；属性仍转义双引号；
- **首尾换行**：文本段首尾的换行改用 `<br/>` 送出。Satori 把「含换行的首尾空白」
  当排版空白裁掉，`@某人\n[图片]` 的换行原本会被实现端吃掉；
- **私聊字段**：私聊事件不再写入 `group_id = 0`，避免「本群/我的」类作用域误判；
- **元素属性**：`mface` / `poke` 的连字符属性转换为插件既有的下划线键，并补齐
  `at here`、`sharp`、链接、CDATA、资源元数据与布尔属性；
- **连接生命周期**：WebSocket 重连不再重复触发 connected，避免 `stats` 与
  `ai_news` 的定时任务每次断线叠加一份；
- **申请与禁言**：申请类事件的 `message.id` 是审批 flag（非数字），改为原样保留在
  `flag`；禁言事件补出秒级 `duration`，并按 `duration = 0` 区分 `ban` /
  `lift_ban`；
- **文件发送**：不再把 Termux 私有路径交给 QQ 进程，改走标准 multipart
  `upload.create`，规避 Android 进程权限差异；
- **引用消息**：实现端原先在引用元素里填 QQ NT 的 `replayMsgSeq`，而其
  `message.get` / `message.delete` 只认 `msgId`，两端对不上。已在 `satori-qq`
  0.8.9.16 改为优先取 `replayMsgId`（见该仓库 `Convert.java` 的 `case 7`）。

协议层继续自持：crates.io 上的 `satori` 是 0.0.0 空壳，`satori-rs` 是同名无关的
图片渲染库，第三方 `shirabe-core` 自带一套插件体系，接入等于重写 ayjx。

## 回归验证

- 自动构造群聊与私聊规范化事件，依次通过全部注册插件；
- 消息元素测试覆盖普通元素、布尔属性、合并转发、撇号/引号转义与首尾换行；
- 事件测试覆盖手发消息虚拟作者、申请 flag、禁言与解禁；
- 本机 `satori-qq` 只读联调覆盖 `login.get`、`guild.list`、`guild.member.get`、
  `message.list`、`message.get`，并确认 `upload.create` multipart 返回
  `internal:red/...` 资源；
- 引用链路：修复前 `message.get` 返回 `message not found: 1911845`，修复后同一
  引用位置返回 19 位 `msgId` 并能取回原消息正文；
- 桩实现端观察信令：首次 `IDENTIFY` 不带 `sn`，每 10 秒一次 `PING`，断线后以
  `IDENTIFY {"sn": 2}` 恢复会话，重连不重复跑 connected；
- `login.features` 包含插件所需的标准 Satori 能力。

## 尚存的实现端限制

`satori-qq` 只有文本快照的历史消息（进程内没有 `msgRecord`），`message.get` 会
退回 `raw_message`，此时 Satori `content` 里是 `[CQ:image,file=...]` 这类旧式文本
而非标准元素；引用这类老消息时，插件拿到的是一段文本而不是图片段。这与协议迁移
无关（OneBot11 时期同样如此），修法在实现端：回退时应把 CQ 文本转成 Satori 元素。
