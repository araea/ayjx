//! 联网搜索支持。
//!
//! 搜索不属于 OpenAI 兼容协议，所以这里绕开 `async-openai` 自己发请求：
//! 走桥接的 DeepSeek APP 接口时，请求体里多一个 `search: true` 就开启服务端检索，
//! 答案正文带 `[citation:N]` 标记，来源放在响应的 `search_results` 数组里
//! （`cite_index` 与正文的 N 对应）。
//!
//! `async-openai` 的 `CreateChatCompletionRequest` 没有自定义字段的口子，
//! 而为了一个布尔值去手写整套消息类型不划算——直接把已构造好的消息序列化出去。

use super::types::Source;
use async_openai::types::chat::ChatCompletionRequestMessage;
use regex::Regex;
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// 正文里的引用标记，形如 `[citation:3]`。
fn citation_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[citation:(\d+)\]").expect("引用标记正则字面量合法"))
}

/// 一次带搜索的回复：正文里的引用标记已改写成来源序号。
pub(super) struct SearchReply {
    pub(super) text: String,
    pub(super) sources: Vec<Source>,
}

/// 发一次带 `search: true` 的补全请求。
pub(super) async fn complete(
    base: &str,
    api_key: &str,
    model: &str,
    messages: Vec<ChatCompletionRequestMessage>,
    thinking: Option<&str>,
) -> anyhow::Result<SearchReply> {
    let url = format!("{}/chat/completions", base.trim_end_matches('/'));
    let mut body = serde_json::json!({
        "model": model,
        "messages": serde_json::to_value(&messages)?,
        "search": true,
    });
    if let Some(effort) = thinking.and_then(super::logic::reasoning_effort) {
        body["reasoning_effort"] = serde_json::to_value(effort)?;
    }

    let resp = crate::http::client()
        .post(&url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("搜索请求失败: {e}"))?;

    let status = resp.status();
    let payload: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("搜索响应解析失败 ({status}): {e}"))?;

    if !status.is_success() {
        let detail = payload
            .pointer("/error/message")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| payload.to_string());
        let detail: String = detail.chars().take(200).collect();
        anyhow::bail!("搜索接口返回 {status}：{detail}");
    }

    let choice = payload
        .pointer("/choices/0/message")
        .ok_or_else(|| anyhow::anyhow!("搜索接口未返回任何候选回复"))?;
    let raw_text = choice
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if raw_text.trim().is_empty() {
        anyhow::bail!("搜索接口返回了空回复");
    }

    let results = payload.get("search_results").and_then(|v| v.as_array());
    Ok(resolve_citations(raw_text, results))
}

/// 把正文里的 `[citation:N]` 换成`[1]`这种顺序号，并挑出真正被引用的来源。
///
/// 只保留**正文真的引用过**的来源：检索回来的页面常常远多于引用数
/// （实测取回 12 条、正文只引 4 条），全列出来既占版面又对不上号。
/// 序号按 `cite_index` 升序重排，保证卡片里第 k 条正好对应正文的 `[k]`。
fn resolve_citations(text: &str, results: Option<&Vec<serde_json::Value>>) -> SearchReply {
    let mut by_index: BTreeMap<u64, Source> = BTreeMap::new();
    if let Some(items) = results {
        for item in items {
            let Some(index) = item.get("cite_index").and_then(|v| v.as_u64()) else {
                continue;
            };
            let url = item.get("url").and_then(|v| v.as_str()).unwrap_or_default();
            if url.is_empty() {
                continue;
            }
            let title = item
                .get("title")
                .and_then(|v| v.as_str())
                .filter(|t| !t.trim().is_empty())
                .unwrap_or(url);
            by_index.entry(index).or_insert_with(|| Source {
                title: title.to_string(),
                url: url.to_string(),
            });
        }
    }

    // 只认正文里出现过的引用，且按 cite_index 升序编号。
    let cited: Vec<u64> = {
        let mut seen: Vec<u64> = Vec::new();
        for cap in citation_re().captures_iter(text) {
            if let Some(index) = cap.get(1).and_then(|m| m.as_str().parse::<u64>().ok())
                && !seen.contains(&index)
            {
                seen.push(index);
            }
        }
        seen.sort_unstable();
        seen
    };

    let mut sources = Vec::new();
    let mut number_of: BTreeMap<u64, usize> = BTreeMap::new();
    for index in cited {
        if let Some(source) = by_index.get(&index) {
            sources.push(source.clone());
            number_of.insert(index, sources.len());
        }
    }

    // 重写标记：能对应上来源的换成 `[k]`，对不上的直接去掉（宁可没有，
    // 也不要让群里出现 `[citation:7]` 这种既难读又无解的东西）。
    let rewritten = citation_re()
        .replace_all(text, |caps: &regex::Captures| {
            caps.get(1)
                .and_then(|m| m.as_str().parse::<u64>().ok())
                .and_then(|index| number_of.get(&index))
                .map(|n| format!("[{n}]"))
                .unwrap_or_default()
        })
        .into_owned();

    SearchReply {
        text: rewritten,
        sources,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn results() -> Vec<serde_json::Value> {
        vec![
            json!({"cite_index": 1, "url": "https://a.example/x", "title": "甲"}),
            json!({"cite_index": 2, "url": "https://b.example/y", "title": "乙"}),
            json!({"cite_index": 3, "url": "https://c.example/z", "title": "丙"}),
        ]
    }

    #[test]
    fn renumbers_only_cited_sources_in_ascending_order() {
        let out = resolve_citations("结论[citation:3]与[citation:1]。", Some(&results()));
        assert_eq!(out.text, "结论[2]与[1]。");
        assert_eq!(out.sources.len(), 2);
        assert_eq!(out.sources[0].url, "https://a.example/x");
        assert_eq!(out.sources[1].url, "https://c.example/z");
    }

    #[test]
    fn repeated_citations_share_one_source() {
        let out = resolve_citations("[citation:2][citation:2]", Some(&results()));
        assert_eq!(out.text, "[1][1]");
        assert_eq!(out.sources.len(), 1);
        assert_eq!(out.sources[0].title, "乙");
    }

    #[test]
    fn citation_without_matching_result_is_dropped() {
        let out = resolve_citations("有[citation:9]和[citation:1]", Some(&results()));
        assert_eq!(out.text, "有和[1]");
        assert_eq!(out.sources.len(), 1);
    }

    #[test]
    fn no_results_leaves_no_markers() {
        let out = resolve_citations("没有来源[citation:1]", None);
        assert_eq!(out.text, "没有来源");
        assert!(out.sources.is_empty());
    }

    #[test]
    fn text_without_citations_is_untouched() {
        let out = resolve_citations("普通回答", Some(&results()));
        assert_eq!(out.text, "普通回答");
        assert!(out.sources.is_empty());
    }

    #[test]
    fn empty_title_falls_back_to_url() {
        let items = vec![json!({"cite_index": 1, "url": "https://d.example/p", "title": null})];
        let out = resolve_citations("[citation:1]", Some(&items));
        assert_eq!(out.sources[0].title, "https://d.example/p");
    }

    #[test]
    fn result_without_url_is_skipped() {
        let items = vec![json!({"cite_index": 1, "url": ""})];
        let out = resolve_citations("[citation:1]", Some(&items));
        assert_eq!(out.text, "");
        assert!(out.sources.is_empty());
    }

    /// 打真实桥接的端到端检查：`dsapi start` 之后手动跑
    /// `cargo test --offline --bin ayjx live_bridge -- --ignored --nocapture`。
    ///
    /// 靠真实接口才能发现的东西——响应字段路径、`search` 开关是否被认、
    /// `reasoning_effort` 的序列化形态——单测都覆盖不到。
    #[tokio::test]
    #[ignore = "需要本机 ~/dev/deepseek-api 的 dsapi 桥接正在运行"]
    async fn live_bridge_returns_citations_and_sources() {
        // 关思考走 `[citation:N]`，开思考走上游的深搜 `[reference:N]`；桥接把两者
        // 都归一成 `[citation:N]` + `cite_index`，两条都验一遍。
        live_case(None).await;
        live_case(Some("high")).await;
    }

    async fn live_case(thinking: Option<&str>) {
        use async_openai::types::chat::ChatCompletionRequestUserMessageArgs;

        let base = std::env::var("DSAPP_BASE").unwrap_or("http://127.0.0.1:9000/v1".into());
        let key = std::env::var("DSAPP_KEY")
            .unwrap_or_else(|_| std::fs::read_to_string("/data/data/com.termux/files/home/dev/deepseek-api/apikey.txt").unwrap_or_default())
            .trim()
            .to_string();
        assert!(!key.is_empty(), "缺少 apikey：设 DSAPP_KEY 或先 dsapi start");

        let msgs = vec![
            ChatCompletionRequestUserMessageArgs::default()
                .content("今天的国际金价是多少？")
                .build()
                .unwrap()
                .into(),
        ];
        let reply = complete(&base, &key, "deepseek-chat", msgs, thinking)
            .await
            .expect("搜索请求应当成功");

        println!("--- 正文 ---\n{}", reply.text);
        println!("--- 来源 ({}) ---", reply.sources.len());
        for (i, s) in reply.sources.iter().enumerate() {
            println!("[{}] {} — {}", i + 1, s.title, s.url);
        }

        assert!(!reply.text.trim().is_empty(), "正文不应为空");
        assert!(!reply.sources.is_empty(), "问实时问题应当带回来源");
        // 标记必须已改写干净：两种上游写法都不该残留，也不该出现越界序号。
        assert!(
            !reply.text.contains("[citation:") && !reply.text.contains("[reference:"),
            "原始引用标记应已改写：{}",
            reply.text
        );
        // 改写后是 `[k]` 这种纯数字序号，用另一个正则校验它落在来源范围内。
        let numbered = Regex::new(r"\[(\d+)\]").expect("序号正则字面量合法");
        let max = reply.sources.len();
        let mut seen = 0;
        for cap in numbered.captures_iter(&reply.text) {
            let n: usize = cap[1].parse().unwrap();
            assert!((1..=max).contains(&n), "序号 {n} 超出来源数 {max}");
            seen += 1;
        }
        assert!(seen > 0, "正文里应当至少有一个来源序号");
    }
}
