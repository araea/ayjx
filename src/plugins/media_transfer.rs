use crate::adapters::onebot::{LockedWriter, api, send_msg};
use crate::command::match_command;
use crate::config::build_config;
use crate::event::Context;
use crate::message::Message;
use crate::plugins::{PluginError, get_config};
use futures_util::future::BoxFuture;
use regex::Regex;
use serde::{Deserialize, Serialize};
use simd_json::OwnedValue;
use simd_json::base::ValueAsScalar;
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsScalar};
use std::sync::OnceLock;
use toml::Value;

#[derive(Serialize, Deserialize)]
struct Config {
    enabled: bool,
    cmd_to_url: Vec<String>,
    cmd_to_media: Vec<String>,
}

pub fn default_config() -> Value {
    build_config(Config {
        enabled: true,
        cmd_to_url: vec![
            "转链接".into(),
            "看链接".into(),
            "提取地址".into(),
            "url".into(),
        ],
        cmd_to_media: vec!["转图片".into(), "转视频".into(), "预览".into()],
    })
}

static URL_REGEX: OnceLock<Regex> = OnceLock::new();

fn get_url_regex() -> &'static Regex {
    URL_REGEX.get_or_init(|| Regex::new(r"https?://[^\s\u4e00-\u9fa5]+").expect("Invalid Regex"))
}

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        let config: Config = get_config(&ctx, "media_transfer")
            .unwrap_or_else(|| serde::Deserialize::deserialize(default_config()).unwrap());

        if !config.enabled {
            return Ok(Some(ctx));
        }

        // 功能 1: 转链接 (Media -> URL)
        for cmd in &config.cmd_to_url {
            if let Some(matched) = match_command(&ctx, cmd) {
                return handle_to_url(ctx, writer, matched).await;
            }
        }

        // 功能 2: 转媒体 (URL -> Media)
        for cmd in &config.cmd_to_media {
            if let Some(matched) = match_command(&ctx, cmd) {
                let is_video_cmd = cmd.contains("视频");
                return handle_to_media(ctx, writer, matched, is_video_cmd).await;
            }
        }

        Ok(Some(ctx))
    })
}

async fn handle_to_url(
    ctx: Context,
    writer: LockedWriter,
    matched: crate::command::CommandMatch,
) -> Result<Option<Context>, PluginError> {
    let msg = ctx.as_message().unwrap();

    // 1. 检查指令后的参数中是否直接包含图片/视频
    if let Some((url, type_name)) = find_media_in_segments(&matched.args) {
        let reply = Message::new()
            .reply(msg.message_id())
            .text(format!("🔗 已提取{}：\n{}", type_name, url));
        send_msg(&ctx, writer, msg.group_id(), Some(msg.user_id()), reply).await?;
        return Ok(None);
    }

    // 2. 检查引用消息
    if let Some(reply_id_str) = matched.reply_id
        && let Ok(reply_id) = reply_id_str.parse::<i32>()
            && let Ok(res) = api::get_msg(&ctx, writer.clone(), reply_id).await {
                for seg in res.message.0 {
                    let type_ = seg.type_.as_str();
                    if type_ == "image" {
                        if let Some(url) = seg.data.get("url").and_then(|v| v.as_str()) {
                            let reply = Message::new()
                                .reply(msg.message_id())
                                .text(format!("🔗 已提取图片：\n{}", url));
                            send_msg(&ctx, writer, msg.group_id(), Some(msg.user_id()), reply)
                                .await?;
                            return Ok(None);
                        }
                    } else if type_ == "video" {
                        let url_opt = seg
                            .data
                            .get("url")
                            .or_else(|| seg.data.get("file"))
                            .and_then(|v| v.as_str());

                        if let Some(url) = url_opt {
                            let reply = Message::new()
                                .reply(msg.message_id())
                                .text(format!("🔗 已提取视频：\n{}", url));
                            send_msg(&ctx, writer, msg.group_id(), Some(msg.user_id()), reply)
                                .await?;
                            return Ok(None);
                        }
                    }
                }
            }

    send_msg(
        &ctx,
        writer,
        msg.group_id(),
        Some(msg.user_id()),
        Message::new().reply(msg.message_id()).text(
            "⚠️ 未检测到媒体文件。\n请【引用】一条包含图片或视频的消息，或在发送指令时附带图片。",
        ),
    )
    .await?;

    Ok(None)
}

fn find_media_in_segments(segments: &[OwnedValue]) -> Option<(String, String)> {
    for seg in segments {
        let type_ = seg.get_str("type").unwrap_or("");
        let data = seg.get("data");

        if type_ == "image" {
            if let Some(url) = data.and_then(|d| d.get_str("url")) {
                return Some((url.to_string(), "图片".to_string()));
            }
        } else if type_ == "video"
            && let Some(url) = data.and_then(|d| d.get_str("url").or(d.get_str("file"))) {
                return Some((url.to_string(), "视频".to_string()));
            }
    }
    None
}

async fn handle_to_media(
    ctx: Context,
    writer: LockedWriter,
    matched: crate::command::CommandMatch,
    is_video_cmd: bool,
) -> Result<Option<Context>, PluginError> {
    let msg = ctx.as_message().unwrap();
    let regex = get_url_regex();

    // 1. 尝试从指令参数中提取 URL
    let mut target_url = None;
    for seg in &matched.args {
        if seg.get_str("type") == Some("text")
            && let Some(text) = seg.get("data").and_then(|d| d.get_str("text"))
                && let Some(m) = regex.find(text) {
                    target_url = Some(m.as_str().to_string());
                    break;
                }
    }

    // 2. 如果参数没有 URL，尝试从引用消息的文本中提取
    if target_url.is_none()
        && let Some(reply_id_str) = matched.reply_id
            && let Ok(reply_id) = reply_id_str.parse::<i32>()
                && let Ok(res) = api::get_msg(&ctx, writer.clone(), reply_id).await {
                    for seg in &res.message.0 {
                        if seg.type_ == "text"
                            && let Some(text) = seg.data.get("text").and_then(|v| v.as_str())
                                && let Some(m) = regex.find(text) {
                                    target_url = Some(m.as_str().to_string());
                                    break;
                                }
                    }
                }

    if let Some(url) = target_url {
        // 判断是否发送为视频
        // 1. 指令中包含 "视频" 二字
        // 2. 链接以常见视频后缀结尾
        let is_video = is_video_cmd || url.ends_with(".mp4") || url.ends_with(".mov");

        let reply = if is_video {
            Message::new().reply(msg.message_id()).video(url)
        } else {
            Message::new().reply(msg.message_id()).image(url)
        };
        send_msg(&ctx, writer, msg.group_id(), Some(msg.user_id()), reply).await?;
    } else {
        send_msg(
            &ctx,
            writer,
            msg.group_id(),
            Some(msg.user_id()),
            Message::new()
                .reply(msg.message_id())
                .text("⚠️ 未检测到有效链接。\n请在指令后附带 URL，或【引用】一条包含 URL 的消息。"),
        )
        .await?;
    }

    Ok(None)
}
