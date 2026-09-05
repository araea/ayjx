//! AIHOT 模型榜（`/leaderboard`）抓取与解析。
//!
//! AIHOT 的公开 API v1 只覆盖资讯、热点、日报三类数据，模型榜没有对应端点
//! （`openapi-v1.json` 里查得到全部操作，其中没有 leaderboard），
//! 所以这里退而求其次读页面。取数方式仍尽量克制：
//!
//!   1. 页面在 `robots.txt` 的 `User-agent: *` 组里未被 Disallow，允许抓取；
//!   2. 只读榜单页一个 URL，不遍历子页、不抓资讯正文；
//!   3. 结果按 `leaderboard_cache_minutes` 本地缓存（默认 30 分钟），
//!      榜单每天只更新几次，指令再密也不会变成对站点的轮询；
//!   4. 沿用 `api` 模块的客户端与 UA，保持可识别、可追溯。
//!
//! 解析取的是页面里 Next.js 的 RSC 数据段而非渲染后的 DOM：数据段是结构化 JSON
//! （字段名与站点内部模型一致），比起追着 class 名解析 HTML 更不容易被样式调整打断。
//! 站点改版仍可能让解析失效——所有字段一律按可空处理，取不到就如实报错，不猜不编。

use super::api::{ApiError, client};
use regex::Regex;
use serde::Deserialize;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const PAGE_URL: &str = "https://aihot.virxact.com/leaderboard";
pub const ATTRIBUTION: &str = "数据来源：AIHOT 模型榜 (aihot.virxact.com/leaderboard)";

// ================= 数据结构 =================

/// 榜单上的一个模型。字段全部可空：站点演进时宁可少显示一项，也不要整榜解析失败。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelEntry {
    pub rank: Option<u32>,
    /// 上一期名次，缺失表示新进榜
    pub previous_rank: Option<u32>,
    /// 名次变化，正数为上升
    pub rank_change: Option<i32>,
    pub name: Option<String>,
    /// 厂商，如 Anthropic / OpenAI
    pub provider: Option<String>,
    pub slug: Option<String>,
    /// 上线日期（ISO8601）
    pub released_at: Option<String>,
    pub context_window_tokens: Option<u64>,
    /// 官网参考价，人民币／百万 Token
    pub input_price_per_million_cny: Option<f64>,
    pub output_price_per_million_cny: Option<f64>,
    /// AIHOT 共识分（0—100）
    pub score: Option<f64>,
    /// 评测完整度（0—1）
    pub coverage: Option<f64>,
    /// 证据可信度：HIGH / MEDIUM / LOW
    pub confidence: Option<String>,
    pub possible_rank_from: Option<u32>,
    pub possible_rank_to: Option<u32>,
    pub summary: Option<String>,
}

impl ModelEntry {
    pub fn display_name(&self) -> &str {
        self.name.as_deref().map(str::trim).unwrap_or("(未知模型)")
    }

    pub fn provider_name(&self) -> Option<&str> {
        self.provider.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }

    /// 共识分保留一位小数
    pub fn score_text(&self) -> Option<String> {
        self.score.map(|s| format!("{:.1}", s))
    }

    /// 评测完整度百分比
    pub fn coverage_text(&self) -> Option<String> {
        self.coverage.map(|c| format!("{:.0}%", c * 100.0))
    }

    /// 证据可信度的中文标签
    pub fn confidence_label(&self) -> Option<&'static str> {
        match self.confidence.as_deref()?.trim().to_ascii_uppercase().as_str() {
            "HIGH" => Some("高"),
            "MEDIUM" => Some("中"),
            "LOW" => Some("低"),
            _ => None,
        }
    }

    /// 名次变化：上升 / 下降 / 持平 / 新入榜
    pub fn trend(&self) -> Trend {
        match (self.rank_change, self.previous_rank) {
            (Some(0), _) => Trend::Flat,
            (Some(delta), _) if delta > 0 => Trend::Up(delta as u32),
            (Some(delta), _) => Trend::Down(delta.unsigned_abs()),
            (None, None) => Trend::New,
            (None, Some(_)) => Trend::Flat,
        }
    }

    /// 上线日期，只取 `YYYY-MM-DD`
    pub fn released_date(&self) -> Option<&str> {
        let raw = self.released_at.as_deref()?;
        (raw.len() >= 10).then(|| &raw[..10])
    }

    /// 「输入 ¥x / 输出 ¥y」；两项都缺时返回 None（站点显示为「待核验」）
    pub fn price_text(&self) -> Option<String> {
        let fmt = |v: f64| {
            if v >= 100.0 {
                format!("¥{:.0}", v)
            } else {
                format!("¥{:.2}", v)
            }
        };
        match (
            self.input_price_per_million_cny,
            self.output_price_per_million_cny,
        ) {
            (Some(i), Some(o)) => Some(format!("入 {} / 出 {}", fmt(i), fmt(o))),
            (Some(i), None) => Some(format!("入 {}", fmt(i))),
            (None, Some(o)) => Some(format!("出 {}", fmt(o))),
            (None, None) => None,
        }
    }

    /// 上下文窗口，如 1M / 256K
    pub fn context_text(&self) -> Option<String> {
        let tokens = self.context_window_tokens.filter(|t| *t > 0)?;
        Some(if tokens >= 1_000_000 {
            format!("{:.0}M", tokens as f64 / 1_000_000.0)
        } else if tokens >= 1_000 {
            format!("{:.0}K", tokens as f64 / 1_000.0)
        } else {
            tokens.to_string()
        })
    }
}

/// 名次变化
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trend {
    Up(u32),
    Down(u32),
    Flat,
    New,
}

impl Trend {
    /// 群聊里够用的短标记；持平留空，避免一列全是符号反而看不出重点
    pub fn marker(&self) -> String {
        match self {
            Trend::Up(n) => format!("↑{}", n),
            Trend::Down(n) => format!("↓{}", n),
            Trend::Flat => String::new(),
            Trend::New => "NEW".to_string(),
        }
    }
}

/// 一次抓取的榜单快照
#[derive(Debug, Clone, Default)]
pub struct Board {
    /// 站点标注的更新时间，如「8月21日 20:00」
    pub updated_at: Option<String>,
    /// 参与汇总的公开榜单家数
    pub source_count: Option<u32>,
    pub entries: Vec<ModelEntry>,
}

// ================= 抓取 =================

type Cache = Mutex<Option<(Instant, Board)>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// 带本地缓存的抓取：`ttl_minutes` 内重复调用直接复用上次结果
pub async fn fetch_cached(timeout_secs: u64, ttl_minutes: u64) -> Result<Board, ApiError> {
    let ttl = Duration::from_secs(ttl_minutes.clamp(1, 24 * 60) * 60);

    if let Ok(guard) = cache().lock()
        && let Some((fetched_at, board)) = guard.as_ref()
        && fetched_at.elapsed() < ttl
    {
        return Ok(board.clone());
    }

    let board = fetch(timeout_secs).await?;

    if let Ok(mut guard) = cache().lock() {
        *guard = Some((Instant::now(), board.clone()));
    }
    Ok(board)
}

/// 抓一次榜单页并解析
pub async fn fetch(timeout_secs: u64) -> Result<Board, ApiError> {
    let resp = client(timeout_secs)?
        .get(PAGE_URL)
        .header(reqwest::header::ACCEPT, "text/html")
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("AIHOT 模型榜返回 {}", status.as_u16()).into());
    }

    parse_board(&resp.text().await?)
}

/// 仅用于测试与离线排查：清空缓存
#[cfg(test)]
pub fn clear_cache() {
    if let Ok(mut guard) = cache().lock() {
        *guard = None;
    }
}

// ================= 解析 =================

pub fn parse_board(html: &str) -> Result<Board, ApiError> {
    let payload = flight_payload(html);
    let source = if payload.contains("\"entries\"") {
        payload.as_str()
    } else {
        // 数据段结构变化时再退回原始 HTML 试一次（内容一致，只是转义不同）
        html
    };

    let entries = extract_entries(source)
        .ok_or("未能从 AIHOT 模型榜页面解析出榜单数据（站点结构可能已调整）")?;
    if entries.is_empty() {
        return Err("AIHOT 模型榜当前没有条目".into());
    }

    Ok(Board {
        updated_at: capture(updated_at_re(), source),
        source_count: capture(source_count_re(), source).and_then(|s| s.parse().ok()),
        entries,
    })
}

/// 把页面里 `self.__next_f.push([1,"..."])` 的分片拼回一整段 RSC 数据。
///
/// 分片是 JSON 字符串字面量，内容本身又是 JSON——数组可能横跨两个分片，
/// 所以先整体解码拼接，再在完整文本里找数据。
fn flight_payload(html: &str) -> String {
    const MARKER: &str = "self.__next_f.push([1,";
    let mut out = String::new();
    let mut rest = html;

    while let Some(pos) = rest.find(MARKER) {
        rest = &rest[pos + MARKER.len()..];
        let Some(quote) = rest.find('"') else { break };
        let chunk = &rest[quote..];
        let Some(end) = string_literal_end(chunk) else {
            break;
        };
        if let Ok(decoded) = serde_json::from_str::<String>(&chunk[..=end]) {
            out.push_str(&decoded);
        }
        rest = &chunk[end + 1..];
    }
    out
}

/// 给定以 `"` 开头的切片，返回该 JSON 字符串字面量结束引号的下标
fn string_literal_end(slice: &str) -> Option<usize> {
    let bytes = slice.as_bytes();
    let mut escaped = false;
    for (idx, byte) in bytes.iter().enumerate().skip(1) {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' => escaped = true,
            b'"' => return Some(idx),
            _ => {}
        }
    }
    None
}

/// 在文本里定位 `"entries":[ ... ]` 并反序列化
fn extract_entries(source: &str) -> Option<Vec<ModelEntry>> {
    let key = source.find("\"entries\":[")?;
    let start = key + "\"entries\":".len();
    let end = array_end(&source[start..])?;
    serde_json::from_str::<Vec<ModelEntry>>(&source[start..start + end + 1]).ok()
}

/// 给定以 `[` 开头的切片，返回配对右括号的下标（跳过字符串内的括号）
fn array_end(slice: &str) -> Option<usize> {
    let bytes = slice.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (idx, byte) in bytes.iter().enumerate() {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

fn updated_at_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#""更新于 ",\s*"([^"]{1,40})""#).expect("正则字面量合法"))
}

fn source_count_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#""children":(\d{1,3})\}\]," 家公开榜单""#).expect("正则字面量合法"))
}

fn capture(re: &Regex, source: &str) -> Option<String> {
    re.captures(source)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与真实页面同构的最小样本：RSC 分片 + 转义的 JSON
    fn sample_html() -> String {
        let flight_a = r#"["$","div",null,{"className":"lb-title-meta","children":[["$","span",null,{"children":["综合 ",["$","strong",null,{"children":7}]," 家公开榜单"]}],["$","span",null,{"children":["更新于 ","8月21日 20:00"]}]]}],["$","$L2a",null,{"entries":[{"rank":1,"previousRank":2,"rankChange":1,"slug":"model-a","name":"Model A","provider":"Vendor","releasedAt":"2026-06-09T00:00:00.000Z","contextWindowTokens":1000000,"inputPricePerMillionCny":67.206,"outputPricePerMillionCny":336.03,"score":89.4,"coverage":0.88,"confidence":"HIGH","possibleRankFrom":1,"possibleRankTo":3,"summary":"由 6 个独立证据家族共同支持"},{"rank":2,"#;
        // 数组横跨两个分片，验证拼接逻辑
        let flight_b = r#""previousRank":null,"rankChange":null,"slug":"model-b","name":"Model B","provider":"Vendor B","releasedAt":"2026-07-24T00:00:00.000Z","inputPricePerMillionCny":null,"outputPricePerMillionCny":null,"score":86.2,"coverage":0.6,"confidence":"LOW"}],"basePath":"/leaderboard"}]"#;

        format!(
            "<html><body><div>页面正文</div>\
<script>self.__next_f.push([1,{}])</script>\
<script>self.__next_f.push([1,{}])</script></body></html>",
            serde_json::to_string(flight_a).unwrap(),
            serde_json::to_string(flight_b).unwrap()
        )
    }

    #[test]
    fn parses_entries_across_flight_chunks() {
        let board = parse_board(&sample_html()).expect("样本应能解析");
        assert_eq!(board.entries.len(), 2);
        assert_eq!(board.updated_at.as_deref(), Some("8月21日 20:00"));
        assert_eq!(board.source_count, Some(7));

        let first = &board.entries[0];
        assert_eq!(first.display_name(), "Model A");
        assert_eq!(first.provider_name(), Some("Vendor"));
        assert_eq!(first.score_text().as_deref(), Some("89.4"));
        assert_eq!(first.coverage_text().as_deref(), Some("88%"));
        assert_eq!(first.confidence_label(), Some("高"));
        assert_eq!(first.trend(), Trend::Up(1));
        assert_eq!(first.released_date(), Some("2026-06-09"));
        assert_eq!(first.context_text().as_deref(), Some("1M"));
        assert_eq!(first.price_text().as_deref(), Some("入 ¥67.21 / 出 ¥336"));
    }

    #[test]
    fn treats_missing_fields_as_absent_not_zero() {
        let board = parse_board(&sample_html()).unwrap();
        let second = &board.entries[1];
        // 没有上期名次也没有变化量 => 新入榜；价格缺失不编造
        assert_eq!(second.trend(), Trend::New);
        assert_eq!(second.price_text(), None);
        assert_eq!(second.context_text(), None);
        assert_eq!(second.confidence_label(), Some("低"));
    }

    #[test]
    fn reports_a_clear_error_when_the_page_changes() {
        let err = parse_board("<html><body>什么都没有</body></html>").unwrap_err();
        assert!(err.to_string().contains("站点结构"), "{}", err);
    }

    #[test]
    fn array_scanner_ignores_brackets_inside_strings() {
        assert_eq!(array_end(r#"[{"a":"]["}]"#), Some(11));
        assert_eq!(array_end(r#"[1,[2],3]"#), Some(8));
        assert_eq!(array_end("[1,2"), None);
    }

    /// 联网冒烟测试：确认线上页面仍能解析出榜单
    ///   cargo test plugins::ai_news::leaderboard::tests::live -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "需要访问 aihot.virxact.com"]
    async fn live_page_is_parseable() {
        let board = fetch(20).await.expect("榜单页应可访问并解析");
        assert!(!board.entries.is_empty());
        println!(
            "更新于 {:?} · {} 家榜单 · {} 个模型",
            board.updated_at,
            board.source_count.unwrap_or(0),
            board.entries.len()
        );
        println!(
            "{}",
            crate::plugins::ai_news::render::render_models(&board, 12).to_text()
        );

        // 顺带把真实数据的卡片 HTML 落盘，方便肉眼校版
        if let Ok(dir) = std::env::var("AI_NEWS_CARD_DUMP") {
            std::fs::create_dir_all(&dir).unwrap();
            let html = crate::plugins::ai_news::card::models_card(
                &board,
                12,
                crate::plugins::ai_news::card::CardTheme::Dark,
            );
            std::fs::write(format!("{}/models_live.html", dir), html).unwrap();
        }
    }
}
