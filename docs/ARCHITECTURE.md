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
  render/          卡片渲染层：网页阅读卡片、原生字体与画布（详见「出图与渲染」）
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

按内容的阅读体验选择出图路线：

| 路线 | 依赖 | 谁在用 | 适用 |
| --- | --- | --- | --- |
| 网页阅读卡片 `render/web.rs` | Chrome/Chromium、系统 CJK 字体 | help、ctl、ciyi | 插件手册、状态清单、配置与差异、宣纸风盘面 |
| 插件自有原生绘图 `ciyi/painter.rs` | 系统 CJK 字体 | ciyi（截图不可用时兜底） | 宣纸风盘面 |
| 图表 plotters | 无 | stats、wordcloud | 坐标轴、折线、柱状、词云 |
| 浏览器截图 cdp_html_shot | Chrome/Chromium | webshot、ai_news、oai | 真实网页、资讯长图、Markdown |

help 与 ctl 共用 `render/web.rs` 的结构化文档和 `res/cards/reading.css`，由浏览器
完成字体塑形、标点与长文本换行。640 CSS px 版心、22px 正文、1.7 倍行高，配合
默认 3 倍 PNG 输出；帮助采用青绿点缀，控制采用暖棕点缀。清单单列显示，停用项仍
保留正常文字对比度，状态靠文字和颜色共同表达。所有动态内容进行 HTML 转义，
页面不执行脚本、不加载外部资源；截图前等待字体和布局完成。

系统卡片串行截图，排队、浏览器初始化、建页和截图共用 45 秒超时，任何结果均尝试
关闭页面。最大高度 16000 CSS px、位图最多 6400 万像素，超过限制回复完整文本，
不裁掉内容。`image_scale` 有限值限制为 1—4 倍，非有限值回退默认 3 倍。

ciyi 用 `render/web.rs::capture_html` 送自己写的整页 HTML（`ciyi/web.rs` +
`res/cards/ciyi.css`），660 CSS px 版心，宣纸底、朱砂一色、汉字走宋体，
`Doc` 模型排不出来的盘面走这条路。它比 help/ctl 多一层兜底：网页截图失败时
落到 `ciyi/card.rs` 的原生绘图，原生也失败才发纯文本。

兜底那一层要跟得上，否则浏览器一挂卡片就掉档。`ciyi/painter.rs` 因此补了三样
浏览器免费给的东西：**合成粗体**（差多少字重补多少，见下）、**虚线圆角路径**
（沿弧长用圆头笔触点，先进蒙版取最大值再一次性混合，拐角不叠色）、
**印章超采样**（小角度旋转前按 2 倍画，重采样才不磨圆笔画）。

**字重**：Android 自带的 Noto Serif/Sans CJK 只有 Regular 一档，向系统要 Bold
拿回来的还是那张 400 的脸。两条出图路径都会自己合成伪粗体顶上（浏览器天生会，
原生绘制靠 `Typeface.embolden` 做形态学膨胀），但外扩轮廓补不出笔画的粗细对比。
`sh scripts/install-cjk-weights.sh` 把真的 Bold(700) 与 Black(900) 装进 `~/.fonts`
之后，fontconfig 与 fontdb 都会自动改用它，合成量归零，代码一行不用动；
不装也能跑，只是题字虚一档。宋体分三档：`serif_x` 是最重的一档，只留给题字
——大标题、揭晓的答案、印章；正文级加粗仍走 `serif_b`，二十来 px 上再重就糊了。
网页卡片同一套分工，靠 `font-weight: 900` 表达。字体是设备本地状态，
仓库里恢复不出来，换机器要重跑一次脚本。

原生工具 `render/font.rs`、`canvas.rs`、`kit.rs` 保留供原生绘图使用；迁移渲染方式
以实际阅读质量为准。ai_news 保持网页日夜主题。

**出图失败回退纯文本**。浏览器缺失、初始化失败、截图超时或尺寸超限都不应让
帮助与控制失去响应。图文数据来自同一份注册表及经过权限校验、敏感字段脱敏的配置。

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
