//! 词意卡片的网页版排版：同一份数据，改由浏览器出图。
//!
//! 为什么从原生绘制换到网页：原生那版每一处位置都是手算的常量，
//! 汉字与拉丁混排的宽度、长昵称的折行、行与行的呼吸都只能逼近；
//! 交给浏览器之后，字距、基线、省略号、弹性列宽都由排版引擎负责，
//! 于是版面能做得更松、层级更清楚，也更经得起长内容的折腾。
//!
//! 视觉取向没变，还是 [`super::card`] 定下的**宣纸 + 朱砂**：暖白纸底、
//! 古籍式内框、汉字走宋体、数字与元信息压在灰阶里、全卡只有朱砂一种强调色。
//! 判定「远近」的那套语义（五档距离、对数刻度的接近度）也直接复用 `card` 里
//! 的函数，图上说的和文本里说的仍是同一件事。
//!
//! 浏览器不可用时调用方退回 [`super::card`] 的原生绘制，再不行才发纯文本——
//! 三层兜底，功能任何时候都不受影响。

use super::card::{group, heat, stamp, tier};
use super::view::{Board, COMMAND_ROWS, HintRow, RankBoard, Reply, Win};
use crate::render::web::capture_html;
use anyhow::Result;

/// 版心宽度（CSS 像素）。配合 `image_scale`（默认 3 倍）出图约 1980px 宽，
/// 在手机聊天窗口里既是一屏可读的缩略图，点开又经得起放大。
const WIDTH: u32 = 660;

/// 动态内容一律转义后再拼进 HTML：群昵称、猜过的词都来自用户输入。
fn esc(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// 两格虚线田字格，压在标题右侧当配图
fn aside_cells() -> &'static str {
    r#"<div class="grid-cells"><span class="cell blank">？</span><span class="cell blank">？</span></div>"#
}

/// 一条提示：名次 + 距离档 | 留白牌 · 词 · 留白牌 | 接近度条 | 「新」印
fn hint(row: &HintRow, pool: usize) -> String {
    let (label, color) = tier(row.rank);
    let width = (heat(row.rank, pool) * 100.0).clamp(2.0, 100.0);
    format!(
        r#"<li class="hint{fresh}" style="--tier:{color}">
<div class="rk{wide}"><span class="hash">#</span><span class="num">{rank}</span><span class="tier">{label}</span></div>
<div><div class="trio"><span class="nb"><i class="q">？</i><i>{prev}</i></span><span class="word">{word}</span><span class="nb"><i>{next}</i><i class="q">？</i></span></div>
<div class="bar"><i style="width:{width:.1}%"></i></div></div>{flag}</li>"#,
        fresh = if row.fresh { " fresh" } else { "" },
        // 名次列宽是固定的，五位数得收一号字才不会顶到列边
        wide = if row.rank >= 10_000 { " wide" } else { "" },
        rank = row.rank,
        prev = esc(&row.prev),
        word = esc(&row.word),
        next = esc(&row.next),
        flag = if row.fresh {
            r#"<span class="flag">新</span>"#
        } else {
            ""
        },
    )
}

/// 三格数字卡（揭晓卡在不足一分钟时只有两格）
fn stats(items: &[(String, &str)]) -> String {
    let mut out = String::from(r#"<div class="stats">"#);
    for (value, label) in items {
        out.push_str(&format!(
            "<div><b>{}</b><span>{}</span></div>",
            esc(value),
            esc(label)
        ));
    }
    out.push_str("</div>");
    out
}

fn note(text: &str) -> String {
    format!(r#"<div class="note"><b>注</b>{}</div>"#, esc(text))
}

fn empty(text: &str) -> String {
    format!(r#"<div class="empty">{}</div>"#, esc(text))
}

/// 盘面卡：一局进行中的全部有效提示，按名次从近到远排
fn board_body(board: &Board) -> (String, String, String, String) {
    let mut body = String::new();
    if let Some(text) = &board.notice {
        body.push_str(&note(text));
    }
    if board.rows.is_empty() {
        body.push_str(&empty("还没有命中排名的词，换个方向再试一个"));
    } else {
        body.push_str(r#"<ol class="hints">"#);
        for row in &board.rows {
            body.push_str(&hint(row, board.pool));
        }
        body.push_str("</ol>");
    }
    let best = board
        .rows
        .first()
        .map(|row| format!("#{}", row.rank))
        .unwrap_or_else(|| "—".into());
    body.push_str(&stats(&[
        (group(board.guesses), "已猜次数"),
        (group(board.hits), "命中排名"),
        (best, "最近名次"),
    ]));

    let sub = format!(
        "已猜 {} 次 · 命中 {} 词 · 词池 {}",
        group(board.guesses),
        group(board.hits),
        group(board.pool)
    );
    let foot = match board.hidden {
        0 => "左邻更近 · 右邻更远 · ？为待猜的字".to_string(),
        n => format!("另有 {n} 条更远的记录未列出 · 左邻更近 · 右邻更远"),
    };
    ("今日词意".into(), sub, body, foot)
}

/// 揭晓卡：田字格从虚线换成实线、答案落格，右侧盖一枚「猜中」圆印
fn win_body(win: &Win) -> (String, String, String, String) {
    let cells: String = win
        .answer
        .chars()
        .map(|ch| format!(r#"<span class="cell filled">{}</span>"#, esc(&ch.to_string())))
        .collect();
    let mut body = format!(
        r#"<div class="reveal"><div class="grid-cells cells-lg">{cells}</div><div class="mark"><span>猜</span><span>中</span></div></div>"#
    );

    let mut tiles = vec![
        (group(win.guesses), "猜测次数"),
        (group(win.hits), "命中排名"),
    ];
    if let Some(elapsed) = win.elapsed() {
        tiles.push((elapsed, "本局历时"));
    }
    body.push_str(&stats(&tiles));
    body.push_str(&format!(
        r#"<div class="winner"><span>猜中者</span><b>{}</b></div>"#,
        esc(&win.winner)
    ));

    (
        "猜对了".into(),
        format!("今日答案「{}」已揭晓", esc(&win.answer)),
        body,
        "明日零点换新词 · 猜中次数已计入排行".into(),
    )
}

/// 排行榜卡：前三名实心朱砂号牌，其余描边；条长按榜首归一化
fn rank_body(rank: &RankBoard) -> (String, String, String, String) {
    if rank.items.is_empty() {
        return (
            esc(&rank.title),
            esc(&rank.subtitle),
            empty("当前还没有人猜对过，第一个名字留给你"),
            "猜中一次即入榜".into(),
        );
    }

    let top = rank.items.iter().map(|item| item.score).max().unwrap_or(1).max(1) as f64;
    let mut body = String::from(r#"<ol class="ranks">"#);
    for (index, item) in rank.items.iter().enumerate() {
        let width = (item.score as f64 / top * 100.0).clamp(3.0, 100.0);
        body.push_str(&format!(
            r#"<li class="rank{top_class}"><span class="place">{place}</span>
<div class="who"><b>{name}</b><div class="bar"><i style="width:{width:.1}%"></i></div></div>
<div class="score"><b>{score}</b><span>胜</span></div></li>"#,
            top_class = if index < 3 { " top" } else { "" },
            place = index + 1,
            name = esc(&item.name),
            score = item.score,
        ));
    }
    body.push_str("</ol>");

    let total: i64 = rank.items.iter().map(|item| item.score).sum();
    body.push_str(&stats(&[
        (group(rank.items.len()), "上榜人数"),
        (group(total.max(0) as usize), "合计猜中"),
        (group(top as usize), "榜首战绩"),
    ]));

    (
        esc(&rank.title),
        esc(&rank.subtitle),
        body,
        "每日一词 · 每人每日至多一胜".into(),
    )
}

/// 指令卡：左栏指令与别名，右栏一句话说明；前缀取本机真实配置，图上照抄即可
fn help_body(prefix: &str) -> (String, String, String, String) {
    let prefix = esc(prefix);
    let mut body = String::from(r#"<ol class="cmds">"#);
    for (cmd, extra, desc) in COMMAND_ROWS {
        let arg = if extra.starts_with('[') {
            format!(r#"<span class="arg">{}</span>"#, esc(extra))
        } else {
            String::new()
        };
        let alias = if !extra.is_empty() && !extra.starts_with('[') {
            format!(
                r#"<span class="alias">亦可 {prefix}{}</span>"#,
                esc(extra)
            )
        } else {
            String::new()
        };
        body.push_str(&format!(
            r#"<li class="cmd"><div class="name">{prefix}{}{arg}{alias}</div><p class="desc">{}</p></li>"#,
            esc(cmd),
            esc(desc),
        ));
    }
    body.push_str("</ol>");

    (
        "词意指令".into(),
        format!("共 {} 条指令 · 群内发送即可", COMMAND_ROWS.len()),
        body,
        format!("发送「{prefix}词意玩法」查看规则"),
    )
}

/// 玩法卡：示例直接用真实的提示行渲染——说明与实战看到的是同一套版式
fn rules_body() -> (String, String, String, String) {
    let sample = [
        ("镯子", 14, "器", "玉", false),
        ("玉佩", 15, "子", "东", false),
        ("东西", 16, "佩", "冥", true),
    ];
    let rows: String = sample
        .iter()
        .map(|(word, rank, prev, next, fresh)| {
            hint(
                &HintRow {
                    rank: *rank,
                    prev: (*prev).into(),
                    word: (*word).into(),
                    next: (*next).into(),
                    fresh: *fresh,
                },
                3000,
            )
        })
        .collect();

    let keys = [
        ("？佩", "名次更靠前（更近）的那个词，末字是「佩」", false),
        ("冥？", "名次更靠后（更远）的那个词，首字是「冥」", false),
        ("#15", "语义排名，数字越小离答案越近", false),
        ("新", "本次刚猜出来的那一条，会单独标出", true),
    ];
    let keys: String = keys
        .iter()
        .map(|(term, desc, solid)| {
            format!(
                r#"<li><span class="term{}">{}</span><p>{}</p></li>"#,
                if *solid { " solid" } else { "" },
                esc(term),
                esc(desc)
            )
        })
        .collect();

    let tiers = [
        ("咫尺", "#B0342A", "#1 — #10"),
        ("相邻", "#C0662A", "#11 — #50"),
        ("相近", "#A08420", "#51 — #200"),
        ("相关", "#4B7A62", "#201 — #1000"),
        ("天涯", "#5D6C85", "#1000 之后"),
    ];
    let tiers: String = tiers
        .iter()
        .map(|(label, color, range)| {
            format!(
                r#"<div style="--tier:{color}"><b>{}</b><span>{}</span></div>"#,
                esc(label),
                esc(range)
            )
        })
        .collect();

    let section = |num: &str, title: &str, para: &str| {
        format!(
            r#"<div class="sec"><b>{num}</b><h2>{}</h2></div><p class="para">{}</p>"#,
            esc(title),
            esc(para)
        )
    };

    let body = format!(
        concat!(
            "{one}{two}",
            r#"<ol class="hints">{rows}</ol><ul class="keys">{keys}</ul><div class="divider"></div>"#,
            r#"{three}<div class="tiers">{tiers}</div><div class="divider"></div>{four}"#,
        ),
        one = section(
            "一",
            "目标",
            "猜出系统每天选定的那个两字词语。全群共用一个词，谁先猜中算谁的。"
        ),
        two = section(
            "二",
            "反馈",
            "每猜一个词，若它落在该词的语义排名里，就会得到一个名次与它的左右邻词。名次越小，离答案越近。"
        ),
        three = section(
            "三",
            "远近",
            "名次按五档标注，色条越长离答案越近（对数刻度，前排之间的差距也看得出来）。"
        ),
        four = section(
            "四",
            "周期",
            "每日一词，猜中后次日零点换新；系统记录每个人的猜中次数，可用「词意榜」「词意全榜」查看。"
        ),
        rows = rows,
        keys = keys,
        tiers = tiers,
    );

    (
        "词意玩法".into(),
        "猜词 · 看名次 · 顺着邻词收网".into(),
        body,
        "发送「词意猜测 词语」即可开局".into(),
    )
}

/// 排版成一整页 HTML。`Notice` 不出图，返回 `None` 由调用方走文本。
///
/// `title` / `sub` / `foot` 里已经按需转义过：静态文案原样写，
/// 掺了用户内容（答案、榜单标题）的那几处在各自的 `*_body` 里就转义了。
pub fn html(reply: &Reply, prefix: &str) -> Option<String> {
    let (aside, (title, sub, body, foot)) = match reply {
        Reply::Board(board) => (true, board_body(board)),
        Reply::Win(win) => (false, win_body(win)),
        Reply::Rank(rank) => (false, rank_body(rank)),
        Reply::Help => (false, help_body(prefix)),
        Reply::Rules => (true, rules_body()),
        Reply::Notice(_) => return None,
    };

    Some(format!(
        r#"<!doctype html><html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'">
<title>词意 · {title}</title><style>{css}</style></head>
<body style="width:{WIDTH}px"><main class="shot"><div class="sheet"><div class="frame">
<header class="head"><div class="seal">词</div><div class="brand"><b>词意</b><span>每日一词 · 语义寻踪</span></div><span class="stamp">{stamp}</span></header>
<div class="lede"><div class="lede-text"><h1>{title}</h1><p class="sub">{sub}</p></div>{aside}</div>
<div class="rule"></div>
{body}
<footer><p>{foot}</p><div class="dots"><i></i><i></i><i></i></div></footer>
</div></div></main></body></html>"#,
        css = include_str!("../../../res/cards/ciyi.css"),
        stamp = stamp(),
        aside = if aside { aside_cells() } else { "" },
    ))
}

/// 出图：网页排版 + 无头浏览器截图，返回 PNG 的 base64。
pub async fn render(
    reply: &Reply,
    prefix: &str,
    scale: f64,
    browser_path: Option<&str>,
) -> Result<String> {
    let html = html(reply, prefix).ok_or_else(|| anyhow::anyhow!("该回应不出图"))?;
    capture_html(&html, WIDTH, scale, browser_path).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::ciyi::view::RankItem;
    use chrono::{Duration, Utc};

    fn sample_board() -> Board {
        let words = [
            (7, "环", "手镯", "玉"),
            (14, "器", "镯子", "玉"),
            (15, "子", "玉佩", "东"),
            (137, "佩", "东西", "冥"),
            (864, "西", "物件", "行"),
            (2301, "件", "行头", "衣"),
        ];
        Board {
            notice: None,
            rows: words
                .iter()
                .enumerate()
                .map(|(i, (rank, prev, word, next))| HintRow {
                    rank: *rank,
                    prev: (*prev).into(),
                    word: (*word).into(),
                    next: (*next).into(),
                    fresh: i == 3,
                })
                .collect(),
            hidden: 4,
            guesses: 21,
            hits: 10,
            pool: 4831,
        }
    }

    fn sample_win() -> Win {
        Win {
            answer: "东西".into(),
            winner: "夜航船".into(),
            guesses: 23,
            hits: 9,
            started_at: Utc::now() - Duration::minutes(194),
        }
    }

    fn sample_rank() -> RankBoard {
        RankBoard {
            title: "词意榜".into(),
            subtitle: "本群 · 猜中次数前 6 名".into(),
            items: [("夜航船", 12), ("清风", 9), ("南山", 7), ("拾遗", 4), ("白露", 2), ("无名氏", 1)]
                .iter()
                .map(|(name, score)| RankItem {
                    name: (*name).into(),
                    score: *score,
                })
                .collect(),
        }
    }

    #[test]
    fn notice_never_renders() {
        assert!(html(&Reply::Notice("不在词库中".into()), "/").is_none());
    }

    #[test]
    fn every_card_is_a_complete_page() {
        for reply in [
            Reply::Board(sample_board()),
            Reply::Win(sample_win()),
            Reply::Rank(sample_rank()),
            Reply::Help,
            Reply::Rules,
        ] {
            let out = html(&reply, "/").expect("这五类回应都应当出图");
            assert!(out.starts_with("<!doctype html>"));
            assert!(out.contains(r#"<main class="shot">"#), "截图锚点必须存在");
            assert!(out.ends_with("</html>"));
        }
    }

    /// 昵称与词都来自用户输入，必须以文本形态落进 HTML
    #[test]
    fn user_content_is_escaped() {
        let payload = "<script>alert('x')</script> & \"引号\"";
        let win = Win {
            winner: payload.into(),
            ..sample_win()
        };
        let out = html(&Reply::Win(win), "/").unwrap();
        assert!(!out.contains("<script>"));
        assert!(out.contains("&lt;script&gt;"));
        assert!(out.contains("default-src 'none'"));
    }

    /// 接近度条与榜单条的宽度都得落在 0—100%，否则版面会被撑破
    #[test]
    fn bar_widths_stay_within_the_track() {
        let board = Board {
            rows: vec![
                HintRow { rank: 1, prev: "甲".into(), word: "极近".into(), next: "乙".into(), fresh: false },
                HintRow { rank: 999_999, prev: "丙".into(), word: "极远".into(), next: "丁".into(), fresh: false },
            ],
            ..sample_board()
        };
        let out = html(&Reply::Board(board), "/").unwrap();
        for chunk in out.split("width:").skip(1) {
            let Some(value) = chunk.split('%').next() else { continue };
            let Ok(width) = value.trim().parse::<f64>() else { continue };
            assert!((0.0..=100.0).contains(&width), "条宽 {width} 越界");
        }
    }

    /// 把五种卡片各出一次图，供人工核对版面：
    ///   CIYI_WEB_DUMP=/tmp/ciyi cargo test ciyi::web::tests::dump -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "需要 Chromium；与其他截图测试串行"]
    async fn dump_sample_cards() {
        use base64::{Engine, engine::general_purpose::STANDARD};

        let Ok(dir) = std::env::var("CIYI_WEB_DUMP") else {
            return;
        };
        std::fs::create_dir_all(&dir).unwrap();

        let long = "✿ 今天也要元气满满地猜词哦超级无敌长的群昵称 ✿ QwQ";
        let cases: Vec<(&str, Reply)> = vec![
            ("board", Reply::Board(sample_board())),
            (
                "board_notice",
                Reply::Board(Board {
                    notice: Some("「玉佩」已经猜过了".into()),
                    hidden: 0,
                    ..sample_board()
                }),
            ),
            (
                "board_empty",
                Reply::Board(Board {
                    rows: vec![],
                    notice: None,
                    hidden: 0,
                    guesses: 3,
                    hits: 0,
                    pool: 4831,
                }),
            ),
            ("win", Reply::Win(sample_win())),
            (
                "win_long_name",
                Reply::Win(Win {
                    winner: long.into(),
                    ..sample_win()
                }),
            ),
            ("rank", Reply::Rank(sample_rank())),
            (
                "rank_long_name",
                Reply::Rank(RankBoard {
                    title: "词意全榜".into(),
                    subtitle: "全部群聊 · 猜中次数前 3 名".into(),
                    items: vec![
                        RankItem { name: long.into(), score: 12 },
                        RankItem { name: "无空格的超长英文名".repeat(4), score: 9 },
                        RankItem { name: "短名".into(), score: 1 },
                    ],
                }),
            ),
            (
                // 最大规模：注 + 十行 + 五位名次 + 折叠计数，版面最容易崩在这里
                "board_worst",
                Reply::Board(Board {
                    notice: Some("「衣物」不在词库中".into()),
                    rows: [
                        (7usize, "环", "手镯", "玉"),
                        (14, "器", "镯子", "玉"),
                        (15, "子", "玉佩", "东"),
                        (137, "佩", "东西", "冥"),
                        (864, "西", "物件", "行"),
                        (2301, "件", "行头", "衣"),
                        (4500, "行", "衣物", "穿"),
                        (7800, "物", "服装", "面"),
                        (12000, "衣", "穿戴", "环"),
                        (17000, "穿", "打扮", "天"),
                    ]
                    .iter()
                    .enumerate()
                    .map(|(i, (rank, prev, word, next))| HintRow {
                        rank: *rank,
                        prev: (*prev).into(),
                        word: (*word).into(),
                        next: (*next).into(),
                        fresh: i == 9,
                    })
                    .collect(),
                    hidden: 1234,
                    guesses: 21,
                    hits: 10,
                    pool: 18054,
                }),
            ),
            ("help", Reply::Help),
            ("rules", Reply::Rules),
        ];

        let browser = std::env::var("CHROME_BIN").ok();
        // 默认按线上倍数出图，尺寸护栏与实际观感都按真实产物核对
        let scale = std::env::var("CIYI_WEB_SCALE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3.0);
        for (name, reply) in cases {
            let b64 = render(&reply, "/", scale, browser.as_deref())
                .await
                .unwrap_or_else(|e| panic!("{name} 应当出图：{e}"));
            let bytes = STANDARD.decode(&b64).expect("应是合法 base64");
            std::fs::write(format!("{dir}/{name}.png"), &bytes).unwrap();
            println!("{name} 出图 {} 字节", bytes.len());
        }
        // 全局浏览器实例不关，测试进程会连同整棵 Chromium 树一直挂着不退出
        cdp_html_shot::Browser::shutdown_global().await;
    }
}
