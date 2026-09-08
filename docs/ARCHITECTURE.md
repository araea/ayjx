# 架构说明

面向维护者与后续自动化任务的参考手册。只描述现状与约定，不描述历史。

- [目录结构](#目录结构)
- [事件流](#事件流)
- [插件系统](#插件系统)
- [插件编写约定](#插件编写约定)
- [出图与渲染](#出图与渲染)
- [配置与数据](#配置与数据)
- [新增一个插件](#新增一个插件)
- [构建与测试](#构建与测试)

## 目录结构

```
src/
  main.rs          启动：加载配置、初始化数据库、连接适配器、优雅退出
  adapters/        适配器。satori.rs 为 Satori WS/HTTP 实现，console.rs 供本地测试
  command.rs       指令解析与消息内容提取的公共工具
  config.rs        AppConfig 与插件配置读写，build_config 辅助函数
  event.rs         Context / EventType / MessageEvent 定义
  http.rs          全局 reqwest 客户端（Android CA 兼容），download_bytes
  matcher.rs       事件去重
  message.rs       Message 消息构建器（text/image/node_custom 等）
  plugins.rs       插件框架核心：Plugin 定义、注册宏、流水线、配置读写
  plugins/         各插件；registry.rs 为注册表（唯一的插件清单）
  render/          原生卡片渲染层：字体、画布、卡片部件（详见「出图与渲染」）
  scheduler.rs     定时任务（daily / interval / 周期推送，带 Pace 错峰）
  db/              sea-orm 实体与查询（SQLite，data/bot.db）
```

`res/` 存放插件的静态资源（词库、人格提示词、技能说明），`docs/` 是本手册所在，
`tests/` 是几个用 Node 跑的端到端脚本（前台指令、重启、Satori 工具）。

## 事件流

```
适配器收到事件 → Context 构造 → plugins::run()
  逐个执行启用的插件 handler：
    Ok(Some(ctx)) → 接力给下一插件（插件拥有 Context 所有权，可改写事件）
    Ok(None)      → 事件被消费，流水线结束
    Err           → 记 error 日志，按已消费处理，不会崩掉适配器
  流水线走完仍未消费 → 末尾派发 EventType::BeforeSend
```

Context 通过 Move 传递，不深拷贝事件。`plugins::send_fake_event` 可把伪造事件推回流水线。

插件的执行顺序**就是 `registry.rs` 里的书写顺序**。因此过滤类插件写在最前
（`meta_filter` 掐掉心跳与元事件），`ctl` 紧随其后确保管理入口不被任何插件截胡，
记录类（`logger`、`recorder`）在业务插件之前拿到原始消息。

## 插件系统

一个插件就是 `src/plugins/` 下的一个模块，需要提供三个必需项与两个可选钩子：

| 项 | 签名 | 说明 |
| --- | --- | --- |
| `handle` | `fn(Context, LockedWriter) -> BoxFuture<Result<Option<Context>, PluginError>>` | 必需，事件处理 |
| `default_config` | `fn() -> toml::Value` | 必需，通常是 `build_config(Config::default())` |
| `validate_config` | `fn(&toml::Value) -> Result<(), String>` | 必需，用真实配置类型反序列化 |
| `init` | `fn(Context) -> BoxFuture<Result<(), PluginError>>` | 可选，启动时建表 / 载入数据 |
| `on_connected` | 同 `handle` | 可选，Bot 连接就绪后注册推送任务 |

`Plugin` 结构除上述函数指针外还带一组**帮助元数据**，全部在注册表里声明：

| 字段 | 缺省 | 用途 |
| --- | --- | --- |
| `display_name` | 模块名 | 中文展示名；`/help`、`/ctl` 都认它 |
| `section` | `"misc"` | 帮助总览的分区代号，取值见 `help::SECTIONS` |
| `summary` | `""` | 一句话说明 |
| `commands` | `&[]` | 指令清单，`cmds![("指令", "说明"), …]` |

**帮助中心不再自带清单**：`/help` 与 `/ctl` 全部从注册表读这些字段，
所以新增插件只改 `registry.rs` 一处，帮助总览、插件详情与控制面板同时跟上。
`section` 写错会落到「其他」而不是消失，`summary` 漏填由 `help::tests` 当场拦下。

插件配置使用顶层 `[<name>] enabled`（例如 `[help]`），运行时每次事件从配置快照读取。
带生命周期钩子的插件如果启动时未开启，后来开启会等待重启初始化，避免调用未就绪的
handler；`/ctl list` 里标注为「待重启」。统一控制与部署说明见 [CONTROL.md](CONTROL.md)。

## 插件编写约定

**配置**：单一 Default 来源 + 容器级 `serde(default)`，缺字段自动回落，勿再写字段级 `default = "fn"`：

```rust
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Config { enabled: bool, /* ... */ }

impl Default for Config {
    fn default() -> Self { Self { enabled: true, /* ... */ } }
}

pub fn default_config() -> Value { build_config(Config::default()) }
```

读取用 `get_config_or_default(&ctx, "name")`（需 `T: Default`），反序列化失败会告警并回落默认值。

**指令匹配**：统一走 `crate::command`：

- `match_command(ctx, cmd)` / `first_command_match(ctx, &[cmd])` — 前缀类指令
- `match_word_command(ctx, cmd)` — 要求指令名后为空白或消息末尾，用于 ctl
- `strip_prefix(ctx, text)` — 自带正则匹配的指令（词云、stats 式）
- `extract_text_arg(&matched.args)` — 参数拼接为纯文本
- `get_image_url(ctx, writer, &args, reply_id)` — 取图（参数或引用）
- `find_url(text)` — 文本中提取第一个 http(s) URL

匹配到即处理，返回 `Ok(None)`；不属于本插件返回 `Ok(Some(ctx))` 放行。

**错误处理**：插件公开接口统一 `PluginError`（= `Box<dyn Error + Send + Sync>`），
可用 `PluginResult<T>` 别名；内部子模块可用 anyhow，但不要在边界外露。
发送消息失败直接 `?` 传播，流水线会记日志，不要 `let _ =` 吞错。

**发送消息**：统一 `crate::adapters::satori::send_msg(&ctx, writer, group_id, user_id, msg)`，
msg 支持 `Message`、`&str`、`String`。下载资源用 `crate::http::download_bytes(url)`。

**日志**：target 用 `"Plugin/<Name>"` 常量或字面量，命名与注册名一致（如 `Plugin/WordCloud`）。

## 出图与渲染

框架里有几条出图路线，按内容形态选，不要混用：

| 路线 | 依赖 | 谁在用 | 适用 |
| --- | --- | --- | --- |
| **原生卡片** `crate::render` | 系统 CJK 字体 | help、ctl | 版式确定的清单与说明 |
| **原生卡片（插件私有）** `ciyi/painter.rs` | 系统 CJK 字体 | ciyi | 宣纸风盘面 |
| **图表** plotters | 无 | stats、wordcloud | 坐标轴、折线、柱状、词云 |
| **浏览器截图** cdp_html_shot | Chrome/Chromium | webshot、ai_news、oai | 真实网页、资讯长图、任意 Markdown |

**结构化清单走原生**。浏览器那条路要拉起一个 Chromium 进程：慢、吃内存，而且是
唯一会「因为外部程序起不来」而整条链路失败的一环。help 与 ctl 的内容就是几行
字段，不需要排版引擎，原生几十毫秒画完。

**读的东西以人眼为准**。`ai_news` 的四张卡与 `ciyi` 的宣纸卡都为「被人一行行读完」
而排，行距、字重、留白差一点就明显难读——这两处的版式各自调到位，就不再为了省一个
进程去动它：ai_news 保持 HTML 截图，ciyi 保持自己那份 `painter.rs`。省下来的开销
不值得拿阅读体验去换。剩下的 `webshot` 要的就是真实网页的样子，`oai` 渲染的是模型
产出的任意 Markdown，表格、代码块、嵌套列表都要排——那正是浏览器存在的理由。

原生渲染层 `src/render/` 分三层，从下往上：

```
render/font.rs     字体装载 + 逐字符回退链（缺字不画豆腐块，按度量留位）
render/canvas.rs   逻辑像素画布：SDF 几何、文字光栅化与度量、折行、外投影
render/kit.rs      系统类卡片的成品部件：Theme + Block 序列 → PNG base64
```

- **逻辑像素**：排版代码只写逻辑坐标，绘制时统一乘设备比例 `s`（各插件的
  `image_scale`，1—4 倍，默认 3）。同一份版式换倍率不用改一个数字。
- **先量后画**：`kit::render` 先用一张 1×1 的量尺画布算出每个 Block 的高度，
  累加得到卡片真实高度后再开画布。既不会「先给足高度再裁」而静默截断，
  也不必猜上界。ciyi 的宣纸卡版式独特，用自己的 `painter.rs` 画，但同样先算后裁。
- **折行**：`Canvas::wrap` 西文按词断、中文避头点避尾点，末行超宽加省略号。
- **换皮不改版式**：`kit::Theme` 收拢全部配色。help 用 `blueprint()`（青绿），
  ctl 用 `graphite()`（琥珀），同一套版式语言、不同色相，一眼能分辨两张卡的来路。
- **字号与网页一致**：画布把字号作为 em 像素，按每个回退字体的
  `height_unscaled / units_per_em` 换算为 ab_glyph 的 `PxScale`。直接把字号传给
  `PxScale` 会把 CJK 字面缩小；度量、折行、墨迹与绘制必须共用换算。
- **手机阅读**：`kit.rs` 的 `FS_*` / `LH_*` 集中管理字号和行高，正文 19—20px、
  指令 22px；help 总览 720px、详情与 ctl 680px。长指令、别名、配置和页脚自动换行，
  保留完整参数；插件名称、配置键与说明分层排版。出图测试同时落盘 420px 手机预览。
- **迁移边界**：本次核对了全部浏览器调用点。资讯卡曾因原生排版降低阅读质量回退，
  当前保留其已有的日夜主题与长文版式；OAI 的任意 Markdown、webshot 的真实网页
  继续使用浏览器。原生迁移须先证明与现有页面同等易读，再替换线上实现。

**出图失败一律回退纯文本**。系统原生卡片在字体不可用或位图超过 6400 万像素时返回
`None`；浏览器截图则可能因缺少 Chrome 而失败。两种情况下插件都必须仍能把同样的
内容用文字讲清楚——图里一套、文字另一套是不允许的。

短反馈不出图：一句话的纠错、开关确认、报错走纯文本，出图既慢又刷屏，
还挡住了复制粘贴。ciyi 的 `Reply::wants_card` 与 ctl 的 `Output::card` 都是这条线。

## 配置与数据

- `config.toml` 不入库；首次启动写默认值，启动时补字段、清残留，解析失败则退出不覆盖
- 插件配置改动经 `plugins::update_config` 或 ctl 插件，持久化受 `config_save_lock` 串行化
- 数据库 `data/bot.db`，插件数据目录 `data/<plugin>/`（`get_data_dir`）

写配置只有一条路：**`ctl::change`**。它拿 `config_save_lock`、按插件真实的 serde 类型
校验、先写盘再改内存，任一步失败都不留下半个状态。三个入口都汇到这里——

| 入口 | 身份 | 实现 |
| --- | --- | --- |
| 聊天 / 控制台 `/ctl` | 消息发起人，按 `ctl.admins` 判权 | `plugins/ctl.rs` |
| pi 房间 `ayjx --ctl` | 一次性凭据换维护者身份 | `plugins/ctl/bridge.rs` |
| 网页面板 | 密钥换维护者身份 | `plugins/webui/` |

面板不重写任何校验，只是把默认配置翻译成表单（`webui/schema.rs`），
再把表单结果送回同一条事务；详见 [WEBUI.md](WEBUI.md)。

## 新增一个插件

1. 写 `src/plugins/<name>.rs`（或 `<name>/mod.rs` 式的目录模块），
   提供 `handle`、`default_config`、`validate_config`，按需加 `init` / `on_connected`。
2. 在 `src/plugins/registry.rs` 增加一条记录，位置决定它在流水线中的顺序：

   ```rust
   my_plugin {
       display_name: "我的插件",
       section: "play",
       summary: "一句话说明它做什么",
       commands: cmds![("我的指令 <参数>", "这条指令干什么")],
       on_init: Some(my_plugin::init)
   },
   ```

3. `cargo test`。注册表与帮助的一致性检查会告诉你还差什么：
   `every_plugin_has_a_summary`、`every_plugin_claims_a_known_section`、
   `grouping_loses_no_plugin`，以及跑遍全部插件的 `satori_compat_tests`。

**不需要**改动 `help.rs`、`ctl.rs` 或任何渲染代码——`/help`、`/help <插件名>`、
`/ctl list` 都会自动带上新插件。只有在需要一个全新分区时，才去 `help::SECTIONS`
加一行。

## 构建与测试

```sh
cargo check        # 快速验证
cargo test         # 出图与浏览器类为 ignored
cargo fmt          # 提交前
```

改动插件后至少跑 `cargo test`：`plugins::satori_compat_tests` 会用规范化消息跑全部插件，
`help::tests` 校验注册表元数据完整、分区不丢插件。

卡片版式改动要人工看图，三个插件各有一个 `ignored` 的落盘测试：

```sh
CIYI_CARD_DUMP=/tmp/cards    cargo test ciyi::card     -- --ignored
HELP_CARD_DUMP=/tmp/cards    cargo test help::card     -- --ignored
CTL_CARD_DUMP=/tmp/cards     cargo test ctl::card      -- --ignored
AI_NEWS_CARD_DUMP=/tmp/cards cargo test live_page_is_parseable -- --ignored  # 落盘 HTML
```

它们用真实注册表造样张（含启用/停用、长昵称、超长指令表等边界），
出图落盘后逐张核对；断言只保证「画得完、是合法 PNG」，好不好看得自己看。
