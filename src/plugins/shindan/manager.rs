use crate::adapters::onebot::{LockedWriter, send_msg};
use crate::command::get_prefixes;
use crate::event::Context;
use crate::message::Message;
use anyhow::Result;
use shindan_maker::ShindanDomain;

use super::config::ShindanDefinition;
use super::executor::fetch_info;
use super::storage::Storage;
use super::utils::reply_text;

pub async fn handle_add(
    ctx: &Context,
    writer: LockedWriter,
    params: &[&str],
    domain: ShindanDomain,
    storage: &Storage,
) -> Result<()> {
    if params.len() < 2 {
        reply_text(ctx, writer, "添加神断 <命令> <ID> [模式(image/text)]").await?;
        return Ok(());
    }
    let cmd = params[0];
    let id = params[1];
    let mode = params.get(2).unwrap_or(&"image");

    if !id.chars().all(char::is_numeric) {
        reply_text(ctx, writer, "ID 必须是数字。").await?;
        return Ok(());
    }

    // Check duplicates
    let current = storage.get_shindans();
    if current.iter().any(|s| s.command == cmd) {
        reply_text(ctx, writer, "命令已存在。").await?;
        return Ok(());
    }
    if current.iter().any(|s| s.id == id) {
        reply_text(ctx, writer, "ID 已存在。").await?;
        return Ok(());
    }

    match fetch_info(domain, id).await {
        Ok((title, desc)) => {
            let s = ShindanDefinition {
                id: id.to_string(),
                title,
                description: desc,
                command: cmd.to_string(),
                mode: mode.to_string(),
            };
            storage.add_shindan(s.clone()).await;
            reply_text(ctx, writer, &format!("添加成功：{} ({})", s.title, s.id)).await?;
        }
        Err(_) => {
            reply_text(ctx, writer, "获取神断信息失败，请检查 ID。").await?;
        }
    }
    Ok(())
}

pub async fn handle_del(
    ctx: &Context,
    writer: LockedWriter,
    params: &[&str],
    storage: &Storage,
) -> Result<()> {
    if params.is_empty() {
        reply_text(ctx, writer, "删除神断 <命令>").await?;
        return Ok(());
    }
    if let Some(s) = storage.remove_shindan(params[0]).await {
        reply_text(ctx, writer, &format!("已删除：{} ({})", s.title, s.id)).await?;
    } else {
        reply_text(ctx, writer, "未找到该神断。").await?;
    }
    Ok(())
}

pub async fn handle_list(ctx: &Context, writer: LockedWriter, storage: &Storage) -> Result<()> {
    let mut list = storage.get_shindans();
    if list.is_empty() {
        reply_text(ctx, writer, "列表为空。").await?;
        return Ok(());
    }

    // 按命令排序
    list.sort_by(|a, b| a.command.cmp(&b.command));

    // 构建合并转发消息
    let mut forward_msg = Message::new();
    let bot_id = ctx.bot.login_user.id.parse::<i64>().unwrap_or(10000);
    let bot_name = ctx
        .bot
        .login_user
        .name
        .clone()
        .unwrap_or_else(|| "System Bot".to_string());

    // 分页处理
    for chunk in list.chunks(200) {
        let mut text = String::new();
        // 仅在第一个节点添加标题
        if text.is_empty() && forward_msg.0.is_empty() {
            text.push_str("=== 神断列表 ===\n");
        }

        for s in chunk {
            text.push_str(&format!("{} ", s.command));
        }

        let node = Message::new().text(text);
        forward_msg = forward_msg.node_custom(bot_id, &bot_name, node);
    }

    let msg_evt = ctx.as_message().unwrap();
    send_msg(
        ctx,
        writer,
        msg_evt.group_id(),
        Some(msg_evt.user_id()),
        forward_msg,
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed to send message: {}", e))?;

    Ok(())
}

pub async fn handle_set_mode(
    ctx: &Context,
    writer: LockedWriter,
    params: &[&str],
    storage: &Storage,
) -> Result<()> {
    if params.len() < 2 {
        reply_text(ctx, writer, "设置神断 <命令> <text/image>").await?;
        return Ok(());
    }
    let mode = params[1];
    if mode != "text" && mode != "image" {
        reply_text(ctx, writer, "模式仅支持 text 或 image。").await?;
        return Ok(());
    }
    if storage.update_mode(params[0], mode).await {
        reply_text(ctx, writer, "设置成功。").await?;
    } else {
        reply_text(ctx, writer, "未找到该神断。").await?;
    }
    Ok(())
}

pub async fn handle_modify(
    ctx: &Context,
    writer: LockedWriter,
    params: &[&str],
    storage: &Storage,
) -> Result<()> {
    if params.len() < 2 {
        reply_text(ctx, writer, "修改神断 <旧命令> <新命令>").await?;
        return Ok(());
    }
    if storage.update_command(params[0], params[1]).await {
        reply_text(ctx, writer, "修改成功。").await?;
    } else {
        reply_text(ctx, writer, "未找到该神断。").await?;
    }
    Ok(())
}

pub async fn handle_search(
    ctx: &Context,
    writer: LockedWriter,
    params: &[&str],
    storage: &Storage,
    max: u32,
) -> Result<()> {
    if params.is_empty() {
        reply_text(ctx, writer, "查找神断 <关键词>").await?;
        return Ok(());
    }
    let keyword = params[0];
    let list = storage.get_shindans();
    let matches: Vec<&str> = list
        .iter()
        .filter(|s| s.command.contains(keyword) || s.title.contains(keyword))
        .map(|s| s.command.as_str())
        .take(max as usize)
        .collect();

    if matches.is_empty() {
        reply_text(ctx, writer, "未找到相关神断。").await?;
    } else {
        reply_text(ctx, writer, &format!("搜索结果：\n{}", matches.join("\n"))).await?;
    }
    Ok(())
}

pub async fn handle_view_info(
    ctx: &Context,
    writer: LockedWriter,
    params: &[&str],
    storage: &Storage,
) -> Result<()> {
    if params.is_empty() {
        reply_text(ctx, writer, "查看神断 <命令>").await?;
        return Ok(());
    }
    let target = params[0];
    let list = storage.get_shindans();
    if let Some(s) = list.iter().find(|s| s.command == target || s.id == target) {
        let msg = format!(
            "标题：{}\nID：{}\n命令：{}\n模式：{}\n描述：{}",
            s.title, s.id, s.command, s.mode, s.description
        );
        reply_text(ctx, writer, &msg).await?;
    } else {
        reply_text(ctx, writer, "未找到。").await?;
    }
    Ok(())
}

pub async fn handle_help(ctx: &Context, writer: LockedWriter) -> Result<()> {
    let p = get_prefixes(ctx);
    let msg = format!(
        r#"神断插件指令:
{0}添加神断 <命令> <ID> [mode]
{0}删除神断 <命令>
{0}设置神断 <命令> <text/image>
{0}修改神断 <旧> <新>
{0}随机神断 [名字]
{0}神断列表
{0}查看神断 <命令>
{0}查找神断 <词>
{0}用户次数 / 用户排行榜
{0}神断次数 (热度榜)
直接输入神断命令即可触发 (支持 -t/-i 覆盖模式)"#,
        p.first().unwrap_or(&String::new()).to_string()
    );
    reply_text(ctx, writer, &msg).await?;
    Ok(())
}
