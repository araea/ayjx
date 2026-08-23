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

/// 单条指令说明：cmd 为指令（可含参数占位符），note 为用途说明（可为空）
struct Cmd {
    cmd: &'static str,
    note: &'static str,
}

/// 展开为 `&'static [Cmd]`（结构体字面量，可静态提升）
macro_rules! cmds {
    ( $( ($c:expr, $n:expr) ),* $(,)? ) => {
        &[ $( Cmd { cmd: $c, note: $n } ),* ]
    };
}

/// 每个插件的完整指令清单。仅作展示用，与各插件实际指令保持同步。
fn describe(name: &str) -> (&'static str, &'static [Cmd]) {
    match name {
        "meta_filter" => ("过滤心跳/元事件，避免噪声进入流水线", &[]),
        "logger" => ("将收到的消息打印到控制台日志", &[]),
        "recorder" => (
            "把消息记录到数据库，为词云、统计等插件提供数据源",
            &[],
        ),
        "media" => (
            "媒体与链接互转：图片/视频 ↔ 直链",
            cmds![
                ("转链接 / 看链接 / 提取地址 / url", "将图片/视频转为直链（可引用消息）"),
                ("转图片 / 预览", "将链接转为图片发送"),
                ("转视频", "将链接转为视频发送"),
            ],
        ),
        "sticker" => (
            "保存/收藏对方发送的表情或图片（需引用原消息）",
            cmds![
                ("收 / 偷 / 存表情", "引用表情/图片后收藏"),
                ("表情转图片", "引用动画表情后转为静态图片"),
            ],
        ),
        "group_title" => (
            "Bot 为群主时，给自己设置专属头衔",
            cmds![("我要头衔 <文字>", "设置 Bot 的群专属头衔")],
        ),
        "ping" => (
            "心跳测试，统计全服 Ping 次数",
            cmds![("ping", "测试 Bot 在线状态")],
        ),
        "recall" => (
            "撤回引用的消息（需引用回复使用）",
            cmds![("撤回", "引用要撤回的消息后发送")],
        ),
        "echo" => (
            "回显参数内容（支持图片等富文本）",
            cmds![("echo <内容>", "原样回显参数")],
        ),
        "repeater" => ("群聊复读机，达到阈值后自动跟读", &[]),
        "wordcloud" => (
            "根据消息记录生成词云图",
            cmds![
                ("<范围><时间>词云", "范围：本群/跨群/我的；时间：今日/昨日/本周/上周/近7天/近30天/本月/上月/今年/去年/总"),
                ("本群今日词云", "示例：本群今日"),
                ("我的总词云", "示例：个人全部"),
            ],
        ),
        "stats" => (
            "群统计图表：发言/表情/消息类型排行榜与走势，支持早中晚与周月的错峰定时推送",
            cmds![
                ("<范围><时间><类型><图表>", "范围：本群/跨群/我的/所有群；时间：今日…总；类型：发言/表情包/消息类型；图表：排行榜/走势"),
                ("本群今日发言排行榜", "示例"),
                ("本群本周发言走势", "示例"),
                ("所有群近7天发言排行榜", "示例：跨全部群"),
            ],
        ),
        "card" => (
            "解析 JSON 卡片消息为可读文本/链接（需引用角色卡图片）",
            cmds![("读卡 / 解析卡 / 看卡 / card", "引用角色卡图片后发送")],
        ),
        "gif" => (
            "GIF 工具箱：合成、变速、倒放、缩放等",
            cmds![
                ("gif帮助 / gifhelp", "GIF 工具使用帮助"),
                ("合成gif", "多张图片合成 GIF"),
                ("gif变速", "调整播放速度"),
                ("gif倒放", "反向播放"),
                ("gif信息", "查看 GIF 帧数/尺寸等"),
                ("gif缩放", "调整 GIF 尺寸"),
                ("gif旋转", "旋转角度"),
                ("gif翻转", "水平/垂直翻转"),
                ("gif拆分", "拆分为单帧图片"),
                ("gif拼图", "多张图片拼接"),
            ],
        ),
        "image_split" => (
            "将一张图按行列切片",
            cmds![("裁剪 <行>x<列> / 切图 / 分割", "如：裁剪 3x3")],
        ),
        "ciyi" => (
            "词意游戏：猜词与排行榜",
            cmds![
                ("词意帮助 / 词意指令 / 词意指令列表 / 词意帮助列表", "查看指令列表"),
                ("词意玩法 / 词意规则", "查看游戏规则"),
                ("词意猜测 [词语]", "开始猜词或提交答案"),
                ("词意榜", "当前频道排行榜"),
                ("词意全榜", "全服排行榜"),
            ],
        ),
        "webshot" => ("自动对消息中的网页链接进行截图", &[]),
        "shindan" => (
            "占卜（神断）系统：自定义与触发",
            cmds![
                ("神断帮助 / 插件指令列表", "查看指令列表"),
                ("随机神断", "随机触发一个神断"),
                ("神断列表", "已保存的神断列表"),
                ("添加神断 <关键词> <问题>", "新增自定义神断"),
                ("删除神断 <关键词>", "删除神断"),
                ("设置神断 <关键词>", "设置神断参数"),
                ("修改神断 <关键词>", "修改神断内容"),
                ("查看神断 <关键词>", "查看具体神断"),
                ("查找神断 <关键词>", "按关键词查找"),
                ("神断次数 <关键词>", "该神断被使用次数"),
                ("用户次数", "查看本人占卜次数"),
                ("用户排行榜", "占卜次数排行"),
            ],
        ),
        "oai" => (
            "多智能体对话：## 创建智能体，~对话，模型/历史管理（符号指令）",
            cmds![
                ("oai", "查看使用帮助"),
                ("oai <API地址> <密钥>", "配置模型 API"),
                ("##<名称>(<描述>) <模型> <提示词>", "创建智能体"),
                ("~<名称> <内容>", "与智能体对话"),
                ("~<名称> 停止", "停止智能体回复"),
                ("-#<名称>", "删除智能体"),
                ("~#<名称>", "复制智能体"),
                ("~=<名称> <新名>", "重命名智能体"),
                ("##:<描述...>", "自动填充智能体描述"),
                ("/#", "智能体列表"),
                ("/%", "模型列表"),
                ("-*", "清空所有公开智能体"),
                ("-*!", "清空全部（含私有）"),
            ],
        ),
        "ai_news" => (
            "AI 资讯 / 热点 / 日报 / 模型榜（数据源 AIHOT）：实时快报 + 定时推送 + 随时查询，先发排版卡片图，再补带链接的合并转发",
            cmds![
                ("ai资讯 / ai新闻", "最近 24 小时 AI 精选资讯"),
                ("ai热点", "当前 AI 热点榜 Top 10"),
                ("ai日报", "最新一期 AI 日报"),
                ("ai模型榜 / 模型排行榜", "AIHOT 大模型排行榜：共识分、评测完整度与官网参考价"),
                ("ai搜索 <关键词>", "近 7 天按关键词检索 AI 资讯"),
                ("ai推送开启 / ai推送关闭", "开启/关闭本群的资讯推送"),
                ("ai实时开启 / ai实时关闭", "本群是否接收实时快报（新资讯进池即推）"),
                ("ai推送状态", "查看推送开关、实时参数与排期"),
                ("ai推送重置", "清空本群去重记录，重新推送近期资讯"),
            ],
        ),
        "settings" => (
            "查看/修改机器人可调设置，无需编辑配置文件",
            cmds![
                ("设置", "查看全部可调项"),
                ("设置 <插件> <键>", "查看某项详情"),
                ("设置 <插件> <键> <值>", "修改并自动保存"),
            ],
        ),
        "help" => (
            "显示本帮助信息",
            cmds![
                ("help / 帮助 / 插件列表", "插件总览"),
                ("help <插件名>", "查看插件详情与全部指令"),
            ],
        ),
        "restart" => (
            "每日定时自动重启 + 内存阈值监控，防止长时间运行卡顿",
            cmds![("restart", "手动重启（需先在设置中开启 allow_manual_restart）")],
        ),
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

/// 与其他插件保持一致的分隔线
const DIVIDER: &str = "———————————————";

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
        "🧩 ayjx 插件总览\n已启用 {} / {} 个插件\n{}\n",
        enabled_count, total, DIVIDER
    ));

    // 一条两行：首行是身份与开关，次行是它到底做什么，扫读时不必在长句里找边界
    for p in plugins {
        let (desc, _) = describe(p.name);
        let mark = if is_enabled(ctx, p.name) { "✅" } else { "⬜" };
        out.push_str(&format!("{} {}（{}）\n   {}\n", mark, p.display_name, p.name, desc));
    }

    out.push_str(DIVIDER);
    out.push_str(&format!(
        "\n💡 看某个插件的全部指令：{p}help <插件名>\n   例：{p}help ai_news",
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
        return format!(
            "🔍 没有找到插件「{}」\n{}\n发送 {}help 可以查看全部插件。",
            name, DIVIDER, prefix
        );
    };

    let (desc, cmds) = describe(plugin.name);
    let status = if is_enabled(ctx, plugin.name) {
        "✅ 已启用"
    } else {
        "⬜ 已禁用"
    };

    let mut out = String::new();
    out.push_str(&format!(
        "🧩 {}（{}）\n状态：{}\n{}\n📖 {}\n{}\n",
        plugin.display_name, plugin.name, status, DIVIDER, desc, DIVIDER
    ));

    if cmds.is_empty() {
        out.push_str("该插件在后台自动工作，没有需要手动触发的指令。");
        return out;
    }

    out.push_str("⌨️ 指令\n");
    for c in cmds {
        // 符号指令（/#、~名、##、-# 等）本身是完整指令，不再拼接前缀
        let full = if c.cmd.starts_with(['/', '#', '~', '-']) {
            c.cmd.to_string()
        } else {
            format!("{}{}", prefix, c.cmd)
        };
        if c.note.is_empty() {
            out.push_str(&format!("· {}\n", full));
        } else {
            out.push_str(&format!("· {}\n   {}\n", full, c.note));
        }
    }
    out.pop();
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
