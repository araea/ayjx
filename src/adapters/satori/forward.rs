//! 把一条合并转发读成完整的聊天记录。
//!
//! QQ 有两条取回路径，质量差得很远：
//!
//! - `internal/get_forward` 传 resId 时走 `SsoRecvLongMsg` 的伪造节点协议。NT 客户端
//!   发出的图片、表情包在那条路上整段消失，节点只剩空正文，也没有逐条消息 ID。
//! - 传 `native:<父消息 ID>` 时走 QQ 内核缓存，图片、逐条 ID 和时间戳都在，代价是
//!   父消息一旦被缓存淘汰就查不到。
//!
//! 所以这里先要内核，失败再退回 resId，并把「退回过」写进 notes——否则模型会把
//! 协议丢失的正文当成群友原本就没说话。嵌套转发按同样规则逐层展开，受节点和层数
//! 双重预算约束，避免一条恶意转发把上下文撑爆。

use super::{LockedWriter, message};
use crate::event::Context;
use crate::message::{Message, Segment};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use simd_json::base::ValueAsScalar;

/// 一次展开最多读回多少条节点（含所有层）。
pub const MAX_NODES: usize = 60;
/// 嵌套转发最多再往里读几层。
pub const MAX_DEPTH: usize = 3;
/// 单条节点正文在转写里的最大字符数。
const MAX_NODE_CHARS: usize = 400;

/// 从哪里读这条合并转发。两个来源都给上时先试内核。
#[derive(Debug, Clone, Default)]
pub struct Source {
    /// `<message forward id="...">` 里的 resId。
    pub resource_id: Option<String>,
    /// 携带这条合并转发的那条消息的 ID，用于走内核缓存。
    pub message_id: Option<i64>,
    /// 这条消息所在的会话。satori-qq 的父消息缓存被淘汰后靠它重新定位内核记录。
    pub channel: Option<String>,
}

impl Source {
    pub fn new(resource_id: Option<String>, message_id: Option<i64>) -> Self {
        Self {
            resource_id: resource_id.filter(|value| !value.is_empty()),
            message_id: message_id.filter(|value| *value != 0),
            channel: None,
        }
    }

    /// 会话跟着整条展开链走：嵌套转发和父消息在同一个群里。
    pub fn in_channel(mut self, channel: impl Into<String>) -> Self {
        let channel = channel.into();
        self.channel = (!channel.is_empty()).then_some(channel);
        self
    }

    fn is_empty(&self) -> bool {
        self.resource_id.is_none() && self.message_id.is_none()
    }
}

/// 转发里的一条消息。
#[derive(Debug, Clone)]
pub struct Node {
    /// 0 是最外层，往里每嵌套一层加一。
    pub depth: usize,
    /// 内核路径才有；伪造节点协议返回空 ID。
    pub message_id: Option<i64>,
    pub user_id: String,
    pub name: String,
    /// Unix 秒；0 表示这条路径没给时间。
    pub time: i64,
    pub message: Message,
}

#[derive(Debug, Clone, Default)]
pub struct View {
    pub nodes: Vec<Node>,
    /// 触到节点或层数上限，后面还有没读的内容。
    pub truncated: bool,
    /// 读取过程中的降级与失败，必须让模型看见，别把缺失当原文。
    pub notes: Vec<String>,
}

impl View {
    /// 同一种降级在嵌套里会反复发生，说明一次就够。
    fn note(&mut self, text: String) {
        if !self.notes.contains(&text) {
            self.notes.push(text);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// 转发里出现过的图片直链，供多模态模型真正看到内容。
    pub fn images(&self) -> Vec<String> {
        let mut out = Vec::new();
        for node in &self.nodes {
            for segment in &node.message.0 {
                if !matches!(segment.type_.as_str(), "image" | "mface") {
                    continue;
                }
                if let Some(url) = segment
                    .data
                    .get("url")
                    .or_else(|| segment.data.get("file"))
                    .and_then(|value| value.as_str())
                    .filter(|url| url.starts_with("http"))
                    && !out.iter().any(|seen| seen == url)
                {
                    out.push(url.to_string());
                }
            }
        }
        out
    }

    /// 展开成给模型读的纯文本：每条一行，嵌套层缩进。
    pub fn transcript(&self) -> String {
        let mut out = String::new();
        for (index, node) in self.nodes.iter().enumerate() {
            let indent = "  ".repeat(node.depth);
            let clock = if node.time > 0 {
                chrono::DateTime::from_timestamp(node.time, 0)
                    .map(|time| {
                        time.with_timezone(&chrono::Local)
                            .format("%m-%d %H:%M")
                            .to_string()
                    })
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let who = if node.user_id.is_empty() {
                node.name.clone()
            } else {
                format!("{}({})", node.name, node.user_id)
            };
            out.push_str(&format!("{indent}{}. {who}", index + 1));
            if !clock.is_empty() {
                out.push_str(&format!(" {clock}"));
            }
            out.push_str("：");
            out.push_str(&describe(&node.message));
            out.push('\n');
        }
        if self.truncated {
            out.push_str("（已达展开上限，后面还有内容未读取）\n");
        }
        for note in &self.notes {
            out.push_str(&format!("（{note}）\n"));
        }
        out
    }
}

/// 读回一条合并转发的全部内容，失败也返回带 notes 的空视图。
pub async fn expand(ctx: &Context, writer: &LockedWriter, source: Source) -> View {
    let mut view = View::default();
    if source.is_empty() {
        return view;
    }
    walk(ctx, writer, source, 0, &mut view).await;
    view
}

/// 从一条消息里找出合并转发的入口；`message_id` 是这条消息自己的 ID。
pub fn source_of(message: &Message, message_id: Option<i64>) -> Option<Source> {
    let forward = message
        .0
        .iter()
        .find(|segment| segment.type_ == "forward")?;
    Some(Source::new(
        forward
            .data
            .get("id")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        message_id,
    ))
}

fn walk<'a>(
    ctx: &'a Context,
    writer: &'a LockedWriter,
    source: Source,
    depth: usize,
    view: &'a mut View,
) -> BoxFuture<'a, ()> {
    Box::pin(async move {
        if view.nodes.len() >= MAX_NODES {
            view.truncated = true;
            return;
        }
        if depth > MAX_DEPTH {
            view.truncated = true;
            return;
        }
        let raw = match resolve(ctx, writer, &source, view).await {
            Some(raw) => raw,
            None => return,
        };
        for mut node in raw {
            if view.nodes.len() >= MAX_NODES {
                view.truncated = true;
                return;
            }
            node.depth = depth;
            let nested = nested_sources(&node, source.channel.as_deref());
            let inline = inline_nodes(&node, depth + 1);
            view.nodes.push(node);
            for child in inline {
                if view.nodes.len() >= MAX_NODES {
                    view.truncated = true;
                    return;
                }
                view.nodes.push(child);
            }
            for child in nested {
                walk(ctx, writer, child, depth + 1, view).await;
            }
        }
    })
}

/// 先内核后 resId；两条都失败时把原因留在 notes 里。
async fn resolve(
    ctx: &Context,
    writer: &LockedWriter,
    source: &Source,
    view: &mut View,
) -> Option<Vec<Node>> {
    let mut failures = Vec::new();
    if let Some(message_id) = source.message_id {
        match fetch(
            ctx,
            writer,
            &format!("native:{message_id}"),
            source.channel.as_deref(),
        )
        .await
        {
            Ok(nodes) if !nodes.is_empty() => return Some(nodes),
            Ok(_) => failures.push("内核缓存返回空".to_string()),
            Err(error) => failures.push(format!("内核缓存不可用：{error}")),
        }
    }
    let Some(resource) = source.resource_id.clone() else {
        if !failures.is_empty() {
            view.note(format!("合并转发读取失败：{}", failures.join("；")));
        }
        return None;
    };
    match fetch(ctx, writer, &resource, source.channel.as_deref()).await {
        Ok(nodes) if !nodes.is_empty() => {
            if !failures.is_empty() {
                view.note(
                    "已退回旧协议读取：该路径不返回图片、表情包和逐条消息 ID，缺失的媒体不代表原文没有"
                        .to_string(),
                );
            }
            Some(nodes)
        }
        Ok(_) => {
            failures.push("转发内容为空".to_string());
            view.note(format!("合并转发读取失败：{}", failures.join("；")));
            None
        }
        Err(error) => {
            failures.push(format!("{error}"));
            view.note(format!("合并转发读取失败：{}", failures.join("；")));
            None
        }
    }
}

async fn fetch(
    ctx: &Context,
    writer: &LockedWriter,
    id: &str,
    channel: Option<&str>,
) -> Result<Vec<Node>, super::BotError> {
    let mut params = json!({"id": id});
    if let Some(channel) = channel {
        params["channel_id"] = json!(channel);
    }
    let value: Value = writer.call(ctx, "internal/get_forward", params).await?;
    Ok(parse(&value, &writer.resources()))
}

fn parse(value: &Value, resources: &message::ResourceProxy) -> Vec<Node> {
    let mut out = Vec::new();
    for item in value
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let user = item.get("user").unwrap_or(&Value::Null);
        out.push(Node {
            depth: 0,
            message_id: item.get("id").and_then(number),
            user_id: user
                .get("id")
                .and_then(text_id)
                .filter(|id| id != "0")
                .unwrap_or_default(),
            name: user
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            time: item
                .get("created_at")
                .and_then(Value::as_i64)
                .map(|millis| millis / 1000)
                .unwrap_or_default(),
            message: message::from_content_with(
                item.get("content").and_then(Value::as_str).unwrap_or(""),
                resources,
            ),
        });
    }
    out
}

/// 节点正文里还嵌着的合并转发；父消息 ID 用节点自己的，好继续走内核路径。
fn nested_sources(node: &Node, channel: Option<&str>) -> Vec<Source> {
    node.message
        .0
        .iter()
        .filter(|segment| segment.type_ == "forward")
        .map(|segment| {
            Source::new(
                segment
                    .data
                    .get("id")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                node.message_id,
            )
            .in_channel(channel.unwrap_or_default())
        })
        .filter(|source| !source.is_empty())
        .collect()
}

/// `<message forward>` 直接内联了子消息时，正文里就是 node 段，不必再发请求。
fn inline_nodes(node: &Node, depth: usize) -> Vec<Node> {
    node.message
        .0
        .iter()
        .filter(|segment| segment.type_ == "node")
        .map(|segment| Node {
            depth,
            message_id: segment
                .data
                .get("id")
                .and_then(|value| value.as_str())
                .and_then(|value| value.parse().ok()),
            user_id: segment
                .data
                .get("user_id")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string(),
            name: segment
                .data
                .get("nickname")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string(),
            time: 0,
            message: segment
                .data
                .get("content")
                .cloned()
                .and_then(|content| simd_json::serde::from_owned_value::<Message>(content).ok())
                .unwrap_or_default(),
        })
        .collect()
}

/// 一条节点正文的可读形式；媒体保留可辨认的占位，别让模型以为是空消息。
pub fn describe(message: &Message) -> String {
    let mut out = String::new();
    for segment in &message.0 {
        match segment.type_.as_str() {
            "text" => out.push_str(string(segment, "text")),
            "at" => {
                let target = string(segment, "qq");
                let name = string(segment, "name");
                if name.is_empty() {
                    out.push_str(&format!("@{target}"));
                } else {
                    out.push_str(&format!("@{name}({target})"));
                }
            }
            "face" => out.push_str(&format!("[表情:{}]", string(segment, "id"))),
            "image" => out.push_str("[图片]"),
            "mface" => out.push_str("[表情包]"),
            "record" => out.push_str("[语音]"),
            "video" => out.push_str("[视频]"),
            "file" => {
                let name = string(segment, "name");
                if name.is_empty() {
                    out.push_str("[文件]");
                } else {
                    out.push_str(&format!("[文件:{name}]"));
                }
            }
            "reply" => out.push_str(&format!("[引用:{}]", string(segment, "id"))),
            "json" => out.push_str("[卡片]"),
            "poke" => out.push_str("[戳一戳]"),
            "dice" => out.push_str("[骰子]"),
            "rps" => out.push_str("[猜拳]"),
            "forward" | "node" => out.push_str("[嵌套合并转发]"),
            other => out.push_str(&format!("[{other}]")),
        }
    }
    let flat = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > MAX_NODE_CHARS {
        let kept: String = flat.chars().take(MAX_NODE_CHARS).collect();
        format!("{kept}…（本条已截断）")
    } else {
        flat
    }
}

fn string<'a>(segment: &'a Segment, key: &str) -> &'a str {
    segment
        .data
        .get(key)
        .and_then(|value| value.as_str())
        .unwrap_or("")
}

fn number(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        .filter(|id| *id != 0)
}

fn text_id(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_i64().map(|id| id.to_string()))
        .or_else(|| value.as_u64().map(|id| id.to_string()))
        .filter(|id| !id.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy() -> message::ResourceProxy {
        message::ResourceProxy::new(
            "http://127.0.0.1:3001".to_string(),
            std::sync::Arc::new(Vec::new()),
        )
    }

    #[test]
    fn kernel_nodes_keep_ids_images_and_time() {
        let value = json!({"data":[
            {"id":"7683193934447916799","created_at":1788879862000_i64,
             "user":{"id":"1094950020","name":"清"},
             "content":"@楚梼 你是男的女的"},
            {"id":"7683206700481871416","created_at":1788872243000_i64,
             "user":{"id":"42","name":"smm"},
             "content":"<img src=\"http://127.0.0.1:3001/v1/assets/abc.image\"/>"}]});
        let nodes = parse(&value, &proxy());
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].message_id, Some(7683193934447916799));
        assert_eq!(nodes[0].name, "清");
        assert_eq!(nodes[0].time, 1788879862);
        let view = View {
            nodes,
            ..Default::default()
        };
        let text = view.transcript();
        assert!(text.contains("清(1094950020)"), "{text}");
        assert!(text.contains("[图片]"), "{text}");
        assert_eq!(
            view.images(),
            vec!["http://127.0.0.1:3001/v1/assets/abc.image".to_string()]
        );
    }

    #[test]
    fn legacy_nodes_survive_missing_ids_and_report_the_downgrade() {
        let value = json!({"data":[{"id":"","user":{"id":"1094950020","name":"楚梼"},
            "content":"<message><author id=\"1094950020\" name=\"楚梼\"/>啊？</message>"}]});
        let nodes = parse(&value, &proxy());
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].message_id, None);
        assert_eq!(describe(&nodes[0].message), "啊？");
    }

    #[test]
    fn nested_forwards_are_followed_through_the_owning_node() {
        let value = json!({"data":[{"id":"99","user":{"id":"7","name":"套娃"},
            "content":"<message forward id=\"res-inner\"/>"}]});
        let nodes = parse(&value, &proxy());
        let nested = nested_sources(&nodes[0], Some("282381753"));
        assert_eq!(nested.len(), 1);
        assert_eq!(nested[0].resource_id.as_deref(), Some("res-inner"));
        assert_eq!(nested[0].message_id, Some(99));
        assert_eq!(nested[0].channel.as_deref(), Some("282381753"));
        assert!(describe(&nodes[0].message).contains("嵌套合并转发"));
    }

    #[test]
    fn inline_nodes_do_not_need_another_request() {
        let content =
            "<message forward><message><author id=\"5\" name=\"甲\"/>一句话</message></message>";
        let message = message::from_content_with(content, &proxy());
        assert!(message.0.iter().any(|segment| segment.type_ == "node"));
        let node = Node {
            depth: 0,
            message_id: None,
            user_id: String::new(),
            name: String::new(),
            time: 0,
            message,
        };
        let inline = inline_nodes(&node, 1);
        assert_eq!(inline.len(), 1);
        assert_eq!(inline[0].name, "甲");
        assert_eq!(describe(&inline[0].message), "一句话");
    }

    /// 只读的真机核对：在指定群里找一条合并转发展开出来，不发任何消息。
    #[tokio::test]
    #[ignore = "需要本机 satori-qq 在线；设置 AYJX_FORWARD_LIVE_CHANNEL=<群号>，只读不发消息"]
    async fn live_forward_expansion_against_satori_qq() {
        let channel = std::env::var("AYJX_FORWARD_LIVE_CHANNEL").expect("群号");
        let endpoint = std::env::var("AYJX_FORWARD_LIVE_ENDPOINT")
            .unwrap_or_else(|_| "http://127.0.0.1:3001".to_string());
        let writer: LockedWriter = std::sync::Arc::new(super::super::SatoriClient::new(
            endpoint.clone(),
            std::env::var("AYJX_FORWARD_LIVE_TOKEN").ok(),
        ));
        writer.set_proxy_urls(vec![format!("{endpoint}/v1/proxy/")]);
        let mut ctx = crate::event::Context {
            event: crate::event::EventType::Init,
            config: std::sync::Arc::new(
                std::sync::RwLock::new(crate::config::AppConfig::default()),
            ),
            config_save_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            db: sea_orm::Database::connect("sqlite::memory:").await.unwrap(),
            scheduler: std::sync::Arc::new(crate::scheduler::Scheduler::new()),
            matcher: std::sync::Arc::new(crate::matcher::Matcher::new()),
            config_path: std::sync::Arc::from("unused-forward-live.toml"),
            bot: std::sync::Arc::new(crate::event::BotStatus {
                adapter: "satori-qq".into(),
                platform: "red".into(),
                login_user: crate::event::LoginUser::default(),
            }),
        };
        // Satori 的每个调用都带登录身份，先问一次真实账号再继续。
        let login: Value = writer
            .call(&ctx, "login.get", json!({}))
            .await
            .expect("login.get");
        ctx.bot = std::sync::Arc::new(crate::event::BotStatus {
            adapter: "satori-qq".into(),
            platform: login["platform"].as_str().unwrap_or("red").into(),
            login_user: crate::event::LoginUser {
                id: login["user"]["id"].as_str().unwrap_or_default().into(),
                ..Default::default()
            },
        });
        let found: Value = writer
            .call(
                &ctx,
                "internal/message_search",
                json!({"channel_id":channel,"query":"message forward","limit":3,"scan_limit":600}),
            )
            .await
            .expect("message_search");
        let hit = found["data"]
            .as_array()
            .and_then(|list| list.first())
            .expect("这个群最近没有合并转发可读");
        let message_id = hit["id"].as_str().and_then(|id| id.parse::<i64>().ok());
        let message =
            message::from_content_with(hit["content"].as_str().unwrap_or(""), &writer.resources());
        let source = source_of(&message, message_id)
            .expect("forward source")
            .in_channel(channel.clone());
        let view = expand(&ctx, &writer, source).await;
        println!("{}", view.transcript());
        println!("images: {:?}", view.images());
        assert!(!view.nodes.is_empty(), "{:?}", view.notes);
        assert!(
            view.nodes.iter().any(|node| !node.name.is_empty()),
            "节点应保留发送者"
        );
    }

    #[test]
    fn source_of_reads_the_forward_entry_point() {
        let message = message::from_content_with("<message forward id=\"abc\"/>", &proxy());
        let source = source_of(&message, Some(12)).expect("forward source");
        assert_eq!(source.resource_id.as_deref(), Some("abc"));
        assert_eq!(source.message_id, Some(12));
        assert_eq!(source.clone().in_channel("").channel, None);
        assert_eq!(
            source.in_channel("46360522").channel.as_deref(),
            Some("46360522")
        );
        assert!(source_of(&Message::new().text("hi"), Some(12)).is_none());
    }
}
