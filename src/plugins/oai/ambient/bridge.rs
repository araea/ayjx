//! 一轮 Pi 专属的 Unix RPC。写操作串行、回执可见、同一请求只执行一次。
use super::{
    AmbientConfig,
    actions::{self, Action, Part},
    window::{self, Turn},
};
use crate::{
    adapters::satori::{LockedWriter, forward, send_msg_id},
    event::Context,
    message::Message,
};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

pub(crate) struct Lease {
    socket: PathBuf,
    token: String,
    task: tokio::task::JoinHandle<()>,
    pub attempted: Arc<AtomicBool>,
    pub seq: Arc<AtomicU64>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.socket);
    }
}
impl Lease {
    pub(crate) fn env(&self) -> Vec<(String, String)> {
        vec![
            (
                "AYJX_CHAT_SOCKET".into(),
                self.socket.to_string_lossy().into_owned(),
            ),
            ("AYJX_CHAT_TOKEN".into(), self.token.clone()),
        ]
    }
    pub(crate) fn used(&self) -> bool {
        self.attempted.load(Ordering::SeqCst)
    }
    pub(crate) fn revision(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }
}

struct Session {
    ctx: Context,
    writer: LockedWriter,
    group: i64,
    config: AmbientConfig,
    scratch: PathBuf,
    media: PathBuf,
    seq: Arc<AtomicU64>,
    attempted: Arc<AtomicBool>,
    writes: usize,
    messages: usize,
    spoke: bool,
    started: Instant,
    receipts: HashMap<String, Value>,
    capabilities: Value,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn start(
    ctx: &Context,
    writer: &LockedWriter,
    group: i64,
    seq: u64,
    config: &AmbientConfig,
    scratch: &Path,
    base: &Path,
) -> Result<Lease> {
    let socket =
        std::env::temp_dir().join(format!("ayjx-chat-{:016x}.sock", rand::random::<u64>()));
    let listener = tokio::net::UnixListener::bind(&socket)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    let token = format!("{:032x}", rand::random::<u128>());
    let seq = Arc::new(AtomicU64::new(seq));
    let attempted = Arc::new(AtomicBool::new(false));
    let mut session = Session {
        ctx: ctx.clone(),
        writer: writer.clone(),
        group,
        config: config.clone(),
        scratch: scratch.to_path_buf(),
        media: base.join("media"),
        seq: seq.clone(),
        attempted: attempted.clone(),
        writes: 0,
        messages: 0,
        spoke: false,
        started: Instant::now(),
        receipts: HashMap::new(),
        capabilities: Value::Null,
    };
    let expected = token.clone();
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            // 超长和半截请求不能长时间堵塞整轮；每轮最多 64 个请求。
            let input = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                BufReader::new(read.take(128 * 1024)).read_line(&mut line),
            )
            .await;
            let response = match input {
                Ok(Ok(_)) if line.ends_with('\n') => match serde_json::from_str::<Value>(&line) {
                    Ok(request) if request["token"].as_str() == Some(&expected) => {
                        session.request(request).await
                    }
                    _ => json!({"ok":false,"error":"invalid request or expired token"}),
                },
                _ => json!({"ok":false,"error":"invalid request size or timeout"}),
            };
            let _ = write.write_all(format!("{response}\n").as_bytes()).await;
        }
    });
    Ok(Lease {
        socket,
        token,
        task,
        attempted,
        seq,
    })
}

impl Session {
    fn turns(&self) -> Vec<Turn> {
        window::with_group(self.group, |s| s.recent(80))
    }
    fn current(&self) -> bool {
        super::current(&self.ctx, self.group, self.seq.load(Ordering::SeqCst))
    }
    async fn request(&mut self, request: Value) -> Value {
        let key = request["id"].as_str().unwrap_or("").to_string();
        if key.is_empty() || key.len() > 200 {
            return json!({"ok":false,"error":"request id required"});
        }
        if let Some(receipt) = self.receipts.get(&key) {
            return receipt.clone();
        }
        if self.receipts.len() >= 64 {
            return json!({"ok":false,"error":"request budget exhausted"});
        }
        let result = self.execute(&request).await;
        let response = match result {
            Ok(value) => json!({"ok":true,"result":value}),
            Err(error) => json!({"ok":false,"error":format!("{error:#}")}),
        };
        self.receipts.insert(key, response.clone());
        response
    }
    async fn execute(&mut self, request: &Value) -> Result<Value> {
        match request["op"].as_str().unwrap_or("") {
            "context" => {
                ensure!(self.enabled(), "该群的搭话功能已停用");
                if self.capabilities.is_null() {
                    self.capabilities = match self
                        .writer
                        .call::<_, Value>(&self.ctx, "login.get", json!({}))
                        .await
                    {
                        Ok(login) => {
                            json!({"adapter":self.ctx.bot.adapter,"features":login.get("features"),"qq_extensions":self.ctx.bot.adapter == "satori-qq"})
                        }
                        Err(_) => {
                            json!({"adapter":self.ctx.bot.adapter,"features":null,"qq_extensions":self.ctx.bot.adapter == "satori-qq","note":"能力查询失败，实际动作以回执为准"})
                        }
                    };
                }
                let (seq, turns, rhythm) = window::with_group(self.group, |s| {
                    s.take_mention();
                    (
                        s.seq,
                        s.recent(self.config.context_turns.clamp(1, 80)),
                        s.rhythm(),
                    )
                });
                self.seq.store(seq, Ordering::SeqCst);
                let turns: Vec<Value> = turns.iter().map(|t| json!({
                    "message_id":t.message_id.to_string(),"user_id":t.user_id.to_string(),"name":t.name,
                    "text":t.text,"from_me":t.from_me,"time":t.at,"elements":t.elements,
                })).collect();
                let mut media = Vec::new();
                if let Ok(mut entries) = tokio::fs::read_dir(&self.media).await {
                    while let Ok(Some(entry)) = entries.next_entry().await {
                        if media.len() >= 40 {
                            break;
                        }
                        if entry.file_type().await.is_ok_and(|t| t.is_file()) {
                            media.push(entry.path().to_string_lossy().into_owned());
                        }
                    }
                }
                Ok(
                    json!({"revision":seq,"group_id":self.group.to_string(),"self_id":self.ctx.bot.login_user.id,
                    "capabilities":self.capabilities,"rhythm":rhythm,"messages":turns,"media":media,
                    "writes_remaining":self.config.max_actions.clamp(1,12).saturating_sub(self.writes),
                    "messages_remaining":self.config.max_messages.clamp(1,5).saturating_sub(self.messages)}),
                )
            }
            "read" => {
                ensure!(self.enabled(), "该群的搭话功能已停用");
                let turns = self.turns();
                let id = request["message_id"].as_str().unwrap_or("");
                let turn = actions::message(&turns, id)?;
                if request["forward"].as_bool().unwrap_or(false) {
                    let source = forward::source_of(&turn.elements, Some(turn.message_id))
                        .ok_or_else(|| anyhow::anyhow!("该消息不是合并转发"))?;
                    let view = forward::expand(&self.ctx, &self.writer, source).await;
                    ensure!(
                        !view.is_empty(),
                        "合并转发没有读到内容：{}",
                        if view.notes.is_empty() {
                            "原文为空".to_string()
                        } else {
                            view.notes.join("；")
                        }
                    );
                    Ok(json!({
                        "message_id": id,
                        "node_count": view.nodes.len(),
                        "truncated": view.truncated,
                        "notes": view.notes,
                        "images": view.images(),
                        "transcript": view.transcript(),
                        "nodes": view.nodes.iter().map(|node| json!({
                            "depth": node.depth,
                            "message_id": node.message_id.map(|id| id.to_string()),
                            "user_id": node.user_id,
                            "name": node.name,
                            "time": node.time,
                            "text": forward::describe(&node.message),
                            "elements": node.message,
                        })).collect::<Vec<_>>(),
                    }))
                } else {
                    self.rpc(
                        "message.get",
                        json!({"channel_id":self.group.to_string(),"message_id":id}),
                    )
                    .await
                }
            }
            "action" => {
                // 一旦选择工具动作，就不再把最终解释当作第二份消息发送。
                self.attempted.store(true, Ordering::SeqCst);
                ensure!(
                    self.current(),
                    "群聊已更新或停用。先读 satori_context 再决定，禁止盲目重试旧动作"
                );
                let action: Action = serde_json::from_value(request["request"].clone())?;
                let turns = self.turns();
                action.validate(&turns)?;
                ensure!(
                    self.writes < self.config.max_actions.clamp(1, 12),
                    "本轮动作额度已用完"
                );
                ensure!(
                    !action.is_message() || self.messages < self.config.max_messages.clamp(1, 5),
                    "本轮消息额度已用完"
                );
                self.writes += 1;
                if action.is_message() {
                    self.messages += 1;
                }
                let result = self.perform(&action, &turns).await;
                if let Err(ref error) = result {
                    // 错误也进入下一轮上下文；网络超时可能已经成功，不自动重放。
                    self.record(
                        format!(
                            "[动作未确认 {}：{}；勿盲目重复]",
                            request["request"]["action"], error
                        ),
                        0,
                        Message::new(),
                        false,
                    );
                }
                result
            }
            _ => anyhow::bail!("unknown operation"),
        }
    }
    fn enabled(&self) -> bool {
        let c = crate::plugins::get_config_or_default::<super::super::OaiConfig>(&self.ctx, "oai");
        c.enabled && c.ambient.enabled && c.ambient.groups.contains(&self.group)
    }
    async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        self.writer
            .call(&self.ctx, method, params)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
    async fn source(&self, source: &str, name: &str) -> Result<String> {
        if source.starts_with("https://") || source.starts_with("http://") {
            let url = url::Url::parse(source)?;
            ensure!(
                url.host_str().is_some() && url.username().is_empty() && url.password().is_none(),
                "资源 URL 无效"
            );
            return Ok(source.into());
        }
        // Termux 的私有文件不能直接让 QQ 进程读取，必须走 upload.create。
        let input = Path::new(source);
        let path = tokio::fs::canonicalize(if input.is_absolute() {
            input.to_path_buf()
        } else {
            self.scratch.join(input)
        })
        .await?;
        let scratch = tokio::fs::canonicalize(&self.scratch).await?;
        let media = tokio::fs::canonicalize(&self.media).await.ok();
        ensure!(
            path.starts_with(scratch) || media.is_some_and(|root| path.starts_with(root)),
            "本地资源应放在本轮工作目录或 ambient/media，不能发送任意私有文件"
        );
        let meta = tokio::fs::metadata(&path).await?;
        ensure!(
            meta.is_file() && meta.len() <= 20 * 1024 * 1024,
            "文件须为普通文件且不超过 20 MiB"
        );
        let bytes = tokio::fs::read(path).await?;
        let uploaded = self
            .writer
            .upload(&self.ctx, bytes, name, "application/octet-stream")
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        uploaded["file"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("上传未返回资源"))
    }
    async fn perform(&mut self, action: &Action, turns: &[Turn]) -> Result<Value> {
        let group = self.group.to_string();
        let (method, params, summary) = match action {
            Action::Send { parts, reply_to } => {
                let mut msg = Message::new();
                if let Some(id) = reply_to {
                    msg = msg.reply(id);
                }
                for part in parts {
                    msg = match part {
                        Part::Text { text } => msg.text(text),
                        Part::At { user_id } => msg.at(user_id),
                        Part::Face { id } => msg.face(id),
                        Part::Image { source } => {
                            msg.image(self.source(source, "image.png").await?)
                        }
                        Part::File { source, name } => {
                            msg.file(self.source(source, name).await?, Some(name))
                        }
                        Part::Audio { source } => {
                            msg.record(self.source(source, "audio.mp3").await?)
                        }
                        Part::Video { source } => {
                            msg.video(self.source(source, "video.mp4").await?)
                        }
                        Part::Sticker { message_id, index } => {
                            msg.0.extend(
                                actions::sticker(actions::message(turns, message_id)?, *index)?.0,
                            );
                            msg
                        }
                        Part::Dice => msg.dice(),
                        Part::Rps => msg.rps(),
                    };
                }
                return self.send(msg).await;
            }
            Action::Forward { message_ids, texts } => {
                let mut msg = Message::new();
                for id in message_ids {
                    let t = actions::message(turns, id)?;
                    msg = msg.node_custom(t.user_id, &t.name, t.elements.clone());
                }
                for text in texts {
                    msg = msg.node_custom(
                        &self.ctx.bot.login_user.id,
                        self.ctx.bot.login_user.name.as_deref().unwrap_or("我"),
                        Message::new().text(text),
                    );
                }
                return self.send(msg).await;
            }
            Action::Poke { user_id } => (
                "internal/poke",
                json!({"guild_id":group,"user_id":user_id}),
                format!("[戳一戳 {user_id}]"),
            ),
            Action::Like { user_id, times } => (
                "internal/like",
                json!({"user_id":user_id,"times":times}),
                format!("[给 {user_id} 资料卡点赞 {times} 次]"),
            ),
            Action::React {
                message_id,
                emoji_id,
                remove,
            } => (
                if *remove {
                    "reaction.delete"
                } else {
                    "reaction.create"
                },
                json!({"channel_id":group,"message_id":message_id,"emoji_id":emoji_id}),
                format!(
                    "[{}消息 {message_id} 的表态 {emoji_id}]",
                    if *remove { "取消" } else { "添加" }
                ),
            ),
            Action::Recall { message_id } => (
                "message.delete",
                json!({"channel_id":group,"message_id":message_id}),
                format!("[撤回自己的消息 {message_id}]"),
            ),
        };
        ensure!(
            !method.starts_with("internal/") || self.ctx.bot.adapter == "satori-qq",
            "当前适配器未声明 QQ 扩展"
        );
        ensure!(
            self.current(),
            "群聊已更新，动作未执行；请读 satori_context"
        );
        let result = self.rpc(method, params).await?;
        if let Action::Recall { message_id } = action {
            window::with_group(self.group, |s| {
                s.recall(actions::id(message_id).unwrap_or(0))
            });
        }
        self.record(summary, 0, Message::new(), true);
        Ok(json!({"status":"confirmed","data":result}))
    }
    async fn send(&mut self, message: Message) -> Result<Value> {
        let spoken = super::plain_text(&message);
        let pace = self.config.pace();
        let typing = pace.typing_delay(spoken.chars().count());
        let delay = if self.spoke {
            pace.gap() + typing
        } else {
            pace.think_delay(self.started.elapsed()) + typing.saturating_sub(self.started.elapsed())
        };
        tokio::time::sleep(delay).await;
        ensure!(
            self.current(),
            "准备发送期间群聊已更新，尚未发送；请读 satori_context"
        );
        let receipt = send_msg_id(
            &self.ctx,
            self.writer.clone(),
            Some(self.group),
            None,
            &message,
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        let id = receipt.ok_or_else(|| {
            anyhow::anyhow!("没有消息回执，可能被插件拦截或条件过期，不能算发送成功")
        })?;
        let numeric = actions::id(&id)?;
        self.record(spoken, numeric, message, true);
        Ok(json!({"status":"confirmed","message_id":id}))
    }
    fn record(&mut self, text: String, message_id: i64, elements: Message, success: bool) {
        let me = self.ctx.bot.login_user.id.parse().unwrap_or(0);
        info!(target: "Plugin/OAI", "群 {} 动作：{}", self.group, text);
        window::with_group(self.group, |s| {
            if success && !self.spoke {
                s.mark_spoke();
            }
            s.receive(Turn {
                user_id: me,
                name: "我".into(),
                text: text.chars().take(1000).collect(),
                images: vec![],
                elements,
                message_id,
                from_me: true,
                mentions_me: false,
                at: chrono::Local::now().timestamp(),
            });
        });
        self.spoke |= success;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{AppConfig, build_config},
        event::{BotStatus, EventType, LoginUser},
        plugins::oai::OaiConfig,
    };
    use std::sync::{Mutex, RwLock};

    async fn fixture(
        group: i64,
    ) -> (
        Context,
        LockedWriter,
        Arc<Mutex<Vec<(String, Value)>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let calls = requests.clone();
        let task = tokio::spawn(async move {
            let mut next = 7837409278651234567_i64;
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut first = String::new();
                reader.read_line(&mut first).await.unwrap();
                let method = first
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .trim_start_matches("/v1/")
                    .to_string();
                let mut size = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(s) = line.to_lowercase().strip_prefix("content-length:") {
                        size = s.trim().parse().unwrap();
                    }
                }
                let mut bytes = vec![0; size];
                reader.read_exact(&mut bytes).await.unwrap();
                let body =
                    serde_json::from_slice::<Value>(&bytes).unwrap_or(json!({"multipart":true}));
                calls.lock().unwrap().push((method.clone(), body.clone()));
                let response = match method.as_str() {
                    "login.get" => json!({"features":["message.create","message.delete","reaction.create","reaction.delete","upload.create"]}),
                    "upload.create" => json!({"file":"internal:red/10000/_tmp/test"}),
                    "message.create" => { next += 1; json!([{"id":next.to_string()}]) },
                    "message.get" => json!({"id":body["message_id"],"content":"原始内容"}),
                    // 真机行为：内核缓存有图片和逐条 ID，resId 那条旧协议两样都没有。
                    "internal/get_forward" => match body["id"].as_str().unwrap_or("") {
                        "native:124" => json!({"data":[
                            {"id":"9001","created_at":1788879862000_i64,"user":{"id":"42","name":"群友"},
                             "content":"转发原文<img src=\"https://example.com/in-forward.png\"/>"},
                            {"id":"9002","created_at":1788879870000_i64,"user":{"id":"43","name":"套娃"},
                             "content":"<message forward id=\"res-inner\"/>"}]}),
                        "native:9002" => json!({"message":"native forward is no longer cached"}),
                        "res-inner" => json!({"data":[{"id":"","user":{"id":"44","name":"里层"},
                             "content":"<message><author id=\"44\" name=\"里层\"/>最里面这句</message>"}]}),
                        _ => json!({"data":[{"id":"","user":{"id":"42","name":"群友"},"content":"转发原文"}]}),
                    },
                    _ => json!({}),
                }.to_string();
                let mut stream = reader.into_inner();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",response.len(),response).as_bytes()).await.unwrap();
            }
        });
        let oai = OaiConfig {
            ambient: AmbientConfig {
                enabled: true,
                groups: vec![group],
                max_actions: 12,
                max_messages: 5,
                typing_cpm: 60000,
                voice_cpm: 60000,
                think_seconds: 0.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut config = AppConfig::default();
        for plugin in crate::plugins::get_plugins() {
            config.plugins.insert(
                plugin.name.into(),
                toml::from_str("enabled = false").unwrap(),
            );
        }
        config.plugins.insert("oai".into(), build_config(oai));
        let ctx = Context {
            event: EventType::Init,
            config: Arc::new(RwLock::new(config)),
            config_save_lock: Arc::new(tokio::sync::Mutex::new(())),
            db: sea_orm::Database::connect("sqlite::memory:").await.unwrap(),
            scheduler: Arc::new(crate::scheduler::Scheduler::new()),
            matcher: Arc::new(crate::matcher::Matcher::new()),
            config_path: Arc::from("unused-social-test.toml"),
            bot: Arc::new(BotStatus {
                adapter: "satori-qq".into(),
                platform: "red".into(),
                login_user: LoginUser {
                    id: "10000".into(),
                    name: Some("我".into()),
                    ..Default::default()
                },
            }),
        };
        window::with_group(group, |s| {
            *s = Default::default();
            s.receive(Turn {
                user_id: 42,
                name: "群友".into(),
                text: "测试".into(),
                images: vec![],
                elements: Message::new()
                    .text("原文")
                    .image("https://example.com/a.gif"),
                message_id: 123,
                mentions_me: false,
                from_me: false,
                at: 0,
            });
            // 另一条带合并转发的消息，供 satori_read 展开。
            s.receive(Turn {
                user_id: 42,
                name: "群友".into(),
                text: "[合并转发，可用 satori_read 展开]".into(),
                images: vec![],
                elements: Message::new().add("forward", {
                    let mut data = simd_json::owned::Object::new();
                    data.insert("id".into(), simd_json::owned::Value::from("res-outer"));
                    data
                }),
                message_id: 124,
                mentions_me: false,
                from_me: false,
                at: 0,
            });
        });
        (
            ctx,
            Arc::new(crate::adapters::satori::SatoriClient::new(
                format!("http://{address}"),
                None,
            )),
            requests,
            task,
        )
    }
    async fn request(lease: &Lease, value: Value) -> Value {
        let mut value = value;
        value["token"] = json!(lease.token);
        let mut stream = tokio::net::UnixStream::connect(&lease.socket)
            .await
            .unwrap();
        stream
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }
    async fn action(lease: &Lease, id: &str, value: Value) -> Value {
        request(lease, json!({"id":id,"op":"action","request":value})).await
    }

    #[tokio::test]
    async fn reading_a_forward_prefers_the_kernel_copy_and_follows_the_nested_one() {
        let group = -8_000_102;
        let (ctx, writer, calls, server) = fixture(group).await;
        let dir =
            crate::plugins::oai::pi_agent::ScratchDir::under(&std::env::temp_dir(), "social-read")
                .unwrap();
        tokio::fs::create_dir(dir.path().join("media")).await.unwrap();
        let config = crate::plugins::get_config_or_default::<OaiConfig>(&ctx, "oai").ambient;
        let lease = start(&ctx, &writer, group, 1, &config, dir.path(), dir.path())
            .await
            .unwrap();
        let read = request(
            &lease,
            json!({"id":"read","op":"read","message_id":"124","forward":true}),
        )
        .await;
        assert_eq!(read["ok"], true, "{read}");
        let result = &read["result"];
        let transcript = result["transcript"].as_str().unwrap();
        assert!(transcript.contains("群友(42)"), "{transcript}");
        assert!(transcript.contains("[图片]"), "{transcript}");
        // 内层是靠嵌套 resId 读到的，缩进比外层深一级。
        assert!(transcript.contains("  3. 里层(44)"), "{transcript}");
        assert!(transcript.contains("最里面这句"), "{transcript}");
        assert_eq!(result["node_count"], 3);
        assert_eq!(
            result["images"][0],
            "https://example.com/in-forward.png"
        );
        // 内核路径给出真实逐条 ID，旧协议节点没有。
        assert_eq!(result["nodes"][0]["message_id"], "9001");
        assert!(result["nodes"][2]["message_id"].is_null());
        assert!(
            result["notes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|note| note.as_str().unwrap_or("").contains("旧协议")),
            "{result}"
        );
        let plain = request(
            &lease,
            json!({"id":"plain","op":"read","message_id":"123","forward":true}),
        )
        .await;
        assert_eq!(plain["ok"], false, "{plain}");
        let ids: Vec<String> = calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, _)| method == "internal/get_forward")
            .map(|(_, body)| body["id"].as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(ids, ["native:124", "native:9002", "res-inner"]);
        drop(lease);
        server.abort();
    }

    #[tokio::test]
    async fn real_rpc_chain_retains_receipts_uploads_and_deduplicates() {
        let group = -8_000_101;
        let (ctx, writer, calls, server) = fixture(group).await;
        let dir =
            crate::plugins::oai::pi_agent::ScratchDir::under(&std::env::temp_dir(), "social-test")
                .unwrap();
        tokio::fs::create_dir(dir.path().join("media"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("answer.txt"), "检查第二步\n来源链接")
            .await
            .unwrap();
        let config = crate::plugins::get_config_or_default::<OaiConfig>(&ctx, "oai").ambient;
        let lease = start(&ctx, &writer, group, 1, &config, dir.path(), dir.path())
            .await
            .unwrap();
        let context = request(&lease, json!({"id":"context","op":"context"})).await;
        assert_eq!(context["result"]["messages"][0]["message_id"], "123");
        let sent = action(&lease,"send",json!({"action":"send","reply_to":"123","parts":[{"type":"at","user_id":"42"},{"type":"text","text":"好 这步\n有问题，重试？"},{"type":"sticker","message_id":"123"}]})).await;
        assert_eq!(sent["ok"], true, "{sent}");
        let mid = sent["result"]["message_id"].as_str().unwrap();
        assert!(mid.len() > 16);
        let duplicate = action(
            &lease,
            "send",
            json!({"action":"send","parts":[{"type":"text","text":"不应发送"}]}),
        )
        .await;
        assert_eq!(sent, duplicate);
        for (id, args) in [
            ("poke", json!({"action":"poke","user_id":"42"})),
            ("like", json!({"action":"like","user_id":"42","times":2})),
            (
                "react",
                json!({"action":"react","message_id":"123","emoji_id":"76"}),
            ),
            (
                "unreact",
                json!({"action":"react","message_id":"123","emoji_id":"76","remove":true}),
            ),
            (
                "file",
                json!({"action":"send","parts":[{"type":"file","source":"answer.txt","name":"步骤.txt"}]}),
            ),
            (
                "forward",
                json!({"action":"forward","message_ids":["123"],"texts":["我的整理"]}),
            ),
            ("recall", json!({"action":"recall","message_id":mid})),
        ] {
            let r = action(&lease, id, args).await;
            assert_eq!(r["ok"], true, "{id}: {r}");
        }
        assert!(!window::with_group(group, |s| s.is_own_message(mid.parse().unwrap())));
        assert_eq!(
            action(
                &lease,
                "notmine",
                json!({"action":"recall","message_id":"123"})
            )
            .await["ok"],
            false
        );
        let calls = calls.lock().unwrap();
        let sends: Vec<_> = calls
            .iter()
            .filter(|(m, _)| m == "message.create")
            .collect();
        assert_eq!(sends.len(), 3);
        let content = sends[0].1["content"].as_str().unwrap();
        assert!(content.contains("<quote id=\"123\"/>"), "{content}");
        assert!(content.contains("<at id=\"42\"/>"), "{content}");
        assert!(content.contains("好 这步\n有问题，重试？"), "{content}");
        assert!(
            sends[1].1["content"]
                .as_str()
                .unwrap()
                .contains("internal:red/10000/_tmp/test")
        );
        assert!(sends[2].1["content"].as_str().unwrap().contains("forward"));
        assert!(
            calls
                .iter()
                .any(|(m, p)| m == "internal/poke" && p["guild_id"] == group.to_string())
        );
        drop(calls);
        assert_eq!(window::with_group(group, |s| s.spoken_last_hour()), 1);
        let path = lease.socket.clone();
        drop(lease);
        assert!(!path.exists());
        server.abort();
    }

    #[tokio::test]
    async fn stale_context_disabled_group_and_private_files_do_not_send() {
        let group = -8_000_102;
        let (ctx, writer, calls, server) = fixture(group).await;
        let dir =
            crate::plugins::oai::pi_agent::ScratchDir::under(&std::env::temp_dir(), "social-test")
                .unwrap();
        let config = crate::plugins::get_config_or_default::<OaiConfig>(&ctx, "oai").ambient;
        let lease = start(&ctx, &writer, group, 1, &config, dir.path(), dir.path())
            .await
            .unwrap();
        window::with_group(group, |s| s.seq += 1);
        let r = action(&lease, "stale", json!({"action":"poke","user_id":"42"})).await;
        assert_eq!(r["ok"], false);
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(
            request(&lease, json!({"id":"refresh","op":"context"})).await["ok"],
            true
        );
        let r = action(&lease,"secret",json!({"action":"send","parts":[{"type":"file","source":"/proc/version","name":"secret.txt"}]})).await;
        assert_eq!(r["ok"], false);
        let mut disabled = config;
        disabled.enabled = false;
        ctx.config.write().unwrap().plugins.insert(
            "oai".into(),
            build_config(OaiConfig {
                ambient: disabled,
                ..Default::default()
            }),
        );
        assert_eq!(
            action(&lease, "disabled", json!({"action":"like","user_id":"42"})).await["ok"],
            false
        );
        assert!(calls.lock().unwrap().iter().all(|(m, _)| m == "login.get"));
        drop(lease);
        server.abort();
    }
    #[tokio::test]
    #[ignore = "真实 Pi 模型验证；所有 QQ 动作只发到本地假服务"]
    async fn live_pi_social_tool_selection() {
        let group = -8_000_103;
        let (ctx, writer, calls, server) = fixture(group).await;
        let dir =
            crate::plugins::oai::pi_agent::ScratchDir::under(&std::env::temp_dir(), "social-live")
                .unwrap();
        super::super::init(dir.path()).await.unwrap();
        let config = crate::plugins::get_config_or_default::<OaiConfig>(&ctx, "oai").ambient;
        window::with_group(group, |s| {
            let mut t = s.recent(1)[0].clone();
            t.text = "@你 给我这条消息点个赞的表态就好，不用再发文字".into();
            t.mentions_me = true;
            *s = Default::default();
            s.receive(t);
        });
        let turns = window::with_group(group, |s| s.recent(20));
        let mut seq = 1;
        let raw = super::super::speak::compose(
            "pi",
            &super::super::base_dir(dir.path()),
            &super::super::skill_dir(dir.path()),
            super::super::PERSONA,
            &config,
            Some(std::time::Duration::from_secs(70)),
            &turns,
            &[],
            true,
            "群友刚刚在与你正常交流",
            Some((&ctx, &writer, group, &mut seq)),
        )
        .await
        .unwrap();
        let methods: Vec<String> = calls
            .lock()
            .unwrap()
            .iter()
            .map(|(m, _)| m.clone())
            .collect();
        println!("Pi 模型动作选择：{methods:?}，最终正文：{raw}");
        assert!(methods.contains(&"reaction.create".into()), "{methods:?}");
        assert!(!methods.contains(&"message.create".into()));
        assert!(raw.contains("[silent]"));
        server.abort();
    }
}
