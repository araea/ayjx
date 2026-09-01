use crate::adapters::satori::{LockedWriter, SatoriClient, process_event};
use crate::config::{AppConfig, BotConfig};
use crate::event::{BotStatus, LoginUser};
use crate::matcher::Matcher;
use crate::message::Message;
use crate::scheduler::Scheduler;
use crate::{info, warn};
use futures_util::future::BoxFuture;
use sea_orm::DatabaseConnection;
use serde::Serialize;
use std::sync::{Arc, RwLock};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex as AsyncMutex;

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
    message_id: i64,
    sender: MockSender,
    raw_message: String,
    message: Message,
}

/// 控制台适配器沿用与 Satori 相同的内部事件和发送流水线。
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

        let mut reader = BufReader::new(tokio::io::stdin()).lines();
        let writer: LockedWriter = Arc::new(SatoriClient::console());
        let matcher = Arc::new(Matcher::new());
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

        while let Ok(Some(line)) = reader.next_line().await {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let event = MockMessageEvent {
                post_type: "message".to_string(),
                message_type: "private".to_string(),
                time: timestamp,
                self_id: 0,
                sub_type: "friend".to_string(),
                user_id: 1,
                message_id: timestamp as i64,
                sender: MockSender {
                    user_id: 1,
                    nickname: "ConsoleUser".to_string(),
                    card: String::new(),
                },
                raw_message: line.to_string(),
                message: Message::new().text(line),
            };
            let event = match simd_json::serde::to_owned_value(event) {
                Ok(event) => event,
                Err(err) => {
                    warn!(target: "Console", "构造模拟消息失败: {}", err);
                    continue;
                }
            };
            if let Err(err) = process_event(
                event,
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
                warn!(target: "Console", "处理消息时出错: {}", err);
            }
        }
    })
}
