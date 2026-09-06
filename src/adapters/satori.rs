use crate::config::{AppConfig, BotConfig};
use crate::event::{BotStatus, Context, Event, EventType, LoginUser, SendPacket};
use crate::matcher::Matcher;
use crate::scheduler::Scheduler;
use crate::{error, info, plugins, warn};
use futures_util::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use sea_orm::DatabaseConnection;
use serde::Serialize;
use serde_json::{Value, json};
use simd_json::base::ValueAsScalar;
use simd_json::derived::ValueObjectAccessAsScalar;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, protocol::Message as WsMessage},
};

pub mod api;
pub mod message;

pub type BotError = Box<dyn std::error::Error + Send + Sync>;
pub type LockedWriter = Arc<SatoriClient>;

/// Satori 的事件和 API 使用两条独立通道：WS 只收事件，HTTP 负责所有调用。
pub struct SatoriClient {
    endpoint: String,
    token: Option<String>,
    http: reqwest::Client,
    console: bool,
    /// `READY` / `META` 下发的代理路由前缀，决定哪些平台链接要经 `/v1/proxy` 取。
    proxy_urls: RwLock<Arc<Vec<String>>>,
}

impl SatoriClient {
    fn new(endpoint: String, token: Option<String>) -> Self {
        Self {
            endpoint,
            token: token.filter(|value| !value.trim().is_empty()),
            http: crate::http::client(),
            console: false,
            proxy_urls: RwLock::new(Arc::new(Vec::new())),
        }
    }

    pub fn console() -> Self {
        Self {
            endpoint: String::new(),
            token: None,
            http: crate::http::client(),
            console: true,
            proxy_urls: RwLock::new(Arc::new(Vec::new())),
        }
    }

    pub fn set_proxy_urls(&self, urls: Vec<String>) {
        *self.proxy_urls.write().unwrap() = Arc::new(urls);
    }

    /// 解析消息元素里 `src` 的取件方式，交给 `message` 模块使用。
    pub fn resources(&self) -> message::ResourceProxy {
        message::ResourceProxy::new(
            self.endpoint.clone(),
            self.proxy_urls.read().unwrap().clone(),
        )
    }

    pub fn connection_key(&self) -> &str {
        if self.console {
            "console"
        } else {
            &self.endpoint
        }
    }

    pub async fn call<P, R>(&self, ctx: &Context, method: &str, params: P) -> Result<R, BotError>
    where
        P: Serialize,
        R: serde::de::DeserializeOwned,
    {
        if self.console {
            if method == "message.create" {
                let value = serde_json::to_value(params)?;
                println!(
                    "\x1b[36m[Bot Reply] > \x1b[0m{}",
                    value.get("content").and_then(Value::as_str).unwrap_or("")
                );
                return Ok(serde_json::from_value(Value::Array(Vec::new()))?);
            }
            return Err(format!("控制台模式不支持 Satori API: {method}").into());
        }

        let url = format!("{}/v1/{}", self.endpoint, method);
        let mut request = self
            .http
            .post(url)
            .header("Satori-Platform", &ctx.bot.platform)
            .header("Satori-User-ID", &ctx.bot.login_user.id)
            .json(&params);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            let detail = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|value| {
                    value
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned());
            return Err(format!("Satori API {method} 失败 ({status}): {detail}").into());
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// 使用标准 `upload.create` multipart 把 ayjx 侧文件传给实现端。
    pub async fn upload(
        &self,
        ctx: &Context,
        data: Vec<u8>,
        name: &str,
        mime: &str,
    ) -> Result<Value, BotError> {
        if self.console {
            return Err("控制台模式不支持文件上传".into());
        }
        let part = reqwest::multipart::Part::bytes(data)
            .file_name(name.to_string())
            .mime_str(mime)?;
        let form = reqwest::multipart::Form::new().part("file", part);
        let mut request = self
            .http
            .post(format!("{}/v1/upload.create", self.endpoint))
            .header("Satori-Platform", &ctx.bot.platform)
            .header("Satori-User-ID", &ctx.bot.login_user.id)
            .multipart(form);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            let detail = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|value| {
                    value
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned());
            return Err(format!("Satori API upload.create 失败 ({status}): {detail}").into());
        }
        Ok(serde_json::from_slice(&bytes)?)
    }
}

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

/// Satori 主循环：3 秒起指数退避，最长 60 秒。
pub async fn run_bot_loop(
    bot_config: BotConfig,
    global_config: Arc<RwLock<AppConfig>>,
    db: DatabaseConnection,
    scheduler: Arc<Scheduler>,
    save_lock: Arc<AsyncMutex<()>>,
    config_path: Arc<str>,
) {
    let endpoint = bot_config.url.clone().unwrap_or_default();
    let mut backoff = Duration::from_secs(3);
    // 最后一个收到的事件序列号。重连时带上它，实现端会补推断线期间的事件。
    let mut session_sn: Option<i64> = None;
    loop {
        match connect_and_listen(
            &bot_config,
            global_config.clone(),
            db.clone(),
            scheduler.clone(),
            save_lock.clone(),
            config_path.clone(),
            &mut session_sn,
        )
        .await
        {
            Ok(()) => {
                warn!(target: "Bot", "Satori [{}] 连接断开，{:?} 后重连...", endpoint, backoff)
            }
            Err(err) => {
                error!(target: "Bot", "Satori [{}] 连接失败: {}。{:?} 后重试...", endpoint, err, backoff)
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// 协议规定应用每 10 秒发一次 `PING`，实现端回 `PONG`。
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

const OP_EVENT: i64 = 0;
const OP_PING: i64 = 1;
const OP_PONG: i64 = 2;
const OP_IDENTIFY: i64 = 3;
const OP_READY: i64 = 4;
const OP_META: i64 = 5;

#[allow(clippy::too_many_arguments)]
async fn connect_and_listen(
    config: &BotConfig,
    global_config: Arc<RwLock<AppConfig>>,
    db: DatabaseConnection,
    scheduler: Arc<Scheduler>,
    save_lock: Arc<AsyncMutex<()>>,
    config_path: Arc<str>,
    session_sn: &mut Option<i64>,
) -> Result<(), BotError> {
    let endpoint = normalize_endpoint(config.url.as_deref().ok_or("Satori URL 未配置")?)?;
    let events_url = events_url(&endpoint)?;
    let request = events_url.into_client_request()?;
    let (stream, _) = connect_async(request).await?;
    let (mut ws_write, mut ws_read) = stream.split();

    // 出站帧统一走一条队列：心跳任务和事件循环都只是往队列里投递。
    let (outbound, mut outbound_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let writer_task = tokio::spawn(async move {
        while let Some(text) = outbound_rx.recv().await {
            if ws_write.send(WsMessage::Text(text.into())).await.is_err() {
                break;
            }
        }
        let _ = ws_write.close().await;
    });

    let token = effective_token(config);
    let mut identify_body = json!({});
    if let Some(token) = token.as_deref() {
        identify_body["token"] = json!(token);
    }
    // 省略 sn 表示开新会话；带上 sn 则请求补推断线期间的事件。
    if let Some(sn) = *session_sn {
        identify_body["sn"] = json!(sn);
        info!(target: "Bot", "Satori [{}] 尝试从 sn={} 恢复会话。", endpoint, sn);
    }
    outbound.send(json!({"op": OP_IDENTIFY, "body": identify_body}).to_string())?;

    let ready = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(frame) = ws_read.next().await {
            let frame = frame?;
            if let WsMessage::Text(text) = frame {
                let packet: Value = serde_json::from_str(&text)?;
                if packet.get("op").and_then(Value::as_i64) == Some(OP_READY) {
                    return Ok::<Value, BotError>(packet);
                }
            }
        }
        Err("Satori 在 READY 前关闭连接".into())
    })
    .await
    .map_err(|_| "等待 Satori READY 超时")??;

    let login = ready
        .pointer("/body/logins/0")
        .ok_or("Satori READY 未提供登录信息")?;
    let user = login.get("user").unwrap_or(&Value::Null);
    let bot_status = Arc::new(BotStatus {
        adapter: login
            .get("adapter")
            .and_then(Value::as_str)
            .unwrap_or("satori-qq")
            .to_string(),
        platform: login
            .get("platform")
            .and_then(Value::as_str)
            .unwrap_or("red")
            .to_string(),
        login_user: LoginUser {
            id: string_id(user.get("id")),
            name: optional_string(user.get("name")),
            nick: optional_string(user.get("nick")),
            avatar: optional_string(user.get("avatar")),
        },
    });
    let writer = Arc::new(SatoriClient::new(endpoint.clone(), token));
    writer.set_proxy_urls(proxy_urls(&ready));
    let matcher = Arc::new(Matcher::new());

    info!(
        target: "Bot",
        "Bot [{}] 连接成功！(Satori {}/{}, login={})",
        endpoint,
        bot_status.adapter,
        bot_status.platform,
        bot_status.login_user.id
    );

    let connected_ctx = Context {
        event: EventType::Init,
        config: global_config.clone(),
        config_save_lock: save_lock.clone(),
        db: db.clone(),
        scheduler: scheduler.clone(),
        matcher: matcher.clone(),
        config_path: config_path.clone(),
        bot: bot_status.clone(),
    };
    plugins::do_connected(connected_ctx, writer.clone()).await?;

    let heartbeat = tokio::spawn({
        let outbound = outbound.clone();
        async move {
            let ping = json!({"op": OP_PING, "body": {}}).to_string();
            loop {
                tokio::time::sleep(HEARTBEAT_INTERVAL).await;
                if outbound.send(ping.clone()).is_err() {
                    break;
                }
            }
        }
    });

    let result = listen(
        &mut ws_read,
        &outbound,
        &writer,
        &bot_status,
        &global_config,
        &db,
        &scheduler,
        &save_lock,
        &config_path,
        &matcher,
        session_sn,
    )
    .await;

    heartbeat.abort();
    drop(outbound);
    let _ = writer_task.await;
    result
}

/// 事件循环：`EVENT` 进插件流水线，`META` 刷新代理路由，`PING` 回 `PONG`。
#[allow(clippy::too_many_arguments)]
async fn listen(
    ws_read: &mut futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    outbound: &tokio::sync::mpsc::UnboundedSender<String>,
    writer: &LockedWriter,
    bot_status: &Arc<BotStatus>,
    global_config: &Arc<RwLock<AppConfig>>,
    db: &DatabaseConnection,
    scheduler: &Arc<Scheduler>,
    save_lock: &Arc<AsyncMutex<()>>,
    config_path: &Arc<str>,
    matcher: &Arc<Matcher>,
    session_sn: &mut Option<i64>,
) -> Result<(), BotError> {
    while let Some(frame) = ws_read.next().await {
        match frame? {
            WsMessage::Text(text) => {
                let packet: Value = match serde_json::from_str(&text) {
                    Ok(packet) => packet,
                    Err(err) => {
                        warn!(target: "Bot", "忽略无效 Satori 帧: {}", err);
                        continue;
                    }
                };
                match packet.get("op").and_then(Value::as_i64) {
                    Some(OP_EVENT) => {
                        let Some(body) = packet.get("body") else {
                            continue;
                        };
                        if let Some(sn) = body.get("sn").and_then(Value::as_i64) {
                            *session_sn = Some(sn);
                        }
                        let event = match normalize_event(body, bot_status, &writer.resources()) {
                            Ok(event) => event,
                            Err(err) => {
                                warn!(target: "Bot", "Satori 事件转换失败: {}", err);
                                continue;
                            }
                        };
                        let writer = writer.clone();
                        let config = global_config.clone();
                        let db = db.clone();
                        let scheduler = scheduler.clone();
                        let save_lock = save_lock.clone();
                        let config_path = config_path.clone();
                        let matcher = matcher.clone();
                        let bot = bot_status.clone();
                        tokio::spawn(async move {
                            if let Err(err) = process_event(
                                event,
                                writer,
                                config,
                                db,
                                scheduler,
                                save_lock,
                                config_path,
                                matcher,
                                bot,
                            )
                            .await
                            {
                                error!(target: "Bot", "Satori event processing error: {}", err);
                            }
                        });
                    }
                    // 协议里 PING 由应用发出，这里回 PONG 只是兼容反向心跳的实现端。
                    Some(OP_PING) => {
                        outbound.send(json!({"op": OP_PONG, "body": {}}).to_string())?;
                    }
                    Some(OP_META) => {
                        if let Some(body) = packet.get("body") {
                            writer.set_proxy_urls(proxy_urls(body));
                        }
                    }
                    _ => {}
                }
            }
            WsMessage::Close(_) => return Ok(()),
            _ => {}
        }
    }
    Ok(())
}

/// `READY` 与 `META` 的 body 都带 `proxy_urls`，取值规则一致。
fn proxy_urls(packet: &Value) -> Vec<String> {
    packet
        .pointer("/body/proxy_urls")
        .or_else(|| packet.get("proxy_urls"))
        .and_then(Value::as_array)
        .map(|urls| {
            urls.iter()
                .filter_map(Value::as_str)
                .filter(|url| !url.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn effective_token(config: &BotConfig) -> Option<String> {
    std::env::var("AYJX_SATORI_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            config
                .access_token
                .clone()
                .filter(|value| !value.trim().is_empty())
        })
}

fn normalize_endpoint(raw: &str) -> Result<String, BotError> {
    let mut url = url::Url::parse(raw.trim())?;
    match url.scheme() {
        "ws" => url.set_scheme("http").map_err(|_| "无法转换 Satori URL")?,
        "wss" => url.set_scheme("https").map_err(|_| "无法转换 Satori URL")?,
        "http" | "https" => {}
        scheme => return Err(format!("不支持的 Satori URL scheme: {scheme}").into()),
    }
    let path = url.path().trim_end_matches('/').to_string();
    let base_path = path.strip_suffix("/v1/events").unwrap_or(&path).to_string();
    url.set_path(&base_path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn events_url(endpoint: &str) -> Result<String, BotError> {
    let mut url = url::Url::parse(endpoint)?;
    match url.scheme() {
        "http" => url
            .set_scheme("ws")
            .map_err(|_| "无法转换 Satori events URL")?,
        "https" => url
            .set_scheme("wss")
            .map_err(|_| "无法转换 Satori events URL")?,
        _ => return Err("Satori endpoint 必须是 http(s) URL".into()),
    }
    url.set_path(&format!("{}/v1/events", url.path().trim_end_matches('/')));
    Ok(url.into())
}

#[allow(clippy::too_many_arguments)]
pub async fn process_event(
    event: Event,
    writer: LockedWriter,
    config: Arc<RwLock<AppConfig>>,
    db: DatabaseConnection,
    scheduler: Arc<Scheduler>,
    save_lock: Arc<AsyncMutex<()>>,
    config_path: Arc<str>,
    matcher: Arc<Matcher>,
    bot: Arc<BotStatus>,
) -> Result<(), BotError> {
    let event = if event.get_str("post_type") == Some("message") {
        match matcher.dispatch(event).await {
            Some(event) => event,
            None => return Ok(()),
        }
    } else {
        event
    };
    let group_id = event
        .get_i64("group_id")
        .or_else(|| event.get_u64("group_id").map(|value| value as i64));
    if let Some(group_id) = group_id {
        let should_drop = {
            let guard = config.read().unwrap();
            if guard.global_filter.enable_whitelist {
                !guard.global_filter.whitelist.contains(&group_id)
            } else if guard.global_filter.enable_blacklist {
                guard.global_filter.blacklist.contains(&group_id)
            } else {
                false
            }
        };
        if should_drop {
            return Ok(());
        }
    }

    let ctx = Context {
        event: EventType::Satori(event),
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
    dispatch_send(ctx, writer, group_id, user_id, message)
        .await
        .map(|_| ())
}

/// Satori 的 message.create 是同步 HTTP RPC，成功返回即视为已确认。
pub async fn send_msg_ack<M>(
    ctx: &Context,
    writer: LockedWriter,
    group_id: Option<i64>,
    user_id: Option<i64>,
    message: M,
) -> Result<bool, BotError>
where
    M: Serialize,
{
    dispatch_send(ctx, writer, group_id, user_id, message).await?;
    Ok(true)
}

/// 发送消息并返回实现端分配的第一条消息 ID。
///
/// AI News 等需要让后续引用回复精确关联原消息的功能应使用此接口；普通发送
/// 仍使用 [`send_msg`]，无需关心回执内容。
pub async fn send_msg_id<M>(
    ctx: &Context,
    writer: LockedWriter,
    group_id: Option<i64>,
    user_id: Option<i64>,
    message: M,
) -> Result<Option<String>, BotError>
where
    M: Serialize,
{
    Ok(dispatch_send(ctx, writer, group_id, user_id, message)
        .await?
        .into_iter()
        .next())
}

async fn dispatch_send<M>(
    ctx: &Context,
    writer: LockedWriter,
    group_id: Option<i64>,
    user_id: Option<i64>,
    message: M,
) -> Result<Vec<String>, BotError>
where
    M: Serialize,
{
    let (message_type, group_id, user_id) = if let Some(id) = group_id.filter(|id| *id != 0) {
        ("group", Some(id), None)
    } else if let Some(id) = user_id.filter(|id| *id != 0) {
        ("private", None, Some(id))
    } else {
        return Ok(Vec::new());
    };
    let params = simd_json::serde::to_owned_value(json!({
        "message_type": message_type,
        "group_id": group_id,
        "user_id": user_id,
        "message": serde_json::to_value(message)?,
    }))?;
    let original_event = match &ctx.event {
        EventType::Satori(event) => Some(event.clone()),
        EventType::BeforeSend(packet) => packet.original_event.clone(),
        EventType::Init => None,
    };
    let receipt_message_ids = Arc::new(std::sync::Mutex::new(Vec::new()));
    let packet = SendPacket {
        action: "message.create".to_string(),
        params,
        original_event,
        receipt_message_ids: receipt_message_ids.clone(),
    };
    let next = Context {
        event: EventType::BeforeSend(packet),
        config: ctx.config.clone(),
        config_save_lock: ctx.config_save_lock.clone(),
        db: ctx.db.clone(),
        scheduler: ctx.scheduler.clone(),
        matcher: ctx.matcher.clone(),
        config_path: ctx.config_path.clone(),
        bot: ctx.bot.clone(),
    };
    plugins::run(next, writer).await?;
    Ok(receipt_message_ids
        .lock()
        .map_err(|_| "发送回执锁已损坏")?
        .clone())
}

/// 执行插件修改后的发送包。
pub async fn dispatch_packet(
    ctx: &Context,
    writer: LockedWriter,
    packet: &SendPacket,
) -> Result<(), BotError> {
    let group_id = packet.group_id();
    let user_id = packet
        .params
        .get_i64("user_id")
        .or_else(|| packet.params.get_u64("user_id").map(|value| value as i64));
    let channel_id = if let Some(group_id) = group_id.filter(|id| *id != 0) {
        group_id.to_string()
    } else if let Some(user_id) = user_id.filter(|id| *id != 0) {
        format!("private:{user_id}")
    } else {
        return Ok(());
    };
    let content = packet
        .message()
        .map(message::to_content)
        .unwrap_or_default();
    let created: Vec<Value> = writer
        .call(
            ctx,
            "message.create",
            json!({"channel_id": channel_id, "content": content}),
        )
        .await?;
    let ids = created
        .iter()
        .map(|message| raw_id(message.get("id")))
        .filter(|id| !id.is_empty())
        .collect();
    *packet
        .receipt_message_ids
        .lock()
        .map_err(|_| "发送回执锁已损坏")? = ids;
    Ok(())
}

fn normalize_event(
    body: &Value,
    bot: &BotStatus,
    proxy: &message::ResourceProxy,
) -> Result<Event, BotError> {
    let event_type = body.get("type").and_then(Value::as_str).unwrap_or("");
    let timestamp = body
        .get("timestamp")
        .and_then(Value::as_i64)
        .unwrap_or_default()
        / 1000;
    let guild = body.get("guild").unwrap_or(&Value::Null);
    let channel = body.get("channel").unwrap_or(&Value::Null);
    let user = body.get("user").unwrap_or(&Value::Null);
    let member = body.get("member").unwrap_or(&Value::Null);
    let guild_id = parse_id(guild.get("id"));
    let group_id = if guild_id != 0 {
        guild_id
    } else if channel.get("type").and_then(Value::as_i64) == Some(0) {
        parse_id(channel.get("id"))
    } else {
        0
    };
    let mut user_id = parse_id(user.get("id"));
    if user_id == 0 {
        user_id = body
            .pointer("/satori_qq/actual_user_id")
            .and_then(value_id)
            .unwrap_or_default();
    }
    // 协议规定每个事件都自带 login 资源，多登录场景下它才是这条事件的归属账号；
    // 缺失时退回 READY 时记录的登录号。
    let self_id = parse_id(body.pointer("/login/user/id"));
    let self_id = if self_id != 0 {
        self_id
    } else {
        bot.login_user.id.parse::<i64>().unwrap_or_default()
    };
    let mut out = json!({
        "time": timestamp,
        "self_id": self_id,
        "satori_type": event_type,
        "_satori": body,
    });

    if event_type == "message-created" {
        let message = body.get("message").unwrap_or(&Value::Null);
        let content = message.get("content").and_then(Value::as_str).unwrap_or("");
        let chain = message::from_content_with(content, proxy);
        let raw_message = chain
            .0
            .iter()
            .filter(|segment| segment.type_ == "text")
            .filter_map(|segment| segment.data.get("text").and_then(|value| value.as_str()))
            .collect::<String>();
        let message_id_str = string_id(message.get("id"));
        let message_id = message_id_str.parse::<i64>().unwrap_or_default();
        let group = group_id != 0;
        out["post_type"] = json!("message");
        out["message_type"] = json!(if group { "group" } else { "private" });
        out["sub_type"] = json!(if group { "normal" } else { "friend" });
        if group {
            out["group_id"] = json!(group_id);
            out["group_name"] = json!(
                guild
                    .get("name")
                    .or_else(|| channel.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
            );
        }
        out["user_id"] = json!(user_id);
        out["message_id"] = json!(message_id);
        out["message_id_str"] = json!(message_id_str);
        out["raw_message"] = json!(raw_message);
        out["message"] = serde_json::to_value(chain)?;
        let role = member
            .get("roles")
            .and_then(Value::as_array)
            .and_then(|roles| roles.first())
            .and_then(|role| role.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("member");
        out["sender"] = json!({
            "user_id": user_id,
            "nickname": user.get("name").and_then(Value::as_str).unwrap_or(""),
            "card": member.get("nick").or_else(|| member.get("name")).and_then(Value::as_str).unwrap_or(""),
            "role": role,
        });
    } else {
        let (post_type, notice_type, request_type, sub_type) = match event_type {
            "message-deleted" => ("notice", "message_recall", "", ""),
            "guild-member-added" => ("notice", "group_increase", "", "approve"),
            "guild-member-removed" => ("notice", "group_decrease", "", "leave"),
            "guild-member-updated" => ("notice", "group_ban", "", "ban"),
            "friend-request" => ("request", "", "friend", ""),
            "guild-request" => ("request", "", "group", "invite"),
            "guild-member-request" => ("request", "", "group", "add"),
            "internal" if body.get("_type").and_then(Value::as_str) == Some("satori-qq/poke") => {
                ("notice", "notify", "", "poke")
            }
            _ => ("satori", "", "", ""),
        };
        // 禁言与解禁共用 guild-member-updated，靠 _data.duration 区分；实现端给的是毫秒。
        let ban_duration = body
            .pointer("/_data/duration")
            .and_then(value_id)
            .map(|value| value / 1000);
        let sub_type = match (notice_type, ban_duration) {
            ("group_ban", Some(0)) => "lift_ban",
            _ => sub_type,
        };
        out["post_type"] = json!(post_type);
        out["notice_type"] = json!(notice_type);
        out["request_type"] = json!(request_type);
        out["sub_type"] = json!(sub_type);
        if group_id != 0 {
            out["group_id"] = json!(group_id);
        }
        out["user_id"] = json!(user_id);
        // 申请类事件的 message.id 是审批 flag（非数字），只能按字符串保留。
        let message_id_str = raw_id(body.pointer("/message/id"));
        out["message_id"] = json!(message_id_str.parse::<i64>().unwrap_or_default());
        out["message_id_str"] = json!(message_id_str);
        if post_type == "request" {
            out["flag"] = json!(message_id_str);
            out["comment"] = json!(
                body.pointer("/message/content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            );
        }
        if let Some(duration) = ban_duration {
            out["duration"] = json!(duration);
        }
        out["operator_id"] = json!(parse_id(body.pointer("/operator/id")));
        if let Some(data) = body.get("_data") {
            out["satori_data"] = data.clone();
        }
    }

    let bytes = serde_json::to_vec(&out)?;
    let mut bytes = bytes;
    Ok(simd_json::to_owned_value(&mut bytes)?)
}

fn optional_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn string_id(value: Option<&Value>) -> String {
    value
        .and_then(value_id)
        .map(|value| value.to_string())
        .unwrap_or_default()
}

/// 原样保留实现端给的 ID：申请类事件的 `message.id` 是审批 flag，不是数字。
fn raw_id(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(value)) => value.clone(),
        Some(value) => value_id(value)
            .map(|value| value.to_string())
            .unwrap_or_default(),
        None => String::new(),
    }
}

fn parse_id(value: Option<&Value>) -> i64 {
    value.and_then(value_id).unwrap_or_default()
}

fn value_id(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_message_event() {
        let bot = BotStatus {
            adapter: "satori-qq".to_string(),
            platform: "red".to_string(),
            login_user: LoginUser {
                id: "10000".to_string(),
                ..Default::default()
            },
        };
        let event = json!({
            "type": "message-created",
            "timestamp": 1_700_000_000_000i64,
            "guild": {"id": "123", "name": "test"},
            "channel": {"id": "123"},
            "user": {"id": "42", "name": "Alice"},
            "member": {"nick": "A", "roles": [{"id": "admin"}]},
            "message": {"id": "7000000000000000000", "content": "hi <at id=\"7\"/>"}
        });
        let normalized = normalize_event(&event, &bot, &Default::default()).unwrap();
        assert_eq!(normalized.get_str("post_type"), Some("message"));
        assert_eq!(normalized.get_i64("group_id"), Some(123));
        assert_eq!(
            normalized.get_i64("message_id"),
            Some(7_000_000_000_000_000_000)
        );
        assert_eq!(normalized.get_str("raw_message"), Some("hi "));
    }

    #[test]
    fn private_message_has_no_group_id() {
        let bot = BotStatus {
            adapter: "satori-qq".to_string(),
            platform: "red".to_string(),
            login_user: LoginUser {
                id: "10000".to_string(),
                ..Default::default()
            },
        };
        let event = json!({
            "type": "message-created",
            "timestamp": 1_700_000_000_000i64,
            "channel": {"id": "private:42", "type": 1},
            "user": {"id": "42", "name": "Alice"},
            "message": {"id": "7000000000000000000", "content": "hello"}
        });
        let normalized = normalize_event(&event, &bot, &Default::default()).unwrap();
        let ctx_group = normalized
            .get_i64("group_id")
            .or_else(|| normalized.get_u64("group_id").map(|value| value as i64));
        assert_eq!(normalized.get_str("message_type"), Some("private"));
        assert_eq!(ctx_group, None);
    }

    /// QQ 客户端手发的消息带虚拟作者 `qq-client:{uin}`，真实身份在 satori_qq 扩展里。
    #[test]
    fn manual_self_message_keeps_the_real_author() {
        let bot = test_bot();
        let event = json!({
            "type": "message-created",
            "timestamp": 1_700_000_000_000i64,
            "guild": {"id": "123", "name": "test"},
            "channel": {"id": "123", "type": 0},
            "user": {"id": "qq-client:10000", "name": "我"},
            "satori_qq": {"manual_self": true, "actual_user_id": "10000"},
            "message": {"id": "7000000000000000000", "content": "自己发的"}
        });
        let normalized = normalize_event(&event, &bot, &Default::default()).unwrap();
        assert_eq!(normalized.get_i64("user_id"), Some(10000));
        assert_eq!(normalized.get_i64("group_id"), Some(123));
    }

    /// 加群申请的 `message.id` 是审批 flag，不是数字 ID，必须原样留住。
    #[test]
    fn group_request_keeps_the_approval_flag() {
        let bot = test_bot();
        let event = json!({
            "type": "guild-member-request",
            "timestamp": 1_700_000_000_000i64,
            "guild": {"id": "123"},
            "channel": {"id": "123", "type": 0},
            "user": {"id": "42"},
            "message": {"id": "flag-abc123", "content": "求进群"}
        });
        let normalized = normalize_event(&event, &bot, &Default::default()).unwrap();
        assert_eq!(normalized.get_str("post_type"), Some("request"));
        assert_eq!(normalized.get_str("flag"), Some("flag-abc123"));
        assert_eq!(normalized.get_str("comment"), Some("求进群"));
    }

    /// 禁言与解禁共用一个 Satori 事件，靠毫秒 duration 区分。
    #[test]
    fn lift_ban_is_told_apart_from_ban() {
        let bot = test_bot();
        let mute = json!({
            "type": "guild-member-updated",
            "timestamp": 1_700_000_000_000i64,
            "guild": {"id": "123"},
            "channel": {"id": "123", "type": 0},
            "user": {"id": "42"},
            "operator": {"id": "7"},
            "_type": "satori-qq/mute",
            "_data": {"duration": 600_000}
        });
        let normalized = normalize_event(&mute, &bot, &Default::default()).unwrap();
        assert_eq!(normalized.get_str("notice_type"), Some("group_ban"));
        assert_eq!(normalized.get_str("sub_type"), Some("ban"));
        assert_eq!(normalized.get_i64("duration"), Some(600));

        let lift = json!({
            "type": "guild-member-updated",
            "timestamp": 1_700_000_000_000i64,
            "guild": {"id": "123"},
            "channel": {"id": "123", "type": 0},
            "user": {"id": "42"},
            "operator": {"id": "7"},
            "_type": "satori-qq/mute",
            "_data": {"duration": 0}
        });
        let normalized = normalize_event(&lift, &bot, &Default::default()).unwrap();
        assert_eq!(normalized.get_str("sub_type"), Some("lift_ban"));
        assert_eq!(normalized.get_i64("duration"), Some(0));
    }

    fn test_bot() -> BotStatus {
        BotStatus {
            adapter: "satori-qq".to_string(),
            platform: "red".to_string(),
            login_user: LoginUser {
                id: "10000".to_string(),
                ..Default::default()
            },
        }
    }
}
