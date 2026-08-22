#![allow(dead_code)]

// 引用同模块下的工具函数
use super::{LockedWriter, send_frame_raw};
use crate::event::Context;
use crate::message::Message;
use serde::{Deserialize, Serialize};
use simd_json::OwnedValue;
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsScalar};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub type ApiError = Box<dyn std::error::Error + Send + Sync>;

static ECHO_COUNTER: AtomicU64 = AtomicU64::new(1);

/// 生成一个全局唯一的请求标记，用于把 OneBot 的响应对回到发起方
pub fn next_echo() -> String {
    let count = ECHO_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("api-req-{}", count)
}

#[derive(Serialize)]
struct ApiRequest<T> {
    action: String,
    params: T,
    echo: String,
}

/// 通用 API 调用函数
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
    let echo = next_echo();
    let req = ApiRequest {
        action: action.to_string(),
        params,
        echo: echo.clone(),
    };

    let json_str = simd_json::to_string(&req)?;

    // 注册监听
    // 默认超时 60 秒 (上传文件可能较慢)
    let wait_future = ctx.matcher.wait_resp(echo, Duration::from_secs(60));

    // 发送请求
    send_frame_raw(writer, json_str).await?;

    // 等待响应
    let resp_event = wait_future.await.ok_or("API 请求超时")?;

    // 解析响应
    // 响应格式: { status, retcode, data, echo }
    let retcode = resp_event
        .get_i64("retcode")
        .or_else(|| resp_event.get_u64("retcode").map(|v| v as i64))
        .unwrap_or(-1);

    if retcode != 0 {
        // 尝试获取 msg 或 wording 错误信息
        let msg = resp_event.get_str("msg").unwrap_or("Unknown Error");
        return Err(format!("API 调用失败 (retcode={}): {}", retcode, msg).into());
    }

    // 提取 data 字段
    let data_val = resp_event
        .get("data")
        .cloned()
        .unwrap_or(OwnedValue::from(()));

    // 反序列化 data
    let data: R = simd_json::serde::from_owned_value(data_val)?;

    Ok(data)
}

/// 不等待响应的 API 调用函数 (Fire-and-forget)
/// 用于 send_like, delete_msg 等无需返回值的操作，提高并发性能
pub async fn call_action_no_wait<P>(
    _ctx: &Context,
    writer: LockedWriter,
    action: &str,
    params: P,
) -> Result<(), ApiError>
where
    P: Serialize,
{
    let echo = next_echo();
    let req = ApiRequest {
        action: action.to_string(),
        params,
        echo,
    };

    let json_str = simd_json::to_string(&req)?;

    // 直接发送请求，不等待 WS 返回
    send_frame_raw(writer, json_str).await?;

    Ok(())
}

// ================= API 定义 =================

// --- delete_msg ---

#[derive(Serialize)]
struct DeleteMsgParams {
    message_id: i32,
}

pub async fn delete_msg(
    ctx: &Context,
    writer: LockedWriter,
    message_id: i32,
) -> Result<(), ApiError> {
    call_action_no_wait(ctx, writer, "delete_msg", DeleteMsgParams { message_id }).await
}

// --- get_msg ---

#[derive(Serialize)]
struct GetMsgParams {
    message_id: i32,
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
    pub time: i32,
    pub message_type: String,
    pub message_id: i32,
    pub real_id: i32,
    pub sender: SenderInfo,
    pub message: Message,
}

pub async fn get_msg(
    ctx: &Context,
    writer: LockedWriter,
    message_id: i32,
) -> Result<MsgData, ApiError> {
    call_action(ctx, writer, "get_msg", GetMsgParams { message_id }).await
}

// --- get_forward_msg ---

#[derive(Serialize)]
struct GetForwardMsgParams {
    id: String,
}

#[derive(Debug, Deserialize)]
pub struct ForwardMsgData {
    pub message: Message, // 这里的 Message 内部 segment 全是 node
}

pub async fn get_forward_msg(
    ctx: &Context,
    writer: LockedWriter,
    id: String,
) -> Result<ForwardMsgData, ApiError> {
    call_action(ctx, writer, "get_forward_msg", GetForwardMsgParams { id }).await
}

// --- send_like ---

#[derive(Serialize)]
struct SendLikeParams {
    user_id: i64,
    times: i32,
}

pub async fn send_like(
    ctx: &Context,
    writer: LockedWriter,
    user_id: i64,
    times: i32,
) -> Result<(), ApiError> {
    call_action_no_wait(ctx, writer, "send_like", SendLikeParams { user_id, times }).await
}

#[derive(Serialize)]
struct SetGroupSpecialTitleParams {
    group_id: i64,
    user_id: i64,
    special_title: String,
    duration: i64,
}

pub async fn set_group_special_title(
    ctx: &Context,
    writer: LockedWriter,
    group_id: i64,
    user_id: i64,
    special_title: String,
    duration: i64,
) -> Result<(), ApiError> {
    let params = SetGroupSpecialTitleParams {
        group_id,
        user_id,
        special_title,
        duration,
    };
    call_action_no_wait(ctx, writer, "set_group_special_title", params).await
}

// --- get_group_member_info ---

#[derive(Serialize)]
struct GetGroupMemberInfoParams {
    group_id: i64,
    user_id: i64,
    #[serde(default)]
    no_cache: bool,
}

#[derive(Debug, Deserialize)]
pub struct GroupMemberInfo {
    pub group_id: i64,
    pub user_id: i64,
    pub nickname: String,
    pub card: String,
    pub sex: String, // "male", "female", or "unknown"
    pub age: i32,
    pub area: String,
    pub join_time: i32,
    pub last_sent_time: i32,
    pub level: String,
    pub role: String, // "owner", "admin", or "member"
    pub unfriendly: bool,
    pub title: String,
    pub title_expire_time: i32,
    pub card_changeable: bool,
}

pub async fn get_group_member_info(
    ctx: &Context,
    writer: LockedWriter,
    group_id: i64,
    user_id: i64,
    no_cache: bool,
) -> Result<GroupMemberInfo, ApiError> {
    let params = GetGroupMemberInfoParams {
        group_id,
        user_id,
        no_cache,
    };
    call_action(ctx, writer, "get_group_member_info", params).await
}

// --- get_login_info ---

#[derive(Serialize)]
struct GetLoginInfoParams {}

#[derive(Debug, Deserialize)]
pub struct LoginInfo {
    pub user_id: i64,
    pub nickname: String,
}

pub async fn get_login_info(ctx: &Context, writer: LockedWriter) -> Result<LoginInfo, ApiError> {
    call_action(ctx, writer, "get_login_info", GetLoginInfoParams {}).await
}

// --- get_group_list ---

#[derive(Serialize)]
struct GetGroupListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    no_cache: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct GroupInfo {
    pub group_id: i64,
    pub group_name: String,
    pub member_count: Option<i32>,
    pub max_member_count: Option<i32>,
}

pub async fn get_group_list(
    ctx: &Context,
    writer: LockedWriter,
    no_cache: bool,
) -> Result<Vec<GroupInfo>, ApiError> {
    call_action(
        ctx,
        writer,
        "get_group_list",
        GetGroupListParams {
            no_cache: Some(no_cache),
        },
    )
    .await
}

// --- upload_file (group/private) ---

#[derive(Serialize)]
struct UploadFileParams<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    group_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_id: Option<i64>,
    file: &'a str,
    name: &'a str,
}

pub async fn upload_file(
    ctx: &Context,
    writer: LockedWriter,
    group_id: Option<i64>,
    user_id: Option<i64>,
    file: &str,
    name: &str,
) -> Result<(), ApiError> {
    let action = if group_id.is_some() {
        "upload_group_file"
    } else {
        "upload_private_file"
    };

    let params = UploadFileParams {
        group_id,
        user_id,
        file,
        name,
    };

    // 文件上传通常无需关心返回的 data 内容，只要 retcode=0 即可
    call_action::<_, simd_json::OwnedValue>(ctx, writer, action, params).await?;
    Ok(())
}

// --- set_msg_emoji_like ---

#[derive(Serialize)]
struct SetMsgEmojiLikeParams {
    message_id: i32,
    emoji_id: i64,
    set: bool,
}

pub async fn set_msg_emoji_like(
    ctx: &Context,
    writer: LockedWriter,
    message_id: i32,
    emoji_id: i64,
    set: bool,
) -> Result<(), ApiError> {
    let params = SetMsgEmojiLikeParams {
        message_id,
        emoji_id,
        set,
    };
    call_action_no_wait(ctx, writer, "set_msg_emoji_like", params).await
}
