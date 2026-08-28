//! 把帮助排版成一张卡片图（HTML → 截图）。
//!
//! 设计取向：**蓝图 / 说明书**。帮助本质上是一张「装配图」——先给全貌，
//! 再给单件的接线方式，所以整张卡片按图纸的语言来组织：
//!   - 底纹是 26px 细网格 + 130px 主色模数格，像坐标纸；四角是印前十字规线（crop mark）；
//!   - 分区标题带编号与英文代号，右侧一条虚线延伸到计数，扫读时不必数条目；
//!   - 插件名用等宽字体单独成 chip——它是要被原样敲进输入框的字符串，
//!     与中文显示名在字形上就该分开；
//!   - 指令里的 `<参数>` / `[参数]` 一律标成主色，一眼看出哪里得自己填。
//!
//! 状态只用一种编码贯穿全卡：主色 = 已启用，灰 = 已停用。总览顶部那排小格
//! 是「一格一个插件」的实心仪表，不是按比例拉伸的进度条——数格子就能对上清单。
//!
//! 版式参数按「群聊里先看缩略图、再点开看」定：
//!   - 总览版心 880px（双栏），详情 760px（单栏，指令行不折行）；
//!   - 配合 `image_scale`（默认 3 倍）出图，缩略图状态下也能读出插件名。

use super::{Cmd, Entry, Group, needs_prefix};
use anyhow::Result;
use cdp_html_shot::{Browser, CaptureOptions, Viewport};
use chrono::{FixedOffset, Utc};
use std::time::Duration;

/// 总览版心宽度（双栏）
const OVERVIEW_WIDTH: u32 = 880;
/// 详情版心宽度（单栏，指令行要能一行放下）
const DETAIL_WIDTH: u32 = 760;

/// 一张待截图的卡片：HTML 与它的版心宽度绑在一起，免得两处各写一个数字
pub struct Card {
    html: String,
    width: u32,
}

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

/// 出图时刻（北京时间），落在页眉右上角
fn stamp() -> String {
    let beijing = FixedOffset::east_opt(8 * 3600).expect("UTC+8 是合法时区偏移");
    Utc::now()
        .with_timezone(&beijing)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

/// 把 `<参数>` / `[参数]` 标成主色：占位符是要用户自己替换的部分，
/// 和字面量指令混在一起最容易被照抄错。
fn placeholders(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    let mut rest = text;

    while let Some(pos) = rest.find(['<', '[']) {
        out.push_str(&esc(&rest[..pos]));
        let close = if rest.as_bytes()[pos] == b'<' { '>' } else { ']' };
        match rest[pos..].find(close) {
            Some(end) => {
                out.push_str(&format!(
                    r#"<span class="ph">{}</span>"#,
                    esc(&rest[pos..pos + end + 1])
                ));
                rest = &rest[pos + end + 1..];
            }
            // 没有闭合符号，剩下的按普通文本处理，不吞字
            None => {
                out.push_str(&esc(&rest[pos..]));
                return out;
            }
        }
    }
    out.push_str(&esc(rest));
    out
}

/// `"收 / 偷 / 存表情"` → 主指令 + 别名。清单里的别名用 ` / ` 分隔，
/// 图里把第一个抬成主指令，其余降级成小字，避免一行挤三个同义词。
fn split_aliases(cmd: &str) -> (&str, Vec<&str>) {
    let mut parts = cmd.split(" / ").map(str::trim).filter(|s| !s.is_empty());
    let primary = parts.next().unwrap_or(cmd);
    (primary, parts.collect())
}

/// 带前缀的完整指令。符号指令（`/#`、`~名`、`##`、`-#`）本身就是完整写法，不再拼前缀。
fn full_cmd(prefix: &str, cmd: &str) -> String {
    if needs_prefix(cmd) {
        format!("{}{}", prefix, cmd)
    } else {
        cmd.to_string()
    }
}

/// 指令行：前缀单独着色，让「前缀可配置」这件事在图上就说清楚
fn cmd_line(prefix: &str, cmd: &str) -> String {
    if needs_prefix(cmd) {
        format!(
            r#"<span class="pf">{}</span>{}"#,
            esc(prefix),
            placeholders(cmd)
        )
    } else {
        placeholders(cmd)
    }
}

const CSS: &str = r#"
*{margin:0;padding:0;box-sizing:border-box}
.shot{padding:28px;background:#04060A}
.card{position:relative;overflow:hidden;border-radius:22px;padding:46px 46px 30px;
  background:#080B11;border:1px solid rgba(255,255,255,.075);
  /* 坐标纸：细网格打底，主色模数格每 130px 落一次 */
  background-image:
    linear-gradient(rgba(255,255,255,.020) 1px,transparent 1px),
    linear-gradient(90deg,rgba(255,255,255,.020) 1px,transparent 1px),
    linear-gradient(rgba(93,230,201,.045) 1px,transparent 1px),
    linear-gradient(90deg,rgba(93,230,201,.045) 1px,transparent 1px);
  background-size:26px 26px,26px 26px,130px 130px,130px 130px;
  font-family:"PingFang SC","Microsoft YaHei","Noto Sans CJK SC","Source Han Sans SC","WenQuanYi Zen Hei","Helvetica Neue",Arial,sans-serif;
  color:#EDF2F8;-webkit-font-smoothing:antialiased;text-rendering:geometricPrecision}
/* 右上角一团主色微光，给深底一点纵深 */
.card::before{content:"";position:absolute;top:-270px;right:-150px;width:520px;height:520px;
  border-radius:50%;background:rgba(93,230,201,.16);filter:blur(110px);pointer-events:none}
.card>*{position:relative}
/* 印前规线：四角各一个直角，图纸的签名式细节 */
.crop{position:absolute;width:15px;height:15px;border:2px solid rgba(93,230,201,.42);z-index:1}
.crop.tl{top:18px;left:18px;border-right:none;border-bottom:none;border-radius:4px 0 0 0}
.crop.tr{top:18px;right:18px;border-left:none;border-bottom:none;border-radius:0 4px 0 0}
.crop.bl{bottom:18px;left:18px;border-right:none;border-top:none;border-radius:0 0 0 4px}
.crop.br{bottom:18px;right:18px;border-left:none;border-top:none;border-radius:0 0 4px 0}

.mono{font-family:ui-monospace,"JetBrains Mono","SFMono-Regular",Menlo,Consolas,
  "Noto Sans Mono CJK SC","Liberation Mono",monospace}

.eyebrow{display:flex;align-items:center;justify-content:space-between;gap:20px;margin-bottom:22px}
.kicker{display:flex;align-items:center;gap:10px;font-size:14px;font-weight:800;
  letter-spacing:.22em;color:#5DE6C9}
.dot{width:9px;height:9px;border-radius:50%;background:#5DE6C9;
  box-shadow:0 0 0 5px rgba(93,230,201,.14)}
.stamp{display:flex;align-items:center;gap:10px;font-size:13.5px;color:#6C778B;
  letter-spacing:.03em;font-variant-numeric:tabular-nums;white-space:nowrap}

.title{font-size:44px;line-height:1.18;font-weight:800;letter-spacing:-.02em;color:#FFFFFF}
.titlerow{display:flex;align-items:center;flex-wrap:wrap;gap:14px}
.subtitle{margin-top:13px;font-size:17px;line-height:1.6;font-weight:500;color:#93A0B4}
.subtitle b{color:#5DE6C9;font-weight:800;font-variant-numeric:tabular-nums}

/* 一格一个插件的实心仪表：数格子就能和下面的清单对上 */
.cells{display:flex;gap:4px;margin-top:20px}
.cells i{flex:1;height:8px;border-radius:2px;background:rgba(255,255,255,.09)}
.cells i.on{background:#5DE6C9;box-shadow:0 0 12px rgba(93,230,201,.35)}
.rule{height:1px;margin:26px 0 4px;
  background:linear-gradient(90deg,rgba(93,230,201,.55),rgba(255,255,255,.07) 52%,transparent)}

.sec{margin-top:25px}
.sec-h{display:flex;align-items:center;gap:12px;margin-bottom:13px}
.sec-no{font-size:13px;font-weight:700;letter-spacing:.06em;color:#5DE6C9;
  font-variant-numeric:tabular-nums}
.sec-t{font-size:20px;font-weight:800;color:#FFFFFF;letter-spacing:.01em;white-space:nowrap}
.sec-en{font-size:11.5px;font-weight:700;letter-spacing:.2em;color:#59647A;white-space:nowrap}
.sec-line{flex:1;height:1px;
  background:repeating-linear-gradient(90deg,rgba(255,255,255,.16) 0 6px,transparent 6px 13px)}
.sec-c{font-size:12.5px;font-weight:700;color:#77839A;
  font-variant-numeric:tabular-nums;white-space:nowrap}

.grid{display:grid;grid-template-columns:1fr 1fr;gap:10px 18px}
.it{position:relative;overflow:hidden;padding:13px 15px 14px 18px;border-radius:12px;
  background:rgba(255,255,255,.028);border:1px solid rgba(255,255,255,.06)}
.it::before{content:"";position:absolute;left:0;top:13px;bottom:13px;width:3px;
  border-radius:0 3px 3px 0;background:#5DE6C9}
.it.off{background:rgba(255,255,255,.014);border-style:dashed;border-color:rgba(255,255,255,.075)}
.it.off::before{background:rgba(255,255,255,.15)}
.it-h{display:flex;align-items:center;flex-wrap:wrap;gap:9px}
.nm{font-size:18.5px;font-weight:700;color:#FFFFFF;letter-spacing:.01em}
.it.off .nm{color:#98A3B5}
.key{font-size:12.5px;font-weight:600;letter-spacing:.02em;color:#5DE6C9;
  background:rgba(93,230,201,.12);padding:2px 7px;border-radius:6px}
.it.off .key{color:#828EA2;background:rgba(255,255,255,.06)}
.tag{margin-left:auto;font-size:11px;font-weight:700;letter-spacing:.08em;color:#78849A;
  border:1px solid rgba(255,255,255,.14);border-radius:5px;padding:1px 6px}
/* 三行封顶：现有说明最长的一条正好三行，句子不会被截在半途 */
.ds{margin-top:7px;font-size:14px;line-height:1.6;color:#9CA8BB;
  display:-webkit-box;-webkit-line-clamp:3;-webkit-box-orient:vertical;overflow:hidden}
.it.off .ds{color:#798396}

/* —— 详情页 —— */
.pill{display:inline-flex;align-items:center;gap:8px;padding:6px 13px;border-radius:99px;
  font-size:13.5px;font-weight:700;color:#5DE6C9;background:rgba(93,230,201,.12);
  border:1px solid rgba(93,230,201,.3)}
.pill.off{color:#95A0B2;background:rgba(255,255,255,.05);border-color:rgba(255,255,255,.13)}
.pill i{display:block;width:7px;height:7px;border-radius:50%;background:currentColor}
.lead{margin-top:22px;padding:18px 20px;border-radius:12px;
  border:1px solid rgba(255,255,255,.07);border-left:3px solid #5DE6C9;
  background:rgba(255,255,255,.035);font-size:17px;line-height:1.75;color:#C7D2E0}
/* 指令与说明同行：短指令的右侧不留空档，说明放不下时自然落到下一行 */
.cmd{display:grid;grid-template-columns:38px 1fr;gap:14px;padding:14px 0;
  border-bottom:1px solid rgba(255,255,255,.06)}
.cmd:last-of-type{border-bottom:none;padding-bottom:4px}
.cno{padding-top:9px;font-size:14.5px;font-weight:700;text-align:right;
  color:rgba(93,230,201,.7);font-variant-numeric:tabular-nums}
.cbody{display:flex;align-items:baseline;flex-wrap:wrap;gap:9px 14px}
.cl{padding:7px 13px;border-radius:9px;font-size:18.5px;line-height:1.5;
  font-weight:600;color:#FFFFFF;background:rgba(93,230,201,.08);
  border:1px solid rgba(93,230,201,.18);word-break:break-all}
.cl .pf{color:#5DE6C9;font-weight:800}
.cl .ph{color:#5DE6C9}
.note{flex:1 1 240px;font-size:15.5px;line-height:1.7;color:#A7B3C5}
.alias{flex-basis:100%;display:flex;align-items:center;flex-wrap:wrap;gap:8px;
  font-size:13px;color:#76829A}
.alias span{font-size:13.5px;color:#9FADC2;background:rgba(255,255,255,.055);
  border-radius:6px;padding:2px 8px}
.auto{margin-top:24px;padding:26px 22px;border-radius:12px;text-align:center;
  border:1px dashed rgba(255,255,255,.15);background:rgba(255,255,255,.02);
  font-size:16.5px;line-height:1.75;color:#93A0B4}
.auto b{display:block;margin-bottom:6px;font-size:14px;font-weight:800;letter-spacing:.16em;
  color:#5DE6C9}

.foot{display:flex;align-items:center;justify-content:space-between;gap:20px;
  margin-top:32px;padding-top:18px;border-top:1px solid rgba(255,255,255,.08);
  font-size:13px;font-weight:500;color:#6C778B;letter-spacing:.02em}
.legend{display:flex;align-items:center;gap:16px}
.lg{display:flex;align-items:center;gap:7px}
.lg i{display:block;width:8px;height:8px;border-radius:2px;background:#5DE6C9}
.lg.off i{background:rgba(255,255,255,.16)}
.hint{display:flex;align-items:center;gap:9px;color:#9AA6B8;white-space:nowrap}
.hint code{font-size:13px;color:#D6DFEC;background:rgba(255,255,255,.07);
  border:1px solid rgba(255,255,255,.09);border-radius:6px;padding:3px 9px}
"#;

/// 卡片外壳：页眉（主色标签 + 出图时间）、标题区、正文、页脚
fn shell(kicker: &str, head: &str, body: &str, foot: &str) -> String {
    format!(
        r#"<!DOCTYPE html><html lang="zh-CN"><head><meta charset="utf-8"><style>{css}</style></head>
<body><div class="shot"><div class="card">
<span class="crop tl"></span><span class="crop tr"></span>
<span class="crop bl"></span><span class="crop br"></span>
<div class="eyebrow"><div class="kicker"><span class="dot"></span>{kicker}</div>
<div class="stamp mono">{stamp}</div></div>
{head}
{body}
<div class="foot">{foot}</div>
</div></div></body></html>"#,
        css = CSS,
        kicker = esc(kicker),
        stamp = stamp(),
        head = head,
        body = body,
        foot = foot,
    )
}

/// 页脚右侧的用法提示
fn hint(label: &str, code: &str) -> String {
    format!(
        r#"<div class="hint">{}<code class="mono">{}</code></div>"#,
        esc(label),
        esc(code)
    )
}

/// 总览卡：分区 → 双栏清单。主色格 = 已启用，虚线灰格 = 已停用。
pub fn overview(groups: &[Group], prefix: &str) -> Card {
    let total: usize = groups.iter().map(|g| g.items.len()).sum();
    let enabled = groups
        .iter()
        .flat_map(|g| g.items.iter())
        .filter(|e| e.enabled)
        .count();

    // 仪表：一格一个插件，顺序与下方清单一致
    let cells: String = groups
        .iter()
        .flat_map(|g| g.items.iter())
        .map(|e| if e.enabled { r#"<i class="on"></i>"# } else { "<i></i>" })
        .collect();

    let head = format!(
        r#"<div class="title">插件总览</div>
<div class="subtitle">共 <b>{total}</b> 个插件 · 已启用 <b>{enabled}</b> 个 · 指令前缀 <b>{prefix}</b></div>
<div class="cells">{cells}</div>
<div class="rule"></div>"#,
        total = total,
        enabled = enabled,
        prefix = esc(prefix),
        cells = cells,
    );

    let mut body = String::new();
    for (idx, group) in groups.iter().enumerate() {
        body.push_str(&format!(
            r#"<div class="sec"><div class="sec-h">
<span class="sec-no mono">{no:02}</span><span class="sec-t">{title}</span>
<span class="sec-en mono">{en}</span><span class="sec-line"></span>
<span class="sec-c mono">{count} 项</span></div><div class="grid">"#,
            no = idx + 1,
            title = esc(group.title),
            en = esc(group.en),
            count = group.items.len(),
        ));

        for item in &group.items {
            let (cls, tag) = if item.enabled {
                ("it", "")
            } else {
                ("it off", r#"<span class="tag">停用</span>"#)
            };
            body.push_str(&format!(
                r#"<div class="{cls}"><div class="it-h"><span class="nm">{name}</span>
<span class="key mono">{key}</span>{tag}</div><div class="ds">{desc}</div></div>"#,
                cls = cls,
                name = esc(item.display),
                key = esc(item.name),
                tag = tag,
                desc = esc(item.desc),
            ));
        }
        body.push_str("</div></div>");
    }

    let foot = format!(
        r#"<div class="legend"><div class="lg"><i></i>已启用</div>
<div class="lg off"><i></i>已停用</div></div>{}"#,
        hint("查看某个插件的全部指令", &format!("{}help <插件名>", prefix))
    );

    Card {
        html: shell("AYJX · MANUAL", &head, &body, &foot),
        width: OVERVIEW_WIDTH,
    }
}

/// 详情卡：一条指令一格，主指令抬到等宽 chip 里，别名与说明依次下沉
pub fn detail(entry: &Entry, cmds: &[Cmd], prefix: &str) -> Card {
    let (pill_cls, pill_text) = if entry.enabled {
        ("pill", "已启用")
    } else {
        ("pill off", "已停用")
    };

    let head = format!(
        r#"<div class="titlerow"><div class="title">{name}</div>
<span class="{pill_cls}"><i></i>{pill_text}</span></div>
<div class="subtitle">配置键 <b class="mono">{key}</b></div>
<div class="lead">{desc}</div>"#,
        name = esc(entry.display),
        pill_cls = pill_cls,
        pill_text = pill_text,
        key = esc(entry.name),
        desc = esc(entry.desc),
    );

    let mut body = String::new();
    if cmds.is_empty() {
        body.push_str(
            r#"<div class="auto"><b>BACKGROUND</b>该插件在后台自动工作，没有需要手动触发的指令。</div>"#,
        );
    } else {
        body.push_str(&format!(
            r#"<div class="sec"><div class="sec-h"><span class="sec-t">指令</span>
<span class="sec-en mono">COMMANDS</span><span class="sec-line"></span>
<span class="sec-c mono">{} 条</span></div></div>"#,
            cmds.len()
        ));

        for (idx, cmd) in cmds.iter().enumerate() {
            let (primary, aliases) = split_aliases(cmd.cmd);
            body.push_str(&format!(
                r#"<div class="cmd"><div class="cno mono">{no:02}</div><div class="cbody">
<div class="cl mono">{line}</div>"#,
                no = idx + 1,
                line = cmd_line(prefix, primary),
            ));

            if !cmd.note.is_empty() {
                body.push_str(&format!(r#"<div class="note">{}</div>"#, esc(cmd.note)));
            }
            if !aliases.is_empty() {
                let chips: String = aliases
                    .iter()
                    .map(|a| format!(r#"<span class="mono">{}</span>"#, esc(&full_cmd(prefix, a))))
                    .collect();
                body.push_str(&format!(r#"<div class="alias">别名{}</div>"#, chips));
            }
            body.push_str("</div></div>");
        }
    }

    let foot = format!(
        r#"<div class="legend"><div class="lg"><i></i>AYJX · 插件手册</div></div>{}"#,
        hint("回到插件总览", &format!("{}help", prefix))
    );

    Card {
        html: shell("MANUAL · PLUGIN", &head, &body, &foot),
        width: DETAIL_WIDTH,
    }
}

impl Card {
    /// 把卡片截成图，返回 base64。
    ///
    /// 先按版心宽度定版，量出实际高度后再放大视口重截，长卡片一次出完不被截断。
    /// `scale` 是设备像素比：版心不变、分辨率翻倍，字形边缘更实，限制在 1—4 倍。
    pub async fn capture(&self, scale: f64) -> Result<String> {
        let scale = if scale.is_finite() {
            scale.clamp(1.0, 4.0)
        } else {
            3.0
        };
        let browser = Browser::instance().await;
        let tab = browser.new_tab().await.map_err(|e| anyhow::anyhow!(e))?;

        let result = async {
            tab.set_viewport(&Viewport::new(self.width, 600).with_device_scale_factor(scale))
                .await?;
            tab.set_content(&self.html).await?;
            tokio::time::sleep(Duration::from_millis(150)).await;

            let height = tab
                .evaluate("document.body.scrollHeight")
                .await?
                .as_f64()
                .unwrap_or(1400.0) as u32;
            let viewport = Viewport::new(self.width, (height + 40).clamp(400, 16000))
                .with_device_scale_factor(scale);
            tab.set_viewport(&viewport).await?;
            tokio::time::sleep(Duration::from_millis(80)).await;

            let opts = CaptureOptions::new().with_viewport(viewport).with_quality(92);
            let b64 = tab.find_element(".shot").await?.screenshot_with_options(opts).await?;
            Ok::<String, anyhow::Error>(b64)
        }
        .await;

        let _ = tab.close().await;
        result
    }

    #[cfg(test)]
    pub fn html(&self) -> &str {
        &self.html
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_groups() -> Vec<Group> {
        vec![Group {
            title: "消息 · 媒体",
            en: "MESSAGE",
            items: vec![
                Entry {
                    display: "媒体转换",
                    name: "media",
                    desc: "图片/视频 ↔ 直链",
                    enabled: true,
                },
                Entry {
                    display: "网页截图",
                    name: "webshot",
                    desc: "自动对链接截图",
                    enabled: false,
                },
            ],
        }]
    }

    #[test]
    fn marks_placeholders_and_keeps_unclosed_brackets() {
        assert_eq!(
            placeholders("裁剪 <行>x<列>"),
            r#"裁剪 <span class="ph">&lt;行&gt;</span>x<span class="ph">&lt;列&gt;</span>"#
        );
        assert_eq!(
            placeholders("词意猜测 [词语]"),
            r#"词意猜测 <span class="ph">[词语]</span>"#
        );
        // 缺少闭合符号时原样输出，不吞掉后面的字
        assert_eq!(placeholders("a <b"), "a &lt;b");
    }

    #[test]
    fn escapes_html_in_content() {
        let out = placeholders(r#"<script>alert("x")"#);
        assert!(!out.contains("<script"));
        assert!(out.contains("&lt;script&gt;"));
    }

    #[test]
    fn splits_aliases_on_slash() {
        let (primary, aliases) = split_aliases("收 / 偷 / 存表情");
        assert_eq!(primary, "收");
        assert_eq!(aliases, vec!["偷", "存表情"]);

        let (primary, aliases) = split_aliases("ping");
        assert_eq!(primary, "ping");
        assert!(aliases.is_empty());
    }

    #[test]
    fn prefixes_word_commands_but_not_symbol_ones() {
        assert_eq!(full_cmd("/", "help"), "/help");
        assert_eq!(full_cmd("/", "~<名称> <内容>"), "~<名称> <内容>");
        assert!(cmd_line("/", "help").contains(r#"<span class="pf">/</span>"#));
        assert!(!cmd_line("/", "/#").contains(r#"class="pf""#));
    }

    #[test]
    fn overview_counts_enabled_cells() {
        let html = overview(&sample_groups(), "/").html().to_string();
        assert!(html.contains("共 <b>2</b> 个插件 · 已启用 <b>1</b> 个"));
        assert_eq!(html.matches(r#"<i class="on"></i>"#).count(), 1);
        assert!(html.contains("媒体转换"));
        // 停用的插件仍然在图上，只是降级展示
        assert!(html.contains(r#"class="it off""#));
        assert!(html.contains("网页截图"));
    }

    #[test]
    fn detail_renders_commands_aliases_and_notes() {
        let entry = Entry {
            display: "表情收藏",
            name: "sticker",
            desc: "保存对方发送的表情",
            enabled: true,
        };
        let cmds = [Cmd {
            cmd: "收 / 偷 / 存表情",
            note: "引用表情后收藏",
        }];
        let html = detail(&entry, &cmds, "/").html().to_string();

        assert!(html.contains("表情收藏"));
        assert!(html.contains("已启用"));
        assert!(html.contains(r#"<span class="pf">/</span>收"#));
        assert!(html.contains("/偷"));
        assert!(html.contains("引用表情后收藏"));
        assert!(html.contains("1 条"));
    }

    #[test]
    fn detail_says_so_when_a_plugin_has_no_commands() {
        let entry = Entry {
            display: "复读机",
            name: "repeater",
            desc: "达到阈值后自动跟读",
            enabled: false,
        };
        let html = detail(&entry, &[], "/").html().to_string();
        assert!(html.contains("没有需要手动触发的指令"));
        assert!(html.contains(r#"class="pill off""#));
    }

    /// 把两张卡片的 HTML 落到 `HELP_CARD_DUMP` 指定的目录，方便肉眼校版：
    ///   HELP_CARD_DUMP=/tmp/help cargo test help::card::tests::dump -- --ignored
    #[test]
    #[ignore = "仅用于人工核对排版"]
    fn dump_sample_cards() {
        let Ok(dir) = std::env::var("HELP_CARD_DUMP") else {
            return;
        };
        std::fs::create_dir_all(&dir).unwrap();

        // 显示名取自真实注册表，中文字宽与线上一致，校版才有意义
        let display_of = |name: &str| -> &'static str {
            crate::plugins::get_plugins()
                .iter()
                .find(|p| p.name == name)
                .map(|p| p.display_name)
                .unwrap_or("未注册")
        };

        let groups: Vec<Group> = super::super::SECTIONS
            .iter()
            .enumerate()
            .map(|(gi, sec)| Group {
                title: sec.title,
                en: sec.en,
                items: sec
                    .members
                    .iter()
                    .copied()
                    .enumerate()
                    .map(|(i, name)| Entry {
                        display: display_of(name),
                        name,
                        desc: super::super::describe(name).0,
                        // 随手停用几个，好看清两种状态的对比
                        enabled: (gi + i) % 5 != 0,
                    })
                    .collect(),
            })
            .collect();

        let (desc, cmds) = super::super::describe("shindan");
        let entry = Entry {
            display: display_of("shindan"),
            name: "shindan",
            desc,
            enabled: true,
        };

        for (file, card) in [
            ("overview.html", overview(&groups, "/")),
            ("detail.html", detail(&entry, cmds, "/")),
        ] {
            std::fs::write(format!("{}/{}", dir, file), card.html()).unwrap();
        }
    }

    /// 端到端跑一次截图，确认 HTML 能被无头浏览器吃下并出图：
    ///   HELP_CARD_DUMP=/tmp/help cargo test help::card::tests::captures -- --ignored
    #[tokio::test]
    #[ignore = "需要可用的无头浏览器"]
    async fn captures_a_card_to_png() {
        let Ok(dir) = std::env::var("HELP_CARD_DUMP") else {
            return;
        };
        std::fs::create_dir_all(&dir).unwrap();

        let b64 = overview(&sample_groups(), "/")
            .capture(2.0)
            .await
            .expect("截图应当成功");
        assert!(!b64.is_empty());

        use base64::{Engine, engine::general_purpose::STANDARD};
        let bytes = STANDARD.decode(&b64).expect("截图应是合法 base64");
        std::fs::write(format!("{}/captured.png", dir), &bytes).unwrap();
        println!("出图 {} 字节", bytes.len());
    }
}
