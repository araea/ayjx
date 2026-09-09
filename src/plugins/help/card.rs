//! 网页卡片：共用纸面排版，由 Chromium 完成字体塑形、换行与 PNG 截图。

use super::{Entry, Group, needs_prefix};
use crate::plugins::Cmd;
use crate::render::web::{self, Block, Doc, Item, Theme, Tone};

/// 总览版心宽度（单列）
const OVERVIEW_WIDTH: f32 = 640.0;
/// 详情版心宽度（单栏，长指令自动换行）
const DETAIL_WIDTH: f32 = 640.0;

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
    /// 出图失败时由调用方回退到完整文本。
    pub async fn render(&self, scale: f64, browser_path: Option<&str>) -> anyhow::Result<String> {
        web::capture(&self.0, scale, browser_path).await
    }
}

/// 总览卡：分区 → 单列清单，状态文字与颜色同时呈现。
pub fn overview(groups: &[Group], prefix: &str) -> Card {
    let mut blocks = vec![
        Block::Title {
            title: "插件总览".into(),
            pill: None,
            sub: format!("按用途查找功能 · 指令前缀 {prefix}"),
        },
        // 汇总启用与停用数量，状态清单在下方逐项展开
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
            "Satori v1 · 管理开关与配置：{prefix}ctl（聊天）\n状态为配置开关；首次初始化及定时排期修改需重启。"
        ),
    });

    Card(Doc {
        theme: Theme::Help,
        width: OVERVIEW_WIDTH,
        kicker: "AYJX · MANUAL".into(),
        blocks,
        foot: "开关状态以当前配置为准".into(),
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
                    web::Cmd {
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
            "管理：{p}ctl show {n}\n开关：{p}ctl on/off {n}\n首次初始化及定时排期修改需重启；详见 {p}ctl list。",
            p = prefix,
            n = entry.name
        ),
    });

    Card(Doc {
        theme: Theme::Help,
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
    #[tokio::test]
    #[ignore = "需要 Chromium，落盘网页与 PNG 供人工核对"]
    async fn renders_sample_cards_to_png() {
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
        // oai 有全表最长的一条指令，样张顺带盯住「指令 chip 会不会顶出版心」
        let wide = get_plugins().iter().find(|p| p.name == "oai").unwrap();
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
            ("detail_widest", detail(&entry(wide, true), wide.commands, "/")),
        ];
        for (name, card) in cases {
            std::fs::write(format!("{dir}/{name}.html"), web::html(&card.0)).unwrap();
            let browser_path = std::env::var("CHROME_BIN").ok();
            let b64 = card.render(3.0, browser_path.as_deref()).await.expect("浏览器应当出图");
            let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &b64)
                .expect("应是合法 base64");
            assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G']), "{name} 应是 PNG");
            std::fs::write(format!("{dir}/{name}.png"), &bytes).unwrap();
            // 同时查看手机宽度的预览，避免只看高分辨率原图误判字号。
            let img = image::load_from_memory(&bytes).unwrap();
            img.resize(420, u32::MAX, image::imageops::FilterType::Lanczos3)
                .save(format!("{dir}/{name}_phone.png")).unwrap();
            println!("{name} 出图 {} 字节", bytes.len());
        }
        cdp_html_shot::Browser::shutdown_global().await;
    }

    #[test]
    fn symbol_commands_keep_their_own_form() {
        assert_eq!(full_cmd("/", "帮助"), "/帮助");
        assert_eq!(full_cmd("/", "/#"), "/#");
        assert_eq!(full_cmd("!", "~pi <任务>"), "~pi <任务>");
    }
}
