# 插件控制 ctl

`help` 负责说明和状态展示，`ctl`（中文别名 `控制`、`插件`）负责统一管理。

## 初次配置

在 ayjx 停止运行时编辑 `config.toml`：

```toml
[ctl]
enabled = true
admins = [123456789] # 维护者 QQ 号，可填多个
```

控制台适配器可以直接管理。QQ 里只有上述全局管理员能查看配置或修改全局状态，群管理员身份不会自动获得全局权限；`admins` 为空表示仅允许本机控制台管理。`/ctl` 用法、`/ctl list` 状态和 `/help` 帮助对所有人开放。权限检查不依赖 `ctl.enabled`，关闭 ctl 也不会让 `/restart` 失去权限检查。

## 常用命令

下列命令使用默认 `/` 前缀。修改 `command_prefix` 后使用相应前缀，空数组表示无前缀。

| 指令 | 功能 |
| --- | --- |
| `/ctl` | 完整用法 |
| `/ctl list` | 所有注册插件及全局开关 |
| `/ctl list on`、`/ctl list off` | 按开关状态筛选 |
| `/ctl list 统计` | 按英文名或中文名筛选 |
| `/ctl on help ping` | 一次开启多个插件 |
| `/插件 关闭 复读机,网页截图` | 中文名与逗号分隔也可用 |
| `/ctl show repeater` | 当前完整配置 |
| `/ctl show oai model_filter` | 查看嵌套字段 |
| `/ctl defaults help` | 查看默认值 |
| `/ctl set help image_enabled 关` | 设置布尔值 |
| `/ctl set oai plain_text_max_chars 120` | 设置数字 |
| `/ctl set repeater channel.white [123456, 789012]` | 替换数组 |
| `/ctl set repeater channel.white.0 456789` | 修改已有数组元素，索引从 0 开始 |
| `/ctl set repeater channel { white = [123456], black = [] }` | 替换整张表 |
| `/ctl set wordcloud font_family Noto Sans CJK SC` | 字符串可含空格 |
| `/ctl diff help` | 与默认值比较 |
| `/ctl set ctl image_enabled 关` | 让 ctl 只回纯文本，不出卡片图 |
| `/ctl reset help image_scale --confirm` | 恢复某项默认值 |
| `/ctl reset help --confirm` | 恢复插件参数，保留其开关 |

中文操作名：列表、状态、开启、启用、关闭、禁用、查看、默认、设置、重置、差异。英文插件名忽略大小写，中文显示名完全匹配。数组与表使用 TOML 语法，空字符串写 `""`，清空数组写 `[]`，带引号的字符串按 TOML 解码。配置路径以点分隔，只能修改已有路径，数组元素必须已经存在，空数组先整体设置。词云的可选 `font_path`、`font_family` 默认省略，可以直接用 `set` 添加，整插件 `reset` 会移除它们。

## 群名单：黑名单与白名单

`repeater`、`webshot`、`stats` 共用同一套 `channel` 子表，语义完全一致：

| 配置 | 效果 |
| --- | --- |
| 两个都留空 | 对所有群生效 |
| 只填 `black` | 除名单内的群以外都生效（`stats` 的定时推送同样跳过名单内的群） |
| 只填 `white` | 只有名单内的群生效并推送 |
| 两个都填 | 黑名单优先，同时出现在两边的群按禁止处理 |

私聊不受群名单约束，`stats` 的「我的」「跨群」查询在私聊里照常可用。

```text
/ctl set stats channel.black [123456789]
/ctl set webshot channel.white [123456789, 987654321]
/ctl set stats channel { white = [], black = [] }
```

这一份是插件自己的名单，与 `config.toml` 顶层的 `[global_filter]` 是两道独立的过滤：主动推送要同时通过全局过滤和插件名单才会发出。

## 模型列表过滤

`/%` 展示的模型来自 `[oai].model_filter`。中转站一次可以返回上千个 id，其中大多是历史快照、小参数量档位和语音视频等与聊天无关的条目，默认规则只保留当前可用的旗舰对话与图像模型。

| 字段 | 作用 |
| --- | --- |
| `keep` | 只保留命中任一关键字的模型；留空表示不筛选 |
| `drop` | 命中即剔除，优先于 `keep` |

两份关键字都不区分大小写，按子串匹配，写到「系列」粒度即可。站点上新或下架时修改配置即可，不必改代码，`/%` 每次都按当前规则重新拉取。

```text
/ctl show oai model_filter
/ctl set oai model_filter.keep ["gpt-5.6", "claude-opus-5", "gemini-3.8-flash"]
/ctl set oai model_filter.drop ["-2026-", "-lite"]
```

模型列表按厂商分区展示，Midjourney 的绘图模型不走过滤，始终附在列表末尾。

## 保存、权限与生效时间

- 批量开关全部验证通过才写入，任何插件名错误或保留入口检查失败都不会产生部分修改
- 通过真实插件配置类型检查数组元素和整数范围，并检查概率、图片倍率、时间等常用约束；未知字段和不完整的固定结构表会被拒绝
- 配置先写入同目录临时文件，同步后原子替换，成功后才发布到内存。保存失败保留原内存配置，并发修改通过同一把锁串行提交
- 启动时按插件默认值补全 `config.toml` 里缺失的字段，嵌套表里的也补，只补空缺、从不覆盖已有取值。`/ctl` 的路径解析走不进不存在的键，所以升级带来的新开关如果没有被补出来，运行时靠 serde 默认值照常工作，管理员却改不到它，`[ambient.peak]` 这种嵌套新表就属于这一类。补全会写回配置文件并记日志，补过一次之后不再重写
- ctl 位于日志与消息记录插件之前，控制指令不会被这些插件记录，也不会被业务插件消费。回复隐藏名称含 `token`、`secret`、`password`、`api_key` 等的字段，成功回执不复述输入值。敏感参数与管理员列表请在私聊或本机控制台设置
- 全局开关影响所有适配器和会话。关闭后下一条事件不再进入该插件；`recorder`、`stats`、`ai_news`、`restart` 的后台任务在后续触发时检查总开关。已经开始执行的请求或任务不会强制取消
- 无生命周期钩子的插件可以直接开关。带 `init` / `on_connected` 的插件如果启动时未开启，运行时开启会标注「待重启」，在重启完成初始化前不会进入消息处理。初始化参数、定时排期、推送间隔等在重启后完整生效，实时读取的参数下一次使用时生效
- ctl 不允许通过聊天关闭自身，也不允许管理员通过聊天移除自己的权限；整插件 `reset` 保留 `enabled`，重置 ctl 还保留 `admins`
- `/restart` 需要全局管理员，且 `restart.allow_manual_restart = true`。定时与手动重启都只向主循环提出请求，由主循环停止任务、关闭数据库与浏览器、保存配置；Unix/Termux 随后 exec 替换当前进程，保留 PID、终端、环境变量、启动参数与单实例锁，重新连接 Satori 前有短暂连接中断。`restart.time` 支持 `HH:MM` 或 `HH:MM:SS`，使用系统本地时区；内存阈值只统计 ayjx 自身 RSS（Linux/Android），不含 Chromium 子进程

## 在 agent 房间里用自然语言操作

`[ctl].pi_control`（默认 `true`）让 agent 房间可以直接说「把复读机关掉」「词云的字体调大一点」，由 agent 自己去查、去改、去复核。

一轮 agent 房间对话开始时，ayjx 为这一轮签发一次性凭据，随环境变量交给 agent 的工具子进程，并附带说明用法的 `ayjx-control` skill。agent 执行 `ayjx --ctl "<命令>"`，命令经本机 Unix 套接字回到运行中的实例，由 ctl 以维护者身份执行，回执原样打到 stdout，因此 agent 能看见结果并据此继续，而不是不查看结果就发下一条命令。

- 不做身份限制：任何能在 agent 房间里说话的人都能借它操作机器人。这是部署时的明确选择；agent 房间本来就持有全权限 shell，这条通道没有扩大它的能力边界，但确实把「改配置」从管理员专属变成了人人可用
- ctl 自身的保护规则仍然有效：不能通过聊天关闭 ctl，也不能让管理员失去管理入口。这些规则防的是误操作，不是权限
- 凭据随这一轮对话结束立即作废（最长寿命 30 分钟），只存在于内存与子进程环境变量里，不落盘，也不出现在命令行。套接字是 `data/ctl/control.sock`，权限 `0600`
- 每条经通道执行的命令都按「控制通道执行（QQ号）：命令」记进日志，可以追溯到人
- 群聊搭话（`[ambient]`）里的 agent 不签发 ctl 管理凭据，那是无人触发的自发言，不该带有修改配置的能力
- 收回这份开放有两个层次：`/ctl set ctl pi_control 关` 只关闭这条通道，agent 房间的 shell 仍然存在；真正的边界在 agent 的工具白名单

ctl 操作 `config.toml` 中插件自己的配置。连接凭据、全局过滤规则、数据库中的插件业务数据，以及 oai 独立存储的模型 API 与智能体历史仍由各自的入口管理。例如 oai 的 API、模型与房间操作见 `/oai`，推送目标快捷指令见 `/help ai_news`。

## 部署顺序

1. 编译并测试：`cargo test`、`cargo build --release`；也可以运行 `node tests/foreground.cjs` 验证隔离配置下的前台指令、进程管理与退出保存，运行 `node tests/restart.cjs` 验证保留 PID 的手动重启与定时重启
2. 向正在运行的 ayjx 发送 SIGTERM，等待进程退出和「配置已保存」日志
3. 备份并修改配置，开启所需插件。基础部署可以开启 `ctl`、`help`、`meta_filter`、`logger`、`recorder`、`ping`，按实际需求启用其他插件
4. 从仓库目录运行 `./bot start`，前台启动并临时开放本机控制台
5. 检查日志中的插件初始化与 Satori READY / 登录状态，再通过 `/ctl list` 查看配置

先停机再改配置，避免退出保存覆盖手工修改。私有配置、凭据、运行日志与数据库不提交到 Git。

## 前台运行与进程管理（Linux / Termux）

在仓库目录执行 `./bot start`。它会切换到正确的工作目录并运行 release 程序，输出实时日志，直接输入 `/ctl` 或 `/ctl list` 即可管理插件。按 `Ctrl+C` 停止，等待日志显示「配置已保存」和「Bye!」后，再手工编辑配置。

`--console` 只作用于本次启动，不改变 `[[bots]]` 的 console 开关，也不改变任何插件开关。例如 help 原来是关闭的，仍会保持关闭，可以用 `/ctl on help` 手动开启。

另一个终端中可执行：

| 指令 | 作用 |
| --- | --- |
| `./bot status` | 查看本仓库 release 程序的进程状态与 PID；停止时退出码为 3 |
| `./bot stop` | 发送 SIGTERM 并等待退出，超时报告失败而不强制终止 |
| `./bot restart` | 停止后在当前终端重新前台启动 |
| `./bot session` | 用 tmux 创建名为 ayjx 的可重新进入的前台会话 |
| `./bot attach` | 进入该会话，查看实时日志并输入指令 |
| `./bot` | 无参数时创建（或直接进入）名为 ayjx 的前台会话 |
| `./bot power on/off` | Termux 唤醒锁：熄屏保持网络，`off` 需先停止 bot |
| `./bot help` | 启动脚本帮助 |

`status`、`stop`、`session`、`attach` 分别可以简写为 `s`、`down`、`up`、`a`。把脚本链接到 `$PREFIX/bin/bot`（Termux）或 `~/.local/bin/bot` 后，任意目录都能直接使用。`./bot start` 在 Termux 上会自动取得唤醒锁以避免熄屏断网，`AYJX_WAKE_LOCK=0` 可以关闭；唤醒锁由整个 Termux 共享，`./bot power off` 会影响其他 Termux 任务，因此要求先停止 bot。

在 tmux 会话中按 `Ctrl+B` 后松开再按 `D` 可以暂离，bot 继续运行；`Ctrl+C` 会停止 bot，停止后用 `./bot session` 创建新会话，再 `./bot attach` 进入。直接前台启动则不需要 tmux。脚本通过进程可执行文件路径识别本仓库实例，并用文件锁防止重复启动，不要绕过脚本另外启动第二份程序。

进程「运行中」不代表 QQ 已经连接，连接成功应看到 Satori READY / 登录就绪日志。`/ctl list` 查看插件开关，`/ctl show <插件>` 查看配置。

ctl 与 help 使用 Chromium 网页卡片，共用 640px 纸面版式。插件清单单列呈现，开关与待重启状态有文字标签，指令、别名、配置与差异自动换行并保留完整内容。`image_scale` 控制 PNG 分辨率（1—4 倍，默认 3），`image_enabled = false` 可以使用纯文本。

安装 Chrome/Chromium 与系统中日韩字体，并在全局 `browser_path` 指定浏览器路径。出图含排队最多等待 45 秒，结束后清理页面；缺少浏览器、超时或图片超出安全尺寸时自动回复完整文本。`on` / `off` / `set` / `reset` 的确认及错误继续以文本回复。
