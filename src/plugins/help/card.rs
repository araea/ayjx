//! 把帮助排版成一张卡片图（原生绘制，不走浏览器）。
//!
//! 设计取向：**蓝图 / 说明书**。帮助本质上是一张「装配图」——先给全貌，
//! 再给单件的接线方式，所以整张卡片按图纸的语言组织：坐标纸底纹、四角印前
//! 规线、分区标题带英文代号并用虚线延伸到计数，扫读时不必逐条去数。
//!
//! 状态只用一种编码贯穿全卡：主色 = 已启用，虚线灰格 = 已停用。总览顶部那排
//! 小格是「一格一个插件」的实心仪表，不是按比例拉伸的进度条——数格子就能对上清单。
//!
//! 版式与配色都来自框架的 [`crate::render::kit`]：这里只负责把注册表里的数据
//! 翻译成 [`Block`] 序列。新增一个插件不必碰这个文件；新增一种版式也只需在
//! kit 里加一种 Block，帮助与控制台同时受益。
//!
//! 版心按「群聊里先看缩略图、再点开看」定：总览 880px（双栏），
//! 详情 760px（单栏，指令行不折行）；配合 `image_scale`（默认 3 倍）出图。

use super::{Entry, Group, needs_prefix};
use crate::plugins::Cmd;
use crate::render::kit::{self, Block, Doc, Item, Theme, Tone};

/// 总览版心宽度（双栏）
const OVERVIEW_WIDTH: f32 = 880.0;
/// 详情版心宽度（单栏，指令行要能一行放下）
const DETAIL_WIDTH: f32 = 760.0;

/// `"收 / 偷 / 存表情"` → 主指令 + 别名。清单里的别名用 ` / ` 分隔，
/// 图里把第一个抬成主指令，其余降级成小字，避免一行挤三个同义词。
fn split_aliases(cmd: &str) -> (&str, Vec<&str>) {
    let mut parts = cmd.split(" / ").map(str::trim).filter(|s| !s.is_empty());
    let primary = parts.next().unwrap_or(cmd);
    (primary, parts.collect())
}

/// 带前缀的完整指令。符号指令本身就是完整写法，不再拼前缀。
fn full_cmd(prefix: &str, cmd: &str) -> String {
    if needs_prefix(cmd) {
        format!("{prefix}{cmd}")
    } else {
        cmd.to_string()
    }
}

/// 一张待渲染的卡片：Doc 与它的渲染入口绑在一起
pub struct Card(Doc);

impl Card {
    /// 出图，返回 PNG base64。字体不可用时返回 None，调用方退回纯文本。
    pub fn render(&self, scale: f64) -> Option<String> {
        kit::render(&self.0, scale)
    }
}

/// 总览卡：分区 → 双栏清单。主色格 = 已启用，虚线灰格 = 已停用。
pub fn overview(groups: &[Group], prefix: &str) -> Card {
    let total: usize = groups.iter().map(|g| g.items.len()).sum();
    let enabled = groups.iter().flat_map(|g| g.items.iter()).filter(|e| e.enabled).count();

    let mut blocks = vec![
        Block::Title {
            title: "插件总览".into(),
            pill: None,
            sub: format!("共 {total} 个插件 · 已启用 {enabled} 个 · 指令前缀 {prefix}"),
        },
        // 仪表：一格一个插件，顺序与下方清单一致
        Block::Meter(groups.iter().flat_map(|g| g.items.iter()).map(|e| e.enabled).collect()),
        Block::Rule,
    ];

    for group in groups {
        blocks.push(Block::Section {
            title: group.title.into(),
            en: group.en.into(),
            count: format!("{} 项", group.items.len()),
        });
        blocks.push(Block::Items(
            group
                .items
                .iter()
                .map(|e| Item {
                    name: e.display.into(),
                    key: e.name.into(),
                    desc: e.desc.into(),
                    on: e.enabled,
                })
                .collect(),
        ));
    }

    blocks.push(Block::Callout {
        tone: Tone::Info,
        text: format!(
            "Satori v1 · 管理开关与配置：{prefix}ctl（聊天）/ {prefix}webui（网页面板）\n状态为配置开关；首次初始化及定时排期修改需重启。"
        ),
    });

    Card(Doc {
        theme: Theme::blueprint(),
        width: OVERVIEW_WIDTH,
        kicker: "AYJX · MANUAL".into(),
        blocks,
        foot: "实心格与主色 = 已启用 · 虚线格 = 已停用".into(),
        hint: ("查看某个插件的全部指令".into(), format!("{prefix}help <插件名>")),
    })
}

/// 详情卡：一条指令一格，主指令抬到 chip 里，别名与说明依次下沉
pub fn detail(entry: &Entry, cmds: &[Cmd], prefix: &str) -> Card {
    let mut blocks = vec![
        Block::Title {
            title: entry.display.into(),
            pill: Some((
                if entry.enabled { "已启用" } else { "已停用" }.into(),
                entry.enabled,
            )),
            sub: format!("配置键 {}", entry.name),
        },
        Block::Callout { tone: Tone::Info, text: entry.desc.into() },
    ];

    if cmds.is_empty() {
        blocks.push(Block::Callout {
            tone: Tone::Empty,
            text: "该插件在后台自动工作，没有需要手动触发的指令。".into(),
        });
    } else {
        blocks.push(Block::Section {
            title: "指令".into(),
            en: "COMMANDS".into(),
            count: format!("{} 条", cmds.len()),
        });
        blocks.push(Block::Cmds(
            cmds.iter()
                .map(|c| {
                    let (primary, aliases) = split_aliases(c.cmd);
                    kit::Cmd {
                        prefix: if needs_prefix(primary) { prefix.into() } else { String::new() },
                        cmd: primary.into(),
                        note: c.note.into(),
                        aliases: aliases.iter().map(|a| full_cmd(prefix, a)).collect(),
                    }
                })
                .collect(),
        ));
    }

    blocks.push(Block::Callout {
        tone: Tone::Info,
        text: format!(
            "管理：{p}ctl show {n}\n开关：{p}ctl on/off {n}\n网页面板：{p}webui（表单化改配置，与 ctl 同一套校验）\n首次初始化及定时排期修改需重启；详见 {p}ctl list。",
            p = prefix,
            n = entry.name
        ),
    });

    Card(Doc {
        theme: Theme::blueprint(),
        width: DETAIL_WIDTH,
        kicker: "MANUAL · PLUGIN".into(),
        blocks,
        foot: "AYJX · 插件手册".into(),
        hint: ("回到插件总览".into(), format!("{prefix}help")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_split_off_the_primary_command() {
        let (primary, aliases) = split_aliases("收 / 偷 / 存表情");
        assert_eq!(primary, "收");
        assert_eq!(aliases, vec!["偷", "存表情"]);
        let (only, none) = split_aliases("ping");
        assert_eq!(only, "ping");
        assert!(none.is_empty());
    }

    /// 把两张帮助卡各出一次图，确认能走完绘制并编码成 PNG：
    ///   HELP_CARD_DUMP=/tmp/help cargo test help::card -- --ignored
    #[test]
    #[ignore = "需要可用的 CJK 字体，落盘 PNG 供人工核对"]
    fn renders_sample_cards_to_png() {
        use crate::plugins::get_plugins;
        let Ok(dir) = std::env::var("HELP_CARD_DUMP") else {
            return;
        };
        std::fs::create_dir_all(&dir).unwrap();

        // 用真实注册表造样张：分区、条目数与线上完全一致
        let known: Vec<&str> = crate::plugins::help::SECTIONS.iter().map(|(c, _, _)| *c).collect();
        let groups: Vec<Group> = crate::plugins::help::SECTIONS
            .iter()
            .map(|(code, title, en)| Group {
                title,
                en,
                items: get_plugins()
                    .iter()
                    .filter(|p| {
                        let sec = if known.contains(&p.section) { p.section } else { "misc" };
                        sec == *code
                    })
                    .enumerate()
                    .map(|(i, p)| Entry {
                        display: p.display_name,
                        name: p.name,
                        desc: p.summary,
                        // 让样张同时含启用与停用两种形态
                        enabled: i % 5 != 3,
                    })
                    .collect(),
            })
            .filter(|g| !g.items.is_empty())
            .collect();

        let ai = get_plugins().iter().find(|p| p.name == "ai_news").unwrap();
        let bg = get_plugins().iter().find(|p| p.name == "repeater").unwrap();
        let entry = |p: &'static crate::plugins::Plugin, on| Entry {
            display: p.display_name,
            name: p.name,
            desc: p.summary,
            enabled: on,
        };

        let cases: Vec<(&str, Card)> = vec![
            ("overview", overview(&groups, "/")),
            ("detail_ai_news", detail(&entry(ai, true), ai.commands, "/")),
            ("detail_background", detail(&entry(bg, false), bg.commands, "/")),
        ];
        for (name, card) in cases {
            let b64 = card.render(3.0).expect("字体可用时应当出图");
            let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &b64)
                .expect("应是合法 base64");
            assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G']), "{name} 应是 PNG");
            std::fs::write(format!("{dir}/{name}.png"), &bytes).unwrap();
            println!("{name} 出图 {} 字节", bytes.len());
        }
    }

    #[test]
    fn symbol_commands_keep_their_own_form() {
        assert_eq!(full_cmd("/", "帮助"), "/帮助");
        assert_eq!(full_cmd("/", "/#"), "/#");
        assert_eq!(full_cmd("!", "~pi <任务>"), "~pi <任务>");
    }
}
