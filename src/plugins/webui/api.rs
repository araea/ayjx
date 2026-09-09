//! 面板的 JSON 接口。
//!
//! 四个写入口都不自己动配置：`toggle` 与 `reset` 复用 [`ctl::execute`]，`set`
//! 复用 [`ctl::set_value`]，于是校验、保命规则、写盘顺序和给出的回执都与
//! `/ctl` 一字不差——面板换个说法说同一件事，不会出现「网页上成功、聊天里失败」。
//!
//! 每次写入都把最新状态一并带回。前端不必再发一次 `state`，也就没有「按下开关
//! 到界面更新之间那半秒」——列表、徽标、差异计数在同一帧里一起变。

use super::schema;
use crate::event::Context;
use crate::plugins::{Cmd, Plugin, ctl, get_plugins, needs_startup, pending_startup};
use serde::Deserialize;
use serde_json::{Value as Json, json};
use toml::Value;

/// 分区标题。与帮助中心同一份分区，面板与 `/help` 的分组永远对得上。
const SECTIONS: &[(&str, &str)] = &[
    ("message", "消息 · 媒体"),
    ("play", "互动 · 娱乐"),
    ("insight", "统计 · 资讯"),
    ("system", "系统 · 运维"),
    ("misc", "其他"),
];

fn section_title(code: &str) -> &'static str {
    SECTIONS
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, title)| *title)
        .unwrap_or("其他")
}

fn section_code(code: &str) -> &'static str {
    SECTIONS
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(c, _)| *c)
        .unwrap_or("misc")
}

/// 配置指纹。前端拿它做轮询：变了才重新取整份状态，
/// 于是聊天里用 `/ctl` 改的东西几秒内自己出现在页面上。
pub(super) fn version(ctx: &Context) -> String {
    let snapshot = ctx.config.read().unwrap();
    config_version(&snapshot)
}

fn config_version(snapshot: &crate::config::AppConfig) -> String {
    let text = toml::to_string(snapshot).unwrap_or_default();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn commands(cmds: &'static [Cmd]) -> Vec<Json> {
    cmds.iter()
        .map(|c| json!({ "cmd": c.cmd, "note": c.note, "prefixed": crate::plugins::help::needs_prefix(c.cmd) }))
        .collect()
}

fn plugin_json(snapshot: &crate::config::AppConfig, p: &'static Plugin) -> Json {
    let defaults = (p.default_config)();
    let current = snapshot
        .plugins
        .get(p.name)
        .cloned()
        .unwrap_or_else(|| defaults.clone());

    let mut diff = Vec::new();
    ctl::differences(&defaults, &current, "", &mut diff);
    let enabled = current
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    json!({
        "name": p.name,
        "display": p.display_name,
        "section": section_code(p.section),
        "section_title": section_title(p.section),
        "summary": p.summary,
        "enabled": enabled,
        "pending": enabled && pending_startup(p.name),
        "restart": needs_startup(p.name),
        "commands": commands(p.commands),
        "fields": schema::fields(p, &current, &defaults),
        "diff": diff,
    })
}

/// 完整状态：全局概况 + 每个插件的开关、表单与差异。
pub(super) fn state(ctx: &Context) -> Json {
    // 字段与指纹来自同一份快照，避免并发保存时返回新指纹配旧字段。
    let snapshot = ctx.config.read().unwrap().clone();
    let (prefix, bots, filter) = {
        let prefix = snapshot.command_prefix.first().cloned().unwrap_or_default();
        let bots: Vec<Json> = snapshot
            .bots
            .iter()
            .map(|b| {
                json!({
                    "protocol": b.protocol,
                    "enabled": b.enabled,
                    "url": b.url.clone().unwrap_or_default(),
                })
            })
            .collect();
        let filter = json!({
            "enable_blacklist": snapshot.global_filter.enable_blacklist,
            "blacklist": snapshot.global_filter.blacklist,
            "enable_whitelist": snapshot.global_filter.enable_whitelist,
            "whitelist": snapshot.global_filter.whitelist,
        });
        (prefix, bots, filter)
    };

    let plugins: Vec<Json> = get_plugins()
        .iter()
        .map(|p| plugin_json(&snapshot, p))
        .collect();
    let on = plugins
        .iter()
        .filter(|p| p["enabled"].as_bool().unwrap_or(false))
        .count();

    json!({
        "version": config_version(&snapshot),
        "prefix": prefix,
        "sections": SECTIONS.iter().map(|(code, title)| json!({"code": code, "title": title})).collect::<Vec<_>>(),
        "plugins": plugins,
        "summary": {"enabled": on, "total": plugins.len()},
        "bots": bots,
        "global_filter": filter,
    })
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(super) enum Command {
    /// 开关一个插件
    Toggle { name: String, on: bool },
    /// 写入一个字段。`raw` 为真时 `value` 是一段 TOML 文本。
    Set {
        name: String,
        path: String,
        value: Json,
        #[serde(default)]
        raw: bool,
    },
    /// 恢复默认。`path` 为空即整插件恢复（保留开关与管理员）。
    Reset {
        name: String,
        #[serde(default)]
        path: String,
    },
}

/// 执行一条写入，成功与失败都带回最新状态。
pub(super) async fn execute(ctx: &Context, command: Command) -> (bool, String, Json) {
    let outcome = match command {
        Command::Toggle { name, on } => {
            let action = if on { "on" } else { "off" };
            ctl::execute(ctx, &format!("{action} {name}"))
                .await
                .map(|out| out.text)
        }
        Command::Reset { name, path } => {
            if path.is_empty() && name.eq_ignore_ascii_case("webui") {
                // 整体恢复仍走 ctl 原子事务，但保留当前访问凭据。
                ctl::change(ctx, |cfg| {
                    let old = cfg.plugins.get("webui").ok_or("配置不存在")?;
                    let mut value = super::default_config();
                    value["enabled"] = old["enabled"].clone();
                    value["token"] = old["token"].clone();
                    cfg.plugins.insert("webui".into(), value);
                    Ok("已恢复网页面板默认参数，保留开关与访问密钥；监听参数重启后生效。".into())
                })
                .await
            } else if path.is_empty() {
                ctl::execute(ctx, &format!("reset {name} --confirm"))
                    .await
                    .map(|out| out.text)
            } else {
                reset_field(ctx, &name, &path).await
            }
        }
        Command::Set {
            name,
            path,
            value,
            raw,
        } => set_field(ctx, &name, &path, &value, raw).await,
    };
    let state = state(ctx);
    match outcome {
        Ok(text) => (true, text, state),
        Err(text) => (false, text, state),
    }
}

fn current_value(ctx: &Context, plugin: &Plugin, path: &str) -> Option<Value> {
    let snapshot = ctx.config.read().unwrap();
    ctl::at(snapshot.plugins.get(plugin.name)?, path).cloned()
}

async fn set_field(
    ctx: &Context,
    name: &str,
    path: &str,
    value: &Json,
    raw: bool,
) -> Result<String, String> {
    let plugin = ctl::resolve(name)?;
    if path.is_empty() {
        return Err("缺少配置路径".into());
    }
    let old = current_value(ctx, plugin, path);
    let next = if raw {
        let text = value.as_str().ok_or("TOML 文本必须是字符串")?;
        schema::parse_toml(text, old.as_ref())?
    } else {
        schema::to_toml(value, old.as_ref())?
    };
    if plugin.name == "webui"
        && path == "token"
        && next.as_str().is_none_or(|s| s.trim().is_empty())
    {
        return Err("访问密钥不能为空；请填写新密钥后保存。".into());
    }
    ctl::set_value(ctx, plugin.name, path, next).await
}

async fn reset_field(ctx: &Context, name: &str, path: &str) -> Result<String, String> {
    let plugin = ctl::resolve(name)?;
    if plugin.name == "webui" && path == "token" {
        return Err("访问密钥不能恢复为空值；请填写新密钥后保存。".into());
    }
    let defaults = (plugin.default_config)();
    let value = ctl::at(&defaults, path)
        .cloned()
        .ok_or("该路径没有默认值")?;
    ctl::set_value(ctx, plugin.name, path, value).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::event::{BotStatus, EventType, LoginUser};
    use crate::matcher::Matcher;
    use crate::scheduler::Scheduler;
    use sea_orm::Database;
    use std::sync::{Arc, RwLock};
    use tokio::sync::Mutex as AsyncMutex;

    /// 面板永远以维护者身份执行，等同本机控制台。
    async fn context() -> Context {
        let mut config = AppConfig::default();
        for p in get_plugins() {
            config.plugins.insert(p.name.into(), (p.default_config)());
        }
        let path =
            std::env::temp_dir().join(format!("ayjx-webui-test-{}.toml", rand::random::<u64>()));
        Context {
            event: EventType::Init,
            config: Arc::new(RwLock::new(config)),
            config_save_lock: Arc::new(AsyncMutex::new(())),
            db: Database::connect("sqlite::memory:").await.unwrap(),
            scheduler: Arc::new(Scheduler::new()),
            matcher: Arc::new(Matcher::new()),
            config_path: Arc::from(path.to_str().unwrap()),
            bot: Arc::new(BotStatus {
                adapter: "console".into(),
                platform: "console".into(),
                login_user: LoginUser::default(),
            }),
        }
    }

    async fn cleanup(ctx: &Context) {
        let _ = tokio::fs::remove_file(ctx.config_path.as_ref()).await;
    }

    #[tokio::test]
    async fn state_covers_every_plugin_and_tracks_the_toggle() {
        let ctx = context().await;
        let before = state(&ctx);
        assert_eq!(
            before["plugins"].as_array().unwrap().len(),
            get_plugins().len()
        );
        assert!(before["summary"]["total"].as_u64().unwrap() > 0);

        let (ok, message, after) = execute(
            &ctx,
            Command::Toggle {
                name: "help".into(),
                on: false,
            },
        )
        .await;
        assert!(ok, "{message}");
        let help = after["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "help")
            .unwrap();
        assert_eq!(help["enabled"].as_bool(), Some(false));
        assert_ne!(before["version"], after["version"], "指纹必须随改动变化");
        cleanup(&ctx).await;
    }

    #[tokio::test]
    async fn writes_go_through_the_same_validation_as_ctl() {
        let ctx = context().await;
        // 浏览器送来的整数落到浮点字段上要被收敛，而不是被当成类型错误拒绝
        let (ok, message, _) = execute(
            &ctx,
            Command::Set {
                name: "help".into(),
                path: "image_scale".into(),
                value: json!(2),
                raw: false,
            },
        )
        .await;
        assert!(ok, "{message}");
        assert_eq!(
            ctx.config.read().unwrap().plugins["help"]["image_scale"].as_float(),
            Some(2.0)
        );

        // 越界值仍旧由 ctl 拦下，面板不放行
        let (ok, message, _) = execute(
            &ctx,
            Command::Set {
                name: "help".into(),
                path: "image_scale".into(),
                value: json!(9),
                raw: false,
            },
        )
        .await;
        assert!(!ok);
        assert!(message.contains("1 到 4"), "{message}");

        // 单项恢复默认
        let (ok, message, _) = execute(
            &ctx,
            Command::Reset {
                name: "help".into(),
                path: "image_scale".into(),
            },
        )
        .await;
        assert!(ok, "{message}");
        assert_eq!(
            ctx.config.read().unwrap().plugins["help"]["image_scale"].as_float(),
            Some(3.0)
        );
        cleanup(&ctx).await;
    }

    #[tokio::test]
    async fn arrays_and_raw_tables_round_trip() {
        let ctx = context().await;
        let (ok, message, state) = execute(
            &ctx,
            Command::Set {
                name: "repeater".into(),
                path: "channel.white".into(),
                value: json!([123, 456]),
                raw: false,
            },
        )
        .await;
        assert!(ok, "{message}");
        let repeater = state["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "repeater")
            .unwrap();
        assert!(
            repeater["diff"]
                .as_array()
                .unwrap()
                .iter()
                .any(|line| line.as_str().unwrap().contains("channel.white")),
            "{repeater}"
        );

        let (ok, message, _) = execute(
            &ctx,
            Command::Set {
                name: "ai_news".into(),
                path: "group_preferences".into(),
                value: json!("[\"175131947\"]\ncategory = \"model\"\n"),
                raw: true,
            },
        )
        .await;
        assert!(ok, "{message}");
        cleanup(&ctx).await;
    }

    #[tokio::test]
    async fn resetting_webui_preserves_access_and_empty_tokens_are_rejected() {
        let ctx = context().await;
        ctx.config
            .write()
            .unwrap()
            .plugins
            .get_mut("webui")
            .unwrap()["token"] = Value::String("test-key".into());
        let (ok, message, _) = execute(
            &ctx,
            Command::Reset {
                name: "webui".into(),
                path: String::new(),
            },
        )
        .await;
        assert!(ok, "{message}");
        assert_eq!(
            ctx.config.read().unwrap().plugins["webui"]["token"].as_str(),
            Some("test-key")
        );
        let (ok, _, _) = execute(
            &ctx,
            Command::Set {
                name: "webui".into(),
                path: "token".into(),
                value: json!(""),
                raw: false,
            },
        )
        .await;
        assert!(!ok);
        let (ok, _, _) = execute(
            &ctx,
            Command::Reset {
                name: "webui".into(),
                path: "token".into(),
            },
        )
        .await;
        assert!(!ok);
        cleanup(&ctx).await;
    }

    /// ctl 的保命规则在面板上同样生效：不能把管理入口关掉。
    #[tokio::test]
    async fn the_panel_cannot_lock_the_operator_out() {
        let ctx = context().await;
        let (ok, message, _) = execute(
            &ctx,
            Command::Toggle {
                name: "ctl".into(),
                on: false,
            },
        )
        .await;
        assert!(!ok);
        assert!(message.contains("管理入口"), "{message}");
        cleanup(&ctx).await;
    }
}
