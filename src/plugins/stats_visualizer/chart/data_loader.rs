use crate::db::queries;
use crate::plugins::recorder::entity::{self, Entity as MessageLogs};
use plotters::style::RGBColor;
use sea_orm::sea_query::{Func, SimpleExpr};
use sea_orm::{
    ColumnTrait, Condition, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect, QueryTrait,
};
use std::collections::HashMap;

// 单点数据结构
#[derive(Clone)]
pub struct ChartDataPoint {
    pub label: String,
    pub value: i64,
}

// 多线数据结构
#[derive(Clone)]
pub struct SeriesData {
    pub name: String,
    pub color: RGBColor,
    pub points: Vec<ChartDataPoint>,
}

// 柱状图数据结构 (包含头像和主题色)
pub struct BarData {
    pub label: String,
    pub value: i64,
    pub user_id: Option<i64>, // 用于识别是否是发送者
    pub avatar_url: Option<String>,
    pub avatar_img: Option<image::RgbaImage>,
    pub theme_color: RGBColor, // 从头像提取的主题色
}

/// 调色板助手
fn get_palette_color(idx: usize) -> RGBColor {
    let colors = [
        RGBColor(59, 130, 246), // Blue
        RGBColor(16, 185, 129), // Green
        RGBColor(249, 115, 22), // Orange
        RGBColor(239, 68, 68),  // Red
        RGBColor(139, 92, 246), // Purple
        RGBColor(236, 72, 153), // Pink
        RGBColor(234, 179, 8),  // Yellow
        RGBColor(14, 165, 233), // Sky
    ];
    colors[idx % colors.len()]
}

/// 获取走势图数据
/// 支持：普通消息量走势 (单线)、消息类型走势 (多线)、所有群组走势 (多线)
pub async fn fetch_line_data(
    db: &DatabaseConnection,
    is_all_groups: bool,
    data_type: &str,
    query_group: Option<i64>,
    query_user: Option<i64>,
    start_time: i64,
    end_time: i64,
) -> Result<Vec<SeriesData>, String> {
    let mut series_list: Vec<SeriesData> = Vec::new();

    // 24小时内按小时聚合
    let is_hourly = end_time - start_time <= 86400;

    // 1. 所有群组今日发言走势 (多线)
    if is_all_groups && data_type == "发言" {
        let trends = queries::get_daily_trend_by_group(db, start_time, end_time, is_hourly)
            .await
            .map_err(|e| e.to_string())?;

        // 按 GroupName 分组
        let mut group_map: HashMap<String, Vec<ChartDataPoint>> = HashMap::new();
        for row in trends {
            group_map
                .entry(row.group_name)
                .or_default()
                .push(ChartDataPoint {
                    label: row.date,
                    value: row.count,
                });
        }

        // 转换为 SeriesData
        let mut raw_series: Vec<(String, i64, Vec<ChartDataPoint>)> = Vec::new();
        for (name, points) in group_map {
            let total: i64 = points.iter().map(|p| p.value).sum();
            raw_series.push((name, total, points));
        }

        // 按总消息量降序排序，并取前10名
        raw_series.sort_by_key(|b| std::cmp::Reverse(b.1));
        if raw_series.len() > 10 {
            raw_series.truncate(10);
        }

        for (i, (name, _, points)) in raw_series.into_iter().enumerate() {
            series_list.push(SeriesData {
                name,
                color: get_palette_color(i),
                points,
            });
        }

        return Ok(series_list);
    }

    // 2. 消息类型多维度走势 (多线)
    if data_type == "消息类型" {
        let trend = queries::get_message_type_trend(
            db,
            query_group,
            query_user,
            start_time,
            end_time,
            is_hourly,
        )
        .await
        .map_err(|e| e.to_string())?;

        type Extractor = fn(&queries::MessageTypeTrend) -> i64;

        // 预定义颜色和对应值提取函数
        let types: Vec<(&str, RGBColor, Extractor)> = vec![
            ("文本", RGBColor(100, 116, 139), |t| t.text),
            ("图片", RGBColor(59, 130, 246), |t| t.image),
            ("语音", RGBColor(16, 185, 129), |t| t.voice),
            ("视频", RGBColor(139, 92, 246), |t| t.video),
            ("动画表情", RGBColor(249, 115, 22), |t| t.anim_emoji),
            ("表情", RGBColor(234, 179, 8), |t| t.face),
        ];

        for (name, color, extractor) in types {
            let points: Vec<ChartDataPoint> = trend
                .iter()
                .map(|t| ChartDataPoint {
                    label: t.date.clone(),
                    value: extractor(t),
                })
                .collect();

            // 只有当该类型有数据时才添加
            if points.iter().any(|p| p.value > 0) {
                series_list.push(SeriesData {
                    name: name.to_string(),
                    color,
                    points,
                });
            }
        }

        return Ok(series_list);
    }

    // 3. 普通消息量走势 (单线)
    let mut chart_data: Vec<ChartDataPoint> = Vec::new();

    if is_hourly {
        use crate::plugins::recorder::entity::Column as RecordColumn;

        // 获取所有符合条件的时间戳
        let timestamps: Vec<i64> = MessageLogs::find()
            .filter(
                Condition::all()
                    .add(RecordColumn::Time.gte(start_time))
                    .add(RecordColumn::Time.lt(end_time)),
            )
            .apply_if(query_group, |q, g| q.filter(RecordColumn::GroupId.eq(g)))
            .apply_if(query_user, |q, u| q.filter(RecordColumn::UserId.eq(u)))
            .select_only()
            .column(RecordColumn::Time)
            .into_tuple()
            .all(db)
            .await
            .map_err(|e| e.to_string())?;

        let duration_hours = ((end_time - start_time) as f64 / 3600.0).ceil() as i64;
        let total_buckets = duration_hours.clamp(1, 24) as usize;
        let mut counts = vec![0i64; total_buckets];

        for ts in timestamps {
            let offset = ts - start_time;
            if offset >= 0 {
                let bucket = (offset / 3600) as usize;
                if bucket < total_buckets {
                    counts[bucket] += 1;
                }
            }
        }

        use chrono::{Local, TimeZone};
        for (i, count) in counts.iter().enumerate() {
            let label_time = Local
                .timestamp_opt(start_time + (i as i64 * 3600), 0)
                .unwrap();
            chart_data.push(ChartDataPoint {
                label: label_time.format("%H:%M").to_string(),
                value: *count,
            });
        }
    } else {
        // 超过24小时按天
        let trend = queries::get_daily_trend(db, query_group, query_user, start_time, end_time)
            .await
            .map_err(|e| e.to_string())?;

        chart_data = trend
            .into_iter()
            .map(|t| ChartDataPoint {
                label: t.date,
                value: t.count,
            })
            .collect();
    }

    series_list.push(SeriesData {
        name: "消息量".to_string(),
        color: RGBColor(59, 130, 246), // Primary Blue
        points: chart_data,
    });

    Ok(series_list)
}

/// 获取柱状图数据
#[allow(clippy::too_many_arguments)]
pub async fn fetch_bar_data(
    db: &DatabaseConnection,
    is_all_groups: bool,
    data_type: &str,
    query_group: Option<i64>,
    query_user: Option<i64>,
    sender_id: i64,
    start_time: i64,
    end_time: i64,
) -> Result<Vec<BarData>, String> {
    let mut bar_data: Vec<BarData> = Vec::new();
    let limit = 20;

    // 1. 消息类型统计
    if data_type == "消息类型" {
        let stats =
            queries::get_message_type_stats(db, query_group, query_user, start_time, end_time)
                .await
                .map_err(|e| e.to_string())?;

        let raw_data = vec![
            ("文本".to_string(), stats.text),
            ("图片".to_string(), stats.image),
            ("语音".to_string(), stats.voice),
            ("视频".to_string(), stats.video),
            ("动画表情".to_string(), stats.anim_emoji),
            ("表情".to_string(), stats.face),
        ];

        bar_data = raw_data
            .into_iter()
            .filter(|(_, v)| *v > 0)
            .map(|(k, v)| BarData {
                label: k,
                value: v,
                user_id: None,
                avatar_url: None,
                avatar_img: None,
                theme_color: RGBColor(59, 130, 246),
            })
            .collect();

        bar_data.sort_by_key(|b| std::cmp::Reverse(b.value));

        return Ok(bar_data);
    }

    // 2. 我的群组发言排行
    if let Some(uid) = query_user
        && query_group.is_none()
        && !is_all_groups
    {
        let ranking =
            queries::get_user_group_participation_ranking(db, uid, start_time, end_time, limit)
                .await
                .map_err(|e| e.to_string())?;

        for r in ranking {
            let url = format!("http://p.qlogo.cn/gh/{}/{}/100/", r.group_id, r.group_id);
            bar_data.push(BarData {
                label: r.group_name,
                value: r.count,
                user_id: None,
                avatar_url: Some(url),
                avatar_img: None,
                theme_color: RGBColor(59, 130, 246),
            });
        }
        return Ok(bar_data);
    }

    // 3. 所有群活跃排行
    if is_all_groups {
        let ranking = queries::get_group_ranking(db, start_time, end_time, limit)
            .await
            .map_err(|e| e.to_string())?;

        for r in ranking {
            let url = format!("http://p.qlogo.cn/gh/{}/{}/100/", r.group_id, r.group_id);
            bar_data.push(BarData {
                label: r.group_name,
                value: r.count,
                user_id: None,
                avatar_url: Some(url),
                avatar_img: None,
                theme_color: RGBColor(59, 130, 246),
            });
        }
        return Ok(bar_data);
    }

    // 4. 用户排行 (发言 或 表情包)
    let ranking = if data_type == "表情包" {
        queries::get_user_emoji_ranking(db, query_group, start_time, end_time, limit)
            .await
            .map_err(|e| e.to_string())?
    } else {
        queries::get_user_ranking(db, query_group, start_time, end_time, limit)
            .await
            .map_err(|e| e.to_string())?
    };

    let mut sender_found = false;

    for r in ranking {
        let mut label = r.nickname;
        let is_sender = r.user_id == sender_id;

        if is_sender && sender_id != 0 {
            label = format!("★ {}", label);
            sender_found = true;
        }

        let url = format!("https://q1.qlogo.cn/g?b=qq&nk={}&s=640", r.user_id);
        bar_data.push(BarData {
            label,
            value: r.count,
            user_id: Some(r.user_id),
            avatar_url: Some(url),
            avatar_img: None,
            theme_color: RGBColor(59, 130, 246),
        });
    }

    // 补位逻辑
    if !sender_found && sender_id != 0 {
        let count_query = MessageLogs::find()
            .filter(entity::Column::Time.gte(start_time))
            .filter(entity::Column::Time.lt(end_time))
            .filter(entity::Column::UserId.eq(sender_id));

        let count_query = if let Some(gid) = query_group {
            count_query.filter(entity::Column::GroupId.eq(gid))
        } else {
            count_query
        };

        let count = if data_type == "表情包" {
            use sea_orm::prelude::Expr;
            use sea_orm::sea_query::Alias;

            let count_expr = Func::cast_as(
                Expr::col(entity::Column::IsAnimEmoji),
                Alias::new("integer"),
            );
            let sum_expr = SimpleExpr::from(Func::sum(count_expr));

            let res: Option<i64> = count_query
                .select_only()
                .column_as(sum_expr, "count")
                .into_tuple()
                .one(db)
                .await
                .map_err(|e| e.to_string())?
                .unwrap_or(Some(0));
            res.unwrap_or(0)
        } else {
            count_query.count(db).await.map_err(|e| e.to_string())? as i64
        };

        if count > 0 {
            let nick_query = MessageLogs::find()
                .select_only()
                .column(entity::Column::SenderNick)
                .filter(entity::Column::UserId.eq(sender_id))
                .apply_if(query_group, |q, g| q.filter(entity::Column::GroupId.eq(g)))
                .order_by_desc(entity::Column::Time)
                .into_tuple::<String>()
                .one(db)
                .await
                .unwrap_or(None);

            let nick = nick_query.unwrap_or_else(|| sender_id.to_string());
            let label = format!("★ {}", nick);
            let url = format!("https://q1.qlogo.cn/g?b=qq&nk={}&s=640", sender_id);

            bar_data.push(BarData {
                label,
                value: count,
                user_id: Some(sender_id),
                avatar_url: Some(url),
                avatar_img: None,
                theme_color: RGBColor(59, 130, 246),
            });
        }
    }

    Ok(bar_data)
}
