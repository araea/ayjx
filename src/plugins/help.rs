//! 帮助中心：插件总览与单插件详情。
//!
//! 两条展示线并存：
//!   - **卡片图**（默认）：把清单排版成一张图发出去，长内容不再刷屏，
//!     版式见 [`card`]；
//!   - **纯文本**：`image_enabled = false` 或截图失败时自动接管，
//!     内容与图一致，绝不出现「图里一套、文字另一套」。
//!
//! 清单本身只有一份来源：[`describe`] 给出每个插件的说明与指令，
//! [`SECTIONS`] 给出总览的分区顺序。新插件若忘了写进分区表，
//! 会被兜底归入「其他」而不是从帮助里消失——单元测试也会盯着这件事。

mod card;

use crate::adapters::satori::{LockedWriter, send_msg};
use crate::command::{extract_text_arg, get_prefixes, match_command};
use crate::config::build_config;
use crate::event::Context;
use crate::message::Message;
use crate::plugins::{PluginError, get_config, get_plugins};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use toml::Value;

const LOG_TARGET: &str = "Plugin/Help";

#[derive(Serialize, Deserialize, Clone)]
struct Config {
    enabled: bool,
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

/// 单条指令说明：cmd 为指令（可含参数占位符），note 为用途说明（可为空）
struct Cmd {
    cmd: &'static str,
    note: &'static str,
}

/// 展开为 `&'static [Cmd]`（结构体字面量，可静态提升）
macro_rules! cmds {
    ( $( ($c:expr, $n:expr) ),* $(,)? ) => {
        &[ $( Cmd { cmd: $c, note: $n } ),* ]
    };
}

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

/// 分区表：决定总览的分组与顺序。未列出的插件统一落到「其他」，不会丢。
struct Section {
    title: &'static str,
    en: &'static str,
    members: &'static [&'static str],
}

const SECTIONS: &[Section] = &[
    Section {
        title: "消息 · 媒体",
        en: "MESSAGE",
        members: &[
            "media",
            "sticker",
            "image_split",
            "gif",
            "echo",
            "recall",
            "webshot",
        ],
    },
    Section {
        title: "互动 · 娱乐",
        en: "PLAY",
        members: &["repeater", "ping", "group_title", "ciyi", "oai"],
    },
    Section {
        title: "统计 · 资讯",
        en: "INSIGHT",
        members: &["recorder", "wordcloud", "stats", "ai_news"],
    },
    Section {
        title: "系统 · 运维",
        en: "SYSTEM",
        members: &["settings", "help", "restart", "logger", "meta_filter"],
    },
];

/// 每个插件的完整指令清单。仅作展示用，与各插件实际指令保持同步。
fn describe(name: &str) -> (&'static str, &'static [Cmd]) {
    match name {
        "meta_filter" => ("过滤心跳/元事件，避免噪声进入流水线", &[]),
        "logger" => ("将收到的消息打印到控制台日志", &[]),
        "recorder" => (
            "把消息记录到数据库，为词云、统计等插件提供数据源",
            &[],
        ),
        "media" => (
            "媒体与链接互转：图片/视频 ↔ 直链",
            cmds![
                ("转链接 / 看链接 / 提取地址 / url", "将图片/视频转为直链（可引用消息）"),
                ("转图片 / 预览", "将链接转为图片发送"),
                ("转视频", "将链接转为视频发送"),
            ],
        ),
        "sticker" => (
            "保存/收藏对方发送的表情或图片（需引用原消息）",
            cmds![
                ("收 / 偷 / 存表情", "引用表情/图片后收藏"),
                ("表情转图片", "引用动画表情后转为静态图片"),
            ],
        ),
        "group_title" => (
            "Bot 为群主时，给申请者设置群专属头衔",
            cmds![("我要头衔 <文字>", "给自己申请一个群专属头衔")],
        ),
        "ping" => (
            "心跳测试，统计全服 Ping 次数",
            cmds![("ping", "测试 Bot 在线状态")],
        ),
        "recall" => (
            "撤回引用的消息（需引用回复使用）",
            cmds![("撤回", "引用要撤回的消息后发送")],
        ),
        "echo" => (
            "回显参数内容（支持图片等富文本）",
            cmds![("echo <内容>", "原样回显参数")],
        ),
        "repeater" => ("群聊复读机，达到阈值后自动跟读", &[]),
        "wordcloud" => (
            "根据消息记录生成词云图",
            cmds![
                ("<范围><时间>词云", "范围：本群/跨群/我的；时间：今日/昨日/本周/上周/近7天/近30天/本月/上月/今年/去年/总"),
                ("本群今日词云", "示例：本群今日"),
                ("我的总词云", "示例：个人全部"),
            ],
        ),
        "stats" => (
            "群统计图表：发言/表情/消息类型排行榜与走势，支持早中晚与周月的错峰定时推送",
            cmds![
                ("<范围><时间><类型><图表>", "范围：本群/跨群/我的/所有群；时间：今日…总；类型：发言/表情包/消息类型；图表：排行榜/走势"),
                ("本群今日发言排行榜", "示例"),
                ("本群本周发言走势", "示例"),
                ("所有群近7天发言排行榜", "示例：跨全部群"),
            ],
        ),
        "gif" => (
            "GIF 工具箱：合成、变速、倒放、缩放等",
            cmds![
                ("gif帮助 / gifhelp", "GIF 工具使用帮助"),
                ("合成gif", "多张图片合成 GIF"),
                ("gif变速", "调整播放速度"),
                ("gif倒放", "反向播放"),
                ("gif信息", "查看 GIF 帧数/尺寸等"),
                ("gif缩放", "调整 GIF 尺寸"),
                ("gif旋转", "旋转角度"),
                ("gif翻转", "水平/垂直翻转"),
                ("gif拆分", "拆分为单帧图片"),
                ("gif拼图", "多张图片拼接"),
            ],
        ),
        "image_split" => (
            "将一张图按行列切片",
            cmds![("裁剪 <行>x<列> / 切图 / 分割", "如：裁剪 3x3")],
        ),
        "ciyi" => (
            "词意游戏：猜词与排行榜",
            cmds![
                ("词意帮助 / 词意指令 / 词意指令列表 / 词意帮助列表", "查看指令列表"),
                ("词意玩法 / 词意规则", "查看游戏规则"),
                ("词意猜测 [词语]", "开始猜词或提交答案"),
                ("词意榜", "当前频道排行榜"),
                ("词意全榜", "全服排行榜"),
            ],
        ),
        "webshot" => ("自动对消息中的网页链接进行截图", &[]),
        "oai" => (
            "多智能体对话：## 创建智能体，~对话，模型/历史管理（符号指令）",
            cmds![
                ("oai", "查看使用帮助"),
                ("oai <API地址> <密钥>", "配置模型 API"),
                ("##<名称>(<描述>) <模型> <提示词>", "创建智能体"),
                ("~<名称> <内容>", "与智能体对话"),
                ("~<名称> 停止", "停止智能体回复"),
                ("-#<名称>", "删除智能体"),
                ("~#<名称>", "复制智能体"),
                ("~=<名称> <新名>", "重命名智能体"),
                ("##:<描述...>", "自动填充智能体描述"),
                ("/#", "智能体列表"),
                ("/%", "模型列表"),
                ("-*", "清空所有公开智能体"),
                ("-*!", "清空全部（含私有）"),
            ],
        ),
        "ai_news" => (
            "AI 资讯 / 热点 / 日报 / 模型榜（数据源 AIHOT）：实时快报 + 定时推送 + 随时查询，日夜主题卡片配合可点击的链接附录",
            cmds![
                ("ai资讯 / ai新闻", "最近 24 小时 AI 精选资讯"),
                ("ai热点", "当前 AI 热点榜 Top 10"),
                ("ai日报", "最新一期 AI 日报"),
                ("ai模型榜 / 模型排行榜", "AIHOT 大模型排行榜：共识分、评测完整度与官网参考价"),
                ("ai搜索 <关键词>", "近 7 天按关键词检索 AI 资讯"),
                ("ai推送添加 <群|私聊> <ID>", "从任意群聊或私聊添加指定推送目标"),
                ("ai推送删除 <群|私聊> <ID>", "从任意群聊或私聊删除指定推送目标"),
                ("ai推送开启 / ai推送关闭", "不带参数时开启/关闭当前会话，也可指定目标"),
                ("ai推送列表", "查看全部群聊与私聊推送目标"),
                ("ai实时开启 / ai实时关闭", "当前或指定目标是否接收实时快报"),
                ("ai实时模式 <精选|全部>", "默认仅推精选；可切换实时资讯来源"),
                ("ai分类 <分类>", "当前目标独立选择模型、产品、行业、论文、技巧或全部"),
                ("ai静默 <时间段>", "当前目标独立设置实时静默时段，如 23:30-07:30"),
                ("ai推送状态 [目标]", "查看当前或指定目标的开关、实时参数与排期"),
                ("ai推送重置 [目标]", "清空当前或指定目标的去重记录"),
                ("设置 ai_news card_theme auto", "阅读主题自动切换；也可使用 light / dark"),
            ],
        ),
        "settings" => (
            "查看/修改机器人可调设置，无需编辑配置文件",
            cmds![
                ("设置", "查看全部可调项"),
                ("设置 <插件> <键>", "查看某项详情"),
                ("设置 <插件> <键> <值>", "修改并自动保存"),
            ],
        ),
        "help" => (
            "显示本帮助信息",
            cmds![
                ("help / 帮助 / 插件列表", "插件总览"),
                ("help <插件名>", "查看插件详情与全部指令"),
            ],
        ),
        "restart" => (
            "每日定时自动重启 + 内存阈值监控，防止长时间运行卡顿",
            cmds![("restart", "手动重启（需先在设置中开启 allow_manual_restart）")],
        ),
        _ => ("(暂无说明)", &[]),
    }
}

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
fn needs_prefix(cmd: &str) -> bool {
    !cmd.starts_with(['/', '#', '~', '-'])
}

fn prefix_of(ctx: &Context) -> String {
    get_prefixes(ctx)
        .first()
        .cloned()
        .unwrap_or_else(|| "/".into())
}

/// 按 [`SECTIONS`] 把已注册插件分组；漏配的插件归入「其他」，不会从帮助里消失。
fn grouped(ctx: &Context) -> Vec<Group> {
    let plugins = get_plugins();
    let entry = |name: &str| -> Option<Entry> {
        plugins.iter().find(|p| p.name == name).map(|p| Entry {
            display: p.display_name,
            name: p.name,
            desc: describe(p.name).0,
            enabled: is_enabled(ctx, p.name),
        })
    };

    let mut groups: Vec<Group> = SECTIONS
        .iter()
        .map(|sec| Group {
            title: sec.title,
            en: sec.en,
            items: sec.members.iter().filter_map(|n| entry(n)).collect(),
        })
        .filter(|g| !g.items.is_empty())
        .collect();

    let rest: Vec<Entry> = plugins
        .iter()
        .filter(|p| !SECTIONS.iter().any(|s| s.members.contains(&p.name)))
        .filter_map(|p| entry(p.name))
        .collect();
    if !rest.is_empty() {
        groups.push(Group {
            title: "其他",
            en: "MISC",
            items: rest,
        });
    }
    groups
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
        "\n💡 看某个插件的全部指令：{p}help <插件名>\n   例：{p}help ai_news",
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

/// 按配置键或中文显示名查插件——图上两个名字都印着，用哪个都该找得到
fn lookup(ctx: &Context, arg: &str) -> Option<Entry> {
    let arg = arg.trim();
    get_plugins()
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(arg) || p.display_name == arg)
        .map(|p| Entry {
            display: p.display_name,
            name: p.name,
            desc: describe(p.name).0,
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
            let cmds = describe(entry.name).1;
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

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        let msg = match ctx.as_message() {
            Some(m) => m,
            None => return Ok(Some(ctx)),
        };

        for trigger in TRIGGERS {
            if let Some(matched) = match_command(&ctx, trigger) {
                let config = load_config(&ctx);
                let arg = extract_text_arg(&matched.args);
                let reply = build_reply(&ctx, &arg);

                let mut out = Message::new().reply(msg.message_id());
                let image = match (&reply.card, config.image_enabled) {
                    (Some(c), true) => match c.capture(config.image_scale).await {
                        Ok(b64) => Some(b64),
                        Err(e) => {
                            warn!(target: LOG_TARGET, "帮助卡片渲染失败，改发纯文本: {}", e);
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

    #[test]
    fn every_registered_plugin_belongs_to_a_section() {
        for plugin in get_plugins() {
            assert!(
                SECTIONS.iter().any(|s| s.members.contains(&plugin.name)),
                "插件 {} 没有写进 SECTIONS，总览里会被归到「其他」",
                plugin.name
            );
        }
    }

    #[test]
    fn sections_only_reference_registered_plugins() {
        for section in SECTIONS {
            for name in section.members {
                assert!(
                    get_plugins().iter().any(|p| &p.name == name),
                    "分区 {} 里的 {} 已不在插件注册表中",
                    section.title,
                    name
                );
            }
        }
    }

    #[test]
    fn every_plugin_has_a_description() {
        for plugin in get_plugins() {
            assert_ne!(
                describe(plugin.name).0,
                "(暂无说明)",
                "插件 {} 缺少说明",
                plugin.name
            );
        }
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
