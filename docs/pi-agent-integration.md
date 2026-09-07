# OAI Pi Agent 接入记录

续接 CodeBuddy 会话 `01a07c71-442e-7930-8493-e737675a1af9`（OAI插件pi房间接入pi agent）。
原会话仅创建了未接入的 `pi_agent.rs`；本次完成主流程、配置、历史、呈现和部署。

## 行为

- `pi` / `pi-*` 房间使用本机 Pi；创建、复制、重命名支持 Pi 前缀，模型以 Pi 配置为准。
- 公有、私有和临时请求各自隔离；历史改写和重新生成会重建 Pi 会话。
- 正文通过 stdin 传入，图片通过临时文件传入，结束后清理；中间思考和工具输出不写入聊天记录。
- 卡片显示实际模型、耗时和工具摘要；短回复直接输出文字；CLI 错误作为对话失败返回。
- 停止和超时终止 Pi 及其仍属于该调用树的工具，覆盖 Pi bash 的独立进程组。
- 移除旧内置工具循环；配置改用 `pi_command`，保留请求超时、进度和呈现选项。

## 验证

- `cargo test`：146 通过，0 失败，14 项需要外部环境的测试默认跳过。
- 覆盖房间路由、重新生成、并发隔离、CLI 文本和图片传输、stderr 排空、错误解析、取消及临时目录清理。
- `cargo build --release`：通过；使用 `./bot stop` / `./bot session` 更新运行版本。
- 本机控制台 `~"pi ...` 验证已走 Pi 链路，并成功返回 `AYJX_RUNNING_OK`。
- 已备份并为本机 Pi 的 `apilio/gpt-5.4-mini` 增加 `input: ["text", "image"]`，其余配置不变。
- 真实 Pi 验收已通过：成功回忆历史暗号、执行只读 `bash` 工具，并识别测试图片为红色；实际模型为 `apilio/gpt-5.4-mini`。
- 期间出现过一次上游 `Connection error`，重试后恢复；底层 API 直接请求也返回 200。
- Chromium 卡片实测在当前 `cdp-html-shot 0.2.8` 与 Chromium 149 组合下卡在 CDP 会话建立，120 秒后终止；Markdown/HTML 单元测试通过。此项不影响 Pi 文本回复，卡片渲染失败时现有逻辑会退回纯文本。

```sh
cargo test live_pi_reads_history_image_and_runs_a_tool -- --ignored --nocapture
```

## 本机配置备份

- 项目 `config.toml.backup-before-pi-agent`：OAI 配置迁移前备份。
- `~/.pi/agent/models.json.backup-before-ayjx-pi-images`：图片能力声明前备份。

测试日志位于 `~/ayjx-pi-test-final.log`、`~/ayjx-pi-build.log`、`~/ayjx-pi-live-2.log`。
