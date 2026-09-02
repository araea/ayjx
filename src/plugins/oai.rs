use crate::adapters::satori::LockedWriter;
use crate::config::build_config;
use crate::event::Context;
use crate::plugins::{PluginError, get_data_dir};
use futures_util::future::BoxFuture;
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsArray, ValueObjectAccessAsScalar};

use std::sync::Arc;
use toml::Value;

pub mod data;
pub mod logic;
pub mod parser;
pub mod types;
pub mod utils;

use data::MANAGER;

pub fn default_config() -> Value {
    build_config(serde_json::json!({
        "enabled": true
    }))
}

pub fn init(_ctx: Context) -> BoxFuture<'static, Result<(), PluginError>> {
    Box::pin(async move {
        let dir = get_data_dir("oai").await?;
        let mgr = Arc::new(data::Manager::new(dir));

        // 尝试预加载模型列表
        let mgr_clone = mgr.clone();
        tokio::spawn(async move {
            if let Err(e) = mgr_clone.fetch_models().await {
                warn!(target: "OAI", "初始化获取模型列表失败: {}", e);
            } else {
                info!(target: "OAI", "初始化获取模型列表成功");
            }
        });

        if MANAGER.set(mgr).is_err() {
            warn!(target: "OAI", "Manager 已经被初始化");
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
                error!(target: "OAI", "插件尚未初始化");
                return Ok(Some(ctx));
            }
        };

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
