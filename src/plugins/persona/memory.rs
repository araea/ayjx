use super::types::Memory;

pub fn now_secs() -> i64 {
    chrono::Local::now().timestamp()
}

pub fn memory_score(m: &Memory, now: i64, half_life_days: f64) -> f32 {
    let age_days = (now - m.created_at).max(0) as f64 / 86400.0;
    let recall_age_days = (now - m.last_recalled_at).max(0) as f64 / 86400.0;
    let decay = (-(age_days / half_life_days.max(0.5)) * std::f64::consts::LN_2).exp();
    let recall_decay =
        (-(recall_age_days / half_life_days.max(0.5)) * std::f64::consts::LN_2 * 0.5).exp();
    let recall_boost = (m.recall_count as f32).min(5.0) * 0.05;
    (m.importance as f64 * decay).max(m.importance as f64 * 0.4 * recall_decay) as f32
        + recall_boost
}

pub fn rank_memories<'a>(
    mems: &'a [Memory],
    half_life_days: f64,
    keep_top: usize,
) -> Vec<&'a Memory> {
    let now = now_secs();
    let mut idx: Vec<(usize, f32)> = mems
        .iter()
        .enumerate()
        .map(|(i, m)| (i, memory_score(m, now, half_life_days)))
        .collect();
    idx.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    idx.iter()
        .take(keep_top)
        .map(|(i, _)| &mems[*i])
        .collect()
}

pub fn add_memory(mems: &mut Vec<Memory>, content: &str, max_memories: usize, half_life: f64) {
    let trimmed = content.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 120 {
        return;
    }
    if let Some(m) = mems
        .iter_mut()
        .find(|m| m.content.trim() == trimmed || similar(&m.content, trimmed))
    {
        m.last_recalled_at = now_secs();
        m.recall_count = m.recall_count.saturating_add(1);
        return;
    }
    let now = now_secs();
    mems.push(Memory {
        content: trimmed.to_string(),
        importance: 0.6,
        created_at: now,
        last_recalled_at: now,
        recall_count: 0,
    });
    prune(mems, max_memories, half_life);
}

pub fn prune(mems: &mut Vec<Memory>, max_memories: usize, half_life: f64) {
    if mems.len() <= max_memories {
        return;
    }
    let now = now_secs();
    let mut scored: Vec<(usize, f32)> = mems
        .iter()
        .enumerate()
        .map(|(i, m)| (i, memory_score(m, now, half_life)))
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let keep: std::collections::HashSet<usize> =
        scored.iter().take(max_memories).map(|(i, _)| *i).collect();
    let mut i = 0usize;
    mems.retain(|_| {
        let k = keep.contains(&i);
        i += 1;
        k
    });
}

fn similar(a: &str, b: &str) -> bool {
    let a = a.trim();
    let b = b.trim();
    if a == b {
        return true;
    }
    let (long, short) = if a.chars().count() >= b.chars().count() {
        (a, b)
    } else {
        (b, a)
    };
    if short.chars().count() < 6 {
        return false;
    }
    long.contains(short)
}
