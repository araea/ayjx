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

插件放在 `src/plugins/`，清单在 `src/plugins/registry.rs`。新增插件只改这一处，`/help` 和 `/ctl` 会自动包含它。发送 `/help` 查看指令；`/ctl`（别名 `/控制`、`/插件`）用于查看和修改插件开关与配置。两者默认以卡片图作答，把 `image_enabled` 设为 false 可以改回纯文本。首次使用前需要停机设置 `[ctl].admins`，列表为空时只允许本机控制台管理。

复读插件为每个频道保留最近 128 条记录，记录已经确认复读或打断的原内容。群友继续复读同一内容时，即使中途有人插话、发送指令或超过冷却时间，也不会再次触发；新内容仍按阈值处理。这些记录保存在内存中，重启或频道状态淘汰后清除。

```sh
./bot status
./bot stop
./bot restart
```

Termux 下 `./bot start` 会取得唤醒锁。需要用 tmux 时执行 `./bot session` 和 `./bot attach`。

任意房间都可以交给内置 agent。`##研究 pi` 建房，`助手%pi apilio/claude-opus-5` 换引擎与模型，房间名不受限制。它自己调用模型和工具，不需要额外安装。`[oai.ambient]` 可以让同一个 agent 以固定人格在指定群里旁听并偶尔发言。普通房间的模型可以写 `供应商/模型`（供应商在 `[oai].providers` 里配置，例如 `deepseek/deepseek-flash`），也可以用 `:强度` 后缀或房间的 `thinking` 字段指定思考强度。

内置 agent 可以联网。`web_search` 查最新信息，`web_fetch` 读网页正文。后端和密钥在 `[oai.search]` 里配置一次，房间与搭话共用。房间默认关闭（`[oai.search].enabled`），搭话默认开启（`[oai.ambient].search_enabled`）。默认使用免密钥的 Bing 和 DuckDuckGo 抓取，不需要配置即可使用。

画图预设房间（`画·手办`、`画·乐高`、`画·图解` 等）在首次启动时自动建好，使用 gpt-image 图像接口，在 `/#` 的「画图预设」分区单独成组。`画·手办 一只戴眼镜的橘猫` 直接出图，发一张图或引用图片即为垫图改图。提示词就是房间的系统提示词，`画·手办$自己的写法` 随时可以修改。删除的房间不会在下次启动时恢复。

对话模板房间（`聊·GPT`、`聊·Opus`、`聊·双子`、`聊·Grok`、`聊·深度` 等）同样在首次启动时自动建好，在 `/#` 的「对话预设」分区单独成组。一间房对应一个当下主流的对话模型，系统提示词留空，用来试模型或直接开聊；`聊·GPT 帮我把这段话改得更短` 即可使用。想固定风格就自己写提示词，`聊·GPT~#我的GPT` 可以复制一份再改。房间名的 `·` 和画图预设一样，是为了不被日常聊天误触发。删除的房间不会在下次启动时恢复。

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
