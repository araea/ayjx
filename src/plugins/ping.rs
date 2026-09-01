use crate::adapters::satori::{LockedWriter, send_msg};
use crate::command::match_command;
use crate::config::build_config;
use crate::event::Context;
use crate::message::Message;
use crate::plugins::PluginError;
use futures_util::future::BoxFuture;
use sea_orm::{ActiveModelTrait, ConnectionTrait, EntityTrait, PaginatorTrait, Schema, Set};
use serde::{Deserialize, Serialize};
use toml::Value;

mod entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "plugin_ping_stats")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub user_id: i64,
        #[sea_orm(default_expr = "Expr::current_timestamp()")]
        pub created_at: DateTimeUtc,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

use entity::ActiveModel as PingStatsActiveModel;
use entity::Entity as PingStats;

#[derive(Debug, Serialize, Deserialize, Clone)]
struct PingConfig {
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

pub fn default_config() -> Value {
    build_config(PingConfig { enabled: true })
}

pub fn init(ctx: Context) -> BoxFuture<'static, Result<(), PluginError>> {
    Box::pin(async move {
        let db = &ctx.db;
        let builder = db.get_database_backend();
        let schema = Schema::new(builder);

        let mut create_table_stmt = schema.create_table_from_entity(PingStats);
        create_table_stmt.if_not_exists();

        let stmt = builder.build(&create_table_stmt);

        db.execute_raw(stmt)
            .await
            .map_err(|e| format!("PingPong Plugin Init DB Error: {}", e))?;

        Ok(())
    })
}

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        if let Some(_cmd) = match_command(&ctx, "ping") {
            let msg = ctx.as_message().unwrap();

            let db = &ctx.db;

            let user_id = msg.user_id();
            let group_id = msg.group_id();
            let message_id = msg.message_id();

            let new_stat = PingStatsActiveModel {
                user_id: Set(user_id),
                ..Default::default()
            };

            new_stat
                .insert(db)
                .await
                .map_err(|e| format!("DB Insert Error: {}", e))?;

            let count = PingStats::find()
                .count(db)
                .await
                .map_err(|e| format!("DB Query Error: {}", e))?;

            // 回复消息
            let reply_msg = Message::new()
                .reply(message_id)
                .text(format!("✅ Pong！全服累计 Ping 次数：{}", count));

            send_msg(&ctx, writer.clone(), group_id, Some(user_id), reply_msg).await?;

            // 发送合并转发消息
            let node1 = Message::new().text("系统日志：收到心跳检测请求");
            let node2 = Message::new().text(format!("数据库写入成功，当前记录 ID：{}", count));
            let node3 = Message::new()
                .text("这是一个自定义合并转发消息的示例。")
                .image("https://www.sea-ql.org/SeaORM/img/SeaORM%20banner.png");

            let forward_msg = Message::new()
                .node_custom(10000, "System Bot", node1)
                .node_custom(10000, "Database", node2)
                .node_custom(user_id, "User Context", node3);

            send_msg(&ctx, writer, group_id, Some(user_id), forward_msg).await?;

            return Ok(None);
        }

        Ok(Some(ctx))
    })
}
