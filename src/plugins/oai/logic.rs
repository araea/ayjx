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
        ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
        ChatCompletionRequestMessageContentPartImageArgs,
        ChatCompletionRequestMessageContentPartTextArgs, ChatCompletionRequestSystemMessageArgs,
        ChatCompletionRequestUserMessageArgs, CreateChatCompletionRequest,
        CreateChatCompletionRequestArgs, ImageUrlArgs,
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
    sources: &[super::types::Source],
    footer: Option<super::render::Footer>,
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
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> =
        std::sync::OnceLock::new();
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

/// 普通房间直接使用 Chat Completions；Pi 房间由本机 CLI 管理工具。
fn build_chat_request(
    model: &str,
    messages: Vec<ChatCompletionRequestMessage>,
) -> anyhow::Result<CreateChatCompletionRequest> {
    Ok(CreateChatCompletionRequestArgs::default()
        .model(model)
        .messages(messages)
        .build()?)
}

pub(crate) async fn complete(
    client: &Client<OpenAIConfig>,
    model: &str,
    messages: Vec<ChatCompletionRequestMessage>,
) -> anyhow::Result<String> {
    let response = client
        .chat()
        .create(build_chat_request(model, messages)?)
        .await?;
    let choice = response
        .choices
        .first()
        .ok_or_else(|| anyhow::anyhow!("API 未返回任何候选回复"))?;
    choice
        .message
        .content
        .clone()
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("API 返回了空回复"))
}

fn prepare_history(
    history: &[ChatMessage],
    prompt: &str,
    imgs: &[String],
    regen: bool,
) -> Vec<ChatMessage> {
    let mut hist = history.to_vec();
    if regen {
        if hist.last().is_some_and(|m| m.role == "assistant") {
            hist.pop();
        }
        if !prompt.is_empty() || !imgs.is_empty() {
            if hist.last().is_some_and(|m| m.role == "user") {
                hist.pop();
            }
            hist.push(ChatMessage::new("user", prompt, imgs.to_vec()));
        }
    } else {
        hist.push(ChatMessage::new("user", prompt, imgs.to_vec()));
    }
    hist
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

    let use_pi = agent.uses_pi();
    if !use_pi && super::mj::is_mj_model(&agent.model) {
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

    if !use_pi && (api.0.is_empty() || api.1.is_empty()) {
        reply_text(ctx, writer, &event, "❌ API 未配置。").await;
        return;
    }

    if !regen && prompt.is_empty() && imgs.is_empty() {
        reply_text(ctx, writer, &event, "💬 请输入内容。").await;
        return;
    }
    let (hist, gen_id) = if temp_mode {
        (prepare_history(&[], prompt, &imgs, regen), 0)
    } else {
        let mut config = mgr.config.write().await;
        let Some(current) = config.agents.iter_mut().find(|a| a.name == name) else {
            return;
        };
        let reservation = mgr.generating.write().await.begin(name, is_priv_ctx, &uid);
        let Some(id) = reservation else {
            drop(config);
            reply_text(
                ctx,
                writer,
                &event,
                "⏳ 正在生成中，请等待，或使用「智能体!」停止。",
            )
            .await;
            return;
        };
        let hist = prepare_history(current.history(is_priv_ctx, &uid), prompt, &imgs, regen);
        *current.history_mut(is_priv_ctx, &uid) = hist.clone();
        mgr.save(&config);
        (hist, id)
    };

    let api_base = super::utils::openai_api_base(&api.0);
    let client = Client::with_config(
        OpenAIConfig::new()
            .with_api_base(api_base.clone())
            .with_api_key(api.1.clone()),
    )
    .with_http_client(crate::http::client());

    let oai = crate::plugins::get_config_or_default::<super::OaiConfig>(ctx, "oai");
    let annotate = !event.is_manual_self();
    if annotate {
        let _ = api::set_msg_emoji_like(ctx, writer.clone(), event.message_id(), 124, true).await;
    }

    let started = std::time::Instant::now();
    // 发起人本身是全局管理员时，这一轮 pi 房间对话可以用自然语言驱动 ctl；
    // 凭据随 `control` 一起活到本轮结束。其余情况下拿到 None，行为与从前一致。
    let control = if use_pi {
        crate::plugins::ctl::bridge::lease(ctx).await
    } else {
        None
    };

    // 出结果与到总预算在同一个任务里赛跑。用 select 而不是另起任务，是因为发消息
    // 要用借来的 ctx/event，搬进 spawn 就得整套克隆一遍。把工作 Future 放进独立
    // 作用域，超时/停止后立即 drop 并终止 Pi 子进程。
    //
    // 中途不发任何「还在处理」提示：等待本身是隐式的，一条进度播报换不来更快的
    // 回复，只会在群里插进一段与上下文无关的噪音。
    let mut outcome = {
        // 图像模型走专用绘图接口，其余房间继续走聊天补全 / Pi。
        // Pi 房间的模型是交给 Pi 解析的，不能拿它去撞中转站的绘图模型关键字。
        let draw = !use_pi && super::images::is_images_model(&agent.model, &oai.image_models);
        let work = async {
            if draw {
                super::images::generate_reply(&api_base, &api.1, &agent, &hist).await
            } else {
                respond(
                    &client,
                    &agent,
                    &hist,
                    &oai,
                    mgr.path.parent().unwrap_or(&mgr.path),
                    control.as_ref(),
                )
                .await
            }
        };
        let mut work = std::pin::pin!(work);
        let mut budget = std::pin::pin!(tokio::time::sleep(oai.request_timeout()));

        let mut cancellation = tokio::time::interval(std::time::Duration::from_millis(200));
        loop {
            tokio::select! {
                result = &mut work => break Some(result.map(Some)),
                _ = &mut budget => break None,
                _ = cancellation.tick(), if !temp_mode => {
                    let config = mgr.config.read().await;
                    if !config.agents.iter().any(|a| a.name == name)
                        || !mgr.generating.read().await.is_current(name, is_priv_ctx, &uid, gen_id) {
                        break Some(Ok(None));
                    }
                }
            }
        }
    };

    // 在保存历史前再次验证请求身份；旧请求不能清掉新请求的占用状态。
    if !temp_mode {
        let mut config = mgr.config.write().await;
        let mut generating = mgr.generating.write().await;
        if generating.is_current(name, is_priv_ctx, &uid, gen_id) {
            generating.set_generating(name, is_priv_ctx, &uid, false);
            if let Some(Ok(Some(reply))) = &outcome {
                if let Some(agent) = config.agents.iter_mut().find(|a| a.name == name) {
                    agent.history_mut(is_priv_ctx, &uid).push(ChatMessage::new(
                        "assistant",
                        &reply.text,
                        vec![],
                    ));
                    mgr.save(&config);
                } else {
                    outcome = Some(Ok(None));
                }
            }
        } else {
            outcome = Some(Ok(None));
        }
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
            reply_text(ctx, writer, &event, format!("❌ 对话失败：{error:#}")).await;
        }
        Some(Ok(None)) => {}
        Some(Ok(Some(reply_data))) => {
            let content = reply_data.text;

            let msg_index = if temp_mode {
                0
            } else {
                let c = mgr.config.read().await;
                c.agents
                    .iter()
                    .find(|a| a.name == name)
                    .map(|a| a.history(is_priv_ctx, &uid).len())
                    .unwrap_or(0)
            };

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
            let footer = (oai.show_trace_footer() && !plain).then(|| super::render::Footer {
                meta: format!(
                    "{} · {}",
                    reply_data.model.as_deref().unwrap_or(&agent.model),
                    super::utils::format_elapsed(started)
                ),
                trace: reply_data.trace.clone(),
                trace_overflow: reply_data.trace_overflow,
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

/// 房间列表和回执里显示的模型：Pi 房间前面挂上引擎，一眼能看出这间屋子谁在跑。
fn room_model_label(agent: &Agent) -> String {
    if !agent.uses_pi() {
        return agent.model.clone();
    }
    if super::pi_agent::follows_pi_config(&agent.model) {
        "Pi · 本机配置".to_string()
    } else {
        format!("Pi · {}", agent.model)
    }
}

/// 一次成功回复的产物。
pub(super) struct Reply {
    pub(super) text: String,
    pub(super) sources: Vec<super::types::Source>,
    pub(super) trace: Vec<super::types::TraceStep>,
    /// 超出页脚保留上限、只计数的调用次数。
    pub(super) trace_overflow: usize,
    pub(super) model: Option<String>,
}

/// Pi 房间直接读取本机 Pi 配置，普通房间继续使用 OAI 配置。
async fn respond(
    client: &Client<OpenAIConfig>,
    agent: &Agent,
    hist: &[ChatMessage],
    oai: &super::OaiConfig,
    data_dir: &std::path::Path,
    control: Option<&crate::plugins::ctl::bridge::Lease>,
) -> anyhow::Result<Reply> {
    if agent.uses_pi() {
        let result = super::pi_agent::conversation(
            &oai.pi_command,
            data_dir,
            &agent.system_prompt,
            &agent.model,
            oai.pi_stall(),
            hist,
            control,
        )
        .await?;
        return Ok(Reply {
            text: result.text,
            sources: Vec::new(),
            trace: result.trace,
            trace_overflow: result.trace_overflow,
            model: result.model,
        });
    }
    let msgs = build_chat_messages(agent, hist).await;
    Ok(Reply {
        text: complete(client, &agent.model, msgs).await?,
        sources: Vec::new(),
        trace: Vec::new(),
        trace_overflow: 0,
        model: Some(agent.model.clone()),
    })
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

    let mut pending_sys_prompt =
        (!agent.system_prompt.is_empty()).then(|| agent.system_prompt.clone());

    if !force_user_role_for_system && let Some(sp) = pending_sys_prompt.take() {
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
            || line.split_once(". ").is_some_and(|(head, _)| {
                !head.is_empty() && head.chars().all(|c| c.is_ascii_digit())
            })
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
            let filter = crate::plugins::get_config_or_default::<super::OaiConfig>(ctx, "oai")
                .model_filter;
            match mgr.fetch_models(&filter).await {
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
            if !super::parser::valid_agent_name(&cmd.args) {
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
                // 副本要连引擎一起带走：新名字不再能推断出「这是一间 Pi 房间」。
                new_agent.set_engine(&src.engine, &src.model);
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
            if !super::parser::valid_agent_name(&cmd.args) {
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
                mgr.generating.write().await.cancel_room(name);
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
        // 同一个 `%` 既换模型也换引擎：`房间%pi` 交给本机 pi，`房间%pi 模型` 顺带指定
        // pi 用哪个模型，写中转站模型名则转回中转站房间。房间名不再参与判断。
        Action::SetModel => {
            if cmd.args.is_empty() {
                reply_text(
                    ctx,
                    writer,
                    &msg_event,
                    "❌ 请指定模型：`智能体%模型名`；交给本机 Pi 用 `智能体%pi` 或 `智能体%pi 模型`。",
                )
                .await;
                return;
            }
            let mut c = mgr.config.write().await;
            let models = c.models.clone();
            let pi_model = super::pi_agent::parse_pi_spec(&cmd.args);
            let resolved = match &pi_model {
                Some(model) => Some(model.clone()),
                None => mgr.resolve_model(&cmd.args, &models),
            };
            let Some(model) = resolved else {
                reply_text(
                    ctx,
                    writer,
                    &msg_event,
                    "❌ 无效模型。`/%` 查看中转站模型，或用 `智能体%pi 模型` 交给本机 Pi。",
                )
                .await;
                return;
            };
            let Some(a) = c.agents.iter_mut().find(|a| a.name == *name) else {
                reply_text(ctx, writer, &msg_event, format!("❌ {} 不存在", name)).await;
                return;
            };
            let old = room_model_label(a);
            a.set_engine(
                if pi_model.is_some() {
                    super::types::ENGINE_PI
                } else {
                    super::types::ENGINE_CHAT
                },
                &model,
            );
            let new = room_model_label(a);
            mgr.save(&c);
            reply_text(
                ctx,
                writer,
                &msg_event,
                format!("🔄 {} 模型：{} → {}", name, old, new),
            )
            .await;
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
                    room_model_label(a),
                    prompt_display
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
                groups
                    .entry(room_model_label(a))
                    .or_default()
                    .push((i + 1, a));
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
                mgr.generating.write().await.cancel_room(name);
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
            let filter = crate::plugins::get_config_or_default::<super::OaiConfig>(ctx, "oai")
                .model_filter;
            if let Err(e) = mgr.fetch_models(&filter).await {
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
                    "📭 未找到可用模型（过滤规则见 [oai] model_filter.keep / .drop）",
                )
                .await;
                return;
            }

            use std::collections::HashMap;
            let mut usage_count = HashMap::new();
            for agent in &c.agents {
                *usage_count.entry(agent.model.clone()).or_insert(0) += 1;
            }

            // 按厂商分区；分区顺序取各组首次出现的次序，模型列表本身已按 id 排序，
            // 因此同一厂商的条目天然连在一起，顺序稳定可预期。
            let mut groups: HashMap<&'static str, Vec<(usize, String)>> = HashMap::new();
            let mut group_order: Vec<&'static str> = Vec::new();
            for (i, m) in models.iter().enumerate() {
                let vendor = crate::plugins::oai::utils::model_vendor(m);
                let entry = groups.entry(vendor).or_insert_with(|| {
                    group_order.push(vendor);
                    Vec::new()
                });
                entry.push((i + 1, m.clone()));
            }
            // 认不出厂商的一律排到最后，别插在正经分区中间
            if let Some(pos) = group_order.iter().position(|v| *v == "其他") {
                let other = group_order.remove(pos);
                group_order.push(other);
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
            for vendor in group_order {
                if let Some(items) = groups.get(vendor) {
                    html.push_str(&render_group(vendor, items));
                }
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
                    mgr.generating
                        .write()
                        .await
                        .set_generating(name, priv_scope, &uid, false);
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
                if !deleted.is_empty() {
                    mgr.generating
                        .write()
                        .await
                        .set_generating(name, priv_scope, &uid, false);
                }
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
            let is_priv_ctx = matches!(scope, Scope::Private);
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
| `智能体%模型` | 修改模型/引擎 | `助手%gpt-5.6-luna` |
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

## Pi Agent 房间
| 指令 | 效果 | 示例 |
|------|------|------|
| `##名称 pi` | 建一间 Pi 房间 | `##研究 pi` |
| `##名称 pi/模型` | 建房并指定模型 | `##研究 pi/claude-opus-5` |
| `智能体%pi` | 已有房间转 Pi | `助手%pi` |
| `智能体%pi 模型` | 换 Pi 用的模型 | `助手%pi apilio/kimi-k3` |
| `智能体%中转站模型` | 转回中转站房间 | `助手%gpt-5.6-luna` |

> 房间名可以随便取，中文也行；决定引擎的是这条指令，不是名字。旧的 `pi` / `pi-*`
> 房间已自动带上 Pi 引擎，行为不变。
> 模型写 `provider/id`（如 `apilio/claude-opus-5`）或裸 id，由本机 Pi 解析；
> 只写 `pi` 则沿用 Pi 自己的默认模型。`/#` 里显示为 `Pi · 模型`。
> 公有、`&` 私有和 `~` 临时模式均使用 Pi，历史按原模式隔离。
> 房间提示词追加到 Pi 系统提示词；支持图片、历史编辑/删除/清空/重新生成；
> 长回复卡片显示实际应答模型、耗时和工具轨迹。
> Pi 可执行文件由 `[oai].pi_command` 指定，默认 `pi`。

## MJ 绘图房间
| 房间模型 | 直接操作 |
|------|------|
| `mj` | 输入提示词绘图；消息图片/引用图片自动作为垫图 |

> 引用 `mj` 返回的四宫格，回复任意一个或多个 `1`–`4` 即可放大；已完成的放大直接读取缓存。

## 图像生成房间 (gpt-image)
| 房间模型 | 直接操作 |
|------|------|
| `gpt-image-2.5-flare` / `gpt-image-2.5-sunburst` | 输入提示词直接出图；发图或引用图片则作为垫图编辑 |

> 例：`##画图 gpt-image-2.5-flare` 创建房间，然后 `画图 一只在窗台晒太阳的橘猫`。
> 垫图：直接发送图片或引用图片，再说修改要求，如「把背景改成星空」；最多 4 张。
> 可选参数：`--size 1536x1024`（或 `-s auto`）、`--quality high`（或 `-q low/medium/high/auto`）。
> 走图像接口的模型关键字由 `[oai].image_models` 配置，默认 `["gpt-image-2.5"]`。

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
    // 建房时的模型位同样认 Pi 写法：`##研究 pi` 或 `##研究 pi/apilio/claude-opus-5`。
    let pi_model = super::pi_agent::parse_pi_spec(model);
    let engine = if pi_model.is_some() {
        super::types::ENGINE_PI
    } else {
        super::types::ENGINE_CHAT
    };
    let model = match pi_model {
        Some(model) => model,
        None => mgr
            .resolve_model(model, &models)
            .unwrap_or_else(|| model.to_string()),
    };
    let prompt = if super::mj::is_mj_model(&model) && prompt.is_empty() {
        String::new()
    } else if prompt.is_empty() && !c.agents.iter().any(|a| a.name == name) {
        c.default_prompt.clone()
    } else {
        prompt.to_string()
    };

    if let Some(a) = c.agents.iter_mut().find(|a| a.name == name) {
        // 省略模型位时只改提示词和描述，保留这个房间原来的引擎。
        if !model.is_empty() || engine == super::types::ENGINE_PI {
            a.set_engine(engine, &model);
        }
        a.system_prompt = prompt;
        if !desc.is_empty() {
            a.description = desc.to_string();
        }
        let updated_model = room_model_label(a);
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
        let mut agent = Agent::new(name, &model, &prompt, &description);
        agent.set_engine(engine, &model);
        let label = room_model_label(&agent);
        c.agents.push(agent);
        mgr.save(&c);
        reply_text(
            ctx,
            writer,
            &msg_event,
            format!("🤖 已创建 {}（模型：{}）", name, label),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_chat_request_has_no_tools() {
        let request = build_chat_request(
            "example-model",
            vec![
                ChatCompletionRequestUserMessageArgs::default()
                    .content("test")
                    .build()
                    .unwrap()
                    .into(),
            ],
        )
        .unwrap();
        let serialized = serde_json::to_value(request).unwrap();
        assert!(serialized.get("tools").is_none());
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
    #[test]
    fn regeneration_replays_only_the_current_user_message() {
        let hist = vec![
            ChatMessage::new("user", "旧问题", vec!["image".into()]),
            ChatMessage::new("assistant", "旧回答", vec![]),
        ];
        let replay = prepare_history(&hist, "", &[], true);
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].images, vec!["image"]);
        let edited = prepare_history(&hist, "新问题", &[], true);
        assert_eq!(edited.len(), 1);
        assert_eq!(edited[0].content, "新问题");
        assert!(edited[0].images.is_empty());
    }

    #[tokio::test]
    async fn pi_room_routing_ignores_the_oai_model_and_credentials() {
        let dir = std::env::temp_dir().join(format!("pi-routing-{:032x}", rand::random::<u128>()));
        let config = super::super::OaiConfig {
            pi_command: dir.join("missing-pi").to_string_lossy().into(),
            ..Default::default()
        };
        let client = Client::with_config(OpenAIConfig::new().with_api_base("").with_api_key(""));
        for name in ["pi", "PI-test"] {
            let agent = Agent::new(name, "mj", "", "");
            let history = vec![ChatMessage::new("user", "test", vec![])];
            let result = respond(&client, &agent, &history, &config, &dir, None).await;
            assert!(result.err().unwrap().to_string().contains("无法启动 pi"));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
