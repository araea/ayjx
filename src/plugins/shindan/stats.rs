use crate::adapters::onebot::LockedWriter;
use crate::event::Context;
use anyhow::Result;
use simd_json::OwnedValue;

use super::storage::Storage;
use super::utils::{get_at_target, reply_text};

pub async fn handle_user_count(
    ctx: &Context,
    writer: LockedWriter,
    params: &[&str],
    args: &[OwnedValue],
    storage: &Storage,
) -> Result<()> {
    let target_id = if let Some(qq) = get_at_target(args) {
        qq
    } else if let Some(p) = params.first() {
        if let Ok(id) = p.parse::<i64>() {
            id
        } else {
            ctx.as_message().unwrap().user_id()
        }
    } else {
        ctx.as_message().unwrap().user_id()
    };

    let count = storage.get_user_count(&ctx.db, target_id).await;
    reply_text(ctx, writer, &format!("神断次数：{}", count)).await?;
    Ok(())
}

pub async fn handle_user_rank(
    ctx: &Context,
    writer: LockedWriter,
    params: &[&str],
    storage: &Storage,
    max: u32,
) -> Result<()> {
    let limit = params
        .first()
        .and_then(|x| x.parse::<u64>().ok())
        .unwrap_or(max as u64);
    let ranks = storage.get_user_ranking(&ctx.db, limit).await;

    if ranks.is_empty() {
        reply_text(ctx, writer, "暂无数据。").await?;
        return Ok(());
    }

    let mut msg = String::from("用户排行榜：\n");
    for (i, r) in ranks.iter().enumerate() {
        msg.push_str(&format!("{}. {}: {}\n", i + 1, r.name, r.count));
    }
    reply_text(ctx, writer, msg.trim()).await?;
    Ok(())
}

pub async fn handle_item_rank(
    ctx: &Context,
    writer: LockedWriter,
    params: &[&str],
    storage: &Storage,
    max: u32,
) -> Result<()> {
    let limit = params
        .first()
        .and_then(|x| x.parse::<u64>().ok())
        .unwrap_or(max as u64);
    let ranks = storage.get_item_ranking(&ctx.db, limit).await;
    if ranks.is_empty() {
        reply_text(ctx, writer, "暂无数据。").await?;
        return Ok(());
    }

    let shindans = storage.get_shindans();

    let mut msg = String::from("神断热度榜：\n");
    for (i, r) in ranks.iter().enumerate() {
        let name = shindans
            .iter()
            .find(|s| s.id == r.shindan_id)
            .map(|s| s.command.as_str())
            .unwrap_or(&r.shindan_id);
        msg.push_str(&format!("{}. {}: {}\n", i + 1, name, r.count));
    }
    reply_text(ctx, writer, msg.trim()).await?;
    Ok(())
}
