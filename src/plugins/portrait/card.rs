//! 画像报告卡（HTML → 截图）。
//!
//! 版式借了三处现成的做法：年度报告那种「先给标签再给数字」的叙事顺序、
//! 编辑型排版的字号与行高体系、以及本仓库资讯卡片已经调好的明暗双主题骨架。
//! 目标是「一屏读得下去」：正文 19.5px、行高 1.8、版心 720px，中文一行约 30 字，
//! 手机上缩略图能读出标题，点开长文不累。
//!
//! 约束与本仓库其它卡片一致：不加载任何外部资源（字体、图片、脚本都不引），
//! 所有动态文本一律转义，出图走 `TabGuard` 并在 45 秒处兜底。

use super::collect::Material;
use super::persona::Persona;
use crate::render::web::TabGuard;
use anyhow::Result;
use cdp_html_shot::{Browser, CaptureOptions, Viewport};
use chrono::{DateTime, FixedOffset, Timelike, Utc};
use std::time::Duration;

/// 卡片渲染宽度（CSS 像素）。出图宽度 = `WIDTH × scale`。
const WIDTH: u32 = 720;
/// 截图上限，与其它卡片保持一致。
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(45);

/// 明暗两套主题。切换只动明度与文字三档灰，不动版式，切换后仍像同一份东西。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Light,
    Dark,
}

impl Theme {
    /// `auto` 在北京时间 07:00—18:59 用日读，其余时间用夜读。
    pub fn resolve(mode: &str, now: DateTime<FixedOffset>) -> Self {
        match mode.trim().to_ascii_lowercase().as_str() {
            "light" | "day" | "白天" | "日间" => Theme::Light,
            "dark" | "night" | "夜晚" | "夜间" => Theme::Dark,
            _ if (7..19).contains(&now.hour()) => Theme::Light,
            _ => Theme::Dark,
        }
    }

    fn vars(self) -> &'static str {
        match self {
            Theme::Light => {
                r#"color-scheme:light;
  --canvas:#EDF0F4;--surface:#FFFFFF;
  --title:#141922;--strong:#242B35;--body:#3A414C;
  --subtle:#57606E;--muted:#69717E;--faint:#858D99;
  --line:rgba(20,25,34,.09);--strong-line:rgba(20,25,34,.13);
  --panel:rgba(20,25,34,.035);--panel-border:rgba(20,25,34,.075);
  --chip:rgba(20,25,34,.05);--track:rgba(20,25,34,.075);
  --pattern:rgba(20,25,34,.03);--shadow:0 18px 44px rgba(15,23,42,.10);
  --glow-alpha:.13;--chip-alpha:.10;--quote-alpha:.05;--bar-alpha:.16"#
            }
            Theme::Dark => {
                r#"color-scheme:dark;
  --canvas:#0E1218;--surface:#181D26;
  --title:#F3F1EB;--strong:#E8E6E0;--body:#D4D3CE;
  --subtle:#ADB3BD;--muted:#9DA4AF;--faint:#878F9B;
  --line:rgba(232,234,238,.09);--strong-line:rgba(232,234,238,.13);
  --panel:rgba(240,236,226,.05);--panel-border:rgba(232,234,238,.10);
  --chip:rgba(232,234,238,.07);--track:rgba(232,234,238,.10);
  --pattern:rgba(232,234,238,.026);--shadow:0 18px 48px rgba(0,0,0,.30);
  --glow-alpha:.11;--chip-alpha:.13;--quote-alpha:.06;--bar-alpha:.20"#
            }
        }
    }
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

/// 四位数以上的计数加千分位，扫一眼就知道量级。
fn fmt_num(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn stamp(now: DateTime<FixedOffset>) -> String {
    now.format("%Y-%m-%d %H:%M").to_string()
}

fn date_of(timestamp: i64, offset: FixedOffset) -> String {
    DateTime::from_timestamp(timestamp, 0)
        .map(|utc| utc.with_timezone(&offset).format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// 头像用的那个字：取名字里第一个汉字或字母数字，挑不出就用「群」。
fn initial(name: &str) -> String {
    name.chars()
        .find(|ch| ch.is_alphanumeric())
        .or_else(|| name.chars().find(|ch| !ch.is_whitespace()))
        .map(|ch| ch.to_string())
        .unwrap_or_else(|| "群".to_string())
}

/// 一份画像报告要用到的全部信息。
pub struct View<'a> {
    pub material: &'a Material,
    pub persona: &'a Persona,
    pub model: &'a str,
    /// 主题模式：`auto` / `light` / `dark`，见 [`Theme::resolve`]。
    pub theme: &'a str,
    pub offset: FixedOffset,
    pub now: DateTime<FixedOffset>,
}

/// 渲染成整页 HTML（截图用）。
pub fn html(view: &View<'_>) -> String {
    let theme = Theme::resolve(view.theme, view.now);
    let accent = view.persona.accent(view.material.user_id);
    let dark = theme == Theme::Dark;
    let css = format!(":root{{{}}}\n{CSS}", theme.vars())
        .replace("__ACCENT__", accent.hex(dark))
        .replace("__RGB__", accent.rgb(dark));

    let material = view.material;
    let persona = view.persona;

    let codename_pill = if persona.estimated {
        r#"<span class="pill">纯统计版</span>"#.to_string()
    } else {
        String::new()
    };
    let codename_block = format!(
        r#"<div class="codename-block"><div class="label-row"><span class="label">画像代号</span>{codename_pill}</div><div class="codename">{}</div><div class="tagline">{}</div></div>"#,
        esc(if persona.codename.is_empty() {
            "尚未命名"
        } else {
            &persona.codename
        }),
        esc(&persona.tagline),
    );

    format!(
        r#"<!DOCTYPE html><html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><style>{css}</style></head>
<body><div class="shot"><div class="card">
{eyebrow}
{hero}
{codename_block}
{lead}
{tiles}
{traits}
{interests}
{rhythm}
{groups}
{quotes}
{prose}
{advice}
{foot}
</div></div></body></html>"#,
        css = css,
        eyebrow = eyebrow(view),
        hero = hero(view),
        lead = lead(&persona.summary),
        tiles = tiles(view),
        traits = traits(&persona.traits),
        interests = interests(&persona.interests),
        rhythm = rhythm(view),
        groups = groups(material),
        quotes = quotes(&persona.quotes),
        prose = prose(persona),
        advice = advice(&persona.advice),
        foot = foot(view),
    )
}

fn eyebrow(view: &View<'_>) -> String {
    format!(
        r#"<div class="eyebrow"><div class="kicker"><span class="dot"></span>用户画像<span class="kicker-en">PORTRAIT</span></div><div class="stamp">{}</div></div>"#,
        esc(&stamp(view.now)),
    )
}

fn hero(view: &View<'_>) -> String {
    let material = view.material;
    let bits = [
        format!("QQ {}", material.user_id),
        format!("{} 个群", material.groups.len().max(1)),
        format!("{} 起", date_of(material.first_time, view.offset)),
        format!("{} 条发言", fmt_num(material.total)),
    ];
    format!(
        r#"<div class="hero"><div class="avatar">{}</div><div class="who"><div class="who-name">{}</div><div class="who-meta">{}</div></div></div>"#,
        esc(&initial(&material.name)),
        esc(&material.name),
        bits.join(r#"<span class="sep">·</span>"#),
    )
}

fn lead(summary: &str) -> String {
    if summary.trim().is_empty() {
        return String::new();
    }
    format!(r#"<div class="lead">{}</div>"#, esc(summary))
}

fn tiles(view: &View<'_>) -> String {
    let material = view.material;
    let items = [
        (
            fmt_num(material.total),
            "发言".to_string(),
            format!("{} 天里", material.span_days()),
        ),
        (
            fmt_num(material.active_days),
            "活跃天数".to_string(),
            format!("平均每天 {:.1} 条", material.per_day()),
        ),
        (
            format!("{} 点", material.peak_hour()),
            "最活跃".to_string(),
            format!("{} 条", material.hour[material.peak_hour()]),
        ),
        (
            format!("{:.1} 字", material.avg_len()),
            "平均每条".to_string(),
            format!("最长 {} 字", material.longest),
        ),
    ];
    let cells: String = items
        .into_iter()
        .map(|(value, label, note)| {
            format!(
                r#"<div class="tile"><div class="tile-value">{}</div><div class="tile-label">{}</div><div class="tile-note">{}</div></div>"#,
                esc(&value),
                esc(&label),
                esc(&note)
            )
        })
        .collect();
    format!(r#"<div class="tiles">{cells}</div>"#)
}

fn traits(traits: &[super::persona::Trait]) -> String {
    if traits.is_empty() {
        return String::new();
    }
    let rows: String = traits
        .iter()
        .map(|item| {
            format!(
                r#"<div class="trait"><div class="trait-top"><span class="trait-name">{}</span><span class="trait-score">{:.0}</span></div><div class="track"><i style="width:{:.1}%"></i></div><div class="trait-note">{}</div></div>"#,
                esc(&item.name),
                item.score,
                item.score.clamp(2.0, 100.0),
                esc(&item.note)
            )
        })
        .collect();
    format!(
        r#"<div class="sec"><div class="sec-head"><span class="bar-mark"></span>性格特质</div>{rows}</div>"#
    )
}

fn interests(words: &[String]) -> String {
    if words.is_empty() {
        return String::new();
    }
    let chips: String = words
        .iter()
        .map(|word| format!(r#"<span class="chip">{}</span>"#, esc(word)))
        .collect();
    format!(
        r#"<div class="sec"><div class="sec-head"><span class="bar-mark"></span>话题兴趣</div><div class="chips">{chips}</div></div>"#
    )
}

fn rhythm(view: &View<'_>) -> String {
    let material = view.material;
    let peak = material.peak_hour();
    let max = material.hour.iter().copied().max().unwrap_or(0).max(1);
    let columns: String = material
        .hour
        .iter()
        .enumerate()
        .map(|(hour, count)| {
            let ratio = *count as f64 / max as f64;
            let height = (ratio * 100.0).max(if *count > 0 { 4.0 } else { 0.0 });
            let peak_class = if hour == peak { " peak" } else { "" };
            let label = if hour % 6 == 0 {
                format!("{hour}")
            } else {
                String::new()
            };
            format!(
                r#"<div class="col"><div class="col-bar"><i class="{}{}" style="height:{:.1}%"></i></div><div class="col-tick{}">{}</div></div>"#,
                if *count > 0 { "on" } else { "off" },
                peak_class,
                height,
                peak_class,
                esc(&label),
            )
        })
        .collect();
    format!(
        r#"<div class="sec"><div class="sec-head"><span class="bar-mark"></span>活跃节律</div><div class="rhythm">{columns}</div><div class="caption">{} 前后最活跃，最活跃的一天是{}，夜间（0—6 点）占 {}。</div></div>"#,
        esc(&super::persona::hour_label(peak)),
        esc(super::persona::weekday_label(material.peak_weekday())),
        esc(&super::persona::percent(material.night_ratio())),
    )
}

fn groups(material: &Material) -> String {
    if material.groups.is_empty() {
        return String::new();
    }
    let max = material.groups.iter().map(|g| g.count).max().unwrap_or(1).max(1);
    let rows: String = material
        .groups
        .iter()
        .take(4)
        .map(|group| {
            let share = group.count as f64 / max as f64 * 100.0;
            format!(
                r#"<div class="grow"><div class="grow-top"><span class="grow-name">{}</span><span class="grow-count">{} 条</span></div><div class="track slim"><i style="width:{:.1}%"></i></div></div>"#,
                esc(&group.name),
                fmt_num(group.count),
                share.clamp(3.0, 100.0),
            )
        })
        .collect();
    format!(
        r#"<div class="sec"><div class="sec-head"><span class="bar-mark"></span>常在的群</div>{rows}</div>"#
    )
}

fn quotes(quotes: &[super::persona::Quote]) -> String {
    if quotes.is_empty() {
        return String::new();
    }
    let items: String = quotes
        .iter()
        .map(|quote| {
            format!(
                r#"<figure class="quote"><div class="quote-text">{}</div><figcaption class="quote-why">{}</figcaption></figure>"#,
                esc(&quote.text),
                esc(&quote.why),
            )
        })
        .collect();
    format!(
        r#"<div class="sec"><div class="sec-head"><span class="bar-mark"></span>发言样本</div>{items}</div>"#
    )
}

fn prose(persona: &Persona) -> String {
    let mut blocks = String::new();
    for (label, text) in [("群里的位置", &persona.role), ("说话风格", &persona.style)] {
        if text.trim().is_empty() {
            continue;
        }
        blocks.push_str(&format!(
            r#"<div class="prose-row"><div class="prose-label">{}</div><div class="prose-text">{}</div></div>"#,
            esc(label),
            esc(text)
        ));
    }
    if blocks.is_empty() {
        return String::new();
    }
    format!(r#"<div class="sec prose">{blocks}</div>"#)
}

fn advice(advice: &str) -> String {
    if advice.trim().is_empty() {
        return String::new();
    }
    format!(
        r#"<div class="advice"><div class="advice-label">写在最后</div><div class="advice-text">{}</div></div>"#,
        esc(advice)
    )
}

fn foot(view: &View<'_>) -> String {
    let material = view.material;
    let range = format!(
        "{} — {}",
        date_of(material.first_time, view.offset),
        date_of(material.last_time, view.offset)
    );
    format!(
        r#"<div class="foot"><div>统计区间 {range}<span class="sep">·</span>样本 {} 条<span class="sep">·</span>{model}</div><div class="foot-note">用模型读群聊记录推出来的，仅供娱乐</div></div>"#,
        material.samples.len(),
        range = esc(&range),
        model = esc(view.model),
    )
}

/// 出图。`scale` 是设备像素比，限制在 1—4 倍，与其它卡片一致。
pub async fn capture(html: &str, scale: f64) -> Result<String> {
    let scale = if scale.is_finite() {
        scale.clamp(1.0, 4.0)
    } else {
        3.0
    };
    let browser = Browser::instance().await;
    let guard = TabGuard::new(browser.new_tab().await.map_err(|e| anyhow::anyhow!(e))?);

    let result = tokio::time::timeout(CAPTURE_TIMEOUT, async {
        let tab = guard.tab();
        tab.set_viewport(&Viewport::new(WIDTH, 800).with_device_scale_factor(scale))
            .await?;
        tab.set_content(html).await?;
        // 字体就绪之前量高度会算出偏小的值，底部会被切掉；给一帧让布局落地。
        tokio::time::sleep(Duration::from_millis(200)).await;

        let height = tab
            .evaluate("document.body.scrollHeight")
            .await?
            .as_f64()
            .unwrap_or(1200.0) as u32;
        let viewport =
            Viewport::new(WIDTH, (height + 40).clamp(400, 14_000)).with_device_scale_factor(scale);
        tab.set_viewport(&viewport).await?;
        tokio::time::sleep(Duration::from_millis(120)).await;

        let options = CaptureOptions::new().with_viewport(viewport).with_quality(90);
        let shot = tab
            .find_element(".shot")
            .await?
            .screenshot_with_options(options)
            .await?;
        Ok::<String, anyhow::Error>(shot)
    })
    .await
    .map_err(|_| anyhow::anyhow!("画像卡片截图超时（{} 秒）", CAPTURE_TIMEOUT.as_secs()))?;

    guard.close().await;
    result
}

/// 页面主色取当前的北京时刻，与 `html()` 里的主题判定保持同一条规则。
pub fn now(offset: FixedOffset) -> DateTime<FixedOffset> {
    Utc::now().with_timezone(&offset)
}

const CSS: &str = r#"
*{margin:0;padding:0;box-sizing:border-box}
.shot{padding:22px;background:var(--canvas)}
.card{position:relative;overflow:hidden;border-radius:24px;padding:40px 40px 30px;
  background:var(--surface);border:1px solid var(--strong-line);box-shadow:var(--shadow);
  background-image:radial-gradient(var(--pattern) 1px,transparent 1px);
  background-size:26px 26px;
  font-family:"PingFang SC","Microsoft YaHei","Noto Sans CJK SC","Source Han Sans SC","WenQuanYi Zen Hei","Helvetica Neue",Arial,sans-serif;
  color:var(--body);-webkit-font-smoothing:antialiased;text-rendering:geometricPrecision;
  overflow-wrap:anywhere;word-break:normal}
.card::before{content:"";position:absolute;top:-240px;left:-140px;width:520px;height:520px;
  border-radius:50%;background:rgba(__RGB__,var(--glow-alpha));filter:blur(100px);pointer-events:none}
.card>*{position:relative}

/* —— 页眉 —— */
.eyebrow{display:flex;align-items:center;justify-content:space-between;margin-bottom:22px}
.kicker{display:flex;align-items:center;gap:11px;font-size:16px;font-weight:800;
  letter-spacing:.16em;color:__ACCENT__}
.dot{width:9px;height:9px;border-radius:50%;background:__ACCENT__;
  box-shadow:0 0 0 5px rgba(__RGB__,var(--glow-alpha))}
.kicker-en{font-size:12px;font-weight:700;letter-spacing:.24em;color:var(--faint)}
.stamp{font-size:14.5px;color:var(--faint);letter-spacing:.03em;font-variant-numeric:tabular-nums}

/* —— 主体：头像 + 名字 —— */
.hero{display:flex;align-items:center;gap:20px}
.avatar{flex:none;display:flex;align-items:center;justify-content:center;width:86px;height:86px;
  border-radius:50%;font-size:36px;font-weight:800;color:__ACCENT__;
  background:rgba(__RGB__,var(--chip-alpha));border:2px solid rgba(__RGB__,.30);letter-spacing:0}
.who{min-width:0}
.who-name{font-size:34px;line-height:1.28;font-weight:800;letter-spacing:-.015em;color:var(--title)}
.who-meta{margin-top:9px;font-size:15.5px;line-height:1.6;font-weight:500;color:var(--muted)}
.sep{margin:0 8px;color:var(--faint)}

/* —— 代号 —— */
.codename-block{margin-top:30px}
.label-row{display:flex;align-items:center;gap:12px}
.label{font-size:13.5px;font-weight:800;letter-spacing:.2em;color:var(--faint)}
.pill{padding:3px 10px;border-radius:8px;font-size:13px;font-weight:700;letter-spacing:.02em;
  color:__ACCENT__;background:rgba(__RGB__,var(--chip-alpha))}
.codename{margin-top:10px;font-size:46px;line-height:1.24;font-weight:800;
  letter-spacing:-.02em;color:var(--title)}
.tagline{margin-top:10px;font-size:21px;line-height:1.62;font-weight:600;color:__ACCENT__}

/* —— 总评 —— */
.lead{margin-top:24px;padding:21px 23px;border-radius:16px;
  background:var(--panel);border:1px solid var(--panel-border);
  font-size:20.5px;line-height:1.82;font-weight:500;color:var(--strong)}

/* —— 数字条 —— */
.tiles{display:grid;grid-template-columns:repeat(4,1fr);gap:12px;margin-top:26px}
.tile{padding:17px 15px;border-radius:14px;background:var(--panel);
  border:1px solid var(--panel-border)}
.tile-value{font-size:31px;line-height:1.14;font-weight:800;letter-spacing:-.02em;
  color:__ACCENT__;font-variant-numeric:tabular-nums}
.tile-label{margin-top:7px;font-size:15.5px;font-weight:700;color:var(--strong)}
.tile-note{margin-top:4px;font-size:13.5px;line-height:1.45;color:var(--faint)}

/* —— 分节 —— */
.sec{margin-top:32px;padding-top:26px;border-top:1px solid var(--line)}
.sec-head{display:flex;align-items:center;gap:11px;margin-bottom:18px;
  font-size:21px;font-weight:800;color:var(--title)}
.bar-mark{width:5px;height:20px;border-radius:3px;background:__ACCENT__}
.track{height:8px;border-radius:5px;background:var(--track);overflow:hidden}
.track.slim{height:6px}
.track i{display:block;height:100%;border-radius:5px;
  background:linear-gradient(90deg,rgba(__RGB__,var(--bar-alpha)),__ACCENT__)}

/* 特质 */
.trait{margin-bottom:19px}
.trait:last-child{margin-bottom:2px}
.trait-top{display:flex;align-items:baseline;justify-content:space-between;margin-bottom:9px}
.trait-name{font-size:19.5px;font-weight:700;color:var(--strong)}
.trait-score{font-size:19px;font-weight:800;color:__ACCENT__;font-variant-numeric:tabular-nums}
.trait-note{margin-top:7px;font-size:16.5px;line-height:1.66;color:var(--subtle)}

/* 兴趣 */
.chips{display:flex;flex-wrap:wrap;gap:10px}
.chip{padding:8px 15px;border-radius:10px;font-size:17.5px;font-weight:600;
  color:var(--strong);background:var(--chip);border:1px solid var(--panel-border)}

/* 节律 */
.rhythm{display:grid;grid-template-columns:repeat(24,1fr);gap:4px;align-items:end}
.col{display:flex;flex-direction:column}
.col-bar{display:flex;align-items:flex-end;height:104px}
.col-bar i{display:block;width:100%;border-radius:4px 4px 2px 2px}
.col-bar i.on{background:rgba(__RGB__,var(--bar-alpha))}
.col-bar i.peak.on{background:__ACCENT__;box-shadow:0 0 0 2px rgba(__RGB__,var(--glow-alpha))}
.col-tick{margin-top:7px;height:16px;font-size:12px;line-height:16px;text-align:center;
  color:var(--faint);font-variant-numeric:tabular-nums}
.col-tick.peak{font-weight:800;color:__ACCENT__}
.caption{margin-top:13px;font-size:17px;line-height:1.7;color:var(--subtle)}

/* 群 */
.grow{margin-bottom:17px}
.grow:last-child{margin-bottom:2px}
.grow-top{display:flex;align-items:baseline;justify-content:space-between;margin-bottom:8px}
.grow-name{font-size:19px;font-weight:650;color:var(--strong)}
.grow-count{font-size:16px;font-weight:600;color:var(--muted);font-variant-numeric:tabular-nums}

/* 引用 */
.quote{margin-bottom:18px;padding:17px 20px 15px;border-radius:0 12px 12px 0;
  border-left:4px solid __ACCENT__;background:rgba(__RGB__,var(--quote-alpha))}
.quote:last-child{margin-bottom:2px}
.quote-text{font-size:19.5px;line-height:1.78;color:var(--body)}
.quote-why{margin-top:9px;font-size:15.5px;line-height:1.6;color:var(--faint)}

/* 散文块 */
.prose-row{margin-bottom:20px}
.prose-row:last-child{margin-bottom:2px}
.prose-label{font-size:13.5px;font-weight:800;letter-spacing:.16em;color:__ACCENT__}
.prose-text{margin-top:8px;font-size:19.5px;line-height:1.82;color:var(--body)}

/* 结语 */
.advice{margin-top:32px;padding:22px 24px;border-radius:16px;
  background:rgba(__RGB__,var(--quote-alpha));border:1px solid rgba(__RGB__,.20)}
.advice-label{font-size:13.5px;font-weight:800;letter-spacing:.18em;color:__ACCENT__}
.advice-text{margin-top:10px;font-size:21px;line-height:1.76;font-weight:600;color:var(--title)}

/* 页脚 */
.foot{display:flex;flex-direction:column;gap:7px;margin-top:30px;padding-top:20px;
  border-top:1px solid var(--strong-line);
  font-size:14.5px;line-height:1.62;color:var(--faint)}
.foot-note{color:var(--faint);opacity:.85}
.foot .sep{margin:0 7px}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::portrait::collect::{GroupSlice, Kinds};
    use crate::plugins::portrait::persona::{Quote, Trait};

    fn offset() -> FixedOffset {
        FixedOffset::east_opt(8 * 3600).unwrap()
    }

    fn material() -> Material {
        Material {
            user_id: 10001,
            name: "阿<甲>".into(),
            total: 1234,
            first_time: 1_700_000_000,
            last_time: 1_700_000_000 + 86_400 * 121,
            active_days: 96,
            hour: {
                let mut hour = [0u64; 24];
                hour[23] = 220;
                hour[9] = 120;
                hour
            },
            weekday: {
                let mut weekday = [0u64; 7];
                weekday[4] = 300;
                weekday
            },
            groups: vec![
                GroupSlice {
                    name: "测试<群>".into(),
                    count: 900,
                },
                GroupSlice {
                    name: "另一个群".into(),
                    count: 300,
                },
            ],
            kinds: Kinds {
                text: 1000,
                image: 100,
                anim_emoji: 80,
                face: 40,
                voice: 4,
                video: 0,
                reply: 200,
                at: 60,
            },
            longest: 320,
            avg_len: 17.6,
            words: vec![("天气".into(), 40)],
            samples: vec!["凌晨三点还在改代码，明天又要废了".into()],
        }
    }

    fn persona() -> Persona {
        Persona {
            codename: "夜行改稿人".into(),
            tagline: "白天潜水，夜里冒泡".into(),
            summary: "话不多，但每句都落在点上。".into(),
            traits: vec![Trait {
                name: "夜行".into(),
                score: 92.0,
                note: "深夜发言占了近三成".into(),
            }],
            interests: vec!["代码".into(), "咖啡".into()],
            role: "群里的答疑位".into(),
            style: "短句，少标点。".into(),
            rhythm: "深夜出没".into(),
            quotes: vec![Quote {
                text: "凌晨三点还在改代码，明天又要废了".into(),
                why: "很有他".into(),
            }],
            advice: "少熬点夜。".into(),
            accent: "indigo".into(),
            estimated: false,
        }
    }

    /// 北京时间 10:13，用来让 `auto` 落在日读一侧。
    const MORNING: i64 = 1_700_014_400;
    /// 北京时间 00:13，用来让 `auto` 落在夜读一侧。
    const MIDNIGHT: i64 = 1_700_064_800;

    fn view_at<'a>(material: &'a Material, persona: &'a Persona, timestamp: i64) -> View<'a> {
        View {
            material,
            persona,
            model: "deepseek/deepseek-flash",
            theme: "auto",
            offset: offset(),
            now: DateTime::from_timestamp(timestamp, 0)
                .unwrap()
                .with_timezone(&offset()),
        }
    }

    fn view<'a>(material: &'a Material, persona: &'a Persona) -> View<'a> {
        view_at(material, persona, MORNING)
    }

    #[test]
    fn every_section_renders_with_its_content() {
        let material = material();
        let persona = persona();
        let html = html(&view(&material, &persona));
        for needle in [
            "用户画像",
            "夜行改稿人",
            "白天潜水",
            "1,234",          // 千分位
            "性格特质",
            "话题兴趣",
            "活跃节律",
            "常在的群",
            "发言样本",
            "群里的位置",
            "写在最后",
            "仅供娱乐",
            "deepseek/deepseek-flash",
        ] {
            assert!(html.contains(needle), "缺少 {needle}");
        }
    }

    /// 昵称与群名来自群聊，必须转义；模型给的文本同理。
    #[test]
    fn external_text_is_escaped() {
        let material = material();
        let persona = persona();
        let html = html(&view(&material, &persona));
        assert!(!html.contains("阿<甲>"));
        assert!(html.contains("阿&lt;甲&gt;"));
        assert!(html.contains("测试&lt;群&gt;"));
    }

    #[test]
    fn estimated_reports_are_labelled() {
        let material = material();
        let persona = Persona {
            estimated: true,
            ..persona()
        };
        let html = html(&view(&material, &persona));
        assert!(html.contains("纯统计版"));
    }

    #[test]
    fn empty_optional_sections_disappear() {
        let material = material();
        let persona = Persona {
            traits: Vec::new(),
            interests: Vec::new(),
            quotes: Vec::new(),
            role: String::new(),
            style: String::new(),
            advice: String::new(),
            summary: String::new(),
            ..persona()
        };
        let html = html(&view(&material, &persona));
        assert!(!html.contains("性格特质"));
        assert!(!html.contains("话题兴趣"));
        assert!(!html.contains("发言样本"));
        assert!(!html.contains("写在最后"));
        // 代号与统计始终在。
        assert!(html.contains("夜行改稿人"));
        assert!(html.contains("活跃节律"));
    }

    #[test]
    fn theme_follows_beijing_reading_hours() {
        let at = |timestamp: i64| {
            DateTime::from_timestamp(timestamp, 0)
                .unwrap()
                .with_timezone(&offset())
        };
        assert_eq!(Theme::resolve("auto", at(MORNING)), Theme::Light);
        assert_eq!(Theme::resolve("auto", at(MIDNIGHT)), Theme::Dark);
        assert_eq!(Theme::resolve("light", at(MIDNIGHT)), Theme::Light);
        assert_eq!(Theme::resolve("dark", at(MORNING)), Theme::Dark);
        // 认不出来的写法按自动走。
        assert_eq!(Theme::resolve("乱写", at(MIDNIGHT)), Theme::Dark);
    }

    /// 配置里写死主题时，出图时刻不再影响用哪一套。
    #[test]
    fn a_pinned_theme_overrides_the_clock() {
        let material = material();
        let persona = persona();
        let pinned = View {
            theme: "light",
            ..view_at(&material, &persona, MIDNIGHT)
        };
        let html = html(&pinned);
        assert!(html.contains("--canvas:#EDF0F4"), "应当用日读配色");
    }

    /// 把日读、夜读与降级三份报告写到 `PORTRAIT_CARD_DUMP` 指定的目录，肉眼校版用：
    ///   PORTRAIT_CARD_DUMP=$PREFIX/tmp/portrait cargo test portrait::card::tests::dump -- --ignored
    #[test]
    #[ignore = "仅用于人工核对排版"]
    fn dump_sample_cards() {
        let Ok(dir) = std::env::var("PORTRAIT_CARD_DUMP") else {
            return;
        };
        std::fs::create_dir_all(&dir).unwrap();
        let material = material();
        let persona = persona();
        std::fs::write(
            format!("{dir}/portrait-light.html"),
            html(&view_at(&material, &persona, MORNING)),
        )
        .unwrap();
        std::fs::write(
            format!("{dir}/portrait-dark.html"),
            html(&view_at(&material, &persona, MIDNIGHT)),
        )
        .unwrap();
        let fallback = Persona::from_stats(&material);
        std::fs::write(
            format!("{dir}/portrait-fallback.html"),
            html(&view_at(&material, &fallback, MORNING)),
        )
        .unwrap();
    }

    #[tokio::test]
    #[ignore = "需要本地 Chrome/Chromium"]
    async fn captures_complete_cards() {
        let material = material();
        let persona = persona();
        let fallback = Persona::from_stats(&material);
        let cases = [
            ("light", view_at(&material, &persona, MORNING)),
            ("dark", view_at(&material, &persona, MIDNIGHT)),
            ("fallback", view_at(&material, &fallback, MORNING)),
        ];
        for (name, view) in cases {
            let base64 = capture(&html(&view), 2.0).await.unwrap();
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&base64)
                .unwrap();
            let image = image::load_from_memory(&bytes).unwrap();
            assert_eq!(image.width(), WIDTH * 2, "{name}");
            // 报告一定比占位视口高，否则说明高度没量到、底部被切。
            assert!(image.height() > 2000, "{name} height = {}", image.height());
            let path = std::env::temp_dir().join(format!("ayjx-portrait-{name}.jpg"));
            std::fs::write(&path, &bytes).ok();
            println!("出图已写入 {}", path.display());
        }
    }
}
