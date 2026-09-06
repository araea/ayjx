use super::data::Manager;
use super::parser::{Action, Command, Scope};
use super::types::{Agent, ChatMessage};
use super::utils::{escape_markdown_special, format_export_txt, format_history};
use crate::adapters::satori::{LockedWriter, api, send_msg};
use crate::event::{Context, MessageEvent};
use crate::message::Message;
use async_openai::{
    Client,
    config::OpenAIConfig,
    types::chat::{
        ChatCompletionMessageToolCalls, ChatCompletionRequestAssistantMessageArgs,
        ChatCompletionRequestMessage,
        ChatCompletionRequestMessageContentPartImageArgs,
        ChatCompletionRequestMessageContentPartTextArgs, ChatCompletionRequestSystemMessageArgs,
        ChatCompletionRequestToolMessageArgs, ChatCompletionRequestUserMessageArgs,
        CreateChatCompletionRequest, CreateChatCompletionRequestArgs, ImageUrlArgs,
    },
};
use regex::Regex;
use std::{fs::File, io::Write, sync::Arc};

pub(crate) async fn reply_text(
    ctx: &Context,
    writer: &LockedWriter,
    event: &MessageEvent<'_>,
    text: impl Into<String>,
) {
    let msg = Message::new().reply(event.message_id()).text(text.into());
    let _ = send_msg(
        ctx,
        writer.clone(),
        event.group_id(),
        Some(event.user_id()),
        msg,
    )
    .await;
}

async fn reply(
    ctx: &Context,
    writer: &LockedWriter,
    event: &MessageEvent<'_>,
    text: &str,
    text_mode: bool,
    header: &str,
) {
    reply_card(ctx, writer, event, text, text_mode, header, &[], None).await;
}

/// 把回复渲染成卡片图片发出；`text_mode` 或渲染失败时退回纯文本。
#[allow(clippy::too_many_arguments)]
async fn reply_card(
    ctx: &Context,
    writer: &LockedWriter,
    event: &MessageEvent<'_>,
    text: &str,
    text_mode: bool,
    header: &str,
    sources: &[super::agent::Source],
    footer: Option<String>,
) {
    let msg = Message::new().reply(event.message_id());

    if text_mode {
        let _ = send_msg(
            ctx,
            writer.clone(),
            event.group_id(),
            Some(event.user_id()),
            msg.text(text),
        )
        .await;
        return;
    }

    let card = super::render::Card {
        title: header,
        markdown: text,
        sources,
        footer,
    };
    match super::render::render_card(card).await {
        Ok(b64) => {
            let _ = send_msg(
                ctx,
                writer.clone(),
                event.group_id(),
                Some(event.user_id()),
                msg.image(format!("base64://{}", b64)),
            )
            .await;
        }
        Err(error) => {
            warn!(target: "Plugin/OAI", "回复卡片渲染失败，退回纯文本：{error:#}");
            let re = Regex::new(r"!\[.*?\]\((data:image/[^\s\)]+)\)").unwrap();
            let clean_text = re.replace_all(text, "[图片渲染失败]").to_string();
            let _ = send_msg(
                ctx,
                writer.clone(),
                event.group_id(),
                Some(event.user_id()),
                msg.text(&clean_text),
            )
            .await;
        }
    }
}

fn extract_image_urls(content: &str) -> Vec<String> {
    let re = Regex::new(r"!\[.*?\]\(((?:https?://|data:image/)[^\s\)]+)\)|(?:https?://[^\s]+\.(?:png|jpg|jpeg|gif|webp|bmp))").unwrap();
    let mut urls: Vec<String> = re
        .captures_iter(content)
        .filter_map(|cap| cap.get(1).or(cap.get(0)).map(|m| m.as_str().to_string()))
        .collect();
    let mut seen = std::collections::HashSet::new();
    urls.retain(|url| seen.insert(url.clone()));
    urls
}

fn extract_video_urls(content: &str) -> Vec<String> {
    let re = Regex::new(r"\[download video\]\((https?://[^\s\)]+)\)").unwrap();
    re.captures_iter(content)
        .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_string()))
        .collect()
}

/// 把图片地址转成多模态模型可内联的 data URL。
/// 已经是 `data:` 的保持不变；其余（QQ 图片 / 头像等远程 URL）下载后转 base64，
/// 避免 QQ 图片防盗链导致服务端拉不到图。下载失败则回退原 URL，由服务端自行尝试。
pub(crate) async fn to_data_url(url: &str) -> String {
    if url.starts_with("data:") {
        return url.to_string();
    }
    // 历史里的图片每一轮都会重新入参，没有缓存就意味着每轮重下一遍：多图会话里
    // 这部分等待往往比模型本身还久。
    if let Some(cached) = cached_data_url(url) {
        return cached;
    }
    match download_image_to_data_url(url).await {
        Some(data_url) => {
            remember_data_url(url, &data_url);
            data_url
        }
        None => url.to_string(),
    }
}

/// 远程图片 → data URL 的进程内缓存。
///
/// QQ 图片链接带签名且很快失效，缓存同时兼作「这张图还能用」的保底：链接过期后
/// 历史里的图仍然可以继续参与对话。容量到顶时整体清空——命中率远比精确淘汰重要，
/// 也省下维护 LRU 链表的复杂度。
fn data_url_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, String>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 缓存条目上限，按每张图数 MB 的量级留出余量。
const DATA_URL_CACHE_CAPACITY: usize = 64;

fn cached_data_url(url: &str) -> Option<String> {
    data_url_cache().lock().ok()?.get(url).cloned()
}

fn remember_data_url(url: &str, data_url: &str) {
    if let Ok(mut cache) = data_url_cache().lock() {
        if cache.len() >= DATA_URL_CACHE_CAPACITY {
            cache.clear();
        }
        cache.insert(url.to_string(), data_url.to_string());
    }
}

async fn download_image_to_data_url(url: &str) -> Option<String> {
    const MAX_BYTES: usize = 20 * 1024 * 1024;

    let resp = crate::http::client()
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36",
        )
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let bytes = resp.bytes().await.ok()?;
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return None;
    }

    let mime = content_type
        .filter(|ct| ct.starts_with("image/"))
        .unwrap_or_else(|| sniff_image_mime(&bytes).to_string());

    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Some(format!("data:{};base64,{}", mime, b64))
}

fn sniff_image_mime(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else if bytes.starts_with(b"BM") {
        "image/bmp"
    } else {
        "image/jpeg"
    }
}

/// 执行标准的模型 → tool calls → tool results → 模型循环。
///
/// 和 pi-agent / Rig 的手动工具循环一致，只把最终自然语言写入房间历史；中间的
/// assistant/tool 消息仅属于本次推理，避免把可能很大的终端输出永久落盘。
fn build_chat_request(
    model: &str,
    messages: Vec<ChatCompletionRequestMessage>,
    harness: Option<super::harness::HarnessConfig>,
) -> anyhow::Result<CreateChatCompletionRequest> {
    let mut builder = CreateChatCompletionRequestArgs::default();
    builder.model(model).messages(messages);
    if let Some(harness) = harness {
        // Chat Completions 的工具支持是跨模型/中转的公共能力，而 reasoning_effort
        // 属于模型和端点相关的可选扩展。不要把后者附加到工具请求：部分服务（如
        // Luna）只在 Responses API 支持两者组合，且同一模型在不同中转上的行为也
        // 可能不同。让服务端采用该模型的默认推理策略是最可移植的请求契约。
        builder.tools(super::harness::chat_tool_definitions(harness));
    }
    Ok(builder.build()?)
}

async fn complete(
    client: &Client<OpenAIConfig>,
    model: &str,
    mut messages: Vec<ChatCompletionRequestMessage>,
    harness: Option<super::harness::HarnessConfig>,
) -> anyhow::Result<String> {
    for _ in 0..super::harness::MAX_TOOL_ROUNDS {
        let request = build_chat_request(model, messages.clone(), harness)?;
        let response = client.chat().create(request).await?;
        let choice = response
            .choices
            .first()
            .ok_or_else(|| anyhow::anyhow!("API 未返回任何候选回复"))?;
        let tool_calls = choice.message.tool_calls.clone().unwrap_or_default();

        if tool_calls.is_empty() {
            return choice
                .message
                .content
                .clone()
                .filter(|content| !content.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("API 返回了空回复"));
        }

        let harness = harness.ok_or_else(|| anyhow::anyhow!("模型请求了未启用的工具"))?;
        let mut assistant = ChatCompletionRequestAssistantMessageArgs::default();
        assistant.tool_calls(tool_calls.clone());
        if let Some(content) = choice.message.content.clone().filter(|value| !value.is_empty()) {
            assistant.content(content);
        }
        messages.push(assistant.build()?.into());

        let executions = tool_calls.iter().map(|call| async move {
            match call {
                ChatCompletionMessageToolCalls::Function(call) => {
                    let run = super::harness::execute_tool(
                        &call.function.name,
                        &call.function.arguments,
                        harness,
                    )
                    .await;
                    (call.id.clone(), run.output)
                }
                ChatCompletionMessageToolCalls::Custom(call) => (
                    call.id.clone(),
                    format!("Tool error: unsupported custom tool {}", call.custom_tool.name),
                ),
            }
        });
        for (call_id, output) in futures_util::future::join_all(executions).await {
            messages.push(
                ChatCompletionRequestToolMessageArgs::default()
                    .tool_call_id(call_id)
                    .content(output)
                    .build()?
                    .into(),
            );
        }
    }
    anyhow::bail!(
        "工具调用超过 {} 轮，已停止以避免无限循环",
        super::harness::MAX_TOOL_ROUNDS
    )
}

#[allow(clippy::too_many_arguments)]
async fn chat(
    name: &str,
    prompt: &str,
    imgs: Vec<String>,
    regen: bool,
    cmd: &Command,
    ctx: &Context,
    writer: &LockedWriter,
    mgr: &Arc<Manager>,
) {
    let event = match ctx.as_message() {
        Some(e) => e,
        None => return,
    };
    let (agent, api) = {
        let c = mgr.config.read().await;
        let a = c.agents.iter().find(|a| a.name == name).cloned();
        (a, (c.api_base.clone(), c.api_key.clone()))
    };

    let agent = match agent {
        Some(a) => a,
        None => {
            reply_text(ctx, writer, &event, format!("❌ 智能体 {} 不存在", name)).await;
            return;
        }
    };

    if super::mj::is_mj_model(&agent.model) {
        // MJ 房间天生是无历史任务流；引用文字也不应混入绘图提示词。
        super::mj::handle_agent(&agent, &cmd.args, imgs, ctx, writer, mgr).await;
        return;
    }

    let is_priv_ctx = cmd.private_reply;
    let uid = event.user_id().to_string();
    let temp_mode = cmd.temp_mode;

    if !temp_mode {
        let generating = mgr.generating.read().await;
        if generating.is_generating(name, is_priv_ctx, &uid) {
            reply_text(
                ctx,
                writer,
                &event,
                "⏳ 正在生成中，请等待，或使用「智能体!」停止。",
            )
            .await;
            return;
        }
    }

    if api.0.is_empty() || api.1.is_empty() {
        reply_text(ctx, writer, &event, "❌ API 未配置。").await;
        return;
    }

    let mut hist = if temp_mode {
        Vec::new()
    } else {
        agent.history(is_priv_ctx, &uid).to_vec()
    };

    if regen {
        if hist.last().map(|m| m.role == "assistant").unwrap_or(false) {
            hist.pop();
        }
        if !prompt.is_empty() {
            if hist.last().map(|m| m.role == "user").unwrap_or(false) {
                hist.pop();
            }
            hist.push(ChatMessage::new("user", prompt, imgs.clone()));
        }
    } else {
        if prompt.is_empty() && imgs.is_empty() {
            reply_text(ctx, writer, &event, "💬 请输入内容。").await;
            return;
        }
        hist.push(ChatMessage::new("user", prompt, imgs.clone()));
    }

    let gen_id = if temp_mode {
        0
    } else {
        let mut c = mgr.config.write().await;
        if let Some(a) = c.agents.iter_mut().find(|a| a.name == name) {
            *a.history_mut(is_priv_ctx, &uid) = hist.clone();
            a.generation_id += 1;
            let id = a.generation_id;
            mgr.save(&c);
            id
        } else {
            return;
        }
    };

    if !temp_mode {
        let mut generating = mgr.generating.write().await;
        generating.set_generating(name, is_priv_ctx, &uid, true);
    }

    let api_base = super::utils::openai_api_base(&api.0);
    let client = Client::with_config(
        OpenAIConfig::new()
            .with_api_base(api_base.clone())
            .with_api_key(api.1.clone()),
    )
    .with_http_client(crate::http::client());

    let oai = crate::plugins::get_config_or_default::<super::OaiConfig>(ctx, "oai");
    let harness = oai.harness_for(name, is_priv_ctx);
    let annotate = !event.is_manual_self();
    if annotate {
        let _ = api::set_msg_emoji_like(ctx, writer.clone(), event.message_id(), 124, true).await;
    }

    let started = std::time::Instant::now();
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();

    // 三件事在同一个任务里赛跑：出结果、到总预算、到进度提示时刻。用 select 而不是
    // 另起任务，是因为发消息要用借来的 ctx/event，搬进 spawn 就得整套克隆一遍。
    let work = respond(
        &client,
        &agent,
        &hist,
        harness,
        &oai,
        &api_base,
        &api.1,
        name,
        progress_tx,
    );
    let mut work = std::pin::pin!(work);
    let mut budget = std::pin::pin!(tokio::time::sleep(oai.request_timeout()));
    let notice_delay = harness.and_then(|_| oai.progress_notice());
    let mut notice = std::pin::pin!(async move {
        match notice_delay {
            Some(delay) => tokio::time::sleep(delay).await,
            None => std::future::pending::<()>().await,
        }
    });
    let mut noticed = false;

    let outcome = loop {
        tokio::select! {
            result = &mut work => break Some(result),
            _ = &mut budget => break None,
            _ = &mut notice, if !noticed => {
                noticed = true;
                let mut done = Vec::new();
                while let Ok(progress) = progress_rx.try_recv() {
                    done.push(match progress {
                        super::agent::Progress::HostedSearch(query) => format!("搜索「{query}」"),
                        super::agent::Progress::Tool(summary) => summary,
                    });
                }
                let detail = if done.is_empty() {
                    "正在思考".to_string()
                } else {
                    format!("已完成 {}", done.join("、"))
                };
                reply_text(
                    ctx,
                    writer,
                    &event,
                    format!("⏳ 还在处理（{detail}），稍等一下…"),
                )
                .await;
            }
        }
    };

    if !temp_mode {
        mgr.generating
            .write()
            .await
            .set_generating(name, is_priv_ctx, &uid, false);
    }

    match outcome {
        None => {
            reply_text(
                ctx,
                writer,
                &event,
                format!(
                    "⏳ 请求超时：模型响应超过 {} 秒，已强制停止。",
                    oai.request_timeout().as_secs()
                ),
            )
            .await;
        }
        Some(Err(error)) => {
            reply_text(ctx, writer, &event, format!("❌ API 错误：{error:#}")).await;
        }
        Some(Ok(reply_data)) => {
            let content = reply_data.text;

            // 期间被「智能体!」打断或历史被改写过，这次结果就作废。
            if !temp_mode {
                let stale = {
                    let c = mgr.config.read().await;
                    c.agents
                        .iter()
                        .find(|a| a.name == name)
                        .is_some_and(|a| a.generation_id != gen_id)
                };
                if stale {
                    if annotate {
                        let _ = api::set_msg_emoji_like(
                            ctx,
                            writer.clone(),
                            event.message_id(),
                            124,
                            false,
                        )
                        .await;
                    }
                    return;
                }
            }

            let msg_index = if temp_mode {
                0
            } else {
                let c = mgr.config.read().await;
                c.agents
                    .iter()
                    .find(|a| a.name == name)
                    .map(|a| a.history(is_priv_ctx, &uid).len() + 1)
                    .unwrap_or(0)
            };

            if !temp_mode {
                let mut c = mgr.config.write().await;
                if let Some(a) = c.agents.iter_mut().find(|a| a.name == name) {
                    a.history_mut(is_priv_ctx, &uid)
                        .push(ChatMessage::new("assistant", &content, vec![]));
                }
                mgr.save(&c);
            }

            let image_urls = extract_image_urls(&content);
            let header = if temp_mode {
                format!("{} (临时会话)", agent.name)
            } else {
                format!(
                    "{} #{}回复{}",
                    agent.name,
                    msg_index,
                    if cmd.private_reply { " (私有)" } else { "" }
                )
            };

            let display_content = if !image_urls.is_empty() && !cmd.text_mode {
                let urls_text = image_urls
                    .iter()
                    .map(|u| {
                        if u.starts_with("data:") {
                            "- [Base64 Image]".to_string()
                        } else {
                            format!("- {}", u)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("{}\n\n---\n**图片链接：**\n{}", content, urls_text)
            } else {
                content.clone()
            };

            let reply_text_content = if cmd.text_mode && !image_urls.is_empty() {
                let re = Regex::new(r"!\[.*?\]\(((?:https?://|data:image/)[^\s\)]+)\)").unwrap();
                re.replace_all(&content, |caps: &regex::Captures| {
                    let url = &caps[1];
                    if url.starts_with("data:") {
                        "[图片]".to_string()
                    } else {
                        url.to_string()
                    }
                })
                .to_string()
            } else {
                display_content.clone()
            };

            // 一两句话没必要走一次浏览器截图：文本更快，也方便直接复制。
            let plain = cmd.text_mode
                || (image_urls.is_empty()
                    && reply_data.sources.is_empty()
                    && is_plain_enough(&content, oai.plain_text_max_chars()));
            let footer = (oai.show_trace_footer() && !plain).then(|| {
                let mut footer = format!(
                    "{} · {}",
                    agent.model,
                    super::agent::format_elapsed(started)
                );
                if !reply_data.trace.is_empty() {
                    footer.push_str(" · ");
                    footer.push_str(&reply_data.trace.join(" | "));
                }
                footer
            });

            reply_card(
                ctx,
                writer,
                &event,
                &reply_text_content,
                plain,
                &header,
                &reply_data.sources,
                footer,
            )
            .await;

            for url in &image_urls {
                if url.starts_with("data:") {
                    if let Some(base64_data) = url.split(',').nth(1) {
                        let _ = send_msg(
                            ctx,
                            writer.clone(),
                            event.group_id(),
                            Some(event.user_id()),
                            Message::new().image(format!("base64://{}", base64_data)),
                        )
                        .await;
                    }
                } else {
                    let _ = send_msg(
                        ctx,
                        writer.clone(),
                        event.group_id(),
                        Some(event.user_id()),
                        Message::new().image(url),
                    )
                    .await;
                }
            }

            for url in extract_video_urls(&content) {
                let _ = send_msg(
                    ctx,
                    writer.clone(),
                    event.group_id(),
                    Some(event.user_id()),
                    Message::new().video(url),
                )
                .await;
            }
        }
    }

    if annotate {
        let _ = api::set_msg_emoji_like(ctx, writer.clone(), event.message_id(), 124, false).await;
    }
}

/// 一次成功回复的产物。
struct Reply {
    text: String,
    sources: Vec<super::agent::Source>,
    trace: Vec<String>,
}

/// 选择请求链路并取回最终文本。
///
/// 工具房间优先走 Responses：托管检索、推理档位与跨轮推理态都只在那条路上可用。
/// 端点没有实现 `/responses` 时回落到 Chat Completions，用户侧无感。
#[allow(clippy::too_many_arguments)]
async fn respond(
    client: &Client<OpenAIConfig>,
    agent: &Agent,
    hist: &[ChatMessage],
    harness: Option<super::harness::HarnessConfig>,
    oai: &super::OaiConfig,
    api_base: &str,
    api_key: &str,
    room: &str,
    progress: tokio::sync::mpsc::UnboundedSender<super::agent::Progress>,
) -> anyhow::Result<Reply> {
    if let Some(harness) = harness {
        let request = super::agent::AgentRequest {
            api_base: api_base.to_string(),
            api_key: api_key.to_string(),
            model: agent.model.clone(),
            instructions: super::agent::build_instructions(
                &agent.system_prompt,
                harness.hosted_web_search,
                room,
            ),
            input: build_agent_input(hist).await,
            harness,
            reasoning_effort: oai.effort(),
            cache_key: Some(format!("ayjx-oai:{room}")),
            progress: Some(progress),
        };
        match super::agent::run(request).await {
            Ok(outcome) => {
                return Ok(Reply {
                    text: outcome.text,
                    sources: outcome.sources,
                    trace: outcome.trace,
                });
            }
            Err(super::agent::AgentError::Unsupported(reason)) => {
                warn!(target: "Plugin/OAI", "Responses 不可用，回落到 Chat Completions：{reason}");
            }
            Err(error) => return Err(anyhow::anyhow!("{error}")),
        }
    }

    let msgs = build_chat_messages(agent, hist).await;
    Ok(Reply {
        text: complete(client, &agent.model, msgs, harness).await?,
        sources: Vec::new(),
        trace: Vec::new(),
    })
}

/// 把房间历史转成 Responses 输入项。系统提示走 `instructions`，不占输入位。
async fn build_agent_input(hist: &[ChatMessage]) -> Vec<serde_json::Value> {
    let re = Regex::new(r"!\[.*?\]\((data:image/[^\s\)]+)\)").unwrap();
    let mut items = Vec::with_capacity(hist.len());
    for message in hist {
        match message.role.as_str() {
            "user" => {
                let images = resolve_images(&message.images).await;
                if let Some(item) = super::agent::user_item(&message.content, &images) {
                    items.push(item);
                }
            }
            "assistant" => {
                // 历史里的 base64 图片重放一遍只会撑爆上下文，留个占位即可。
                let clean = re.replace_all(&message.content, "[Image Created]");
                if let Some(item) = super::agent::assistant_item(&clean) {
                    items.push(item);
                }
            }
            _ => {}
        }
    }
    items
}

/// 把房间历史转成 Chat Completions 消息。
async fn build_chat_messages(
    agent: &Agent,
    hist: &[ChatMessage],
) -> Vec<ChatCompletionRequestMessage> {
    let mut msgs: Vec<ChatCompletionRequestMessage> = Vec::new();

    // 少数图像模型不接受 system 角色，只能把提示词并进首条用户消息。
    let model_lower = agent.model.to_lowercase();
    let force_user_role_for_system = [
        "nano-banana",
        "gemini-2.5-flash-image",
        "gemini-3-pro-image",
    ]
    .iter()
    .any(|kw| model_lower.contains(kw));

    let mut pending_sys_prompt = (!agent.system_prompt.is_empty()).then(|| agent.system_prompt.clone());

    if !force_user_role_for_system
        && let Some(sp) = pending_sys_prompt.take()
    {
        msgs.push(
            ChatCompletionRequestSystemMessageArgs::default()
                .content(sp)
                .build()
                .unwrap()
                .into(),
        );
    }

    let re = Regex::new(r"!\[.*?\]\((data:image/[^\s\)]+)\)").unwrap();
    for m in hist {
        if m.role == "user" {
            let mut parts = Vec::new();

            if let Some(sp) = pending_sys_prompt.take() {
                parts.push(
                    ChatCompletionRequestMessageContentPartTextArgs::default()
                        .text(sp)
                        .build()
                        .unwrap()
                        .into(),
                );
            }

            if !m.content.is_empty() {
                parts.push(
                    ChatCompletionRequestMessageContentPartTextArgs::default()
                        .text(m.content.clone())
                        .build()
                        .unwrap()
                        .into(),
                );
            }
            for data_url in resolve_images(&m.images).await {
                parts.push(
                    ChatCompletionRequestMessageContentPartImageArgs::default()
                        .image_url(ImageUrlArgs::default().url(data_url).build().unwrap())
                        .build()
                        .unwrap()
                        .into(),
                );
            }
            if parts.is_empty() {
                continue;
            }
            msgs.push(
                ChatCompletionRequestUserMessageArgs::default()
                    .content(parts)
                    .build()
                    .unwrap()
                    .into(),
            );
        } else if m.role == "assistant" {
            let clean_content = re.replace_all(&m.content, "[Image Created]").to_string();
            msgs.push(
                ChatCompletionRequestAssistantMessageArgs::default()
                    .content(clean_content)
                    .build()
                    .unwrap()
                    .into(),
            );
            let gen_imgs = extract_image_urls(&m.content);
            if !gen_imgs.is_empty() {
                let mut img_parts = Vec::new();
                for url in gen_imgs {
                    img_parts.push(
                        ChatCompletionRequestMessageContentPartImageArgs::default()
                            .image_url(ImageUrlArgs::default().url(url).build().unwrap())
                            .build()
                            .unwrap()
                            .into(),
                    );
                }
                msgs.push(
                    ChatCompletionRequestUserMessageArgs::default()
                        .content(img_parts)
                        .build()
                        .unwrap()
                        .into(),
                );
            }
        }
    }

    if let Some(sp) = pending_sys_prompt {
        msgs.push(
            ChatCompletionRequestUserMessageArgs::default()
                .content(sp)
                .build()
                .unwrap()
                .into(),
        );
    }
    msgs
}

/// 并发解析一条消息里的全部图片地址。
///
/// 逐张串行下载会让多图历史在每一轮都白等一次网络往返；缓存则让同一张图在整个
/// 会话里只下载一次。
async fn resolve_images(urls: &[String]) -> Vec<String> {
    futures_util::future::join_all(urls.iter().map(|url| to_data_url(url))).await
}

/// 短回复能否直接以纯文本发送。
///
/// 只要出现标题、列表、表格、代码块或链接，排版就有信息量，仍旧渲染卡片。
fn is_plain_enough(text: &str, max_chars: usize) -> bool {
    if max_chars == 0 || text.chars().count() > max_chars {
        return false;
    }
    if text.contains("](") || text.contains("```") || text.contains('|') {
        return false;
    }
    !text.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with('#')
            || line.starts_with("- ")
            || line.starts_with("* ")
            || line.starts_with("> ")
            || line
                .split_once(". ")
                .is_some_and(|(head, _)| !head.is_empty() && head.chars().all(|c| c.is_ascii_digit()))
    })
}

pub async fn execute(
    cmd: Command,
    prompt: String,
    imgs: Vec<String>,
    ctx: &Context,
    writer: &LockedWriter,
    mgr: &Arc<Manager>,
) {
    let msg_event = match ctx.as_message() {
        Some(e) => e,
        None => return,
    };
    let name = &cmd.agent;
    let uid = msg_event.user_id().to_string();

    match cmd.action {
        Action::UpdateApi(url, key) => {
            let url = super::utils::openai_api_base(&url);
            let mut c = mgr.config.write().await;
            c.api_base = url.clone();
            c.api_key = key;
            mgr.save(&c);
            drop(c);
            reply_text(ctx, writer, &msg_event, format!("✅ API 已配置：{}", url)).await;
            match mgr.fetch_models().await {
                Ok(models) => {
                    reply_text(
                        ctx,
                        writer,
                        &msg_event,
                        format!("📋 验证成功，已获取 {} 个模型。", models.len()),
                    )
                    .await
                }
                Err(e) => {
                    reply_text(ctx, writer, &msg_event, format!("⚠️ 获取模型失败：{}", e)).await
                }
            }
        }
        Action::Chat => {
            chat(name, &prompt, imgs, false, &cmd, ctx, writer, mgr).await;
        }
        Action::Regenerate => {
            chat(name, &cmd.args, imgs, true, &cmd, ctx, writer, mgr).await;
        }
        Action::Stop => {
            let is_priv_ctx = cmd.private_reply;
            {
                mgr.generating
                    .write()
                    .await
                    .set_generating(name, is_priv_ctx, &uid, false);
            }
            let mut c = mgr.config.write().await;
            if let Some(a) = c.agents.iter_mut().find(|a| a.name == *name) {
                a.generation_id += 1;
                mgr.save(&c);
                reply_text(ctx, writer, &msg_event, "🛑 已停止。").await;
            } else {
                reply_text(
                    ctx,
                    writer,
                    &msg_event,
                    format!("❌ 智能体 {} 不存在", name),
                )
                .await;
            }
        }
        Action::Copy => {
            if cmd.args.is_empty() {
                reply_text(ctx, writer, &msg_event, "❌ 请指定新名称：智能体~#新名称").await;
                return;
            }
            if cmd.args.chars().count() > 7
                || cmd.args.chars().any(|c| "&\"#~/ -_'!@$%:*".contains(c))
            {
                reply_text(
                    ctx,
                    writer,
                    &msg_event,
                    "❌ 名称限制：最多7字且不能包含指令符号",
                )
                .await;
                return;
            }
            let mut c = mgr.config.write().await;
            if c.agents.iter().any(|a| a.name == cmd.args) {
                reply_text(ctx, writer, &msg_event, format!("❌ {} 已存在", cmd.args)).await;
                return;
            }
            if let Some(src) = c.agents.iter().find(|a| a.name == *name).cloned() {
                let mut new_agent = Agent::new(
                    &cmd.args,
                    &src.model,
                    &src.system_prompt,
                    &format!("复制自 {}", name),
                );
                new_agent.description = src.description.clone();
                c.agents.push(new_agent);
                mgr.save(&c);
                reply_text(
                    ctx,
                    writer,
                    &msg_event,
                    format!("📑 已复制 {} → {}", name, cmd.args),
                )
                .await;
            } else {
                reply_text(ctx, writer, &msg_event, format!("❌ {} 不存在", name)).await;
            }
        }
        Action::Rename => {
            if cmd.args.is_empty() {
                reply_text(ctx, writer, &msg_event, "❌ 请指定新名称：智能体~=新名称").await;
                return;
            }
            if cmd.args.chars().count() > 7
                || cmd.args.chars().any(|c| "&\"#~/ -_'!@$%:*".contains(c))
            {
                reply_text(
                    ctx,
                    writer,
                    &msg_event,
                    "❌ 名称限制：最多7字且不能包含指令符号",
                )
                .await;
                return;
            }
            let mut c = mgr.config.write().await;
            if c.agents.iter().any(|a| a.name == cmd.args) {
                reply_text(
                    ctx,
                    writer,
                    &msg_event,
                    format!("❌ 目标名称 {} 已存在", cmd.args),
                )
                .await;
                return;
            }
            let idx_opt = c.agents.iter().position(|a| a.name == *name);
            if let Some(idx) = idx_opt {
                c.agents[idx].name = cmd.args.clone();
                mgr.save(&c);
                reply_text(
                    ctx,
                    writer,
                    &msg_event,
                    format!("🏷️ 已重命名 {} → {}", name, cmd.args),
                )
                .await;
            } else {
                reply_text(ctx, writer, &msg_event, format!("❌ {} 不存在", name)).await;
            }
        }
        Action::SetDesc => {
            if cmd.args.is_empty() {
                reply_text(ctx, writer, &msg_event, "❌ 请提供描述：智能体:描述内容").await;
                return;
            }
            let mut c = mgr.config.write().await;
            if let Some(a) = c.agents.iter_mut().find(|a| a.name == *name) {
                a.description = cmd.args.clone();
                mgr.save(&c);
                reply_text(ctx, writer, &msg_event, format!("📝 {} 描述已更新", name)).await;
            } else {
                reply_text(ctx, writer, &msg_event, format!("❌ {} 不存在", name)).await;
            }
        }
        Action::SetModel => {
            if cmd.args.is_empty() {
                reply_text(ctx, writer, &msg_event, "❌ 请指定模型：智能体%模型名").await;
                return;
            }
            let mut c = mgr.config.write().await;
            let models = c.models.clone();
            if let Some(model) = mgr.resolve_model(&cmd.args, &models) {
                if let Some(a) = c.agents.iter_mut().find(|a| a.name == *name) {
                    let old = a.model.clone();
                    a.model = model.clone();
                    mgr.save(&c);
                    reply_text(
                        ctx,
                        writer,
                        &msg_event,
                        format!("🔄 {} 模型：{} → {}", name, old, model),
                    )
                    .await;
                } else {
                    reply_text(ctx, writer, &msg_event, format!("❌ {} 不存在", name)).await;
                }
            } else {
                reply_text(ctx, writer, &msg_event, "❌ 无效模型。").await;
            }
        }
        Action::SetPrompt => {
            let mut c = mgr.config.write().await;
            if let Some(a) = c.agents.iter_mut().find(|a| a.name == *name) {
                a.system_prompt = cmd.args.clone();
                mgr.save(&c);
                if cmd.args.is_empty() {
                    reply_text(ctx, writer, &msg_event, format!("📝 {} 提示词已清空", name)).await;
                } else {
                    reply_text(ctx, writer, &msg_event, format!("📝 {} 提示词已更新", name)).await;
                }
            } else {
                reply_text(ctx, writer, &msg_event, format!("❌ {} 不存在", name)).await;
            }
        }
        Action::ViewPrompt => {
            let c = mgr.config.read().await;
            if let Some(a) = c.agents.iter().find(|a| a.name == *name) {
                if cmd.text_mode {
                    reply_text(ctx, writer, &msg_event, &a.system_prompt).await;
                    return;
                }
                let prompt_display = if a.system_prompt.is_empty() {
                    "(空)".to_string()
                } else {
                    escape_markdown_special(&a.system_prompt)
                };
                let content = format!(
                    "**模型**: `{}`\n\n**提示词**:\n```\n{}\n```",
                    a.model, prompt_display
                );
                reply(
                    ctx,
                    writer,
                    &msg_event,
                    &content,
                    cmd.text_mode,
                    &format!("{} 系统提示词", a.name),
                )
                .await;
            } else {
                reply_text(ctx, writer, &msg_event, format!("❌ {} 不存在", name)).await;
            }
        }
        Action::List => {
            let c = mgr.config.read().await;
            if c.agents.is_empty() {
                reply_text(
                    ctx,
                    writer,
                    &msg_event,
                    "📋 暂无智能体，使用 ##名称 模型 提示词 创建",
                )
                .await;
                return;
            }
            use std::collections::BTreeMap;
            let mut groups: BTreeMap<String, Vec<(usize, &Agent)>> = BTreeMap::new();
            for (i, a) in c.agents.iter().enumerate() {
                groups.entry(a.model.clone()).or_default().push((i + 1, a));
            }
            let mut html_parts = Vec::new();
            for (model, mut agents) in groups {
                agents.sort_by_key(|a| a.1.name.to_lowercase());
                html_parts.push(format!(r#"<div class="model-group"><div class="model-header"><span>📦 {}</span><span class="model-count">{}</span></div><div class="agent-grid">"#, model, agents.len()));
                for (real_idx, a) in agents {
                    let desc_display = if !a.description.is_empty() {
                        super::utils::truncate_str(&a.description, 20)
                    } else if !a.system_prompt.is_empty() {
                        super::utils::truncate_str(&a.system_prompt, 20)
                    } else {
                        "无描述".to_string()
                    };
                    html_parts.push(format!(r#"<div class="agent-mini"><div class="agent-mini-top"><div class="agent-idx">{}</div><div class="agent-mini-name">{}</div></div><div class="agent-mini-desc">{}</div></div>"#, real_idx, a.name, desc_display));
                }
                html_parts.push("</div></div>".to_string());
            }
            reply(
                ctx,
                writer,
                &msg_event,
                &html_parts.join("\n"),
                cmd.text_mode,
                &format!("📋 智能体列表 (共{}个)", c.agents.len()),
            )
            .await;
        }
        Action::Delete => {
            let mut c = mgr.config.write().await;
            if let Some(idx) = c.agents.iter().position(|a| a.name == *name) {
                c.agents.remove(idx);
                mgr.save(&c);
                reply_text(ctx, writer, &msg_event, format!("🗑️ 已删除 {}", name)).await;
            } else {
                reply_text(ctx, writer, &msg_event, format!("❌ {} 不存在", name)).await;
            }
        }
        Action::ListModels => {
            // 每次查看都强制刷新，确保能获取最新模型
            // 先发送提示，避免 API 响应慢导致用户以为无反应
            reply_text(ctx, writer, &msg_event, "⏳ 正在刷新模型列表...").await;

            // 尝试获取，如果失败则仅提示警告，后续继续尝试展示缓存
            if let Err(e) = mgr.fetch_models().await {
                reply_text(
                    ctx,
                    writer,
                    &msg_event,
                    format!("⚠️ 刷新失败，将展示缓存列表：{}", e),
                )
                .await;
            }

            let c = mgr.config.read().await;
            let models = &c.models;
            if models.is_empty() {
                reply_text(
                    ctx,
                    writer,
                    &msg_event,
                    "📭 未找到可用模型（请检查过滤关键字）",
                )
                .await;
                return;
            }

            use std::collections::HashMap;
            let mut usage_count = HashMap::new();
            for agent in &c.agents {
                *usage_count.entry(agent.model.clone()).or_insert(0) += 1;
            }

            let mut groups: HashMap<String, Vec<(usize, String)>> = HashMap::new();
            let mut other_models = Vec::new();
            for (i, m) in models.iter().enumerate() {
                let idx = i + 1;
                let lower = m.to_lowercase();
                let mut matched = false;
                for &kw in crate::plugins::oai::utils::MODEL_KEYWORDS {
                    if lower.contains(kw) {
                        let group_name = format!(
                            "{} Series",
                            kw.chars().next().unwrap().to_uppercase().to_string() + &kw[1..]
                        );
                        groups.entry(group_name).or_default().push((idx, m.clone()));
                        matched = true;
                        break;
                    }
                }
                if !matched {
                    other_models.push((idx, m.clone()));
                }
            }
            let mut html = String::new();
            let render_group = |title: &str, items: &Vec<(usize, String)>| -> String {
                let mut s = format!(
                    r#"<div class="mod-group"><div class="mod-title">{}</div><div class="chip-box">"#,
                    title
                );
                for (idx, name) in items {
                    let badge = if let Some(cnt) = usage_count.get(name) {
                        format!(r#"<span class="chip-bad">{}用</span>"#, cnt)
                    } else {
                        String::new()
                    };
                    s.push_str(&format!(r#"<div class="chip"><span class="chip-idx">{}</span><span class="chip-name">{}</span>{}</div>"#, idx, name, badge));
                }
                s.push_str("</div></div>");
                s
            };
            for &kw in crate::plugins::oai::utils::MODEL_KEYWORDS {
                let group_name = format!(
                    "{} Series",
                    kw.chars().next().unwrap().to_uppercase().to_string() + &kw[1..]
                );
                if let Some(items) = groups.get(&group_name) {
                    html.push_str(&render_group(&group_name, items));
                }
            }
            if !other_models.is_empty() {
                html.push_str(&render_group("Other Models", &other_models));
            }
            reply(
                ctx,
                writer,
                &msg_event,
                &html,
                cmd.text_mode,
                &format!("🧩 模型列表 (共{}个)", models.len()),
            )
            .await;
        }
        Action::ViewAll(scope) => {
            let c = mgr.config.read().await;
            if let Some(a) = c.agents.iter().find(|a| a.name == *name) {
                let priv_scope = matches!(scope, Scope::Private);
                let hist = a.history(priv_scope, &uid);
                if hist.is_empty() {
                    let s = if priv_scope { "私有" } else { "公有" };
                    reply_text(
                        ctx,
                        writer,
                        &msg_event,
                        format!("📭 {} {}历史为空", name, s),
                    )
                    .await;
                    return;
                }
                let content = format_history(hist, 0, cmd.text_mode);
                let header = format!(
                    "{} {}历史 ({} 条)",
                    name,
                    if priv_scope { "私有" } else { "公有" },
                    hist.len()
                );
                reply(ctx, writer, &msg_event, &content, cmd.text_mode, &header).await;
            } else {
                reply_text(ctx, writer, &msg_event, format!("❌ {} 不存在", name)).await;
            }
        }
        Action::ViewAt(scope) => {
            if cmd.indices.is_empty() {
                reply_text(ctx, writer, &msg_event, "❌ 请指定索引：智能体/索引").await;
                return;
            }
            let c = mgr.config.read().await;
            if let Some(a) = c.agents.iter().find(|a| a.name == *name) {
                let priv_scope = matches!(scope, Scope::Private);
                let hist = a.history(priv_scope, &uid);
                let mut results = Vec::new();
                let mut extra_images = Vec::new();
                let re = Regex::new(r"!\[.*?\]\(((?:https?://|data:image/)[^\s\)]+)\)").unwrap();

                for i in &cmd.indices {
                    if *i > 0 && *i <= hist.len() {
                        let m = &hist[i - 1];
                        let emoji = match m.role.as_str() {
                            "user" => "👤",
                            "assistant" => "🤖",
                            _ => "❓",
                        };
                        let mut content = m.content.clone();
                        let mut msg_imgs = extract_image_urls(&content);
                        msg_imgs.extend(m.images.clone());
                        if cmd.text_mode {
                            content = re
                                .replace_all(&content, |caps: &regex::Captures| {
                                    let url = &caps[1];
                                    if url.starts_with("data:") {
                                        "[图片]".to_string()
                                    } else {
                                        url.to_string()
                                    }
                                })
                                .to_string();
                        }
                        if !m.images.is_empty() {
                            if !content.is_empty() {
                                content.push_str("\n\n");
                            }
                            for url in &m.images {
                                if cmd.text_mode {
                                    if url.starts_with("data:") {
                                        content.push_str("\n- [Base64 Image]");
                                    } else {
                                        content.push_str(&format!("\n- {}", url));
                                    }
                                } else {
                                    content.push_str(&format!("\n![image]({})", url));
                                }
                            }
                        }
                        extra_images.extend(msg_imgs);
                        results.push(format!("**#{} {}**\n{}", i, emoji, content));
                    }
                }
                if results.is_empty() {
                    reply_text(ctx, writer, &msg_event, "❌ 索引无效。").await;
                } else {
                    reply(
                        ctx,
                        writer,
                        &msg_event,
                        &results.join("\n\n---\n\n"),
                        cmd.text_mode,
                        &format!("{} 历史记录", name),
                    )
                    .await;
                    for url in extra_images {
                        if url.starts_with("data:") {
                            if let Some(base64_data) = url.split(',').nth(1) {
                                let _ = send_msg(
                                    ctx,
                                    writer.clone(),
                                    msg_event.group_id(),
                                    Some(msg_event.user_id()),
                                    Message::new().image(format!("base64://{}", base64_data)),
                                )
                                .await;
                            }
                        } else {
                            let _ = send_msg(
                                ctx,
                                writer.clone(),
                                msg_event.group_id(),
                                Some(msg_event.user_id()),
                                Message::new().image(&url),
                            )
                            .await;
                        }
                    }
                }
            } else {
                reply_text(ctx, writer, &msg_event, format!("❌ {} 不存在", name)).await;
            }
        }
        Action::Export(scope) => {
            let c = mgr.config.read().await;
            if let Some(a) = c.agents.iter().find(|a| a.name == *name) {
                let priv_scope = matches!(scope, Scope::Private);
                let hist = a.history(priv_scope, &uid);
                if hist.is_empty() {
                    reply_text(ctx, writer, &msg_event, "📭 历史为空").await;
                    return;
                }
                let scope_str = if priv_scope { "私有" } else { "公有" };
                let content = format_export_txt(name, &a.model, scope_str, hist);
                let scope_file = if priv_scope { "private" } else { "public" };
                let fname = format!(
                    "{}_{}_{}_{}.txt",
                    name,
                    scope_file,
                    uid,
                    chrono::Local::now().format("%Y%m%d%H%M%S")
                );
                let dir = mgr.path.parent().unwrap_or(&mgr.path).to_path_buf();
                let path = dir.join(&fname);
                match File::create(&path) {
                    Ok(mut f) => {
                        if f.write_all(content.as_bytes()).is_ok() {
                            let path_str = path.to_string_lossy().to_string();
                            let result = api::upload_file(
                                ctx,
                                writer.clone(),
                                msg_event.group_id(),
                                Some(msg_event.user_id()),
                                &path_str,
                                &fname,
                            )
                            .await;
                            match result {
                                Ok(_) => {
                                    reply_text(
                                        ctx,
                                        writer,
                                        &msg_event,
                                        format!("📤 已导出：{}", fname),
                                    )
                                    .await
                                }
                                Err(e) => {
                                    reply_text(
                                        ctx,
                                        writer,
                                        &msg_event,
                                        format!("❌ 上传失败：{}", e),
                                    )
                                    .await
                                }
                            }
                        } else {
                            reply_text(ctx, writer, &msg_event, "❌ 写入失败。").await;
                        }
                    }
                    Err(e) => {
                        reply_text(ctx, writer, &msg_event, format!("❌ 创建文件失败：{}", e)).await
                    }
                }
            } else {
                reply_text(ctx, writer, &msg_event, format!("❌ {} 不存在", name)).await;
            }
        }
        Action::EditAt(scope) => {
            if cmd.indices.is_empty() {
                reply_text(ctx, writer, &msg_event, "❌ 请指定索引：智能体'索引 新内容").await;
                return;
            }
            if cmd.args.is_empty() {
                reply_text(ctx, writer, &msg_event, "❌ 请提供新内容。").await;
                return;
            }
            let idx = cmd.indices[0];
            let mut c = mgr.config.write().await;
            if let Some(a) = c.agents.iter_mut().find(|a| a.name == *name) {
                let priv_scope = matches!(scope, Scope::Private);
                if a.edit_at(priv_scope, &uid, idx, &cmd.args) {
                    mgr.save(&c);
                    reply_text(ctx, writer, &msg_event, format!("✏️ 已编辑第 {} 条", idx)).await;
                } else {
                    reply_text(ctx, writer, &msg_event, format!("❌ 索引 {} 无效。", idx)).await;
                }
            } else {
                reply_text(ctx, writer, &msg_event, format!("❌ {} 不存在", name)).await;
            }
        }
        Action::DeleteAt(scope) => {
            if cmd.indices.is_empty() {
                reply_text(
                    ctx,
                    writer,
                    &msg_event,
                    "❌ 请指定索引：智能体-索引（支持 1,3,5 或 1-5）",
                )
                .await;
                return;
            }
            let mut c = mgr.config.write().await;
            if let Some(a) = c.agents.iter_mut().find(|a| a.name == *name) {
                let priv_scope = matches!(scope, Scope::Private);
                let deleted = a.delete_at(priv_scope, &uid, &cmd.indices);
                if deleted.is_empty() {
                    reply_text(ctx, writer, &msg_event, "❌ 索引无效。").await;
                } else {
                    mgr.save(&c);
                    let s = deleted
                        .iter()
                        .map(|i| i.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    reply_text(
                        ctx,
                        writer,
                        &msg_event,
                        format!("🗑️ 已删除第 {} 条（共 {} 条）", s, deleted.len()),
                    )
                    .await;
                }
            } else {
                reply_text(ctx, writer, &msg_event, format!("❌ {} 不存在", name)).await;
            }
        }
        Action::ClearHistory(scope) => {
            let is_priv_ctx = cmd.private_reply;
            {
                mgr.generating
                    .write()
                    .await
                    .set_generating(name, is_priv_ctx, &uid, false);
            }
            let mut c = mgr.config.write().await;
            if let Some(a) = c.agents.iter_mut().find(|a| a.name == *name) {
                let priv_scope = matches!(scope, Scope::Private);
                let s = if priv_scope { "私有" } else { "公有" };
                a.clear_history(priv_scope, &uid);
                a.generation_id += 1;
                mgr.save(&c);
                reply_text(
                    ctx,
                    writer,
                    &msg_event,
                    format!("🧹 {} {}历史已清空", name, s),
                )
                .await;
            } else {
                reply_text(ctx, writer, &msg_event, format!("❌ {} 不存在", name)).await;
            }
        }
        Action::ClearAllPublic => {
            {
                mgr.generating.write().await.public.clear();
            }
            let mut c = mgr.config.write().await;
            let cnt = c.agents.len();
            for a in c.agents.iter_mut() {
                a.public_history.clear();
                a.generation_id += 1;
            }
            mgr.save(&c);
            reply_text(
                ctx,
                writer,
                &msg_event,
                format!("🧹 已清空 {} 个智能体的公有历史", cnt),
            )
            .await;
        }
        Action::ClearEverything => {
            {
                let mut g = mgr.generating.write().await;
                g.public.clear();
                g.private.clear();
            }
            let mut c = mgr.config.write().await;
            let cnt = c.agents.len();
            for a in c.agents.iter_mut() {
                a.public_history.clear();
                a.private_histories.clear();
                a.generation_id += 1;
            }
            mgr.save(&c);
            reply_text(
                ctx,
                writer,
                &msg_event,
                format!("⚠️ 已清空 {} 个智能体的所有历史", cnt),
            )
            .await;
        }
        Action::Help => {
            let help = r#"## 模式前缀（可组合）
| 符号 | 含义 |
|:---:|------|
| `&` | 私有模式 (独立历史) |
| `"` | 文本模式 (不转图片) |
| `~` | 临时模式 (无历史/不阻塞) |

## 智能体管理
| 指令 | 功能 | 示例 |
|------|------|------|
| `##名称 模型 提示词` | 创建/更新 | `##助手 gpt-4o 你是助手` |
| `##:模型` | 批量生成描述 | `##:gpt-4o` |
| `智能体~=新名` | 重命名 | `助手~=管家` |
| `智能体~#新名` | 复制 | `助手~#助手2` |
| `智能体:描述` | 设置描述 | `助手:通用助手` |
| `-#名称` | 删除 | `-#助手` |
| `/#` | 列表 | `/#` |

## 配置修改
| 指令 | 功能 | 示例 |
|------|------|------|
| `智能体%模型` | 修改模型 | `助手%gpt-4` |
| `智能体$提示词` | 修改提示词 | `助手$你是...` |
| `智能体$` | 清空提示词 | `助手$` |
| `智能体/$` | 查看提示词 | `助手/$` |
| `/%` | 模型列表 | `/%` |

## 对话控制
| 指令 | 功能 |
|------|------|
| `智能体 内容` | 正常对话 |
| `~智能体 内容` | 临时对话 (一次性) |
| `"智能体 内容` | 文本回复对话 |
| `&智能体 内容` | 私有历史对话 |
| `智能体~` | 重新生成上一条 |
| `智能体!` | 停止生成 |

## 工具增强房间
| 房间 | 能力 |
|------|------|
| `pi` | 联网检索并打开网页核实、执行终端命令；终端无需确认 |

> 工具：`web_search` 搜索、`web_fetch` 读取网页正文、`shell` 执行本机命令，同一轮内并发执行。
> 高权限工具仅对 `[oai].harness_rooms` 中列出的公有房间生效，`&` 私有模式不启用工具。
> 回复卡片底部会列出引用来源与本次耗时、工具轨迹。

## MJ 绘图房间
| 房间模型 | 直接操作 |
|------|------|
| `mj` | 输入提示词绘图；消息图片/引用图片自动作为垫图 |
| `mj-describe` | 发送或引用图片，生成描述 |
| `mj-shorten` | 输入提示词，生成精简版本 |
| `mj-blend` | 发送/引用至少两张图进行融合（可写横图/竖图） |

> 引用 `mj` 返回的四宫格，回复任意一个或多个 `1`–`4` 即可放大；已完成的放大直接读取缓存。

## 历史管理
| 指令 | 功能 |
|------|------|
| `智能体/*` | 查看所有 |
| `智能体/1` | 查看第1条 |
| `智能体/1-5` | 查看范围 |
| `智能体_*` | 导出(.txt) |
| `智能体'1 内容` | 编辑第1条 |
| `智能体-1` | 删除第1条 |
| `智能体-1,3` | 删除多条 |
| `智能体-*` | 清空历史 |

> 所有符号支持半角/全角兼容 (如 ～, ＃, ＝)
> 加 `&` 前缀可操作私有历史: `&智能体/*`

## 危险操作
| 指令 | 功能 |
|------|------|
| `-*` | 清空所有智能体公有历史 |
| `-*!` | 清空数据库所有历史 |

## API 配置
更新指令: `oai API地址 API密钥`
"#;
            reply(
                ctx,
                writer,
                &msg_event,
                help,
                cmd.text_mode,
                "🤖 OAI 符号指令帮助",
            )
            .await;
        }
        Action::AutoFillDescriptions(model_ref) => {
            let (target_agents, api_config, use_model) = {
                let c = mgr.config.read().await;
                let models = c.models.clone();
                let resolved_model = if model_ref.is_empty() {
                    c.default_model.clone()
                } else {
                    mgr.resolve_model(&model_ref, &models).unwrap_or(model_ref)
                };
                let targets: Vec<(String, String)> = c
                    .agents
                    .iter()
                    .filter(|a| a.description.is_empty() || a.description == "新建智能体")
                    .map(|a| (a.name.clone(), a.system_prompt.clone()))
                    .collect();
                (
                    targets,
                    (c.api_base.clone(), c.api_key.clone()),
                    resolved_model,
                )
            };

            if target_agents.is_empty() {
                reply_text(
                    ctx,
                    writer,
                    &msg_event,
                    "✅ 所有智能体均已有描述，无需处理。",
                )
                .await;
                return;
            }
            if api_config.0.is_empty() || api_config.1.is_empty() {
                reply_text(ctx, writer, &msg_event, "❌ API 未配置").await;
                return;
            }

            reply_text(
                ctx,
                writer,
                &msg_event,
                format!(
                    "🤖 开始使用 [{}] 为 {} 个智能体生成描述，请稍候...",
                    use_model,
                    target_agents.len()
                ),
            )
            .await;
            let client = Client::with_config(
                OpenAIConfig::new()
                    .with_api_base(super::utils::openai_api_base(&api_config.0))
                    .with_api_key(api_config.1),
            )
            .with_http_client(crate::http::client());
            let mut success_count = 0;

            for (name, prompt) in target_agents {
                let gen_prompt = format!(
                    "请阅读以下角色的 System Prompt，为其生成一个极简短的中文功能描述（Role/Tag）。\n要求：\n1. 必须控制在 10 个字以内\n2. 不要包含任何标点符号\n3. 直接输出描述内容，不要解释\n\nSystem Prompt:\n{}",
                    prompt
                );
                let req = CreateChatCompletionRequestArgs::default()
                    .model(&use_model)
                    .messages(vec![
                        ChatCompletionRequestUserMessageArgs::default()
                            .content(gen_prompt)
                            .build()
                            .unwrap()
                            .into(),
                    ])
                    .build();

                if let Ok(req) = req
                    && let Ok(res) = client.chat().create(req).await
                    && let Some(choice) = res.choices.first()
                    && let Some(content) = &choice.message.content
                {
                    let new_desc = content.trim().replace(['"', '“', '”', '。', '.'], "");
                    let mut c = mgr.config.write().await;
                    if let Some(a) = c.agents.iter_mut().find(|a| a.name == name) {
                        a.description = new_desc.clone();
                        mgr.save(&c);
                        success_count += 1;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            reply_text(
                ctx,
                writer,
                &msg_event,
                format!("✅ 批量处理完成，已更新 {} 个智能体的描述。", success_count),
            )
            .await;
        }
        Action::Create => {}
    }
}

pub async fn handle_create(
    name: &str,
    desc: &str,
    model: &str,
    prompt: &str,
    ctx: &Context,
    writer: &LockedWriter,
    mgr: &Arc<Manager>,
) {
    let msg_event = match ctx.as_message() {
        Some(e) => e,
        None => return,
    };
    let mut c = mgr.config.write().await;
    let models = c.models.clone();
    let model = mgr
        .resolve_model(model, &models)
        .unwrap_or_else(|| model.to_string());
    let prompt = if super::mj::is_mj_model(&model) && prompt.is_empty() {
        String::new()
    } else if prompt.is_empty() && !c.agents.iter().any(|a| a.name == name) {
        c.default_prompt.clone()
    } else {
        prompt.to_string()
    };

    if let Some(a) = c.agents.iter_mut().find(|a| a.name == name) {
        if !model.is_empty() {
            a.model = model.clone();
        }
        a.system_prompt = prompt;
        if !desc.is_empty() {
            a.description = desc.to_string();
        }
        let updated_model = a.model.clone();
        mgr.save(&c);
        reply_text(
            ctx,
            writer,
            &msg_event,
            format!("📝 已更新 {}（模型：{}）", name, updated_model),
        )
        .await;
    } else {
        let description = if desc.is_empty() {
            "新建智能体".to_string()
        } else {
            desc.to_string()
        };
        c.agents
            .push(Agent::new(name, &model, &prompt, &description));
        mgr.save(&c);
        reply_text(
            ctx,
            writer,
            &msg_event,
            format!("🤖 已创建 {}（模型：{}）", name, model),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_tool_request_omits_model_specific_reasoning_options() {
        let request = build_chat_request(
            "gpt-5.6-luna",
            vec![
                ChatCompletionRequestUserMessageArgs::default()
                    .content("test")
                    .build()
                    .unwrap()
                    .into(),
            ],
            Some(super::super::harness::HarnessConfig {
                shell_timeout_seconds: 30,
                shell_max_output_bytes: 4096,
                web_search_results: 5,
                web_fetch_max_chars: 4000,
                web_fetch_timeout_seconds: 20,
                hosted_web_search: false,
            }),
        )
        .unwrap();
        let serialized = serde_json::to_value(request).unwrap();

        assert!(serialized.get("tools").is_some());
        assert!(serialized.get("reasoning_effort").is_none());
    }

    #[test]
    fn short_prose_skips_the_image_card() {
        assert!(is_plain_enough("好的，已经帮你重启了。", 120));
        assert!(!is_plain_enough("## 标题\n正文", 120));
        assert!(!is_plain_enough("- 要点一\n- 要点二", 120));
        assert!(!is_plain_enough("见 [文档](https://example.com)", 120));
        assert!(!is_plain_enough(&"很长".repeat(200), 120));
        // 置 0 表示始终渲染卡片。
        assert!(!is_plain_enough("短", 0));
    }
}
