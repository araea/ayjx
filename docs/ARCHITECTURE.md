# 架构说明

面向维护者与后续自动化任务的参考手册。只描述现状与约定，不描述历史。

## 目录结构

```
src/
  main.rs          启动：加载配置、初始化数据库、连接适配器
  adapters/        适配器。satori.rs 为 Satori WS/HTTP 实现，console.rs 供本地测试
  command.rs       指令解析与消息内容提取的公共工具
  config.rs        AppConfig 与插件配置读写，build_config 辅助函数
  event.rs         Context / EventType / MessageEvent 定义
  http.rs          全局 reqwest 客户端（Android CA 兼容），download_bytes
  matcher.rs       事件去重
  message.rs       Message 消息构建器（text/image/node_custom 等）
  plugins.rs       插件框架核心：Plugin 定义、流水线、配置读写
  plugins/         各插件，registry.rs 为注册表
  scheduler.rs     定时任务（daily/interval/周期推送，带 Pace 错峰）
  db/              sea-orm 实体与查询（SQLite，data/bot.db）
```

## 事件流

```
适配器收到事件 → Context 构造 → plugins::run()
  逐个执行启用的插件 handler：
    Ok(Some(ctx)) → 接力给下一插件（插件拥有 Context 所有权，可改写事件）
    Ok(None)      → 事件被消费，流水线结束
    Err           → 记 error 日志，按已消费处理，不会崩掉适配器
  流水线走完仍未消费 → 末尾派发 EventType::BeforeSend
```

Context 通过 Move 传递，不深拷贝事件。`send_fake_event` 可将伪造事件推回流水线。

## 插件系统

插件即一个模块，需提供：

- `handle(ctx, writer) -> BoxFuture<Result<Option<Context>, PluginError>>` — 必需
- `default_config() -> toml::Value` — 必需
- `init(ctx)` / `on_connected(ctx, writer)` — 可选生命周期钩子

注册在 `src/plugins/registry.rs` 的 `register_plugins!` 宏中，宏自动生成模块声明；
`display_name` 为中文展示名（help/settings 用），配置键用标识符。

启用与否只看 `[plugins.<name>] enabled`，运行时每次事件从配置快照读取。

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

**下载与渲染**：图片下载 `http::download_bytes`；合并转发节点用 `Message::node_custom`；
HTML 截图走 `cdp_html_shot`（ai_news/help 已有现成封装可参考）。

## 配置与数据

- `config.toml` 不入库；首次启动写默认值，启动时补字段、清残留，解析失败则退出不覆盖
- 插件配置改动经 `plugins::update_config` 或 settings 插件，持久化受 `config_save_lock` 串行化
- 数据库 `data/bot.db`，插件数据目录 `data/<plugin>/`（`get_data_dir`）

## 构建与测试

```sh
cargo check        # 快速验证
cargo test         # 73 个测试；浏览器截图类为 ignored
cargo fmt          # 提交前
```

改动插件后至少跑 `cargo test`：`plugins::satori_compat_tests` 会用规范化消息跑全部插件，
`help::tests` 校验注册表与帮助分区一致。
