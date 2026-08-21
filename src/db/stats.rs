//! 消息统计聚合表：`message_stats_daily`（群×日）与 `message_user_stats_daily`（群×用户×日）。
//!
//! ## 为什么需要聚合表
//! 基准测试（300 万行原始消息、最活跃群 73 万行/180 天）表明：
//! - 纯计数（`COUNT(*)`）可走 `idx_records_group_time` 覆盖索引，只数索引条目，不读消息行；
//! - 但消息类型统计、每日走势、各类排行榜等聚合查询必须**扫描区间内全部原始消息行**，
//!   "今年/总"范围下单次可达 0.5~1 秒。
//!
//! 聚合表按"群×日"预聚合（100 群 × 180 天仅约 1.8 万行），查询某群今年消息数
//! 只需读取该群 ≤366 行聚合记录求和，不再扫描原始消息。
//!
//! ## 数据同步机制（与 message_records 保持一致）
//! 1. **写入**：recorder 在消息入库的**同一事务**内 UPSERT 当日聚合行（计数 +1、分类计数累加），
//!    两者要么同时成功要么同时回滚，不存在中间态；
//! 2. **删除**：项目中对 message_records 的唯一删除路径是 recorder 每日保留期批量清理
//!    （无逐条删除接口；消息撤回走 OneBot API，不删除本地记录）。聚合行**不随原始数据删除**，
//!    历史统计（如"去年消息数"）在原始消息过期后依然可查；
//! 3. **自愈**：每日 4:00 清理任务开始前、以及每次进程启动时，重建近 7 天的聚合行，
//!    修复异常场景（手工改库、旧版本数据、时钟回拨）导致的漂移。
//!
//! ## 时间口径
//! `stat_date` 为本地时区自然日（'YYYY-MM-DD'）。聚合行按消息自身的 `time` 字段归属日期，
//! 晚到的历史消息会正确累计到其所属日期的聚合行上。

use chrono::{Duration, Local, NaiveDate, TimeZone};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, Statement};

use crate::{info, warn};

/// 自愈重建的天数窗口
pub const SELF_HEAL_DAYS: i64 = 7;

// ================= 初始化与维护 =================

/// 初始化聚合表：建表 + 首次全量回填（或启动自愈）。
/// 依赖 message_records 表已存在（由 recorder::init 先行创建）。
pub async fn init(db: &DatabaseConnection) -> Result<(), DbErr> {
    create_tables(db).await?;

    let stats_rows = scalar_i64(db, "SELECT COUNT(*) AS c FROM message_stats_daily").await?;
    let raw_rows = scalar_i64(db, "SELECT COUNT(*) AS c FROM message_records").await?;
    if raw_rows == 0 {
        return Ok(());
    }

    if stats_rows == 0 {
        // 升级场景：已有历史消息但聚合表为空 → 一次性全量回填
        info!(target: "Database/Stats", "检测到 {} 条历史消息，开始回填统计聚合表...", raw_rows);
        backfill_all(db).await?;
        info!(target: "Database/Stats", "统计聚合表回填完成。");
    } else {
        // 常规启动：重建近 7 天聚合行，修复可能的漂移
        self_heal_recent(db).await?;
    }
    Ok(())
}

async fn create_tables(db: &DatabaseConnection) -> Result<(), DbErr> {
    let backend = db.get_database_backend();
    let stmts = [
        // 群 × 日 聚合（含 6 类消息类型计数，口径与原实时查询完全一致：
        // image = SUM(image_count) - SUM(is_anim_emoji)，此处直接存净额）
        r#"CREATE TABLE IF NOT EXISTS message_stats_daily (
            group_id      INTEGER NOT NULL,
            stat_date     TEXT    NOT NULL,
            group_name    TEXT    NOT NULL DEFAULT '',
            msg_count     INTEGER NOT NULL DEFAULT 0,
            text_count    INTEGER NOT NULL DEFAULT 0,
            image_count   INTEGER NOT NULL DEFAULT 0,
            voice_count   INTEGER NOT NULL DEFAULT 0,
            video_count   INTEGER NOT NULL DEFAULT 0,
            anim_emoji_count INTEGER NOT NULL DEFAULT 0,
            face_count    INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (group_id, stat_date)
        )"#,
        // 群 × 用户 × 日 聚合（服务发言榜/表情包榜/我的群排行等）
        r#"CREATE TABLE IF NOT EXISTS message_user_stats_daily (
            group_id      INTEGER NOT NULL,
            stat_date     TEXT    NOT NULL,
            user_id       INTEGER NOT NULL,
            nick          TEXT    NOT NULL DEFAULT '',
            group_name    TEXT    NOT NULL DEFAULT '',
            msg_count     INTEGER NOT NULL DEFAULT 0,
            anim_emoji_count INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (group_id, stat_date, user_id)
        )"#,
        // 覆盖"按用户查日期范围"的查询（我的群组参与排行）
        "CREATE INDEX IF NOT EXISTS idx_user_stats_user_date \
         ON message_user_stats_daily (user_id, stat_date, group_id)",
    ];

    for sql in stmts {
        db.execute(Statement::from_string(backend, sql.to_owned()))
            .await?;
    }
    Ok(())
}

/// 全量回填（仅在聚合表为空时执行一次）
async fn backfill_all(db: &DatabaseConnection) -> Result<(), DbErr> {
    let backend = db.get_database_backend();

    db.execute(Statement::from_string(
        backend,
        r#"INSERT INTO message_stats_daily
           (group_id, stat_date, group_name, msg_count, text_count, image_count,
            voice_count, video_count, anim_emoji_count, face_count)
           SELECT group_id,
                  strftime('%Y-%m-%d', datetime(time, 'unixepoch', 'localtime')),
                  MAX(group_name),
                  COUNT(*),
                  SUM(CASE WHEN length > 0 THEN 1 ELSE 0 END),
                  SUM(image_count) - SUM(is_anim_emoji),
                  SUM(is_voice),
                  SUM(is_video),
                  SUM(is_anim_emoji),
                  SUM(face_count)
           FROM message_records
           GROUP BY group_id, 2"#
            .to_owned(),
    ))
    .await?;

    db.execute(Statement::from_string(
        backend,
        r#"INSERT INTO message_user_stats_daily
           (group_id, stat_date, user_id, nick, group_name, msg_count, anim_emoji_count)
           SELECT group_id,
                  strftime('%Y-%m-%d', datetime(time, 'unixepoch', 'localtime')),
                  user_id,
                  MAX(sender_nick),
                  MAX(group_name),
                  COUNT(*),
                  SUM(is_anim_emoji)
           FROM message_records
           GROUP BY group_id, 2, user_id"#
            .to_owned(),
    ))
    .await?;

    Ok(())
}

/// 自愈：重建近 SELF_HEAL_DAYS 天的聚合行。
/// 由每日 4:00 保留期清理任务与进程启动时调用。
pub async fn self_heal_recent(db: &DatabaseConnection) -> Result<(), DbErr> {
    let today = Local::now().date_naive();
    let from = today - Duration::days(SELF_HEAL_DAYS - 1);
    match rebuild_range(db, from, today).await {
        Ok(n) => {
            if n > 0 {
                info!(target: "Database/Stats", "统计聚合自愈完成，重建 {} 天聚合行。", n);
            }
            Ok(())
        }
        Err(e) => {
            warn!(target: "Database/Stats", "统计聚合自愈失败: {}", e);
            Err(e)
        }
    }
}

/// 重建 [from, to] 闭区间内的聚合行（返回重建的天数）。
/// 只重建原始数据仍完整覆盖的日期：保留期之外的聚合行是"冻结的历史"，
/// 原始消息已删除，不能被重建，也不能被误删。
async fn rebuild_range(db: &DatabaseConnection, from: NaiveDate, to: NaiveDate) -> Result<i64, DbErr> {
    let raw_rows = scalar_i64(db, "SELECT COUNT(*) AS c FROM message_records").await?;
    if raw_rows == 0 {
        return Ok(0);
    }
    let min_time = scalar_i64(db, "SELECT MIN(time) AS c FROM message_records").await?;

    // 原始数据最早只能覆盖到 min_time 所在日期（且当日可能不完整），
    // 从能被原始数据完整覆盖的第一天开始重建，避免误删冻结行。
    let min_date = date_of_ts(min_time);
    let min_cover_from = if min_time <= midnight_of(min_date) {
        min_date
    } else {
        min_date + Duration::days(1)
    };
    let eff_from = min_cover_from.max(from);
    if eff_from > to {
        return Ok(0);
    }

    let backend = db.get_database_backend();
    let f = eff_from.format("%Y-%m-%d").to_string();
    let t = to.format("%Y-%m-%d").to_string();
    let t_from = midnight_of(eff_from);
    let t_to = midnight_of(to) + 86_400;

    db.execute(Statement::from_string(
        backend,
        format!("DELETE FROM message_stats_daily WHERE stat_date >= '{f}' AND stat_date <= '{t}'"),
    ))
    .await?;
    db.execute(Statement::from_string(
        backend,
        format!("DELETE FROM message_user_stats_daily WHERE stat_date >= '{f}' AND stat_date <= '{t}'"),
    ))
    .await?;

    db.execute(Statement::from_string(
        backend,
        format!(
            r#"INSERT INTO message_stats_daily
               (group_id, stat_date, group_name, msg_count, text_count, image_count,
                voice_count, video_count, anim_emoji_count, face_count)
               SELECT group_id,
                      strftime('%Y-%m-%d', datetime(time, 'unixepoch', 'localtime')),
                      MAX(group_name), COUNT(*),
                      SUM(CASE WHEN length > 0 THEN 1 ELSE 0 END),
                      SUM(image_count) - SUM(is_anim_emoji),
                      SUM(is_voice), SUM(is_video), SUM(is_anim_emoji), SUM(face_count)
               FROM message_records
               WHERE time >= {t_from} AND time < {t_to}
               GROUP BY group_id, 2"#
        ),
    ))
    .await?;

    db.execute(Statement::from_string(
        backend,
        format!(
            r#"INSERT INTO message_user_stats_daily
               (group_id, stat_date, user_id, nick, group_name, msg_count, anim_emoji_count)
               SELECT group_id,
                      strftime('%Y-%m-%d', datetime(time, 'unixepoch', 'localtime')),
                      user_id, MAX(sender_nick), MAX(group_name), COUNT(*), SUM(is_anim_emoji)
               FROM message_records
               WHERE time >= {t_from} AND time < {t_to}
               GROUP BY group_id, 2, user_id"#
        ),
    ))
    .await?;

    Ok((to - eff_from).num_days() + 1)
}

// ================= 写入同步 =================

/// 单条消息的聚合增量
pub struct MessageStatsDelta {
    pub group_id: i64,
    pub group_name: String,
    pub user_id: i64,
    pub nick: String,
    pub time: i64,
    pub length: i32,
    pub image_count: i32,
    pub is_anim_emoji: bool,
    pub is_voice: bool,
    pub is_video: bool,
    pub face_count: i32,
}

/// 消息入库后在**同一事务内**调用：将计数累加到当日聚合行。
/// 接受 `ConnectionTrait` 以便复用外层事务（与消息插入原子绑定）。
pub async fn upsert_message_stats<C>(db: &C, d: &MessageStatsDelta) -> Result<(), DbErr>
where
    C: ConnectionTrait,
{
    let backend = db.get_database_backend();
    let date = date_str_of(d.time);

    // 口径与原实时查询保持一致
    let text = i64::from(d.length > 0);
    let anim = i64::from(d.is_anim_emoji);
    let image = d.image_count as i64 - anim;
    let voice = i64::from(d.is_voice);
    let video = i64::from(d.is_video);
    let face = d.face_count as i64;

    let stmt = Statement::from_sql_and_values(
        backend,
        r#"INSERT INTO message_stats_daily
           (group_id, stat_date, group_name, msg_count, text_count, image_count,
            voice_count, video_count, anim_emoji_count, face_count)
           VALUES (?, ?, ?, 1, ?, ?, ?, ?, ?, ?)
           ON CONFLICT (group_id, stat_date) DO UPDATE SET
             group_name = excluded.group_name,
             msg_count = message_stats_daily.msg_count + 1,
             text_count = message_stats_daily.text_count + excluded.text_count,
             image_count = message_stats_daily.image_count + excluded.image_count,
             voice_count = message_stats_daily.voice_count + excluded.voice_count,
             video_count = message_stats_daily.video_count + excluded.video_count,
             anim_emoji_count = message_stats_daily.anim_emoji_count + excluded.anim_emoji_count,
             face_count = message_stats_daily.face_count + excluded.face_count"#,
        vec![
            d.group_id.into(),
            date.clone().into(),
            d.group_name.clone().into(),
            text.into(),
            image.into(),
            voice.into(),
            video.into(),
            anim.into(),
            face.into(),
        ],
    );
    db.execute(stmt).await?;

    let stmt = Statement::from_sql_and_values(
        backend,
        r#"INSERT INTO message_user_stats_daily
           (group_id, stat_date, user_id, nick, group_name, msg_count, anim_emoji_count)
           VALUES (?, ?, ?, ?, ?, 1, ?)
           ON CONFLICT (group_id, stat_date, user_id) DO UPDATE SET
             nick = excluded.nick,
             group_name = excluded.group_name,
             msg_count = message_user_stats_daily.msg_count + 1,
             anim_emoji_count = message_user_stats_daily.anim_emoji_count + excluded.anim_emoji_count"#,
        vec![
            d.group_id.into(),
            date.into(),
            d.user_id.into(),
            d.nick.clone().into(),
            d.group_name.clone().into(),
            anim.into(),
        ],
    );
    db.execute(stmt).await?;

    Ok(())
}

// ================= 时间范围切分 =================

/// 将时间戳区间切分为"完整自然日 + 非整日边界片段"。
///
/// 完整自然日部分走聚合表（O(天数) 行）；非整日边界（如"近7天"的起止时刻落在
/// 一天中间）回退原始表做小范围查询。所有查询区间均由 `db::utils::get_time_range`
/// 生成，起点为本地零点或当前时刻，因此边界片段最多 2 个（首日头、末日尾）。
pub struct SplitRange {
    /// 完整覆盖的自然日范围（闭区间，'YYYY-MM-DD'）；None 表示无完整自然日
    pub full_days: Option<(String, String)>,
    /// 非整日边界片段（时间戳区间，左闭右开），最多 2 个
    pub partials: Vec<(i64, i64)>,
}

pub fn split_range(start: i64, end: i64) -> SplitRange {
    if end <= start {
        return SplitRange { full_days: None, partials: Vec::new() };
    }

    let d0 = date_of_ts(start);
    let d1 = date_of_ts(end - 1);
    let m0 = midnight_of(d0);

    // start >= m0 恒成立；相等表示起点恰为零点，首日完整
    let full_from = if start <= m0 { d0 } else { d0 + Duration::days(1) };
    // end 为右开边界；末日 d1 完整当且仅当 end 不早于次日零点
    let full_to = if end >= midnight_of(d1) + 86_400 {
        d1
    } else {
        d1 - Duration::days(1)
    };

    if full_from > full_to {
        // 区间不足两个零点边界（如"今日"），整体回退原始表
        return SplitRange { full_days: None, partials: vec![(start, end)] };
    }

    let mut partials = Vec::new();
    let head_end = midnight_of(full_from);
    if start < head_end {
        partials.push((start, head_end));
    }
    let tail_start = midnight_of(full_to) + 86_400;
    if end > tail_start {
        partials.push((tail_start, end));
    }

    SplitRange {
        full_days: Some((
            full_from.format("%Y-%m-%d").to_string(),
            full_to.format("%Y-%m-%d").to_string(),
        )),
        partials,
    }
}

// ================= 内部工具 =================

async fn scalar_i64<C>(db: &C, sql: &str) -> Result<i64, DbErr>
where
    C: ConnectionTrait,
{
    let stmt = Statement::from_string(db.get_database_backend(), sql.to_owned());
    let row = db.query_one(stmt).await?;
    match row {
        Some(r) => Ok(r.try_get::<i64>("", "c")?),
        None => Ok(0),
    }
}

fn date_of_ts(ts: i64) -> NaiveDate {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|dt| dt.date_naive())
        .unwrap_or_default()
}

fn midnight_of(d: NaiveDate) -> i64 {
    d.and_hms_opt(0, 0, 0)
        .and_then(|ndt| Local.from_local_datetime(&ndt).single())
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

fn date_str_of(ts: i64) -> String {
    date_of_ts(ts).format("%Y-%m-%d").to_string()
}
