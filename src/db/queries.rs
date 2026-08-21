use crate::db::stats;
use crate::plugins::recorder::entity::{self, Entity as MessageLogs};
use sea_orm::sea_query::{Alias, Expr, Func, SimpleExpr};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DbErr, EntityTrait, FromQueryResult,
    PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, Statement,
};

// ================= 常量定义 =================

const MAX_TEXT_CORPUS_LIMIT: u64 = 20000;
const MAX_RANKING_LIMIT: u64 = 50;
const MAX_TREND_LIMIT: u64 = 2000;
const MAX_GROUP_TREND_LIMIT: u64 = 10000;

// ================= 数据结构 =================

/// 纯文本数据（用于生成词云）
#[derive(Debug, FromQueryResult)]
pub struct TextData {
    pub content_text: String,
}

/// 用户活跃排行（龙王榜）
#[derive(Debug, FromQueryResult)]
pub struct UserRanking {
    pub user_id: i64,
    pub nickname: String,
    pub count: i64,
}

/// 群组活跃排行
#[derive(Debug, FromQueryResult)]
pub struct GroupRanking {
    pub group_id: i64,
    pub group_name: String,
    pub count: i64,
}

/// 每日消息量走势
#[derive(Debug, FromQueryResult)]
pub struct DailyTrend {
    pub date: String,
    pub count: i64,
}

/// 群组每日消息量走势 (用于多线图)
#[derive(Debug, FromQueryResult)]
pub struct GroupTrend {
    pub date: String,
    pub group_name: String,
    pub count: i64,
}

/// 消息类型走势数据点
#[derive(Debug, FromQueryResult)]
pub struct MessageTypeTrend {
    pub date: String,
    pub text: i64,
    pub image: i64,
    pub voice: i64,
    pub video: i64,
    pub anim_emoji: i64,
    pub face: i64,
}

/// 小时活跃分布
#[derive(Debug, FromQueryResult)]
pub struct HourlyActivity {
    pub hour: i32,
    pub count: i64,
}

/// 星期活跃分布
#[derive(Debug, FromQueryResult)]
pub struct WeekdayActivity {
    pub weekday: i32,
    pub count: i64,
}

/// 星期×小时 热力分布数据
#[derive(Debug, FromQueryResult)]
pub struct HeatmapData {
    pub weekday: i32,
    pub hour: i32,
    pub count: i64,
}

/// 消息类型统计
#[derive(Debug, FromQueryResult)]
pub struct MessageTypeStats {
    pub text: i64,
    pub image: i64,
    pub voice: i64,
    pub video: i64,
    pub anim_emoji: i64,
    pub face: i64,
}

// ================= 查询路由辅助 =================

// 查询策略说明：
// - 群/全局维度的跨多日统计 → 聚合表 message_stats_daily / message_user_stats_daily
//   （每群每日一行，查询"今年"仅读 ≤366 行，不再扫描原始消息）；
// - 非整日边界片段（近7天/近30天的起止时刻落在一天中间）→ 回退原始表小范围查询后合并；
// - 用户维度单用户的统计 → 原始表（idx_records_user_time 覆盖索引本身高效，
//   且单用户消息量远小于全群）；
// - 需要消息内容的查询（词云语料）→ 原始表（聚合表不含内容，无法替代）。

/// 构建非整日边界片段的 UNION ALL 子查询列表。
///
/// 每个边界片段独立成段（而非 OR 条件合并）：实测 OR 条件会让查询计划退化为
/// 按 group_id 前缀扫描全群索引条目，而分段后每段各自走 (group_id, time)
/// 索引范围扫描，边界片段通常只有一个残日，开销可忽略。
///
/// `tmpl` 接收片段的 (start, end) 时间戳，返回一段完整的
/// `UNION ALL SELECT ... FROM message_records WHERE time >= ? AND time < ? ...` 子查询。
fn partial_union_subqueries(
    partials: &[(i64, i64)],
    tmpl: impl Fn(i64, i64) -> String,
) -> String {
    partials
        .iter()
        .map(|(s, e)| tmpl(*s, *e))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 群过滤条件（拼入聚合表 SQL；group_id 为数值，无注入风险）
fn group_cond_sql(group_id: Option<i64>) -> String {
    group_id
        .map(|g| format!(" AND group_id = {}", g))
        .unwrap_or_default()
}

async fn scalar_from_sql(db: &DatabaseConnection, sql: String) -> Result<i64, DbErr> {
    let stmt = Statement::from_string(db.get_database_backend(), sql);
    let row = db.query_one(stmt).await?;
    match row {
        Some(r) => Ok(r.try_get::<i64>("", "c")?),
        None => Ok(0),
    }
}

// ================= 查询函数（公共接口） =================

/// 获取指定时间范围内的纯文本内容列表
///
/// 始终查询原始表：词云需要消息原文（分词结果），聚合表无法替代；
/// 上限 MAX_TEXT_CORPUS_LIMIT 条，实测极端规模（最活跃群 73 万行）下 ~25ms。
pub async fn get_text_corpus(
    db: &DatabaseConnection,
    group_id: Option<i64>,
    user_id: Option<i64>,
    start_time: i64,
    end_time: i64,
) -> Result<Vec<String>, DbErr> {
    raw_text_corpus(db, group_id, user_id, start_time, end_time).await
}

/// 获取活跃用户排行（龙王榜）
///
/// 整日部分走 message_user_stats_daily（群×用户×日聚合），
/// 非整日边界回退原始表，SQL 内 UNION ALL 合并后统一排序。
pub async fn get_user_ranking(
    db: &DatabaseConnection,
    group_id: Option<i64>,
    start_time: i64,
    end_time: i64,
    limit: u64,
) -> Result<Vec<UserRanking>, DbErr> {
    let sr = stats::split_range(start_time, end_time);
    let Some((from, to)) = sr.full_days else {
        return raw_user_ranking(db, group_id, start_time, end_time, limit).await;
    };

    let n = limit.min(MAX_RANKING_LIMIT);
    let g = group_cond_sql(group_id);
    let partials = partial_union_subqueries(&sr.partials, |s, e| {
        format!(
            "UNION ALL SELECT user_id, MAX(sender_nick) AS nickname, COUNT(*) AS cnt \
             FROM message_records WHERE time >= {s} AND time < {e}{g} GROUP BY user_id"
        )
    });
    let sql = format!(
        "SELECT user_id, MAX(nickname) AS nickname, SUM(cnt) AS count FROM (\
           SELECT user_id, nick AS nickname, msg_count AS cnt \
           FROM message_user_stats_daily \
           WHERE stat_date >= '{from}' AND stat_date <= '{to}'{g} \
           {partials} \
         ) GROUP BY user_id ORDER BY count DESC LIMIT {n}"
    );
    UserRanking::find_by_statement(Statement::from_string(
        db.get_database_backend(),
        sql,
    ))
    .all(db)
    .await
}

/// 获取群组活跃排行
///
/// 整日部分走 message_stats_daily，非整日边界回退原始表。
pub async fn get_group_ranking(
    db: &DatabaseConnection,
    start_time: i64,
    end_time: i64,
    limit: u64,
) -> Result<Vec<GroupRanking>, DbErr> {
    let sr = stats::split_range(start_time, end_time);
    let Some((from, to)) = sr.full_days else {
        return raw_group_ranking(db, start_time, end_time, limit).await;
    };

    let n = limit.min(MAX_RANKING_LIMIT);
    let partials = partial_union_subqueries(&sr.partials, |s, e| {
        format!(
            "UNION ALL SELECT group_id, MAX(group_name) AS group_name, COUNT(*) AS cnt \
             FROM message_records WHERE time >= {s} AND time < {e} AND group_id != 0 GROUP BY group_id"
        )
    });
    let sql = format!(
        "SELECT group_id, MAX(group_name) AS group_name, SUM(cnt) AS count FROM (\
           SELECT group_id, group_name, msg_count AS cnt \
           FROM message_stats_daily \
           WHERE group_id != 0 AND stat_date >= '{from}' AND stat_date <= '{to}' \
           {partials} \
         ) GROUP BY group_id ORDER BY count DESC LIMIT {n}"
    );
    GroupRanking::find_by_statement(Statement::from_string(
        db.get_database_backend(),
        sql,
    ))
    .all(db)
    .await
}

/// 获取用户参与的群组排行 ("我的...排行")
///
/// 整日部分走 message_user_stats_daily（idx_user_stats_user_date 索引），
/// 非整日边界回退原始表。
pub async fn get_user_group_participation_ranking(
    db: &DatabaseConnection,
    user_id: i64,
    start_time: i64,
    end_time: i64,
    limit: u64,
) -> Result<Vec<GroupRanking>, DbErr> {
    let sr = stats::split_range(start_time, end_time);
    let Some((from, to)) = sr.full_days else {
        return raw_user_group_participation_ranking(db, user_id, start_time, end_time, limit)
            .await;
    };

    let n = limit.min(MAX_RANKING_LIMIT);
    let partials = partial_union_subqueries(&sr.partials, |s, e| {
        format!(
            "UNION ALL SELECT group_id, MAX(group_name) AS group_name, COUNT(*) AS cnt \
             FROM message_records WHERE time >= {s} AND time < {e} \
             AND user_id = {user_id} AND group_id != 0 GROUP BY group_id"
        )
    });
    let sql = format!(
        "SELECT group_id, MAX(group_name) AS group_name, SUM(cnt) AS count FROM (\
           SELECT group_id, group_name, msg_count AS cnt \
           FROM message_user_stats_daily \
           WHERE user_id = {user_id} AND group_id != 0 \
             AND stat_date >= '{from}' AND stat_date <= '{to}' \
           {partials} \
         ) GROUP BY group_id ORDER BY count DESC LIMIT {n}"
    );
    GroupRanking::find_by_statement(Statement::from_string(
        db.get_database_backend(),
        sql,
    ))
    .all(db)
    .await
}

/// 获取用户表情包使用量排行
///
/// 整日部分走 message_user_stats_daily（anim_emoji_count 聚合），
/// 非整日边界回退原始表。
pub async fn get_user_emoji_ranking(
    db: &DatabaseConnection,
    group_id: Option<i64>,
    start_time: i64,
    end_time: i64,
    limit: u64,
) -> Result<Vec<UserRanking>, DbErr> {
    let sr = stats::split_range(start_time, end_time);
    let Some((from, to)) = sr.full_days else {
        return raw_user_emoji_ranking(db, group_id, start_time, end_time, limit).await;
    };

    let n = limit.min(MAX_RANKING_LIMIT);
    let g = group_cond_sql(group_id);
    let partials = partial_union_subqueries(&sr.partials, |s, e| {
        format!(
            "UNION ALL SELECT user_id, MAX(sender_nick) AS nickname, \
             SUM(CAST(is_anim_emoji AS integer)) AS cnt \
             FROM message_records WHERE time >= {s} AND time < {e}{g} GROUP BY user_id"
        )
    });
    let sql = format!(
        "SELECT user_id, MAX(nickname) AS nickname, SUM(cnt) AS count FROM (\
           SELECT user_id, nick AS nickname, anim_emoji_count AS cnt \
           FROM message_user_stats_daily \
           WHERE stat_date >= '{from}' AND stat_date <= '{to}'{g} \
           {partials} \
         ) GROUP BY user_id ORDER BY count DESC LIMIT {n}"
    );
    UserRanking::find_by_statement(Statement::from_string(
        db.get_database_backend(),
        sql,
    ))
    .all(db)
    .await
}

/// 获取每日消息量走势 (总)
///
/// 用户维度走原始表（单用户消息量小，idx_records_user_time 高效）；
/// 群/全局维度整日部分走 message_stats_daily，边界片段回退原始表。
pub async fn get_daily_trend(
    db: &DatabaseConnection,
    group_id: Option<i64>,
    user_id: Option<i64>,
    start_time: i64,
    end_time: i64,
) -> Result<Vec<DailyTrend>, DbErr> {
    if user_id.is_some() {
        return raw_daily_trend(db, group_id, user_id, start_time, end_time).await;
    }

    let sr = stats::split_range(start_time, end_time);
    let Some((from, to)) = sr.full_days else {
        return raw_daily_trend(db, group_id, None, start_time, end_time).await;
    };

    let mut merged: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();

    // 整日部分：每群每日一行的聚合表，按日求和
    let g = group_cond_sql(group_id);
    let sql = format!(
        "SELECT stat_date AS date, SUM(msg_count) AS count FROM message_stats_daily \
         WHERE stat_date >= '{from}' AND stat_date <= '{to}'{g} \
         GROUP BY stat_date ORDER BY stat_date"
    );
    let rows: Vec<DailyTrend> = DailyTrend::find_by_statement(Statement::from_string(
        db.get_database_backend(),
        sql,
    ))
    .all(db)
    .await?;
    for r in rows {
        merged.insert(r.date, r.count);
    }

    // 边界片段：回退原始表
    for (s, e) in &sr.partials {
        for r in raw_daily_trend(db, group_id, None, *s, *e).await? {
            *merged.entry(r.date).or_insert(0) += r.count;
        }
    }

    Ok(merged
        .into_iter()
        .take(MAX_TREND_LIMIT as usize)
        .map(|(date, count)| DailyTrend { date, count })
        .collect())
}

/// 获取各群每日消息量走势 (用于"所有群...走势")
///
/// 按小时聚合（by_hour）时区间必为单日（≤24h），直接走原始表；
/// 按日聚合时整日部分走 message_stats_daily（自带最新群名）。
pub async fn get_daily_trend_by_group(
    db: &DatabaseConnection,
    start_time: i64,
    end_time: i64,
    by_hour: bool,
) -> Result<Vec<GroupTrend>, DbErr> {
    if by_hour {
        return raw_daily_trend_by_group(db, start_time, end_time, true).await;
    }

    let sr = stats::split_range(start_time, end_time);
    let Some((from, to)) = sr.full_days else {
        return raw_daily_trend_by_group(db, start_time, end_time, false).await;
    };

    // (群名, 日期) → 消息数
    let mut merged: std::collections::HashMap<(String, String), i64> =
        std::collections::HashMap::new();

    let sql = format!(
        "SELECT stat_date AS date, group_name, msg_count AS count \
         FROM message_stats_daily \
         WHERE group_id != 0 AND stat_date >= '{from}' AND stat_date <= '{to}' \
         ORDER BY stat_date"
    );
    let rows: Vec<GroupTrend> = GroupTrend::find_by_statement(Statement::from_string(
        db.get_database_backend(),
        sql,
    ))
    .all(db)
    .await?;
    for r in rows {
        merged.insert((r.group_name, r.date), r.count);
    }

    for (s, e) in &sr.partials {
        for r in raw_daily_trend_by_group(db, *s, *e, false).await? {
            *merged.entry((r.group_name, r.date)).or_insert(0) += r.count;
        }
    }

    let mut result: Vec<GroupTrend> = merged
        .into_iter()
        .map(|((group_name, date), count)| GroupTrend {
            date,
            group_name,
            count,
        })
        .collect();
    result.sort_by(|a, b| a.date.cmp(&b.date));
    result.truncate(MAX_GROUP_TREND_LIMIT as usize);
    Ok(result)
}

/// 获取消息类型走势 (多维度)
///
/// 用户维度走原始表；群/全局维度整日部分走聚合表（按日/按小时），
/// 按小时聚合仅用于 ≤24h 区间（无完整自然日），自动回退原始表。
pub async fn get_message_type_trend(
    db: &DatabaseConnection,
    group_id: Option<i64>,
    user_id: Option<i64>,
    start_time: i64,
    end_time: i64,
    by_hour: bool,
) -> Result<Vec<MessageTypeTrend>, DbErr> {
    // 按小时聚合需要小时粒度，聚合表为日粒度，直接走原始表
    // （调用方仅在区间 ≤24h 时启用 by_hour，原始表扫描量很小）
    if by_hour || user_id.is_some() {
        return raw_message_type_trend(db, group_id, user_id, start_time, end_time, by_hour).await;
    }

    let sr = stats::split_range(start_time, end_time);
    let Some((from, to)) = sr.full_days else {
        return raw_message_type_trend(db, group_id, None, start_time, end_time, false).await;
    };

    let mut merged: std::collections::BTreeMap<String, MessageTypeTrend> =
        std::collections::BTreeMap::new();

    // 整日部分：聚合表逐日累计 6 类计数
    let g = group_cond_sql(group_id);
    let sql = format!(
        "SELECT stat_date AS date, SUM(text_count) AS text, SUM(image_count) AS image, \
         SUM(voice_count) AS voice, SUM(video_count) AS video, \
         SUM(anim_emoji_count) AS anim_emoji, SUM(face_count) AS face \
         FROM message_stats_daily \
         WHERE stat_date >= '{from}' AND stat_date <= '{to}'{g} \
         GROUP BY stat_date ORDER BY stat_date"
    );
    let rows: Vec<MessageTypeTrend> =
        MessageTypeTrend::find_by_statement(Statement::from_string(
            db.get_database_backend(),
            sql,
        ))
        .all(db)
        .await?;
    for r in rows {
        merged.insert(
            r.date.clone(),
            MessageTypeTrend {
                date: r.date,
                text: r.text,
                image: r.image,
                voice: r.voice,
                video: r.video,
                anim_emoji: r.anim_emoji,
                face: r.face,
            },
        );
    }

    // 边界片段：回退原始表并逐字段累加
    for (s, e) in &sr.partials {
        for r in raw_message_type_trend(db, group_id, None, *s, *e, false).await? {
            let entry = merged.entry(r.date.clone()).or_insert(MessageTypeTrend {
                date: r.date.clone(),
                text: 0,
                image: 0,
                voice: 0,
                video: 0,
                anim_emoji: 0,
                face: 0,
            });
            entry.text += r.text;
            entry.image += r.image;
            entry.voice += r.voice;
            entry.video += r.video;
            entry.anim_emoji += r.anim_emoji;
            entry.face += r.face;
        }
    }

    Ok(merged
        .into_values()
        .take(MAX_TREND_LIMIT as usize)
        .collect())
}

/// 获取24小时活跃时段分布
///
/// 始终走原始表：仅用于 ≤24h 区间（"今日/昨日"走势图），
/// time_hour 已物化为列，扫描量小。
pub async fn get_hourly_activity(
    db: &DatabaseConnection,
    group_id: Option<i64>,
    user_id: Option<i64>,
    start_time: i64,
    end_time: i64,
) -> Result<Vec<HourlyActivity>, DbErr> {
    raw_hourly_activity(db, group_id, user_id, start_time, end_time).await
}

/// 获取星期活跃分布
///
/// 始终走原始表：星期维度无聚合表，且该查询当前未被调用方使用。
pub async fn get_weekday_activity(
    db: &DatabaseConnection,
    group_id: Option<i64>,
    start_time: i64,
    end_time: i64,
) -> Result<Vec<WeekdayActivity>, DbErr> {
    raw_weekday_activity(db, group_id, start_time, end_time).await
}

/// 获取 星期×小时 的热力分布数据
///
/// 始终走原始表：二维时间维度无聚合表，且该查询当前未被调用方使用。
pub async fn get_heatmap_data(
    db: &DatabaseConnection,
    group_id: Option<i64>,
    start_time: i64,
    end_time: i64,
) -> Result<Vec<HeatmapData>, DbErr> {
    raw_heatmap_data(db, group_id, start_time, end_time).await
}

/// 获取消息类型统计 (总计)
///
/// 用户维度走原始表；群/全局维度整日部分走聚合表（6 类计数已预聚合），
/// 边界片段回退原始表后逐字段相加。
pub async fn get_message_type_stats(
    db: &DatabaseConnection,
    group_id: Option<i64>,
    user_id: Option<i64>,
    start_time: i64,
    end_time: i64,
) -> Result<MessageTypeStats, DbErr> {
    if user_id.is_some() {
        return raw_message_type_stats(db, group_id, user_id, start_time, end_time).await;
    }

    let sr = stats::split_range(start_time, end_time);
    let Some((from, to)) = sr.full_days else {
        return raw_message_type_stats(db, group_id, None, start_time, end_time).await;
    };

    let g = group_cond_sql(group_id);
    let sql = format!(
        "SELECT COALESCE(SUM(text_count), 0) AS text, COALESCE(SUM(image_count), 0) AS image, \
         COALESCE(SUM(voice_count), 0) AS voice, COALESCE(SUM(video_count), 0) AS video, \
         COALESCE(SUM(anim_emoji_count), 0) AS anim_emoji, \
         COALESCE(SUM(face_count), 0) AS face \
         FROM message_stats_daily \
         WHERE stat_date >= '{from}' AND stat_date <= '{to}'{g}"
    );
    let mut result = MessageTypeStats::find_by_statement(Statement::from_string(
        db.get_database_backend(),
        sql,
    ))
    .one(db)
    .await?
    .unwrap_or(MessageTypeStats {
        text: 0,
        image: 0,
        voice: 0,
        video: 0,
        anim_emoji: 0,
        face: 0,
    });

    for (s, e) in &sr.partials {
        let p = raw_message_type_stats(db, group_id, None, *s, *e).await?;
        result.text += p.text;
        result.image += p.image;
        result.voice += p.voice;
        result.video += p.video;
        result.anim_emoji += p.anim_emoji;
        result.face += p.face;
    }

    Ok(result)
}

/// 获取指定条件下的活跃用户数量 (DISTINCT user_id)
///
/// 始终走原始表：DISTINCT 语义跨日不去重（用户多日活跃只算一次），
/// 无法由按日聚合行直接求和；调用方（定时推送）均使用 今日/昨日/上周/上月
/// 等小范围区间，idx_records_group_time 索引下耗时可忽略。
pub async fn get_active_user_count(
    db: &DatabaseConnection,
    group_id: Option<i64>,
    start_time: i64,
    end_time: i64,
) -> Result<u64, DbErr> {
    raw_active_user_count(db, group_id, start_time, end_time).await
}

/// 获取指定条件下的消息数量
///
/// 用户维度走原始表（覆盖索引计数）；群/全局维度整日部分走聚合表
/// （查询某群"今年"消息数只需读 ≤366 行聚合记录求和），
/// 边界片段回退原始表小范围计数。
pub async fn get_message_count(
    db: &DatabaseConnection,
    group_id: Option<i64>,
    user_id: Option<i64>,
    start_time: i64,
    end_time: i64,
) -> Result<u64, DbErr> {
    if user_id.is_some() {
        return raw_message_count(db, group_id, user_id, start_time, end_time).await;
    }

    let sr = stats::split_range(start_time, end_time);
    let Some((from, to)) = sr.full_days else {
        return raw_message_count(db, group_id, None, start_time, end_time).await;
    };

    let g = group_cond_sql(group_id);
    let sql = format!(
        "SELECT COALESCE(SUM(msg_count), 0) AS c FROM message_stats_daily \
         WHERE stat_date >= '{from}' AND stat_date <= '{to}'{g}"
    );
    let agg = scalar_from_sql(db, sql).await?.max(0) as u64;

    let mut total = agg;
    for (s, e) in &sr.partials {
        total += raw_message_count(db, group_id, None, *s, *e).await?;
    }
    Ok(total)
}

// ================= 原始表查询（回退路径 / 原有实现） =================

async fn raw_text_corpus(
    db: &DatabaseConnection,
    group_id: Option<i64>,
    user_id: Option<i64>,
    start_time: i64,
    end_time: i64,
) -> Result<Vec<String>, DbErr> {
    let mut query = MessageLogs::find()
        .select_only()
        .column_as(entity::Column::Tokens, "content_text")
        .filter(entity::Column::Time.gte(start_time))
        .filter(entity::Column::Time.lt(end_time))
        .filter(entity::Column::Tokens.ne(""));

    if let Some(gid) = group_id {
        query = query.filter(entity::Column::GroupId.eq(gid));
    }
    if let Some(uid) = user_id {
        query = query.filter(entity::Column::UserId.eq(uid));
    }

    let results: Vec<TextData> = query
        .limit(MAX_TEXT_CORPUS_LIMIT)
        .into_model()
        .all(db)
        .await?;
    Ok(results.into_iter().map(|d| d.content_text).collect())
}

async fn raw_user_ranking(
    db: &DatabaseConnection,
    group_id: Option<i64>,
    start_time: i64,
    end_time: i64,
    limit: u64,
) -> Result<Vec<UserRanking>, DbErr> {
    let mut query = MessageLogs::find()
        .select_only()
        .column(entity::Column::UserId)
        .column_as(Expr::col(entity::Column::SenderNick).max(), "nickname")
        .column_as(Expr::col(entity::Column::Id).count(), "count")
        .filter(entity::Column::Time.gte(start_time))
        .filter(entity::Column::Time.lt(end_time));

    if let Some(gid) = group_id {
        query = query.filter(entity::Column::GroupId.eq(gid));
    }

    query
        .group_by(entity::Column::UserId)
        .order_by_desc(Expr::custom_keyword(Alias::new("count")))
        .limit(limit.min(MAX_RANKING_LIMIT))
        .into_model::<UserRanking>()
        .all(db)
        .await
}

async fn raw_group_ranking(
    db: &DatabaseConnection,
    start_time: i64,
    end_time: i64,
    limit: u64,
) -> Result<Vec<GroupRanking>, DbErr> {
    MessageLogs::find()
        .select_only()
        .column(entity::Column::GroupId)
        .column_as(Expr::col(entity::Column::GroupName).max(), "group_name")
        .column_as(Expr::col(entity::Column::Id).count(), "count")
        .filter(entity::Column::Time.gte(start_time))
        .filter(entity::Column::Time.lt(end_time))
        .filter(entity::Column::GroupId.ne(0))
        .group_by(entity::Column::GroupId)
        .order_by_desc(Expr::custom_keyword(Alias::new("count")))
        .limit(limit.min(MAX_RANKING_LIMIT))
        .into_model::<GroupRanking>()
        .all(db)
        .await
}

async fn raw_user_group_participation_ranking(
    db: &DatabaseConnection,
    user_id: i64,
    start_time: i64,
    end_time: i64,
    limit: u64,
) -> Result<Vec<GroupRanking>, DbErr> {
    MessageLogs::find()
        .select_only()
        .column(entity::Column::GroupId)
        .column_as(Expr::col(entity::Column::GroupName).max(), "group_name")
        .column_as(Expr::col(entity::Column::Id).count(), "count")
        .filter(entity::Column::Time.gte(start_time))
        .filter(entity::Column::Time.lt(end_time))
        .filter(entity::Column::UserId.eq(user_id))
        .filter(entity::Column::GroupId.ne(0))
        .group_by(entity::Column::GroupId)
        .order_by_desc(Expr::custom_keyword(Alias::new("count")))
        .limit(limit.min(MAX_RANKING_LIMIT))
        .into_model::<GroupRanking>()
        .all(db)
        .await
}

async fn raw_user_emoji_ranking(
    db: &DatabaseConnection,
    group_id: Option<i64>,
    start_time: i64,
    end_time: i64,
    limit: u64,
) -> Result<Vec<UserRanking>, DbErr> {
    let count_expr = Func::cast_as(
        Expr::col(entity::Column::IsAnimEmoji),
        Alias::new("integer"),
    );
    let sum_expr = SimpleExpr::from(Func::sum(count_expr));

    let mut query = MessageLogs::find()
        .select_only()
        .column(entity::Column::UserId)
        .column_as(Expr::col(entity::Column::SenderNick).max(), "nickname")
        .column_as(sum_expr, "count")
        .filter(entity::Column::Time.gte(start_time))
        .filter(entity::Column::Time.lt(end_time));

    if let Some(gid) = group_id {
        query = query.filter(entity::Column::GroupId.eq(gid));
    }

    query
        .group_by(entity::Column::UserId)
        .order_by_desc(Expr::custom_keyword(Alias::new("count")))
        .limit(limit.min(MAX_RANKING_LIMIT))
        .into_model::<UserRanking>()
        .all(db)
        .await
}

async fn raw_daily_trend(
    db: &DatabaseConnection,
    group_id: Option<i64>,
    user_id: Option<i64>,
    start_time: i64,
    end_time: i64,
) -> Result<Vec<DailyTrend>, DbErr> {
    let date_expr = Expr::cust("strftime('%Y-%m-%d', datetime(time, 'unixepoch', 'localtime'))");

    let mut query = MessageLogs::find()
        .select_only()
        .column_as(date_expr.clone(), "date")
        .column_as(Expr::col(entity::Column::Id).count(), "count")
        .filter(entity::Column::Time.gte(start_time))
        .filter(entity::Column::Time.lt(end_time));

    if let Some(gid) = group_id {
        query = query.filter(entity::Column::GroupId.eq(gid));
    }
    if let Some(uid) = user_id {
        query = query.filter(entity::Column::UserId.eq(uid));
    }

    query
        .group_by(date_expr)
        .order_by_asc(Expr::custom_keyword(Alias::new("date")))
        .limit(MAX_TREND_LIMIT)
        .into_model::<DailyTrend>()
        .all(db)
        .await
}

async fn raw_daily_trend_by_group(
    db: &DatabaseConnection,
    start_time: i64,
    end_time: i64,
    by_hour: bool,
) -> Result<Vec<GroupTrend>, DbErr> {
    let time_expr = if by_hour {
        Expr::cust("strftime('%H:%M', datetime(time, 'unixepoch', 'localtime'))")
    } else {
        Expr::cust("strftime('%Y-%m-%d', datetime(time, 'unixepoch', 'localtime'))")
    };

    MessageLogs::find()
        .select_only()
        .column_as(time_expr.clone(), "date")
        .column(entity::Column::GroupName)
        .column_as(Expr::col(entity::Column::Id).count(), "count")
        .filter(entity::Column::Time.gte(start_time))
        .filter(entity::Column::Time.lt(end_time))
        .filter(entity::Column::GroupId.ne(0))
        .group_by(time_expr)
        .group_by(entity::Column::GroupId)
        .order_by_asc(Expr::custom_keyword(Alias::new("date")))
        .limit(MAX_GROUP_TREND_LIMIT)
        .into_model::<GroupTrend>()
        .all(db)
        .await
}

async fn raw_message_type_trend(
    db: &DatabaseConnection,
    group_id: Option<i64>,
    user_id: Option<i64>,
    start_time: i64,
    end_time: i64,
    by_hour: bool,
) -> Result<Vec<MessageTypeTrend>, DbErr> {
    // 聚合时间键 (按小时 或 按天)
    let time_expr = if by_hour {
        Expr::cust("strftime('%H:%M', datetime(time, 'unixepoch', 'localtime'))")
    } else {
        Expr::cust("strftime('%Y-%m-%d', datetime(time, 'unixepoch', 'localtime'))")
    };

    // 统计逻辑定义：
    // 1. 文本: length > 0
    let text_expr = Expr::cust("SUM(CASE WHEN length > 0 THEN 1 ELSE 0 END)");
    // 2. 动画表情: is_anim_emoji = true
    let anim_expr = Expr::col(entity::Column::IsAnimEmoji).sum();
    // 3. 图片: image_count - is_anim_emoji
    let image_expr = Expr::cust("SUM(image_count) - SUM(is_anim_emoji)");
    // 4. 语音: is_voice
    let voice_expr = Expr::col(entity::Column::IsVoice).sum();
    // 5. 视频: is_video
    let video_expr = Expr::col(entity::Column::IsVideo).sum();
    // 6. 表情 (Face): face_count
    let face_expr = Expr::col(entity::Column::FaceCount).sum();

    let mut query = MessageLogs::find()
        .select_only()
        .column_as(time_expr.clone(), "date")
        .column_as(text_expr, "text")
        .column_as(image_expr, "image")
        .column_as(voice_expr, "voice")
        .column_as(video_expr, "video")
        .column_as(anim_expr, "anim_emoji")
        .column_as(face_expr, "face")
        .filter(entity::Column::Time.gte(start_time))
        .filter(entity::Column::Time.lt(end_time));

    if let Some(gid) = group_id {
        query = query.filter(entity::Column::GroupId.eq(gid));
    }
    if let Some(uid) = user_id {
        query = query.filter(entity::Column::UserId.eq(uid));
    }

    query
        .group_by(time_expr)
        .order_by_asc(Expr::custom_keyword(Alias::new("date")))
        .limit(MAX_TREND_LIMIT)
        .into_model::<MessageTypeTrend>()
        .all(db)
        .await
}

async fn raw_hourly_activity(
    db: &DatabaseConnection,
    group_id: Option<i64>,
    user_id: Option<i64>,
    start_time: i64,
    end_time: i64,
) -> Result<Vec<HourlyActivity>, DbErr> {
    let mut query = MessageLogs::find()
        .select_only()
        .column_as(entity::Column::TimeHour, "hour")
        .column_as(Expr::col(entity::Column::Id).count(), "count")
        .filter(entity::Column::Time.gte(start_time))
        .filter(entity::Column::Time.lt(end_time));

    if let Some(gid) = group_id {
        query = query.filter(entity::Column::GroupId.eq(gid));
    }
    if let Some(uid) = user_id {
        query = query.filter(entity::Column::UserId.eq(uid));
    }

    query
        .group_by(entity::Column::TimeHour)
        .order_by_asc(entity::Column::TimeHour)
        .into_model::<HourlyActivity>()
        .all(db)
        .await
}

async fn raw_weekday_activity(
    db: &DatabaseConnection,
    group_id: Option<i64>,
    start_time: i64,
    end_time: i64,
) -> Result<Vec<WeekdayActivity>, DbErr> {
    let mut query = MessageLogs::find()
        .select_only()
        .column_as(entity::Column::TimeWeekday, "weekday")
        .column_as(Expr::col(entity::Column::Id).count(), "count")
        .filter(entity::Column::Time.gte(start_time))
        .filter(entity::Column::Time.lt(end_time));

    if let Some(gid) = group_id {
        query = query.filter(entity::Column::GroupId.eq(gid));
    }

    query
        .group_by(entity::Column::TimeWeekday)
        .order_by_asc(entity::Column::TimeWeekday)
        .into_model::<WeekdayActivity>()
        .all(db)
        .await
}

async fn raw_heatmap_data(
    db: &DatabaseConnection,
    group_id: Option<i64>,
    start_time: i64,
    end_time: i64,
) -> Result<Vec<HeatmapData>, DbErr> {
    let mut query = MessageLogs::find()
        .select_only()
        .column_as(entity::Column::TimeWeekday, "weekday")
        .column_as(entity::Column::TimeHour, "hour")
        .column_as(Expr::col(entity::Column::Id).count(), "count")
        .filter(entity::Column::Time.gte(start_time))
        .filter(entity::Column::Time.lt(end_time));

    if let Some(gid) = group_id {
        query = query.filter(entity::Column::GroupId.eq(gid));
    }

    query
        .group_by(entity::Column::TimeWeekday)
        .group_by(entity::Column::TimeHour)
        .into_model::<HeatmapData>()
        .all(db)
        .await
}

async fn raw_message_type_stats(
    db: &DatabaseConnection,
    group_id: Option<i64>,
    user_id: Option<i64>,
    start_time: i64,
    end_time: i64,
) -> Result<MessageTypeStats, DbErr> {
    let text_expr = Expr::cust("SUM(CASE WHEN length > 0 THEN 1 ELSE 0 END)");
    let anim_expr = Expr::col(entity::Column::IsAnimEmoji).sum();
    let image_expr = Expr::cust("SUM(image_count) - SUM(is_anim_emoji)");
    let voice_expr = Expr::col(entity::Column::IsVoice).sum();
    let video_expr = Expr::col(entity::Column::IsVideo).sum();
    let face_expr = Expr::col(entity::Column::FaceCount).sum();

    let mut query = MessageLogs::find()
        .select_only()
        .column_as(text_expr, "text")
        .column_as(image_expr, "image")
        .column_as(voice_expr, "voice")
        .column_as(video_expr, "video")
        .column_as(anim_expr, "anim_emoji")
        .column_as(face_expr, "face")
        .filter(entity::Column::Time.gte(start_time))
        .filter(entity::Column::Time.lt(end_time));

    if let Some(gid) = group_id {
        query = query.filter(entity::Column::GroupId.eq(gid));
    }
    if let Some(uid) = user_id {
        query = query.filter(entity::Column::UserId.eq(uid));
    }

    let result = query.into_model::<MessageTypeStats>().one(db).await?;

    Ok(result.unwrap_or(MessageTypeStats {
        text: 0,
        image: 0,
        voice: 0,
        video: 0,
        anim_emoji: 0,
        face: 0,
    }))
}

async fn raw_active_user_count(
    db: &DatabaseConnection,
    group_id: Option<i64>,
    start_time: i64,
    end_time: i64,
) -> Result<u64, DbErr> {
    let mut query = MessageLogs::find()
        .select_only()
        .column_as(Expr::cust("COUNT(DISTINCT user_id)"), "count")
        .filter(entity::Column::Time.gte(start_time))
        .filter(entity::Column::Time.lt(end_time));

    if let Some(gid) = group_id {
        query = query.filter(entity::Column::GroupId.eq(gid));
    }

    let result: Option<i64> = query.into_tuple().one(db).await?;
    Ok(result.unwrap_or(0) as u64)
}

async fn raw_message_count(
    db: &DatabaseConnection,
    group_id: Option<i64>,
    user_id: Option<i64>,
    start_time: i64,
    end_time: i64,
) -> Result<u64, DbErr> {
    let mut query = MessageLogs::find()
        .filter(entity::Column::Time.gte(start_time))
        .filter(entity::Column::Time.lt(end_time));

    if let Some(gid) = group_id {
        query = query.filter(entity::Column::GroupId.eq(gid));
    }
    if let Some(uid) = user_id {
        query = query.filter(entity::Column::UserId.eq(uid));
    }

    query.count(db).await
}
