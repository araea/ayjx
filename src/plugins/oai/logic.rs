use super::data::Manager;
use super::parser::{Action, Command, Scope};
use super::types::{Agent, ChatMessage};
use super::utils::{escape_markdown_special, format_export_txt, format_history, render_md};
use crate::adapters::onebot::{LockedWriter, api, send_msg};
use crate::event::{Context, MessageEvent};
use crate::message::Message;
use async_openai::{
    Client,
    config::OpenAIConfig,
    types::chat::{
        ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
        ChatCompletionRequestMessageContentPartImageArgs,
        ChatCompletionRequestMessageContentPartTextArgs, ChatCompletionRequestSystemMessageArgs,
        ChatCompletionRequestUserMessageArgs, CreateChatCompletionRequestArgs, ImageUrlArgs,
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
    match render_md(text, header).await {
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
        Err(_) => {
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

    if api.0.is_empty() || api.1.is_empty() {
        reply_text(ctx, writer, &event, "❌ API 未配置。").await;
        return;
    }

    let _ = api::set_msg_emoji_like(
        ctx,
        writer.clone(),
        event.message_id().try_into().unwrap(),
        124,
        true,
    )
    .await;

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

    let client = Client::with_config(OpenAIConfig::new().with_api_base(api.0).with_api_key(api.1));
    let mut msgs: Vec<ChatCompletionRequestMessage> = vec![];

    let model_lower = agent.model.to_lowercase();
    let force_user_role_for_system = [
        "nano-banana",
        "gemini-2.5-flash-image",
        "gemini-3-pro-image",
    ]
    .iter()
    .any(|kw| model_lower.contains(kw));

    let mut pending_sys_prompt = if !agent.system_prompt.is_empty() {
        Some(agent.system_prompt.clone())
    } else {
        None
    };

    if !force_user_role_for_system
        && let Some(sp) = pending_sys_prompt.take() {
            msgs.push(
                ChatCompletionRequestSystemMessageArgs::default()
                    .content(sp)
                    .build()
                    .unwrap()
                    .into(),
            );
        }

    let re = Regex::new(r"!\[.*?\]\((data:image/[^\s\)]+)\)").unwrap();
    for m in &hist {
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
            for url in &m.images {
                parts.push(
                    ChatCompletionRequestMessageContentPartImageArgs::default()
                        .image_url(ImageUrlArgs::default().url(url).build().unwrap())
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

    let req = match CreateChatCompletionRequestArgs::default()
        .model(&agent.model)
        .messages(msgs)
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            if !temp_mode {
                mgr.generating
                    .write()
                    .await
                    .set_generating(name, is_priv_ctx, &uid, false);
            }
            reply_text(ctx, writer, &event, format!("❌ 请求构建失败：{}", e)).await;
            return;
        }
    };

    match tokio::time::timeout(
        std::time::Duration::from_secs(300),
        client.chat().create(req),
    )
    .await
    {
        Err(_) => {
            if !temp_mode {
                mgr.generating
                    .write()
                    .await
                    .set_generating(name, is_priv_ctx, &uid, false);
            }
            reply_text(
                ctx,
                writer,
                &event,
                "⏳ 请求超时：模型响应时间超过 5 分钟，已强制停止。",
            )
            .await;
        }
        Ok(result) => match result {
            Ok(res) => {
                if !temp_mode {
                    mgr.generating
                        .write()
                        .await
                        .set_generating(name, is_priv_ctx, &uid, false);
                }

                if !temp_mode {
                    let c = mgr.config.read().await;
                    if let Some(a) = c.agents.iter().find(|a| a.name == name)
                        && a.generation_id != gen_id
                    {
                        return;
                    }
                }

                if let Some(choice) = res.choices.first()
                    && let Some(content) = &choice.message.content
                {
                    let msg_index = if temp_mode {
                        0
                    } else {
                        let c = mgr.config.read().await;
                        if let Some(a) = c.agents.iter().find(|a| a.name == name) {
                            a.history(is_priv_ctx, &uid).len() + 1
                        } else {
                            0
                        }
                    };

                    if !temp_mode {
                        let mut c = mgr.config.write().await;
                        if let Some(a) = c.agents.iter_mut().find(|a| a.name == name) {
                            a.history_mut(is_priv_ctx, &uid).push(ChatMessage::new(
                                "assistant",
                                content,
                                vec![],
                            ));
                        }
                        mgr.save(&c);
                    }

                    let image_urls = extract_image_urls(content);
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
                        let re =
                            Regex::new(r"!\[.*?\]\(((?:https?://|data:image/)[^\s\)]+)\)").unwrap();
                        re.replace_all(content, |caps: &regex::Captures| {
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

                    reply(
                        ctx,
                        writer,
                        &event,
                        &reply_text_content,
                        cmd.text_mode,
                        &header,
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

                    let video_urls = extract_video_urls(content);
                    for url in video_urls {
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
            Err(e) => {
                {
                    mgr.generating
                        .write()
                        .await
                        .set_generating(name, is_priv_ctx, &uid, false);
                }
                reply_text(ctx, writer, &event, format!("❌ API 错误：{}", e)).await;
            }
        },
    }
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
                    .with_api_base(api_config.0)
                    .with_api_key(api_config.1),
            );
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
    let prompt = if prompt.is_empty() && !c.agents.iter().any(|a| a.name == name) {
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
