// 此文件由 src/plugins.rs 包含，用于管理插件注册列表。
// 若要添加新插件，请确保 src/plugins/ 目录下存在对应的 .rs 文件，
// 并在下方列表中添加插件名称。
//
// display_name 为插件的中文显示名（面向用户的展示名）；
// 配置键与代码引用使用标识符（name）。

register_plugins!(
    meta_filter {
        display_name: "元事件过滤"
    },
    logger {
        display_name: "日志输出"
    },
    recorder {
        display_name: "消息记录",
        on_init: Some(recorder::init)
    },
    media {
        display_name: "媒体转换"
    },
    sticker {
        display_name: "表情收藏"
    },
    group_title {
        display_name: "群头衔"
    },
    ping {
        display_name: "心跳测试",
        on_init: Some(ping::init)
    },
    recall {
        display_name: "消息撤回"
    },
    echo {
        display_name: "消息回显"
    },
    repeater {
        display_name: "复读机"
    },
    wordcloud {
        display_name: "词云"
    },
    stats {
        display_name: "统计图表",
        on_connected: Some(stats::on_connected)
    },
    card {
        display_name: "卡片解析"
    },
    gif {
        display_name: "GIF 工具箱"
    },
    image_split {
        display_name: "图片切分"
    },
    ciyi {
        display_name: "词意猜词",
        on_init: Some(ciyi::init)
    },
    webshot {
        display_name: "网页截图"
    },
    shindan {
        display_name: "神断占卜",
        on_init: Some(shindan::init)
    },
    oai {
        display_name: "智能对话",
        on_init: Some(oai::init)
    },
    ai_news {
        display_name: "AI 资讯推送",
        on_init: Some(ai_news::init),
        on_connected: Some(ai_news::on_connected)
    },
    settings {
        display_name: "设置"
    },
    help {
        display_name: "帮助中心"
    },
    restart {
        display_name: "自动重启",
        on_init: Some(restart::init)
    },
);
