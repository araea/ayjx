# ayjx

基于 Rust 的 QQ 机器人框架。通过 Satori v1 协议连接实现端，插件在配置文件里开关和调整。

## 安装

需要 Rust 1.85 或更高版本。先准备配置，再构建：

```sh
cp config.example.toml config.toml
cargo build --release
./bot start
```

Satori 默认地址是 `http://127.0.0.1:3001`。网页截图和资讯长图需要本机安装 Chrome 或 Chromium，可以用 `browser_path` 指定路径。浏览器不可用时，帮助和插件控制改用纯文本。中文出图需要系统中日韩字体。如果字体只有 Regular 一档，标题会使用合成的粗体，运行 `sh scripts/install-cjk-weights.sh` 可以安装真实的粗体字重。

## 配置

`config.toml` 不提交到 Git。首次启动会写入缺少的默认字段，配置解析失败时不覆盖原文件。

- `command_prefix`：指令前缀，默认 `/`
- `global_filter`：全局群黑白名单
- `[[bots]]`：Satori 连接实现端，`console` 用于本地测试
- `access_token`：Satori 令牌，也可以由 `AYJX_SATORI_TOKEN` 提供
- `[ctl]`：插件控制权限，`admins` 填维护者 QQ 号
- `[oai]`：可选的 OAI 与内置 Agent 设置

数据库文件是 `data/bot.db`。

## 插件与运行

插件放在 `src/plugins/`，清单在 `src/plugins/registry.rs`；新增插件只改这一处，`/help` 和 `/ctl` 会自动包含它。发送 `/help` 查看指令；`/ctl`（别名 `/控制`、`/插件`）用于查看和修改插件开关与配置。两者默认以卡片图作答，把 `image_enabled` 设为 false 可以改回纯文本。首次使用前需要停机设置 `[ctl].admins`，列表为空时只允许本机控制台管理。

```sh
./bot status
./bot stop
./bot restart
```

Termux 下 `./bot start` 会取得唤醒锁。需要用 tmux 时执行 `./bot session` 和 `./bot attach`。

## 文档与测试

- [插件控制](docs/CONTROL.md)
- [Satori 接入](docs/SATORI.md)
- [内置 Agent 房间](docs/agent.md)
- [群聊搭话](docs/ambient.md)
- [架构说明](docs/ARCHITECTURE.md)

```sh
cargo test
cargo build --release
```
