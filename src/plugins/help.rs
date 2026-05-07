use crate::adapters::onebot::{LockedWriter, send_msg};
use crate::command::{get_prefixes, match_command};
use crate::config::build_config;
use crate::event::Context;
use crate::message::Message;
use crate::plugins::{PluginError, get_plugins};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsScalar};
use toml::Value;

#[derive(Serialize, Deserialize)]
struct Config {
    enabled: bool,
}

pub fn default_config() -> Value {
    build_config(Config { enabled: true })
}

const TRIGGERS: &[&str] = &["help", "帮助", "插件列表"];

/// 每个插件的简介与示例指令。仅作展示用，与各插件实际指令保持同步。
fn describe(name: &str) -> (&'static str, &'static [&'static str]) {
    match name {
        "filter_meta_event" => ("过滤心跳/元事件，避免噪声进入流水线", &[]),
        "logger" => ("将收到的消息打印到控制台日志", &[]),
        "recorder" => (
            "把消息记录到数据库，为词云、统计等插件提供数据源",
            &[],
        ),
        "media_transfer" => (
            "媒体与链接互转：图片/视频 ↔ 直链",
            &["转链接", "转图片", "转视频", "预览"],
        ),
        "sticker_saver" => (
            "保存/收藏对方发送的表情或图片",
            &["收", "偷", "存表情", "表情转图片"],
        ),
        "group_self_title" => ("Bot 为群主时，给自己设置专属头衔", &["我要头衔 <文字>"]),
        "ping_pong" => ("心跳测试，统计全服 Ping 次数", &["ping"]),
        "recall" => ("撤回引用的消息（需引用回复使用）", &["撤回"]),
        "echo" => ("回显参数内容（支持图片等富文本）", &["echo <内容>"]),
        "repeater" => ("群聊复读机，达到阈值后自动跟读", &[]),
        "word_cloud" => (
            "根据消息记录生成词云图",
            &["本群今日词云", "跨群本周词云", "我的总词云"],
        ),
        "stats_visualizer" => (
            "群发言排行榜与时段走势图",
            &["本群今日发言排行榜", "本群本周发言走势"],
        ),
        "card_reader" => ("解析 JSON 卡片消息为可读文本/链接", &["读卡", "解析卡", "看卡"]),
        "gif_lab" => (
            "GIF 工具箱：合成、变速、倒放、缩放等",
            &["gif帮助", "合成gif", "gif变速", "gif缩放"],
        ),
        "image_splitter" => ("将一张图按行列切片", &["裁剪 3x3", "切图", "分割"]),
        "ciyi" => (
            "词意游戏：猜词与排行榜",
            &["词意帮助", "词意猜测", "词意榜"],
        ),
        "web_shot" => ("自动对消息中的网页链接进行截图", &[]),
        "shindan" => (
            "占卜（神断）系统：自定义与触发",
            &["神断帮助", "随机神断", "神断列表"],
        ),
        "oai" => ("接入 OpenAI 兼容大模型对话", &[]),
        "help" => ("显示本帮助信息", TRIGGERS),
        _ => ("(暂无说明)", &[]),
    }
}

fn is_enabled(ctx: &Context, name: &str) -> bool {
    let guard = ctx.config.read().unwrap();
    guard
        .plugins
        .get(name)
        .and_then(|v| v.get("enabled"))
        .and_then(|x| x.as_bool())
        .unwrap_or(false)
}

fn render_overview(ctx: &Context) -> String {
    let prefix = get_prefixes(ctx)
        .first()
        .cloned()
        .unwrap_or_else(|| "/".into());

    let plugins = get_plugins();
    let total = plugins.len();
    let enabled_count = plugins.iter().filter(|p| is_enabled(ctx, p.name)).count();

    let mut out = String::new();
    out.push_str(&format!(
        "ayjx 插件总览 ({}/{} 已启用)\n",
        enabled_count, total
    ));
    out.push_str("———————————————\n");

    for p in plugins {
        let (desc, _) = describe(p.name);
        let mark = if is_enabled(ctx, p.name) { "✅" } else { "❌" };
        out.push_str(&format!("{} {} — {}\n", mark, p.name, desc));
    }

    out.push_str("———————————————\n");
    out.push_str(&format!(
        "查看某个插件的指令: {p}help <插件名>\n例: {p}help echo",
        p = prefix
    ));
    out
}

fn render_detail(ctx: &Context, name: &str) -> String {
    let prefix = get_prefixes(ctx)
        .first()
        .cloned()
        .unwrap_or_else(|| "/".into());

    let plugins = get_plugins();
    let Some(plugin) = plugins.iter().find(|p| p.name == name) else {
        return format!("未找到插件 [{}]，可发送 {}help 查看插件列表。", name, prefix);
    };

    let (desc, cmds) = describe(plugin.name);
    let status = if is_enabled(ctx, plugin.name) {
        "已启用"
    } else {
        "已禁用"
    };

    let mut out = String::new();
    out.push_str(&format!("插件 [{}]  状态: {}\n", plugin.name, status));
    out.push_str(&format!("简介: {}\n", desc));

    if cmds.is_empty() {
        out.push_str("指令: 无 (该插件为后台运行或自动触发)");
    } else {
        out.push_str("指令:\n");
        for c in cmds {
            out.push_str(&format!("  {}{}\n", prefix, c));
        }
        out.pop();
    }
    out
}

fn extract_text_arg(args: &[simd_json::OwnedValue]) -> String {
    let mut buf = String::new();
    for seg in args {
        if seg.get_str("type") == Some("text")
            && let Some(s) = seg
                .get("data")
                .and_then(|d| d.get_str("text"))
        {
            buf.push_str(s);
        }
    }
    buf.trim().to_string()
}

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        let msg = match ctx.as_message() {
            Some(m) => m,
            None => return Ok(Some(ctx)),
        };

        for trigger in TRIGGERS {
            if let Some(matched) = match_command(&ctx, trigger) {
                let arg = extract_text_arg(&matched.args);
                let body = if arg.is_empty() {
                    render_overview(&ctx)
                } else {
                    render_detail(&ctx, &arg)
                };

                let reply = Message::new().reply(msg.message_id()).text(body);
                send_msg(&ctx, writer, msg.group_id(), Some(msg.user_id()), reply).await?;
                return Ok(None);
            }
        }

        Ok(Some(ctx))
    })
}
