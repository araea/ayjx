# Satori 插件兼容性审计

审计基线：`0146e91` 完成协议迁移后，对全部 22 个注册插件逐项检查事件字段、
消息元素、引用消息、主动 API 与发送拦截链，并与本机 `satori-qq` 只读联调。

| 插件 | 依赖路径 | 审计结果 |
| --- | --- | --- |
| `meta_filter` | 非消息事件 | Satori 协议帧不进入插件链；业务事件正常放行 |
| `logger` | 收发消息字段 | 群聊/私聊区分、发送包和富文本日志正常 |
| `recorder` | 消息段、用户/群字段 | 入站和 `BeforeSend` 记录正常；商城表情键已规范化 |
| `media` | 当前/引用消息媒体 URL | `message.get` 与资源地址正常 |
| `sticker` | 引用图片、可选撤回 | 引用 ID、图片读取和 `message.delete` 正常 |
| `group_title` | 自身成员角色、内部 API | `guild.member.get` 与 `internal/special_title` 映射正常 |
| `ping` | 回复、合并转发 | `<quote>` 与 `<message forward>` 正常 |
| `recall` | 引用与当前消息撤回 | 同频道 `message.delete` 正常 |
| `echo` | 原样消息段 | Satori 元素双向转换正常 |
| `repeater` | 消息链比较、发送前观察 | 图片 `src` 以 md5 为资源 ID，同图仍判定为复读 |
| `wordcloud` | 私聊/群聊作用域、数据库 | 私聊不再产生伪 `group_id=0` |
| `stats` | 群列表、定时多群发送 | `guild.list` 跟随 `next` 翻页；同步 HTTP 发送顺序正常 |
| `card` | 引用图片、文件发送 | 引用图片正常；文件先 `upload.create` 再发送 |
| `gif` | 当前/引用图片、转发节点 | URL 提取和合并转发正常 |
| `image_split` | 引用图片、批量图片发送 | 引用 ID 与批量发送正常 |
| `ciyi` | 群消息、引用、图片 | 指令与 base64 图片发送正常 |
| `webshot` | 文本 URL、引用回复 | 规范化文本段与回复正常 |
| `oai` | 引用消息、图像、表态、文件 | `message.get`、reaction 与 multipart 上传正常 |
| `ai_news` | 主动推送、合并转发 | HTTP 响应保证先图后文；重连不重复注册任务 |
| `settings` | 指令与配置持久化 | 事件/发送路径不受协议迁移影响 |
| `help` | 回复、图片 | `<quote>` 和 base64 图片发送正常 |
| `restart` | 回复、进程生命周期 | 走 `on_init`，不受 connected 去重影响 |

## 已消除的副作用

- 私聊事件不再写入 `group_id = 0`，避免日志和「本群/我的」类作用域误判；
- 出站文本只转义 `< > &`：实现端的反转义不认 `&apos;`，全量转义会把
  `don&apos;t` 这种半成品原样发进 QQ 消息；属性仍转义双引号；
- 文本段首尾的换行改用 `<br/>` 送出。Satori 规范把「含换行的首尾空白」当排版
  空白裁掉，`@某人\n[图片]`、`提取成功：\n[图片]` 的换行原本会被实现端吃掉；
- `mface` / `poke` 的连字符属性转换为插件既有的下划线键；
- 补齐 `at here`、`sharp`、链接、CDATA、资源元数据和 Satori 布尔属性处理；
- WebSocket 重连不再重复触发 connected 生命周期，避免 `stats` 与 `ai_news`
  的定时任务每次断线都叠加一份；
- 申请类事件的 `message.id` 是审批 flag（非数字），改为原样保留在 `flag`；
  禁言事件补出秒级 `duration`，并按 `duration = 0` 区分 `ban` / `lift_ban`；
- 文件不再把 Termux 私有路径直接交给 QQ 进程，而是先走标准 multipart
  `upload.create`，规避 Android 进程权限差异；
- 引用消息可以正常取回。实现端原先在引用元素里填 QQ NT 的 `replayMsgSeq`，而它的
  `message.get` / `message.delete` 只认 `msgId`，两端对不上；已在 `satori-qq`
  0.8.9.16 改为优先取 `replayMsgId`（详见该仓库 `Convert.java` 的 `case 7`）。

## 与官方协议的对齐

对照 [Satori 官方文档](https://satori.js.org/zh-CN/)逐条核对后补上的部分：

- **心跳方向**：协议规定 `PING` 由应用每 10 秒发出、实现端回 `PONG`。原来只被动
  回 `PONG`、从不主动发，连到会做心跳超时的实现端上会被断开；
- **会话恢复**：重连的 `IDENTIFY` 带上最后收到的事件 `sn`，实现端补推断线期间的
  事件。实测本机实现端缓冲区里有 283 条待补事件，原来这些全部静默丢弃；
- **资源链接**：`internal:` 链接和命中 `proxy_urls` 的平台链接改走 `/v1/proxy/{url}`，
  插件因此能取到原本下载不了的资源；
- **分页列表**：`guild.list` 跟随 `next` 令牌翻页；
- **事件归属**：`self_id` 优先取事件自带的 `login.user.id`，多登录场景才准确；
- **转义规则**：协议的转义表只有 `" & < >`，撇号不在其中。

官方在 crates.io 上占位的 `satori` crate 是空壳（0.0.0，`src/lib.rs` 为 0 字节），
`satori-rs` 是 Vercel 的图片渲染库，同名无关；第三方 `shirabe-core` 是一套完整的
机器人框架，接入等于重写 ayjx 的插件体系。因此协议层继续自持，只对齐规范本身。

## 回归验证

- 自动构造群聊和私聊规范化事件，依次通过全部注册插件；
- 消息元素测试覆盖普通元素、布尔属性、合并转发、撇号/引号转义与首尾换行；
- 事件测试覆盖手发消息虚拟作者、申请 flag、禁言与解禁；
- 本机 `satori-qq` 只读联调覆盖 `login.get`、`guild.list`、`guild.member.get`、
  `message.list`、`message.get`，并确认 `upload.create` multipart 返回
  `internal:red/...` 资源；
- 引用链路实测：修复前 `message.get` 对引用 ID 返回
  `message not found: 1911845`，修复后同样的引用位置返回 19 位 `msgId`
  并能取回原消息正文；
- 用桩实现端观察 ayjx 的信令：首次 `IDENTIFY` 不带 `sn`，10 秒发一次 `PING`，
  断线后以 `IDENTIFY {"sn": 2}` 恢复会话，且重连不重复跑 connected；
- `login.features` 已确认包含插件所需的标准 Satori 能力。

## 尚存的实现端限制

`satori-qq` 只有文本快照的历史消息（进程内没有 `msgRecord`），`message.get`
会退回 `raw_message`，此时 Satori `content` 里是 `[CQ:image,file=...]` 这种旧式
文本而非标准元素。引用这类老消息时，插件拿到的是一段文本而不是图片段。这与本次
协议迁移无关（OneBot11 时代同样如此），修法在实现端：回退时应把 CQ 文本转成
Satori 元素，而不是原样塞进 `content`。
