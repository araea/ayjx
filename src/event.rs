#![allow(dead_code)]

use crate::config::AppConfig;
use crate::matcher::Matcher;
use crate::scheduler::Scheduler;
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use simd_json::OwnedValue;
use simd_json::base::ValueAsScalar;
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsScalar};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

pub type Event = OwnedValue;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LoginUser {
    pub id: String,
    pub name: Option<String>,
    pub nick: Option<String>,
    pub avatar: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BotStatus {
    pub adapter: String,
    pub platform: String,
    pub login_user: LoginUser,
}

/// 统一的上下文，包含事件数据、可变配置和任务调度器
/// 注意：event 字段直接持有 EventType，支持在插件链中移交所有权从而实现修改。
/// Context 实现了 Clone（因为 EventType 包含的 simd_json::OwnedValue 实现了 Clone），
/// 但在插件流水线中通常通过 Move 传递，避免了 Deep Copy。
///
/// 优化：`config_path` 与 `bot` 改为 Arc 共享，避免每事件克隆字符串与 BotStatus 内部多个 String。
#[derive(Clone)]
pub struct Context {
    pub event: EventType, // 直接持有，不再使用 Arc
    pub config: Arc<RwLock<AppConfig>>,
    pub config_save_lock: Arc<AsyncMutex<()>>,
    pub db: DatabaseConnection,
    pub scheduler: Arc<Scheduler>,
    pub matcher: Arc<Matcher>,
    pub config_path: Arc<str>,
    pub bot: Arc<BotStatus>,
}

impl Context {
    /// 尝试将当前事件视为规范化后的 Satori 消息事件
    pub fn as_message(&self) -> Option<MessageEvent<'_>> {
        if let EventType::Satori(event) = &self.event {
            let view = GeneralEventView(event);
            if view.post_type() == Some("message") {
                return Some(MessageEvent(event));
            }
        }
        None
    }

    /// 获取规范化事件的 post_type
    pub fn post_type(&self) -> Option<&str> {
        if let EventType::Satori(event) = &self.event {
            GeneralEventView(event).post_type()
        } else {
            None
        }
    }

    /// 等待特定条件的用户输入 (交互式操作)
    pub async fn wait_input(
        &self,
        group_id: Option<i64>,
        user_id: Option<i64>,
        timeout: Duration,
    ) -> Option<Event> {
        self.matcher.wait(group_id, user_id, timeout).await
    }
}

// ================== 事件封装工具 ==================

/// 通用事件视图，用于快速访问基础字段
pub struct GeneralEventView<'a>(&'a Event);

impl<'a> GeneralEventView<'a> {
    /// 获取 post_type，返回的引用生命周期绑定到原始 Event ('a)
    pub fn post_type(&self) -> Option<&'a str> {
        self.0.get_str("post_type")
    }
}

/// 消息事件封装，提供便捷的强类型访问
pub struct MessageEvent<'a>(pub &'a Event);

impl<'a> MessageEvent<'a> {
    /// 获取群号 (如果是群消息)
    pub fn group_id(&self) -> Option<i64> {
        self.0
            .get_i64("group_id")
            .or_else(|| self.0.get_u64("group_id").map(|v| v as i64))
    }

    /// 获取用户 ID
    pub fn user_id(&self) -> i64 {
        self.0
            .get_i64("user_id")
            .or_else(|| self.0.get_u64("user_id").map(|v| v as i64))
            .unwrap_or(0)
    }

    /// 获取消息 ID
    pub fn message_id(&self) -> i64 {
        self.0
            .get_i64("message_id")
            .or_else(|| self.0.get_u64("message_id").map(|v| v as i64))
            .unwrap_or(0)
    }

    /// 获取纯文本内容 (raw_message)
    pub fn text(&self) -> &'a str {
        self.0.get_str("raw_message").unwrap_or("")
    }

    /// 是否为群消息
    pub fn is_group(&self) -> bool {
        self.0.get_str("message_type") == Some("group")
    }

    /// 获取发送者昵称
    pub fn sender_nickname(&self) -> Option<&'a str> {
        self.0.get("sender").and_then(|s| s.get_str("nickname"))
    }

    /// 获取发送者群名片 (如果为空则返回 None)
    pub fn sender_card(&self) -> Option<&'a str> {
        self.0
            .get("sender")
            .and_then(|s| s.get_str("card"))
            .filter(|s| !s.is_empty())
    }

    /// 获取发送者显示名称 (优先名片，其次昵称)
    pub fn sender_name(&self) -> &'a str {
        self.sender_card()
            .or_else(|| self.sender_nickname())
            .unwrap_or("Unknown")
    }

    /// 获取发送者角色 (owner, admin, member)
    pub fn sender_role(&self) -> Option<&'a str> {
        self.0.get("sender").and_then(|s| s.get_str("role"))
    }

    /// 是否为 satori-qq 为 QQ 客户端手发消息补发的自发事件。
    ///
    /// 这类事件仍然允许进入插件流水线（便于机器人账号本人调用指令），但不能对其
    /// 执行 reaction API；satori-qq 使用的虚拟 `qq-client:*` 作者会让表情落错目标。
    pub fn is_manual_self(&self) -> bool {
        self.0.get_bool("manual_self").unwrap_or(false)
            || self
                .0
                .get("_satori")
                .and_then(|value| value.get("satori_qq"))
                .and_then(|value| value.get("manual_self"))
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
    }
}

// ================== 基础结构定义 ==================

/// 事件类型
#[derive(Debug, Clone)]
pub enum EventType {
    /// 来自 Satori 的规范化事件
    Satori(Event),
    /// 插件准备发送消息前的拦截事件
    BeforeSend(SendPacket),
    /// 系统初始化事件 (用于插件 on_init 生命周期)
    Init,
}

/// 发送包结构，用于在 BeforeSend 中传递
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SendPacket {
    pub action: String,
    pub params: OwnedValue,
    /// 原始触发事件（不参与序列化发送给 Bot）
    #[serde(skip)]
    pub original_event: Option<Event>,
    /// `message.create` 成功后由适配器写入实际消息 ID，供需要建立引用关系的
    /// 调用方读取。使用共享容器是因为发送包会经过多个插件并被克隆。
    #[serde(skip)]
    pub receipt_message_ids: Arc<Mutex<Vec<String>>>,
}

impl SendPacket {
    /// 尝试从发送包中提取目标群号
    pub fn group_id(&self) -> Option<i64> {
        self.params
            .get_i64("group_id")
            .or_else(|| self.params.get_u64("group_id").map(|v| v as i64))
    }

    /// 尝试从发送包中提取目标用户号（私聊）
    pub fn user_id(&self) -> Option<i64> {
        self.params
            .get_i64("user_id")
            .or_else(|| self.params.get_u64("user_id").map(|v| v as i64))
    }

    /// 获取 message 字段的 Value
    pub fn message(&self) -> Option<&OwnedValue> {
        self.params.get("message")
    }

    /// 获取消息类型字符串，返回 Option，若不存在则返回 None
    pub fn message_type(&self) -> Option<&str> {
        self.params.get_str("message_type")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message_event(value: serde_json::Value) -> Event {
        simd_json::serde::to_owned_value(value).unwrap()
    }

    #[test]
    fn recognizes_normalized_and_raw_manual_self_markers() {
        let normalized = message_event(json!({"manual_self": true}));
        assert!(MessageEvent(&normalized).is_manual_self());

        let raw = message_event(json!({
            "_satori": {"satori_qq": {"manual_self": true}}
        }));
        assert!(MessageEvent(&raw).is_manual_self());

        let ordinary = message_event(json!({"manual_self": false}));
        assert!(!MessageEvent(&ordinary).is_manual_self());
    }
}
