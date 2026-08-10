use crate::adapters::onebot::LockedWriter;
use crate::adapters::onebot::api;
use crate::command::match_command;
use crate::config::build_config;
use crate::event::{Context, EventType};
use crate::plugins::PluginError;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsScalar};
use toml::Value;

#[derive(Serialize, Deserialize)]
struct Config {
    enabled: bool,
}

pub fn default_config() -> Value {
    build_config(Config { enabled: true })
}

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        if let Some(cmd) = match_command(&ctx, "我要头衔") {
            // 1. 确认是群消息
            let msg = match ctx.as_message() {
                Some(m) => m,
                None => return Ok(Some(ctx)),
            };

            let group_id = match msg.group_id() {
                Some(gid) => gid,
                None => return Ok(Some(ctx)),
            };

            let user_id = msg.user_id();

            // 2. 获取 Bot 自身的 ID (self_id)
            let self_id = if let EventType::Onebot(ev) = &ctx.event {
                ev.get_i64("self_id")
                    .or_else(|| ev.get_u64("self_id").map(|v| v as i64))
                    .unwrap_or(0)
            } else {
                0
            };

            // 3. 检查 Bot 是否为群主
            // 只有群主才有权限设置群头衔 (OneBot 标准行为)
            let bot_info =
                match api::get_group_member_info(&ctx, writer.clone(), group_id, self_id, true)
                    .await
                {
                    Ok(info) => info,
                    Err(e) => {
                        error!(
                            target: "Plugin/Title",
                            "[Group({})] 获取 Bot 成员信息失败: {}",
                            group_id, e
                        );
                        return Ok(Some(ctx));
                    }
                };

            if bot_info.role != "owner" {
                // 如果不是群主，忽略指令（或者可以回复提示）
                warn!(
                    target: "Plugin/Title",
                    "[Group({})] Bot 不是群主，无法设置头衔",
                    group_id
                );
                return Ok(Some(ctx));
            }

            // 4. 拼接头衔内容
            let mut title = String::new();
            for seg in cmd.args {
                // 提取文本段内容
                if let Some(text) = seg.get("data").and_then(|d| d.get_str("text")) {
                    title.push_str(text);
                }
            }
            let title = title.trim();

            // 5. 设置头衔
            if let Err(e) = api::set_group_special_title(
                &ctx,
                writer.clone(),
                group_id,
                user_id,
                title.to_string(),
                -1,
            )
            .await
            {
                error!(
                    target: "Plugin/Title",
                    "[Group({})] 设置头衔失败: {}",
                    group_id, e
                );
            } else {
                return Ok(None);
            }
        }

        Ok(Some(ctx))
    })
}
