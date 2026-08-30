use std::collections::HashMap;

use super::data_loader::{BarData, SeriesData};
use super::utils::{
    ColorScheme, draw_left_accent_bar, draw_rounded_rect, get_contrast_color, get_font,
    get_font_family, get_font_with_color, mix_with_white, overlay_image, save_rgba_to_base64,
    truncate_text_to_fit,
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
        let value_text = item.value.to_string();
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
        let pct_text = format!("{}%", pct_str);

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

/// 消息类型排行榜：竖排信息卡（色条 + 圆角色块图标 + 名称/数量/占比 + 进度条）。
/// 与发言/表情包的头像条形榜区分开，避免复用同一套「满色横条塞字」版式。
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
    let card_border = RGBColor(226, 232, 240);
    let text_primary = RGBColor(15, 23, 42);
    let text_secondary = RGBColor(100, 116, 139);

    let padding = 28 * s;
    let card_gap = 16 * s;
    let card_h = 100 * s;
    let card_radius = 20 * s;
    let accent_w = 10 * s;
    let icon_size = 48 * s;
    let icon_radius = 14 * s;
    let inner_pad = 18 * s;
    let progress_h = 6 * s;
    let progress_radius = 3 * s;

    let header_font_size = 20 * s;
    let title_font_size = 32 * s;
    let name_font_size = 28 * s;
    let value_font_size = 32 * s;
    let pct_font_size = 20 * s;
    let header_margin = 10 * s;
    let title_margin = 24 * s;
    let top_area =
        padding + header_font_size + header_margin + title_font_size + title_margin;

    let canvas_width = 720 * s;
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

    let mut buffer = vec![0u8; (canvas_width * canvas_height * 3) as usize];
    {
        let root = BitMapBackend::with_buffer(&mut buffer, (canvas_width, canvas_height))
            .into_drawing_area();
        root.fill(&page_bg).map_err(|e| e.to_string())?;

        let now_str = Local::now().format("%Y-%m-%d %H:%M").to_string();
        let header_style = get_font_with_color(config, header_font_size, &text_secondary)
            .pos(Pos::new(HPos::Center, VPos::Top));
        root.draw_text(
            &now_str,
            &header_style,
            (canvas_width as i32 / 2, padding as i32),
        )
        .map_err(|e| e.to_string())?;

        let title_y = padding + header_font_size + header_margin;
        let title_style = get_font_with_color(config, title_font_size, &text_primary)
            .pos(Pos::new(HPos::Center, VPos::Top));
        root.draw_text(title, &title_style, (canvas_width as i32 / 2, title_y as i32))
            .map_err(|e| e.to_string())?;

        let card_x0 = padding as i32;
        let card_x1 = (canvas_width - padding) as i32;

        for (i, item) in data.iter().enumerate() {
            let y0 = (top_area + i as u32 * (card_h + card_gap)) as i32;
            let y1 = y0 + card_h as i32;
            let inner_x0 = card_x0 + (2 * s as i32);
            let inner_y0 = y0 + (2 * s as i32);
            let inner_x1 = card_x1 - (2 * s as i32);
            let inner_y1 = y1 - (2 * s as i32);
            let inner_r = (card_radius - 2 * s) as i32;
            let accent = item.theme_color;
            let wash = mix_with_white(accent, 0.07);

            // 描边底 + 淡色卡面，类型色微染增强层次
            draw_rounded_rect(
                &root, card_x0, y0, card_x1, y1, card_radius as i32, card_border,
            )?;
            draw_rounded_rect(
                &root, inner_x0, inner_y0, inner_x1, inner_y1, inner_r, wash,
            )?;

            // 左侧类型色条：仅左上/左下圆角，右侧平切
            let ax0 = inner_x0;
            let ax1 = ax0 + accent_w as i32;
            draw_left_accent_bar(&root, ax0, inner_y0, ax1, inner_y1, inner_r, accent)?;

            // 圆角色块图标（饱和底 + 对比色字）
            let icon_x0 = ax1 + inner_pad as i32;
            let icon_y0 = inner_y0 + (16 * s as i32);
            let icon_x1 = icon_x0 + icon_size as i32;
            let icon_y1 = icon_y0 + icon_size as i32;
            let icon_fill = mix_with_white(accent, 0.90);
            draw_rounded_rect(
                &root,
                icon_x0,
                icon_y0,
                icon_x1,
                icon_y1,
                icon_radius as i32,
                icon_fill,
            )?;
            if let Some(icon_char) = item.icon_char.as_deref() {
                let icon_fg = get_contrast_color(icon_fill);
                let icon_style = get_font_with_color(config, 26 * s, &icon_fg)
                    .pos(Pos::new(HPos::Center, VPos::Center));
                root.draw_text(
                    icon_char,
                    &icon_style,
                    (
                        icon_x0 + (icon_size / 2) as i32,
                        icon_y0 + (icon_size / 2) as i32 + (2 * s as i32),
                    ),
                )
                .map_err(|e| e.to_string())?;
            }

            // 右侧：数量在上、占比在下
            let pct = if total_val > 0 {
                item.value as f64 / total_val as f64 * 100.0
            } else {
                0.0
            };
            let pct_text = if pct > 0.0 && pct < 1.0 {
                format!("{:.1}%", pct)
            } else {
                format!("{:.0}%", pct)
            };
            let value_text = item.value.to_string();

            let (vw, _) = value_font.box_size(&value_text).unwrap_or((0, 0));
            let (pw, _) = pct_font.box_size(&pct_text).unwrap_or((0, 0));
            let right_pad = inner_pad as i32;
            let stats_right = inner_x1 - right_pad;
            let stats_width = vw.max(pw);
            let stats_left = stats_right - stats_width as i32;

            let value_style = get_font_with_color(config, value_font_size, &text_primary)
                .pos(Pos::new(HPos::Right, VPos::Center));
            let pct_style = get_font_with_color(config, pct_font_size, &text_secondary)
                .pos(Pos::new(HPos::Right, VPos::Center));
            let value_y = icon_y0 + (18 * s as i32);
            let pct_y = value_y + (28 * s as i32);
            root.draw_text(&value_text, &value_style, (stats_right, value_y))
                .map_err(|e| e.to_string())?;
            root.draw_text(&pct_text, &pct_style, (stats_right, pct_y))
                .map_err(|e| e.to_string())?;

            // 类型名（图标与数值之间）
            let name_x = icon_x1 + (14 * s as i32);
            let name_max_w = (stats_left - name_x - (16 * s as i32)).max(0) as u32;
            let display_name = truncate_text_to_fit(&name_font, &item.label, name_max_w);
            let name_style = get_font_with_color(config, name_font_size, &text_primary)
                .pos(Pos::new(HPos::Left, VPos::Center));
            let name_y = icon_y0 + (icon_size / 2) as i32 + (2 * s as i32);
            if !display_name.is_empty() {
                root.draw_text(&display_name, &name_style, (name_x, name_y))
                    .map_err(|e| e.to_string())?;
            }

            // 底部进度条：更细，长度相对第一名
            let bar_x0 = icon_x0;
            let bar_x1 = stats_right;
            let bar_y1 = inner_y1 - (14 * s as i32);
            let bar_y0 = bar_y1 - progress_h as i32;
            let track = mix_with_white(accent, 0.12);
            draw_rounded_rect(
                &root,
                bar_x0,
                bar_y0,
                bar_x1,
                bar_y1,
                progress_radius as i32,
                track,
            )?;

            let ratio = item.value as f64 / max_val as f64;
            let fill_w = ((bar_x1 - bar_x0) as f64 * ratio).round() as i32;
            if fill_w > 0 {
                let fill_x1 = (bar_x0 + fill_w).max(bar_x0 + progress_radius as i32);
                draw_rounded_rect(
                    &root,
                    bar_x0,
                    bar_y0,
                    fill_x1.min(bar_x1),
                    bar_y1,
                    progress_radius as i32,
                    accent,
                )?;
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
        if !multi && max_val > 0 {
            if let Some(peak) = series_list[0].points.iter().max_by_key(|p| p.value) {
                if let Some(&i) = label_index.get(peak.label.as_str()) {
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
            }
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

