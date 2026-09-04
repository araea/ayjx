pub mod avatar;
pub mod data_loader;
pub mod renderer;
pub mod utils;

use crate::event::Context;
use crate::plugins::get_config_or_default;
use crate::plugins::stats::StatsConfig;

use self::avatar::prepare_avatars;
use self::data_loader::{BarData, SeriesData, fetch_bar_data, fetch_line_data};
use self::renderer::{draw_bar_chart, draw_line_chart, draw_message_type_ranking};

/// Guard against plotters panics when font glyphs are missing (e.g. CJK text with Latin-only font).
fn draw_with_font_panic_guard<F>(config: &StatsConfig, f: F) -> Result<String, String>
where
    F: FnOnce() -> Result<String, String>,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(e) => {
            let msg = e
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| e.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown rendering error".to_string());
            Err(format!(
                "图表渲染失败（当前字体可能不支持中文渲染。font_path='{}', font_family='{}'。请通过 font_path 指定字体文件，或安装并配置 font_family）: {}",
                config.font_path, config.font_family, msg
            ))
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn generate(
    ctx: &Context,
    is_all_groups: bool,
    data_type: &str,
    chart_type: &str,
    query_group: Option<i64>,
    query_user: Option<i64>,
    sender_id: i64,
    start_time: i64,
    end_time: i64,
    title: &str,
) -> Result<String, String> {
    let db = &ctx.db;
    let config: StatsConfig = get_config_or_default(ctx, "stats");

    // 1. 走势图
    if chart_type == "走势" {
        let chart_data: Vec<SeriesData> = fetch_line_data(
            db,
            is_all_groups,
            data_type,
            query_group,
            query_user,
            start_time,
            end_time,
        )
        .await?;

        return draw_with_font_panic_guard(&config, || {
            draw_line_chart(&config, title, chart_data)
        });
    }

    // 2. 柱状图 / 排行榜
    let mut bar_data: Vec<BarData> = fetch_bar_data(
        db,
        is_all_groups,
        data_type,
        query_group,
        query_user,
        sender_id,
        start_time,
        end_time,
    )
    .await?;

    // 3. 准备头像
    prepare_avatars(&mut bar_data).await;

    // 4. 绘图：消息类型用竖排信息卡，其余沿用头像条形榜
    draw_with_font_panic_guard(&config, || {
        if data_type == "消息类型" {
            draw_message_type_ranking(&config, title, bar_data)
        } else {
            draw_bar_chart(&config, title, bar_data)
        }
    })
}
