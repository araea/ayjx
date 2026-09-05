//! AIHOT v1 API 客户端（匿名只读，无需 API Key）。
//!
//! 之所以选 API 而不是 RSS：
//!   1. 返回结构化 JSON，字段稳定（`schemaVersion`），无需引入 XML/HTML 解析依赖；
//!   2. 支持服务端筛选（`mode` / `window` / `category` / `q` / `limit`），
//!      推送多少条、推什么分类都交给服务端，客户端不必拉全量再本地过滤；
//!   3. 每条带稳定 `id`，可做跨次推送去重，RSS 只能靠链接猜；
//!   4. 额外提供热点榜与日报端点，RSS 只有三个固定 feed；
//!   5. 支持 `ETag` / `If-None-Match` 条件请求，轮询成本低。
//!
//! 契约要点（来自 AIHOT 官方 Agent 接入文档与 `llms.txt`）：
//!   - Base URL `https://aihot.virxact.com`，匿名只读，不带 cookie；
//!   - 时间窗仅承诺 `24h` 与 `7d`；
//!   - 轮询间隔以响应 `Cache-Control` 的 `s-maxage` 为下限：
//!     `/api/v1/items` 为 60 秒，`/api/v1/hot-topics` 为 300 秒；更密只会拿到同一份缓存副本；
//!   - 遇到 429 / 503 按 `Retry-After` 退避（见 [`backoff_seconds_left`]）；
//!   - 官方明确「没有资讯推送通道」：REST / RSS 无 Webhook 或流式订阅，
//!     MCP 也只在 Agent 主动调用时读取。因此实时推送只能是条件轮询（见 `realtime.rs`）；
//!   - 返回内容属于不可信外部数据，只作为资讯展示，不参与任何指令解析。

use chrono::{DateTime, FixedOffset, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

pub type ApiError = Box<dyn std::error::Error + Send + Sync>;

pub const BASE_URL: &str = "https://aihot.virxact.com";
pub const ATTRIBUTION: &str = "数据来源：AIHOT (aihot.virxact.com)";

/// 允许的分类 slug（服务端可能新增值，这里只用于校验用户配置，不用于校验响应）
pub const CATEGORIES: &[(&str, &str)] = &[
    ("ai-models", "模型"),
    ("ai-products", "产品"),
    ("industry", "行业"),
    ("paper", "论文"),
    ("tip", "技巧"),
];

pub fn category_label(slug: &str) -> &str {
    CATEGORIES
        .iter()
        .find(|(s, _)| *s == slug)
        .map(|(_, label)| *label)
        .unwrap_or(slug)
}

// ================= HTTP 客户端 =================

static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
/// (分区, URL) → ETag，用于条件请求；仅轮询使用，手动指令不走缓存
static ETAGS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
/// 服务端要求退避到的时间点（Unix 秒）；收到 429 / 503 时按 `Retry-After` 设置
static BACKOFF_UNTIL: AtomicI64 = AtomicI64::new(0);

/// 轮询的缓存策略。
///
/// 条件请求靠 `ETag` 省流量，但不同用途的轮询必须各记各的 ETag：
/// 实时轮询每几分钟就把最新的 ETag 刷一遍，若与定时档共用同一份，
/// 定时档到点时只会收到一个 304，永远等不到内容。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Poll {
    /// 不带条件请求，每次都取回完整响应（手动指令走这条）
    Fresh,
    /// 带 `If-None-Match`，ETag 记在给定分区下（定时档 / 实时轮询各一个分区）
    Cached(&'static str),
}

impl Poll {
    fn scope(&self) -> Option<&'static str> {
        match self {
            Poll::Fresh => None,
            Poll::Cached(scope) => Some(scope),
        }
    }
}

/// 距离服务端要求的退避结束还剩几秒（0 表示当前可以正常请求）。
///
/// 轮询任务应在发起请求前检查这里；用户指令不受影响——
/// 手动查询本来就是低频的，没必要因为一次限流把功能整个关掉。
pub fn backoff_seconds_left() -> i64 {
    (BACKOFF_UNTIL.load(Ordering::Relaxed) - Utc::now().timestamp()).max(0)
}

/// 记录一次服务端要求的退避；`Retry-After` 缺失或不是秒数时按 60 秒处理
fn note_backoff(headers: &reqwest::header::HeaderMap) -> u64 {
    let seconds = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(60)
        .clamp(1, 3600);
    BACKOFF_UNTIL.store(Utc::now().timestamp() + seconds as i64, Ordering::Relaxed);
    seconds
}

/// 全局复用一个客户端以复用连接池；超时取首次调用时的配置值，
/// 改动 `request_timeout_seconds` 需重启后生效。
pub(super) fn client(timeout_secs: u64) -> Result<&'static reqwest::Client, ApiError> {
    if let Some(c) = CLIENT.get() {
        return Ok(c);
    }
    let built = crate::http::builder()
        .timeout(Duration::from_secs(timeout_secs.clamp(3, 60)))
        .user_agent(concat!(
            "ayjx-ai-news/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/araea/ayjx)"
        ))
        .build()?;
    Ok(CLIENT.get_or_init(|| built))
}

fn etags() -> &'static Mutex<HashMap<String, String>> {
    ETAGS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// ETag 的存储键：同一个 URL 在不同分区下各记一份
fn etag_key(scope: &str, url: &str) -> String {
    format!("{}\u{1}{}", scope, url)
}

/// 发起一次 GET 请求并解析 JSON。
///
/// [`Poll::Cached`] 会携带上次的 `If-None-Match`；服务端回 304 表示内容未变化，
/// 返回 `Ok(None)`，调用方据此跳过本轮推送。
async fn get_json(
    path_and_query: &str,
    timeout_secs: u64,
    poll: Poll,
) -> Result<Option<JsonValue>, ApiError> {
    // 退避期内不再打扰服务端；用户指令（Fresh）照常放行
    let left = backoff_seconds_left();
    if left > 0 && poll.scope().is_some() {
        return Err(format!("AIHOT 要求退避，还剩 {} 秒", left).into());
    }

    let url = format!("{}{}", BASE_URL, path_and_query);
    let mut req = client(timeout_secs)?.get(&url);

    if let Some(scope) = poll.scope()
        && let Ok(guard) = etags().lock()
        && let Some(tag) = guard.get(&etag_key(scope, &url))
    {
        req = req.header(reqwest::header::IF_NONE_MATCH, tag.clone());
    }

    let resp = req.send().await?;
    let status = resp.status();

    if status == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(None);
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(format!("AIHOT 接口 404：{}", path_and_query).into());
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
    {
        let seconds = note_backoff(resp.headers());
        return Err(format!(
            "AIHOT 接口返回 {}，按 Retry-After 退避 {} 秒：{}",
            status.as_u16(),
            seconds,
            path_and_query
        )
        .into());
    }
    if !status.is_success() {
        return Err(format!("AIHOT 接口返回 {}：{}", status.as_u16(), path_and_query).into());
    }

    if let Some(scope) = poll.scope()
        && let Some(tag) = resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
        && let Ok(mut guard) = etags().lock()
    {
        guard.insert(etag_key(scope, &url), tag);
    }

    Ok(Some(resp.json::<JsonValue>().await?))
}

// ================= 数据结构 =================

/// `id` 等字段的类型服务端未承诺一定是字符串，这里统一收敛成字符串
fn flex_string<'de, D>(de: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = JsonValue::deserialize(de)?;
    Ok(match v {
        JsonValue::String(s) if !s.is_empty() => Some(s),
        JsonValue::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Links {
    /// AIHOT 站内中文阅读页，默认主链接
    #[serde(default)]
    pub aihot: Option<String>,
    /// 第三方原文
    #[serde(default)]
    pub original: Option<String>,
    /// 事件页（仅热点榜可能有），是给人读的 HTML 页面
    #[serde(default)]
    pub story: Option<String>,
}

impl Links {
    /// 展示用主链接：优先站内页，其次原文
    pub fn primary(&self) -> Option<&str> {
        self.aihot
            .as_deref()
            .or(self.original.as_deref())
            .filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Source {
    #[serde(default)]
    pub name: Option<String>,
}

/// 一条资讯。文档承诺 `id` / `title` / `links` / `discoveredAt` 非空，
/// 但客户端仍全部按可空处理，避免服务端演进时整批解析失败。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    #[serde(default, deserialize_with = "flex_string")]
    pub id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    /// 网页「推荐理由」，未精选或无理由时为 null
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub source: Option<Source>,
    #[serde(default)]
    pub links: Links,
    /// 第三方原文发布时间
    #[serde(default)]
    pub published_at: Option<String>,
    /// AIHOT 首次收录时间
    #[serde(default)]
    pub discovered_at: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
}

impl Item {
    /// 去重键：优先服务端 id，缺失时退回站内链接或标题
    pub fn dedupe_key(&self) -> Option<String> {
        if let Some(id) = self.id.as_deref().filter(|s| !s.is_empty()) {
            return Some(format!("id:{}", id));
        }
        if let Some(url) = self.links.primary() {
            return Some(format!("url:{}", url));
        }
        self.title
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|t| format!("title:{}", t))
    }

    /// 是否有足够信息可以展示
    pub fn is_displayable(&self) -> bool {
        self.title.as_deref().is_some_and(|t| !t.trim().is_empty())
    }

    /// 「这条有多新」的时间戳（Unix 秒）：以 AIHOT 首次收录时间为准，
    /// 缺失时退回原文发布时间。
    ///
    /// 用收录时间而非发布时间，是因为实时推送关心的是「刚刚出现在 AIHOT 上」——
    /// 一篇三天前发布、今天才被收录的文章，对群里同样是新消息。
    pub fn discovered_ts(&self) -> Option<i64> {
        self.discovered_at
            .as_deref()
            .or(self.published_at.as_deref())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.timestamp())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ItemsResponse {
    #[serde(default)]
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HotTopic {
    #[serde(default)]
    pub rank: Option<u32>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub source_count: Option<u32>,
    #[serde(default)]
    pub source_names: Vec<String>,
    #[serde(default)]
    pub latest_at: Option<String>,
    #[serde(default)]
    pub links: Links,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct HotTopicsResponse {
    #[serde(default)]
    pub items: Vec<HotTopic>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyIndexItem {
    #[serde(default)]
    pub date: Option<String>,
    /// 索引条目的头条标题（`leadTitle`），正文里可能没有
    #[serde(default)]
    pub lead_title: Option<String>,
    /// 索引条目的导语（`leadParagraph`）
    #[serde(default)]
    pub lead_paragraph: Option<String>,
    #[serde(default)]
    pub links: Links,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DailiesResponse {
    #[serde(default)]
    pub items: Vec<DailyIndexItem>,
}

/// 日报正文。`sections` / `flashes` 的内部结构官方未逐字段固化，
/// 这里用宽容的通用块解析，任何一层缺字段都只影响该块的展示。
#[derive(Debug, Clone, Default)]
pub struct DailyReport {
    pub date: Option<String>,
    pub title: Option<String>,
    pub lead: Option<String>,
    pub links: Links,
    pub sections: Vec<DailyBlock>,
    pub flashes: Vec<DailyBlock>,
}

#[derive(Debug, Clone, Default)]
pub struct DailyBlock {
    pub title: Option<String>,
    pub text: Option<String>,
    pub url: Option<String>,
    pub children: Vec<DailyBlock>,
}

fn pick_str(v: &JsonValue, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(s) = v.get(*k).and_then(JsonValue::as_str) {
            let s = s.trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

fn parse_links(v: &JsonValue) -> Links {
    v.get("links")
        .and_then(|l| serde_json::from_value::<Links>(l.clone()).ok())
        .unwrap_or_default()
}

fn parse_block(v: &JsonValue) -> DailyBlock {
    let links = parse_links(v);
    let mut children = Vec::new();
    for key in ["items", "entries", "list", "flashes", "children"] {
        if let Some(arr) = v.get(key).and_then(JsonValue::as_array) {
            children.extend(arr.iter().map(parse_block));
        }
    }

    // 板块用 `label` 命名，条目 / 快讯用 `title`
    let title = pick_str(v, &["title", "label", "name", "heading"]);
    let mut text = pick_str(v, &["summary", "lead", "text", "content", "digest", "desc"]);

    // 快讯常只有标题、来源与时间而无摘要，正文缺失时合成一行 meta，避免信息静默丢失
    if text.is_none() {
        let source = v
            .get("source")
            .and_then(|s| s.get("name"))
            .and_then(JsonValue::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let time = v
            .get("publishedAt")
            .and_then(JsonValue::as_str)
            .and_then(format_time);
        let meta: Vec<String> = [source.map(str::to_string), time].into_iter().flatten().collect();
        if !meta.is_empty() {
            text = Some(meta.join(" · "));
        }
    }

    DailyBlock {
        title,
        text,
        url: links.primary().map(str::to_string),
        children,
    }
}

fn parse_blocks(v: &JsonValue, key: &str) -> Vec<DailyBlock> {
    v.get(key)
        .and_then(JsonValue::as_array)
        .map(|arr| arr.iter().map(parse_block).collect())
        .unwrap_or_default()
}

// ================= 端点 =================

/// 最近资讯 / 分类 / 搜索：`GET /api/v1/items`
///
/// - `mode`：`selected`（精选，默认）或 `all`（全部公开动态）
/// - `window`：仅支持 `24h` 与 `7d`
/// - `category`：可选分类 slug
/// - `query`：可选关键词（服务端搜索，2—200 字）
pub async fn fetch_items(
    mode: &str,
    window: &str,
    category: Option<&str>,
    query: Option<&str>,
    limit: u32,
    timeout_secs: u64,
    poll: Poll,
) -> Result<Option<Vec<Item>>, ApiError> {
    let mut path = format!(
        "/api/v1/items?mode={}&window={}&limit={}",
        encode(mode),
        encode(window),
        limit.clamp(1, 100)
    );
    if let Some(c) = category.filter(|c| !c.is_empty()) {
        path.push_str(&format!("&category={}", encode(c)));
    }
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        path.push_str(&format!("&q={}", encode(q)));
    }

    let Some(value) = get_json(&path, timeout_secs, poll).await? else {
        return Ok(None);
    };
    let parsed: ItemsResponse = serde_json::from_value(value)?;
    Ok(Some(
        parsed
            .items
            .into_iter()
            .filter(Item::is_displayable)
            .collect(),
    ))
}

/// 当前热点榜：`GET /api/v1/hot-topics`（最多 Top 10，按 rank 升序展示）
pub async fn fetch_hot_topics(
    timeout_secs: u64,
    poll: Poll,
) -> Result<Option<Vec<HotTopic>>, ApiError> {
    let Some(value) = get_json("/api/v1/hot-topics", timeout_secs, poll).await? else {
        return Ok(None);
    };
    let parsed: HotTopicsResponse = serde_json::from_value(value)?;
    let mut items: Vec<HotTopic> = parsed
        .items
        .into_iter()
        .filter(|t| t.title.as_deref().is_some_and(|s| !s.trim().is_empty()))
        .collect();
    // 保持服务端顺序，仅在 rank 齐全时按 rank 兜底排序
    if items.iter().all(|t| t.rank.is_some()) {
        items.sort_by_key(|t| t.rank.unwrap_or(u32::MAX));
    }
    Ok(Some(items))
}

/// 日报索引：`GET /api/v1/dailies?limit=N`
pub async fn fetch_daily_index(
    limit: u32,
    timeout_secs: u64,
) -> Result<Vec<DailyIndexItem>, ApiError> {
    let path = format!("/api/v1/dailies?limit={}", limit.clamp(1, 30));
    let value = get_json(&path, timeout_secs, Poll::Fresh)
        .await?
        .ok_or("AIHOT 日报索引无响应体")?;
    let parsed: DailiesResponse = serde_json::from_value(value)?;
    Ok(parsed.items)
}

/// 指定日期的日报：`GET /api/v1/dailies/{YYYY-MM-DD}`
///
/// 按官方要求，日期只能来自日报索引实际返回的值，绝不本地拼「今天 / 昨天」。
pub async fn fetch_daily_report(date: &str, timeout_secs: u64) -> Result<DailyReport, ApiError> {
    if !is_iso_date(date) {
        return Err(format!("非法的日报日期：{}", date).into());
    }
    let path = format!("/api/v1/dailies/{}", date);
    let value = get_json(&path, timeout_secs, Poll::Fresh)
        .await?
        .ok_or("AIHOT 日报无响应体")?;
    Ok(parse_daily_report(value, date))
}

/// 从日报 JSON 里解析出正文。
///
/// 顶层没有 `title`，标题取 `lead.title`；`lead` 是对象 `{title, leadParagraph}`
/// （可能为 null），不是字符串。标题 / 导语缺失时由调用方用索引的 `leadTitle` 兜底。
fn parse_daily_report(value: JsonValue, fallback_date: &str) -> DailyReport {
    let report = value.get("report").cloned().unwrap_or(value);
    let lead_obj = report.get("lead").cloned().unwrap_or(JsonValue::Null);

    let title = pick_str(&report, &["title", "name"])
        .or_else(|| pick_str(&lead_obj, &["title"]));
    let lead = pick_str(&report, &["summary", "digest"]).or_else(|| {
        pick_str(
            &lead_obj,
            &[
                "leadParagraph",
                "lead_paragraph",
                "paragraph",
                "text",
                "content",
                "summary",
            ],
        )
    });

    DailyReport {
        date: pick_str(&report, &["date"]).or_else(|| Some(fallback_date.to_string())),
        title,
        lead,
        links: parse_links(&report),
        sections: parse_blocks(&report, "sections"),
        flashes: parse_blocks(&report, "flashes"),
    }
}

/// 取最新一期日报：先查索引，再用索引实际返回的日期请求正文
pub async fn fetch_latest_daily(timeout_secs: u64) -> Result<Option<DailyReport>, ApiError> {
    let index = fetch_daily_index(1, timeout_secs).await?;
    let Some(first) = index.into_iter().next() else {
        return Ok(None);
    };
    let Some(date) = first.date.filter(|d| is_iso_date(d)) else {
        return Ok(None);
    };
    // 正文里 `lead` 常为 null，此时用索引的 leadTitle / leadParagraph 兜底
    let lead_title = first.lead_title.clone();
    let lead_paragraph = first.lead_paragraph.clone();
    let mut report = fetch_daily_report(&date, timeout_secs).await?;
    if report.title.is_none() {
        report.title = lead_title;
    }
    if report.lead.is_none() {
        report.lead = lead_paragraph;
    }
    if report.links.primary().is_none() {
        report.links = first.links;
    }
    Ok(Some(report))
}

// ================= 工具 =================

fn is_iso_date(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| if i == 4 || i == 7 { true } else { b.is_ascii_digit() })
}

/// ISO8601 → `MM-DD HH:MM`（北京时间），供日报快讯的发布时间展示
fn format_time(iso: &str) -> Option<String> {
    DateTime::parse_from_rfc3339(iso).ok().map(|dt| {
        dt.with_timezone(&FixedOffset::east_opt(8 * 3600).expect("UTC+8 是合法时区偏移"))
            .format("%m-%d %H:%M")
            .to_string()
    })
}

/// 最小 URL 查询值编码（关键词可能含中文与空格）
fn encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_reserved_characters() {
        assert_eq!(encode("OpenAI"), "OpenAI");
        assert_eq!(encode("a b"), "a%20b");
        assert_eq!(encode("模型"), "%E6%A8%A1%E5%9E%8B");
    }

    #[test]
    fn validates_iso_dates() {
        assert!(is_iso_date("2026-07-24"));
        assert!(!is_iso_date("2026-7-24"));
        assert!(!is_iso_date("../secret"));
        assert!(!is_iso_date(""));
    }

    #[test]
    fn dedupe_key_falls_back_when_id_missing() {
        let mut item = Item {
            title: Some("标题".into()),
            ..Default::default()
        };
        assert_eq!(item.dedupe_key().as_deref(), Some("title:标题"));

        item.links.aihot = Some("https://aihot.virxact.com/i/1".into());
        assert_eq!(
            item.dedupe_key().as_deref(),
            Some("url:https://aihot.virxact.com/i/1")
        );

        item.id = Some("abc".into());
        assert_eq!(item.dedupe_key().as_deref(), Some("id:abc"));
    }

    #[test]
    fn parses_daily_sections_labels_flashes_and_lead() {
        // 结构与 AIHOT 真实日报对齐：板块用 `label`，条目带 `title/summary`，
        // 快讯带 `title/source/publishedAt`，`lead` 是对象而非字符串。
        let value: JsonValue = serde_json::json!({
            "report": {
                "date": "2026-08-21",
                "lead": { "title": "今日头条", "leadParagraph": "这是导语" },
                "sections": [
                    {
                        "label": "模型发布/更新",
                        "items": [
                            {
                                "title": "条目A",
                                "summary": "摘要A",
                                "source": { "name": "IT之家" },
                                "links": { "aihot": "https://aihot.virxact.com/items/1", "original": "https://x.com/1" }
                            }
                        ]
                    }
                ],
                "flashes": [
                    {
                        "title": "快讯B",
                        "source": { "name": "某信源" },
                        "links": { "original": "https://x.com/2" },
                        "publishedAt": "2026-08-21T01:00:00Z"
                    }
                ]
            }
        });

        let report = parse_daily_report(value, "2026-08-21");
        assert_eq!(report.date.as_deref(), Some("2026-08-21"));
        // lead.title 应作为日报标题
        assert_eq!(report.title.as_deref(), Some("今日头条"));
        assert_eq!(report.lead.as_deref(), Some("这是导语"));

        // 板块 label 不能丢
        assert_eq!(report.sections.len(), 1);
        assert_eq!(report.sections[0].title.as_deref(), Some("模型发布/更新"));
        assert_eq!(report.sections[0].children.len(), 1);
        assert_eq!(report.sections[0].children[0].title.as_deref(), Some("条目A"));
        assert_eq!(report.sections[0].children[0].text.as_deref(), Some("摘要A"));
        assert_eq!(
            report.sections[0].children[0].url.as_deref(),
            Some("https://aihot.virxact.com/items/1")
        );

        // 快讯的标题与链接解析，来源 + 时间合成 meta
        assert_eq!(report.flashes.len(), 1);
        assert_eq!(report.flashes[0].title.as_deref(), Some("快讯B"));
        let meta = report.flashes[0].text.as_deref().expect("快讯应合成 meta");
        assert!(meta.contains("某信源"), "{}", meta);
        assert!(meta.contains("08-21 09:00"), "{}", meta);
    }

    #[test]
    fn daily_index_parses_lead_title() {
        let value: JsonValue = serde_json::json!({
            "items": [
                {
                    "date": "2026-08-21",
                    "generatedAt": "2026-08-21T00:01:06.088Z",
                    "leadTitle": "阿里发布 Qwen-UI-Agent",
                    "leadParagraph": null,
                    "links": { "aihot": "https://aihot.virxact.com/daily/2026-08-21" }
                }
            ]
        });
        let parsed: DailiesResponse = serde_json::from_value(value).expect("索引应可解析");
        assert_eq!(parsed.items.len(), 1);
        assert_eq!(parsed.items[0].date.as_deref(), Some("2026-08-21"));
        assert_eq!(parsed.items[0].lead_title.as_deref(), Some("阿里发布 Qwen-UI-Agent"));
        assert_eq!(parsed.items[0].lead_paragraph, None);
    }
}
