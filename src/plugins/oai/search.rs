//! 免凭据网页搜索：多引擎并发扇出 + 跨引擎共识排序。
//!
//! 结构取自 oh-my-pi 的 Public Web 聚合器：所有引擎同时发车，软截止（拿到第一份
//! 结果就返回）与硬截止（无论如何返回）各自把守一头，慢引擎只会减少覆盖面，不会
//! 把整个工具调用拖到模型侧超时。跨引擎去重后按「命中引擎数 → 最佳单引擎排名」
//! 排序，多个独立索引都给出的链接自然浮到前面。
//!
//! 单引擎抓取随时可能被反爬拦截（DuckDuckGo 的 anomaly 弹窗、Mojeek 的 403），
//! 所以这里不追求任何一家稳定可用，只要还有一家活着就能给出结果。

use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::task::JoinSet;
use tokio::time::Instant;

/// 拿到首个引擎结果后再等这么久，用于让快引擎之间互相补充。
const SOFT_DEADLINE: Duration = Duration::from_secs(4);
/// 无论如何都要返回的硬截止。
const HARD_DEADLINE: Duration = Duration::from_secs(14);
/// 单引擎响应体上限，避免异常页面吃满内存。
const MAX_BODY_BYTES: usize = 3 * 1024 * 1024;

const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, Default)]
pub(crate) struct SearchOutcome {
    pub hits: Vec<SearchHit>,
    /// 实际给出结果的引擎，按优先级顺序。
    pub engines: Vec<&'static str>,
    /// 失败引擎的诊断信息，全灭时用于向模型解释原因。
    pub failures: Vec<String>,
}

/// 免凭据引擎。顺序即合并时的并列排名裁决顺序，靠前的引擎排名质量更好。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Engine {
    Brave,
    DuckDuckGo,
    DuckDuckGoLite,
    Mojeek,
    BingRss,
}

impl Engine {
    const ALL: [Engine; 5] = [
        Engine::Brave,
        Engine::DuckDuckGo,
        Engine::DuckDuckGoLite,
        Engine::Mojeek,
        Engine::BingRss,
    ];

    fn name(self) -> &'static str {
        match self {
            Engine::Brave => "brave",
            Engine::DuckDuckGo => "duckduckgo",
            Engine::DuckDuckGoLite => "duckduckgo-lite",
            Engine::Mojeek => "mojeek",
            Engine::BingRss => "bing",
        }
    }

    async fn run(self, query: String, limit: usize) -> anyhow::Result<Vec<SearchHit>> {
        match self {
            Engine::Brave => search_brave(&query, limit).await,
            Engine::DuckDuckGo => search_duckduckgo(&query, limit).await,
            Engine::DuckDuckGoLite => search_duckduckgo_lite(&query, limit).await,
            Engine::Mojeek => search_mojeek(&query, limit).await,
            Engine::BingRss => search_bing_rss(&query, limit).await,
        }
    }
}

/// 并发查询所有免凭据引擎并合并结果。
///
/// 只有全部引擎都失败才返回 `Err`；任意一家有结果就返回该结果，并在
/// `failures` 里保留其余引擎的失败原因。
pub(crate) async fn search_web(query: &str, limit: usize) -> anyhow::Result<SearchOutcome> {
    let query = query.trim();
    if query.is_empty() {
        anyhow::bail!("query must not be empty");
    }
    let limit = limit.clamp(1, 20);
    // 每家多取一点，合并去重后才够 limit 条。
    let per_engine = (limit + 4).min(20);

    let mut tasks = JoinSet::new();
    for engine in Engine::ALL {
        let query = query.to_string();
        tasks.spawn(async move { (engine, engine.run(query, per_engine).await) });
    }

    let started = Instant::now();
    let hard_deadline = started + HARD_DEADLINE;
    let mut collected: Vec<(Engine, Vec<SearchHit>)> = Vec::new();
    let mut failures: Vec<String> = Vec::new();

    while !tasks.is_empty() {
        // 已经有结果时只再等软截止；一条都没有时死等到硬截止，
        // 慢引擎总比空手而归好。
        let wait_until = if collected.is_empty() {
            hard_deadline
        } else {
            hard_deadline.min(started + SOFT_DEADLINE)
        };
        if Instant::now() >= wait_until {
            break;
        }
        match tokio::time::timeout_at(wait_until, tasks.join_next()).await {
            Ok(Some(Ok((engine, Ok(hits))))) if !hits.is_empty() => collected.push((engine, hits)),
            Ok(Some(Ok((engine, Ok(_))))) => failures.push(format!("{}: 无结果", engine.name())),
            Ok(Some(Ok((engine, Err(error))))) => {
                failures.push(format!("{}: {error:#}", engine.name()))
            }
            Ok(Some(Err(error))) => failures.push(format!("engine task: {error}")),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    tasks.abort_all();

    if collected.is_empty() {
        anyhow::bail!(
            "所有免凭据搜索引擎均不可用（{}）",
            if failures.is_empty() {
                "无诊断信息".to_string()
            } else {
                failures.join("；")
            }
        );
    }

    // 按引擎优先级而非返回顺序合并，保证并列排名的裁决是确定的。
    collected.sort_by_key(|(engine, _)| {
        Engine::ALL.iter().position(|item| item == engine).unwrap_or(usize::MAX)
    });
    let engines = collected.iter().map(|(engine, _)| engine.name()).collect();
    let hits = merge_hits(collected.iter().map(|(_, hits)| hits.as_slice()), limit);
    Ok(SearchOutcome {
        hits,
        engines,
        failures,
    })
}

/// 一条去重后的结果及其跨引擎信号。
struct Merged {
    hit: SearchHit,
    /// 命中该链接的引擎数量，共识越强排名越高。
    engines: usize,
    /// 各引擎中最好的名次。
    best_rank: usize,
    /// 首次出现的插入序，保证排序稳定。
    order: usize,
}

/// URL 规范化去重键：忽略 `www.`、大小写、结尾斜杠与锚点。
fn dedup_key(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(parsed) => {
            let host = parsed
                .host_str()
                .unwrap_or_default()
                .to_lowercase()
                .trim_start_matches("www.")
                .to_string();
            let path = parsed.path().trim_end_matches('/');
            format!("{host}{path}{}", parsed.query().unwrap_or_default())
        }
        Err(_) => raw.to_string(),
    }
}

fn merge_hits<'a>(sources: impl Iterator<Item = &'a [SearchHit]>, limit: usize) -> Vec<SearchHit> {
    let mut merged: HashMap<String, Merged> = HashMap::new();
    for hits in sources {
        for (rank, hit) in hits.iter().enumerate() {
            let key = dedup_key(&hit.url);
            match merged.get_mut(&key) {
                None => {
                    let order = merged.len();
                    merged.insert(
                        key,
                        Merged {
                            hit: hit.clone(),
                            engines: 1,
                            best_rank: rank,
                            order,
                        },
                    );
                }
                Some(existing) => {
                    existing.engines += 1;
                    if rank < existing.best_rank {
                        existing.best_rank = rank;
                        existing.hit.title = hit.title.clone();
                        existing.hit.url = hit.url.clone();
                    }
                    // 摘要取最长的那份，信息量更大。
                    if hit.snippet.len() > existing.hit.snippet.len() {
                        existing.hit.snippet = hit.snippet.clone();
                    }
                }
            }
        }
    }

    let mut ranked: Vec<Merged> = merged.into_values().collect();
    ranked.sort_by(|a, b| {
        b.engines
            .cmp(&a.engines)
            .then(a.best_rank.cmp(&b.best_rank))
            .then(a.order.cmp(&b.order))
    });
    ranked.into_iter().take(limit).map(|item| item.hit).collect()
}

/// 贴近真实浏览器导航的请求头；缺了 `Sec-Fetch-*` 会被多数引擎直接 403。
fn browser_request(
    builder: reqwest::RequestBuilder,
    referer: Option<&str>,
) -> reqwest::RequestBuilder {
    let builder = builder
        .header(reqwest::header::USER_AGENT, BROWSER_UA)
        .header(
            reqwest::header::ACCEPT,
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
        )
        .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9,zh-CN;q=0.8")
        .header("Sec-Ch-Ua", "\"Google Chrome\";v=\"149\", \"Chromium\";v=\"149\", \";Not A Brand\";v=\"99\"")
        .header("Sec-Ch-Ua-Mobile", "?0")
        .header("Sec-Ch-Ua-Platform", "\"macOS\"")
        .header("Sec-Fetch-Dest", "document")
        .header("Sec-Fetch-Mode", "navigate")
        .header("Sec-Fetch-User", "?1")
        .header("Upgrade-Insecure-Requests", "1")
        .timeout(HARD_DEADLINE);
    match referer {
        Some(referer) => builder
            .header(reqwest::header::REFERER, referer)
            .header("Sec-Fetch-Site", "same-origin"),
        None => builder.header("Sec-Fetch-Site", "none"),
    }
}

async fn read_body(response: reqwest::Response, engine: &str) -> anyhow::Result<String> {
    let status = response.status();
    let body = response.bytes().await?;
    if !status.is_success() {
        anyhow::bail!("{engine} 返回 HTTP {status}");
    }
    if body.len() > MAX_BODY_BYTES {
        anyhow::bail!("{engine} 响应超过 {} MiB", MAX_BODY_BYTES / 1024 / 1024);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

// ---------------------------------------------------------------- Brave

async fn search_brave(query: &str, limit: usize) -> anyhow::Result<Vec<SearchHit>> {
    let mut url = url::Url::parse("https://search.brave.com/search")?;
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("source", "web");
    let response = browser_request(crate::http::client().get(url), None)
        .send()
        .await?;
    let html = read_body(response, "Brave").await?;
    Ok(parse_brave(&html, limit))
}

/// Brave 的类名带 Svelte 构建哈希，只能锚定稳定的结构标记：
/// `data-type="web"` 划分结果块，块内首个绝对链接是目标地址。
fn parse_brave(html: &str, limit: usize) -> Vec<SearchHit> {
    static BLOCK: OnceLock<Regex> = OnceLock::new();
    static LINK: OnceLock<Regex> = OnceLock::new();
    static TITLE: OnceLock<Regex> = OnceLock::new();
    let block_re = BLOCK.get_or_init(|| {
        Regex::new(r#"(?is)<div\b[^>]*\bclass="snippet[^"]*"[^>]*\bdata-type="web""#).unwrap()
    });
    let link_re = LINK.get_or_init(|| Regex::new(r#"(?is)<a\s+href="(https?://[^"]+)""#).unwrap());
    let title_re = TITLE
        .get_or_init(|| Regex::new(r#"(?is)<div\b[^>]*\bclass="title[^"]*"[^>]*>(.*?)</div>"#).unwrap());

    let starts: Vec<usize> = block_re.find_iter(html).map(|m| m.start()).collect();
    let mut hits = Vec::new();
    for (index, start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(html.len());
        let block = &html[*start..end];
        let Some(url) = link_re
            .captures(block)
            .and_then(|caps| caps.get(1))
            .map(|value| decode_html(value.as_str()))
        else {
            continue;
        };
        let title = title_re
            .captures(block)
            .and_then(|caps| caps.get(1))
            .map(|value| decode_html(value.as_str()))
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        // 摘要块内嵌套 span，闭合标签配不准，改为从标记位置起截一段再去标签。
        let snippet = block
            .find("generic-snippet")
            .and_then(|at| block[at..].find('>').map(|offset| at + offset + 1))
            .map(|at| {
                let slice = &block[at..(at + 1600).min(block.len())];
                truncate_chars(&decode_html(slice), 320)
            })
            .unwrap_or_default();
        hits.push(SearchHit { title, url, snippet });
        if hits.len() >= limit {
            break;
        }
    }
    hits
}

// ---------------------------------------------------------- DuckDuckGo

async fn search_duckduckgo(query: &str, limit: usize) -> anyhow::Result<Vec<SearchHit>> {
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("q", query)
        .append_pair("kl", "wt-wt")
        .append_pair("b", "")
        .finish();
    let response = browser_request(
        crate::http::client().post("https://html.duckduckgo.com/html/"),
        Some("https://html.duckduckgo.com/"),
    )
    .header(
        reqwest::header::CONTENT_TYPE,
        "application/x-www-form-urlencoded",
    )
    .body(form)
    .send()
    .await?;
    let html = read_body(response, "DuckDuckGo").await?;
    if is_ddg_anomaly(&html) {
        anyhow::bail!("DuckDuckGo 触发了反自动化拦截");
    }
    Ok(parse_duckduckgo(&html, limit))
}

async fn search_duckduckgo_lite(query: &str, limit: usize) -> anyhow::Result<Vec<SearchHit>> {
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("q", query)
        .append_pair("kl", "wt-wt")
        .finish();
    let response = browser_request(
        crate::http::client().post("https://lite.duckduckgo.com/lite/"),
        Some("https://lite.duckduckgo.com/"),
    )
    .header(
        reqwest::header::CONTENT_TYPE,
        "application/x-www-form-urlencoded",
    )
    .body(form)
    .send()
    .await?;
    let html = read_body(response, "DuckDuckGo Lite").await?;
    if is_ddg_anomaly(&html) {
        anyhow::bail!("DuckDuckGo Lite 触发了反自动化拦截");
    }
    Ok(parse_duckduckgo_lite(&html, limit))
}

fn is_ddg_anomaly(html: &str) -> bool {
    html.contains("anomaly-modal") || html.contains("anomaly.js")
}

fn parse_duckduckgo(html: &str, limit: usize) -> Vec<SearchHit> {
    static TITLE: OnceLock<Regex> = OnceLock::new();
    static SNIPPET: OnceLock<Regex> = OnceLock::new();
    let title_re = TITLE.get_or_init(|| {
        Regex::new(
            r#"(?is)<a\b[^>]*class=["'][^"']*\bresult__a\b[^"']*["'][^>]*href=["']([^"']+)["'][^>]*>(.*?)</a>"#,
        )
        .unwrap()
    });
    let snippet_re = SNIPPET.get_or_init(|| {
        Regex::new(
            r#"(?is)<(?:a|div|span)\b[^>]*class=["'][^"']*\bresult__snippet\b[^"']*["'][^>]*>(.*?)</(?:a|div|span)>"#,
        )
        .unwrap()
    });

    let matches: Vec<_> = title_re.captures_iter(html).collect();
    let mut hits: Vec<SearchHit> = Vec::new();
    for (index, captures) in matches.iter().enumerate() {
        let Some(url) = captures
            .get(1)
            .and_then(|value| unwrap_redirect_url(value.as_str()))
        else {
            continue;
        };
        if hits.iter().any(|hit| hit.url == url) {
            continue;
        }
        let title = decode_html(captures.get(2).map(|value| value.as_str()).unwrap_or(""));
        if title.is_empty() {
            continue;
        }
        let start = captures.get(0).map(|value| value.end()).unwrap_or(0);
        let end = matches
            .get(index + 1)
            .and_then(|next| next.get(0))
            .map(|value| value.start())
            .unwrap_or(html.len());
        let snippet = snippet_re
            .captures(&html[start..end])
            .and_then(|caps| caps.get(1))
            .map(|value| decode_html(value.as_str()))
            .unwrap_or_default();
        hits.push(SearchHit { title, url, snippet });
        if hits.len() >= limit {
            break;
        }
    }
    hits
}

/// Lite 版是表格布局：`a.result-link` 给标题，同行下方的 `td.result-snippet` 给摘要。
fn parse_duckduckgo_lite(html: &str, limit: usize) -> Vec<SearchHit> {
    static LINK: OnceLock<Regex> = OnceLock::new();
    static SNIPPET: OnceLock<Regex> = OnceLock::new();
    let link_re = LINK.get_or_init(|| {
        Regex::new(
            r#"(?is)<a\b[^>]*class=["'][^"']*\bresult-link\b[^"']*["'][^>]*href=["']([^"']+)["'][^>]*>(.*?)</a>"#,
        )
        .unwrap()
    });
    let snippet_re = SNIPPET.get_or_init(|| {
        Regex::new(r#"(?is)<td\b[^>]*class=["'][^"']*\bresult-snippet\b[^"']*["'][^>]*>(.*?)</td>"#)
            .unwrap()
    });

    let matches: Vec<_> = link_re.captures_iter(html).collect();
    let mut hits: Vec<SearchHit> = Vec::new();
    for (index, captures) in matches.iter().enumerate() {
        let Some(url) = captures
            .get(1)
            .and_then(|value| unwrap_redirect_url(value.as_str()))
        else {
            continue;
        };
        if hits.iter().any(|hit| hit.url == url) {
            continue;
        }
        let title = decode_html(captures.get(2).map(|value| value.as_str()).unwrap_or(""));
        if title.is_empty() {
            continue;
        }
        let start = captures.get(0).map(|value| value.end()).unwrap_or(0);
        let end = matches
            .get(index + 1)
            .and_then(|next| next.get(0))
            .map(|value| value.start())
            .unwrap_or(html.len());
        let snippet = snippet_re
            .captures(&html[start..end])
            .and_then(|caps| caps.get(1))
            .map(|value| decode_html(value.as_str()))
            .unwrap_or_default();
        hits.push(SearchHit { title, url, snippet });
        if hits.len() >= limit {
            break;
        }
    }
    hits
}

// --------------------------------------------------------------- Mojeek

async fn search_mojeek(query: &str, limit: usize) -> anyhow::Result<Vec<SearchHit>> {
    let mut url = url::Url::parse("https://www.mojeek.com/search")?;
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("t", &limit.to_string())
        .append_pair("arc", "none");
    let response = browser_request(
        crate::http::client().get(url),
        Some("https://www.mojeek.com/"),
    )
    .send()
    .await?;
    let html = read_body(response, "Mojeek").await?;
    Ok(parse_mojeek(&html, limit))
}

fn parse_mojeek(html: &str, limit: usize) -> Vec<SearchHit> {
    static ITEM: OnceLock<Regex> = OnceLock::new();
    static SNIPPET: OnceLock<Regex> = OnceLock::new();
    let item_re = ITEM.get_or_init(|| {
        Regex::new(
            r#"(?is)<a\b[^>]*class=["'][^"']*\btitle\b[^"']*["'][^>]*href=["']([^"']+)["'][^>]*>(.*?)</a>"#,
        )
        .unwrap()
    });
    let snippet_re = SNIPPET
        .get_or_init(|| Regex::new(r#"(?is)<p\b[^>]*class=["'][^"']*\bs\b[^"']*["'][^>]*>(.*?)</p>"#).unwrap());

    let matches: Vec<_> = item_re.captures_iter(html).collect();
    let mut hits: Vec<SearchHit> = Vec::new();
    for (index, captures) in matches.iter().enumerate() {
        let Some(url) = captures
            .get(1)
            .and_then(|value| unwrap_redirect_url(value.as_str()))
        else {
            continue;
        };
        if hits.iter().any(|hit| hit.url == url) {
            continue;
        }
        let title = decode_html(captures.get(2).map(|value| value.as_str()).unwrap_or(""));
        if title.is_empty() {
            continue;
        }
        let start = captures.get(0).map(|value| value.end()).unwrap_or(0);
        let end = matches
            .get(index + 1)
            .and_then(|next| next.get(0))
            .map(|value| value.start())
            .unwrap_or(html.len());
        let snippet = snippet_re
            .captures(&html[start..end])
            .and_then(|caps| caps.get(1))
            .map(|value| decode_html(value.as_str()))
            .unwrap_or_default();
        hits.push(SearchHit { title, url, snippet });
        if hits.len() >= limit {
            break;
        }
    }
    hits
}

// ------------------------------------------------------------- Bing RSS

async fn search_bing_rss(query: &str, limit: usize) -> anyhow::Result<Vec<SearchHit>> {
    let mut url = url::Url::parse("https://www.bing.com/search")?;
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("format", "rss")
        .append_pair("count", &limit.to_string());
    let response = browser_request(crate::http::client().get(url), None)
        .header(
            reqwest::header::ACCEPT,
            "application/rss+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .send()
        .await?;
    let xml = read_body(response, "Bing").await?;
    Ok(parse_bing_rss(&xml, limit))
}

fn parse_bing_rss(xml: &str, limit: usize) -> Vec<SearchHit> {
    static ITEM: OnceLock<Regex> = OnceLock::new();
    static FIELD: OnceLock<Regex> = OnceLock::new();
    let item_re = ITEM.get_or_init(|| Regex::new(r"(?is)<item>(.*?)</item>").unwrap());
    let field_re = FIELD.get_or_init(|| {
        Regex::new(r"(?is)<(title|link|description)>(.*?)</(?:title|link|description)>").unwrap()
    });
    item_re
        .captures_iter(xml)
        .filter_map(|item| {
            let mut title = String::new();
            let mut url = String::new();
            let mut snippet = String::new();
            for field in field_re.captures_iter(item.get(1)?.as_str()) {
                let value = decode_html(field.get(2)?.as_str());
                match field.get(1)?.as_str().to_ascii_lowercase().as_str() {
                    "title" => title = value,
                    "link" => url = value,
                    "description" => snippet = value,
                    _ => {}
                }
            }
            if title.is_empty() || !(url.starts_with("https://") || url.starts_with("http://")) {
                None
            } else {
                Some(SearchHit { title, url, snippet })
            }
        })
        .take(limit)
        .collect()
}

// --------------------------------------------------------------- 公共工具

/// 去标签 + 实体解码 + 空白归一。RSS 摘要会把标签编码成实体，故解码两轮。
pub(crate) fn decode_html(value: &str) -> String {
    static TAGS: OnceLock<Regex> = OnceLock::new();
    static SPACE: OnceLock<Regex> = OnceLock::new();
    let decoded_entities = quick_xml::escape::unescape(value)
        .map(|value| value.into_owned())
        .unwrap_or_else(|_| value.to_string());
    let without_tags = TAGS
        .get_or_init(|| Regex::new(r"(?is)<[^>]*>").unwrap())
        .replace_all(&decoded_entities, " ");
    let decoded = quick_xml::escape::unescape(&without_tags)
        .map(|value| value.into_owned())
        .unwrap_or_else(|_| without_tags.into_owned());
    static PUNCT: OnceLock<Regex> = OnceLock::new();
    let collapsed = SPACE
        .get_or_init(|| Regex::new(r"\s+").unwrap())
        .replace_all(&decoded, " ");
    // 去标签会在 `<b>词</b>.` 这类位置留下一个空格，读起来像断句错误。
    PUNCT
        .get_or_init(|| Regex::new(r"\s+([,.;:!?)\]}»。，、；：！？）】])").unwrap())
        .replace_all(&collapsed, "$1")
        .trim()
        .to_string()
}

pub(crate) fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (index, ch) in value.chars().enumerate() {
        if index >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

/// 还原引擎的跳转包装（DuckDuckGo 的 `uddg=`、Mojeek 的 `/out?u=`）。
fn unwrap_redirect_url(href: &str) -> Option<String> {
    let href = href.replace("&amp;", "&");
    let absolute = if href.starts_with("//") {
        format!("https:{href}")
    } else {
        href
    };
    if let Ok(url) = url::Url::parse(&absolute)
        && let Some((_, target)) = url
            .query_pairs()
            .find(|(name, _)| name == "uddg" || name == "u")
    {
        return Some(target.into_owned());
    }
    if absolute.starts_with("https://") || absolute.starts_with("http://") {
        return Some(absolute);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_brave_result_blocks() {
        let html = r#"
          <div class="snippet svelte-x" data-pos="1" data-type="web">
            <a href="https://example.com/a" class="l1">
              <div class="title search-snippet-title svelte-y">Example &amp; page</div>
            </a>
            <div class="generic-snippet svelte-z"><div class="content"><span>2026 -</span> A useful <b>result</b>.</div></div>
          </div>
          <div class="snippet svelte-x" data-pos="2" data-type="web">
            <a href="https://example.org/b" class="l1"><div class="title">Second</div></a>
          </div>
        "#;
        let hits = parse_brave(html, 5);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://example.com/a");
        assert_eq!(hits[0].title, "Example & page");
        assert!(hits[0].snippet.contains("A useful result."), "{:?}", hits[0]);
        assert_eq!(hits[1].url, "https://example.org/b");
    }

    #[test]
    fn parses_and_unwraps_duckduckgo_results() {
        let html = r#"
          <div class="result results_links">
            <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fx%3D1&amp;rut=abc"><b>Example</b> &amp; result</a>
            <a class="result__snippet">A useful <b>search</b> result.</a>
          </div>
        "#;
        assert_eq!(
            parse_duckduckgo(html, 5),
            vec![SearchHit {
                title: "Example & result".to_string(),
                url: "https://example.com/a?x=1".to_string(),
                snippet: "A useful search result.".to_string(),
            }]
        );
    }

    #[test]
    fn parses_bing_rss_results() {
        let xml = r#"<rss><channel><item><title>Rust &amp; tools</title><link>https://example.com/rust</link><description>Fast &lt;b&gt;agent&lt;/b&gt; runtime.</description></item></channel></rss>"#;
        assert_eq!(
            parse_bing_rss(xml, 5),
            vec![SearchHit {
                title: "Rust & tools".to_string(),
                url: "https://example.com/rust".to_string(),
                snippet: "Fast agent runtime.".to_string(),
            }]
        );
    }

    #[test]
    fn consensus_outranks_single_engine_top_hit() {
        let first = vec![
            SearchHit { title: "solo".into(), url: "https://solo.example/".into(), snippet: String::new() },
            SearchHit { title: "shared".into(), url: "https://shared.example/page".into(), snippet: "short".into() },
        ];
        let second = vec![SearchHit {
            title: "shared".into(),
            url: "https://www.shared.example/page/".into(),
            snippet: "a much longer snippet".into(),
        }];
        let merged = merge_hits([first.as_slice(), second.as_slice()].into_iter(), 10);
        // 共识条目排到首位，展示地址取自把它排得最靠前的那家引擎。
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].title, "shared");
        assert_eq!(merged[0].url, "https://www.shared.example/page/");
        assert_eq!(merged[0].snippet, "a much longer snippet");
        assert_eq!(merged[1].url, "https://solo.example/");
    }

    #[test]
    fn dedup_key_ignores_www_case_and_trailing_slash() {
        assert_eq!(
            dedup_key("https://WWW.Example.com/a/"),
            dedup_key("https://example.com/a")
        );
    }

    #[tokio::test]
    #[ignore = "需要访问公共搜索引擎"]
    async fn live_search_returns_results() {
        let outcome = search_web("Rust async runtime", 5).await.unwrap();
        println!(
            "有结果的引擎：{:?}\n失败：{:?}",
            outcome.engines, outcome.failures
        );
        for hit in &outcome.hits {
            println!("- {} | {}\n  {}", hit.title, hit.url, hit.snippet);
        }
        assert!(!outcome.hits.is_empty(), "{outcome:?}");
    }
}
