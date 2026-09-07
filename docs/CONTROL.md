# 插件控制 ctl

`help` 负责说明与状态展示，`ctl`（中文别名 `控制`、`插件`）负责统一管理。
`settings` 保留精选参数的旧指令，配置写入与权限检查统一交给 ctl。

## 初次配置

在 ayjx **停止运行时**编辑 `config.toml`：

```toml
[ctl]
enabled = true
admins = [123456789] # 替换为维护者 QQ 号，可填多个
```

控制台适配器可直接管理。QQ 中只有上述全局管理员能查看配置或修改全局状态；
群管理员身份不会自动获得全局权限。空 `admins` 表示仅允许本机控制台管理。
`/ctl` 用法、`/ctl list` 状态和 `/help` 帮助向所有人开放。
权限检查不依赖 ctl.enabled，关闭 ctl 也不会使旧 `/设置` 或 `/restart` 失去权限检查。

## 常用命令

以下使用默认 `/` 前缀；修改 `command_prefix` 后使用相应前缀。空数组表示无前缀。

| 指令 | 功能 |
| --- | --- |
| `/ctl` | 完整用法 |
| `/ctl list` | 所有注册插件及全局开关 |
| `/ctl list on`、`/ctl list off` | 筛选开/关状态 |
| `/ctl list 统计` | 按英文名或中文名筛选 |
| `/ctl on help ping` | 一次开启多个插件 |
| `/插件 关闭 复读机,网页截图` | 中文名与逗号分隔也可用 |
| `/ctl show repeater` | 当前完整配置 |
| `/ctl show ciyi plugin.image_scale` | 查看嵌套字段 |
| `/ctl defaults help` | 查看默认值 |
| `/ctl set help image_enabled 关` | 设置布尔值 |
| `/ctl set ciyi plugin.image_scale 2` | 设置数字 |
| `/ctl set repeater channel.white [123456, 789012]` | 替换数组 |
| `/ctl set repeater channel.white.0 456789` | 修改已有数组元素，索引从 0 开始 |
| `/ctl set repeater channel { white = [123456], black = [] }` | 替换整张表 |
| `/ctl set wordcloud font_family Noto Sans CJK SC` | 字符串可包含空格 |
| `/ctl diff help` | 与默认值比较 |
| `/ctl reset help image_scale --confirm` | 恢复某项默认值 |
| `/ctl reset help --confirm` | 恢复插件参数，保留其开关 |

中文操作名：列表、状态、开启、启用、关闭、禁用、查看、默认、设置、重置、差异。
英文插件名忽略大小写，中文显示名完全匹配。数组和表使用 TOML 语法；
空字符串写 `""`，清空数组写 `[]`，带引号的字符串按 TOML 解码。
配置路径以点分隔，只能修改已有路径，数组元素须先存在；空数组先整体设置。
词云的可选 `font_path`、`font_family` 默认省略，可直接用 set 添加；整插件 reset 会移除它们。

## 保存、权限与生效时间

- 批量开关全部验证通过才写入，任何插件名错误或保留入口检查失败都不产生部分修改。
- 通过真实插件配置类型检查数组元素、整数范围，并检查概率、图片倍率、时间等常用约束。
  未知字段和不完整的固定结构表会被拒绝。
- 配置先写入同目录临时文件、同步并原子替换，成功后才发布到内存。
  保存失败保留原内存配置；并发修改通过同一把锁串行提交。
- ctl 位于日志/消息记录插件之前，控制指令不会被这些插件记录或被业务插件吞掉。
  回复隐藏名称含 token、secret、password、api_key 等的字段，成功回执不复述输入值。
  敏感参数和管理员列表请在私聊或本机控制台设置。
- 全局开关影响所有适配器和会话。关闭后下一条事件不再进入该插件；
  recorder、stats、ai_news、restart 的后台任务在后续触发时检查总开关。
  已开始执行的请求或任务不会强制取消。
- 无生命周期钩子的插件可直接开关。带 init/on_connected 的插件如果启动时未开启，
  运行时开启会标注“待重启”，在重新启动完成初始化前不会进入消息处理。
  初始化参数、定时排期、推送间隔等在重启后完整生效；实时读取的参数下一次使用时生效。
- ctl 不允许通过聊天关闭自身，也不允许管理员通过聊天移除自己的权限。
  整插件 reset 保留 enabled；重置 ctl 还保留 admins。
- `/restart` 需全局管理员且 `restart.allow_manual_restart = true`；
  不具备重启管理条件时，由本机维护者停止并重新启动框架。
  定时与手动重启都只向主循环提出请求，由主循环停止任务、关闭数据库与浏览器、保存配置；
  Unix/Termux 随后 exec 替换当前进程，保留 PID、终端、环境变量与启动参数和单实例锁，
  重新连接 Satori 前有短暂连接中断。`restart.time` 支持 `HH:MM` 或 `HH:MM:SS`，
  使用系统本地时区；内存阈值只统计 ayjx 自身 RSS（Linux/Android），不含 Chromium 子进程。

ctl 操作 `config.toml` 中插件自己的配置。连接凭据、全局过滤规则、数据库中的
插件业务数据，以及 oai 独立存储的模型 API 与智能体历史仍由其原入口管理。
例如 oai 的 API/模型/房间操作见 `/oai`；推送目标快捷指令见 `/help ai_news`。

## 部署顺序

1. 编译并测试：`cargo test`、`cargo build --release`；可再运行
   `node tests/foreground.cjs` 验证隔离配置下的前台指令、进程管理及退出保存，
   `node tests/restart.cjs` 验证保留 PID 的手动重启与定时重启。
2. 向正在运行的 ayjx 发送 SIGTERM，等待进程退出和“配置已保存”日志。
3. 备份并修改配置，开启所需插件。基础部署可开启 ctl、help、settings、
   meta_filter、logger、recorder、ping；按实际需求启用其他插件。
4. 从仓库目录运行 `./bot start`，前台启动并临时开放本机控制台。
5. 检查日志中插件初始化与 Satori READY/登录状态，再通过 `/ctl list` 查看配置。

先停机再改配置，避免退出保存将手工修改覆盖。私有配置、凭据、运行日志及数据库不提交到 Git。

## 前台运行与进程管理（Linux / Termux）

在仓库目录执行 `./bot start`。它会切换到正确工作目录并运行 release 程序，
输出实时日志，直接输入 `/ctl` 或 `/ctl list` 即可管理插件。按 `Ctrl+C` 停止，
等待日志显示“配置已保存”和“Bye!”后再手工编辑配置。

`--console` 仅作用于本次启动，不改变 `[[bots]]` 的 console 开关，也不改变任何插件开关。
例如 help 原来关闭，仍会保持关闭；可以使用 `/ctl on help` 主动开启。

另一个终端中可执行：

| 指令 | 作用 |
| --- | --- |
| `./bot status` | 查看此仓库 release 程序的进程状态与 PID；停止时退出码为 3 |
| `./bot stop` | 发送 SIGTERM，等待退出；超时报告失败，不强制终止 |
| `./bot restart` | 停止后在当前终端重新前台启动 |
| `./bot session` | 使用 tmux 创建名为 ayjx 的可重新进入的前台会话 |
| `./bot attach` | 进入该会话，查看实时日志并输入指令 |
| `./bot` | 无参数时创建（或直接进入）名为 ayjx 的前台会话 |
| `./bot power on/off` | Termux 唤醒锁：熄屏保持网络；`off` 需先停止 bot |
| `./bot help` | 显示启动脚本帮助 |

`status`、`stop`、`session`、`attach` 分别可简写为 `s`、`down`、`up`、`a`。
把脚本链接到 `$PREFIX/bin/bot`（Termux）或 `~/.local/bin/bot` 后，任意目录都能直接使用。
`./bot start` 在 Termux 上自动取得唤醒锁以避免熄屏断网，`AYJX_WAKE_LOCK=0` 可关闭；
唤醒锁由整个 Termux 共享，`./bot power off` 会影响其它 Termux 任务，因此要求先停止 bot。

在 tmux 会话中，按 `Ctrl+B` 后松开，再按 `D` 可暂离；bot 持续运行。
`Ctrl+C` 会停止 bot。停止后用 `./bot session` 创建新会话，再 `./bot attach` 进入。
若直接前台启动，则无需 tmux。脚本通过进程可执行文件路径识别本仓库实例，
并使用文件锁防止脚本重复启动；不要绕过脚本另外启动第二份程序。

进程“运行中”不等于 QQ 已连接；连接成功应看到 Satori READY/登录就绪日志。
`/ctl list` 查看插件开关，`/ctl show <插件>` 查看配置。截图依赖 Chrome/Chromium；
缺少浏览器时框架继续运行，help 在渲染失败时回退为文字。
