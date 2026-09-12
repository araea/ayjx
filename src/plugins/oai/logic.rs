use super::data::Manager;
use super::parser::{Action, Command, Scope};
use super::types::{Agent, ChatMessage};
use super::utils::{escape_markdown_special, format_export_txt, format_history};
use crate::adapters::satori::{LockedWriter, api, send_msg};
use crate::event::{Context, MessageEvent};
use crate::message::Message;
use regex::Regex;
use rig_core::completion::message::{DocumentSourceKind, Image, Text, UserContent};
use rig_core::completion::{AssistantContent, Message as LlmMessage};
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
    let oai = crate::plugins::get_config_or_default::<super::OaiConfig>(ctx, "oai");
    // 内置 agent 房间可以只写 `pi`（或干脆留空）表示「用默认模型」：先看
    // `[oai] agent_default_model`，再退到 oai config.json 里的 default_model。
    // 普通房间没有这层回退——模型是建房时就定下的。
    let spec = if use_pi && super::agent::uses_default_model(&agent.model) {
        let fallback = mgr.config.read().await.default_model.clone();
        oai.agent_default_model()
            .unwrap_or(fallback.as_str())
            .to_string()
    } else {
        agent.model.clone()
    };
    if spec.trim().is_empty() {
        reply_text(
            ctx,
            writer,
            &event,
            "❌ 这间内置房间还没指定模型：用 `房间%供应商/模型` 设一个，或配置 [oai] agent_default_model。",
        )
        .await;
        return;
    }
    // 房间模型可以写成 `供应商/模型`；前缀决定打哪个接口，剥掉前缀的部分才是发给
    // 接口的模型 id。内置 agent 房间同样按前缀选接口（见 `resolve_endpoint`）。
    let (provider, chat_model) = super::utils::split_provider(&spec);
    // 按剥掉供应商前缀后的名字判家族，与后面的图像模型判断保持一致。
    if !use_pi && super::mj::is_mj_model(&chat_model) {
        // MJ 房间天生是无历史任务流；引用文字也不应混入绘图提示词。
        super::mj::handle_agent(&agent, &cmd.args, imgs, ctx, writer, mgr).await;
        return;
    }
    let (api_base, api_key) = match super::resolve_endpoint(
        &oai.providers,
        &api.0,
        &api.1,
        provider.as_deref(),
    ) {
        Some(endpoint) => endpoint,
        None => {
            reply_text(
                ctx,
                writer,
                &event,
                format!(
                    "❌ 未知供应商：{}（在 [oai.providers] 里配置，或用默认接口）",
                    provider.as_deref().unwrap_or_default()
                ),
            )
            .await;
            return;
        }
    };

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

    if api_base.is_empty() || api_key.is_empty() {
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

    let base = super::utils::openai_api_base(&api_base);

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
        // 图像模型走专用绘图接口，其余房间继续走聊天补全 / 内置 agent。
        // 内置 agent 房间的模型是交给执行层解析的，不能拿它去撞中转站的绘图模型关键字。
        let draw = !use_pi && super::images::is_images_model(&chat_model, &oai.image_models);
        let work = async {
            if draw {
                let mut draw_agent = agent.clone();
                draw_agent.model = chat_model.clone();
                super::images::generate_reply(&base, &api_key, &draw_agent, &hist).await
            } else {
                respond(
                    &api_base,
                    &api_key,
                    &agent,
                    &chat_model,
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

/// 回执里那句「会打到哪些后端」，优先的在前面。
///
/// 只在这一轮真的能联网时才显示：链空着说明没配出任何可用后端，这时把话说破，
/// 省得管理员以为开关打开了就一定搜得到。
fn search_backends(config: &super::search::SearchConfig) -> String {
    let chain = config.chain();
    if chain.is_empty() {
        return "（没有可用的后端，去 [oai.search.backends] 配密钥）".to_string();
    }
    chain.join(" → ")
}

/// 房间里显示的联网状态：区分「这间房自己定的」和「跟着全局走」。
fn search_state(agent: &Agent, global: bool) -> String {
    if !agent.uses_pi() {
        return "不适用（普通房间）".to_string();
    }
    match (agent.web_search(global), agent.search) {
        (true, Some(true)) => "开（这间房自己定的）".to_string(),
        (true, _) => "开（跟随全局）".to_string(),
        (false, Some(false)) => "关（这间房自己定的）".to_string(),
        (false, _) => "关（跟随全局）".to_string(),
    }
}

/// 房间列表和回执里显示的模型：内置 agent 房间前面挂上引擎，一眼能看出这间屋子
/// 谁在跑；设了思考强度就一并标出。
fn room_model_label(agent: &Agent) -> String {
    let base = if !agent.uses_pi() {
        agent.model.clone()
    } else if super::agent::uses_default_model(&agent.model) {
        "内置 · 默认模型".to_string()
    } else {
        format!("内置 · {}", agent.model)
    };
    match agent.effective_thinking() {
        Some(level) => format!("{base} · 思考:{level}"),
        None => base,
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

/// 内置 agent 房间走带工具的多轮循环，普通房间走单轮 Chat Completions。
///
/// `api_base` / `api_key` 已按房间模型的 `供应商/` 前缀解析好；
/// `chat_model` 是真正要发给接口的模型 id（已剥掉前缀）。
#[allow(clippy::too_many_arguments)]
async fn respond(
    api_base: &str,
    api_key: &str,
    agent: &Agent,
    chat_model: &str,
    hist: &[ChatMessage],
    oai: &super::OaiConfig,
    data_dir: &std::path::Path,
    control: Option<&crate::plugins::ctl::bridge::Lease>,
) -> anyhow::Result<Reply> {
    let thinking = agent.effective_thinking();
    if agent.uses_pi() {
        // 联网开关是房间自己的选择优先，没写过才跟 `[oai.search].enabled`。
        // 工具只是挂上去，搜不搜由模型按需决定，没有任何一轮是强制的。
        let search = super::search::SearchConfig {
            enabled: agent.web_search(oai.search.enabled),
            ..oai.search.clone()
        };
        let result = super::agent::conversation(
            api_base,
            api_key,
            data_dir,
            &agent.system_prompt,
            chat_model,
            thinking.as_deref(),
            oai.pi_stall(),
            hist,
            control,
            &search,
        )
        .await?;
        return Ok(Reply {
            text: result.text,
            sources: result.sources,
            trace: result.trace,
            trace_overflow: result.trace_overflow,
            // 页脚显示房间写的那份（`供应商/模型`）；房间没写模型时显示解析出来的默认模型。
            model: Some(if super::agent::uses_default_model(&agent.model) {
                chat_model.to_string()
            } else {
                agent.model.clone()
            }),
        });
    }
    let msgs = build_chat_messages(agent, hist).await;
    Ok(Reply {
        text: super::llm::complete(api_base, api_key, chat_model, msgs, thinking.as_deref()).await?,
        sources: Vec::new(),
        trace: Vec::new(),
        trace_overflow: 0,
        model: Some(chat_model.to_string()),
    })
}

/// 把房间历史转成 Chat Completions 消息。
async fn build_chat_messages(agent: &Agent, hist: &[ChatMessage]) -> Vec<LlmMessage> {
    let mut msgs: Vec<LlmMessage> = Vec::new();

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
        msgs.push(LlmMessage::System { content: sp });
    }

    let re = Regex::new(r"!\[.*?\]\((data:image/[^\s\)]+)\)").unwrap();
    for m in hist {
        if m.role == "user" {
            let mut parts = Vec::new();

            if let Some(sp) = pending_sys_prompt.take() {
                parts.push(UserContent::Text(Text::new(sp)));
            }

            if !m.content.is_empty() {
                parts.push(UserContent::Text(Text::new(m.content.clone())));
            }
            for data_url in resolve_images(&m.images).await {
                parts.push(image_content(data_url));
            }
            if parts.is_empty() {
                continue;
            }
            msgs.push(LlmMessage::User { content: parts });
        } else if m.role == "assistant" {
            let clean_content = re.replace_all(&m.content, "[Image Created]").to_string();
            // 空内容的 assistant 消息会被请求校验拒掉（provider 侧同样报错），跳过。
            if !clean_content.trim().is_empty() {
                msgs.push(LlmMessage::Assistant {
                    id: None,
                    content: vec![AssistantContent::Text(Text::new(clean_content))],
                });
            }
            let gen_imgs = extract_image_urls(&m.content);
            if !gen_imgs.is_empty() {
                msgs.push(LlmMessage::User {
                    content: gen_imgs.into_iter().map(image_content).collect(),
                });
            }
        }
    }

    if let Some(sp) = pending_sys_prompt {
        msgs.push(LlmMessage::User {
            content: vec![UserContent::Text(Text::new(sp))],
        });
    }
    msgs
}

/// 图片地址是多模态模型可内联的 data URL 时原样交给 provider。
fn image_content(url: String) -> UserContent {
    UserContent::Image(Image {
        data: DocumentSourceKind::Url(url),
        media_type: None,
        detail: None,
        additional_params: None,
    })
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
                // 副本要连引擎一起带走：新名字不再能推断出「这是一间 Agent 房间」。
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
                    "❌ 请指定模型：`智能体%模型名`；交给内置 agent 用 `智能体%pi` 或 `智能体%pi 模型`。",
                )
                .await;
                return;
            }
            let mut c = mgr.config.write().await;
            let models = c.models.clone();
            // 模型串支持 `:强度` 后缀；先摘掉它，剩下的再判 Pi 写法 / 供应商前缀。
            let (spec, thinking) = super::utils::split_thinking(&cmd.args);
            let pi_model = super::agent::parse_pi_spec(&spec);
            let resolved = match &pi_model {
                Some(model) => Some((super::types::ENGINE_PI, model.clone())),
                None => {
                    let (provider, bare) = super::utils::split_provider(&spec);
                    match provider {
                        // 带供应商前缀时不做中转站模型匹配，原样保留前缀交给路由。
                        Some(_) => Some((super::types::ENGINE_CHAT, spec.clone())),
                        None => mgr
                            .resolve_model(&bare, &models)
                            .map(|model| (super::types::ENGINE_CHAT, model)),
                    }
                }
            };
            let Some((engine, model)) = resolved else {
                reply_text(
                    ctx,
                    writer,
                    &msg_event,
                    "❌ 无效模型。`/%` 查看中转站模型，或用 `智能体%pi 模型` 交给内置 agent。",
                )
                .await;
                return;
            };
            let Some(a) = c.agents.iter_mut().find(|a| a.name == *name) else {
                reply_text(ctx, writer, &msg_event, format!("❌ {} 不存在", name)).await;
                return;
            };
            let old = room_model_label(a);
            a.set_engine(engine, &model);
            // 没写 `:强度` 就保留房间原来的档位，只换模型时不必重报思考强度。
            if let Some(level) = thinking {
                a.thinking = level;
            }
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
        // `房间?` 管这间房要不要联网：不带词就换一边，`开`/`关` 明确指定，
        // `默认` 交回 [oai.search].enabled。房间自己的选择优先于全局。
        Action::SetSearch => {
            let oai = crate::plugins::get_config_or_default::<super::OaiConfig>(ctx, "oai");
            let global = oai.search.enabled;
            let mut c = mgr.config.write().await;
            let Some(a) = c.agents.iter_mut().find(|a| a.name == *name) else {
                reply_text(ctx, writer, &msg_event, format!("❌ {} 不存在", name)).await;
                return;
            };
            if !a.uses_pi() {
                reply_text(
                    ctx,
                    writer,
                    &msg_event,
                    format!(
                        "❌ {} 是普通房间，联网搜索只挂在内置 agent 上；先 `{}%pi` 把它转过来。",
                        name, name
                    ),
                )
                .await;
                return;
            }
            let Some(choice) = super::parser::search_choice(&cmd.args, a.web_search(global)) else {
                reply_text(
                    ctx,
                    writer,
                    &msg_event,
                    "❌ 不认识这个写法。`房间?` 换一边，`房间?开` / `房间?关` 明确开关，`房间?默认` 跟随全局。",
                )
                .await;
                return;
            };
            a.search = choice;
            let now = a.web_search(global);
            mgr.save(&c);
            let state = match choice {
                Some(true) => "已打开（按需触发）",
                Some(false) => "已关闭",
                None if now => "跟随全局（当前开，按需触发）",
                None => "跟随全局（当前关）",
            };
            // 打开时顺带说一句会打到哪个后端，省得去翻配置确认密钥有没有生效。
            let backends = if now {
                format!("　后端 {}", search_backends(&oai.search))
            } else {
                String::new()
            };
            reply_text(
                ctx,
                writer,
                &msg_event,
                format!("🔍 {} 联网搜索：{}{}", name, state, backends),
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
                let search_default =
                    crate::plugins::get_config_or_default::<super::OaiConfig>(ctx, "oai")
                        .search
                        .enabled;
                let content = format!(
                    "**模型**: `{}`\n\n**联网搜索**: {}\n\n**提示词**:\n```\n{}\n```",
                    room_model_label(a),
                    search_state(a, search_default),
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
            let search_default =
                crate::plugins::get_config_or_default::<super::OaiConfig>(ctx, "oai")
                    .search
                    .enabled;
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
            // 分区优先于模型：内置预设那一批的共同点是「预设」而不是「跑哪个模型」，
            // 混进用户自建的同模型房间里就找不着了。带分区的排在前面（false < true）。
            let mut groups: BTreeMap<(bool, String), Vec<(usize, &Agent)>> = BTreeMap::new();
            for (i, a) in c.agents.iter().enumerate() {
                let section = a.section.trim();
                let key = if section.is_empty() {
                    (true, room_model_label(a))
                } else {
                    (false, format!("{section} · {}", room_model_label(a)))
                };
                groups.entry(key).or_default().push((i + 1, a));
            }
            let mut html_parts = Vec::new();
            for ((plain, model), mut agents) in groups {
                agents.sort_by_key(|a| a.1.name.to_lowercase());
                html_parts.push(format!(r#"<div class="model-group"><div class="model-header"><span>{} {}</span><span class="model-count">{}</span></div><div class="agent-grid">"#, if plain { "📦" } else { "🎨" }, model, agents.len()));
                for (real_idx, a) in agents {
                    let desc_display = if !a.description.is_empty() {
                        super::utils::truncate_str(&a.description, 20)
                    } else if !a.system_prompt.is_empty() {
                        super::utils::truncate_str(&a.system_prompt, 20)
                    } else {
                        "无描述".to_string()
                    };
                    // 联网的房间挂个放大镜：一眼看出哪几间会出去查资料。
                    let desc_display = if a.uses_pi() && a.web_search(search_default) {
                        format!("🔍 {}", desc_display)
                    } else {
                        desc_display
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
| `智能体%供应商/模型` | 指定供应商 | `助手%deepseek/deepseek-flash` |
| `智能体%模型:强度` | 顺带设思考强度 | `助手%deepseek-flash:high` |
| `智能体$提示词` | 修改提示词 | `助手$你是...` |
| `智能体$` | 清空提示词 | `助手$` |
| `智能体/$` | 查看提示词 | `助手/$` |
| `智能体?` | 联网搜索换一边 | `研究?` |
| `智能体?开` / `?关` | 这间房开或联网 | `研究?关` |
| `智能体?默认` | 跟随全局开关 | `研究?默认` |
| `/%` | 模型列表 | `/%` |

> 普通房间的模型可写 `供应商/模型`，按 `[oai.providers]` 选接口；不带前缀走默认接口。
> 思考强度 `off`/`minimal`/`low`/`medium`/`high`，模型后的 `:强度` 优先于房间设置。
> 联网是**按需**的：开着只是让模型能查，搜不搜由它自己按这句话需不需要判断。

## 对话控制
| 指令 | 功能 |
|------|------|
| `智能体 内容` | 正常对话 |
| `~智能体 内容` | 临时对话 (一次性) |
| `"智能体 内容` | 文本回复对话 |
| `&智能体 内容` | 私有历史对话 |
| `智能体~` | 重新生成上一条 |
| `智能体!` | 停止生成 |

## 内置 Agent 房间
| 指令 | 效果 | 示例 |
|------|------|------|
| `##名称 pi` | 建一间 Agent 房间 | `##研究 pi` |
| `##名称 pi/模型` | 建房并指定模型 | `##研究 pi/deepseek/deepseek-flash` |
| `智能体%pi` | 已有房间转 Agent | `助手%pi` |
| `智能体%pi 模型` | 换 Agent 用的模型 | `助手%pi apilio/kimi-k3` |
| `智能体%中转站模型` | 转回中转站房间 | `助手%gpt-5.6-luna` |

> 房间名可以随便取，中文也行；决定引擎的是这条指令，不是名字。旧的 `pi` / `pi-*`
> 房间已自动带上该引擎，行为不变。
> 模型写 `供应商/模型`（如 `apilio/claude-opus-5`）或裸 id，按 `[oai.providers]` 取接口；
> 可加 `:强度`（如 `deepseek/deepseek-flash:high`），或在房间上单独设思考强度；
> 只写 `pi` 则用 `[oai].agent_default_model`。`/#` 里显示为 `内置 · 模型 · 思考:强度`。
> 公有、`&` 私有和 `~` 临时模式都适用，历史按原模式隔离。
> 它会自己调工具：读写文件、执行命令，查到的结果自己用进回答里。
> 房间提示词追加在内置系统提示词之后；支持图片、历史编辑/删除/清空/重新生成；
> 长回复卡片显示实际应答模型、耗时和工具轨迹。

## MJ 绘图房间
| 房间模型 | 直接操作 |
|------|------|
| `mj` | 输入提示词绘图；消息图片/引用图片自动作为垫图 |

> 引用 `mj` 返回的四宫格，回复任意一个或多个 `1`–`4` 即可放大；已完成的放大直接读取缓存。

## 图像生成房间 (gpt-image)
| 房间模型 | 直接操作 |
|------|------|
| `gpt-image-2.5-flare` / `gpt-image-2.5-sunburst` | 输入提示词直接出图；发图、引用图片或房间名后 @用户则作为垫图编辑 |

> 例：`##画图 gpt-image-2.5-flare` 创建房间，然后 `画图 一只在窗台晒太阳的橘猫`。
> 垫图：直接发送图片、引用图片，或在房间名后 @用户，再说修改要求；合计最多 4 张。
> 头像示例：`画图 @某位群友 把头像改成水彩风格`（使用聊天界面的真实 @提及）。与 Gemini 生图共用提取逻辑，房间名前的 @ 和 @全体不作为垫图。
> 可选参数：`--size 1536x1024`（或 `-s auto`）、`--quality high`（或 `-q low/medium/high/auto`）。
> 走图像接口的模型关键字由 `[oai].image_models` 配置，默认 `["gpt-image-2.5"]`。

## 画图预设房间
首次启动自动建好，在 `/#` 的「画图预设」分区里；名字中间的 `·` 是为了不被日常聊天误触发。

| 指令 | 画什么 |
|------|------|
{{presets}}

> 用法：`画·手办 一只戴眼镜的橘猫`；发图、引用图片或房间名后 @某人，就以那张图为垫图改图。
> 可选参数同上：`--size 1536x1024`、`--quality high`；加 `~` 前缀（`~画·手办 ...`）不留历史。
> 预设就是房间的系统提示词：`画·手办/$` 看一眼，`画·手办$自己的提示词` 改掉，
> `画·手办~#我的手办` 复制一份再改。删掉的房间不会在下次启动时复活。

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
            // 预设那张表跟着代码走：加一间房间不该忘了改帮助。
            let presets = super::presets::PRESETS
                .iter()
                .map(|preset| format!("| `{} 内容` | {} |", preset.name, preset.desc))
                .collect::<Vec<_>>()
                .join("\n");
            let help = help.replace("{{presets}}", &presets);
            reply(
                ctx,
                writer,
                &msg_event,
                &help,
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
            let mut success_count = 0;

            for (name, prompt) in target_agents {
                let gen_prompt = format!(
                    "请阅读以下角色的 System Prompt，为其生成一个极简短的中文功能描述（Role/Tag）。\n要求：\n1. 必须控制在 10 个字以内\n2. 不要包含任何标点符号\n3. 直接输出描述内容，不要解释\n\nSystem Prompt:\n{}",
                    prompt
                );
                let msgs = vec![LlmMessage::User {
                    content: vec![UserContent::Text(Text::new(gen_prompt))],
                }];

                if let Ok(content) = super::llm::complete(
                    &api_config.0,
                    &api_config.1,
                    &use_model,
                    msgs,
                    None,
                )
                .await
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
    // 建房时的模型位同样认 Pi 写法与供应商前缀：
    // `##研究 pi`、`##研究 pi/apilio/claude-opus-5`、`##研究 deepseek/deepseek-flash:high`。
    let (spec, thinking) = super::utils::split_thinking(model);
    let pi_model = super::agent::parse_pi_spec(&spec);
    let (engine, model) = match pi_model {
        Some(model) => (super::types::ENGINE_PI, model),
        None => match super::utils::split_provider(&spec) {
            // 带供应商前缀时保留原样，交给请求路由按供应商选接口。
            (Some(_), _) => (super::types::ENGINE_CHAT, spec.clone()),
            (None, bare) => (
                super::types::ENGINE_CHAT,
                mgr.resolve_model(&bare, &models).unwrap_or(bare),
            ),
        },
    };
    // 不再默认填充「你是一个有帮助的助手」：没写提示词就留空，让模型用裸提示词。
    // 生图房间尤其不该被一句通用预设污染提示词；普通房间也保持中立（MJ 同样留空）。
    let prompt = prompt.to_string();

    if let Some(a) = c.agents.iter_mut().find(|a| a.name == name) {
        // 省略模型位时只改提示词和描述，保留这个房间原来的引擎。
        if !model.is_empty() || engine == super::types::ENGINE_PI {
            a.set_engine(engine, &model);
        }
        if let Some(level) = &thinking {
            a.thinking = level.clone();
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
        if let Some(level) = thinking {
            agent.thinking = level;
        }
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

    #[tokio::test]
    async fn mentioned_avatars_reach_gpt_image_edits() {
        use crate::event::{BotStatus, EventType};
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        let png = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let ctx = Context {
            event: EventType::Satori(
                simd_json::serde::to_owned_value(serde_json::json!({
                    "post_type": "message",
                    "message": [
                        {"type":"at", "data":{"qq":"10000"}},
                        {"type":"text", "data":{"text":"画图 "}},
                        {"type":"at", "data":{"qq":"114514"}},
                        {"type":"at", "data":{"qq":1919810}},
                        {"type":"at", "data":{"qq":"all"}},
                        {"type":"image", "data":{"url":png}},
                        {"type":"text", "data":{"text":" 把头像改成水彩 --quality high"}}
                    ]
                }))
                .unwrap(),
            ),
            config: Arc::new(std::sync::RwLock::new(crate::config::AppConfig::default())),
            config_save_lock: Arc::new(tokio::sync::Mutex::new(())),
            db: sea_orm::Database::connect("sqlite::memory:").await.unwrap(),
            scheduler: Arc::new(crate::scheduler::Scheduler::new()),
            matcher: Arc::new(crate::matcher::Matcher::new()),
            config_path: Arc::from("unused-image-test.toml"),
            bot: Arc::new(BotStatus::default()),
        };
        let writer = Arc::new(crate::adapters::satori::SatoriClient::console());
        let raw = super::super::extract_clean_text(&ctx).unwrap();
        let cmd = super::super::parser::parse_agent_cmd(&raw, &["画图".into()]).unwrap();
        let (quote, images) =
            super::super::utils::get_full_content(&ctx, &writer, Some(&cmd.agent)).await;
        assert!(quote.is_empty());
        assert_eq!(
            images,
            vec![
                "https://q.qlogo.cn/g?b=qq&nk=114514&s=640".to_string(),
                "https://q.qlogo.cn/g?b=qq&nk=1919810&s=640".to_string(),
                png.to_string(),
            ]
        );
        // Seed the normal download cache: no QQ avatar or model service is contacted.
        for url in &images[..2] {
            remember_data_url(url, png);
        }
        let history = prepare_history(&[], &cmd.args, &images, false);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            // Both image-model variants must upload the avatars through edits.
            for model in ["gpt-image-2.5-flare", "gpt-image-2.5-sunburst"] {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                assert_eq!(line.trim(), "POST /v1/images/edits HTTP/1.1");
                let mut length = 0;
                loop {
                    line.clear();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).await.unwrap();
                let form = String::from_utf8_lossy(&body);
                assert_eq!(form.matches("name=\"image[]\"").count(), 3);
                assert_eq!(
                    body.windows(4).filter(|bytes| *bytes == b"\x89PNG").count(),
                    3
                );
                assert!(form.contains(model));
                assert!(form.contains("把头像改成水彩"));
                assert!(form.contains("name=\"quality\"\r\n\r\nhigh"));
                let response = r#"{"data":[{"url":"https://example.invalid/result.png"}]}"#;
                reader.get_mut().write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.len(), response,
                ).as_bytes()).await.unwrap();
            }
        });
        for model in ["gpt-image-2.5-flare", "gpt-image-2.5-sunburst"] {
            let config = super::super::OaiConfig::default();
            assert!(super::super::images::is_images_model(
                model,
                &config.image_models
            ));
            let agent = Agent::new("画图", model, "", "");
            let reply = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                super::super::images::generate_reply(&base, "test-only", &agent, &history),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(
                reply
                    .text
                    .contains("![image](https://example.invalid/result.png)")
            );
        }
        server.await.unwrap();
    }

    #[test]
    fn ordinary_chat_request_has_no_tools() {
        let request = super::super::llm::request(
            vec![LlmMessage::User {
                content: vec![UserContent::Text(Text::new("test"))],
            }],
            Vec::new(),
            None,
            None,
        );
        assert!(request.tools.is_empty());
        assert!(request.additional_params.is_none());
    }

    /// 房间设了思考强度时，普通房间把它转成 `reasoning_effort` 发给接口。
    #[test]
    fn rooms_with_a_thinking_level_send_reasoning_effort() {
        let messages = || {
            vec![LlmMessage::User {
                content: vec![UserContent::Text(Text::new("test"))],
            }]
        };
        for (level, expected) in [("high", "high"), ("off", "none")] {
            let request = super::super::llm::request(messages(), Vec::new(), Some(level), None);
            assert_eq!(
                request.additional_params,
                Some(serde_json::json!({ "reasoning_effort": expected })),
                "{level}"
            );
        }
        // 非法档位不写入参数，保持请求干净。
        let request = super::super::llm::request(messages(), Vec::new(), Some("nonsense"), None);
        assert!(request.additional_params.is_none());
    }

    /// 普通房间真的发出去时也不带工具：走一遍真实 HTTP，看请求体里有没有 tools。
    #[tokio::test]
    async fn ordinary_completion_over_the_wire_is_tool_free() {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            assert_eq!(line.trim(), "POST /v1/chat/completions HTTP/1.1");
            let mut length = 0;
            loop {
                line.clear();
                reader.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).await.unwrap();
            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert!(request.get("tools").is_none(), "{request}");
            assert_eq!(request["reasoning_effort"], "high");
            assert_eq!(request["messages"][0]["role"], "user");
            let response = r#"{"id":"x","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"好的"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
            reader
                .get_mut()
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response.len(),
                        response,
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });

        let text = super::super::llm::complete(
            &base,
            "test-only",
            "example-model",
            vec![LlmMessage::User {
                content: vec![UserContent::Text(Text::new("test"))],
            }],
            Some("high"),
        )
        .await
        .unwrap();
        assert_eq!(text, "好的");
        server.await.unwrap();
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

    /// 新建房间没写提示词时不再默认填充「你是一个有帮助的助手」，生图房间尤其如此。
    #[tokio::test]
    async fn new_agents_without_a_prompt_keep_an_empty_system_prompt() {
        use crate::event::{BotStatus, EventType};

        let dir = std::env::temp_dir().join(format!("oai-noprompt-{:032x}", rand::random::<u128>()));
        let mgr = Arc::new(Manager::new(dir.clone()));
        let ctx = Context {
            event: EventType::Satori(
                simd_json::serde::to_owned_value(serde_json::json!({
                    "post_type": "message",
                    "message_type": "group",
                    "group_id": 1,
                    "user_id": 42,
                    "message_id": 7,
                    "time": 1_788_800_000_i64,
                    "message": [{"type": "text", "data": {"text": "画图"}}],
                }))
                .unwrap(),
            ),
            config: Arc::new(std::sync::RwLock::new(crate::config::AppConfig::default())),
            config_save_lock: Arc::new(tokio::sync::Mutex::new(())),
            db: sea_orm::Database::connect("sqlite::memory:").await.unwrap(),
            scheduler: Arc::new(crate::scheduler::Scheduler::new()),
            matcher: Arc::new(crate::matcher::Matcher::new()),
            config_path: Arc::from("unused-noprompt.toml"),
            bot: Arc::new(BotStatus::default()),
        };
        let writer = Arc::new(crate::adapters::satori::SatoriClient::console());

        // 生图房间：没给提示词，系统提示词应当留空，避免「帮助者」预设污染绘图提示词。
        handle_create("画图", "", "gpt-image-2.5-flare", "", &ctx, &writer, &mgr).await;
        let agents = mgr.config.read().await.agents.clone();
        let image_room = agents.iter().find(|a| a.name == "画图").unwrap();
        assert!(
            image_room.system_prompt.is_empty(),
            "无提示词的生图房间不应被默认填充：{}",
            image_room.system_prompt
        );
        assert_eq!(image_room.model, "gpt-image-2.5-flare");

        // 普通房间同样不默认填充。
        handle_create("助手", "", "gpt-5.6-luna", "", &ctx, &writer, &mgr).await;
        let agents = mgr.config.read().await.agents.clone();
        let assistant = agents.iter().find(|a| a.name == "助手").unwrap();
        assert!(
            assistant.system_prompt.is_empty(),
            "无提示词的普通房间不应被默认填充：{}",
            assistant.system_prompt
        );

        std::fs::remove_dir_all(dir).unwrap();
    }
}
