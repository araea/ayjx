//! 网页控制台：在本机浏览器里可视化管理全部插件的开关与配置。
//!
//! **它不是第二套配置系统**。每一次改动最终都落到 [`ctl`](super::ctl) 的同一条
//! 事务上——同样的类型校验、同样的保命规则（不能关掉 ctl、不能锁死管理入口）、
//! 同样的「先写盘再改内存」。面板只做两件 ctl 做不好的事：
//!
//!   1. **把配置的形状显示出来**。`/ctl show` 打印一段 TOML，改一个布尔值也要
//!      记住路径、拼对字面量；面板从插件的默认配置推导出表单，开关就是开关，
//!      群号列表就是一排可增删的标签，每一项旁边写着默认值。
//!   2. **让「改错了」有退路**。每一项都能单独恢复默认，改动过的项一眼看得见。
//!
//! 安全边界只有一道：**密钥**。默认只监听 `127.0.0.1`，密钥在首次启动时随机生成
//! 并写进 `config.toml`；拿到密钥的人以维护者身份操作，与本机控制台等同。
//! 想让手机以外的设备访问就得改 `bind`——那是明确的选择，不是默认。
//! 密钥只在私聊或控制台里给出，群里索取一律拒绝。

mod api;
mod schema;
mod server;

use crate::adapters::satori::{LockedWriter, send_msg};
use crate::command::{extract_text_arg, match_word_command};
use crate::config::build_config;
use crate::event::Context;
use crate::message::Message;
use crate::plugins::{PluginError, get_config_or_default, update_config};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use toml::Value;

pub(crate) const LOG_TARGET: &str = "Plugin/WebUI";

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Config {
    pub enabled: bool,
    /// 监听地址。默认只有本机能连；改成 0.0.0.0 就是把维护者权限开放给整个局域网。
    pub bind: String,
    /// 监听端口。
    pub port: u16,
    /// 访问密钥。留空时首次启动随机生成并写回配置。
    pub token: String,
    /// 对外展示的地址（走反向代理或需要用局域网 IP 打开时填），留空则用监听地址拼。
    pub public_url: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            bind: "127.0.0.1".to_string(),
            port: 6520,
            token: String::new(),
            public_url: String::new(),
        }
    }
}

pub fn default_config() -> Value {
    build_config(Config::default())
}

pub fn validate_config(value: &Value) -> Result<(), String> {
    let config = Config::deserialize(value.clone())
        .map_err(|_| "bind 必须是字符串，port 必须是 1—65535 的整数，token/public_url 必须是字符串".to_string())?;
    if config.port == 0 {
        return Err("port 必须在 1—65535 之间".into());
    }
    if config.bind.trim().is_empty() {
        return Err("bind 不能为空；只监听本机填 127.0.0.1".into());
    }
    Ok(())
}

/// 服务端在进程内的落点：一个维护者身份的上下文，加上实际绑定到的地址。
///
/// 密钥不放在这里——它活在配置里，每次请求现读，于是 `/webui 重置密钥` 与
/// `/ctl set webui token` 都立刻生效，不需要重启。
pub(crate) struct Running {
    pub ctx: Context,
    pub addr: std::net::SocketAddr,
}

static RUNNING: OnceLock<Running> = OnceLock::new();

pub(crate) fn running() -> Option<&'static Running> {
    RUNNING.get()
}

fn new_token() -> String {
    format!("{:032x}", rand::random::<u128>())
}

/// 面板地址。配置了 `public_url` 就用它，否则按监听地址拼——
/// `0.0.0.0` 对浏览器没有意义，换成 `127.0.0.1` 让链接总是可点。
fn link(config: &Config, addr: Option<std::net::SocketAddr>) -> String {
    let base = if !config.public_url.trim().is_empty() {
        config.public_url.trim().trim_end_matches('/').to_string()
    } else {
        let host = match config.bind.as_str() {
            "0.0.0.0" | "::" | "[::]" => "127.0.0.1".to_string(),
            host if host.contains(':') => format!("[{host}]"),
            host => host.to_string(),
        };
        let port = addr.map(|a| a.port()).unwrap_or(config.port);
        format!("http://{host}:{port}")
    };
    format!("{base}/?k={}", config.token)
}

pub fn init(ctx: Context) -> BoxFuture<'static, Result<(), PluginError>> {
    Box::pin(async move {
        let mut config = get_config_or_default::<Config>(&ctx, "webui");

        // 首次启动补一把密钥。写回配置而不是只留在内存里：重启后链接不变，
        // 浏览器里存着的那把钥匙还能用。
        if config.token.trim().is_empty() {
            let token = new_token();
            let assigned = token.clone();
            update_config::<Config, _>(&ctx, "webui", move |mut c| {
                c.token = assigned;
                c
            })
            .await?;
            config.token = token;
            info!(target: LOG_TARGET, "已为网页面板生成访问密钥并写入配置。");
        }

        let listener = tokio::net::TcpListener::bind((config.bind.as_str(), config.port))
            .await
            .map_err(|e| format!("网页面板无法监听 {}:{}：{e}", config.bind, config.port))?;
        let addr = listener.local_addr()?;

        let _ = RUNNING.set(Running {
            ctx: crate::plugins::ctl::bridge::maintainer(&ctx),
            addr,
        });
        tokio::spawn(server::serve(listener));

        info!(target: LOG_TARGET, "网页面板已就绪：{}", link(&config, Some(addr)));
        if !config.bind.starts_with("127.") && config.bind != "localhost" {
            warn!(
                target: LOG_TARGET,
                "网页面板监听在 {} —— 拿到密钥的人即拥有维护者权限，请确认这是有意为之。",
                config.bind
            );
        }
        Ok(())
    })
}

const TRIGGERS: &[&str] = &["webui", "面板", "网页面板"];

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        let Some(matched) = TRIGGERS
            .iter()
            .find_map(|name| match_word_command(&ctx, name))
        else {
            return Ok(Some(ctx));
        };
        let Some(msg) = ctx.as_message() else {
            return Ok(Some(ctx));
        };
        let arg = extract_text_arg(&matched.args);
        let reply = respond(&ctx, arg.trim(), msg.group_id().is_some()).await;

        send_msg(
            &ctx,
            writer,
            msg.group_id(),
            Some(msg.user_id()),
            Message::new().reply(msg.message_id()).text(reply),
        )
        .await?;
        Ok(None)
    })
}

/// `/webui` 的三种回答。密钥只在私聊与控制台给出。
async fn respond(ctx: &Context, arg: &str, in_group: bool) -> String {
    let config = get_config_or_default::<Config>(ctx, "webui");
    let addr = running().map(|r| r.addr);
    let status = match addr {
        Some(addr) => format!("运行中，监听 {addr}"),
        None => "未启动（端口被占用或首次启用后尚未重启）".to_string(),
    };

    match arg {
        "状态" | "status" => format!(
            "🖥️ 网页面板\n状态：{status}\n配置：bind = {}，port = {}\n获取带密钥的地址请在私聊或本机控制台发送 webui。",
            config.bind, config.port
        ),
        _ if !super::ctl::is_manager(ctx) => super::ctl::DENIED.to_string(),
        "重置密钥" | "reset" | "reset-key" | "换密钥" => {
            if in_group {
                return "密钥不在群里给出，请私聊或使用本机控制台。".into();
            }
            let token = new_token();
            let assigned = token.clone();
            match update_config::<Config, _>(ctx, "webui", move |mut c| {
                c.token = assigned;
                c
            })
            .await
            {
                Ok(()) => {
                    let mut next = config.clone();
                    next.token = token;
                    format!(
                        "🔑 已生成新密钥，旧链接立即失效。\n{}\n浏览器里打开一次即可记住。",
                        link(&next, addr)
                    )
                }
                Err(e) => format!("重置失败：{e}"),
            }
        }
        "" => {
            if in_group {
                return "面板密钥不在群里给出，请私聊或使用本机控制台发送 webui。".into();
            }
            format!(
                "🖥️ 网页面板 · {status}\n{}\n打开后密钥会记在浏览器里，之后直接访问首页即可。\n换密钥：webui 重置密钥",
                link(&config, addr)
            )
        }
        other => format!("未知操作「{other}」。可用：webui、webui 状态、webui 重置密钥。"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_bind_to_loopback_and_validate() {
        let value = default_config();
        validate_config(&value).unwrap();
        assert_eq!(value["bind"].as_str(), Some("127.0.0.1"));
        assert_eq!(value["enabled"].as_bool(), Some(true));
    }

    #[test]
    fn invalid_bind_and_port_are_refused() {
        let mut value = default_config();
        value["bind"] = Value::String(" ".into());
        assert!(validate_config(&value).is_err());
        let mut value = default_config();
        value["port"] = Value::Integer(0);
        assert!(validate_config(&value).is_err());
        let mut value = default_config();
        value["port"] = Value::Integer(70_000);
        assert!(validate_config(&value).is_err());
    }

    #[test]
    fn wildcard_binds_become_a_clickable_loopback_link() {
        let config = Config {
            bind: "0.0.0.0".into(),
            port: 6520,
            token: "abc".into(),
            ..Config::default()
        };
        assert_eq!(link(&config, None), "http://127.0.0.1:6520/?k=abc");
        let proxied = Config {
            public_url: "https://panel.example/".into(),
            ..config
        };
        assert_eq!(link(&proxied, None), "https://panel.example/?k=abc");
    }
}
