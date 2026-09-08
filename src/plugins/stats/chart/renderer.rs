use std::collections::HashMap;

use super::data_loader::{BarData, SeriesData};
use super::utils::{
    ColorScheme, draw_rounded_rect, format_percent, format_thousands, get_contrast_color,
    get_font, get_font_family, get_font_with_color, mix_with_white, overlay_image,
    save_rgba_to_base64, truncate_text_to_fit,
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
    let pct_font_size = 20 * s;
    let pct_font_obj = (font_family, pct_font_size).into_font();

    // 每行都展示 "数值 + 百分比"，百分比用更小的灰色字体，提升可读性
    let mut formatted_counts: Vec<(String, String)> = Vec::new();
    let mut max_count_text_width = 0u32;

    for item in data.iter() {
        let value_text = format_thousands(item.value);
        let pct_text = format_percent(item.value, total_val);

        let (vw, _) = font_obj.box_size(&value_text).unwrap_or((0, 0));
        let (pw, _) = pct_font_obj.box_size(&pct_text).unwrap_or((0, 0));
        let total_w = vw + (8 * s) + pw;
        max_count_text_width = max_count_text_width.max(total_w);
        formatted_counts.push((value_text, pct_text));
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

            // 5. 绘制数值 (Bar 外部右侧，黑色) + 百分比 (灰色小字号)
            let (value_text, pct_text) = &formatted_counts[i];
            let count_x = start_x + current_bar_width + (10 * s as i32);
            let text_mid_y = y + (row_height / 2) as i32 + (2 * s as i32);

            let count_style = get_font_with_color(config, font_size, &BLACK)
                .pos(Pos::new(HPos::Left, VPos::Center));
            root.draw_text(value_text, &count_style, (count_x, text_mid_y))
                .map_err(|e| e.to_string())?;

            let (vw, _) = font_obj.box_size(value_text).unwrap_or((0, 0));
            let pct_style = get_font_with_color(config, pct_font_size, &RGBColor(100, 116, 139))
                .pos(Pos::new(HPos::Left, VPos::Center));
            root.draw_text(
                pct_text,
                &pct_style,
                (count_x + vw as i32 + (8 * s as i32), text_mid_y),
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

        // 7. 绘制图标徽章 (消息类型等无头像条目：主题色圆底 + 类型字符)
        for (i, item) in data.iter().enumerate() {
            if item.avatar_img.is_some() {
                continue;
            }
            let Some(icon_char) = item.icon_char.as_deref() else {
                continue;
            };

            let y = top_area_height as i32 + (i as u32 * row_height) as i32;
            let cx = padding as i32 + (avatar_width / 2) as i32;
            let cy = y + (row_height / 2) as i32;
            let radius = (avatar_width as f32 * 0.46) as i32;

            // 外圈淡色光晕 + 主题色圆底
            let halo_color = mix_with_white(item.theme_color, 0.35);
            root.draw(&Circle::new((cx, cy), radius + (3 * s as i32), halo_color.filled()))
                .map_err(|e| e.to_string())?;
            root.draw(&Circle::new((cx, cy), radius, item.theme_color.filled()))
                .map_err(|e| e.to_string())?;

            // 圆内字符 (自动根据底色选择黑/白)
            let icon_color = get_contrast_color(item.theme_color);
            let icon_style = get_font_with_color(config, 24 * s, &icon_color)
                .pos(Pos::new(HPos::Center, VPos::Center));
            root.draw_text(icon_char, &icon_style, (cx, cy + (2 * s as i32)))
                .map_err(|e| e.to_string())?;
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

/// 消息类型排行榜：标题区 + 构成条 + 竖排信息卡。
///
/// 消息类型本质上是「一个整体的构成」，而不是彼此独立的选手，所以在卡片列表之上
/// 先放一条分段构成条：一眼看到各类型占了多大一块，再往下看逐条的名次与数字。
/// 卡片内部按两行栅格排布——上行是名称与数值（同一条基线），下行是长度条；
/// 左起固定是「名次 + 类型色图标」，保证每张卡的视线落点一致。
///
/// 与发言/表情包的头像条形榜共用配色、字号层级与时间戳/标题写法，
/// 但不复用「满色横条塞字」的版式——那套是为头像行设计的。
pub fn draw_message_type_ranking(
    config: &StatsConfig,
    title: &str,
    data: Vec<BarData>,
) -> Result<String, String> {
    if data.is_empty() {
        return Err("暂无数据".to_string());
    }

    let s = 2u32;
    let page_bg = RGBColor(248, 250, 252);
    let card_face = RGBColor(255, 255, 255);
    let card_border = RGBColor(226, 232, 240);
    let card_shadow = RGBColor(228, 233, 240);
    let track_bg = RGBColor(237, 241, 246);
    let text_primary = RGBColor(15, 23, 42);
    let text_secondary = RGBColor(100, 116, 139);
    let text_muted = RGBColor(148, 163, 184);

    // —— 布局常量：一切间距都是 s 的整数倍，缩放后不会出现半像素毛边 ——
    let padding = 30 * s;
    let card_h = 96 * s;
    let card_gap = 14 * s;
    let card_radius = 22 * s;
    let border_w = 2 * s;
    let rank_col_w = 34 * s;
    let rail_pad = 10 * s;
    let icon_size = 54 * s;
    let icon_radius = 16 * s;
    let icon_gap = 18 * s;
    let inner_pad = 22 * s;
    let bar_h = 8 * s;

    let header_font_size = 20 * s;
    let title_font_size = 32 * s;
    let sub_font_size = 20 * s;
    let name_font_size = 27 * s;
    let value_font_size = 32 * s;
    let pct_font_size = 21 * s;
    let rank_font_size = 21 * s;
    let icon_font_size = 26 * s;

    let strip_h = 16 * s;
    let strip_gap = 4 * s;

    // 标题区：时间戳 → 标题 → 概览副标题 → 构成条
    let title_y = padding + header_font_size + 8 * s;
    let sub_y = title_y + title_font_size + 10 * s;
    let strip_y = sub_y + sub_font_size + 24 * s;
    let top_area = strip_y + strip_h + 28 * s;

    let canvas_width = 760 * s;
    let canvas_height = top_area
        + data.len() as u32 * card_h
        + data.len().saturating_sub(1) as u32 * card_gap
        + padding;

    let total_val: i64 = data.iter().map(|d| d.value).sum();
    let max_val = data.iter().map(|d| d.value).max().unwrap_or(1).max(1);

    let font_family = get_font_family(config);
    let name_font = (font_family, name_font_size).into_font();
    let value_font = (font_family, value_font_size).into_font();
    let pct_font = (font_family, pct_font_size).into_font();

    let card_x0 = padding as i32;
    let card_x1 = (canvas_width - padding) as i32;

    // 构成条各段宽度：先按占比分配，再把不足一格的段抬到最小可见宽度，
    // 多出来的像素从最宽的一段里扣回去，保证整条正好填满且不留缝。
    let strip_widths = allocate_strip_widths(
        &data,
        total_val,
        card_x1 - card_x0,
        strip_gap as i32,
        strip_h as i32,
    );

    let mut buffer = vec![0u8; (canvas_width * canvas_height * 3) as usize];
    {
        let root = BitMapBackend::with_buffer(&mut buffer, (canvas_width, canvas_height))
            .into_drawing_area();
        root.fill(&page_bg).map_err(|e| e.to_string())?;

        // === 标题区 ===
        let now_str = Local::now().format("%Y-%m-%d %H:%M").to_string();
        let header_style = get_font_with_color(config, header_font_size, &text_muted)
            .pos(Pos::new(HPos::Center, VPos::Top));
        root.draw_text(
            &now_str,
            &header_style,
            (canvas_width as i32 / 2, padding as i32),
        )
        .map_err(|e| e.to_string())?;

        let title_style = get_font_with_color(config, title_font_size, &text_primary)
            .pos(Pos::new(HPos::Center, VPos::Top));
        root.draw_text(title, &title_style, (canvas_width as i32 / 2, title_y as i32))
            .map_err(|e| e.to_string())?;

        let subtitle = format!(
            "共 {} 条消息 · {} 种类型",
            format_thousands(total_val),
            data.len()
        );
        let sub_style = get_font_with_color(config, sub_font_size, &text_secondary)
            .pos(Pos::new(HPos::Center, VPos::Top));
        root.draw_text(
            &subtitle,
            &sub_style,
            (canvas_width as i32 / 2, sub_y as i32),
        )
        .map_err(|e| e.to_string())?;

        // === 构成条：整体占比的一眼概览 ===
        let strip_radius = (strip_h / 2) as i32;
        let mut seg_x = card_x0;
        for (item, width) in data.iter().zip(strip_widths.iter()) {
            draw_rounded_rect(
                &root,
                seg_x,
                strip_y as i32,
                seg_x + width,
                (strip_y + strip_h) as i32,
                strip_radius,
                item.theme_color,
            )?;
            seg_x += width + strip_gap as i32;
        }

        // === 信息卡 ===
        for (i, item) in data.iter().enumerate() {
            let y0 = (top_area + i as u32 * (card_h + card_gap)) as i32;
            let y1 = y0 + card_h as i32;
            let cy = y0 + (card_h / 2) as i32;
            let accent = item.theme_color;
            let leading = i == 0;

            // 卡片：投影 → 描边 → 卡面。榜首用更明显的类型色微染做视觉锚点。
            draw_rounded_rect(
                &root,
                card_x0,
                y0 + (3 * s) as i32,
                card_x1,
                y1 + (4 * s) as i32,
                card_radius as i32,
                card_shadow,
            )?;
            draw_rounded_rect(
                &root,
                card_x0,
                y0,
                card_x1,
                y1,
                card_radius as i32,
                if leading {
                    mix_with_white(accent, 0.22)
                } else {
                    card_border
                },
            )?;
            let inner_x0 = card_x0 + border_w as i32;
            let inner_y0 = y0 + border_w as i32;
            let inner_x1 = card_x1 - border_w as i32;
            let inner_y1 = y1 - border_w as i32;
            let inner_r = (card_radius - border_w) as i32;
            let face = if leading {
                mix_with_white(accent, 0.06)
            } else {
                card_face
            };
            draw_rounded_rect(&root, inner_x0, inner_y0, inner_x1, inner_y1, inner_r, face)?;

            // 名次：卡片左起的第一段，弱化处理，只作次序参照
            let rank_color = if leading {
                accent
            } else {
                mix_with_white(accent, 0.62)
            };
            let rank_style = get_font_with_color(config, rank_font_size, &rank_color)
                .pos(Pos::new(HPos::Center, VPos::Center));
            root.draw_text(
                &(i + 1).to_string(),
                &rank_style,
                (
                    inner_x0 + (rail_pad + rank_col_w / 2) as i32,
                    cy + (2 * s) as i32,
                ),
            )
            .map_err(|e| e.to_string())?;

            // 类型图标：淡色底 + 同色字，比满色底更耐看，也不抢数值的视线
            let icon_x0 = inner_x0 + (rail_pad + rank_col_w) as i32;
            let icon_y0 = cy - (icon_size / 2) as i32;
            let icon_x1 = icon_x0 + icon_size as i32;
            let icon_fill = mix_with_white(accent, 0.16);
            draw_rounded_rect(
                &root,
                icon_x0,
                icon_y0,
                icon_x1,
                icon_y0 + icon_size as i32,
                icon_radius as i32,
                icon_fill,
            )?;
            if let Some(icon_char) = item.icon_char.as_deref() {
                let icon_style = get_font_with_color(config, icon_font_size, &accent)
                    .pos(Pos::new(HPos::Center, VPos::Center));
                root.draw_text(
                    icon_char,
                    &icon_style,
                    (
                        icon_x0 + (icon_size / 2) as i32,
                        cy + (2 * s) as i32,
                    ),
                )
                .map_err(|e| e.to_string())?;
            }

            // 上行右侧：数值（主色大字）+ 占比（次级小字），右对齐收边
            let value_text = format_thousands(item.value);
            let pct_text = format_percent(item.value, total_val);
            let (pw, _) = pct_font.box_size(&pct_text).unwrap_or((0, 0));
            let (vw, _) = value_font.box_size(&value_text).unwrap_or((0, 0));

            let stats_right = inner_x1 - inner_pad as i32;
            let top_row_y = cy - (13 * s) as i32;
            let pct_style = get_font_with_color(config, pct_font_size, &text_secondary)
                .pos(Pos::new(HPos::Right, VPos::Center));
            root.draw_text(&pct_text, &pct_style, (stats_right, top_row_y))
                .map_err(|e| e.to_string())?;
            let value_right = stats_right - pw as i32 - (14 * s) as i32;
            let value_style = get_font_with_color(config, value_font_size, &text_primary)
                .pos(Pos::new(HPos::Right, VPos::Center));
            root.draw_text(&value_text, &value_style, (value_right, top_row_y))
                .map_err(|e| e.to_string())?;

            // 上行左侧：类型名，与数值同基线；过长按可用宽度截断
            let name_x = icon_x1 + icon_gap as i32;
            let name_max_w =
                (value_right - vw as i32 - (20 * s) as i32 - name_x).max(0) as u32;
            let display_name = truncate_text_to_fit(&name_font, &item.label, name_max_w);
            if !display_name.is_empty() {
                let name_style = get_font_with_color(config, name_font_size, &text_primary)
                    .pos(Pos::new(HPos::Left, VPos::Center));
                root.draw_text(&display_name, &name_style, (name_x, top_row_y))
                    .map_err(|e| e.to_string())?;
            }

            // 下行：相对榜首的长度条，贯通名称到数值的整个宽度
            let bar_x0 = name_x;
            let bar_x1 = stats_right;
            let bar_y0 = cy + (17 * s) as i32;
            let bar_y1 = bar_y0 + bar_h as i32;
            let bar_radius = (bar_h / 2) as i32;
            draw_rounded_rect(&root, bar_x0, bar_y0, bar_x1, bar_y1, bar_radius, track_bg)?;

            if item.value > 0 {
                // 至少画成一个圆点，否则量级极小的类型在条上会完全消失
                let ratio = (item.value as f64 / max_val as f64).clamp(0.0, 1.0);
                let fill_w = ((bar_x1 - bar_x0) as f64 * ratio).round() as i32;
                let fill_x1 = (bar_x0 + fill_w.max(bar_h as i32)).min(bar_x1);
                draw_rounded_rect(&root, bar_x0, bar_y0, fill_x1, bar_y1, bar_radius, accent)?;
            }
        }

        root.present().map_err(|e| e.to_string())?;
    }

    let mut rgba_image = RgbaImage::new(canvas_width, canvas_height);
    for y in 0..canvas_height {
        for x in 0..canvas_width {
            let idx = ((y * canvas_width + x) * 3) as usize;
            rgba_image.put_pixel(
                x,
                y,
                Rgba([buffer[idx], buffer[idx + 1], buffer[idx + 2], 255]),
            );
        }
    }

    save_rgba_to_base64(rgba_image)
}

/// 构成条的分段宽度。按占比切分总宽度（已扣除段间空隙），再把小到看不见的段
/// 抬到 `min_width`，多出来的像素从当前最宽的段里逐格扣回，最后把舍入误差补给
/// 最宽的一段——这样整条始终正好填满，不会因为四舍五入在右端留下一道缝。
fn allocate_strip_widths(
    data: &[BarData],
    total_val: i64,
    strip_width: i32,
    gap: i32,
    min_width: i32,
) -> Vec<i32> {
    let count = data.len() as i32;
    let usable = (strip_width - gap * (count - 1)).max(count * min_width);
    if total_val <= 0 {
        let even = usable / count;
        return (0..count).map(|_| even).collect();
    }

    let mut widths: Vec<i32> = data
        .iter()
        .map(|d| {
            ((usable as f64 * d.value as f64 / total_val as f64).round() as i32).max(min_width)
        })
        .collect();

    let widest = |widths: &[i32]| {
        widths
            .iter()
            .enumerate()
            .max_by_key(|(_, w)| **w)
            .map(|(i, _)| i)
            .unwrap_or(0)
    };

    let mut sum: i32 = widths.iter().sum();
    while sum > usable {
        let i = widest(&widths);
        if widths[i] <= min_width {
            break;
        }
        widths[i] -= 1;
        sum -= 1;
    }
    if sum < usable {
        let i = widest(&widths);
        widths[i] += usable - sum;
    }
    widths
}

// ================= 走势图 (与排行榜统一的手绘风格) =================

/// 将坐标轴刻度取整为 1/2/5×10^k 的"美观"步长，返回 (步长, 格数)
fn nice_axis(max_val: i64) -> (f64, usize) {
    let target = (max_val.max(1) as f64) * 1.05;
    let raw_step = target / 4.0;
    let exp = raw_step.log10().floor() as i32;
    let base = 10f64.powi(exp);
    let frac = raw_step / base;
    let step = if frac <= 1.0 {
        base
    } else if frac <= 2.0 {
        2.0 * base
    } else if frac <= 5.0 {
        5.0 * base
    } else {
        10.0 * base
    };
    let steps = ((target / step).ceil() as usize).clamp(2, 6);
    (step, steps)
}

/// Y 轴刻度文本：过万缩写为 "x.x万"，其余直接显示整数
fn format_y_label(v: f64) -> String {
    let vi = v.round() as i64;
    if vi >= 10000 {
        let t = format!("{:.1}", vi as f64 / 10000.0);
        let t = t.strip_suffix(".0").unwrap_or(&t).to_string();
        format!("{}万", t)
    } else {
        vi.to_string()
    }
}

/// X 轴标签：去掉日期前缀中的年份部分 (如 "2026-08-01" -> "08-01")
fn short_x_label(label: &str) -> String {
    if label.len() >= 10 && label.contains('-') {
        label[5..].to_string()
    } else {
        label.to_string()
    }
}

/// 绘制折线图 (支持单线/多线)。
/// 与排行榜柱状图共用同一套视觉规范：白底、灰阶时间戳/标题、统一字体字号与边距、
/// 浅色网格线、白描边圆点数据点，配色取自系列颜色（消息类型与排行榜共用调色板）。
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
    let multi = series_list.len() > 1;

    // === 1. 统一 X 轴：合并所有系列的标签（按首次出现顺序去重），
    //        保证多系列的稀疏数据点也按真实时间位置对齐 ===
    let mut x_labels: Vec<String> = Vec::new();
    let mut label_index: HashMap<&str, usize> = HashMap::new();
    for series in &series_list {
        for p in &series.points {
            if let std::collections::hash_map::Entry::Vacant(e) =
                label_index.entry(p.label.as_str())
            {
                e.insert(x_labels.len());
                x_labels.push(p.label.clone());
            }
        }
    }
    let point_count = x_labels.len().max(1);

    let max_val = series_list
        .iter()
        .flat_map(|sr| sr.points.iter().map(|p| p.value))
        .max()
        .unwrap_or(0);
    let (step, steps) = nice_axis(max_val);
    let y_max = step * steps as f64;

    // === 2. 布局 (与柱状图一致的字号与边距) ===
    let padding = 24 * s;
    let header_font_size = 20 * s;
    let title_font_size = 32 * s;
    let axis_font_size = 20 * s;
    let legend_font_size = 20 * s;
    let gap = 10 * s;

    let font_family = get_font_family(config);
    let axis_font = (font_family, axis_font_size).into_font();
    let legend_font = (font_family, legend_font_size).into_font();

    // Y 轴标签宽度
    let mut y_label_w = 0u32;
    for i in 0..=steps {
        let text = format_y_label(step * i as f64);
        let (w, _) = axis_font.box_size(&text).unwrap_or((0, 0));
        y_label_w = y_label_w.max(w);
    }

    let title_y = padding + header_font_size + gap;

    // === 3. 图例布局 (多系列时)：圆点 + 名称，水平排列，超宽自动换行 ===
    let dot_r = 6 * s;
    let item_gap = 24 * s;
    let legend_row_h = 32 * s;
    let mut legend_rows: Vec<Vec<(usize, u32)>> = Vec::new();
    let mut legend_h = 0u32;

    if multi {
        let avail = width.saturating_sub(2 * padding);
        let mut row: Vec<(usize, u32)> = Vec::new();
        let mut row_w = 0u32;

        for (idx, series) in series_list.iter().enumerate() {
            let (tw, _) = legend_font.box_size(&series.name).unwrap_or((0, 0));
            let item_w = 2 * dot_r + (8 * s) + tw;
            let new_w = if row.is_empty() {
                item_w
            } else {
                row_w + item_gap + item_w
            };
            if !row.is_empty() && new_w > avail {
                legend_rows.push(std::mem::take(&mut row));
                row_w = item_w;
            } else {
                row_w = new_w;
            }
            row.push((idx, item_w));
        }
        if !row.is_empty() {
            legend_rows.push(row);
        }
        legend_h = legend_rows.len() as u32 * legend_row_h;
    }

    let legend_y = title_y + title_font_size + gap;
    let chart_top = if multi {
        legend_y + legend_h + (12 * s)
    } else {
        title_y + title_font_size + (24 * s)
    };
    let x_label_area = axis_font_size + 14 * s;
    let chart_bottom = height.saturating_sub(padding + x_label_area).max(chart_top + 40 * s);
    let chart_left = padding + y_label_w + (14 * s);
    let chart_right = width.saturating_sub(padding).max(chart_left + 40 * s);

    let chart_w = (chart_right - chart_left) as f64;
    let chart_h = (chart_bottom - chart_top) as f64;

    // X/Y 坐标换算
    let x_pos = |i: usize| chart_left as f64 + chart_w * ((i as f64 + 0.5) / point_count as f64);
    let y_pos = |v: i64| chart_bottom as f64 - chart_h * (v as f64 / y_max).clamp(0.0, 1.0);

    // === 4. 绘制 ===
    let mut buffer = vec![0u8; (width * height * 3) as usize];
    {
        let root = BitMapBackend::with_buffer(&mut buffer, (width, height)).into_drawing_area();

        root.fill(&RGBColor(255, 255, 255)).map_err(|e| e.to_string())?;

        // 4.1 时间戳 + 标题 (与柱状图一致)
        let now_str = Local::now().format("%Y-%m-%d %H:%M").to_string();
        let header_style = get_font(config, header_font_size)
            .pos(Pos::new(HPos::Center, VPos::Top))
            .color(&RGBColor(100, 116, 139));
        root.draw_text(&now_str, &header_style, (width as i32 / 2, padding as i32))
            .map_err(|e| e.to_string())?;

        let title_style =
            get_font(config, title_font_size).pos(Pos::new(HPos::Center, VPos::Top));
        root.draw_text(title, &title_style, (width as i32 / 2, title_y as i32))
            .map_err(|e| e.to_string())?;

        // 4.2 图例
        for (row_i, row) in legend_rows.iter().enumerate() {
            let row_total: u32 = row.iter().map(|(_, w)| *w).sum::<u32>() + item_gap * (row.len() as u32 - 1);
            let mut x = (width.saturating_sub(row_total)) as i32 / 2;
            let row_mid_y = legend_y + row_i as u32 * legend_row_h + legend_row_h / 2;

            for &(idx, item_w) in row {
                let color = series_list[idx].color;
                root.draw(&Circle::new((x + dot_r as i32, row_mid_y as i32), dot_r as i32, color.filled()))
                    .map_err(|e| e.to_string())?;
                let legend_style = get_font_with_color(config, legend_font_size, &colors.text_primary)
                    .pos(Pos::new(HPos::Left, VPos::Center));
                root.draw_text(
                    &series_list[idx].name,
                    &legend_style,
                    (x + 2 * dot_r as i32 + (8 * s as i32), row_mid_y as i32 + (2 * s as i32)),
                )
                .map_err(|e| e.to_string())?;
                x += item_w as i32 + item_gap as i32;
            }
        }

        // 4.3 水平网格线 + Y 轴刻度 (基线加深，与柱状图竖线装饰同色系)
        let y_label_style = get_font_with_color(config, axis_font_size, &colors.text_secondary)
            .pos(Pos::new(HPos::Right, VPos::Center));

        for i in 0..=steps {
            let v = step * i as f64;
            let y = y_pos(v.round() as i64);
            let line_color = if i == 0 {
                RGBAColor(0, 0, 0, 0.12)
            } else {
                RGBAColor(colors.grid_line.0, colors.grid_line.1, colors.grid_line.2, 1.0)
            };
            root.draw(&PathElement::new(
                vec![(chart_left as i32, y as i32), (chart_right as i32, y as i32)],
                line_color.stroke_width(2 * s),
            ))
            .map_err(|e| e.to_string())?;

            let label = format_y_label(v);
            root.draw_text(
                &label,
                &y_label_style,
                ((chart_left - (10 * s)) as i32, y as i32),
            )
            .map_err(|e| e.to_string())?;
        }

        // 4.4 X 轴标签 (过多时自动抽稀)
        let x_label_style = get_font_with_color(config, axis_font_size, &colors.text_secondary)
            .pos(Pos::new(HPos::Center, VPos::Top));
        let label_every = ((point_count as f64) / 10.0).ceil().max(1.0) as usize;

        for (i, label) in x_labels.iter().enumerate() {
            if i % label_every != 0 && i != point_count - 1 {
                continue;
            }
            let text = short_x_label(label);
            root.draw_text(
                &text,
                &x_label_style,
                (x_pos(i) as i32, (chart_bottom + (12 * s)) as i32),
            )
            .map_err(|e| e.to_string())?;
        }

        // 4.5 数据系列：面积填充(单系列) + 折线 + 白描边圆点
        for series in &series_list {
            let color = series.color;

            // 将系列数据点映射到统一 X 轴位置
            let pts: Vec<(f64, f64)> = series
                .points
                .iter()
                .filter_map(|p| label_index.get(p.label.as_str()).map(|&i| (x_pos(i), y_pos(p.value))))
                .collect();
            if pts.is_empty() {
                continue;
            }

            // 面积填充 (仅单系列，避免多系列叠加混淆)
            if !multi {
                let mut poly: Vec<(i32, i32)> = pts
                    .iter()
                    .map(|&(x, y)| (x.round() as i32, y.round() as i32))
                    .collect();
                poly.push((pts[pts.len() - 1].0.round() as i32, chart_bottom as i32));
                poly.push((pts[0].0.round() as i32, chart_bottom as i32));
                root.draw(&Polygon::new(
                    poly,
                    RGBAColor(color.0, color.1, color.2, 0.13).filled(),
                ))
                .map_err(|e| e.to_string())?;
            }

            // 折线
            let line_pts: Vec<(i32, i32)> = pts
                .iter()
                .map(|&(x, y)| (x.round() as i32, y.round() as i32))
                .collect();
            root.draw(&PathElement::new(line_pts, color.stroke_width(3 * s)))
                .map_err(|e| e.to_string())?;

            // 数据点：白底圆 + 主题色内圆
            for &(x, y) in &pts {
                let (xi, yi) = (x.round() as i32, y.round() as i32);
                root.draw(&Circle::new((xi, yi), (6 * s) as i32, RGBColor(255, 255, 255).filled()))
                    .map_err(|e| e.to_string())?;
                root.draw(&Circle::new((xi, yi), (4 * s) as i32, color.filled()))
                    .map_err(|e| e.to_string())?;
            }
        }

        // 4.6 峰值标注 (仅单系列：在最高点上方标注数值)
        if !multi
            && max_val > 0
            && let Some(peak) = series_list[0].points.iter().max_by_key(|p| p.value)
            && let Some(&i) = label_index.get(peak.label.as_str())
        {
            let px = x_pos(i);
            let py = y_pos(peak.value);
            let text = peak.value.to_string();
            let peak_style = get_font_with_color(config, axis_font_size, &colors.text_primary)
                .pos(Pos::new(HPos::Center, VPos::Bottom));
            root.draw_text(
                &text,
                &peak_style,
                (px.round() as i32, (py - (10 * s) as f64).round() as i32),
            )
            .map_err(|e| e.to_string())?;
        }

        root.present().map_err(|e| e.to_string())?;
    }

    // === 5. RGB -> RGBA 并编码 ===
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::stats::chart::data_loader::message_type_style;

    fn sample(label: &str, value: i64) -> BarData {
        let (color, icon) = message_type_style(label);
        BarData {
            label: label.to_string(),
            value,
            user_id: None,
            avatar_url: None,
            avatar_img: None,
            theme_color: color,
            icon_char: Some(icon.to_string()),
        }
    }

    #[test]
    fn strip_segments_fill_the_width_exactly() {
        let data = vec![
            sample("文本", 8_120),
            sample("图片", 2_004),
            sample("表情", 1),
        ];
        let total: i64 = data.iter().map(|d| d.value).sum();
        let (width, gap, min) = (1400, 8, 16);
        let widths = allocate_strip_widths(&data, total, width, gap, min);

        assert_eq!(widths.len(), 3);
        let laid_out: i32 = widths.iter().sum::<i32>() + gap * (data.len() as i32 - 1);
        assert_eq!(laid_out, width, "分段加空隙应正好铺满整条");
        assert!(widths.iter().all(|w| *w >= min), "极小占比也要看得见");
        assert!(widths[0] > widths[1] && widths[1] > widths[2]);
    }

    #[test]
    fn strip_handles_single_and_empty_totals() {
        let one = vec![sample("文本", 5)];
        assert_eq!(allocate_strip_widths(&one, 5, 600, 8, 16), vec![600]);

        // 全为 0 时不做除零，均分即可
        let zeros = vec![sample("文本", 0), sample("图片", 0)];
        let widths = allocate_strip_widths(&zeros, 0, 600, 8, 16);
        assert_eq!(widths, vec![296, 296]);
    }

    #[test]
    fn message_type_ranking_renders_a_png() {
        let config = StatsConfig::default();
        let data = vec![
            sample("文本", 8_120),
            sample("图片", 2_004),
            sample("动画表情", 947),
            sample("表情", 133),
            sample("语音", 21),
            sample("视频", 2),
        ];
        let out = draw_message_type_ranking(&config, "本群 今日 消息类型 排行榜", data)
            .expect("消息类型排行榜应当能渲染");
        save_preview(&out, "AYJX_CHART_PREVIEW");
    }

    #[test]
    fn bar_chart_still_renders_with_the_shared_number_formatting() {
        let config = StatsConfig::default();
        let data = vec![
            sample("文本", 12_345),
            sample("图片", 678),
            sample("语音", 3),
        ];
        let out = draw_bar_chart(&config, "本群 今日 发言 排行榜", data)
            .expect("发言排行榜应当能渲染");
        save_preview(&out, "AYJX_CHART_PREVIEW_BAR");
    }

    /// 断言产物是 PNG；设了环境变量时顺手落盘一份，方便人工看效果。
    fn save_preview(out: &str, env_key: &str) {
        assert!(out.starts_with("base64://"));
        if let Ok(path) = std::env::var(env_key) {
            use base64::{Engine as _, engine::general_purpose};
            let bytes = general_purpose::STANDARD
                .decode(out.trim_start_matches("base64://"))
                .unwrap();
            std::fs::write(path, bytes).unwrap();
        }
    }
}
