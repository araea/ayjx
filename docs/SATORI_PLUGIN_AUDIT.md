# Satori 插件兼容性审计

审计基线：`0146e91` 完成协议迁移后，对全部 22 个注册插件逐项检查事件字段、
消息元素、引用消息、主动 API 与发送拦截链，并与本机 `satori-qq` 只读联调。

| 插件 | 依赖路径 | 审计结果 |
| --- | --- | --- |
| `meta_filter` | 非消息事件 | Satori 协议帧不进入插件链；业务事件正常放行 |
| `logger` | 收发消息字段 | 群聊/私聊区分、发送包和富文本日志正常 |
| `recorder` | 消息段、用户/群字段 | 入站和 `BeforeSend` 记录正常；商城表情键已规范化 |
| `media` | 当前/引用消息媒体 URL | 当前消息正常；引用消息见下方已知缺陷 |
| `sticker` | 引用图片、可选撤回 | 图片读取和 `message.delete` 正常；引用见已知缺陷 |
| `group_title` | 自身成员角色、内部 API | `guild.member.get` 与 `internal/special_title` 映射正常 |
| `ping` | 回复、合并转发 | `<quote>` 与 `<message forward>` 正常 |
| `recall` | 引用与当前消息撤回 | 当前消息 `message.delete` 正常；引用见已知缺陷 |
| `echo` | 原样消息段 | Satori 元素双向转换正常 |
| `repeater` | 消息链比较、发送前观察 | 图片 `src` 以 md5 为资源 ID，同图仍判定为复读 |
| `wordcloud` | 私聊/群聊作用域、数据库 | 私聊不再产生伪 `group_id=0` |
| `stats` | 群列表、定时多群发送 | `guild.list` 单页返回全部群；同步 HTTP 发送顺序正常 |
| `card` | 引用图片、文件发送 | 文件先 `upload.create` 再发送；引用见已知缺陷 |
| `gif` | 当前/引用图片、转发节点 | 当前消息 URL 提取和合并转发正常 |
| `image_split` | 引用图片、批量图片发送 | 批量发送正常；引用见已知缺陷 |
| `ciyi` | 群消息、引用、图片 | 指令与 base64 图片发送正常 |
| `webshot` | 文本 URL、引用回复 | 规范化文本段与回复正常 |
| `oai` | 引用消息、图像、表态、文件 | reaction 与 multipart 文件上传正常；引用见已知缺陷 |
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
  `upload.create`，规避 Android 进程权限差异。

## 已知缺陷：引用消息 ID（实现端侧）

`satori-qq` 的入站引用元素填的是 QQ NT **`replayMsgSeq`**（`Convert.java`
`case 7`），而它的 `message.get` / `message.delete` 只认 **`msgId`**
（`MsgStore.resolve` → `getByMsgId`，`QQClient.fetchRecord` → `getMsgsByMsgId`）。
两端对不上，因此任何「引用某条消息再发指令」的路径都拿不到原消息。

本机实测（只读）：

```
message.list  → "[CQ:reply,id=1911845]…"      # 引用 ID 为 6~7 位 msgSeq
message.get   → {"message":"message not found: 1911845"}
```

`resolve` 在 `getByMsgId` 落空后还会把该数字当作进程内 legacy int 存储 ID
（`store.get((int) n)`）。长时间运行后这个自增计数会进入 msgSeq 的数量级，
存在解析到**同频道另一条消息**的风险——对 `message.delete` 即撤回错消息。
另外 `fetchRecord` 会以 250ms 间隔重试 6 次，每次失败的引用解析要阻塞约 1.5 秒。

这属于实现端自身契约不一致（`docs/SATORI_SUPPORT.md` 明确「消息 `id` 为 QQ NT
`msgId` 字符串」），ayjx 侧无法用 `message.seq` 游标安全反查——实测
`message.list` 的 seq 游标是近似定位，猜错就会撤错消息。建议在 `satori-qq`
的 `Convert.java` 改为优先取 `replayMsgId`，seq 仅作兜底。

## 回归验证

- 自动构造群聊和私聊规范化事件，依次通过全部注册插件；
- 消息元素测试覆盖普通元素、布尔属性、合并转发、撇号/引号转义与首尾换行；
- 事件测试覆盖手发消息虚拟作者、申请 flag、禁言与解禁；
- 本机 `satori-qq` 只读联调覆盖 `login.get`、`guild.list`、`guild.member.get`、
  `message.list`、`message.get`，并确认 `upload.create` multipart 返回
  `internal:red/...` 资源；
- `login.features` 已确认包含插件所需的标准 Satori 能力。
