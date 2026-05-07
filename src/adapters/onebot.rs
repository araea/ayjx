use crate::config::{AppConfig, BotConfig};
use crate::event::{BotStatus, Context, Event, EventType, LoginUser, SendPacket};
use crate::matcher::Matcher;
use crate::scheduler::Scheduler;
use crate::{error, info, plugins, warn};
use futures_util::future::BoxFuture;
use futures_util::{Sink, SinkExt, StreamExt};
use http::HeaderValue;
use sea_orm::DatabaseConnection;
use serde::Serialize;
use simd_json::base::ValueAsScalar;
use simd_json::derived::ValueObjectAccess;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, protocol::Message as WsMessage},
};

pub mod api;

pub type BotError = Box<dyn std::error::Error + Send + Sync>;

pub type TraitSink =
    Box<dyn Sink<WsMessage, Error = tokio_tungstenite::tungstenite::Error> + Send + Unpin>;
pub type LockedWriter = Arc<AsyncMutex<TraitSink>>;

#[derive(Serialize)]
struct SendParamsInner<T> {
    message_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    group_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_id: Option<i64>,
    message: T,
}

/// 适配器入口函数 (Adapter Entry)
pub fn entry(
    bot_config: BotConfig,
    global_config: Arc<RwLock<AppConfig>>,
    db: DatabaseConnection,
    scheduler: Arc<Scheduler>,
    save_lock: Arc<AsyncMutex<()>>,
    config_path: Arc<str>,
) -> BoxFuture<'static, ()> {
    Box::pin(async move {
        run_bot_loop(
            bot_config,
            global_config,
            db,
            scheduler,
            save_lock,
            config_path,
        )
        .await
    })
}

/// OneBot 协议的主循环逻辑（重连退避：3s 起，指数翻倍，封顶 60s；连接成功重置）
pub async fn run_bot_loop(
    bot_config: BotConfig,
    global_config: Arc<RwLock<AppConfig>>,
    db: DatabaseConnection,
    scheduler: Arc<Scheduler>,
    save_lock: Arc<AsyncMutex<()>>,
    config_path: Arc<str>,
) {
    let bot_url = bot_config
        .url
        .clone()
        .unwrap_or_else(|| "Unknown".to_string());
    let mut backoff = Duration::from_secs(3);
    let max_backoff = Duration::from_secs(60);
    loop {
        match connect_and_listen(
            &bot_config,
            global_config.clone(),
            db.clone(),
            scheduler.clone(),
            save_lock.clone(),
            config_path.clone(),
        )
        .await
        {
            Ok(()) => {
                warn!(target: "Bot", "Bot [{}] 连接断开，{:?} 后重连...", bot_url, backoff);
            }
            Err(e) => {
                error!(target: "Bot", "Bot [{}] 连接失败: {}。{:?} 后重试...", bot_url, e, backoff);
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}

async fn connect_and_listen(
    config: &BotConfig,
    global_config: Arc<RwLock<AppConfig>>,
    db: DatabaseConnection,
    scheduler: Arc<Scheduler>,
    save_lock: Arc<AsyncMutex<()>>,
    config_path: Arc<str>,
) -> Result<(), BotError> {
    let url = config
        .url
        .as_deref()
        .ok_or_else(|| Box::<dyn std::error::Error + Send + Sync>::from("OneBot URL 未配置"))?;

    let mut request = url.into_client_request()?;

    if let Some(token) = &config.access_token
        && !token.is_empty()
    {
        let token_header = format!("Bearer {}", token);
        request
            .headers_mut()
            .insert("Authorization", HeaderValue::from_str(&token_header)?);
    }

    let (ws_stream, _) = connect_async(request).await?;
    info!(target: "Bot", "Bot [{}] 连接成功！(OneBot)", url);

    let (write_half, mut read_half) = ws_stream.split();

    let writer: LockedWriter = Arc::new(AsyncMutex::new(Box::new(write_half)));
    let matcher = Arc::new(Matcher::new());

    // 初始化 Bot 状态容器：外层 RwLock 保护 Arc<BotStatus>，
    // 仅在登录后做一次原子指针替换；之后所有事件分发都只 clone Arc，无需深拷贝 BotStatus。
    let initial_status = Arc::new(BotStatus {
        adapter: "onebot".to_string(),
        platform: "qq".to_string(),
        login_user: LoginUser {
            id: "0".to_string(),
            ..Default::default()
        },
    });
    let bot_status: Arc<RwLock<Arc<BotStatus>>> = Arc::new(RwLock::new(initial_status));

    // 启动后台任务获取登录信息
    {
        let status_ref = bot_status.clone();
        let writer_ref = writer.clone();
        let matcher_ref = matcher.clone();
        let config_ref = global_config.clone();
        let db_ref = db.clone();
        let scheduler_ref = scheduler.clone();
        let save_lock_ref = save_lock.clone();
        let config_path_ref = config_path.clone();

        tokio::spawn(async move {
            // 稍微延时等待连接稳定
            tokio::time::sleep(Duration::from_secs(1)).await;

            // 构建临时上下文用于调用 API
            let initial_bot = status_ref.read().unwrap().clone();
            let ctx = Context {
                event: EventType::Init,
                config: config_ref,
                config_save_lock: save_lock_ref,
                db: db_ref,
                scheduler: scheduler_ref,
                matcher: matcher_ref,
                config_path: config_path_ref,
                bot: initial_bot,
            };

            match api::get_login_info(&ctx, writer_ref.clone()).await {
                Ok(info) => {
                    let new_bot = Arc::new(BotStatus {
                        adapter: "onebot".to_string(),
                        platform: "qq".to_string(),
                        login_user: LoginUser {
                            id: info.user_id.to_string(),
                            name: Some(info.nickname.clone()),
                            nick: Some(info.nickname),
                            avatar: Some(format!(
                                "https://q1.qlogo.cn/g?b=qq&nk={}&s=640",
                                info.user_id
                            )),
                        },
                    });

                    info!(
                        target: "Bot",
                        "已获取登录信息: {} ({})",
                        new_bot.login_user.name.as_deref().unwrap_or("Unknown"),
                        new_bot.login_user.id
                    );

                    {
                        let mut guard = status_ref.write().unwrap();
                        *guard = new_bot.clone();
                    }

                    // 构造新的 Context 包含更新后的 Bot 信息
                    let mut new_ctx = ctx;
                    new_ctx.bot = new_bot;

                    // 触发插件系统的 Connected 钩子
                    if let Err(e) = plugins::do_connected(new_ctx, writer_ref).await {
                        error!(target: "Bot", "插件 Connected 钩子执行失败: {}", e);
                    }
                }
                Err(e) => {
                    warn!(target: "Bot", "获取登录信息失败: {}", e);
                }
            }
        });
    }

    while let Some(message) = read_half.next().await {
        match message {
            Ok(WsMessage::Text(text)) => {
                let mut data = text.as_bytes().to_vec();

                let writer = writer.clone();
                let config = global_config.clone();
                let db = db.clone();
                let scheduler = scheduler.clone();
                let save_lock = save_lock.clone();
                let config_path = config_path.clone();
                let matcher = matcher.clone();
                // 仅克隆 Arc 指针，不深拷贝 BotStatus
                let current_status = bot_status.read().unwrap().clone();

                tokio::spawn(async move {
                    if let Err(e) = process_frame(
                        &mut data,
                        writer,
                        config,
                        db,
                        scheduler,
                        save_lock,
                        config_path,
                        matcher,
                        current_status,
                    )
                    .await
                    {
                        error!(target: "Bot", "Event processing error: {}", e);
                    }
                });
            }
            Ok(WsMessage::Close(_)) => return Ok(()),
            Err(e) => return Err(Box::new(e)),
            _ => {}
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn process_frame(
    data: &mut [u8],
    writer: LockedWriter,
    config: Arc<RwLock<AppConfig>>,
    db: DatabaseConnection,
    scheduler: Arc<Scheduler>,
    save_lock: Arc<AsyncMutex<()>>,
    config_path: Arc<str>,
    matcher: Arc<Matcher>,
    bot: Arc<BotStatus>,
) -> Result<(), BotError> {
    let event: Event = match simd_json::to_owned_value(data) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };

    // 优先尝试分发给等待者 (交互式输入/API响应)
    let event = match matcher.dispatch(event).await {
        Some(e) => e,
        None => return Ok(()),
    };

    // 全局黑白名单过滤
    // 仅过滤带有 group_id 的事件（即群相关事件）
    let group_id = event
        .get("group_id")
        .and_then(|v| v.as_i64().or(v.as_u64().map(|u| u as i64)));

    if let Some(gid) = group_id {
        let should_drop = {
            let guard = config.read().unwrap();
            let filter = &guard.global_filter;
            if filter.enable_whitelist {
                // 开启白名单：仅允许白名单内的频道
                !filter.whitelist.contains(&gid)
            } else if filter.enable_blacklist {
                // 开启黑名单：过滤黑名单内的频道
                filter.blacklist.contains(&gid)
            } else {
                false
            }
        };

        if should_drop {
            return Ok(());
        }
    }

    let ctx = Context {
        event: EventType::Onebot(event),
        config,
        config_save_lock: save_lock,
        db,
        scheduler,
        matcher,
        config_path,
        bot,
    };

    plugins::run(ctx, writer).await?;
    Ok(())
}

pub async fn send_msg<M>(
    ctx: &Context,
    writer: LockedWriter,
    group_id: Option<i64>,
    user_id: Option<i64>,
    message: M,
) -> Result<(), BotError>
where
    M: Serialize,
{
    let (msg_type, target_group, target_user) = if let Some(gid) = group_id.filter(|&id| id != 0) {
        ("group", Some(gid), None)
    } else if let Some(uid) = user_id.filter(|&id| id != 0) {
        ("private", None, Some(uid))
    } else {
        return Ok(());
    };

    let params = SendParamsInner {
        message_type: msg_type,
        group_id: target_group,
        user_id: target_user,
        message,
    };

    // 直接将 Serialize 转为 OwnedValue，避免先序列化为 JSON 字符串再反序列化的双重往返
    let params_val =
        simd_json::serde::to_owned_value(params).map_err(|e| Box::new(e) as BotError)?;

    // 捕获原始事件以便在 BeforeSend 中传递
    let original_event = match &ctx.event {
        EventType::Onebot(ev) => Some(ev.clone()),
        EventType::BeforeSend(pkt) => pkt.original_event.clone(),
        EventType::Init => None,
    };

    let packet = SendPacket {
        action: "send_msg".to_string(),
        params: params_val,
        original_event,
    };

    let new_ctx = Context {
        event: EventType::BeforeSend(packet),
        config: ctx.config.clone(),
        config_save_lock: ctx.config_save_lock.clone(),
        db: ctx.db.clone(),
        scheduler: ctx.scheduler.clone(),
        matcher: ctx.matcher.clone(),
        config_path: ctx.config_path.clone(),
        bot: ctx.bot.clone(),
    };

    plugins::run(new_ctx, writer).await?;
    Ok(())
}

pub async fn send_frame_raw(writer: LockedWriter, json_str: String) -> Result<(), BotError> {
    let mut guard = writer.lock().await;
    guard.send(WsMessage::Text(json_str.into())).await?;
    Ok(())
}
