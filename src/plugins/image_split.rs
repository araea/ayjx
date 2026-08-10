use crate::adapters::onebot::{LockedWriter, api, send_msg};
use crate::command::match_command;
use crate::config::build_config;
use crate::event::Context;
use crate::message::Message;
use crate::plugins::{PluginError, get_config};
use futures_util::future::BoxFuture;
use regex::Regex;
use serde::{Deserialize, Serialize};
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsScalar};
use simd_json::prelude::ValueAsScalar;
use std::sync::OnceLock;
use tokio::task;
use toml::Value;

pub mod processing;

// ================= 配置定义 =================

#[derive(Serialize, Deserialize)]
struct Config {
    enabled: bool,
    // 最大切片行列限制，防止恶意消耗资源
    max_rows: u32,
    max_cols: u32,
}

pub fn default_config() -> Value {
    build_config(Config {
        enabled: true,
        max_rows: 10,
        max_cols: 10,
    })
}

// ================= 正则与工具 =================

static ARGS_REGEX: OnceLock<Regex> = OnceLock::new();

fn get_args_regex() -> &'static Regex {
    // 匹配参数部分，例如 "3x3" 或 "3 3"
    ARGS_REGEX.get_or_init(|| Regex::new(r"^(\d+)\s*(?:[\*xX× ])\s*(\d+)$").unwrap())
}

// ================= 插件入口 =================

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, std::result::Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        let msg = match ctx.as_message() {
            Some(m) => m,
            None => return Ok(Some(ctx)),
        };

        let config: Config = get_config(&ctx, "image_split")
            .unwrap_or_else(|| serde::Deserialize::deserialize(default_config()).unwrap());

        // 支持的指令列表
        let commands = ["裁剪", "切图", "分割"];

        for cmd in commands {
            if let Some(matched) = match_command(&ctx, cmd) {
                // 1. 解析参数 (从 matched.args 提取纯文本)
                let mut args_text = String::new();
                for seg in &matched.args {
                    if seg.get_str("type") == Some("text")
                        && let Some(t) = seg.get("data").and_then(|d| d.get_str("text"))
                    {
                        args_text.push_str(t);
                    }
                }
                let args_text = args_text.trim();

                // 2. 匹配参数正则
                let (rows, cols) = match get_args_regex().captures(args_text) {
                    Some(caps) => {
                        let r = caps.get(1).unwrap().as_str().parse::<u32>().unwrap_or(3);
                        let c = caps.get(2).unwrap().as_str().parse::<u32>().unwrap_or(3);
                        (r, c)
                    }
                    None => {
                        // 如果没有参数，默认 3x3 或者提示用户
                        if args_text.is_empty() {
                            (3, 3)
                        } else {
                            // 参数格式不对，跳过或返回
                            continue;
                        }
                    }
                };

                // 检查限制
                if rows > config.max_rows || cols > config.max_cols {
                    let _ = send_msg(
                        &ctx,
                        writer,
                        msg.group_id(),
                        Some(msg.user_id()),
                        format!(
                            "❌ 切片数量过多，最大支持 {}x{}。",
                            config.max_rows, config.max_cols
                        ),
                    )
                    .await;
                    return Ok(None);
                }

                if rows == 0 || cols == 0 {
                    return Ok(None);
                }

                // 3. 获取图片 URL (优先指令参数，其次引用消息)
                let mut target_url = None;

                // 检查参数中的图片
                for seg in &matched.args {
                    if seg.get_str("type") == Some("image")
                        && let Some(url) = seg.get("data").and_then(|d| d.get_str("url"))
                    {
                        target_url = Some(url.to_string());
                        break;
                    }
                }

                // 检查引用消息
                if target_url.is_none() {
                    let reply_id = matched
                        .reply_id
                        .as_deref()
                        .and_then(|s| s.parse::<i32>().ok());

                    if let Some(rid) = reply_id
                        && let Ok(reply_msg) = api::get_msg(&ctx, writer.clone(), rid).await
                    {
                        for seg in &reply_msg.message.0 {
                            if seg.type_ == "image"
                                && let Some(url) = seg.data.get("url").and_then(|v| v.as_str())
                            {
                                target_url = Some(url.to_string());
                                break;
                            }
                        }
                    }
                }

                let url = match target_url {
                    Some(u) => u,
                    None => {
                        let _ = send_msg(
                            &ctx,
                            writer,
                            msg.group_id(),
                            Some(msg.user_id()),
                            "⚠️ 请在发送指令时附带图片，或引用一张图片。",
                        )
                        .await;
                        return Ok(None);
                    }
                };

                let _ = send_msg(
                    &ctx,
                    writer.clone(),
                    msg.group_id(),
                    Some(msg.user_id()),
                    format!("🔪 正在将图片切成 {} 行 × {} 列，请稍候...", rows, cols),
                )
                .await;

                // 4. 下载与处理
                let img_bytes = match processing::download_image(&url).await {
                    Ok(b) => b,
                    Err(e) => {
                        error!(target: "Plugin/ImageSplitter", "下载失败: {}", e);
                        let _ = send_msg(
                            &ctx,
                            writer,
                            msg.group_id(),
                            Some(msg.user_id()),
                            "❌ 图片下载失败。",
                        )
                        .await;
                        return Ok(None);
                    }
                };

                let split_task = task::spawn_blocking(move || {
                    processing::split_image_blocking(img_bytes, rows, cols)
                });

                match split_task.await {
                    Ok(Ok(base64_list)) => {
                        let bot_id = &ctx.bot.login_user.id;
                        let mut forward_node_msg = Message::new();

                        for (index, b64) in base64_list.into_iter().enumerate() {
                            let image_content = Message::new().image(format!("base64://{}", b64));
                            forward_node_msg = forward_node_msg.node_custom(
                                bot_id,
                                format!("图 {}", index + 1),
                                image_content,
                            );
                        }

                        if let Err(e) = send_msg(
                            &ctx,
                            writer.clone(),
                            msg.group_id(),
                            Some(msg.user_id()),
                            forward_node_msg,
                        )
                        .await
                        {
                            error!(target: "Plugin/ImageSplitter", "发送合并转发失败: {}", e);
                            let _ = send_msg(
                                &ctx,
                                writer,
                                msg.group_id(),
                                Some(msg.user_id()),
                                "❌ 发送合并转发消息失败，可能是风控或API不支持。",
                            )
                            .await;
                        }
                    }
                    Ok(Err(e)) => {
                        let _ = send_msg(
                            &ctx,
                            writer,
                            msg.group_id(),
                            Some(msg.user_id()),
                            format!("❌ 处理失败：{}", e),
                        )
                        .await;
                    }
                    Err(e) => {
                        error!(target: "Plugin/ImageSplitter", "Task join error: {}", e);
                    }
                }

                return Ok(None);
            }
        }

        Ok(Some(ctx))
    })
}
