use crate::adapters::satori::LockedWriter;
use crate::config::build_config;
use crate::event::Context;
use crate::plugins::{PluginError, get_data_dir};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsArray, ValueObjectAccessAsScalar};

use std::sync::Arc;
use toml::Value;

pub(crate) mod ambient;
pub mod data;
pub mod logic;
pub mod mj;
pub mod parser;
mod pi_agent;
pub mod render;
pub mod types;
pub mod utils;

use data::MANAGER;

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct OaiConfig {
    enabled: bool,
    /// 本机 Pi CLI 可执行文件；模型与工具沿用 Pi 配置。
    pi_command: String,
    /// 单次回复的总时间预算。
    request_timeout_seconds: u64,
    /// pi 的事件流静默多少秒算卡死（卡住且尚未出正文时自动重来一次）；置 0 关闭。
    /// pi 自身不给模型请求设超时，中转站抽风时它会一直等到总预算耗尽。
    pi_stall_seconds: u64,
    /// 短回复直接以文本发送而不渲染图片的字符上限；置 0 表示始终渲染图片。
    /// 一句话的答复走文本既快又便于复制。
    plain_text_max_chars: usize,
    /// 在回复卡片页脚展示模型、耗时与工具调用轨迹。
    show_trace_footer: bool,
    /// 模型列表过滤：中转站返回的上千个 id 里只留下当下值得用的那些。
    /// 站点上新或下架时改这里即可，`/%` 会按新规则重新拉取。
    pub(crate) model_filter: utils::ModelFilterConfig,
    /// 群聊搭话：以固定人格作为群成员之一存在，绝大多数时候沉默。
    ambient: ambient::AmbientConfig,
}

impl Default for OaiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            pi_command: "pi".to_string(),
            request_timeout_seconds: 300,
            pi_stall_seconds: 90,
            plain_text_max_chars: 120,
            show_trace_footer: true,
            model_filter: utils::ModelFilterConfig::default(),
            ambient: ambient::AmbientConfig::default(),
        }
    }
}

impl OaiConfig {
    pub(crate) fn request_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.request_timeout_seconds.clamp(30, 1_800))
    }

    /// pi 静默多久算卡死；`None` 表示不看。
    pub(crate) fn pi_stall(&self) -> Option<std::time::Duration> {
        (self.pi_stall_seconds > 0)
            .then(|| std::time::Duration::from_secs(self.pi_stall_seconds.max(20)))
    }

    pub(crate) fn plain_text_max_chars(&self) -> usize {
        self.plain_text_max_chars
    }

    pub(crate) fn show_trace_footer(&self) -> bool {
        self.show_trace_footer
    }
}

pub fn default_config() -> Value {
    build_config(OaiConfig::default())
}

pub fn init(ctx: Context) -> BoxFuture<'static, Result<(), PluginError>> {
    Box::pin(async move {
        let dir = get_data_dir("oai").await?;
        if let Err(error) = ambient::init(&dir).await {
            warn!(target: "Plugin/OAI", "群聊搭话资源初始化失败: {}", error);
        }
        let mgr = Arc::new(data::Manager::new(dir));

        // 尝试预加载模型列表
        let filter = crate::plugins::get_config_or_default::<OaiConfig>(&ctx, "oai").model_filter;
        let mgr_clone = mgr.clone();
        tokio::spawn(async move {
            if let Err(e) = mgr_clone.fetch_models(&filter).await {
                warn!(target: "Plugin/OAI", "初始化获取模型列表失败: {}", e);
            } else {
                info!(target: "Plugin/OAI", "初始化获取模型列表成功");
            }
        });

        if MANAGER.set(mgr).is_err() {
            warn!(target: "Plugin/OAI", "Manager 已经被初始化");
        }
        Ok(())
    })
}

// 提取纯文本内容，自动忽略头部的 At 和 Reply 消息段
fn extract_clean_text(ctx: &Context) -> Option<String> {
    let event = match &ctx.event {
        crate::event::EventType::Satori(e) => e,
        _ => return None,
    };

    if event.get_str("post_type")? != "message" {
        return None;
    }

    let arr = event.get_array("message")?;
    let mut text_acc = String::new();
    let mut found_start = false;

    for seg in arr {
        let type_ = seg.get_str("type")?;

        if !found_start {
            // 跳过头部的 at 和 reply
            if type_ == "at" || type_ == "reply" {
                continue;
            }
            // 如果是文本，检查是否为空白
            if type_ == "text" {
                let data = seg.get("data")?;
                let t = data.get_str("text").unwrap_or("");
                let trimmed = t.trim_start();
                if trimmed.is_empty() {
                    continue;
                }
                // 找到有效文本起点
                found_start = true;
                text_acc.push_str(trimmed);
            } else {
                // 遇到非文本（如图片），视为内容开始，停止跳过
                found_start = true;
            }
        } else if type_ == "text" {
            let t = seg.get("data")?.get_str("text").unwrap_or("");
            text_acc.push_str(t);
        }
    }

    if text_acc.is_empty() {
        None
    } else {
        Some(text_acc)
    }
}

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        // 确保 Manager 已初始化
        let mgr = match MANAGER.get() {
            Some(m) => m,
            None => {
                error!(target: "Plugin/OAI", "插件尚未初始化");
                return Ok(Some(ctx));
            }
        };

        // MJ 的放大交互只依赖被引用的机器人消息，不要求再次写房间名。
        // 因此必须先于普通文本/指令解析处理，也能兼容只发送数字的回复。
        if mj::try_handle_upscale_reply(&ctx, &writer, mgr).await {
            return Ok(None);
        }

        // 获取纯文本内容。没有文字的消息（纯图片、纯表情）不可能是指令，
        // 但仍是群聊上下文的一部分，要让搭话观察者看到。
        let raw_text = match extract_clean_text(&ctx) {
            Some(t) => t,
            None => {
                ambient::observe(&ctx, &writer, mgr).await;
                return Ok(Some(ctx));
            }
        };

        // 1. 全局指令解析
        if let Some(cmd) = parser::parse_global(&raw_text, &crate::command::get_prefixes(&ctx)) {
            logic::execute(cmd, String::new(), vec![], &ctx, &writer, mgr).await;
            return Ok(None); // 指令被消费，不再传递
        }

        // 2. 创建指令解析
        if let Some((name, desc, model, prompt)) = parser::parse_create(&raw_text) {
            logic::handle_create(&name, &desc, &model, &prompt, &ctx, &writer, mgr).await;
            return Ok(None);
        }

        // 3. 删除指令解析
        let agents = mgr.agent_names().await;
        if let Some(name) = parser::parse_delete_agent(&raw_text, &agents) {
            let cmd = parser::Command::new(&name, parser::Action::Delete);
            logic::execute(cmd, String::new(), vec![], &ctx, &writer, mgr).await;
            return Ok(None);
        }

        // 4. 智能体指令/对话解析
        if let Some(cmd) = parser::parse_agent_cmd(&raw_text, &agents) {
            let (quote, imgs) = utils::get_full_content(&ctx, &writer, Some(&cmd.agent)).await;

            // 拼接提示词：引用 + 用户输入参数
            let prompt = if matches!(
                cmd.action,
                parser::Action::Chat | parser::Action::Regenerate
            ) {
                format!("{}{}", quote, cmd.args).trim().to_string()
            } else {
                cmd.args.clone()
            };

            logic::execute(cmd, prompt, imgs, &ctx, &writer, mgr).await;
            return Ok(None);
        }

        // 不属于任何指令的普通群聊：交给搭话观察者，事件继续向后传递。
        ambient::observe(&ctx, &writer, mgr).await;

        Ok(Some(ctx))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_command_defaults_and_legacy_config_remain_loadable() {
        let config: OaiConfig =
            toml::from_str("harness_rooms = ['pi']\nshell_timeout_seconds = 300").unwrap();
        assert_eq!(config.pi_command, "pi");
        let config: OaiConfig = toml::from_str("pi_command = '/custom/pi'").unwrap();
        assert_eq!(config.pi_command, "/custom/pi");
    }
}

/// Validate control edits against the plugin's actual configuration type.
pub fn validate_config(value: &toml::Value) -> Result<(), String> {
    <OaiConfig as serde::Deserialize>::deserialize(value.clone())
        .map(|_| ())
        .map_err(|_| "配置类型不匹配（请检查数组元素、字段类型及整数范围）".to_string())
}
