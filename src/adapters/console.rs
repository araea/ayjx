use crate::adapters::onebot::{LockedWriter, process_frame};
use crate::config::{AppConfig, BotConfig};
use crate::event::{BotStatus, LoginUser};
use crate::matcher::Matcher;
use crate::message::Message;
use crate::scheduler::Scheduler;
use crate::{info, warn};
use futures_util::Sink;
use futures_util::future::BoxFuture;
use sea_orm::DatabaseConnection;
use serde::Serialize;
use simd_json::base::ValueAsScalar;
use simd_json::derived::{TypedArrayValue, ValueObjectAccess, ValueObjectAccessAsScalar};
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex as AsyncMutex;
use tokio_tungstenite::tungstenite::{Error as WsError, Message as WsMessage};

// ================= 定义模拟数据结构 =================

#[derive(Serialize)]
struct MockSender {
    user_id: i64,
    nickname: String,
    card: String,
}

#[derive(Serialize)]
struct MockMessageEvent {
    post_type: String,
    message_type: String,
    time: u64,
    self_id: i64,
    sub_type: String,
    user_id: i64,
    message_id: i32,
    font: i32,
    sender: MockSender,
    raw_message: String,
    message: Message,
}

// ================= 适配器逻辑 =================

/// 控制台适配器入口
pub fn entry(
    _bot_config: BotConfig,
    global_config: Arc<RwLock<AppConfig>>,
    db: DatabaseConnection,
    scheduler: Arc<Scheduler>,
    save_lock: Arc<AsyncMutex<()>>,
    config_path: Arc<str>,
) -> BoxFuture<'static, ()> {
    Box::pin(async move {
        info!(target: "Console", "已启动控制台模式。请输入指令 (例如: /echo hello)");
        info!(target: "Console", "模拟环境: User ID: 1 | Group ID: None (Private)");

        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin).lines();

        // 创建一个模拟的 Writer，将 Bot 回复打印到控制台
        let writer: LockedWriter = Arc::new(AsyncMutex::new(Box::new(ConsoleSink)));
        let matcher = Arc::new(Matcher::new());

        // 定义模拟 Bot 信息（一次构造，多事件共享）
        let bot_status = Arc::new(BotStatus {
            adapter: "console".to_string(),
            platform: "console".to_string(),
            login_user: LoginUser {
                id: "0".to_string(),
                name: Some("ConsoleBot".to_string()),
                nick: Some("ConsoleBot".to_string()),
                avatar: None,
            },
        });

        // 循环读取标准输入
        while let Ok(Some(line)) = reader.next_line().await {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();

            // 构造消息链
            let msg_chain = Message::new().text(line);

            // 构造模拟事件结构体
            let event = MockMessageEvent {
                post_type: "message".to_string(),
                message_type: "private".to_string(),
                time: timestamp,
                self_id: 0,
                sub_type: "friend".to_string(),
                user_id: 1,
                message_id: timestamp as i32,
                font: 0,
                sender: MockSender {
                    user_id: 1,
                    nickname: "ConsoleUser".to_string(),
                    card: "".to_string(),
                },
                raw_message: line.to_string(),
                message: msg_chain,
            };

            // 使用 simd_json 序列化为字节数组
            let mut json_bytes = match simd_json::to_vec(&event) {
                Ok(b) => b,
                Err(e) => {
                    warn!(target: "Console", "构造模拟消息失败: {}", e);
                    continue;
                }
            };

            // 调用 OneBot 的处理逻辑
            if let Err(e) = process_frame(
                &mut json_bytes,
                writer.clone(),
                global_config.clone(),
                db.clone(),
                scheduler.clone(),
                save_lock.clone(),
                config_path.clone(),
                matcher.clone(),
                bot_status.clone(),
            )
            .await
            {
                warn!(target: "Console", "处理消息时出错: {}", e);
            }
        }
    })
}

struct ConsoleSink;

impl Sink<WsMessage> for ConsoleSink {
    type Error = WsError;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: WsMessage) -> Result<(), Self::Error> {
        if let WsMessage::Text(text) = item {
            // 解析发送出来的 JSON，提取消息内容以便友好展示
            // 格式通常为: { "action": "send_msg", "params": { "message": ... } }
            // 使用 simd_json 进行解析
            let mut data = text.as_bytes().to_vec();
            if let Ok(val) = simd_json::to_owned_value(&mut data) {
                if let Some(action) = val.get_str("action")
                    && (action == "send_msg"
                        || action == "send_private_msg"
                        || action == "send_group_msg")
                    && let Some(params) = val.get("params")
                {
                    let msg_content = if let Some(msg_val) = params.get("message") {
                        // 简单格式化
                        if msg_val.is_array() {
                            format!("{:?}", msg_val)
                        } else if let Some(s) = msg_val.as_str() {
                            s.to_string()
                        } else {
                            format!("{:?}", msg_val)
                        }
                    } else {
                        String::from("[无内容]")
                    };

                    // 打印 Bot 回复
                    println!("\x1b[36m[Bot Reply] > \x1b[0m{}", msg_content);
                    return Ok(());
                }
                // 非 send_msg 动作，打印原始动作
                println!(
                    "\x1b[90m[API Call] > {}\x1b[0m",
                    val.get_str("action").unwrap_or("unknown")
                );
            } else {
                // 解析失败，打印原始文本
                println!(
                    "\x1b[36m[Bot Raw] > \x1b[0m{}",
                    String::from_utf8_lossy(&data)
                );
            }
        }
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}
