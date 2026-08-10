pub mod config;
pub mod data;
pub mod engine;
pub mod entity;

use crate::adapters::onebot::{LockedWriter, send_msg};
use crate::command::match_command;
use crate::config::build_config;
use crate::event::Context;
use crate::message::Message;
use crate::plugins::ciyi::config::CiYiConfig;
use crate::plugins::ciyi::entity::{record as record_entity, state as state_entity};
use crate::plugins::{PluginError, get_config};
use futures_util::future::BoxFuture;
use sea_orm::{ConnectionTrait, Schema};
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsScalar};
use toml::Value;

pub fn default_config() -> Value {
    build_config(CiYiConfig::default())
}

pub fn init(ctx: Context) -> BoxFuture<'static, Result<(), PluginError>> {
    Box::pin(async move {
        let db = &ctx.db;
        let builder = db.get_database_backend();
        let schema = Schema::new(builder);

        // 1. 创建 State 表
        let mut create_state_table = schema.create_table_from_entity(state_entity::Entity);
        if let Err(e) = db
            .execute(builder.build(create_state_table.if_not_exists()))
            .await
        {
            crate::warn!(target: "Plugin/CiYi", "Init state table error: {}", e);
        }

        // 2. 创建 Record 表
        let mut create_record_table = schema.create_table_from_entity(record_entity::Entity);
        if let Err(e) = db
            .execute(builder.build(create_record_table.if_not_exists()))
            .await
        {
            crate::warn!(target: "Plugin/CiYi", "Init record table error: {}", e);
        }

        // 3. 创建索引 (针对排行榜查询优化)
        // 索引1: WHERE group_id GROUP BY user_id
        let idx_group_user = sea_orm::sea_query::Index::create()
            .name("idx_ciyi_win_record_group_user")
            .table(record_entity::Entity)
            .col(record_entity::Column::GroupId)
            .col(record_entity::Column::UserId)
            .if_not_exists()
            .to_owned();

        if let Err(e) = db.execute(builder.build(&idx_group_user)).await {
            crate::warn!(target: "Plugin/CiYi", "Init index error: {}", e);
        }

        // crate::info!(target: "Plugin/CiYi", "词意游戏初始化完成 (SQL)");
        Ok(())
    })
}

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        let msg_event = match ctx.as_message() {
            Some(e) => e,
            None => return Ok(Some(ctx)),
        };

        // 仅在群聊中响应
        let group_id = match msg_event.group_id() {
            Some(id) => id,
            None => return Ok(Some(ctx)),
        };
        let user_id = msg_event.user_id();
        let text = msg_event.text().trim();

        if text.is_empty() {
            return Ok(Some(ctx));
        }

        let config: CiYiConfig = get_config(&ctx, "ciyi").unwrap_or_default();

        // A. 直接猜测模式 (两个字)
        if text.chars().count() == 2 {
            let should_direct_guess =
                engine::get_direct_guess_status(&ctx.db, group_id, config.plugin.direct_guess)
                    .await;

            if should_direct_guess {
                let username = msg_event.sender_name().to_string();
                let reply =
                    engine::guess_word(&ctx.db, group_id, user_id, &username, text, &config).await;

                send_response(&ctx, writer, group_id, user_id, &reply, &config).await?;
                return Ok(None); // 阻止后续处理
            }
        }

        // B. 指令处理
        let commands = vec![
            (
                vec!["词意帮助", "词意指令", "词意指令列表", "词意帮助列表"],
                "help",
            ),
            (vec!["词意玩法", "词意规则"], "rules"),
            (vec!["词意猜测"], "guess"),
            (vec!["词意榜"], "rank_group"),
            (vec!["词意全榜"], "rank_global"),
            (vec!["切换猜测模式"], "toggle_mode"),
        ];

        for (aliases, action) in commands {
            for alias in aliases {
                if let Some(cmd) = match_command(&ctx, alias) {
                    let response = match action {
                        "help" => show_commands(),
                        "rules" => show_rules(),
                        "guess" => {
                            let arg = cmd
                                .args
                                .first()
                                .and_then(|seg| seg.get("data"))
                                .and_then(|d| d.get_str("text"))
                                .unwrap_or("")
                                .trim();
                            if arg.chars().count() != 2 {
                                "无效输入，请发送两个字的词语。".to_string()
                            } else {
                                let username = msg_event.sender_name().to_string();
                                engine::guess_word(
                                    &ctx.db, group_id, user_id, &username, arg, &config,
                                )
                                .await
                            }
                        }
                        "rank_group" => {
                            engine::get_channel_leaderboard(
                                &ctx.db,
                                group_id,
                                config.plugin.rank_display,
                            )
                            .await
                        }
                        "rank_global" => {
                            engine::get_global_leaderboard(&ctx.db, config.plugin.rank_display)
                                .await
                        }
                        "toggle_mode" => {
                            engine::toggle_direct_guess_mode(
                                &ctx.db,
                                group_id,
                                config.plugin.direct_guess,
                            )
                            .await
                        }
                        _ => String::new(),
                    };

                    if !response.is_empty() {
                        send_response(&ctx, writer, group_id, user_id, &response, &config).await?;
                    }
                    return Ok(None);
                }
            }
        }

        Ok(Some(ctx))
    })
}

// 辅助函数：构建并发送回复
async fn send_response(
    ctx: &Context,
    writer: LockedWriter,
    group_id: i64,
    user_id: i64,
    text: &str,
    config: &CiYiConfig,
) -> Result<(), PluginError> {
    let mut msg = Message::new();

    if config.plugin.quote_user {
        let msg_id = ctx.as_message().map(|m| m.message_id()).unwrap_or(0);
        msg = msg.reply(msg_id);
    }

    if config.plugin.at_user {
        msg = msg.at(user_id).text("\n");
    }

    msg = msg.text(text);

    let msg_segments: Vec<simd_json::owned::Value> = msg
        .0
        .into_iter()
        .map(|seg| {
            let mut obj = simd_json::owned::Object::new();
            obj.insert("type".into(), seg.type_.into());
            obj.insert("data".into(), seg.data.into());
            simd_json::owned::Value::from(obj)
        })
        .collect();

    send_msg(ctx, writer, Some(group_id), None, msg_segments).await?;
    Ok(())
}

fn show_commands() -> String {
    let list = [
        "词意帮助/词意指令 - 查看插件指令列表",
        "词意玩法/词意规则 - 查看词意游戏规则",
        "词意猜测 [词语] - 猜测两字词语",
        "词意榜 - 查看当前频道的词意排行榜",
        "词意全榜 - 查看所有人的词意排行榜",
        "切换猜测模式 - 切换是否可以直接发送词语猜测",
    ];
    list.join("\n")
}

fn show_rules() -> String {
    "\
目标
    猜出系统选择的两字词语

反馈
    每次猜测后，获得：
    - 与目标词语的相似度排名
    - 相邻词提示

示例
    1. ？器 ) 镯子 ( 玉？   #14
    2. ？子 ) 玉佩 ( 东？   #15
    3. ？佩 ) 东西 ( 冥？   #16

    #14   → 相似度排名（越小越近）
    玉？   → 相邻词提示（？为“佩”）

周期
    每日一词，猜对则次日刷新
    系统记录猜对次数，可查排行"
        .to_string()
}
