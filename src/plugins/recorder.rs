use crate::adapters::satori::LockedWriter;
use crate::config::build_config;
use crate::event::{Context, EventType};
use crate::plugins::{PluginError, get_config};
use chrono::{Datelike, Duration, Local, TimeZone, Timelike};
use futures_util::future::BoxFuture;
use jieba_rs::Jieba;
use sea_orm::{
    ActiveModelTrait, ActiveValue, ConnectionTrait, Schema, Set, Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use simd_json::OwnedValue;
use simd_json::base::{ValueAsArray, ValueAsScalar};
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsScalar};
use std::sync::OnceLock;
use toml::Value;

pub mod entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "message_records")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub platform: String,

        pub group_id: i64,
        pub group_name: String,

        pub user_id: i64,
        pub user_name: String,   // 对应 nickname (用户本名)
        pub sender_nick: String, // 对应 card (群名片)

        pub message_type: String,

        pub content_rich: String, // 富文本摘要
        pub tokens: String,       // 分词结果（空格分隔）

        pub role: String,
        pub is_reply: bool,
        pub length: i32,
        pub time: i64,
        pub time_hour: i32,
        pub time_weekday: i32,

        pub has_image: bool,     // 是否包含图片
        pub image_count: i32,    // 图片数量
        pub is_anim_emoji: bool, // 是否包含动画表情/表情包

        pub has_at: bool,  // 是否包含At
        pub at_count: i32, // At数量

        pub face_count: i32, // 小表情(face)数量

        pub is_voice: bool, // 是否是语音
        pub is_video: bool, // 是否是视频
        pub is_music: bool, // 是否是音乐分享

        pub is_rps: bool,  // 是否是猜拳
        pub is_dice: bool, // 是否是骰子
        pub is_poke: bool, // 是否是戳一戳

        pub is_forward: bool, // 是否是合并转发
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

use entity::ActiveModel as RecordActiveModel;
use entity::Entity as RecordEntity;

// 全局 Jieba 实例
static JIEBA: OnceLock<Jieba> = OnceLock::new();

fn get_jieba() -> &'static Jieba {
    JIEBA.get_or_init(Jieba::new)
}

#[derive(Serialize, Deserialize)]
struct RecorderConfig {
    enabled: bool,
    #[serde(default = "default_true")]
    record_self: bool,
    // 数据保留天数，默认 180 天
    #[serde(default = "default_retention_days")]
    retention_days: i64,
}

fn default_true() -> bool {
    true
}

fn default_retention_days() -> i64 {
    180
}

pub fn default_config() -> Value {
    build_config(RecorderConfig {
        enabled: true,
        record_self: true,
        retention_days: 180,
    })
}

pub fn init(ctx: Context) -> BoxFuture<'static, Result<(), PluginError>> {
    Box::pin(async move {
        let db = &ctx.db;
        let builder = db.get_database_backend();
        let schema = Schema::new(builder);

        // 1. 创建表
        let mut create_table_stmt = schema.create_table_from_entity(RecordEntity);
        create_table_stmt.if_not_exists();

        let stmt = builder.build(&create_table_stmt);
        if let Err(e) = db.execute_raw(stmt).await {
            warn!(target: "Plugin/Recorder", "Init table error (ignore if exists): {}", e);
        }

        // 2. 创建索引
        let indexes = vec![
            sea_orm::sea_query::Index::create()
                .name("idx_records_group_time")
                .table(RecordEntity)
                .col(entity::Column::GroupId)
                .col(entity::Column::Time)
                .if_not_exists()
                .to_owned(),
            sea_orm::sea_query::Index::create()
                .name("idx_records_group_user_time")
                .table(RecordEntity)
                .col(entity::Column::GroupId)
                .col(entity::Column::UserId)
                .col(entity::Column::Time)
                .if_not_exists()
                .to_owned(),
            sea_orm::sea_query::Index::create()
                .name("idx_records_user_time")
                .table(RecordEntity)
                .col(entity::Column::UserId)
                .col(entity::Column::Time)
                .if_not_exists()
                .to_owned(),
            sea_orm::sea_query::Index::create()
                .name("idx_records_time")
                .table(RecordEntity)
                .col(entity::Column::Time)
                .if_not_exists()
                .to_owned(),
        ];

        for idx in indexes {
            let stmt = builder.build(&idx);
            let _ = db.execute_raw(stmt).await;
        }

        // 3. 初始化统计聚合表（建表；若有历史数据则一次性回填，否则自愈近 7 天）
        if let Err(e) = crate::db::stats::init(db).await {
            warn!(target: "Plugin/Recorder", "统计聚合表初始化失败: {}", e);
        }

        // 4. 注册每日数据清理任务
        let scheduler = ctx.scheduler.clone();
        let db_clone = ctx.db.clone();
        let config_clone = ctx.config.clone();

        scheduler.add_daily_at(4, 0, 0, move || {
            let db = db_clone.clone();
            let cfg = config_clone.clone();
            async move {
                // 统计聚合自愈：重建近 7 天聚合行，修复异常场景下的计数漂移
                // （保留期之外的聚合行是冻结的历史，不会被触碰）
                if let Err(e) = crate::db::stats::self_heal_recent(&db).await {
                    warn!(target: "Plugin/Recorder", "统计聚合自愈失败: {}", e);
                }

                let retention_days = {
                    let guard = cfg.read().unwrap();
                    if let Some(v) = guard.plugins.get("recorder") {
                        v.get("retention_days").and_then(|x| x.as_integer()).unwrap_or(180)
                    } else {
                        180
                    }
                };

                if retention_days <= 0 {
                    info!(target: "Plugin/Recorder", "数据保留天数设置为 0 或负数，跳过清理。");
                    return;
                }

                let cutoff_time = Local::now() - Duration::days(retention_days);
                let timestamp = cutoff_time.timestamp();

                info!(target: "Plugin/Recorder", "开始清理 {} 天前的数据 (Time < {})...", retention_days, timestamp);

                // 注意：只删除原始消息记录，统计聚合表 (message_stats_daily /
                // message_user_stats_daily) 中对应日期的聚合行保留——历史统计
                // （如"去年消息数"）在原始数据过期后依然可查。
                let delete_sql = format!("DELETE FROM message_records WHERE time < {}", timestamp);
                let res = db.execute_raw(Statement::from_string(sea_orm::DatabaseBackend::Sqlite, delete_sql)).await;

                match res {
                    Ok(exec_res) => {
                        let rows = exec_res.rows_affected();
                        info!(target: "Plugin/Recorder", "已清理 {} 条过期消息记录。", rows);
                        if rows > 0 {
                            // 增量回收 freelist 页(依赖 db.rs 的 auto_vacuum=INCREMENTAL)，
                            // 避免全量 VACUUM 需要约 2× 文件大小的临时空间且长时间锁库。
                            info!(target: "Plugin/Recorder", "正在增量回收数据库空间 (incremental_vacuum)...");
                            if let Err(e) = db.execute_raw(Statement::from_string(sea_orm::DatabaseBackend::Sqlite, "PRAGMA incremental_vacuum;".to_owned())).await {
                                warn!(target: "Plugin/Recorder", "incremental_vacuum 执行失败: {}", e);
                            } else {
                                info!(target: "Plugin/Recorder", "数据库空间回收完成。");
                            }
                        }
                    },
                    Err(e) => {
                        error!(target: "Plugin/Recorder", "清理数据失败: {}", e);
                    }
                }
            }
        });

        Ok(())
    })
}

pub fn handle(
    ctx: Context,
    _writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        let config: RecorderConfig = get_config(&ctx, "recorder").unwrap_or(RecorderConfig {
            enabled: true,
            record_self: true,
            retention_days: 180,
        });

        let mut record = RecordActiveModel {
            platform: Set("qq".to_string()),
            tokens: Set("".to_string()),
            ..Default::default()
        };

        // 用于接收计算出的纯文本长度和待分词文本
        let mut text_len = 0;
        let mut raw_text_for_tokens = String::new();

        let should_insert = match &ctx.event {
            // === 接收消息 ===
            EventType::Satori(ev) => {
                let post_type = ev.get_str("post_type").unwrap_or("");
                if post_type != "message" {
                    return Ok(Some(ctx));
                }

                // 1. 基础信息
                record.time = Set(ev
                    .get_i64("time")
                    .or_else(|| ev.get_u64("time").map(|v| v as i64))
                    .unwrap_or(0));
                record.message_type =
                    Set(ev.get_str("message_type").unwrap_or("unknown").to_string());

                // 2. 群组信息 (Group)
                let group_id = ev
                    .get_i64("group_id")
                    .or_else(|| ev.get_u64("group_id").map(|v| v as i64))
                    .unwrap_or(0);
                let group_name = ev.get_str("group_name").unwrap_or("");

                record.group_id = Set(group_id);
                record.group_name = Set(group_name.to_string());

                // 3. 用户信息
                record.user_id = Set(ev
                    .get_i64("user_id")
                    .or_else(|| ev.get_u64("user_id").map(|v| v as i64))
                    .unwrap_or(0));

                if let Some(sender) = ev.get("sender") {
                    let nick = sender.get_str("nickname").unwrap_or("");
                    let card = sender.get_str("card").unwrap_or("");

                    record.user_name = Set(nick.to_string());
                    record.sender_nick = Set(if !card.is_empty() {
                        card.to_string()
                    } else {
                        nick.to_string()
                    });

                    record.role = Set(sender.get_str("role").unwrap_or("member").to_string());
                }

                // 4. 消息内容
                let msg_val = ev.get("message");
                // 解析富文本，填充 content_rich 和获取待分词文本
                let (len, raw) = parse_message_content(msg_val, &mut record);
                text_len = len;
                raw_text_for_tokens = raw;

                true
            }
            // === 发送消息 (Bot 自身) ===
            EventType::BeforeSend(packet) => {
                if !config.record_self {
                    return Ok(Some(ctx));
                }
                let now = Local::now().timestamp();
                record.time = Set(now);
                record.message_type = Set(packet.message_type().unwrap_or("unknown").to_string());

                let group_id = packet.group_id().unwrap_or(0);
                record.group_id = Set(group_id);

                // 尝试获取群名（如果 available）
                if let Some(origin) = &packet.original_event {
                    let origin_gid = origin
                        .get_i64("group_id")
                        .or_else(|| origin.get_u64("group_id").map(|v| v as i64))
                        .unwrap_or(0);

                    if origin_gid != 0 && origin_gid == group_id {
                        let g_name = origin.get_str("group_name").unwrap_or("").to_string();
                        record.group_name = Set(g_name);
                    } else {
                        record.group_name = Set("".to_string());
                    }
                } else {
                    record.group_name = Set("".to_string());
                }

                if let Ok(uid) = ctx.bot.login_user.id.parse::<i64>() {
                    record.user_id = Set(uid);
                }
                record.user_name = Set(ctx.bot.login_user.name.clone().unwrap_or_default());
                record.sender_nick = Set(ctx
                    .bot
                    .login_user
                    .nick
                    .clone()
                    .or(ctx.bot.login_user.name.clone())
                    .unwrap_or_default());
                record.role = Set("self".to_string());

                let msg_val = packet.message();

                let (len, raw) = parse_message_content(msg_val, &mut record);
                text_len = len;
                raw_text_for_tokens = raw;

                true
            }
            _ => false,
        };

        if should_insert {
            // 计算时间衍生字段
            let ts = match record.time {
                ActiveValue::Set(t) | ActiveValue::Unchanged(t) => t,
                _ => 0,
            };

            if let Some(dt) = Local.timestamp_opt(ts, 0).single() {
                record.time_hour = Set(dt.hour() as i32);
                record.time_weekday = Set(dt.weekday().num_days_from_sunday() as i32);
            }

            // 计算长度 (仅统计文本消息的字符数)
            record.length = Set(text_len);

            if !raw_text_for_tokens.is_empty() {
                let tokens = tokio::task::spawn_blocking(move || {
                    let jieba = get_jieba();
                    jieba
                        .cut(&raw_text_for_tokens, false)
                        .into_iter()
                        .map(|t| t.word)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .await
                .unwrap_or_default();
                record.tokens = Set(tokens);
            } else {
                record.tokens = Set("".to_string());
            }

            // 构建统计聚合增量（与消息入库同事务执行，保证两表一致）
            let delta = crate::db::stats::MessageStatsDelta {
                group_id: active_i64(&record.group_id),
                group_name: active_str(&record.group_name),
                user_id: active_i64(&record.user_id),
                nick: active_str(&record.sender_nick),
                time: ts,
                length: active_i32(&record.length),
                image_count: active_i32(&record.image_count),
                is_anim_emoji: active_bool(&record.is_anim_emoji),
                is_voice: active_bool(&record.is_voice),
                is_video: active_bool(&record.is_video),
                face_count: active_i32(&record.face_count),
            };

            // 消息插入 + 聚合 UPSERT 在同一事务内：要么同时生效，要么同时回滚
            let insert_res = ctx.db.transaction(|txn| {
                let record = record;
                let delta = delta;
                Box::pin(async move {
                    record.insert(txn).await?;
                    crate::db::stats::upsert_message_stats(txn, &delta).await?;
                    Ok::<(), sea_orm::DbErr>(())
                })
            })
            .await;

            if let Err(e) = insert_res {
                error!(target: "Plugin/Recorder", "消息记录失败: {}", e);
            }
        }

        Ok(Some(ctx))
    })
}

/// 读取 ActiveModel 字段当前值（未设置时返回默认值），用于构建统计聚合增量
fn active_i64(v: &ActiveValue<i64>) -> i64 {
    match v {
        ActiveValue::Set(x) | ActiveValue::Unchanged(x) => *x,
        _ => 0,
    }
}

fn active_i32(v: &ActiveValue<i32>) -> i32 {
    match v {
        ActiveValue::Set(x) | ActiveValue::Unchanged(x) => *x,
        _ => 0,
    }
}

fn active_bool(v: &ActiveValue<bool>) -> bool {
    match v {
        ActiveValue::Set(x) | ActiveValue::Unchanged(x) => *x,
        _ => false,
    }
}

fn active_str(v: &ActiveValue<String>) -> String {
    match v {
        ActiveValue::Set(x) | ActiveValue::Unchanged(x) => x.clone(),
        _ => String::new(),
    }
}

/// 解析消息段数组，提取富文本摘要、特征标记，并返回 (纯文本长度, 拼接后的纯文本)
fn parse_message_content(
    msg_val: Option<&OwnedValue>,
    record: &mut RecordActiveModel,
) -> (i32, String) {
    let mut rich_text = String::new();
    // 存储分段的纯文本，用于最终拼接 tokens
    let mut text_segments: Vec<String> = Vec::new();
    let mut text_char_count = 0;

    // 统计变量
    let mut image_count = 0;
    let mut at_count = 0;
    let mut face_count = 0;

    // 标记变量
    let mut is_anim_emoji = false;
    let mut is_voice = false;
    let mut is_video = false;
    let mut is_music = false;
    let mut is_rps = false;
    let mut is_dice = false;
    let mut is_poke = false;
    let mut is_forward = false;
    let mut is_reply_flag = false;

    if let Some(val) = msg_val {
        // 情况 1: 纯字符串消息
        if let Some(s) = val.as_str() {
            rich_text.push_str(s);
            text_segments.push(s.trim().to_string());
            text_char_count += s.chars().count();
        }
        // 情况 2: 消息段数组
        else if let Some(arr) = val.as_array() {
            for seg in arr {
                let type_ = seg.get_str("type").unwrap_or("unknown");
                let data = seg.get("data");

                match type_ {
                    "text" => {
                        if let Some(t) = data.and_then(|d| d.get_str("text")) {
                            rich_text.push_str(t);
                            let trimmed = t.trim();
                            if !trimmed.is_empty() {
                                text_segments.push(trimmed.to_string());
                            }
                            text_char_count += t.chars().count();
                        }
                    }
                    "at" => {
                        at_count += 1;
                        let qq = data
                            .and_then(|d| {
                                d.get_str("qq")
                                    .map(|s| s.to_string())
                                    .or_else(|| d.get_i64("qq").map(|i| i.to_string()))
                                    .or_else(|| d.get_u64("qq").map(|i| i.to_string()))
                            })
                            .unwrap_or_default();
                        rich_text.push_str(&format!("[@{}]", qq));
                    }
                    "face" => {
                        face_count += 1;
                        rich_text.push_str("[表情]");
                    }
                    "image" => {
                        image_count += 1;
                        // 检查是否为动画表情
                        if let Some(d) = data {
                            let summary = d.get_str("summary").unwrap_or("");
                            let sub_type = d
                                .get_i64("sub_type")
                                .or_else(|| d.get_u64("sub_type").map(|v| v as i64))
                                .unwrap_or(0);
                            if summary == "[动画表情]" || sub_type == 1 {
                                is_anim_emoji = true;
                            }
                        }
                        rich_text.push_str("[图片]");
                    }
                    "record" => {
                        is_voice = true;
                        rich_text.push_str("[语音]");
                    }
                    "video" => {
                        is_video = true;
                        rich_text.push_str("[视频]");
                    }
                    "music" => {
                        is_music = true;
                        rich_text.push_str("[音乐]");
                    }
                    "poke" => {
                        is_poke = true;
                        rich_text.push_str("[戳一戳]");
                    }
                    "rps" => {
                        is_rps = true;
                        rich_text.push_str("[猜拳]");
                    }
                    "dice" => {
                        is_dice = true;
                        rich_text.push_str("[骰子]");
                    }
                    "forward" | "node" => {
                        is_forward = true;
                        rich_text.push_str("[合并转发]");
                    }
                    "reply" => {
                        is_reply_flag = true;
                        rich_text.push_str("[回复]");
                    }
                    "json" => rich_text.push_str("[卡片]"),
                    "file" => rich_text.push_str("[文件]"),
                    other => rich_text.push_str(&format!("[{}]", other)),
                }
            }
        }
    }

    record.content_rich = Set(rich_text);

    // 拼接纯文本
    let joined_text = text_segments.join(" ");

    record.has_image = Set(image_count > 0);
    record.has_at = Set(at_count > 0);
    record.is_reply = Set(is_reply_flag);
    record.image_count = Set(image_count);
    record.is_anim_emoji = Set(is_anim_emoji);
    record.at_count = Set(at_count);
    record.face_count = Set(face_count);
    record.is_voice = Set(is_voice);
    record.is_video = Set(is_video);
    record.is_music = Set(is_music);
    record.is_rps = Set(is_rps);
    record.is_dice = Set(is_dice);
    record.is_poke = Set(is_poke);
    record.is_forward = Set(is_forward);

    (text_char_count as i32, joined_text)
}
