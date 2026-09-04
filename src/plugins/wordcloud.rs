use crate::adapters::satori::{LockedWriter, send_msg};
use crate::command::strip_prefix;
use crate::db::queries::get_text_corpus;
use crate::db::utils::get_time_range;
use crate::event::Context;
use crate::message::Message;
use crate::plugins::{PluginError, get_config_or_default};
use futures_util::future::BoxFuture;
use regex::Regex;
use std::sync::OnceLock;

pub mod config;
pub mod image;
pub mod stopwords;

use config::WordCloudConfig;
pub use config::default_config;

static COMMAND_REGEX: OnceLock<Regex> = OnceLock::new();

fn get_regex() -> &'static Regex {
    COMMAND_REGEX.get_or_init(|| {
        Regex::new(
            r"^(本群|跨群|我的)(今日|昨日|本周|上周|近7天|近30天|本月|上月|今年|去年|总)词云$",
        )
        .unwrap()
    })
}

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        let msg = match ctx.as_message() {
            Some(m) => m,
            None => return Ok(Some(ctx)),
        };
        let text = msg.text();

        let content_to_match = match strip_prefix(&ctx, text) {
            Some(c) => c,
            None => return Ok(Some(ctx)),
        };

        let regex = get_regex();
        if let Some(caps) = regex.captures(content_to_match) {
            let scope_str = caps.get(1).map_or("", |m| m.as_str());
            let time_str = caps.get(2).map_or("", |m| m.as_str());

            info!(target: "Plugin/WordCloud", "收到词云请求: Scope={}, Time={}", scope_str, time_str);

            let (start_time, end_time) = get_time_range(time_str);

            let (query_group_id, query_user_id) = match scope_str {
                "本群" => {
                    if let Some(gid) = msg.group_id() {
                        (Some(gid), None)
                    } else {
                        (None, Some(msg.user_id()))
                    }
                }
                "跨群" => (None, None),
                "我的" => (None, Some(msg.user_id())),
                _ => (None, None),
            };

            if scope_str == "本群" && query_group_id.is_none() && msg.group_id().is_none() {
                let reply =
                    Message::new().text("请在群聊中使用“本群”指令，或使用“我的”查看个人词云。");
                send_msg(&ctx, writer, msg.group_id(), Some(msg.user_id()), reply).await?;
                return Ok(None);
            }
            let title = format!("{} 的 {} 词云", scope_str, time_str);
            let reply_id = msg.message_id();
            let target_group = msg.group_id();
            let target_user = Some(msg.user_id());

            // 发送提示
            send_msg(
                &ctx,
                writer.clone(),
                target_group,
                target_user,
                Message::new()
                    .reply(reply_id)
                    .text(format!("⏳ 正在生成 {}...", title)),
            )
            .await?;

            // 生成并发送
            match generate_image(&ctx, query_group_id, query_user_id, start_time, end_time).await {
                Ok(b64) => {
                    let img_msg = Message::new().image(b64);
                    send_msg(&ctx, writer, target_group, target_user, img_msg).await?;
                }
                Err(e) => {
                    let err_msg = Message::new().text(format!("❌ 生成失败：{}", e));
                    send_msg(&ctx, writer, target_group, target_user, err_msg).await?;
                    error!(target: "Plugin/WordCloud", "Handler error: {}", e);
                }
            }

            return Ok(None);
        }

        Ok(Some(ctx))
    })
}

/// 核心生成逻辑供外部调用 (例如综合日报插件)
pub async fn generate_image(
    ctx: &Context,
    query_group_id: Option<i64>,
    query_user_id: Option<i64>,
    start_time: i64,
    end_time: i64,
) -> Result<String, String> {
    let config: WordCloudConfig = get_config_or_default(ctx, "wordcloud");

    if !config.enabled {
        return Err("词云插件未启用".to_string());
    }

    let db = &ctx.db;
    let mut corpus = get_text_corpus(db, query_group_id, query_user_id, start_time, end_time)
        .await
        .map_err(|e| format!("DB Error: {}", e))?;

    if corpus.is_empty() {
        return Err("该时间段内没有足够的聊天记录".to_string());
    }

    // 截断过多消息
    if config.max_msg > 0 && corpus.len() > config.max_msg {
        let start = corpus.len().saturating_sub(config.max_msg);
        corpus = corpus.split_off(start);
    }

    let font_path = config.font_path.clone();
    let font_family = config.font_family.clone();
    let limit = config.limit;
    let width = config.width;
    let height = config.height;

    // 在阻塞线程中生成图片
    let task_result = tokio::task::spawn_blocking(move || {
        image::generate_word_cloud(corpus, font_path, font_family, limit, width, height)
    })
    .await;

    match task_result {
        Ok(res) => res,
        Err(e) => Err(format!("Task Join Error: {}", e)),
    }
}
