# ayjx

基于 Rust 的 QQ 机器人框架：通过 Satori v1 接入实现端，提供可配置的插件运行环境。

## 安装

需要 Rust 1.85 或更高版本。准备配置后构建：

```sh
cp config.example.toml config.toml
cargo build --release
./bot start
```

默认 Satori 地址为 `http://127.0.0.1:3001`。网页截图与资讯长图需要本机 Chrome 或 Chromium，可在 `browser_path` 指定路径。浏览器不可用时，帮助与插件控制回退纯文本；词意卡片先回退原生绘图，再回退纯文本。中文出图需要系统中日韩字体，只有 Regular 一档时标题会用合成的伪粗体，跑 `sh scripts/install-cjk-weights.sh` 装上真字重即可。

## 配置

`config.toml` 不提交到 Git。首次启动补齐默认字段，解析失败则不覆盖。

- `command_prefix`：指令前缀，默认 `/`
- `global_filter`：全局群黑白名单
- `[[bots]]`：Satori 连接实现端，`console` 用于本地测试
- `access_token`：Satori 令牌，也可由 `AYJX_SATORI_TOKEN` 提供
- `[ctl]`：插件控制权限，`admins` 填维护者 QQ 号
- `[oai]`：可选的 OAI 与 Pi Agent 设置

数据库文件为 `data/bot.db`。

## 插件与运行

插件位于 `src/plugins/`，清单在 `src/plugins/registry.rs`：新增插件只改这一处，`/help` 与 `/ctl` 会自动带上它。发送 `/help` 查看指令，使用 `/ctl`（别名 `/控制`、`/插件`）查看与修改插件开关和配置。两者默认以卡片图作答，`image_enabled` 可改回纯文本。首次使用前在停机状态设置 `[ctl].admins`，空列表只允许本机控制台管理。

复读插件按频道保留最近 128 条已确认复读或打断的原内容记录。群友继续接力时，即使中途插话、发送指令或超过冷却时间，也不会再次触发同一内容，新内容仍按阈值处理。这些记录保存在内存中，重启或频道状态淘汰后清除。

```sh
./bot status
./bot stop
./bot restart
```

Termux 下 `./bot start` 会取得唤醒锁；需要使用 tmux 时执行 `./bot session` 与 `./bot attach`。

任意房间都可以交给本机 Pi CLI：`##研究 pi` 建房，`助手%pi apilio/claude-opus-5` 换引擎与模型，房间名不受限制。`[oai.ambient]` 可让同一个 agent 以固定人格在指定群里旁听、偶尔搭话。普通房间的模型可写 `供应商/模型`（供应商在 `[oai].providers` 里配，如 `deepseek/deepseek-flash`），并支持 `:强度` 后缀或房间的 `thinking` 字段指定思考强度。

画图预设房间（`画·手办`、`画·乐高`、`画·图解` 等）在首次启动时自动建好，走 gpt-image 图像接口，在 `/#` 的「画图预设」分区单独成组：`画·手办 一只戴眼镜的橘猫` 直接出图，发一张图或引用图片即垫图改图。提示词就是房间的系统提示词，`画·手办$自己的写法` 随时可改，删掉的房间不会在下次启动时复活。

## 文档与测试

- [插件控制](docs/CONTROL.md)
- [Satori 接入](docs/SATORI.md)
- [Pi 房间](docs/pi-agent.md)
- [群聊搭话](docs/ambient.md)
- [插件兼容性审计](docs/SATORI_PLUGIN_AUDIT.md)
- [架构说明](docs/ARCHITECTURE.md)

```sh
cargo test
cargo build --release
```
