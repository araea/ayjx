use crate::plugins::recorder::entity::{self, Entity as MessageLogs};
use sea_orm::sea_query::{Alias, Expr, Func, SimpleExpr};
use sea_orm::{
    ColumnTrait, DatabaseConnection, DbErr, EntityTrait, FromQueryResult, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect,
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

// ================= 查询函数 =================

/// 获取指定时间范围内的纯文本内容列表
pub async fn get_text_corpus(
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

/// 获取活跃用户排行（龙王榜）
pub async fn get_user_ranking(
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

/// 获取群组活跃排行
pub async fn get_group_ranking(
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

/// 获取用户参与的群组排行 ("我的...排行")
pub async fn get_user_group_participation_ranking(
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

/// 获取用户表情包使用量排行
pub async fn get_user_emoji_ranking(
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

/// 获取每日消息量走势 (总)
pub async fn get_daily_trend(
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

/// 获取各群每日消息量走势 (用于"所有群...走势")
pub async fn get_daily_trend_by_group(
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

/// 获取消息类型走势 (多维度)
pub async fn get_message_type_trend(
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

/// 获取24小时活跃时段分布
pub async fn get_hourly_activity(
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

/// 获取星期活跃分布
pub async fn get_weekday_activity(
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

/// 获取 星期×小时 的热力分布数据
pub async fn get_heatmap_data(
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

/// 获取消息类型统计 (总计)
pub async fn get_message_type_stats(
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

/// 获取指定条件下的活跃用户数量 (DISTINCT user_id)
pub async fn get_active_user_count(
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

/// 获取指定条件下的消息数量
pub async fn get_message_count(
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
