//! 网页卡片：共用纸面排版，由 Chromium 完成字体塑形、换行与 PNG 截图。

use crate::plugins::help::needs_prefix;
use crate::render::web::{self, Block, Doc, Row, Theme, Tile, Tone};

/// 控制面板版心：状态清单一行一个插件。
/// 640px 版心让正文在手机预览中保持可读大小。
const WIDTH: f32 = 640.0;

/// 一张待渲染的控制卡
pub struct Card(Doc);

impl Card {
    /// 出图失败时由调用方回退到完整文本。
    pub async fn render(&self, scale: f64, browser_path: Option<&str>) -> anyhow::Result<String> {
        web::capture(&self.0, scale, browser_path).await
    }
}

fn doc(kicker: &str, blocks: Vec<Block>, foot: &str, hint: (String, String)) -> Card {
    Card(Doc {
        theme: Theme::Control,
        width: WIDTH,
        kicker: kicker.into(),
        blocks,
        foot: foot.into(),
        hint,
    })
}

/// 用法卡：指令清单直接取自注册表里 ctl 自己的 `commands`，
/// 改指令只改注册表一处，图、纯文本用法与 `/help ctl` 三处同步。
pub fn usage(prefix: &str, cmds: &[crate::plugins::Cmd]) -> Card {
    let blocks = vec![
        Block::Title {
            title: "插件控制".into(),
            pill: None,
            sub: format!("别名 {prefix}控制 · {prefix}插件 —— 全局开关与配置的统一入口"),
        },
        Block::Rule,
        Block::Section {
            title: "指令".into(),
            en: "COMMANDS".into(),
            count: format!("{} 条", cmds.len()),
        },
        Block::Cmds(
            cmds.iter()
                .map(|c| {
                    let primary = c.cmd.split(" / ").next().unwrap_or(c.cmd).trim();
                    let aliases: Vec<String> = c
                        .cmd
                        .split(" / ")
                        .skip(1)
                        .map(|a| format!("{prefix}{}", a.trim()))
                        .collect();
                    web::Cmd {
                        prefix: if needs_prefix(primary) { prefix.into() } else { String::new() },
                        cmd: primary.into(),
                        note: c.note.into(),
                        aliases,
                    }
                })
                .collect(),
        ),
        Block::Section {
            title: "示例".into(),
            en: "EXAMPLES".into(),
            count: String::new(),
        },
        Block::Code(vec![
            format!("{prefix}ctl on 帮助中心 ping"),
            format!("{prefix}ctl set repeater channel.white [123456]"),
            format!("{prefix}ctl set ciyi plugin.image_scale 2"),
            format!("{prefix}ctl reset ai_news --confirm"),
        ]),
        Block::Callout {
            tone: Tone::Info,
            text: "中文操作：列表、开启、关闭、查看、默认、设置、重置、差异。\n\
                   插件名支持英文名与中文显示名，多个名称以空格或逗号分隔。\n\
                   查看与修改配置仅限 ctl.admins；本机控制台始终可管理。\n\
                   ctl 保留管理入口；修改它的 admins 请在私聊或控制台执行。"
                .into(),
        },
        Block::Callout {
            tone: Tone::Info,
            text: "带生命周期的插件（有 on_init / on_connected 的）首次启用与排期改动需重启才完整生效，\
                   状态清单里标注「待重启」；其余改动下一条消息即生效。"
                .into(),
        },
    ];
    doc(
        "AYJX · CONTROL",
        blocks,
        "状态以当前配置为准 · 待重启项需重启生效",
        ("查看全局状态".into(), format!("{prefix}ctl list")),
    )
}

/// 一行插件状态
pub struct Status {
    pub name: &'static str,
    pub display: &'static str,
    pub on: bool,
    /// 已开启但要等重启才真正跑起来
    pub pending: bool,
}

/// 状态卡：三个合计 + 一行一个插件
pub fn list(prefix: &str, filter: &str, rows: &[Status]) -> Card {
    let on = rows.iter().filter(|r| r.on).count();
    let pending = rows.iter().filter(|r| r.pending).count();
    let sub = if filter.is_empty() {
        format!("全局配置 · 共 {} 个插件 · 已启用 {on} 个", rows.len())
    } else {
        format!("筛选「{filter}」· 命中 {} 个 · 已启用 {on} 个", rows.len())
    };

    let mut blocks = vec![
        Block::Title { title: "插件状态".into(), pill: None, sub },
        Block::Tiles(vec![
            Tile { value: on.to_string(), label: "已启用".into() },
            Tile { value: (rows.len() - on).to_string(), label: "已停用".into() },
            Tile { value: pending.to_string(), label: "待重启".into() },
        ]),
        Block::Rule,
    ];

    if rows.is_empty() {
        blocks.push(Block::Callout {
            tone: Tone::Empty,
            text: "没有匹配的插件；去掉筛选词可以看到全部。".into(),
        });
    } else {
        blocks.push(Block::Rows(
            rows.iter()
                .map(|r| Row {
                    on: r.on,
                    main: r.display.into(),
                    sub: r.name.into(),
                    tail: if r.pending { "待重启".into() } else { String::new() },
                })
                .collect(),
        ));
    }

    blocks.push(Block::Callout {
        tone: Tone::Info,
        text: format!(
            "开关：{prefix}ctl on/off <插件...>　　配置：{prefix}ctl show <插件>\n\
             「待重启」= 已开启但生命周期钩子要等下次启动才跑起来。"
        ),
    });

    doc(
        "CONTROL · STATUS",
        blocks,
        "状态以当前配置为准 · 待重启项需重启生效",
        ("查看用法".into(), format!("{prefix}ctl")),
    )
}

/// 配置卡：`show` 与 `defaults` 共用；`path` 为空表示整份配置
pub fn config(prefix: &str, plugin: &str, path: &str, defaults: bool, body: &str, effect: &str) -> Card {
    let title = if defaults { "默认配置" } else { "当前配置" };
    let blocks = vec![
        Block::Title {
            title: title.into(),
            pill: None,
            sub: if path.is_empty() {
                format!("{plugin} · 全部字段")
            } else {
                format!("{plugin} · {path}")
            },
        },
        Block::Rule,
        Block::Code(body.lines().map(str::to_string).collect()),
        Block::Callout { tone: Tone::Info, text: effect.into() },
    ];
    doc(
        "CONTROL · CONFIG",
        blocks,
        "含 token / secret / password 的字段一律显示为「已隐藏」",
        ("修改某一项".into(), format!("{prefix}ctl set {plugin} <路径> <值>")),
    )
}

/// 差异卡：只列与默认值不同的项
pub fn diff(prefix: &str, plugin: &str, lines: &[String]) -> Card {
    let mut blocks = vec![
        Block::Title {
            title: "配置差异".into(),
            pill: None,
            sub: format!("{plugin} · 当前值与默认值的出入"),
        },
        Block::Rule,
    ];
    if lines.is_empty() {
        blocks.push(Block::Callout {
            tone: Tone::Empty,
            text: "配置与默认值完全一致，没有改动过的项。".into(),
        });
    } else {
        blocks.push(Block::Section {
            title: "改动项".into(),
            en: "CHANGED".into(),
            count: format!("{} 项", lines.len()),
        });
        blocks.push(Block::Code(lines.to_vec()));
    }
    blocks.push(Block::Callout {
        tone: Tone::Info,
        text: format!("恢复默认：{prefix}ctl reset {plugin} [路径] --confirm\n整插件重置会保留开关与 ctl.admins。"),
    });
    doc(
        "CONTROL · DIFF",
        blocks,
        "左为默认值，右为当前值",
        ("查看默认配置".into(), format!("{prefix}ctl defaults {plugin}")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::get_plugins;

    /// 把四种控制卡各出一次图，确认都能走完绘制并编码成 PNG：
    ///   CTL_CARD_DUMP=/tmp/ctl cargo test ctl::card -- --ignored
    #[tokio::test]
    #[ignore = "需要 Chromium，落盘网页与 PNG 供人工核对"]
    async fn renders_sample_cards_to_png() {
        let Ok(dir) = std::env::var("CTL_CARD_DUMP") else {
            return;
        };
        std::fs::create_dir_all(&dir).unwrap();

        // 状态样张取自真实注册表，条目数与线上一致
        let rows: Vec<Status> = get_plugins()
            .iter()
            .enumerate()
            .map(|(i, p)| Status {
                name: p.name,
                display: p.display_name,
                on: i % 7 != 4,
                pending: i % 7 == 2,
            })
            .collect();

        let body = "[plugin]\nenabled = true\nimage_scale = 3.0\n\n[schedule]\ndaily = \"08:30\"\ntargets = [123456, 789012]\napi_key = \"<已隐藏>\"";
        let diffs = vec![
            "channel.white: [] → [123456]".to_string(),
            "threshold: 3 → 5".to_string(),
            "probability: 0.5 → 0.85".to_string(),
        ];
        let ctl_cmds = get_plugins().iter().find(|p| p.name == "ctl").unwrap().commands;

        let cases: Vec<(&str, Card)> = vec![
            ("usage", usage("/", ctl_cmds)),
            ("list", list("/", "", &rows)),
            ("config", config("/", "ai_news", "", false, body, "下一条消息生效。")),
            ("diff", diff("/", "repeater", &diffs)),
            ("diff_clean", diff("/", "ping", &[])),
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
}
