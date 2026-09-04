use crate::adapters::satori::{LockedWriter, send_msg};
use crate::command::{extract_text_arg, get_image_url, match_command};
use crate::config::build_config;
use crate::event::Context;
use crate::http::download_bytes;
use crate::message::Message;
use crate::plugins::{PluginError, PluginResult};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use toml::Value;

pub mod gif_ops;
pub mod utils;

// =============================
//      Main Plugin Logic
// =============================

/// 帮助信息
const HELP_TEXT: &str = r#"📝 指令列表 (大小写均可):

• gif帮助 / gifhelp - 显示本帮助
• 合成gif [行x列] [间隔秒] [边距]
    将网格图合成为动图
    示例: 合成gif 3x3 0.1 0
• gif拼图 [列数] - 将动图转为网格图
• gif拆分 - 将动图拆成多张静态图
• gif变速 [倍率] - 调整播放速度
    示例: gif变速 2 (加速2倍)
• gif倒放 - 倒序播放
• gif缩放 [倍率|尺寸]
    示例: gif缩放 0.5 或 gif缩放 100x100
• gif旋转 [角度] - 旋转 (90/180/270/-90)
• gif翻转 [水平|垂直] - 镜像翻转
• gif信息 - 查看 GIF 详情

💡 使用时请附带图片或引用图片消息"#;

/// 支持的指令
const COMMANDS: &[&str] = &[
    "gif帮助",
    "gifhelp",
    "合成gif",
    "gif变速",
    "gif倒放",
    "gif信息",
    "gif缩放",
    "gif旋转",
    "gif翻转",
    "gif拆分",
    "gif拼图",
];

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Config {
    enabled: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { enabled: true }
    }
}

pub fn default_config() -> Value {
    build_config(Config::default())
}

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, std::result::Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        let msg = match ctx.as_message() {
            Some(m) => m,
            None => return Ok(Some(ctx)),
        };

        for &cmd in COMMANDS {
            if let Some(matched) = match_command(&ctx, cmd) {
                let group_id = msg.group_id();
                let user_id = msg.user_id();

                // 提取纯文本参数
                let args_text = extract_text_arg(&matched.args);
                let args: Vec<&str> = args_text.split_whitespace().collect();

                // 3. 帮助指令
                if matches!(cmd, "gif帮助" | "gifhelp") {
                    let _ = send_msg(&ctx, writer, group_id, Some(user_id), HELP_TEXT).await;
                    return Ok(None);
                }

                // 4. 获取图片
                let img_url = match get_image_url(
                    &ctx,
                    writer.clone(),
                    &matched.args,
                    matched.reply_id.as_ref(),
                )
                .await
                {
                    Some(u) => u,
                    None => {
                        let _ = send_msg(
                            &ctx,
                            writer,
                            group_id,
                            Some(user_id),
                            "❌ 请附带图片或引用图片消息。",
                        )
                        .await;
                        return Ok(None);
                    }
                };

                let _ = send_msg(
                    &ctx,
                    writer.clone(),
                    group_id,
                    Some(user_id),
                    "⏳ 处理中...",
                )
                .await;

                let img_bytes = match download_bytes(&img_url).await {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = send_msg(
                            &ctx,
                            writer,
                            group_id,
                            Some(msg.user_id()),
                            format!("❌ 图片下载失败：{}", e),
                        )
                        .await;
                        return Ok(None);
                    }
                };

                // 5. 处理逻辑分发
                let res: PluginResult<Option<String>> = match cmd {
                    "合成gif" => {
                        let (rows, cols) = args
                            .first()
                            .and_then(|s| utils::parse_grid_dim(s))
                            .unwrap_or((3, 3));
                        let interval = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.1);
                        let margin = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
                        gif_ops::grid_to_gif(img_bytes, rows, cols, interval, margin).map(Some)
                    }
                    "gif变速" => {
                        let factor = args.first().and_then(|s| s.parse().ok()).unwrap_or(2.0);
                        gif_ops::process_gif(img_bytes, gif_ops::Transform::Speed(factor)).map(Some)
                    }
                    "gif倒放" => {
                        gif_ops::process_gif(img_bytes, gif_ops::Transform::Reverse).map(Some)
                    }
                    "gif信息" => match gif_ops::gif_info(img_bytes) {
                        Ok(info) => {
                            let _ =
                                send_msg(&ctx, writer.clone(), group_id, Some(user_id), info).await;
                            Ok(None)
                        }
                        Err(e) => Err(e),
                    },
                    "gif缩放" => {
                        let op = args.first().map_or(gif_ops::Transform::Scale(0.5), |s| {
                            if let Some((w, h)) = utils::parse_grid_dim(s) {
                                gif_ops::Transform::Resize(w, h)
                            } else {
                                gif_ops::Transform::Scale(s.parse().unwrap_or(0.5))
                            }
                        });
                        gif_ops::process_gif(img_bytes, op).map(Some)
                    }
                    "gif旋转" => {
                        let deg = args.first().and_then(|s| s.parse().ok()).unwrap_or(90);
                        gif_ops::process_gif(img_bytes, gif_ops::Transform::Rotate(deg)).map(Some)
                    }
                    "gif翻转" => {
                        let op = args.first().map(|s| s.to_lowercase()).as_deref().map_or(
                            gif_ops::Transform::FlipH,
                            |s| {
                                if matches!(s, "垂直" | "v" | "vertical" | "纵向") {
                                    gif_ops::Transform::FlipV
                                } else {
                                    gif_ops::Transform::FlipH
                                }
                            },
                        );
                        gif_ops::process_gif(img_bytes, op).map(Some)
                    }
                    "gif拼图" => {
                        let cols = args.first().and_then(|s| s.parse().ok());
                        gif_ops::gif_to_grid(img_bytes, cols).map(Some)
                    }
                    "gif拆分" => match gif_ops::gif_to_frames(img_bytes) {
                        Ok(list) => {
                            send_forward_msg(&ctx, writer.clone(), list).await;
                            Ok(None)
                        }
                        Err(e) => Err(e),
                    },
                    _ => Ok(None),
                };

                // 6. 发送结果
                match res {
                    Ok(Some(b64)) => {
                        let reply = Message::new().image(format!("base64://{}", b64));
                        let _ = send_msg(&ctx, writer, group_id, Some(user_id), reply).await;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        let _ = send_msg(
                            &ctx,
                            writer,
                            group_id,
                            Some(user_id),
                            format!("❌ 处理失败：{}", e),
                        )
                        .await;
                    }
                }

                return Ok(None);
            }
        }

        Ok(Some(ctx))
    })
}

/// 发送合并转发消息
async fn send_forward_msg(ctx: &Context, writer: LockedWriter, base64_list: Vec<String>) {
    let bot_id = &ctx.bot.login_user.id;

    // 预处理列表：防止风控
    let (process_list, is_truncated) = if base64_list.len() > 99 {
        (&base64_list[0..99], true)
    } else {
        (base64_list.as_slice(), false)
    };

    let msg = ctx.as_message().unwrap();
    let group_id = msg.group_id();
    let user_id = msg.user_id();

    if is_truncated {
        let _ = send_msg(
            ctx,
            writer.clone(),
            group_id,
            Some(user_id),
            "⚠️ 切片数量过多，为防止风控，仅发送前 99 张。",
        )
        .await;
    }

    // 构建节点消息
    let mut forward_msg = Message::new();
    for (index, b64) in process_list.iter().enumerate() {
        let content = Message::new().image(format!("base64://{}", b64));
        forward_msg = forward_msg.node_custom(bot_id.clone(), format!("图 {}", index + 1), content);
    }

    // 调用通用 API
    let _ = send_msg(ctx, writer, group_id, Some(user_id), forward_msg).await;
}
