# ayjx

基于 Rust 的 QQ 机器人框架，通过 Satori v1 连接实现端，并提供可配置的插件运行环境。

## 安装

需要 Rust 1.85 或更高版本。准备 config.toml 后构建：

~~~
cp config.example.toml config.toml
cargo build --release
./bot start
~~~

默认 Satori 地址为 http://127.0.0.1:3001。网页截图和卡片出图需要本机
Chrome 或 Chromium，可在 browser_path 中指定路径。

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

插件位于 src/plugins/。发送 /help 查看指令，使用 /ctl（别名 /控制、/插件）查看和修改
插件开关与配置。首次使用前在停机状态设置 [ctl].admins；空列表只允许本机控制台管理。

~~~
./bot status
./bot stop
./bot restart
~~~

Termux 下 ./bot start 会取得唤醒锁；需要使用 tmux 时执行 ./bot session 和 ./bot attach。
名称为 pi 或以 pi- 开头的房间可调用本机 Pi CLI；[oai.ambient] 可让同一个 agent
以固定人格在指定群里旁听、偶尔搭话。

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
