//! settings 插件:统一的聊天设置指令，让用户无需编辑配置文件即可调整可调项。
//!
//! 用法:
//!   /设置                 → 列出所有可调项
//!   /设置 <插件> <键>      → 查看某项的当前值与说明
//!   /设置 <插件> <键> <值>  → 修改某项并自动保存
//!
//! 插件名与键均支持中文显示名或英文标识符。修改即时持久化到 config.toml，
//! 但定时任务类配置（如重启时间）需重启后生效。

use crate::adapters::onebot::{LockedWriter, send_msg};
use crate::command::match_command;
use crate::config::build_config;
use crate::event::Context;
use crate::message::Message;
use crate::plugins::{PluginError, get_plugins};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsScalar};
use toml::Value;
use toml::map::Map;

#[derive(Serialize, Deserialize)]
struct Config {
    enabled: bool,
}

pub fn default_config() -> Value {
    build_config(Config { enabled: true })
}

const TRIGGERS: &[&str] = &["设置", "set"];

// ================= 可调项注册表 =================

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Bool,
    Int,
    Float,
    Str,
}

struct SettingSpec {
    plugin: &'static str,
    key: &'static str,
    display: &'static str,
    desc: &'static str,
    kind: Kind,
    default: &'static str,
}

const fn s(
    plugin: &'static str,
    key: &'static str,
    display: &'static str,
    desc: &'static str,
    kind: Kind,
    default: &'static str,
) -> SettingSpec {
    SettingSpec { plugin, key, display, desc, kind, default }
}

/// 可调项清单：只收录用户真实会调整的安全项，其余一律默认值。
const SETTINGS: &[SettingSpec] = &[
    // 消息记录
    s("recorder", "retention_days", "消息保留天数", "消息记录保留的天数，超期自动清理", Kind::Int, "180"),
    // 复读机
    s("repeater", "min_times", "复读阈值", "连续几条相同消息后开始跟读", Kind::Int, "2"),
    s("repeater", "probability", "复读概率", "触发复读的概率，取值 0~1", Kind::Float, "1.0"),
    // 词意猜词
    s("ciyi", "plugin.at_user", "猜词@提醒", "猜词时是否@提醒玩家", Kind::Bool, "false"),
    s("ciyi", "plugin.quote_user", "猜词引用", "猜词时是否引用原消息", Kind::Bool, "true"),
    s("ciyi", "plugin.direct_guess", "直接猜词模式", "开启后可直接发送答案猜测", Kind::Bool, "false"),
    s("ciyi", "plugin.history_display", "历史显示条数", "猜词历史记录显示的条数", Kind::Int, "10"),
    s("ciyi", "plugin.rank_display", "排行显示条数", "猜词排行榜显示的条数", Kind::Int, "10"),
    // 统计图表
    s("stats", "push_min_messages", "推送最低消息数", "统计区间消息数低于此值时跳过推送", Kind::Int, "20"),
    s("stats", "daily_push_enabled", "每日总结推送", "每天 23:30 推送当日总结", Kind::Bool, "true"),
    s("stats", "morning_recap_enabled", "早安回顾推送", "每天 09:00 推送昨日回顾", Kind::Bool, "true"),
    s("stats", "noon_brief_enabled", "午间速览推送", "每天 12:30 推送上午速览", Kind::Bool, "true"),
    s("stats", "weekly_recap_enabled", "上周回顾推送", "每周一推送上周回顾", Kind::Bool, "true"),
    s("stats", "weekend_fun_enabled", "周末轻松榜推送", "每周日推送本周表情/类型榜", Kind::Bool, "true"),
    s("stats", "monthly_recap_enabled", "月度回顾推送", "每月 1 日推送上月回顾", Kind::Bool, "true"),
    // AI 资讯推送
    s("ai_news", "limit", "资讯推送条数", "单次推送最多展示的资讯条数", Kind::Int, "8"),
    s("ai_news", "min_items", "资讯推送阈值", "去重后新条目少于此值则本轮不推送", Kind::Int, "1"),
    s("ai_news", "window", "资讯时间窗", "抓取范围：24h 或 7d", Kind::Str, "24h"),
    s("ai_news", "category", "资讯分类过滤", "ai-models/ai-products/industry/paper/tip，留空为不限", Kind::Str, ""),
    s("ai_news", "show_reason", "显示推荐理由", "是否展示 AIHOT 的推荐理由", Kind::Bool, "true"),
    s("ai_news", "show_original_link", "显示原文链接", "是否附带第三方原文链接", Kind::Bool, "false"),
    s("ai_news", "dedupe_days", "资讯去重天数", "同一条资讯在多少天内不重复推送", Kind::Int, "7"),
    s("ai_news", "image_enabled", "资讯卡片图", "先发一张排版好的卡片图，再补带链接的文本", Kind::Bool, "true"),
    s("ai_news", "image_max_items", "卡片图条数", "卡片图最多画几条，其余交给文本", Kind::Int, "10"),
    s("ai_news", "forward_threshold_chars", "合并转发阈值", "消息超过多少字改用合并转发防刷屏，0 为永远纯文本", Kind::Int, "500"),
    s("ai_news", "forward_node_chars", "转发节点长度", "合并转发时每个节点的字数软上限", Kind::Int, "300"),
    s("ai_news", "daily_enabled", "AI 日报推送", "每天推送一期 AIHOT 日报", Kind::Bool, "true"),
    s("ai_news", "daily_time", "AI 日报时间", "日报推送时间（HH:MM:SS），需重启后生效", Kind::Str, "08:20:00"),
    s("ai_news", "brief_enabled", "精选速递推送", "每天定时推送 AI 精选资讯", Kind::Bool, "true"),
    s("ai_news", "hot_topics_enabled", "热点榜推送", "每天推送当前 AI 热点榜", Kind::Bool, "true"),
    s("ai_news", "hot_topics_time", "热点榜时间", "热点榜推送时间（HH:MM:SS），需重启后生效", Kind::Str, "21:40:00"),
    // 实时快报：以下各项均即时生效，无需重启
    s("ai_news", "realtime_enabled", "实时快报", "精选池一有新资讯就推，不必等定时档", Kind::Bool, "true"),
    s("ai_news", "realtime_interval_seconds", "实时轮询间隔", "每隔多少秒查一次新资讯，低于 60 秒无意义", Kind::Int, "180"),
    s("ai_news", "realtime_max_age_minutes", "实时保鲜期", "只推收录时间在此分钟数内的条目，避免离线后补发旧闻", Kind::Int, "240"),
    s("ai_news", "realtime_max_items", "实时单次条数", "一次实时推送最多几条，多的留到下一轮", Kind::Int, "5"),
    s("ai_news", "realtime_max_per_hour", "实时频次上限", "每个群每小时最多实时推送几次", Kind::Int, "4"),
    s("ai_news", "realtime_quiet_start", "实时静默起点", "静默时段起点（HH:MM），与终点相同表示不设静默", Kind::Str, "23:30"),
    s("ai_news", "realtime_quiet_end", "实时静默终点", "静默时段终点（HH:MM），跨午夜按跨天处理", Kind::Str, "07:30"),
    // 自动重启
    s("restart", "time", "每日重启时间", "每日自动重启时间（HH:MM），需重启后生效", Kind::Str, "04:00"),
    s("restart", "memory_threshold_mb", "内存重启阈值", "进程内存超过该值（MB）自动重启，0 关闭", Kind::Int, "0"),
    s("restart", "allow_manual_restart", "允许手动重启", "是否允许群聊发送 /restart 手动重启", Kind::Bool, "false"),
    s("restart", "restart_command", "外部重启命令", "如 systemctl restart xxx；留空则进程自我拉起", Kind::Str, ""),
];

// ================= 工具函数 =================

fn plugin_display_name(name: &str) -> String {
    for p in get_plugins() {
        if p.name == name {
            return p.display_name.to_string();
        }
    }
    name.to_string()
}

/// 解析插件：支持英文标识符或中文显示名
fn resolve_plugin(input: &str) -> Option<&'static str> {
    let input = input.trim();
    for p in get_plugins() {
        if p.name == input {
            return Some(p.name);
        }
    }
    for p in get_plugins() {
        if p.display_name == input {
            return Some(p.name);
        }
    }
    None
}

/// 解析设置项：支持英文键或中文显示名
fn resolve_setting(plugin: &str, key_input: &str) -> Option<&'static SettingSpec> {
    let key_input = key_input.trim();
    SETTINGS
        .iter()
        .find(|s| s.plugin == plugin && (s.key == key_input || s.display == key_input))
}

fn get_value(ctx: &Context, plugin: &str, key: &str) -> Option<Value> {
    let guard = ctx.config.read().unwrap();
    let plugin_val = guard.plugins.get(plugin)?;
    let mut current = plugin_val;
    for part in key.split('.') {
        current = current.as_table()?.get(part)?;
    }
    Some(current.clone())
}

fn set_value_by_path(root: &mut Value, path: &str, value: Value) -> bool {
    let mut parts: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return false;
    }
    let last = parts.pop().unwrap();
    let mut current = root;
    for part in &parts {
        let map = match current {
            Value::Table(map) => map,
            _ => return false,
        };
        current = map
            .entry((*part).to_string())
            .or_insert_with(|| Value::Table(Map::new()));
    }
    match current {
        Value::Table(map) => {
            map.insert(last.to_string(), value);
            true
        }
        _ => false,
    }
}

fn parse_value(kind: Kind, raw: &str) -> Result<Value, String> {
    let raw = raw.trim();
    match kind {
        Kind::Bool => match raw.to_lowercase().as_str() {
            "true" | "1" | "开" | "是" | "on" | "yes" => Ok(Value::Boolean(true)),
            "false" | "0" | "关" | "否" | "off" | "no" => Ok(Value::Boolean(false)),
            _ => Err(format!("布尔值需为：开/关、true/false、1/0（当前输入：{}）", raw)),
        },
        Kind::Int => raw
            .parse::<i64>()
            .map(Value::Integer)
            .map_err(|_| format!("请输入整数（当前输入：{}）", raw)),
        Kind::Float => raw
            .parse::<f64>()
            .map(Value::Float)
            .map_err(|_| format!("请输入数字（当前输入：{}）", raw)),
        Kind::Str => Ok(Value::String(raw.to_string())),
    }
}

fn format_value(v: &Value) -> String {
    match v {
        Value::Boolean(b) => if *b { "开".into() } else { "关".into() },
        Value::Integer(i) => i.to_string(),
        Value::Float(f) => format!("{:.2}", f),
        Value::String(s) => s.clone(),
        Value::Datetime(d) => d.to_string(),
        other => other.to_string(),
    }
}

fn extract_text_arg(args: &[simd_json::OwnedValue]) -> String {
    let mut buf = String::new();
    for seg in args {
        if seg.get_str("type") == Some("text")
            && let Some(s) = seg
                .get("data")
                .and_then(|d| d.get_str("text"))
        {
            buf.push_str(s);
            buf.push(' ');
        }
    }
    buf.trim().to_string()
}

// ================= 指令处理 =================

/// 渲染可调项一览（按插件分组）
fn render_overview() -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "📋 可调设置一览（{} 项），发送「/设置 <插件> <键>」查看详情\n",
        SETTINGS.len()
    ));
    out.push_str("———————————————\n");

    let mut current_plugin: Option<&str> = None;
    for spec in SETTINGS {
        if Some(spec.plugin) != current_plugin {
            current_plugin = Some(spec.plugin);
            out.push_str(&format!(
                "【{}】\n",
                plugin_display_name(spec.plugin)
            ));
        }
        out.push_str(&format!("  {}（{}）\n", spec.display, spec.key));
    }
    out.push_str("———————————————\n");
    out.push_str("修改示例：/设置 统计图表 daily_push_enabled 关");
    out
}

/// 渲染单项详情
fn render_detail(ctx: &Context, spec: &SettingSpec) -> String {
    let current = get_value(ctx, spec.plugin, spec.key);
    let current_str = match &current {
        Some(v) => format_value(v),
        None => "（未设置，使用默认）".to_string(),
    };

    format!(
        "📋 [{}] {}（{}）\n当前值：{}\n默认值：{}\n说明：{}\n修改：/设置 {} {} <值>",
        plugin_display_name(spec.plugin),
        spec.display,
        spec.key,
        current_str,
        if spec.default.is_empty() { "（空）" } else { spec.default },
        spec.desc,
        spec.plugin,
        spec.key,
    )
}

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        for trigger in TRIGGERS {
            let Some(matched) = match_command(&ctx, trigger) else {
                continue;
            };

            let msg = match ctx.as_message() {
                Some(m) => m,
                None => return Ok(Some(ctx)),
            };
            let group_id = msg.group_id();
            let user_id = msg.user_id();
            let text = extract_text_arg(&matched.args);

            // 无参数 → 一览
            if text.is_empty() {
                let reply = Message::new().reply(msg.message_id()).text(render_overview());
                let _ = send_msg(&ctx, writer, group_id, Some(user_id), reply).await;
                return Ok(None);
            }

            let tokens: Vec<&str> = text.split_whitespace().collect();

            // 参数不足 → 用法提示
            if tokens.len() < 2 {
                let reply = Message::new()
                    .reply(msg.message_id())
                    .text("⚠️ 用法：/设置 <插件> <键> [值]\n发送「/设置」可查看全部可调项。");
                let _ = send_msg(&ctx, writer, group_id, Some(user_id), reply).await;
                return Ok(None);
            }

            let plugin = match resolve_plugin(tokens[0]) {
                Some(p) => p,
                None => {
                    let reply = Message::new()
                        .reply(msg.message_id())
                        .text(format!("❌ 未找到插件「{}」。发送「/设置」查看插件列表。", tokens[0]));
                    let _ = send_msg(&ctx, writer, group_id, Some(user_id), reply).await;
                    return Ok(None);
                }
            };

            let spec = match resolve_setting(plugin, tokens[1]) {
                Some(spec) => spec,
                None => {
                    let reply = Message::new()
                        .reply(msg.message_id())
                        .text(format!("❌ 插件「{}」没有「{}」这个设置项。发送「/设置」查看全部可调项。", plugin_display_name(plugin), tokens[1]));
                    let _ = send_msg(&ctx, writer, group_id, Some(user_id), reply).await;
                    return Ok(None);
                }
            };

            // 只有插件+键 → 查看详情
            if tokens.len() == 2 {
                let reply = Message::new().reply(msg.message_id()).text(render_detail(&ctx, spec));
                let _ = send_msg(&ctx, writer, group_id, Some(user_id), reply).await;
                return Ok(None);
            }

            // 插件+键+值 → 修改（第 3 个 token 起拼接，支持含空格的值）
            let value_raw = tokens[2..].join(" ");
            let new_value = match parse_value(spec.kind, &value_raw) {
                Ok(v) => v,
                Err(e) => {
                    let reply = Message::new().reply(msg.message_id()).text(format!("❌ {}", e));
                    let _ = send_msg(&ctx, writer, group_id, Some(user_id), reply).await;
                    return Ok(None);
                }
            };

            // 写入配置并持久化
            let write_failed = {
                let mut guard = ctx.config.write().unwrap();
                match guard.plugins.get_mut(plugin) {
                    None => Some(format!(
                        "插件「{}」尚未初始化配置。",
                        plugin_display_name(plugin)
                    )),
                    Some(plugin_val) => {
                        if set_value_by_path(plugin_val, spec.key, new_value) {
                            None
                        } else {
                            Some(format!(
                                "插件「{}」配置格式异常。",
                                plugin_display_name(plugin)
                            ))
                        }
                    }
                }
            };

            if let Some(err_msg) = write_failed {
                let reply = Message::new()
                    .reply(msg.message_id())
                    .text(format!("❌ 设置失败：{}", err_msg));
                let _ = send_msg(&ctx, writer, group_id, Some(user_id), reply).await;
                return Ok(None);
            }

            // 保存到磁盘
            let save_result = {
                let _fs_guard = ctx.config_save_lock.lock().await;
                let snapshot = { ctx.config.read().unwrap().clone() };
                snapshot.save(&ctx.config_path).await
            };

            match save_result {
                Ok(()) => {
                    let note = if matches!(spec.key, "time") {
                        "（需重启后生效）"
                    } else {
                        ""
                    };
                    let reply = Message::new()
                        .reply(msg.message_id())
                        .text(format!(
                            "✅ 已修改：[{}] {} = {}{}。",
                            plugin_display_name(plugin),
                            spec.display,
                            value_raw.trim(),
                            note
                        ));
                    let _ = send_msg(&ctx, writer, group_id, Some(user_id), reply).await;
                }
                Err(e) => {
                    error!(target: "Plugin/Settings", "设置保存失败: {}", e);
                    let reply = Message::new()
                        .reply(msg.message_id())
                        .text(format!("❌ 设置已生效但保存失败：{}", e));
                    let _ = send_msg(&ctx, writer, group_id, Some(user_id), reply).await;
                }
            }

            return Ok(None);
        }

        Ok(Some(ctx))
    })
}
