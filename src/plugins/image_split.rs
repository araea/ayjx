use crate::adapters::satori::{LockedWriter, send_msg};
use crate::command::{extract_text_arg, first_command_match, get_image_url};
use crate::config::build_config;
use crate::event::Context;
use crate::http::download_bytes;
use crate::message::Message;
use crate::plugins::{PluginError, get_config_or_default};
use futures_util::future::BoxFuture;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use tokio::task;
use toml::Value;

pub mod processing;

// ================= 配置定义 =================

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Config {
    enabled: bool,
    // 最大切片行列限制，防止恶意消耗资源
    max_rows: u32,
    max_cols: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            max_rows: 10,
            max_cols: 10,
        }
    }
}

pub fn default_config() -> Value {
    build_config(Config::default())
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

        let config: Config = get_config_or_default(&ctx, "image_split");

        // 支持的指令列表
        let commands = ["裁剪", "切图", "分割"];

        if let Some(matched) = first_command_match(&ctx, &commands) {
            // 1. 解析参数 (从 matched.args 提取纯文本)
            let args_text = extract_text_arg(&matched.args);

            // 2. 匹配参数正则
            let (rows, cols) = match get_args_regex().captures(&args_text) {
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
                        return Ok(Some(ctx));
                    }
                }
            };

            // 检查限制
            if rows > config.max_rows || cols > config.max_cols {
                send_msg(
                    &ctx,
                    writer,
                    msg.group_id(),
                    Some(msg.user_id()),
                    format!(
                        "❌ 切片数量过多，最大支持 {}x{}。",
                        config.max_rows, config.max_cols
                    ),
                )
                .await?;
                return Ok(None);
            }

            if rows == 0 || cols == 0 {
                return Ok(None);
            }

            // 3. 获取图片 URL (优先指令参数，其次引用消息)
            let url = match get_image_url(&ctx, writer.clone(), &matched.args, matched.reply_id.as_ref())
                .await
            {
                Some(u) => u,
                None => {
                    send_msg(
                        &ctx,
                        writer,
                        msg.group_id(),
                        Some(msg.user_id()),
                        "⚠️ 请在发送指令时附带图片，或引用一张图片。",
                    )
                    .await?;
                    return Ok(None);
                }
            };

            send_msg(
                &ctx,
                writer.clone(),
                msg.group_id(),
                Some(msg.user_id()),
                format!("🔪 正在将图片切成 {} 行 × {} 列，请稍候...", rows, cols),
            )
            .await?;

            // 4. 下载与处理
            let img_bytes = match download_bytes(&url).await {
                Ok(b) => b,
                Err(e) => {
                    error!(target: "Plugin/ImageSplitter", "下载失败: {}", e);
                    send_msg(
                        &ctx,
                        writer,
                        msg.group_id(),
                        Some(msg.user_id()),
                        "❌ 图片下载失败。",
                    )
                    .await?;
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
                        forward_node_msg =
                            forward_node_msg.node_custom(bot_id, format!("图 {}", index + 1), image_content);
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
                        send_msg(
                            &ctx,
                            writer,
                            msg.group_id(),
                            Some(msg.user_id()),
                            "❌ 发送合并转发消息失败，可能是风控或API不支持。",
                        )
                        .await?;
                    }
                }
                Ok(Err(e)) => {
                    send_msg(
                        &ctx,
                        writer,
                        msg.group_id(),
                        Some(msg.user_id()),
                        format!("❌ 处理失败：{}", e),
                    )
                    .await?;
                }
                Err(e) => {
                    error!(target: "Plugin/ImageSplitter", "Task join error: {}", e);
                }
            }

            return Ok(None);
        }

        Ok(Some(ctx))
    })
}
