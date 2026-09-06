use crate::adapters::satori::LockedWriter;
use crate::config::build_config;
use crate::event::Context;
use crate::plugins::{PluginError, get_data_dir};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsArray, ValueObjectAccessAsScalar};

use std::sync::Arc;
use toml::Value;

pub mod agent;
pub mod data;
pub mod harness;
pub mod logic;
pub mod mj;
pub mod parser;
pub mod render;
pub mod search;
pub mod types;
pub mod utils;
pub mod webfetch;

use data::MANAGER;

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct OaiConfig {
    enabled: bool,
    /// 仅这些公有房间启用本机工具；私有历史模式不会获得工具权限。
    harness_rooms: Vec<String>,
    shell_timeout_seconds: u64,
    shell_max_output_bytes: usize,
    web_search_results: usize,
    /// 单次 `web_fetch` 返回给模型的正文字符上限。
    web_fetch_max_chars: usize,
    web_fetch_timeout_seconds: u64,
    /// 优先使用服务端托管的 `web_search` 工具。移动网络下本机抓取公共搜索引擎
    /// 几乎必被反爬拦截，托管检索才是稳定选项；端点不支持时会自动回落到本机搜索。
    hosted_web_search: bool,
    /// 工具房间的推理档位：`minimal` / `low` / `medium` / `high`，留空则用模型默认。
    /// 群聊问答用 `low` 通常就够，等待时间明显更短。
    reasoning_effort: String,
    /// 单次回复的总时间预算。
    request_timeout_seconds: u64,
    /// 超过这个秒数还没出结果就先发一条进度提示；置 0 关闭。
    progress_notice_seconds: u64,
    /// 短回复直接以文本发送而不渲染图片的字符上限；置 0 表示始终渲染图片。
    /// 一句话的答复走文本既快又便于复制。
    plain_text_max_chars: usize,
    /// 在回复卡片页脚展示模型、耗时与工具调用轨迹。
    show_trace_footer: bool,
}

impl Default for OaiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            harness_rooms: vec!["pi".to_string()],
            shell_timeout_seconds: 300,
            shell_max_output_bytes: 64 * 1024,
            web_search_results: 8,
            web_fetch_max_chars: 12_000,
            web_fetch_timeout_seconds: 25,
            hosted_web_search: true,
            reasoning_effort: "low".to_string(),
            request_timeout_seconds: 300,
            progress_notice_seconds: 30,
            plain_text_max_chars: 120,
            show_trace_footer: true,
        }
    }
}

impl OaiConfig {
    pub(crate) fn harness_for(&self, room: &str, private: bool) -> Option<harness::HarnessConfig> {
        if private
            || !self
                .harness_rooms
                .iter()
                .any(|configured| configured.eq_ignore_ascii_case(room))
        {
            return None;
        }
        Some(harness::HarnessConfig {
            shell_timeout_seconds: self.shell_timeout_seconds.clamp(1, 3_600),
            shell_max_output_bytes: self.shell_max_output_bytes.clamp(1_024, 1024 * 1024),
            web_search_results: self.web_search_results.clamp(1, 20),
            web_fetch_max_chars: self.web_fetch_max_chars.clamp(500, 40_000),
            web_fetch_timeout_seconds: self.web_fetch_timeout_seconds.clamp(5, 120),
            hosted_web_search: self.hosted_web_search,
        })
    }

    /// 归一化后的推理档位；无法识别的取值当作未设置。
    pub(crate) fn effort(&self) -> Option<String> {
        let effort = self.reasoning_effort.trim().to_ascii_lowercase();
        matches!(effort.as_str(), "minimal" | "low" | "medium" | "high").then_some(effort)
    }

    pub(crate) fn request_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.request_timeout_seconds.clamp(30, 1_800))
    }

    /// 进度提示的触发时刻；`None` 表示不提示。
    pub(crate) fn progress_notice(&self) -> Option<std::time::Duration> {
        (self.progress_notice_seconds > 0)
            .then(|| std::time::Duration::from_secs(self.progress_notice_seconds.max(5)))
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

pub fn init(_ctx: Context) -> BoxFuture<'static, Result<(), PluginError>> {
    Box::pin(async move {
        let dir = get_data_dir("oai").await?;
        let mgr = Arc::new(data::Manager::new(dir));

        // 尝试预加载模型列表
        let mgr_clone = mgr.clone();
        tokio::spawn(async move {
            if let Err(e) = mgr_clone.fetch_models().await {
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

        // 获取纯文本内容
        let raw_text = match extract_clean_text(&ctx) {
            Some(t) => t,
            None => return Ok(Some(ctx)),
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

        Ok(Some(ctx))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_is_enabled_only_for_pi_public_room_by_default() {
        let config = OaiConfig::default();
        assert!(config.harness_for("pi", false).is_some());
        assert!(config.harness_for("PI", false).is_some());
        assert!(config.harness_for("pi", true).is_none());
        assert!(config.harness_for("other", false).is_none());
    }
}
