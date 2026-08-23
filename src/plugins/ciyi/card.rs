//! 把词意的每一次回应排版成一张卡片图（HTML → 截图）。
//!
//! 设计取向：**宣纸 + 朱砂**。词意猜的是汉字与词义，所以整张卡走纸墨一路——
//! 暖白纸底带极淡的纤维颗粒，古籍式的内外双框，正文汉字一律用宋体，
//! 数字与元信息用无衬线压在灰阶里，只有「朱砂红」一种强调色，
//! 用在印章、名次、新提示这几处，其余全部让给内容。
//!
//! 几个刻意为之的细节：
//!   - **田字格**：未揭晓时是两个虚线田字格里的「？」，猜中后同一位置换成实线格
//!     配答案二字——同一个位置、同一种格子，把「悬念 → 揭晓」画成一个动作；
//!   - **邻词做成留白牌**：`？佩` 不是一串符号，而是一枚虚线小牌，
//!     未知的那个字用浅色「？」占位，一眼看得出「缺哪个字」；
//!   - **接近度按对数刻度画条**：名次 1 与 10 的差距远大于 900 与 1000，
//!     线性条会把前排全挤成满格，取 `1 - ln(rank)/ln(pool)` 才能把前排拉开；
//!   - **五档距离命名**（咫尺/相邻/相近/相关/天涯）：让「#137」这种抽象数字
//!     有一个可直接读出来的语感，颜色同步从朱砂过渡到远山蓝。
//!
//! 版心 720 CSS px，配合 `image_scale`（默认 3 倍）出图 2160px 宽，
//! 群聊里点开放大不糊。

use super::view::{Board, COMMAND_ROWS, HintRow, RankBoard, Reply, Win};
use anyhow::Result;
use cdp_html_shot::{Browser, CaptureOptions, Viewport};
use chrono::{FixedOffset, Utc};
use std::time::Duration;

/// 卡片渲染宽度（CSS 像素）。实际出图宽度 = WIDTH × `image_scale`
const WIDTH: u32 = 720;

fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// 出图时刻（北京时间），压在页眉右上角
fn stamp() -> String {
    let tz = FixedOffset::east_opt(8 * 3600).expect("UTC+8 是合法时区偏移");
    Utc::now().with_timezone(&tz).format("%m-%d %H:%M").to_string()
}

/// 千分位：词池动辄四五位数，不分节读不出量级
fn group(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

// ================= 距离分档 =================

/// 名次对应的「距离感」：一个能读出来的词 + 一种颜色。
///
/// 分档参照词意的实际手感：前十基本咬住答案，五十以内已在同一语义簇，
/// 两百以内还算相近，千以内只是沾亲带故，再往后就是天涯了。
fn tier(rank: usize) -> (&'static str, &'static str) {
    match rank {
        0..=10 => ("咫尺", "#B0342A"),
        11..=50 => ("相邻", "#C0662A"),
        51..=200 => ("相近", "#A08420"),
        201..=1000 => ("相关", "#4B7A62"),
        _ => ("天涯", "#5D6C85"),
    }
}

/// 接近度条的长度。
///
/// 取对数刻度：名次每翻一倍，条长退一档固定距离。线性刻度下
/// 名次 1 和名次 50 在一个万词的词池里几乎一样长，那条就白画了。
fn heat(rank: usize, pool: usize) -> f64 {
    let rank = rank.max(1) as f64;
    let pool = pool.max(rank as usize + 1) as f64;
    (1.0 - rank.ln() / pool.ln()).clamp(0.04, 1.0)
}

// ================= 样式 =================

const CSS: &str = r#"
*{margin:0;padding:0;box-sizing:border-box}
:root{
  --serif:"Source Han Serif SC","Noto Serif CJK SC","Songti SC","STSong","SimSun",Georgia,serif;
  --sans:"PingFang SC","Noto Sans CJK SC","Source Han Sans SC","Microsoft YaHei","WenQuanYi Zen Hei","Helvetica Neue",Arial,sans-serif;
  --ink:#241F1B;--ink2:#4B433A;--ink3:#8B7F70;--line:rgba(120,95,65,.20);--red:#B0342A}
.shot{padding:26px;background:#DCD2BE;
  background-image:radial-gradient(rgba(255,255,255,.42) 1px,transparent 1px);
  background-size:7px 7px}
/* 纸：两层极淡的颗粒 + 一层竖向明度渐变，凑出宣纸的手感，不用贴图 */
.card{position:relative;overflow:hidden;border-radius:5px;padding:42px 44px 30px;
  background-color:#FAF6EC;
  background-image:
    radial-gradient(rgba(120,90,60,.045) 1px,transparent 1px),
    radial-gradient(rgba(120,90,60,.032) 1px,transparent 1px),
    linear-gradient(175deg,rgba(255,253,247,.9),rgba(244,237,224,.85));
  background-size:13px 13px,23px 23px,100% 100%;
  background-position:0 0,8px 11px,0 0;
  box-shadow:0 16px 40px rgba(70,52,32,.20);
  font-family:var(--sans);color:var(--ink);
  -webkit-font-smoothing:antialiased;text-rendering:geometricPrecision}
/* 古籍式内框：离纸边 11px 的一道细线，比加分割线更安静 */
.card::after{content:"";position:absolute;inset:11px;border:1px solid rgba(150,110,70,.16);
  border-radius:2px;pointer-events:none}
.card>*{position:relative;z-index:1}

/* —— 页眉 —— */
.head{display:flex;align-items:center;justify-content:space-between;margin-bottom:22px}
.brand{display:flex;align-items:center;gap:13px}
.seal{width:42px;height:42px;border-radius:5px;background:var(--red);color:#FBF7EF;
  display:flex;align-items:center;justify-content:center;font-family:var(--serif);
  font-size:25px;font-weight:700;transform:rotate(-3deg);
  box-shadow:0 5px 13px rgba(176,52,42,.30),inset 0 0 0 1px rgba(255,255,255,.18)}
.bt b{display:block;font-family:var(--serif);font-size:20px;font-weight:700;letter-spacing:.24em;
  line-height:1.2;color:var(--ink)}
.bt i{display:block;margin-top:4px;font-style:normal;font-size:12px;letter-spacing:.2em;color:var(--ink3)}
.stamp{font-size:13px;letter-spacing:.06em;color:var(--ink3);font-variant-numeric:tabular-nums}

/* —— 标题区 —— */
.lede{display:flex;align-items:flex-end;justify-content:space-between;gap:24px}
.title{font-family:var(--serif);font-size:38px;font-weight:700;line-height:1.2;
  letter-spacing:.06em;color:var(--ink)}
.sub{margin-top:11px;font-size:14.5px;line-height:1.6;letter-spacing:.03em;color:var(--ink3);
  font-variant-numeric:tabular-nums}
.rule{height:2px;margin:22px 0 6px;border-radius:2px;
  background:linear-gradient(90deg,var(--red) 0 54px,rgba(120,95,65,.22) 54px 100%)}

/* —— 田字格 —— */
.cells{display:flex;gap:10px}
.cell{position:relative;width:74px;height:74px;border:2px solid rgba(176,52,42,.30);border-radius:4px;
  display:flex;align-items:center;justify-content:center;background:rgba(255,255,255,.5);
  font-family:var(--serif);font-size:42px;line-height:1;color:var(--ink)}
.cell::before{content:"";position:absolute;left:9%;right:9%;top:50%;
  border-top:1px dashed rgba(176,52,42,.26)}
.cell::after{content:"";position:absolute;top:9%;bottom:9%;left:50%;
  border-left:1px dashed rgba(176,52,42,.26)}
.cell.blank{border-style:dashed;font-size:30px;color:rgba(176,52,42,.42)}
.cells.sm .cell{width:50px;height:50px;font-size:26px}
.cells.sm .cell.blank{font-size:21px}
.cells.lg .cell{width:104px;height:104px;font-size:60px;border-color:rgba(176,52,42,.5)}

/* —— 提示行 —— */
.row{position:relative;display:grid;grid-template-columns:84px 1fr 32px;gap:16px;align-items:center;
  padding:14px 12px}
.row::after{content:"";position:absolute;left:12px;right:12px;bottom:0;
  border-bottom:1px dotted var(--line)}
.row:last-child::after{display:none}
.row.fresh{border-radius:9px;background:rgba(176,52,42,.055);
  box-shadow:inset 3px 0 0 var(--red)}
.rk{text-align:right}
.rk b{display:block;font-size:25px;font-weight:800;line-height:1;letter-spacing:-.02em;
  color:var(--c);font-variant-numeric:tabular-nums}
.rk b s{text-decoration:none;font-size:16px;font-weight:700;opacity:.5;margin-right:1px}
.rk em{display:block;margin-top:7px;font-style:normal;font-family:var(--serif);font-size:12.5px;
  font-weight:600;letter-spacing:.2em;color:var(--c);opacity:.8}
.tri{display:flex;align-items:center;gap:13px}
.wd{font-family:var(--serif);font-size:31px;font-weight:600;letter-spacing:.12em;color:var(--ink)}
/* 邻词牌：缺的那个字用浅色「？」占位，缺口一眼可见 */
.nb{font-family:var(--serif);font-size:19px;letter-spacing:.1em;color:#7E7264;
  padding:4px 10px 5px;border-radius:6px;background:rgba(120,95,65,.055);
  border:1px dashed rgba(120,95,65,.24)}
.nb i{font-style:normal;color:rgba(120,95,65,.42)}
.bar{margin-top:11px;height:5px;border-radius:3px;background:rgba(120,95,65,.10);overflow:hidden}
.bar span{display:block;height:100%;width:var(--w);border-radius:3px;
  background:linear-gradient(90deg,rgba(255,255,255,.45),var(--c))}
.flag{width:26px;height:26px;border-radius:5px;background:var(--red);color:#FBF7EF;
  display:flex;align-items:center;justify-content:center;font-family:var(--serif);font-size:14px;
  transform:rotate(-6deg);box-shadow:0 3px 9px rgba(176,52,42,.28)}

/* —— 提示条 / 空盘 —— */
.note{display:flex;align-items:center;gap:11px;margin:18px 0 2px;padding:12px 16px;
  border-radius:8px;background:rgba(176,52,42,.06);border-left:3px solid var(--red);
  font-size:16px;line-height:1.5;color:#6D4A40}
.note b{width:24px;height:24px;flex:none;border-radius:4px;background:rgba(176,52,42,.14);
  color:var(--red);display:flex;align-items:center;justify-content:center;
  font-family:var(--serif);font-size:14px;font-weight:700}
.empty{margin:22px 0 6px;padding:34px 20px;border:1px dashed var(--line);border-radius:10px;
  text-align:center;font-family:var(--serif);font-size:18px;letter-spacing:.06em;color:var(--ink3)}

/* —— 数字卡 —— */
.stats{display:flex;gap:13px;margin-top:24px}
/* min-width:0 不能省：flex 子项默认 min-width:auto，
   里面 nowrap 的长昵称会把整行撑出卡片再被裁掉，省略号永远等不到 */
.stat{flex:1;min-width:0;padding:15px 10px 14px;border-radius:10px;text-align:center;
  background:rgba(255,255,255,.55);border:1px solid var(--line)}
.stat b{display:block;font-size:26px;font-weight:800;line-height:1.15;letter-spacing:-.01em;
  color:var(--ink);font-variant-numeric:tabular-nums}
.stat b.w{font-family:var(--serif);font-size:21px;font-weight:700;letter-spacing:.04em;
  white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.stat span{display:block;margin-top:8px;font-size:12px;letter-spacing:.16em;color:var(--ink3)}

/* —— 揭晓 —— */
/* 昵称通栏放，最多两行；超长的（含一长串不换行字符）就断行后省略，
   overflow-wrap:anywhere 保证没有空格的长串也能断 */
.winner{display:flex;align-items:baseline;gap:14px;margin-top:13px;padding:15px 20px;
  border-radius:10px;background:rgba(176,52,42,.055);border:1px solid rgba(176,52,42,.16)}
.winner span{flex:none;font-size:12px;letter-spacing:.16em;color:var(--ink3)}
.winner b{min-width:0;font-family:var(--serif);font-size:22px;font-weight:700;letter-spacing:.04em;
  line-height:1.45;color:var(--ink);overflow-wrap:anywhere;overflow:hidden;
  display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical}
.reveal{display:flex;align-items:center;justify-content:center;gap:34px;
  margin:30px 0 6px;padding:30px 0}
.mark{width:104px;height:104px;flex:none;border-radius:50%;
  border:3px solid rgba(176,52,42,.7);box-shadow:inset 0 0 0 2px rgba(176,52,42,.16);
  display:flex;align-items:center;justify-content:center;transform:rotate(-9deg);
  font-family:var(--serif);font-size:29px;font-weight:700;line-height:1.08;letter-spacing:.08em;
  text-align:center;color:rgba(176,52,42,.82)}

/* —— 排行榜 —— */
.lrow{position:relative;display:grid;grid-template-columns:44px 1fr 78px;gap:16px;align-items:center;
  padding:15px 12px}
.lrow::after{content:"";position:absolute;left:12px;right:12px;bottom:0;
  border-bottom:1px dotted var(--line)}
.lrow:last-child::after{display:none}
.lmid{min-width:0}
.lrk{width:38px;height:38px;border-radius:9px;display:flex;align-items:center;justify-content:center;
  font-size:17px;font-weight:800;font-variant-numeric:tabular-nums;color:var(--red);
  background:rgba(176,52,42,.09);border:1px solid rgba(176,52,42,.22)}
.lrk.top{color:#FBF7EF;background:var(--red);border-color:transparent;
  box-shadow:0 5px 13px rgba(176,52,42,.26)}
.lname{font-family:var(--serif);font-size:22px;font-weight:600;letter-spacing:.04em;color:var(--ink);
  white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.lsc{text-align:right}
.lsc b{font-size:26px;font-weight:800;letter-spacing:-.02em;color:var(--ink);
  font-variant-numeric:tabular-nums}
.lsc span{margin-left:4px;font-family:var(--serif);font-size:14px;color:var(--ink3)}

/* —— 指令 / 规则 —— */
.cmd{position:relative;display:grid;grid-template-columns:230px 1fr;gap:18px;align-items:baseline;
  padding:15px 12px}
.cmd::after{content:"";position:absolute;left:12px;right:12px;bottom:0;
  border-bottom:1px dotted var(--line)}
.cmd:last-child::after{display:none}
.ck{font-family:var(--serif);font-size:20px;font-weight:600;letter-spacing:.06em;color:var(--ink)}
.ck i{font-style:normal;font-family:var(--sans);font-size:13.5px;letter-spacing:.02em;color:var(--red);
  opacity:.85}
.ck u{text-decoration:none;display:block;margin-top:5px;font-family:var(--sans);font-size:12.5px;
  letter-spacing:.04em;color:var(--ink3)}
.cd{font-size:15.5px;line-height:1.6;color:var(--ink2)}
.sec{padding:20px 2px 18px;border-bottom:1px dotted var(--line)}
.sec:last-child{border-bottom:none;padding-bottom:4px}
.sh{display:flex;align-items:center;gap:11px;font-family:var(--serif);font-size:21px;font-weight:700;
  letter-spacing:.1em;color:var(--ink)}
.sn{width:24px;height:24px;flex:none;border-radius:4px;background:var(--red);color:#FBF7EF;
  display:flex;align-items:center;justify-content:center;font-size:13px;font-weight:600}
.sb{margin-top:12px;font-size:16px;line-height:1.85;color:var(--ink2)}
.key{margin-top:16px;display:flex;flex-direction:column;gap:10px}
.ki{display:grid;grid-template-columns:96px 1fr;gap:14px;align-items:center}
.ki b{font-family:var(--serif);font-size:19px;font-weight:600;letter-spacing:.08em;color:var(--red);
  text-align:center;padding:5px 0;border-radius:6px;background:rgba(176,52,42,.07);
  border:1px dashed rgba(176,52,42,.26)}
/* 图例里的「新」要和盘面上那枚印一模一样，否则等于又多解释了一个符号 */
.ki b.solid{color:#FBF7EF;background:var(--red);border:1px solid transparent}
.ki span{font-size:15px;line-height:1.6;color:var(--ink2)}
.tiers{display:flex;gap:9px;margin-top:16px}
.tg{flex:1;padding:11px 4px 10px;border-radius:8px;text-align:center;
  background:rgba(255,255,255,.5);border:1px solid var(--line);
  border-top:2px solid var(--c)}
.tg b{display:block;font-family:var(--serif);font-size:16px;font-weight:700;letter-spacing:.1em;
  color:var(--c)}
.tg span{display:block;margin-top:6px;font-size:11.5px;letter-spacing:.02em;color:var(--ink3);
  font-variant-numeric:tabular-nums;white-space:nowrap}

/* —— 页脚 —— */
.foot{display:flex;align-items:center;justify-content:space-between;gap:18px;
  margin-top:26px;padding-top:16px;border-top:1px solid rgba(120,95,65,.16);
  font-size:12.5px;letter-spacing:.06em;color:#9A8E7E}
.foot .dots{display:flex;align-items:center;gap:6px}
.foot .dots i{width:5px;height:5px;border-radius:50%;background:rgba(176,52,42,.55)}
.foot .dots i:nth-child(2){background:rgba(120,95,65,.28)}
.foot .dots i:nth-child(3){background:rgba(120,95,65,.16)}
"#;

// ================= 组件 =================

/// 卡片外壳：页眉印章、标题区（可带右侧配图）、朱砂起首的分隔线、正文、页脚
fn shell(title: &str, sub: &str, aside: &str, body: &str, foot: &str) -> String {
    let sub = match sub.is_empty() {
        true => String::new(),
        false => format!(r#"<div class="sub">{}</div>"#, esc(sub)),
    };
    format!(
        r#"<!DOCTYPE html><html lang="zh-CN"><head><meta charset="utf-8"><style>{css}</style></head>
<body><div class="shot"><div class="card">
<div class="head">
  <div class="brand"><span class="seal">词</span><span class="bt"><b>词意</b><i>每日一词 · 语义寻踪</i></span></div>
  <div class="stamp">{stamp}</div>
</div>
<div class="lede"><div><div class="title">{title}</div>{sub}</div>{aside}</div>
<div class="rule"></div>
{body}
<div class="foot"><span>{foot}</span><span class="dots"><i></i><i></i><i></i></span></div>
</div></div></body></html>"#,
        css = CSS,
        stamp = stamp(),
        title = esc(title),
        sub = sub,
        aside = aside,
        body = body,
        foot = esc(foot),
    )
}

/// 田字格：`chars` 为空则画成虚线格里的「？」，即尚未揭晓的答案
fn cells(word: &str, size: &str) -> String {
    let mut out = format!(r#"<div class="cells {size}">"#);
    if word.is_empty() {
        out.push_str(r#"<span class="cell blank">？</span><span class="cell blank">？</span>"#);
    } else {
        for ch in word.chars() {
            out.push_str(&format!(r#"<span class="cell">{}</span>"#, esc(&ch.to_string())));
        }
    }
    out.push_str("</div>");
    out
}

/// 一条提示：名次 + 距离档 | 左邻词牌 · 猜过的词 · 右邻词牌 + 接近度条 | 「新」印
fn hint_row(row: &HintRow, pool: usize) -> String {
    let (label, color) = tier(row.rank);
    let width = heat(row.rank, pool) * 100.0;
    format!(
        r#"<div class="row{fresh}" style="--c:{color};--w:{width:.1}%">
<div class="rk"><b><s>#</s>{rank}</b><em>{label}</em></div>
<div><div class="tri"><span class="nb"><i>？</i>{prev}</span><span class="wd">{word}</span><span class="nb">{next}<i>？</i></span></div>
<div class="bar"><span></span></div></div>
<div>{flag}</div></div>"#,
        fresh = if row.fresh { " fresh" } else { "" },
        color = color,
        width = width,
        rank = row.rank,
        label = label,
        prev = esc(&row.prev),
        word = esc(&row.word),
        next = esc(&row.next),
        flag = if row.fresh {
            r#"<div class="flag">新</div>"#
        } else {
            ""
        },
    )
}

fn stat(value: &str, label: &str, wide: bool) -> String {
    format!(
        r#"<div class="stat"><b{cls}>{value}</b><span>{label}</span></div>"#,
        cls = if wide { r#" class="w""# } else { "" },
        value = esc(value),
        label = esc(label),
    )
}

// ================= 四种卡片 =================

/// 盘面卡：一局进行中的全部有效提示，按名次从近到远排
pub fn board_card(board: &Board) -> String {
    let mut body = String::new();

    if let Some(notice) = &board.notice {
        body.push_str(&format!(
            r#"<div class="note"><b>注</b>{}</div>"#,
            esc(notice)
        ));
    }

    if board.rows.is_empty() {
        body.push_str(r#"<div class="empty">还没有命中排名的词，换个方向再试一个</div>"#);
    } else {
        for row in &board.rows {
            body.push_str(&hint_row(row, board.pool));
        }
    }

    body.push_str(&format!(
        r#"<div class="stats">{}{}{}</div>"#,
        stat(&group(board.guesses), "已猜次数", false),
        stat(&group(board.hits), "命中排名", false),
        stat(
            &board
                .rows
                .first()
                .map(|r| format!("#{}", r.rank))
                .unwrap_or_else(|| "—".into()),
            "最近名次",
            false
        ),
    ));

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

    shell("今日词意", &sub, &cells("", "sm"), &body, &foot)
}

/// 揭晓卡：田字格从虚线换成实线，答案落格，右侧盖一枚「猜中」圆印
pub fn win_card(win: &Win) -> String {
    let mut body = format!(
        r#"<div class="reveal">{}<div class="mark">猜<br>中</div></div>"#,
        cells(&win.answer, "lg")
    );

    let mut stats = format!(
        "{}{}",
        stat(&group(win.guesses), "猜测次数", false),
        stat(&group(win.hits), "命中排名", false),
    );
    if let Some(elapsed) = win.elapsed() {
        stats.push_str(&stat(&elapsed, "本局历时", true));
    }
    body.push_str(&format!(r#"<div class="stats">{stats}</div>"#));

    // 猜中者独占一行：昵称长短不受控，塞进四分之一宽的数字卡里
    // 六个字就到头了，通栏才放得下一个正常的群昵称
    body.push_str(&format!(
        r#"<div class="winner"><span>猜中者</span><b>{}</b></div>"#,
        esc(&win.winner)
    ));

    shell(
        "猜对了",
        &format!("今日答案「{}」已揭晓", win.answer),
        "",
        &body,
        "明日零点换新词 · 猜中次数已计入排行",
    )
}

/// 排行榜卡：前三名实心朱砂号牌，其余描边；条长按最高分归一化
pub fn rank_card(rank: &RankBoard) -> String {
    if rank.items.is_empty() {
        let body = r#"<div class="empty">当前还没有人猜对过，第一个名字留给你</div>"#;
        return shell(&rank.title, &rank.subtitle, "", body, "猜中一次即入榜");
    }

    let top = rank.items.iter().map(|i| i.score).max().unwrap_or(1).max(1);
    let mut body = String::new();
    for (idx, item) in rank.items.iter().enumerate() {
        let place = idx + 1;
        // 榜单只有一种量纲（猜中次数），条长按榜首归一化即可，不必再分色
        body.push_str(&format!(
            r#"<div class="lrow" style="--c:var(--red);--w:{width:.1}%">
<div class="lrk{top_cls}">{place}</div>
<div class="lmid"><div class="lname">{name}</div><div class="bar"><span></span></div></div>
<div class="lsc"><b>{score}</b><span>胜</span></div></div>"#,
            width = (item.score as f64 / top as f64) * 100.0,
            top_cls = if place <= 3 { " top" } else { "" },
            place = place,
            name = esc(&item.name),
            score = item.score,
        ));
    }

    let total: i64 = rank.items.iter().map(|i| i.score).sum();
    body.push_str(&format!(
        r#"<div class="stats">{}{}{}</div>"#,
        stat(&group(rank.items.len()), "上榜人数", false),
        stat(&group(total as usize), "合计猜中", false),
        stat(&group(top as usize), "榜首战绩", false),
    ));

    shell(
        &rank.title,
        &rank.subtitle,
        "",
        &body,
        "每日一词 · 每人每日至多一胜",
    )
}

/// 指令卡：左栏指令与别名，右栏一句话说明。
///
/// 指令一律带上本机真实的前缀，图里看到什么就照抄什么，不用再猜要不要加斜杠。
pub fn help_card(prefix: &str) -> String {
    let mut body = String::new();
    for (cmd, extra, desc) in COMMAND_ROWS {
        let extra = match extra.is_empty() {
            true => String::new(),
            false if extra.starts_with('[') => format!(r#" <i>{}</i>"#, esc(extra)),
            false => format!(r#"<u>亦可 {}{}</u>"#, esc(prefix), esc(extra)),
        };
        body.push_str(&format!(
            r#"<div class="cmd"><div class="ck">{}{}{}</div><div class="cd">{}</div></div>"#,
            esc(prefix),
            esc(cmd),
            extra,
            esc(desc),
        ));
    }

    let sub = format!("共 {} 条指令 · 群内发送即可", COMMAND_ROWS.len());
    shell(
        "词意指令",
        &sub,
        "",
        &body,
        &format!("发送「{prefix}词意玩法」查看规则"),
    )
}

/// 规则卡：把玩法讲清楚，示例直接用真实的提示行渲染——
/// 说明与实战看到的是同一套版式，不必二次翻译
pub fn rules_card() -> String {
    let sample = [
        HintRow { rank: 14, prev: "器".into(), word: "镯子".into(), next: "玉".into(), fresh: false },
        HintRow { rank: 15, prev: "子".into(), word: "玉佩".into(), next: "东".into(), fresh: false },
        HintRow { rank: 16, prev: "佩".into(), word: "东西".into(), next: "冥".into(), fresh: true },
    ];
    let rows: String = sample.iter().map(|r| hint_row(r, 3000)).collect();

    // 五档距离的图例：颜色、称呼、名次区间三者对上号，盘面上的色条才读得懂
    let scale: String = [
        (1, "#1 — #10"),
        (11, "#11 — #50"),
        (51, "#51 — #200"),
        (201, "#201 — #1000"),
        (1001, "#1000 之后"),
    ]
    .iter()
    .map(|(rank, range)| {
        let (label, color) = tier(*rank);
        format!(r#"<div class="tg" style="--c:{color}"><b>{label}</b><span>{range}</span></div>"#)
    })
    .collect();

    let body = format!(
        r#"<div class="sec"><div class="sh"><span class="sn">一</span>目标</div>
<div class="sb">猜出系统每天选定的那个两字词语。全群共用一个词，谁先猜中算谁的。</div></div>
<div class="sec"><div class="sh"><span class="sn">二</span>反馈</div>
<div class="sb">每猜一个词，若它落在该词的语义排名里，就会得到一个名次与它的左右邻词。名次越小，离答案越近。</div>
{rows}
<div class="key">
<div class="ki"><b>？佩</b><span>名次更靠前（更近）的那个词，末字是「佩」</span></div>
<div class="ki"><b>冥？</b><span>名次更靠后（更远）的那个词，首字是「冥」</span></div>
<div class="ki"><b>#15</b><span>语义排名，数字越小离答案越近</span></div>
<div class="ki"><b class="solid">新</b><span>本次刚猜出来的那一条，会单独标出</span></div>
</div></div>
<div class="sec"><div class="sh"><span class="sn">三</span>远近</div>
<div class="sb">名次按五档标注，色条越长离答案越近（对数刻度，前排之间的差距也看得出来）。</div>
<div class="tiers">{scale}</div></div>
<div class="sec"><div class="sh"><span class="sn">四</span>周期</div>
<div class="sb">每日一词，猜中后次日零点换新；系统记录每个人的猜中次数，可用「词意榜」「词意全榜」查看。</div></div>"#,
        rows = rows,
        scale = scale,
    );

    shell(
        "词意玩法",
        "猜词 · 看名次 · 顺着邻词收网",
        &cells("", "sm"),
        &body,
        "发送「词意猜测 词语」即可开局",
    )
}

/// 按回应类型选卡片；`Notice` 不出图，返回 None 由调用方走文本
pub fn render(reply: &Reply, prefix: &str) -> Option<String> {
    match reply {
        Reply::Board(board) => Some(board_card(board)),
        Reply::Win(win) => Some(win_card(win)),
        Reply::Rank(rank) => Some(rank_card(rank)),
        Reply::Help => Some(help_card(prefix)),
        Reply::Rules => Some(rules_card()),
        Reply::Notice(_) => None,
    }
}

// ================= 截图 =================

/// 把卡片 HTML 截成图，返回 base64。
///
/// 先按固定宽度定版，量出实际高度后再放大视口重截，长卡片一次出完不被截断。
/// `scale` 是设备像素比：版心宽度不变、出图分辨率翻倍，字形边缘更实；
/// 取值过大只会把图撑肥，限制在 1—4 倍。
pub async fn capture(html: &str, scale: f64) -> Result<String> {
    let scale = if scale.is_finite() {
        scale.clamp(1.0, 4.0)
    } else {
        3.0
    };
    let browser = Browser::instance().await;
    let tab = browser.new_tab().await.map_err(|e| anyhow::anyhow!(e))?;

    let result = async {
        tab.set_viewport(&Viewport::new(WIDTH, 600).with_device_scale_factor(scale))
            .await?;
        tab.set_content(html).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;

        let height = tab
            .evaluate("document.body.scrollHeight")
            .await?
            .as_f64()
            .unwrap_or(900.0) as u32;
        let viewport =
            Viewport::new(WIDTH, (height + 40).clamp(320, 12000)).with_device_scale_factor(scale);
        tab.set_viewport(&viewport).await?;
        tokio::time::sleep(Duration::from_millis(80)).await;

        let opts = CaptureOptions::new().with_viewport(viewport).with_quality(92);
        let b64 = tab
            .find_element(".shot")
            .await?
            .screenshot_with_options(opts)
            .await?;
        Ok::<String, anyhow::Error>(b64)
    }
    .await;

    let _ = tab.close().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::ciyi::view::RankItem;
    use chrono::{Duration as ChronoDuration, Utc};

    fn sample_board(hidden: usize, notice: Option<&str>) -> Board {
        let words = [
            (7, "环", "手镯", "玉"),
            (14, "器", "镯子", "玉"),
            (15, "子", "玉佩", "东"),
            (137, "佩", "东西", "冥"),
            (864, "西", "物件", "行"),
            (2301, "件", "行头", "衣"),
        ];
        Board {
            notice: notice.map(str::to_string),
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
            hidden,
            guesses: 21,
            hits: 6 + hidden,
            pool: 4831,
        }
    }

    /// 把五种卡片的 HTML 落到 `CIYI_CARD_DUMP` 指定的目录，方便肉眼校版：
    ///   CIYI_CARD_DUMP=/tmp/ciyi cargo test ciyi::card::tests::dump -- --ignored
    #[test]
    #[ignore = "仅用于人工核对排版"]
    fn dump_sample_cards() {
        let Ok(dir) = std::env::var("CIYI_CARD_DUMP") else {
            return;
        };
        std::fs::create_dir_all(&dir).unwrap();

        let win = Win {
            answer: "东西".into(),
            winner: "夜航船".into(),
            guesses: 23,
            hits: 9,
            started_at: Utc::now() - ChronoDuration::minutes(194),
        };
        let rank = RankBoard {
            title: "词意榜".into(),
            subtitle: "本群 · 猜中次数前 6 名".into(),
            items: [("夜航船", 12), ("清风", 9), ("南山", 7), ("拾遗", 4), ("白露", 2), ("无名氏", 1)]
                .iter()
                .map(|(name, score)| RankItem {
                    name: (*name).into(),
                    score: *score,
                })
                .collect(),
        };

        // 群昵称长度不受控，长名字单独出一版校对
        let long = "✿ 今天也要元气满满地猜词哦超级无敌长的群昵称 ✿ QwQ";
        let long_win = Win {
            answer: "东西".into(),
            winner: long.into(),
            guesses: 7,
            hits: 3,
            started_at: Utc::now() - ChronoDuration::minutes(41),
        };
        let long_rank = RankBoard {
            title: "词意全榜".into(),
            subtitle: "全部群聊 · 猜中次数前 3 名".into(),
            items: vec![
                RankItem { name: long.into(), score: 12 },
                RankItem { name: "无空格的超长英文名".repeat(4), score: 9 },
                RankItem { name: "短名".into(), score: 1 },
            ],
        };

        for (name, html) in [
            ("board.html", board_card(&sample_board(4, None))),
            ("win_long_name.html", win_card(&long_win)),
            ("rank_long_name.html", rank_card(&long_rank)),
            (
                "board_notice.html",
                board_card(&sample_board(0, Some("「玉佩」已经猜过了"))),
            ),
            ("board_empty.html", board_card(&Board {
                notice: None, rows: vec![], hidden: 0, guesses: 3, hits: 0, pool: 4831,
            })),
            ("win.html", win_card(&win)),
            ("rank.html", rank_card(&rank)),
            ("help.html", help_card("/")),
            ("rules.html", rules_card()),
        ] {
            std::fs::write(format!("{}/{}", dir, name), html).unwrap();
        }
    }

    /// 端到端跑一次截图，确认 HTML 能被无头浏览器吃下并出图：
    ///   CIYI_CARD_DUMP=/tmp/ciyi cargo test ciyi::card::tests::captures -- --ignored
    #[tokio::test]
    #[ignore = "需要可用的无头浏览器"]
    async fn captures_a_card_to_png() {
        let Ok(dir) = std::env::var("CIYI_CARD_DUMP") else {
            return;
        };
        dump_sample_cards();

        use base64::{Engine, engine::general_purpose::STANDARD};
        for name in [
            "board",
            "board_notice",
            "board_empty",
            "win",
            "win_long_name",
            "rank",
            "rank_long_name",
            "help",
            "rules",
        ] {
            let html = std::fs::read_to_string(format!("{}/{}.html", dir, name)).unwrap();
            let b64 = capture(&html, 2.0).await.expect("截图应当成功");
            let bytes = STANDARD.decode(&b64).expect("截图应是合法 base64");
            std::fs::write(format!("{}/{}.png", dir, name), &bytes).unwrap();
            println!("{name} 出图 {} 字节", bytes.len());
        }
    }

    #[test]
    fn escapes_user_supplied_names() {
        let rank = RankBoard {
            title: "词意榜".into(),
            subtitle: String::new(),
            items: vec![RankItem {
                name: r#"<img src=x onerror="alert(1)">"#.into(),
                score: 3,
            }],
        };
        let html = rank_card(&rank);
        assert!(!html.contains("<img src=x"));
        assert!(html.contains("&lt;img src=x"));
    }

    #[test]
    fn heat_falls_off_on_a_log_scale() {
        let (near, mid, far) = (heat(1, 5000), heat(50, 5000), heat(4000, 5000));
        assert!(near > mid && mid > far, "名次越大接近度越低");
        assert!(near > 0.95, "第一名应当接近满格");
        // 线性刻度下 50/5000 会画成 99%，对数刻度要把前排真正拉开
        assert!(mid < 0.6, "名次 50 不该看起来像咬住了答案");
    }

    #[test]
    fn tiers_cover_every_rank() {
        for rank in [1usize, 10, 11, 50, 51, 200, 201, 1000, 1001, 99999] {
            let (label, color) = tier(rank);
            assert!(!label.is_empty() && color.starts_with('#'));
        }
    }

    #[test]
    fn board_card_renders_hints_and_summary() {
        let html = board_card(&sample_board(4, Some("「行头」未进入排名")));
        assert!(html.contains("东西"), "应画出猜过的词");
        assert!(html.contains("咫尺"), "前排应标出距离档");
        assert!(html.contains("「行头」未进入排名"), "提示条应保留");
        assert!(html.contains("另有 4 条更远的记录未列出"));
        assert!(html.contains(r#"class="row fresh""#), "本次猜测应高亮");
    }

    #[test]
    fn win_card_puts_the_answer_in_grid_cells() {
        let win = Win {
            answer: "东西".into(),
            winner: "夜航船".into(),
            guesses: 23,
            hits: 9,
            started_at: Utc::now() - ChronoDuration::minutes(194),
        };
        let html = win_card(&win);
        assert_eq!(html.matches(r#"class="cell""#).count(), 2, "两字两格");
        assert!(html.contains("夜航船"));
        assert!(html.contains("3 小时 14 分"));
    }

    /// 图里的指令要能直接照抄，前缀必须跟着本机配置走
    #[test]
    fn help_card_prints_the_live_prefix() {
        let html = help_card("/");
        assert!(html.contains("/词意猜测"));
        assert!(html.contains("亦可 /词意指令"));
        assert!(!help_card("").contains("亦可 /"), "没配前缀就不该凭空加一个");
    }

    #[test]
    fn rules_card_explains_every_tier() {
        let html = rules_card();
        for label in ["咫尺", "相邻", "相近", "相关", "天涯"] {
            assert!(html.contains(label), "图例缺少「{label}」档");
        }
    }

    /// 长昵称不能把版面撑破。
    ///
    /// 关键在 `min-width:0`：flex / grid 子项默认 `min-width:auto`，
    /// 里面 nowrap 的长名字会成为子项的最小宽度，把整行顶出卡片再被
    /// `.card{overflow:hidden}` 裁掉——省略号根本轮不到生效。
    #[test]
    fn long_names_are_bounded_not_overflowing() {
        let long = "✿ 今天也要元气满满地猜词哦超级无敌长的群昵称 ✿ QwQ".repeat(3);

        let win = win_card(&Win {
            answer: "东西".into(),
            winner: long.clone(),
            guesses: 7,
            hits: 3,
            started_at: Utc::now(),
        });
        assert!(win.contains(&esc(&long)), "名字要完整写进 HTML，由 CSS 决定截断");
        assert!(win.contains(".winner b{min-width:0"), "通栏昵称需要 min-width:0");
        assert!(win.contains("-webkit-line-clamp:2"), "超长昵称最多两行");
        assert!(win.contains("overflow-wrap:anywhere"), "无空格长串也要能断行");

        let rank = rank_card(&RankBoard {
            title: "词意榜".into(),
            subtitle: String::new(),
            items: vec![RankItem { name: long, score: 4 }],
        });
        assert!(rank.contains(".lmid{min-width:0}"), "榜单中间列需要 min-width:0");
        assert!(rank.contains(r#"class="lmid""#), "中间列要真的用上这个类");
        assert!(
            rank.contains("text-overflow:ellipsis"),
            "单行名字超宽时以省略号收尾"
        );
    }

    /// 数字卡也在 flex 里，同样会被长内容顶破
    #[test]
    fn stat_tiles_clamp_their_content() {
        assert!(CSS.contains(".stat{flex:1;min-width:0"));
        assert!(CSS.contains(".stat b.w{"));
    }

    #[test]
    fn groups_thousands() {
        assert_eq!(group(0), "0");
        assert_eq!(group(931), "931");
        assert_eq!(group(4831), "4,831");
        assert_eq!(group(1234567), "1,234,567");
    }
}
