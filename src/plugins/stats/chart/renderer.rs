use super::data_loader::{BarData, SeriesData};
use super::utils::{
    ColorScheme, get_contrast_color, get_font, get_font_family, get_font_with_color,
    mix_with_white, overlay_image, save_rgba_to_base64, truncate_text_to_fit,
};
use crate::plugins::stats::StatsConfig;
use chrono::Local;
use image::{Rgba, RgbaImage};
use plotters::prelude::*;
use plotters::style::text_anchor::{HPos, Pos, VPos};

/// 绘制水平条形图 (排行榜)
pub fn draw_bar_chart(
    config: &StatsConfig,
    title: &str,
    data: Vec<BarData>,
) -> Result<String, String> {
    if data.is_empty() {
        return Err("暂无数据".to_string());
    }

    let s = 2u32; // Scale factor

    // === 1. 预计算与布局参数 (Scaling) ===
    let padding = 24 * s;

    // 内部尺寸也随之放大
    let row_height = 50 * s;
    let font_size = 30 * s;
    let avatar_width = 50 * s;
    let gap_text = 10 * s;

    // 标题区域
    let title_font_size = 32 * s;
    let header_font_size = 20 * s;

    let header_margin = 10 * s;
    let title_margin = 15 * s; // 标题和列表的间距
    let top_area_height =
        padding + header_font_size + header_margin + title_font_size + title_margin;

    let base_bar_min_width = 150.0 * (s as f64);
    let base_bar_scale_width = 700.0 * (s as f64);
    let max_possible_bar_width = (base_bar_min_width + base_bar_scale_width) as u32;

    let max_val = data.iter().map(|d| d.value).max().unwrap_or(1).max(1);
    let total_val: i64 = data.iter().map(|d| d.value).sum();

    let font_family = get_font_family(config);
    let font_obj = (font_family, font_size).into_font();

    let mut formatted_counts = Vec::new();
    let mut max_count_text_width = 0u32;

    for item in data.iter() {
        let mut text = item.value.to_string();
        // 只有最大值才显示百分比
        if item.value == max_val && max_val > 0 {
            let pct = if total_val > 0 {
                item.value as f64 / total_val as f64 * 100.0
            } else {
                0.0
            };
            let pct_str = if pct > 0.0 && pct < 0.01 {
                "<0.01".to_string()
            } else if pct > 0.0 && pct < 1.0 {
                format!("{:.2}", pct)
            } else if pct >= 1.0 {
                format!("{:.0}", pct)
            } else {
                "0".to_string()
            };
            text = format!("{} ({}%)", text, pct_str);
        }

        let (w, _) = font_obj.box_size(&text).unwrap_or((0, 0));
        max_count_text_width = max_count_text_width.max(w);
        formatted_counts.push(text);
    }

    // 计算内容区域尺寸
    let content_width = avatar_width + max_possible_bar_width + gap_text + max_count_text_width;
    let content_height = data.len() as u32 * row_height + top_area_height;

    // 计算画布尺寸 (增加四周边距)
    let canvas_width = content_width + padding * 2;
    let canvas_height = content_height + padding; // 底部留白

    // === 2. 绘图 ===
    let mut buffer = vec![0u8; (canvas_width * canvas_height * 3) as usize];
    {
        let root = BitMapBackend::with_buffer(&mut buffer, (canvas_width, canvas_height))
            .into_drawing_area();

        root.fill(&RGBColor(255, 255, 255))
            .map_err(|e| e.to_string())?;

        let now_str = Local::now().format("%Y-%m-%d %H:%M").to_string();
        let header_style = get_font(config, header_font_size)
            .pos(Pos::new(HPos::Center, VPos::Top))
            .color(&RGBColor(100, 116, 139));
        root.draw_text(
            &now_str,
            &header_style,
            (canvas_width as i32 / 2, padding as i32),
        )
        .map_err(|e| e.to_string())?;

        // 绘制标题 (Header 下方)
        let title_y = padding + header_font_size + header_margin;
        let title_style = get_font(config, title_font_size).pos(Pos::new(HPos::Center, VPos::Top));
        root.draw_text(
            title,
            &title_style,
            (canvas_width as i32 / 2, title_y as i32),
        )
        .map_err(|e| e.to_string())?;

        // 绘制每一行
        for (i, item) in data.iter().enumerate() {
            // Y坐标向下偏移 top_area_height
            let y = top_area_height as i32 + (i as u32 * row_height) as i32;
            // X坐标向右偏移 padding + avatar_width
            let start_x = padding as i32 + avatar_width as i32;

            // 1. 计算当前条的实际宽度
            let ratio = item.value as f64 / max_val as f64;
            let current_bar_width =
                (base_bar_min_width + base_bar_scale_width * ratio).round() as i32;

            let theme_color = item.theme_color;
            let faded_color = mix_with_white(theme_color, 0.5);

            // 2. 绘制背景条 (Faded)
            let remaining_start_x = start_x + current_bar_width;
            let faded_bar_end_x = (padding + avatar_width + max_possible_bar_width) as i32;

            if remaining_start_x < faded_bar_end_x {
                root.draw(&Rectangle::new(
                    [
                        (remaining_start_x, y),
                        (faded_bar_end_x, y + row_height as i32),
                    ],
                    faded_color.filled(),
                ))
                .map_err(|e| e.to_string())?;
            }

            // 3. 绘制进度条 (Solid)
            root.draw(&Rectangle::new(
                [
                    (start_x, y),
                    (start_x + current_bar_width, y + row_height as i32),
                ],
                theme_color.filled(),
            ))
            .map_err(|e| e.to_string())?;

            // 4. 绘制昵称 (Bar 内部左侧)
            let name_color = get_contrast_color(theme_color);
            let name_style = get_font_with_color(config, font_size, &name_color)
                .pos(Pos::new(HPos::Left, VPos::Center));

            // 稍微留白
            let max_name_width = if current_bar_width > (20 * s as i32) {
                (current_bar_width - (20 * s as i32)) as u32
            } else {
                0
            };

            let display_name = truncate_text_to_fit(&font_obj, &item.label, max_name_width);

            if !display_name.is_empty() {
                root.draw_text(
                    &display_name,
                    &name_style,
                    (
                        start_x + (10 * s as i32),
                        y + (row_height / 2) as i32 + (2 * s as i32),
                    ),
                )
                .map_err(|e| e.to_string())?;
            }

            // 5. 绘制数值 (Bar 外部右侧)
            let count_text = &formatted_counts[i];
            let count_style = get_font_with_color(config, font_size, &BLACK)
                .pos(Pos::new(HPos::Left, VPos::Center));

            root.draw_text(
                count_text,
                &count_style,
                (
                    start_x + current_bar_width + (10 * s as i32),
                    y + (row_height / 2) as i32 + (2 * s as i32),
                ),
            )
            .map_err(|e| e.to_string())?;
        }

        // 6. 绘制竖线装饰
        let vertical_line_color = RGBAColor(0, 0, 0, 0.12);
        let first_line_x = padding as i32 + (200 * s as i32);
        let line_width = 3 * s as i32;
        let mut line_x = first_line_x;
        let content_end_y = top_area_height as i32 + (data.len() as u32 * row_height) as i32;

        for _ in 0..8 {
            if line_x >= (canvas_width - padding) as i32 {
                break;
            }
            root.draw(&Rectangle::new(
                [
                    (line_x, top_area_height as i32),
                    (line_x + line_width, content_end_y),
                ],
                vertical_line_color.filled(),
            ))
            .map_err(|e| e.to_string())?;
            line_x += 100 * s as i32;
        }

        root.present().map_err(|e| e.to_string())?;
    }

    // === 3. 转换并叠加头像 ===
    let mut rgba_image = RgbaImage::new(canvas_width, canvas_height);
    for y in 0..canvas_height {
        for x in 0..canvas_width {
            let idx = ((y * canvas_width + x) * 3) as usize;
            let r = buffer[idx];
            let g = buffer[idx + 1];
            let b = buffer[idx + 2];
            rgba_image.put_pixel(x, y, Rgba([r, g, b, 255]));
        }
    }

    // 叠加头像 (注意边距偏移)
    for (i, item) in data.iter().enumerate() {
        if let Some(avatar) = &item.avatar_img {
            let y_pos = top_area_height as i32 + (i as u32 * row_height) as i32;
            let x_pos = padding as i32;
            overlay_image(&mut rgba_image, avatar, x_pos, y_pos);
        }
    }

    save_rgba_to_base64(rgba_image)
}

/// 绘制折线图 (支持单线/多线)
pub fn draw_line_chart(
    config: &StatsConfig,
    title: &str,
    series_list: Vec<SeriesData>,
) -> Result<String, String> {
    if series_list.is_empty() {
        return Err("暂无数据".to_string());
    }

    let s = 2u32;
    let width = config.width * s;
    let height = config.height * s;

    let colors = ColorScheme::default();
    let mut buffer = vec![0u8; (width * height * 3) as usize];

    {
        let root = BitMapBackend::with_buffer(&mut buffer, (width, height)).into_drawing_area();

        root.fill(&colors.background).map_err(|e| e.to_string())?;

        // 1. 计算Y轴最大值 (所有Series的Max)
        let max_val = series_list
            .iter()
            .flat_map(|s| s.points.iter().map(|p| p.value))
            .max()
            .unwrap_or(10);
        let y_max = (max_val as f64 * 1.15) as i64;

        // 2. 提取X轴标签 (假设所有Series的X轴对齐，取第一个有数据的Series)
        // 实际上 Series 可能会有缺失点，这里简单假设时间点一致
        let x_labels: Vec<String> = series_list
            .first()
            .map(|s| s.points.iter().map(|p| p.label.clone()).collect())
            .unwrap_or_default();
        let point_count = x_labels.len();

        let padding_top = 25 * s;
        let header_font_size = 20 * s;
        let title_font_size = 28 * s;
        let gap = 10 * s;

        let now_str = Local::now().format("%Y-%m-%d %H:%M").to_string();
        let header_style = get_font(config, header_font_size)
            .pos(Pos::new(HPos::Center, VPos::Top))
            .color(&RGBColor(100, 116, 139));

        root.draw_text(
            &now_str,
            &header_style,
            (width as i32 / 2, padding_top as i32),
        )
        .map_err(|e| e.to_string())?;

        let title_y = padding_top + header_font_size + gap;
        let title_style = get_font(config, title_font_size).pos(Pos::new(HPos::Center, VPos::Top));
        root.draw_text(title, &title_style, (width as i32 / 2, title_y as i32))
            .map_err(|e| e.to_string())?;

        // 计算图表顶部边距
        let chart_margin_top = title_y + title_font_size + (20 * s);

        let mut chart = ChartBuilder::on(&root)
            .margin_top(chart_margin_top as i32)
            .margin_bottom(25 * s as i32)
            .margin_left(45 * s as i32)
            .margin_right(45 * s as i32)
            .x_label_area_size(45 * s as i32)
            .y_label_area_size(55 * s as i32)
            .build_cartesian_2d(0..point_count, 0..y_max)
            .map_err(|e| e.to_string())?;

        chart
            .configure_mesh()
            .light_line_style(colors.grid_line.stroke_width(s))
            .bold_line_style(colors.grid_line.stroke_width(s))
            .x_labels(point_count.min(12))
            .x_label_formatter(&|x| {
                if *x < x_labels.len() {
                    let full = &x_labels[*x];
                    if full.len() >= 10 && full.contains('-') {
                        full[5..].to_string() // remove YYYY-
                    } else {
                        full.clone()
                    }
                } else {
                    "".to_string()
                }
            })
            // 放大坐标轴字体
            .x_label_style(get_font_with_color(config, 11 * s, &colors.text_secondary))
            .y_label_style(get_font_with_color(config, 11 * s, &colors.text_secondary))
            .y_desc("数量")
            .draw()
            .map_err(|e| e.to_string())?;

        // 3. 绘制每条线
        for series in &series_list {
            let color = series.color;

            // Line
            chart
                .draw_series(LineSeries::new(
                    series.points.iter().enumerate().map(|(i, d)| (i, d.value)),
                    color.stroke_width(3 * s),
                ))
                .map_err(|e| e.to_string())?
                .label(&series.name)
                .legend(move |(x, y)| {
                    PathElement::new(vec![(x, y), (x + 20, y)], color.stroke_width(3 * s))
                });

            // Points
            chart
                .draw_series(PointSeries::of_element(
                    series.points.iter().enumerate().map(|(i, d)| (i, d.value)),
                    5 * s,
                    color.filled(),
                    &|c, sz, st| {
                        EmptyElement::at(c)
                            + Circle::new((0, 0), sz, RGBColor(255, 255, 255).filled())
                            + Circle::new((0, 0), sz.saturating_sub(2 * s), st)
                    },
                ))
                .map_err(|e| e.to_string())?;
        }

        // 4. 绘制图例
        chart
            .configure_series_labels()
            .background_style(RGBColor(255, 255, 255))
            .border_style(colors.grid_line)
            .position(SeriesLabelPosition::UpperRight)
            .label_font(get_font_with_color(config, 14 * s, &colors.text_primary))
            .draw()
            .map_err(|e| e.to_string())?;

        root.present().map_err(|e| e.to_string())?;
    }

    let mut rgba_image = RgbaImage::new(width, height);
    for y in 0..height {
        for x in 0..width {
            let idx = ((y * width + x) * 3) as usize;
            let r = buffer[idx];
            let g = buffer[idx + 1];
            let b = buffer[idx + 2];
            rgba_image.put_pixel(x, y, Rgba([r, g, b, 255]));
        }
    }

    save_rgba_to_base64(rgba_image)
}
