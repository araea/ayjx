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
}

impl SatoriClient {
    fn new(endpoint: String, token: Option<String>) -> Self {
        Self {
            endpoint,
            token: token.filter(|value| !value.trim().is_empty()),
            http: reqwest::Client::new(),
            console: false,
        }
    }

    pub fn console() -> Self {
        Self {
            endpoint: String::new(),
            token: None,
            http: reqwest::Client::new(),
            console: true,
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

async fn connect_and_listen(
    config: &BotConfig,
    global_config: Arc<RwLock<AppConfig>>,
    db: DatabaseConnection,
    scheduler: Arc<Scheduler>,
    save_lock: Arc<AsyncMutex<()>>,
    config_path: Arc<str>,
) -> Result<(), BotError> {
    let endpoint = normalize_endpoint(config.url.as_deref().ok_or("Satori URL 未配置")?)?;
    let events_url = events_url(&endpoint)?;
    let request = events_url.into_client_request()?;
    let (stream, _) = connect_async(request).await?;
    let (mut ws_write, mut ws_read) = stream.split();

    let token = effective_token(config);
    let identify = if let Some(token) = token.as_deref() {
        json!({"op": 3, "body": {"token": token}})
    } else {
        json!({"op": 3, "body": {}})
    };
    ws_write
        .send(WsMessage::Text(identify.to_string().into()))
        .await?;

    let ready = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(frame) = ws_read.next().await {
            let frame = frame?;
            if let WsMessage::Text(text) = frame {
                let packet: Value = serde_json::from_str(&text)?;
                if packet.get("op").and_then(Value::as_i64) == Some(4) {
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
                    Some(0) => {
                        let Some(body) = packet.get("body") else {
                            continue;
                        };
                        let event = match normalize_event(body, &bot_status) {
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
                    Some(1) => {
                        ws_write
                            .send(WsMessage::Text(
                                json!({"op": 2, "body": {}}).to_string().into(),
                            ))
                            .await?;
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
    dispatch_send(ctx, writer, group_id, user_id, message).await
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

async fn dispatch_send<M>(
    ctx: &Context,
    writer: LockedWriter,
    group_id: Option<i64>,
    user_id: Option<i64>,
    message: M,
) -> Result<(), BotError>
where
    M: Serialize,
{
    let (message_type, group_id, user_id) = if let Some(id) = group_id.filter(|id| *id != 0) {
        ("group", Some(id), None)
    } else if let Some(id) = user_id.filter(|id| *id != 0) {
        ("private", None, Some(id))
    } else {
        return Ok(());
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
    let packet = SendPacket {
        action: "message.create".to_string(),
        params,
        original_event,
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
    Ok(())
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
    let _: Vec<Value> = writer
        .call(
            ctx,
            "message.create",
            json!({"channel_id": channel_id, "content": content}),
        )
        .await?;
    Ok(())
}

fn normalize_event(body: &Value, bot: &BotStatus) -> Result<Event, BotError> {
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
    let group_id = parse_id(guild.get("id"));
    let mut user_id = parse_id(user.get("id"));
    if user_id == 0 {
        user_id = body
            .pointer("/satori_qq/actual_user_id")
            .and_then(value_id)
            .unwrap_or_default();
    }
    let mut out = json!({
        "time": timestamp,
        "self_id": bot.login_user.id.parse::<i64>().unwrap_or_default(),
        "satori_type": event_type,
        "_satori": body,
    });

    if event_type == "message-created" {
        let message = body.get("message").unwrap_or(&Value::Null);
        let content = message.get("content").and_then(Value::as_str).unwrap_or("");
        let chain = message::from_content(content);
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
        out["group_id"] = json!(group_id);
        out["group_name"] = json!(
            guild
                .get("name")
                .or_else(|| channel.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("")
        );
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
        out["post_type"] = json!(post_type);
        out["notice_type"] = json!(notice_type);
        out["request_type"] = json!(request_type);
        out["sub_type"] = json!(sub_type);
        out["group_id"] = json!(group_id);
        out["user_id"] = json!(user_id);
        out["message_id"] = json!(parse_id(body.pointer("/message/id")));
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
        let normalized = normalize_event(&event, &bot).unwrap();
        assert_eq!(normalized.get_str("post_type"), Some("message"));
        assert_eq!(normalized.get_i64("group_id"), Some(123));
        assert_eq!(
            normalized.get_i64("message_id"),
            Some(7_000_000_000_000_000_000)
        );
        assert_eq!(normalized.get_str("raw_message"), Some("hi "));
    }
}
