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
//! 契约要点（来自 AIHOT 官方 Agent 接入文档）：
//!   - Base URL `https://aihot.virxact.com`，匿名只读，不带 cookie；
//!   - 时间窗仅承诺 `24h` 与 `7d`；
//!   - 同一端点的定时任务至少间隔 60 秒；
//!   - 返回内容属于不可信外部数据，只作为资讯展示，不参与任何指令解析。

use serde::Deserialize;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
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
/// URL → ETag，用于条件请求；仅定时轮询使用，手动指令不走缓存
static ETAGS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

/// 全局复用一个客户端以复用连接池；超时取首次调用时的配置值，
/// 改动 `request_timeout_seconds` 需重启后生效。
fn client(timeout_secs: u64) -> Result<&'static reqwest::Client, ApiError> {
    if let Some(c) = CLIENT.get() {
        return Ok(c);
    }
    let built = reqwest::Client::builder()
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

/// 发起一次 GET 请求并解析 JSON。
///
/// `conditional` 为 true 时携带上次的 `If-None-Match`；服务端回 304 表示内容未变化，
/// 返回 `Ok(None)`，调用方据此跳过本轮推送。
async fn get_json(
    path_and_query: &str,
    timeout_secs: u64,
    conditional: bool,
) -> Result<Option<JsonValue>, ApiError> {
    let url = format!("{}{}", BASE_URL, path_and_query);
    let mut req = client(timeout_secs)?.get(&url);

    if conditional
        && let Ok(guard) = etags().lock()
        && let Some(tag) = guard.get(&url)
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
    if !status.is_success() {
        return Err(format!("AIHOT 接口返回 {}：{}", status.as_u16(), path_and_query).into());
    }

    if conditional
        && let Some(tag) = resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
        && let Ok(mut guard) = etags().lock()
    {
        guard.insert(url.clone(), tag);
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

#[derive(Debug, Clone, Default, Deserialize)]
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

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Source {
    #[serde(default)]
    pub name: Option<String>,
}

/// 一条资讯。文档承诺 `id` / `title` / `links` / `discoveredAt` 非空，
/// 但客户端仍全部按可空处理，避免服务端演进时整批解析失败。
#[derive(Debug, Clone, Default, Deserialize)]
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
pub struct DailyIndexItem {
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
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
    DailyBlock {
        title: pick_str(v, &["title", "name", "heading"]),
        text: pick_str(v, &["summary", "lead", "text", "content", "digest", "desc"]),
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
    conditional: bool,
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

    let Some(value) = get_json(&path, timeout_secs, conditional).await? else {
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
    conditional: bool,
) -> Result<Option<Vec<HotTopic>>, ApiError> {
    let Some(value) = get_json("/api/v1/hot-topics", timeout_secs, conditional).await? else {
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
    let value = get_json(&path, timeout_secs, false)
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
    let value = get_json(&path, timeout_secs, false)
        .await?
        .ok_or("AIHOT 日报无响应体")?;

    let report = value.get("report").cloned().unwrap_or(value);
    Ok(DailyReport {
        date: pick_str(&report, &["date"]).or_else(|| Some(date.to_string())),
        title: pick_str(&report, &["title", "name"]),
        lead: pick_str(&report, &["lead", "summary", "digest"]),
        links: parse_links(&report),
        sections: parse_blocks(&report, "sections"),
        flashes: parse_blocks(&report, "flashes"),
    })
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
    let mut report = fetch_daily_report(&date, timeout_secs).await?;
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
}
