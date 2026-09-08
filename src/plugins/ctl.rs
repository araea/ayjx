//! Unified plugin control. Registry-backed, typed, persisted before publication.
use crate::adapters::satori::{LockedWriter, send_msg};
use crate::command::{extract_text_arg, get_prefixes, match_word_command};
use crate::config::{AppConfig, build_config};
use crate::event::Context;
use crate::message::Message;
use crate::plugins::{Plugin, PluginError, get_plugins, needs_startup, pending_startup};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use toml::Value;

pub mod bridge;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    enabled: bool,
    /// Global operators, not group administrators. Empty means console only.
    admins: Vec<i64>,
    /// 允许管理员在 pi 房间里用自然语言驱动 ctl（见 bridge）。关闭后 pi 房间拿不到凭据。
    pi_control: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            admins: vec![],
            pi_control: true,
        }
    }
}
pub fn default_config() -> Value {
    build_config(Config::default())
}
pub fn validate_config(value: &Value) -> Result<(), String> {
    Config::deserialize(value.clone())
        .map(|_| ())
        .map_err(|_| "admins 必须是 QQ 号整数数组，pi_control 必须是布尔值".into())
}
pub fn is_manager(ctx: &Context) -> bool {
    if ctx.bot.adapter == "console" && ctx.bot.platform == "console" {
        return true;
    }
    let cfg = ctx.config.read().unwrap();
    let admins = cfg
        .plugins
        .get("ctl")
        .and_then(|v| v.get("admins"))
        .and_then(Value::as_array);
    ctx.as_message().is_some_and(|msg| {
        admins.is_some_and(|ids| ids.iter().any(|id| id.as_integer() == Some(msg.user_id())))
    })
}
pub const DENIED: &str = "此操作仅限 ctl.admins 中的全局管理员；请由本机维护者在 config.toml 的 [ctl] 中配置 admins = [QQ号]。";

fn resolve(name: &str) -> Result<&'static Plugin, String> {
    get_plugins()
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(name) || p.display_name == name)
        .ok_or_else(|| format!("未找到插件「{name}」，请用 ctl list 查看名称。"))
}
fn enabled(cfg: &AppConfig, name: &str) -> bool {
    cfg.plugins
        .get(name)
        .and_then(|v| v.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}
fn at<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() {
        return Some(value);
    }
    path.split('.').try_fold(value, |v, key| match v {
        Value::Array(a) => a.get(key.parse::<usize>().ok()?),
        Value::Table(t) => t.get(key),
        _ => None,
    })
}
fn at_mut<'a>(value: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    path.split('.').try_fold(value, |v, key| match v {
        Value::Array(a) => a.get_mut(key.parse::<usize>().ok()?),
        Value::Table(t) => t.get_mut(key),
        _ => None,
    })
}
fn sensitive(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "api_key",
        "apikey",
        "authorization",
        "credential",
    ]
    .iter()
    .any(|s| key.contains(s))
}
fn redacted(value: &Value) -> Value {
    match value {
        Value::Table(t) => Value::Table(
            t.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        if sensitive(k) {
                            Value::String("<已隐藏>".into())
                        } else {
                            redacted(v)
                        },
                    )
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(redacted).collect()),
        v => v.clone(),
    }
}
fn display(value: &Value, path: &str) -> String {
    if sensitive(path) {
        return "<已隐藏>".into();
    }
    let value = redacted(value);
    if value.is_table() {
        toml::to_string_pretty(&value).unwrap_or_default()
    } else {
        value.to_string()
    }
}
fn parse(raw: &str, old: &Value) -> Result<Value, String> {
    if old.is_str() && !raw.starts_with(['"', '\'']) {
        return Ok(Value::String(raw.into()));
    }
    if old.is_bool() {
        match raw.to_ascii_lowercase().as_str() {
            "开" | "开启" | "true" | "on" | "1" => return Ok(Value::Boolean(true)),
            "关" | "关闭" | "false" | "off" | "0" => return Ok(Value::Boolean(false)),
            _ => {}
        }
    }
    let wrapper: Value = toml::from_str(&format!("value = {raw}")).map_err(|_| {
        "值格式错误；数组用 [1, 2]，表用 { key = \"值\" }，空字符串用 \"\"。".to_string()
    })?;
    let table = wrapper.as_table().ok_or("值格式错误")?;
    if table.len() != 1 {
        return Err("一次只能修改一个配置项".into());
    }
    let mut value = table.get("value").ok_or("缺少值")?.clone();
    if old.is_float() && value.is_integer() {
        value = Value::Float(value.as_integer().unwrap() as f64);
    }
    Ok(value)
}
/// Check all nested keys against defaults, including empty arrays via real serde types.
fn shape(default: &Value, value: &Value) -> Result<(), String> {
    match (default, value) {
        (Value::Table(d), Value::Table(v)) => {
            if !d.is_empty() {
                for (key, val) in v {
                    let expected = d.get(key).ok_or_else(|| format!("未知配置键：{key}"))?;
                    shape(expected, val)?;
                }
                for key in d.keys() {
                    if !v.contains_key(key) {
                        return Err(format!("缺少配置键：{key}"));
                    }
                }
            }
        }
        (Value::Array(d), Value::Array(v)) => {
            if let Some(first) = d.first() {
                for val in v {
                    shape(first, val)?;
                }
            }
        }
        (Value::Float(_), Value::Float(v)) if v.is_finite() => {}
        (Value::Boolean(_), Value::Boolean(_))
        | (Value::Integer(_), Value::Integer(_))
        | (Value::String(_), Value::String(_))
        | (Value::Datetime(_), Value::Datetime(_)) => {}
        _ => return Err(format!("配置类型错误，需要 {}", default.type_str())),
    }
    Ok(())
}
fn constraints(value: &Value, path: &str) -> Result<(), String> {
    if let Value::Table(t) = value {
        for (k, v) in t {
            constraints(
                v,
                &if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                },
            )?;
        }
    }
    let key = path.rsplit('.').next().unwrap_or(path);
    if key.contains("probability") && value.as_float().is_some_and(|n| !(0.0..=1.0).contains(&n)) {
        return Err(format!("{path} 应在 0 到 1 之间"));
    }
    if key == "image_scale" && value.as_float().is_some_and(|n| !(1.0..=4.0).contains(&n)) {
        return Err(format!("{path} 应在 1 到 4 之间"));
    }
    if (key == "time" || key.ends_with("_time"))
        && let Some(s) = value.as_str()
    {
        let parts: Vec<_> = s.split(':').map(str::parse::<u32>).collect();
        if !(2..=3).contains(&parts.len())
            || parts.iter().any(Result::is_err)
            || parts[0].as_ref().is_ok_and(|n| *n > 23)
            || parts[1..].iter().any(|p| p.as_ref().is_ok_and(|n| *n > 59))
        {
            return Err(format!("{path} 应为有效的 HH:MM 或 HH:MM:SS"));
        }
    }
    Ok(())
}
fn validate(p: &Plugin, value: &Value) -> Result<(), String> {
    let mut expected = (p.default_config)();
    if p.name == "wordcloud" {
        for key in ["font_path", "font_family"] {
            if value.get(key).is_some() {
                expected
                    .as_table_mut()
                    .unwrap()
                    .insert(key.into(), Value::String(String::new()));
            }
        }
    }
    shape(&expected, value)?;
    (p.validate_config)(value)?;
    constraints(value, "")?;
    if p.name == "ai_news" {
        for key in ["mode", "realtime_mode"] {
            if value
                .get(key)
                .and_then(Value::as_str)
                .is_some_and(|s| !["all", "selected"].contains(&s))
            {
                return Err(format!("{key} 只能是 all 或 selected"));
            }
        }
        if value
            .get("card_theme")
            .and_then(Value::as_str)
            .is_some_and(|s| {
                ![
                    "auto", "light", "dark", "day", "night", "白天", "日间", "夜晚", "夜间",
                ]
                .contains(&s)
            })
        {
            return Err("card_theme 应为 auto、light 或 dark".into());
        }
    }
    Ok(())
}

/// Shared transaction for ctl and legacy settings. No in-memory changes on save failure.
pub async fn change<F>(ctx: &Context, edit: F) -> Result<String, String>
where
    F: FnOnce(&mut AppConfig) -> Result<String, String>,
{
    let _lock = ctx.config_save_lock.lock().await;
    // Recheck after acquiring the lock: another command may have revoked this operator.
    if !is_manager(ctx) {
        return Err(DENIED.into());
    }
    let mut next = ctx.config.read().unwrap().clone();
    let result = edit(&mut next)?;
    for p in get_plugins() {
        if next.plugins.get(p.name) != ctx.config.read().unwrap().plugins.get(p.name) {
            validate(p, next.plugins.get(p.name).ok_or("缺少插件配置")?)?;
        }
    }
    if !enabled(&next, "ctl") {
        return Err("为保留管理入口，ctl 不能通过聊天关闭；需要时请停机编辑配置。".into());
    }
    let console = ctx.bot.adapter == "console" && ctx.bot.platform == "console";
    if !console && let Some(msg) = ctx.as_message() {
        let retained = next
            .plugins
            .get("ctl")
            .and_then(|v| v.get("admins"))
            .and_then(Value::as_array)
            .is_some_and(|ids| ids.iter().any(|id| id.as_integer() == Some(msg.user_id())));
        if !retained {
            return Err("不能移除自己的管理权限；请先交由另一位管理员操作或停机编辑配置。".into());
        }
    }
    next.save(&ctx.config_path).await.map_err(|e| {
        error!(target: "Plugin/Ctl", "保存失败: {}", e);
        "保存失败，内存配置未改变；请检查磁盘权限及空间。".to_string()
    })?;
    *ctx.config.write().unwrap() = next;
    Ok(result)
}
pub async fn set_value(
    ctx: &Context,
    plugin: &str,
    path: &str,
    value: Value,
) -> Result<String, String> {
    let p = resolve(plugin)?;
    change(ctx, |cfg| {
        let v = cfg.plugins.get_mut(p.name).ok_or("插件配置不存在")?;
        if p.name == "wordcloud" && ["font_path", "font_family"].contains(&path) {
            v.as_table_mut()
                .ok_or("配置不是表")?
                .insert(path.into(), value);
        } else {
            *at_mut(v, path).ok_or("配置路径不存在；可用 ctl show 查看完整配置")? = value;
        }
        Ok(format!("已保存 {}.{}。{}", p.name, path, effect(p.name)))
    })
    .await
}
fn effect(name: &str) -> &'static str {
    if needs_startup(name) {
        "消息开关立即生效；首次启用、初始化参数及定时排期需重启后完整生效，已在执行的任务不强制中断。"
    } else {
        "下一条消息生效。"
    }
}
fn usage(prefix: &str) -> String {
    format!(
        "插件控制 ctl（别名：插件、控制）\n\
{prefix}ctl list [on|off|关键词] — 状态列表\n\
{prefix}ctl on <插件...> / off <插件...> — 批量开关\n\
{prefix}ctl show <插件> [路径] — 当前值（敏感项隐藏）\n\
{prefix}ctl defaults <插件> [路径] — 默认值\n\
{prefix}ctl set <插件> <路径> <值> — 修改并保存\n\
{prefix}ctl reset <插件> [路径] --confirm — 恢复默认\n\
{prefix}ctl diff <插件> — 与默认值比较\n\
中文操作：列表、开启、关闭、查看、默认、设置、重置、差异\n\
插件名支持英文及中文显示名；多个名称以空格或逗号分隔。\n\
例：{prefix}ctl on 帮助中心 ping\n\
例：{prefix}ctl set repeater channel.white [123456]\n\
例：{prefix}ctl set ciyi plugin.image_scale 2\n\
配置查看/修改仅限 ctl.admins；控制台可管理。全局开关影响全部会话。\n\
ctl 保留管理入口；修改它的 admins 请在私聊或控制台执行。\n\
带生命周期的插件首次启用及排期修改需重启；状态标注“待重启”。"
    )
}
fn word(text: &str) -> (&str, &str) {
    let text = text.trim();
    text.find(char::is_whitespace)
        .map(|i| (&text[..i], text[i..].trim_start()))
        .unwrap_or((text, ""))
}
pub(crate) async fn execute(ctx: &Context, input: &str) -> Result<String, String> {
    let (action, rest) = word(input);
    let prefix = get_prefixes(ctx).first().cloned().unwrap_or_default();
    if ["", "help", "帮助"].contains(&action) {
        return Ok(usage(&prefix));
    }
    if ["list", "ls", "status", "列表", "状态"].contains(&action) {
        let cfg = ctx.config.read().unwrap();
        let rows: Vec<_> = get_plugins()
            .iter()
            .filter(|p| match rest {
                "on" | "开启" => enabled(&cfg, p.name),
                "off" | "关闭" => !enabled(&cfg, p.name),
                key => p.name.contains(&key.to_ascii_lowercase()) || p.display_name.contains(key),
            })
            .map(|p| {
                format!(
                    "{} {}（{}）{}",
                    if enabled(&cfg, p.name) { "开" } else { "关" },
                    p.name,
                    p.display_name,
                    if enabled(&cfg, p.name) && pending_startup(p.name) {
                        " · 待重启"
                    } else {
                        ""
                    }
                )
            })
            .collect();
        return Ok(format!(
            "插件状态（全局配置）\n{}\n操作帮助：{prefix}ctl",
            if rows.is_empty() {
                "无匹配插件".into()
            } else {
                rows.join("\n")
            }
        ));
    }
    if !is_manager(ctx) {
        return Err(DENIED.into());
    }
    if ["on", "off", "开启", "关闭", "启用", "禁用"].contains(&action) {
        let names: Vec<_> = rest
            .split(|c: char| c.is_whitespace() || c == ',' || c == '，')
            .filter(|s| !s.is_empty())
            .map(resolve)
            .collect::<Result<_, _>>()?;
        if names.is_empty() {
            return Err("请指定插件，例如 ctl on help ping".into());
        }
        let on = ["on", "开启", "启用"].contains(&action);
        return change(ctx, |cfg| {
            for p in &names {
                *cfg.plugins
                    .get_mut(p.name)
                    .and_then(|v| at_mut(v, "enabled"))
                    .ok_or("缺少 enabled 配置")? = Value::Boolean(on);
            }
            Ok(format!(
                "已全部{}并保存：{}。{}",
                if on { "开启" } else { "关闭" },
                names.iter().map(|p| p.name).collect::<Vec<_>>().join("、"),
                if names.iter().any(|p| needs_startup(p.name)) {
                    effect(names.iter().find(|p| needs_startup(p.name)).unwrap().name)
                } else {
                    "下一条消息生效。"
                }
            ))
        })
        .await;
    }
    let (name, rest) = word(rest);
    let p = resolve(name)?;
    let (path, tail) = word(rest);
    match action {
        "show" | "get" | "查看" | "defaults" | "默认" => {
            if !tail.is_empty() {
                return Err("用法：ctl show/defaults <插件> [路径]".into());
            }
            let defaults = (p.default_config)();
            let cfg = ctx.config.read().unwrap();
            let source = if ["defaults", "默认"].contains(&action) {
                &defaults
            } else {
                cfg.plugins.get(p.name).ok_or("配置不存在")?
            };
            let value = at(source, path).ok_or("配置路径不存在；省略路径可查看完整配置")?;
            Ok(format!(
                "{}.{}\n{}\n{}",
                p.name,
                path,
                display(value, path),
                effect(p.name)
            ))
        }
        "diff" | "差异" => {
            if !rest.is_empty() {
                return Err("用法：ctl diff <插件>".into());
            }
            let cfg = ctx.config.read().unwrap();
            let current = cfg.plugins.get(p.name).ok_or("配置不存在")?;
            let defaults = (p.default_config)();
            let mut lines = Vec::new();
            differences(&defaults, current, "", &mut lines);
            Ok(if lines.is_empty() {
                "配置与默认值一致".into()
            } else {
                lines.join("\n")
            })
        }
        "set" | "设置" => {
            if path.is_empty() || tail.is_empty() {
                return Err("用法：ctl set <插件> <路径> <值>".into());
            }
            if (sensitive(path)
                || (p.name == "ctl" && (path == "admins" || path.starts_with("admins."))))
                && ctx.as_message().is_some_and(|m| m.group_id().is_some())
            {
                return Err("此项请在私聊或本机控制台修改。".into());
            }
            let old = {
                let cfg = ctx.config.read().unwrap();
                at(cfg.plugins.get(p.name).ok_or("配置不存在")?, path)
                    .cloned()
                    .or_else(|| {
                        (p.name == "wordcloud" && ["font_path", "font_family"].contains(&path))
                            .then(|| Value::String(String::new()))
                    })
                    .ok_or("配置路径不存在")?
            };
            set_value(ctx, p.name, path, parse(tail, &old)?).await
        }
        "reset" | "重置" => {
            let path = if path == "--confirm" && tail.is_empty() {
                ""
            } else if tail == "--confirm" {
                path
            } else {
                return Err("重置会覆盖现有值；查看 ctl defaults 后，追加 --confirm 确认。".into());
            };
            let defaults = (p.default_config)();
            if path.is_empty() {
                change(ctx, |cfg| {
                    // Whole-plugin reset restores options while preserving operational access/state.
                    let old = cfg.plugins.get(p.name).ok_or("配置不存在")?;
                    let mut value = defaults;
                    value["enabled"] = old.get("enabled").cloned().ok_or("缺少 enabled")?;
                    if p.name == "ctl" {
                        value["admins"] = old.get("admins").cloned().ok_or("缺少 admins")?;
                    }
                    cfg.plugins.insert(p.name.into(), value);
                    Ok(format!(
                        "已恢复 {} 的默认参数，保留开关及控制管理员。{}",
                        p.name,
                        effect(p.name)
                    ))
                })
                .await
            } else {
                set_value(
                    ctx,
                    p.name,
                    path,
                    at(&defaults, path).cloned().ok_or("该路径没有默认值")?,
                )
                .await
            }
        }
        _ => Err(format!("未知操作「{action}」。发送 {prefix}ctl 查看用法。")),
    }
}
fn differences(default: &Value, current: &Value, path: &str, out: &mut Vec<String>) {
    if default == current {
        return;
    }
    if let (Value::Table(d), Value::Table(c)) = (default, current) {
        for (key, value) in c {
            let path = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            if let Some(old) = d.get(key) {
                differences(old, value, &path, out);
            } else {
                out.push(format!("{path} = {}（额外项）", display(value, &path)));
            }
        }
    } else {
        out.push(format!(
            "{path}: {} → {}",
            display(default, path),
            display(current, path)
        ));
    }
}

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        let Some(matched) = ["ctl", "控制", "插件"]
            .iter()
            .find_map(|name| match_word_command(&ctx, name))
        else {
            return Ok(Some(ctx));
        };
        let Some(msg) = ctx.as_message() else {
            return Ok(Some(ctx));
        };
        let input = extract_text_arg(&matched.args);
        let response = execute(&ctx, &input)
            .await
            .unwrap_or_else(|e| format!("操作未完成：{e}"));
        // Bound each message so a large array/config does not exceed adapter limits.
        let chars: Vec<char> = response.chars().collect();
        for part in chars.chunks(2800) {
            send_msg(
                &ctx,
                writer.clone(),
                msg.group_id(),
                Some(msg.user_id()),
                Message::new().text(part.iter().collect::<String>()),
            )
            .await?;
        }
        Ok(None)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{BotStatus, EventType, LoginUser};
    use crate::matcher::Matcher;
    use crate::scheduler::Scheduler;
    use sea_orm::Database;
    use std::sync::{Arc, RwLock};
    use tokio::sync::Mutex;

    async fn context(console: bool) -> Context {
        let mut config = AppConfig::default();
        for p in get_plugins() {
            config.plugins.insert(p.name.into(), (p.default_config)());
        }
        let path =
            std::env::temp_dir().join(format!("ayjx-ctl-test-{}.toml", rand::random::<u64>()));
        Context {
            event: EventType::Satori(simd_json::serde::to_owned_value(serde_json::json!({
                "post_type":"message", "message_type":"private", "user_id":42, "message_id":1,
                "raw_message":"/ctl list", "message":[{"type":"text","data":{"text":"/ctl list"}}]
            })).unwrap()),
            config: Arc::new(RwLock::new(config)), config_save_lock: Arc::new(Mutex::new(())),
            db: Database::connect("sqlite::memory:").await.unwrap(), scheduler: Arc::new(Scheduler::new()),
            matcher: Arc::new(Matcher::new()), config_path: Arc::from(path.to_str().unwrap()),
            bot: Arc::new(BotStatus { adapter: if console { "console" } else { "satori-qq" }.into(), platform: if console { "console" } else { "qq" }.into(), login_user: LoginUser::default() }),
        }
    }
    #[test]
    fn registry_defaults_pass_real_type_validation() {
        for p in get_plugins() {
            validate(p, &(p.default_config)()).unwrap_or_else(|e| panic!("{}: {}", p.name, e));
        }
    }
    #[test]
    fn parse_and_redaction_cover_structured_values() {
        assert_eq!(
            parse("a  b", &Value::String(String::new()))
                .unwrap()
                .as_str(),
            Some("a  b")
        );
        assert_eq!(
            parse("关", &Value::Boolean(true)).unwrap().as_bool(),
            Some(false)
        );
        let v: Value = toml::from_str("[nested]\napi_key = 'private-value'\nitems = [{token = 'hidden-value', name = 'visible'}]").unwrap();
        let text = display(&v, "");
        assert!(!text.contains("private-value") && !text.contains("hidden-value"));
        assert!(text.contains("visible"));
        assert_eq!(
            display(at(&v, "nested.api_key").unwrap(), "nested.api_key"),
            "<已隐藏>"
        );
        assert!(parse("true\nextra = false", &Value::Boolean(true)).is_err());
    }
    #[tokio::test]
    async fn access_control_covers_legacy_paths_and_does_not_depend_on_enabled() {
        let ctx = context(false).await;
        assert!(!is_manager(&ctx));
        assert!(execute(&ctx, "list").await.is_ok());
        for input in [
            "show ctl",
            "on ping",
            "set help image_enabled false",
            "reset help --confirm",
        ] {
            assert!(execute(&ctx, input).await.is_err(), "{input}");
        }
        ctx.config.write().unwrap().plugins.get_mut("ctl").unwrap()["admins"] =
            Value::Array(vec![Value::Integer(42)]);
        ctx.config.write().unwrap().plugins.get_mut("ctl").unwrap()["enabled"] =
            Value::Boolean(false);
        assert!(is_manager(&ctx));
        assert!(execute(&ctx, "show help").await.is_ok());
        assert!(!std::path::Path::new(ctx.config_path.as_ref()).exists());
    }
    #[tokio::test]
    async fn batch_changes_are_atomic_and_persist_across_reload() {
        let ctx = context(true).await;
        assert!(execute(&ctx, "off help unknown_plugin").await.is_err());
        assert!(enabled(&ctx.config.read().unwrap(), "help"));
        execute(&ctx, "关闭 帮助中心,ping").await.unwrap();
        let disk: AppConfig = toml::from_str(
            &tokio::fs::read_to_string(ctx.config_path.as_ref())
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(!enabled(&disk, "help") && !enabled(&disk, "ping"));
        assert!(execute(&ctx, "off echo ctl").await.is_err());
        assert!(enabled(&ctx.config.read().unwrap(), "echo"));
        tokio::fs::remove_file(ctx.config_path.as_ref())
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn nested_edits_validate_empty_arrays_numeric_ranges_and_unknown_keys() {
        let ctx = context(true).await;
        for input in [
            "set repeater channel.white [\"oops\"]",
            "set repeater probability 1.5",
            "set help image_scale nan",
            "set restart time 26:00",
            "set oai shell_timeout_seconds -1",
            "set repeater channel { whiet = [1] }",
            "set help does_not_exist true",
        ] {
            assert!(execute(&ctx, input).await.is_err(), "{input}");
        }
        execute(&ctx, "set repeater channel.white [123, 456]")
            .await
            .unwrap();
        execute(&ctx, "set repeater channel.white.1 789")
            .await
            .unwrap();
        execute(&ctx, "set ciyi plugin.image_scale 2")
            .await
            .unwrap();
        execute(&ctx, "set wordcloud font_family Noto Sans CJK SC")
            .await
            .unwrap();
        assert!(
            execute(&ctx, "diff repeater")
                .await
                .unwrap()
                .contains("789")
        );
        assert!(execute(&ctx, "reset repeater channel.white").await.is_err());
        execute(&ctx, "reset repeater channel.white --confirm")
            .await
            .unwrap();
        assert!(
            execute(&ctx, "diff repeater")
                .await
                .unwrap()
                .contains("一致")
        );
        tokio::fs::remove_file(ctx.config_path.as_ref())
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn failed_save_keeps_memory_and_existing_file() {
        let mut ctx = context(true).await;
        let dir = format!("{}.directory", ctx.config_path);
        tokio::fs::create_dir(&dir).await.unwrap();
        ctx.config_path = Arc::from(dir.as_str()); // rename over a directory must fail even as root.
        assert!(execute(&ctx, "off help").await.is_err());
        assert!(enabled(&ctx.config.read().unwrap(), "help"));
        assert!(std::path::Path::new(&dir).is_dir());
        tokio::fs::remove_dir(dir).await.unwrap();
    }
    #[tokio::test]
    async fn concurrent_edits_and_resets_preserve_other_options() {
        let ctx = context(true).await;
        let (a, b) = tokio::join!(
            execute(&ctx, "set help image_enabled false"),
            execute(&ctx, "set help image_scale 2")
        );
        a.unwrap();
        b.unwrap();
        let val = ctx.config.read().unwrap().plugins["help"].clone();
        assert_eq!(val["image_enabled"].as_bool(), Some(false));
        assert_eq!(val["image_scale"].as_float(), Some(2.0));
        execute(&ctx, "off help").await.unwrap();
        execute(&ctx, "reset help --confirm").await.unwrap();
        assert!(!enabled(&ctx.config.read().unwrap(), "help"));
        assert_eq!(
            ctx.config.read().unwrap().plugins["help"]["image_enabled"].as_bool(),
            Some(true)
        );
        tokio::fs::remove_file(ctx.config_path.as_ref())
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn cannot_remove_own_remote_access() {
        let ctx = context(false).await;
        ctx.config.write().unwrap().plugins.get_mut("ctl").unwrap()["admins"] =
            Value::Array(vec![Value::Integer(42)]);
        assert!(execute(&ctx, "set ctl admins []").await.is_err());
        execute(&ctx, "reset ctl --confirm").await.unwrap();
        assert!(is_manager(&ctx));
        tokio::fs::remove_file(ctx.config_path.as_ref())
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn word_commands_respect_prefixes_and_do_not_steal_help_alias() {
        let mut ctx = context(true).await;
        for (prefixes, text, matched) in [
            (vec!["!".into()], "!ctl list", true),
            (vec![], "ctl list", true),
            (vec!["/".into()], "/ctlx", false),
            (vec!["/".into()], "/插件列表", false),
        ] {
            ctx.config.write().unwrap().command_prefix = prefixes;
            ctx.event = EventType::Satori(simd_json::serde::to_owned_value(serde_json::json!({"post_type":"message","message_type":"private","user_id":42,"message":[{"type":"reply","data":{"id":"7"}},{"type":"at","data":{"qq":"100"}},{"type":"text","data":{"text":text}}]})).unwrap());
            assert_eq!(
                ["ctl", "控制", "插件"]
                    .iter()
                    .any(|name| match_word_command(&ctx, name).is_some()),
                matched
            );
        }
    }
}
