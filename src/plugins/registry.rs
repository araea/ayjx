// 插件注册表：框架里唯一需要为「新增一个插件」改动的文件。
//
// 一条记录同时决定三件事：
//   1. 模块声明与流水线顺序 —— 宏按此顺序生成 `pub mod` 与执行序列；
//   2. 生命周期钩子 —— on_init / on_connected；
//   3. 帮助元数据 —— section / summary / commands，`/help` 与 `/ctl` 直接读，
//      不需要再去别处补一份清单。
//
// 字段说明：
//   display_name  中文显示名（面向用户）；配置键与代码引用用标识符（模块名）
//   section       帮助总览的分区代号，取值见 `help::SECTIONS`；省略即「其他」
//   summary       一句话说明；`cargo test` 会检查每个插件都写了
//   commands      指令清单，`cmds![("指令", "说明"), ...]`；后台插件省略
//
// 指令写法：`/` 前缀由框架按配置拼上，这里只写指令本体；
// 符号指令（`/#`、`~名`、`##`、`-#`、`-*`）自带写法，不会被再拼前缀。
// 别名用 ` / ` 分隔，渲染时首个抬为主指令，其余降级为别名。
// 参数占位符统一 `<必填>` / `[可选]`，图上会单独着色。

register_plugins!(
    meta_filter {
        display_name: "元事件过滤",
        section: "system",
        summary: "过滤心跳/元事件，避免噪声进入流水线"
    },
    ctl {
        display_name: "插件控制",
        section: "system",
        summary: "统一管理全部插件的全局开关与配置；修改仅限 ctl.admins，初始化与排期修改需重启",
        commands: cmds![
            ("ctl / 控制 / 插件", "查看完整用法和示例"),
            ("ctl list [on|off|关键词]", "查看全局状态及待重启提示"),
            ("ctl on <插件...>", "批量开启，支持英文名及中文显示名"),
            ("ctl off <插件...>", "批量关闭；保留 ctl 管理入口"),
            ("ctl show <插件> [路径]", "查看配置，密钥隐藏"),
            ("ctl defaults <插件> [路径]", "查看默认配置"),
            ("ctl set <插件> <路径> <值>", "校验后保存；支持数组、表及点分路径"),
            ("ctl reset <插件> [路径] --confirm", "恢复默认；整插件重置保留开关与管理员"),
            ("ctl diff <插件>", "比较当前配置与默认值"),
        ]
    },
    logger {
        display_name: "日志输出",
        section: "system",
        summary: "将收到的消息打印到控制台日志"
    },
    recorder {
        display_name: "消息记录",
        section: "insight",
        summary: "把消息记录到数据库，为词云、统计等插件提供数据源",
        on_init: Some(recorder::init)
    },
    media {
        display_name: "媒体转换",
        section: "message",
        summary: "媒体与链接互转：图片/视频 ↔ 直链",
        commands: cmds![
            ("转链接 / 看链接 / 提取地址 / url", "将图片/视频转为直链（可引用消息）"),
            ("转图片 / 预览", "将链接转为图片发送"),
            ("转视频", "将链接转为视频发送"),
        ]
    },
    sticker {
        display_name: "表情收藏",
        section: "message",
        summary: "保存/收藏对方发送的表情或图片（需引用原消息）",
        commands: cmds![
            ("收 / 偷 / 存表情", "引用表情/图片后收藏"),
            ("表情转图片", "引用动画表情后转为静态图片"),
        ]
    },
    group_title {
        display_name: "群头衔",
        section: "play",
        summary: "Bot 为群主时，给申请者设置群专属头衔",
        commands: cmds![("我要头衔 <文字>", "给自己申请一个群专属头衔")]
    },
    ping {
        display_name: "心跳测试",
        section: "play",
        summary: "心跳测试，统计全服 Ping 次数",
        commands: cmds![("ping", "测试 Bot 在线状态")],
        on_init: Some(ping::init)
    },
    recall {
        display_name: "消息撤回",
        section: "message",
        summary: "撤回引用的消息（需引用回复使用）",
        commands: cmds![("撤回", "引用要撤回的消息后发送")]
    },
    echo {
        display_name: "消息回显",
        section: "message",
        summary: "回显参数内容（支持图片等富文本）",
        commands: cmds![("echo <内容>", "原样回显参数")]
    },
    repeater {
        display_name: "复读机",
        section: "play",
        summary: "同一句话接力到阈值就跟读一次，带冷却、概率与打断复读"
    },
    wordcloud {
        display_name: "词云",
        section: "insight",
        summary: "根据消息记录生成词云图",
        commands: cmds![
            ("<范围><时间>词云", "范围：本群/跨群/我的；时间：今日/昨日/本周/上周/近7天/近30天/本月/上月/今年/去年/总"),
            ("本群今日词云", "示例：本群今日"),
            ("我的总词云", "示例：个人全部"),
        ]
    },
    stats {
        display_name: "统计图表",
        section: "insight",
        summary: "群统计图表：发言/表情/消息类型排行榜与走势，支持早中晚与周月的错峰定时推送",
        commands: cmds![
            ("<范围><时间><类型><图表>", "范围：本群/跨群/我的/所有群；时间：今日…总；类型：发言/表情包/消息类型；图表：排行榜/走势"),
            ("本群今日发言排行榜", "示例"),
            ("本群本周发言走势", "示例"),
            ("所有群近7天发言排行榜", "示例：跨全部群"),
        ],
        on_connected: Some(stats::on_connected)
    },
    gif {
        display_name: "GIF 工具箱",
        section: "message",
        summary: "GIF 工具箱：合成、变速、倒放、缩放等",
        commands: cmds![
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
        ]
    },
    image_split {
        display_name: "图片切分",
        section: "message",
        summary: "将一张图按行列切片",
        commands: cmds![("裁剪 <行>x<列> / 切图 / 分割", "如：裁剪 3x3")]
    },
    ciyi {
        display_name: "词意猜词",
        section: "play",
        summary: "词意游戏：猜词与排行榜，群聊私聊均可",
        commands: cmds![
            ("词意帮助 / 词意指令 / 词意指令列表 / 词意帮助列表", "查看指令列表"),
            ("词意玩法 / 词意规则", "查看游戏规则"),
            ("词意猜测 [词语]", "开始猜词或提交答案"),
            ("词意榜", "当前会话排行榜"),
            ("词意全榜", "全服排行榜"),
        ],
        on_init: Some(ciyi::init)
    },
    webshot {
        display_name: "网页截图",
        section: "message",
        summary: "自动对消息中的网页链接进行截图"
    },
    oai {
        display_name: "智能对话",
        section: "play",
        summary: "多智能体对话与模型/历史管理；内置 pi 使用 Responses API，按配置开放本机工具（符号指令）",
        commands: cmds![
            ("oai", "查看完整模型、提示词与历史管理指令"),
            ("~pi <任务>", "内置 pi 房间；需先配置可用的模型 API"),
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
        on_init: Some(oai::init)
    },
    ai_news {
        display_name: "AI 资讯推送",
        section: "insight",
        summary: "AI 资讯 / 热点 / 日报 / 模型榜（数据源 AIHOT）：一级推送只发图片，引用卡片后只回标题与链接",
        commands: cmds![
            ("ai资讯 / ai新闻", "最近 24 小时 AI 精选资讯"),
            ("ai热点", "当前 AI 热点榜 Top 10"),
            ("ai日报", "最新一期 AI 日报"),
            ("ai模型榜 / 模型排行榜", "AIHOT 大模型排行榜：共识分、评测完整度与官网参考价"),
            ("ai搜索 <关键词>", "近 7 天按关键词检索 AI 资讯"),
            ("ai提取 <序号|全部>", "引用资讯图片后只回标题与链接；支持 1,3-5 批量提取"),
            ("ai推送添加 <群|私聊> <ID>", "从任意群聊或私聊添加指定推送目标"),
            ("ai推送删除 <群|私聊> <ID>", "从任意群聊或私聊删除指定推送目标"),
            ("ai推送开启 / ai推送关闭", "不带参数时开启/关闭当前会话，也可指定目标"),
            ("ai推送列表", "查看全部群聊与私聊推送目标"),
            ("ai实时开启 / ai实时关闭", "当前或指定目标是否接收实时快报"),
            ("ai实时模式 <精选|全部>", "默认仅推精选；可切换实时资讯来源"),
            ("ai分类 <分类>", "当前目标独立选择模型、产品、行业、论文、技巧或全部"),
            ("ai静默 <时间段>", "当前目标独立设置实时静默时段，如 23:30-07:30"),
            ("ai推送状态 [目标]", "查看当前或指定目标的开关、实时参数与排期"),
            ("ai推送重置 [目标]", "清空当前或指定目标的去重记录"),
            ("设置 ai_news card_theme auto", "阅读主题自动切换；也可使用 light / dark"),
        ],
        on_init: Some(ai_news::init),
        on_connected: Some(ai_news::on_connected)
    },
    webui {
        display_name: "网页面板",
        section: "system",
        summary: "本机网页控制台：可视化管理全部插件的开关与配置，与 /ctl 同一套校验和保存；默认只监听 127.0.0.1，凭密钥访问",
        commands: cmds![
            ("webui / 面板 / 网页面板", "取回带密钥的面板地址；仅限私聊或本机控制台"),
            ("webui 状态", "查看监听地址与运行状态"),
            ("webui 重置密钥", "作废旧密钥并生成新的"),
        ],
        on_init: Some(webui::init)
    },
    settings {
        display_name: "设置",
        section: "system",
        summary: "兼容旧版精选设置入口，仅限全局管理员；全部字段与插件开关请用 ctl",
        commands: cmds![
            ("设置", "查看全部可调项"),
            ("设置 <插件> <键>", "查看某项详情"),
            ("设置 <插件> <键> <值>", "修改并自动保存"),
        ]
    },
    help {
        display_name: "帮助中心",
        section: "system",
        summary: "按当前注册表展示 Satori 插件及开关；配置管理请用 ctl",
        commands: cmds![
            ("help / 帮助 / 插件列表", "插件总览"),
            ("help <插件名>", "查看插件详情与全部指令"),
        ]
    },
    restart {
        display_name: "自动重启",
        section: "system",
        summary: "每日定时自动重启 + 内存阈值监控，防止长时间运行卡顿",
        commands: cmds![("restart", "仅限 ctl.admins；需开启 allow_manual_restart")],
        on_init: Some(restart::init)
    },
);
