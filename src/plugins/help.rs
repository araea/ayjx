//! 帮助中心：插件总览与单插件详情。
//!
//! 两条展示线并存：
//!   - **卡片图**（默认）：把清单排版成一张图发出去，长内容不再刷屏，
//!     版式见 [`card`]，使用 Chromium 网页截图，字体与换行交给浏览器；
//!   - **纯文本**：`image_enabled = false` 或渲染失败时自动接管，
//!     内容与图一致，绝不出现「图里一套、文字另一套」。
//!
//! **清单只有一份来源：插件注册表**。每个插件的 `summary`、`commands`、
//! `section` 都写在 `plugins/registry.rs` 的注册项里，帮助中心只负责按
//! [`SECTIONS`] 分组和排版。新增一个插件不必改动本文件——它会自动出现在
//! 总览里；忘了填 `section` 会落到「其他」而不是消失，忘了填 `summary`
//! 则由单元测试当场拦下。

mod card;

use crate::adapters::satori::{LockedWriter, send_msg};
use crate::command::{extract_text_arg, get_prefixes, match_command};
use crate::config::build_config;
use crate::event::Context;
use crate::message::Message;
use crate::plugins::{Cmd, PluginError, get_config, get_plugins};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use toml::Value;

const LOG_TARGET: &str = "Plugin/Help";

#[derive(Serialize, Deserialize, Clone)]
struct Config {
    enabled: bool,
    /// 仅在私聊应答。帮助属于「自己翻手册」，留在群里只会刷屏；
    /// 打开后群里的 help 原样放行，不消耗事件。
    #[serde(default)]
    private_only: bool,
    /// 是否把帮助排版成卡片图；关掉或渲染失败时退回纯文本
    #[serde(default = "default_true")]
    image_enabled: bool,
    /// 卡片图渲染倍率（1.0—4.0）。倍率越高出图越清晰，3.0 在手机上放大也不糊
    #[serde(default = "default_image_scale")]
    image_scale: f64,
}

fn default_true() -> bool {
    true
}

fn default_image_scale() -> f64 {
    3.0
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            private_only: false,
            image_enabled: true,
            image_scale: default_image_scale(),
        }
    }
}

pub fn default_config() -> Value {
    build_config(Config::default())
}

fn load_config(ctx: &Context) -> Config {
    get_config::<Config>(ctx, "help").unwrap_or_default()
}

const TRIGGERS: &[&str] = &["help", "帮助", "插件列表"];

/// 总览里的一个插件条目
struct Entry {
    display: &'static str,
    name: &'static str,
    desc: &'static str,
    enabled: bool,
}

/// 总览的一个分区（按用途分组，而不是把二十来个插件堆成一长串）
struct Group {
    title: &'static str,
    /// 英文代号，只作版式上的次要标识
    en: &'static str,
    items: Vec<Entry>,
}

/// 分区表：决定总览的分组与顺序。
///
/// 插件在注册表里用 `section` 认领分区代号；代号对不上的（或没填的）
/// 统一落到最后一个分区，不会从帮助里消失。分区内的顺序沿用注册表顺序，
/// 因此新增插件不需要在这里登记第二遍。
const SECTIONS: &[(&str, &str, &str)] = &[
    ("message", "消息 · 媒体", "MESSAGE"),
    ("play", "互动 · 娱乐", "PLAY"),
    ("insight", "统计 · 资讯", "INSIGHT"),
    ("system", "系统 · 运维", "SYSTEM"),
    ("misc", "其他", "MISC"),
];

fn is_enabled(ctx: &Context, name: &str) -> bool {
    let guard = ctx.config.read().unwrap();
    guard
        .plugins
        .get(name)
        .and_then(|v| v.get("enabled"))
        .and_then(|x| x.as_bool())
        .unwrap_or(false)
}

/// 符号指令（`/#`、`~名`、`##`、`-#` 等）本身就是完整指令，不再拼接前缀
pub(crate) fn needs_prefix(cmd: &str) -> bool {
    !cmd.starts_with(['/', '#', '~', '-'])
}

fn prefix_of(ctx: &Context) -> String {
    get_prefixes(ctx).first().cloned().unwrap_or_default()
}

/// 按 [`SECTIONS`] 把已注册插件分组；代号对不上的落到最后一个分区，不会消失。
fn grouped(ctx: &Context) -> Vec<Group> {
    let known: Vec<&str> = SECTIONS.iter().map(|(code, _, _)| *code).collect();
    let fallback = known.last().copied().unwrap_or("misc");

    SECTIONS
        .iter()
        .map(|(code, title, en)| Group {
            title,
            en,
            items: get_plugins()
                .iter()
                .filter(|p| {
                    let sec = if known.contains(&p.section) { p.section } else { fallback };
                    sec == *code
                })
                .map(|p| Entry {
                    display: p.display_name,
                    name: p.name,
                    desc: p.summary,
                    enabled: is_enabled(ctx, p.name),
                })
                .collect(),
        })
        .filter(|g| !g.items.is_empty())
        .collect()
}

/// 与其他插件保持一致的分隔线
const DIVIDER: &str = "———————————————";

/// 纯文本总览：分区与卡片图一致，只是把版式换成缩进
fn render_overview(ctx: &Context, groups: &[Group]) -> String {
    let prefix = prefix_of(ctx);
    let total: usize = groups.iter().map(|g| g.items.len()).sum();
    let enabled = groups
        .iter()
        .flat_map(|g| g.items.iter())
        .filter(|e| e.enabled)
        .count();

    let mut out = format!(
        "🧩 ayjx 插件总览\n已启用 {} / {} 个插件 · 指令前缀 {}\n{}\n",
        enabled, total, prefix, DIVIDER
    );

    // 一条两行：首行是身份与开关，次行是它到底做什么，扫读时不必在长句里找边界
    for group in groups {
        out.push_str(&format!("\n▍{}（{} 项）\n", group.title, group.items.len()));
        for item in &group.items {
            let mark = if item.enabled { "✅" } else { "⬜" };
            out.push_str(&format!(
                "{} {}（{}）\n   {}\n",
                mark, item.display, item.name, item.desc
            ));
        }
    }

    out.push_str(&format!("\n{}", DIVIDER));
    out.push_str(&format!(
        "\n💡 看全部指令：{p}help <插件名>\n管理开关与配置：{p}ctl（聊天）\n连接：Satori v1；状态为配置开关，初始化及排期修改需重启。",
        p = prefix
    ));
    out
}

/// 纯文本详情
fn render_detail(ctx: &Context, entry: &Entry, cmds: &[Cmd]) -> String {
    let prefix = prefix_of(ctx);
    let status = if entry.enabled {
        "✅ 已启用"
    } else {
        "⬜ 已禁用"
    };

    let mut out = format!(
        "🧩 {}（{}）\n状态：{}\n{}\n📖 {}\n{}\n",
        entry.display, entry.name, status, DIVIDER, entry.desc, DIVIDER
    );

    if cmds.is_empty() {
        out.push_str("该插件在后台自动工作，没有需要手动触发的指令。");
        out.push_str(&format!(
            "\n管理：{prefix}ctl show {}；{prefix}ctl on/off {}",
            entry.name, entry.name
        ));
        return out;
    }

    out.push_str("⌨️ 指令\n");
    for c in cmds {
        let full = if needs_prefix(c.cmd) {
            format!("{}{}", prefix, c.cmd)
        } else {
            c.cmd.to_string()
        };
        if c.note.is_empty() {
            out.push_str(&format!("· {}\n", full));
        } else {
            out.push_str(&format!("· {}\n   {}\n", full, c.note));
        }
    }
    out.pop();
    out.push_str(&format!("\n管理：{prefix}ctl show {}；{prefix}ctl on/off {}\n生命周期参数与首次初始化需重启；详见 {prefix}ctl list。", entry.name, entry.name));
    out
}

fn not_found(ctx: &Context, name: &str) -> String {
    format!(
        "🔍 没有找到插件「{}」\n{}\n发送 {}help 可以查看全部插件。",
        name,
        DIVIDER,
        prefix_of(ctx)
    )
}

/// 插件的指令清单（来自注册表）
fn commands_of(name: &str) -> &'static [Cmd] {
    get_plugins()
        .iter()
        .find(|p| p.name == name)
        .map(|p| p.commands)
        .unwrap_or(&[])
}

/// 按配置键或中文显示名查插件——图上两个名字都印着，用哪个都该找得到
fn lookup(ctx: &Context, arg: &str) -> Option<Entry> {
    let arg = arg.trim();
    get_plugins()
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(arg) || p.display_name == arg)
        .map(|p| Entry {
            display: p.display_name,
            name: p.name,
            desc: p.summary,
            enabled: is_enabled(ctx, p.name),
        })
}

/// 一次回复的内容：文本必备，卡片可选（渲染失败时就靠文本兜底）
struct Reply {
    text: String,
    card: Option<card::Card>,
}

fn build_reply(ctx: &Context, arg: &str) -> Reply {
    let prefix = prefix_of(ctx);

    if arg.is_empty() {
        let groups = grouped(ctx);
        return Reply {
            text: render_overview(ctx, &groups),
            card: Some(card::overview(&groups, &prefix)),
        };
    }

    match lookup(ctx, arg) {
        Some(entry) => {
            let cmds = commands_of(entry.name);
            Reply {
                text: render_detail(ctx, &entry, cmds),
                card: Some(card::detail(&entry, cmds, &prefix)),
            }
        }
        // 找不到时只回一行提示，没必要为一句话专门出图
        None => Reply {
            text: not_found(ctx, arg),
            card: None,
        },
    }
}

/// 这条消息该不该由帮助插件应答。
///
/// `private_only` 打开后只在私聊开口；群聊里的 help 是普通消息，原样放行给
/// 后面的插件，而不是被静默吃掉。抽成纯函数是为了能不启浏览器就测这条门禁。
fn answers(config: &Config, group_id: Option<i64>) -> bool {
    !(config.private_only && group_id.is_some())
}

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        let msg = match ctx.as_message() {
            Some(m) => m,
            None => return Ok(Some(ctx)),
        };
        let config = load_config(&ctx);
        // 私聊专用时，群里的 help 当普通消息放行，交给后面的插件。
        if !answers(&config, msg.group_id()) {
            return Ok(Some(ctx));
        }

        for trigger in TRIGGERS {
            if let Some(matched) = match_command(&ctx, trigger) {
                let arg = extract_text_arg(&matched.args);
                let reply = build_reply(&ctx, &arg);

                let mut out = Message::new().reply(msg.message_id());
                let browser_path = ctx.config.read().unwrap().browser_path.clone();
                let image = match (&reply.card, config.image_enabled) {
                    (Some(c), true) => match c.render(config.image_scale, browser_path.as_deref()).await {
                        Ok(b64) => Some(b64),
                        Err(e) => {
                            warn!(target: LOG_TARGET, "帮助网页卡片出图失败，改发纯文本：{e}");
                            None
                        }
                    },
                    _ => None,
                };
                out = match image {
                    Some(b64) => out.image(format!("base64://{}", b64)),
                    None => out.text(reply.text),
                };

                send_msg(&ctx, writer, msg.group_id(), Some(msg.user_id()), out).await?;
                return Ok(None);
            }
        }

        Ok(Some(ctx))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 私聊专用只在私聊开口；群里的 help 原样放行，交给后面的插件。
    #[test]
    fn private_only_answers_private_but_lets_group_pass_through() {
        let private: Config = toml::from_str("enabled = true\nprivate_only = true").unwrap();
        assert!(answers(&private, None), "私聊必须应答");
        assert!(!answers(&private, Some(123456)), "群聊必须放行");

        // 默认（不设 private_only）时群里也应答，保持历史行为。
        let everywhere: Config = toml::from_str("enabled = true").unwrap();
        assert!(answers(&everywhere, None));
        assert!(answers(&everywhere, Some(123456)));
    }

    /// 分区代号写错会静默落到「其他」，看图的人不会察觉——所以在这里拦下
    #[test]
    fn every_plugin_claims_a_known_section() {
        let known: Vec<&str> = SECTIONS.iter().map(|(code, _, _)| *code).collect();
        for plugin in get_plugins() {
            assert!(
                known.contains(&plugin.section),
                "插件 {} 的 section = {:?} 不在 SECTIONS 里；可选：{:?}",
                plugin.name,
                plugin.section,
                known
            );
        }
    }

    /// 注册表里漏填 summary 的插件，在总览里只剩一个名字
    #[test]
    fn every_plugin_has_a_summary() {
        for plugin in get_plugins() {
            assert!(
                !plugin.summary.trim().is_empty(),
                "插件 {} 在 registry.rs 里缺少 summary",
                plugin.name
            );
        }
    }

    /// 分区表本身不该有摆设：每个代号都得有插件认领
    #[test]
    fn sections_are_all_in_use_or_are_the_fallback() {
        let fallback = SECTIONS.last().expect("SECTIONS 非空").0;
        for (code, title, _) in SECTIONS {
            if *code == fallback {
                continue;
            }
            assert!(
                get_plugins().iter().any(|p| p.section == *code),
                "分区 {title}（{code}）没有任何插件认领"
            );
        }
    }

    /// 分组必须覆盖全部插件，一个都不能丢
    #[test]
    fn grouping_loses_no_plugin() {
        let known: Vec<&str> = SECTIONS.iter().map(|(code, _, _)| *code).collect();
        let fallback = *known.last().unwrap();
        let mut seen = 0usize;
        for (code, _, _) in SECTIONS {
            seen += get_plugins()
                .iter()
                .filter(|p| {
                    let sec = if known.contains(&p.section) { p.section } else { fallback };
                    sec == *code
                })
                .count();
        }
        assert_eq!(seen, get_plugins().len(), "分组前后插件数量对不上");
    }

    /// 私聊专用默认关闭（保持历史行为），显式打开也不影响其余字段。
    #[test]
    fn private_only_defaults_off_and_can_be_turned_on() {
        // 老配置没有这个字段，必须能照常加载并保持「群里也应答」的旧行为。
        let base: Config = toml::from_str("enabled = true").unwrap();
        assert!(!base.private_only);
        let private: Config = toml::from_str("enabled = true\nprivate_only = true").unwrap();
        assert!(private.private_only);
        assert!(private.image_enabled);
    }

    #[test]
    fn symbol_commands_keep_their_own_prefix() {
        assert!(needs_prefix("help"));
        assert!(needs_prefix("词意榜"));
        assert!(!needs_prefix("/#"));
        assert!(!needs_prefix("~<名称> <内容>"));
        assert!(!needs_prefix("##<名称>"));
        assert!(!needs_prefix("-*"));
    }
}

/// Validate control edits against the plugin's actual configuration type.
pub fn validate_config(value: &toml::Value) -> Result<(), String> {
    <Config as serde::Deserialize>::deserialize(value.clone())
        .map(|_| ())
        .map_err(|_| "配置类型不匹配（请检查数组元素、字段类型及整数范围）".to_string())
}
