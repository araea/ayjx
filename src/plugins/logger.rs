use crate::adapters::satori::LockedWriter;
use crate::config::build_config;
use crate::event::{Context, EventType};
use crate::plugins::{PluginError, get_config};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use simd_json::OwnedValue;
use simd_json::base::{ValueAsArray, ValueAsScalar};
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsScalar};
use toml::Value;

#[derive(Serialize, Deserialize)]
struct LoggerConfig {
    enabled: bool,
    #[serde(default)]
    debug: bool,
}

pub fn default_config() -> Value {
    build_config(LoggerConfig {
        enabled: true,
        debug: false,
    })
}

pub fn handle(
    ctx: Context,
    _writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        // 获取配置
        let config: LoggerConfig = get_config(&ctx, "logger").unwrap_or(LoggerConfig {
            enabled: true,
            debug: false,
        });

        match &ctx.event {
            EventType::Satori(ev) => {
                if config.debug {
                    debug!(target: "Logger", "ev: {:?}", ev);
                }

                if let Some(msg) = ctx.as_message() {
                    let content = format_message(ev.get("message"));
                    // 尝试获取规范化事件携带的群名（同 recorder 插件逻辑）
                    let group_name = ev.get_str("group_name");
                    let sender = format!("{}({})", msg.sender_name(), msg.user_id());

                    if let Some(gid) = msg.group_id() {
                        let group_str = if let Some(name) = group_name {
                            format!("{}|{}", name, gid)
                        } else {
                            gid.to_string()
                        };
                        // 格式: 接收 <- 群聊 [Group(Name|ID)] [Sender(Name(ID))] Content
                        info!(
                            target: "Chat",
                            "接收 <- 群聊 [Group({})] [{}] {}",
                            group_str, sender, content
                        );
                    } else {
                        // 格式: 接收 <- 私聊 [Sender(Name(ID))] Content
                        info!(
                            target: "Chat",
                            "接收 <- 私聊 [{}] {}",
                            sender, content
                        );
                    }
                } else if let Some(post_type) = ctx.post_type() {
                    // 过滤心跳日志，减少干扰
                    if post_type != "meta_event" {
                        debug!(target: "Event", "Type: {}", post_type);
                    }
                }
            }
            EventType::BeforeSend(packet) => {
                if config.debug {
                    debug!(target: "Logger", "packet: {:?}", packet);
                }
                if packet.action == "message.create" {
                    let params = &packet.params;
                    let msg_type = params.get_str("message_type").unwrap_or("unknown");
                    let content = format_message(params.get("message"));

                    if msg_type == "group" {
                        let gid = params
                            .get_i64("group_id")
                            .or_else(|| params.get_u64("group_id").map(|v| v as i64))
                            .unwrap_or(0);

                        // 尝试从原始事件中获取上下文信息 (如群名)
                        let mut group_info = gid.to_string();
                        if let Some(origin) = &packet.original_event {
                            let origin_gid = origin
                                .get_i64("group_id")
                                .or_else(|| origin.get_u64("group_id").map(|v| v as i64))
                                .unwrap_or(0);

                            // 如果发送的目标群与原始事件的群一致，则复用群名
                            if origin_gid == gid
                                && let Some(name) = origin.get_str("group_name")
                            {
                                group_info = format!("{}|{}", name, gid);
                            }
                        }

                        info!(
                            target: "Chat",
                            "发送 -> 群聊 [Group({})] {}",
                            group_info, content
                        );
                    } else if msg_type == "private" {
                        let uid = params
                            .get_i64("user_id")
                            .or_else(|| params.get_u64("user_id").map(|v| v as i64))
                            .unwrap_or(0);
                        info!(
                            target: "Chat",
                            "发送 -> 私聊 [User({})] {}",
                            uid, content
                        );
                    } else {
                        info!(
                            target: "Chat",
                            "发送 -> 未知 [{}] {}",
                            msg_type, content
                        );
                    }
                } else {
                    debug!(target: "Bot", "Action: {}", packet.action);
                }
            }
            EventType::Init => {
                // Init 阶段由 plugins.rs 统一输出日志，这里不再重复
            }
        }

        Ok(Some(ctx))
    })
}

/// 将内部消息链转换为人类可读的字符串
fn format_message(msg_val: Option<&OwnedValue>) -> String {
    use std::fmt::Write as _;

    let val = match msg_val {
        Some(v) => v,
        None => return String::new(),
    };

    // 1. 纯字符串情况
    if let Some(s) = val.as_str() {
        return s.to_string();
    }

    // 2. 消息段数组情况
    if let Some(arr) = val.as_array() {
        // 预分配避免增长再分配；多数消息长度有限
        let mut result = String::with_capacity(64);
        for seg in arr {
            let type_ = seg.get_str("type").unwrap_or("unknown");
            let data = seg.get("data");

            match type_ {
                "text" => {
                    if let Some(t) = data.and_then(|d| d.get_str("text")) {
                        result.push_str(t);
                    }
                }
                "at" => {
                    result.push_str(" [@");
                    if let Some(d) = data {
                        if let Some(s) = d.get_str("qq") {
                            result.push_str(s);
                        } else if let Some(i) = d.get_i64("qq") {
                            let _ = write!(result, "{}", i);
                        } else if let Some(i) = d.get_u64("qq") {
                            let _ = write!(result, "{}", i);
                        } else {
                            result.push_str("Unknown");
                        }
                    } else {
                        result.push_str("Unknown");
                    }
                    result.push_str("] ");
                }
                "face" => result.push_str(" [表情] "),
                "image" => {
                    let is_anim = data
                        .map(|d| {
                            let summary = d.get_str("summary").unwrap_or("");
                            let sub_type = d
                                .get_i64("sub_type")
                                .or_else(|| d.get_u64("sub_type").map(|v| v as i64))
                                .unwrap_or(0);
                            summary == "[动画表情]" || sub_type == 1
                        })
                        .unwrap_or(false);

                    if is_anim {
                        result.push_str(" [动画表情] ");
                    } else {
                        result.push_str(" [图片] ");
                    }
                }
                "record" => result.push_str(" [语音] "),
                "video" => result.push_str(" [视频] "),
                "music" => result.push_str(" [音乐] "),
                "reply" => result.push_str(" [回复] "),
                "forward" | "node" => result.push_str(" [合并转发] "),
                "json" => result.push_str(" [卡片消息] "),
                "xml" => result.push_str(" [XML消息] "),
                "poke" => result.push_str(" [戳一戳] "),
                "rps" => result.push_str(" [猜拳] "),
                "dice" => result.push_str(" [骰子] "),
                "file" => result.push_str(" [文件] "),
                "share" => result.push_str(" [分享] "),
                "location" => result.push_str(" [位置] "),
                other => {
                    result.push_str(" [");
                    result.push_str(other);
                    result.push_str("] ");
                }
            }
        }
        return result;
    }

    "[复杂消息]".to_string()
}
