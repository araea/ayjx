use regex::Regex;
use std::sync::OnceLock;

/// 解析 "3x3" 或 "3*3" 或 "3×3" 等格式 (大小写不敏感)
pub fn parse_grid_dim(s: &str) -> Option<(u32, u32)> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?i)(\d+)\s*[xX*×]\s*(\d+)").unwrap());
    re.captures(s).and_then(|caps| {
        let r = caps[1].parse().ok().filter(|&v| v > 0)?;
        let c = caps[2].parse().ok().filter(|&v| v > 0)?;
        Some((r, c))
    })
}

pub fn format_size(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    if bytes as f64 >= MB {
        format!("{:.2} MB", bytes as f64 / MB)
    } else {
        format!("{:.2} KB", bytes as f64 / KB)
    }
}
