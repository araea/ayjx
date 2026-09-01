#![allow(dead_code)]

use super::{LockedWriter, message};
use crate::event::{Context, EventType};
use crate::message::Message;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use simd_json::derived::ValueObjectAccessAsScalar;

pub type ApiError = Box<dyn std::error::Error + Send + Sync>;

pub async fn call_action<P, R>(
    ctx: &Context,
    writer: LockedWriter,
    action: &str,
    params: P,
) -> Result<R, ApiError>
where
    P: Serialize,
    R: serde::de::DeserializeOwned,
{
    writer.call(ctx, action, params).await
}

pub async fn call_action_no_wait<P>(
    ctx: &Context,
    writer: LockedWriter,
    action: &str,
    params: P,
) -> Result<(), ApiError>
where
    P: Serialize,
{
    let _: Value = writer.call(ctx, action, params).await?;
    Ok(())
}

pub async fn delete_msg(
    ctx: &Context,
    writer: LockedWriter,
    message_id: i64,
) -> Result<(), ApiError> {
    let _: Value = writer
        .call(
            ctx,
            "message.delete",
            json!({"channel_id": channel_id(ctx)?, "message_id": message_id.to_string()}),
        )
        .await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct SenderInfo {
    pub nickname: Option<String>,
    pub card: Option<String>,
    #[serde(flatten)]
    pub other: std::collections::HashMap<String, simd_json::OwnedValue>,
}

#[derive(Debug, Deserialize)]
pub struct MsgData {
    pub time: i64,
    pub message_type: String,
    pub message_id: i64,
    pub real_id: i64,
    pub sender: SenderInfo,
    pub message: Message,
}

pub async fn get_msg(
    ctx: &Context,
    writer: LockedWriter,
    message_id: i64,
) -> Result<MsgData, ApiError> {
    let value: Value = writer
        .call(
            ctx,
            "message.get",
            json!({"channel_id": channel_id(ctx)?, "message_id": message_id.to_string()}),
        )
        .await?;
    let resources = writer.resources();
    let channel = value.get("channel").unwrap_or(&Value::Null);
    let user = value.get("user").unwrap_or(&Value::Null);
    let member = value.get("member").unwrap_or(&Value::Null);
    let id = value.get("id").and_then(value_id).unwrap_or(message_id);
    Ok(MsgData {
        time: value
            .get("created_at")
            .and_then(Value::as_i64)
            .unwrap_or_default()
            / 1000,
        message_type: if channel
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| id.starts_with("private:"))
        {
            "private".to_string()
        } else {
            "group".to_string()
        },
        message_id: id,
        real_id: id,
        sender: SenderInfo {
            nickname: optional_string(user.get("name")),
            card: optional_string(member.get("nick")).or_else(|| optional_string(user.get("nick"))),
            other: Default::default(),
        },
        message: message::from_content_with(
            value.get("content").and_then(Value::as_str).unwrap_or(""),
            &resources,
        ),
    })
}

#[derive(Debug, Deserialize)]
pub struct ForwardMsgData {
    pub message: Message,
}

pub async fn get_forward_msg(
    ctx: &Context,
    writer: LockedWriter,
    id: String,
) -> Result<ForwardMsgData, ApiError> {
    let value: Value = writer
        .call(ctx, "internal/get_forward", json!({"id": id}))
        .await?;
    let resources = writer.resources();
    let mut chain = Message::new();
    if let Some(items) = value.get("data").and_then(Value::as_array) {
        for item in items {
            let user = item.get("user").unwrap_or(&Value::Null);
            let content = message::from_content_with(
                item.get("content").and_then(Value::as_str).unwrap_or(""),
                &resources,
            );
            chain = chain.node_custom(
                item.get("user")
                    .and_then(|user| user.get("id"))
                    .and_then(value_id)
                    .unwrap_or_default(),
                user.get("name").and_then(Value::as_str).unwrap_or(""),
                content,
            );
        }
    }
    Ok(ForwardMsgData { message: chain })
}

pub async fn send_like(
    ctx: &Context,
    writer: LockedWriter,
    user_id: i64,
    times: i32,
) -> Result<(), ApiError> {
    let _: Value = writer
        .call(
            ctx,
            "internal/like",
            json!({"user_id": user_id.to_string(), "times": times}),
        )
        .await?;
    Ok(())
}

pub async fn set_group_special_title(
    ctx: &Context,
    writer: LockedWriter,
    group_id: i64,
    user_id: i64,
    special_title: String,
    _duration: i64,
) -> Result<(), ApiError> {
    let _: Value = writer
        .call(
            ctx,
            "internal/special_title",
            json!({
                "guild_id": group_id.to_string(),
                "user_id": user_id.to_string(),
                "title": special_title,
            }),
        )
        .await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct GroupMemberInfo {
    pub group_id: i64,
    pub user_id: i64,
    pub nickname: String,
    pub card: String,
    pub sex: String,
    pub age: i32,
    pub area: String,
    pub join_time: i64,
    pub last_sent_time: i64,
    pub level: String,
    pub role: String,
    pub unfriendly: bool,
    pub title: String,
    pub title_expire_time: i64,
    pub card_changeable: bool,
}

pub async fn get_group_member_info(
    ctx: &Context,
    writer: LockedWriter,
    group_id: i64,
    user_id: i64,
    _no_cache: bool,
) -> Result<GroupMemberInfo, ApiError> {
    let value: Value = writer
        .call(
            ctx,
            "guild.member.get",
            json!({"guild_id": group_id.to_string(), "user_id": user_id.to_string()}),
        )
        .await?;
    let user = value.get("user").unwrap_or(&Value::Null);
    let role = value
        .get("roles")
        .and_then(Value::as_array)
        .and_then(|roles| roles.first())
        .and_then(|role| role.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("member")
        .to_string();
    Ok(GroupMemberInfo {
        group_id,
        user_id,
        nickname: user
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        card: value
            .get("nick")
            .or_else(|| value.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        sex: "unknown".to_string(),
        age: 0,
        area: String::new(),
        join_time: value
            .get("joined_at")
            .and_then(Value::as_i64)
            .unwrap_or_default()
            / 1000,
        last_sent_time: value
            .get("last_sent_at")
            .and_then(Value::as_i64)
            .unwrap_or_default()
            / 1000,
        level: value
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        role,
        unfriendly: false,
        title: value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        title_expire_time: value
            .get("title_expire_time")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        card_changeable: true,
    })
}

#[derive(Debug, Deserialize)]
pub struct LoginInfo {
    pub user_id: i64,
    pub nickname: String,
}

pub async fn get_login_info(ctx: &Context, writer: LockedWriter) -> Result<LoginInfo, ApiError> {
    let value: Value = writer.call(ctx, "login.get", json!({})).await?;
    let user = value.get("user").unwrap_or(&Value::Null);
    Ok(LoginInfo {
        user_id: user.get("id").and_then(value_id).unwrap_or_default(),
        nickname: user
            .get("name")
            .or_else(|| user.get("nick"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

#[derive(Debug, Deserialize)]
pub struct GroupInfo {
    pub group_id: i64,
    pub group_name: String,
    pub member_count: Option<i32>,
    pub max_member_count: Option<i32>,
}

/// `guild.list` 是标准分页列表：跟着 `next` 令牌翻到底，否则群多时会漏群。
pub async fn get_group_list(
    ctx: &Context,
    writer: LockedWriter,
    _no_cache: bool,
) -> Result<Vec<GroupInfo>, ApiError> {
    const MAX_PAGES: usize = 64;
    let mut out = Vec::new();
    let mut next: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let params = match &next {
            Some(cursor) => json!({"next": cursor}),
            None => json!({}),
        };
        let value: Value = writer.call(ctx, "guild.list", params).await?;
        out.extend(
            value
                .get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|guild| {
                    Some(GroupInfo {
                        group_id: guild.get("id").and_then(value_id)?,
                        group_name: guild
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        member_count: None,
                        max_member_count: None,
                    })
                }),
        );
        next = value
            .get("next")
            .and_then(Value::as_str)
            .filter(|cursor| !cursor.is_empty())
            .map(str::to_owned);
        if next.is_none() {
            break;
        }
    }
    Ok(out)
}

pub async fn upload_file(
    ctx: &Context,
    writer: LockedWriter,
    group_id: Option<i64>,
    user_id: Option<i64>,
    file: &str,
    name: &str,
) -> Result<(), ApiError> {
    let data = tokio::fs::read(file).await?;
    let mime = if name.ends_with(".json") {
        "application/json"
    } else if name.ends_with(".txt") || name.ends_with(".md") {
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    };
    let uploaded = writer.upload(ctx, data, name, mime).await?;
    let resource = uploaded
        .get("file")
        .and_then(Value::as_str)
        .ok_or("Satori upload.create 未返回 file 资源")?;
    super::send_msg(
        ctx,
        writer,
        group_id,
        user_id,
        Message::new().file(resource, Some(name)),
    )
    .await
}

pub async fn set_msg_emoji_like(
    ctx: &Context,
    writer: LockedWriter,
    message_id: i64,
    emoji_id: i64,
    set: bool,
) -> Result<(), ApiError> {
    let method = if set {
        "reaction.create"
    } else {
        "reaction.delete"
    };
    let _: Value = writer
        .call(
            ctx,
            method,
            json!({
                "channel_id": channel_id(ctx)?,
                "message_id": message_id.to_string(),
                "emoji_id": emoji_id.to_string(),
            }),
        )
        .await?;
    Ok(())
}

fn channel_id(ctx: &Context) -> Result<String, ApiError> {
    let event = match &ctx.event {
        EventType::Satori(event) => Some(event),
        EventType::BeforeSend(packet) => packet.original_event.as_ref(),
        EventType::Init => None,
    }
    .ok_or("当前上下文没有 Satori 频道")?;
    let group_id = event
        .get_i64("group_id")
        .or_else(|| event.get_u64("group_id").map(|value| value as i64))
        .unwrap_or_default();
    if group_id != 0 {
        return Ok(group_id.to_string());
    }
    let user_id = event
        .get_i64("user_id")
        .or_else(|| event.get_u64("user_id").map(|value| value as i64))
        .unwrap_or_default();
    if user_id != 0 {
        Ok(format!("private:{user_id}"))
    } else {
        Err("当前上下文缺少 Satori channel_id".into())
    }
}

fn value_id(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn optional_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}
