use crate::adapters::satori::{LockedWriter, api, send_msg};
use crate::command::first_command_match;
use crate::config::build_config;
use crate::event::Context;
use crate::message::Message;
use crate::plugins::{PluginError, get_config_or_default};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use simd_json::base::ValueAsScalar;
use toml::Value;

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

// 收藏指令（内置，无需配置）
const COMMANDS: &[&str] = &["表情转图片", "收", "偷", "存表情"];
// 保存成功后是否撤回触发指令（内置，默认关闭）
const RECALL_AFTER_SAVE: bool = false;

pub fn default_config() -> Value {
    build_config(Config::default())
}

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        let msg = match ctx.as_message() {
            Some(m) => m,
            None => return Ok(Some(ctx)),
        };

        let config: Config = get_config_or_default(&ctx, "sticker");

        if !config.enabled {
            return Ok(Some(ctx));
        }

        if let Some(matched) = first_command_match(&ctx, COMMANDS) {
                // 必须通过引用回复
                let reply_id = match matched.reply_id {
                    Some(id_str) => id_str.parse::<i64>().unwrap_or(0),
                    None => {
                        let _ = send_msg(
                            &ctx,
                            writer,
                            msg.group_id(),
                            Some(msg.user_id()),
                            Message::new()
                                .reply(msg.message_id())
                                .text("❌ 请【引用】你想要保存的表情包，然后发送此指令。"),
                        )
                        .await;
                        return Ok(None);
                    }
                };

                match api::get_msg(&ctx, writer.clone(), reply_id).await {
                    Ok(res) => {
                        let urls: Vec<String> = res
                            .message
                            .0
                            .iter()
                            .filter(|seg| seg.type_ == "image")
                            .filter_map(|seg| {
                                seg.data
                                    .get("url")
                                    .and_then(|v| v.as_str())
                                    .map(String::from)
                            })
                            .collect();

                        if urls.is_empty() {
                            let _ = send_msg(
                                &ctx,
                                writer.clone(),
                                msg.group_id(),
                                Some(msg.user_id()),
                                Message::new()
                                    .reply(msg.message_id())
                                    .text("⚠️ 检测不到图片或表情，可能是商城表情等特殊格式。"),
                            )
                            .await;
                        } else {
                            let mut reply_msg = Message::new()
                                .reply(msg.message_id())
                                .text("✅ 图片提取成功：\n");

                            for url in urls {
                                reply_msg = reply_msg.image(url);
                            }

                            let _ = send_msg(
                                &ctx,
                                writer.clone(),
                                msg.group_id(),
                                Some(msg.user_id()),
                                reply_msg,
                            )
                            .await;

                            if RECALL_AFTER_SAVE && msg.is_group() {
                                let _ = api::delete_msg(&ctx, writer, msg.message_id()).await;
                            }
                        }
                    }
                    Err(_) => {
                        let _ = send_msg(
                            &ctx,
                            writer,
                            msg.group_id(),
                            Some(msg.user_id()),
                            Message::new().text("❌ 获取原消息失败，消息可能已过期。"),
                        )
                        .await;
                    }
                }
                return Ok(None);
        }

        Ok(Some(ctx))
    })
}