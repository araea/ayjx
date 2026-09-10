# ayjx

基于 Rust 的 QQ 机器人框架，通过 Satori v1 连接实现端，并提供可配置的插件运行环境。

## 安装

需要 Rust 1.85 或更高版本。准备 config.toml 后构建：

~~~
cp config.example.toml config.toml
cargo build --release
./bot start
~~~

默认 Satori 地址为 http://127.0.0.1:3001。网页截图与资讯长图需要本机
Chrome 或 Chromium，可在 browser_path 中指定路径；帮助与插件控制也使用网页
卡片，浏览器不可用时自动回退纯文本。词意卡片同样走网页排版，截图不可用时回退到
自带的原生绘图，再不行才发纯文本。中文出图需系统中日韩字体；系统只带 Regular 一档时
标题会用合成的伪粗体，跑 `sh scripts/install-cjk-weights.sh` 装上真字重即可。

## 配置

config.toml 不提交到 Git。首次启动会补齐默认字段，不覆盖解析失败的文件。

- command_prefix：指令前缀，默认 /；
- global_filter：全局群黑白名单；
- [[bots]]：satori 连接实现端，console 用于本地测试；
- access_token：Satori 令牌，也可由 AYJX_SATORI_TOKEN 提供；
- [ctl]：插件控制权限，admins 填维护者 QQ 号；
- [oai]：可选的 OAI 与 Pi Agent 设置。

数据库文件为 data/bot.db。

## 插件与运行

插件位于 src/plugins/，清单在 src/plugins/registry.rs —— 新增插件只改这一处，
/help 与 /ctl 会自动带上它。发送 /help 查看指令，使用 /ctl（别名 /控制、/插件）
查看和修改插件开关与配置；两者默认都以卡片图作答，`image_enabled` 可改回纯文本。
首次使用前在停机状态设置 [ctl].admins；空列表只允许本机控制台管理。

复读插件按频道保留最近 128 条已确认复读或打断的原内容记录。群友继续接力时，
即使中途插话、发送指令或超过冷却时间，也不会再次触发同一内容；新内容仍按阈值处理。
这些记录保存在内存中，重启或频道状态淘汰后清除。

~~~
./bot status
./bot stop
./bot restart
~~~

Termux 下 ./bot start 会取得唤醒锁；需要使用 tmux 时执行 ./bot session 和 ./bot attach。
任意房间都可以交给本机 Pi CLI：`##研究 pi` 建房，`助手%pi apilio/claude-opus-5`
换引擎和模型，房间名不受限制；[oai.ambient] 可让同一个 agent 以固定人格在指定群里
旁听、偶尔搭话。普通房间的模型可写 `供应商/模型`（供应商在 [oai].providers 里配，
如 `deepseek/deepseek-flash`），并支持 `:强度` 或房间 `thinking` 指定思考强度。

## 文档与测试

- [插件控制](docs/CONTROL.md)
- [Satori 接入](docs/SATORI.md)
- [Pi 房间](docs/pi-agent.md)
- [群聊搭话](docs/ambient.md)
- [插件兼容性审计](docs/SATORI_PLUGIN_AUDIT.md)
- [架构说明](docs/ARCHITECTURE.md)

~~~
cargo test
cargo build --release
~~~
