ayjx
====

[<img alt="github" src="https://img.shields.io/badge/github-araea/ayjx-8da0cb?style=for-the-badge&labelColor=555555&logo=github" height="20">](https://github.com/araea/ayjx)

QQ 机器人框架。以 [Satori v1](https://satori.js.org/zh-CN/) 协议连接实现端，
默认对接本机 [`satori-qq`](https://github.com/araea/satori-qq)。

## 使用

1. 启动 `satori-qq`，默认监听 `http://127.0.0.1:3001`。
2. 复制 `config.example.toml` 为 `config.toml`，填入实现端的 `access_token`；
   留空表示不鉴权。
3. `cargo run --release`，在 QQ 中发送 `/help`。

构建需要支持 edition 2024 的 Rust（1.85+）。网页截图与卡片出图调用本机
Chrome/Chromium，默认自动查找，也可用 `browser_path` 指定可执行文件。

## 配置

`config.toml` 已被 Git 忽略。首次启动写入全部插件的默认配置，之后每次启动补齐
新增字段、清理已移除插件的残留项；文件解析失败时程序停止，不覆盖原文件。

- `command_prefix`：指令前缀，默认 `["/"]`，可配多个；
- `browser_path`：浏览器可执行文件路径，留空为自动查找；
- `global_filter`：全局群黑/白名单，在事件进入插件流水线前生效；
- `[[bots]]`：适配器列表，`satori` 连接实现端，`console` 供本地测试；
- `access_token`：也可用环境变量 `AYJX_SATORI_TOKEN` 注入，优先级更高。

数据库为 `data/bot.db`（SQLite，WAL 模式）。

## 插件

插件在 `src/plugins/`，注册表为 `src/plugins/registry.rs`；各自带默认配置与开关，
指令清单见 `/help`。

## 文档

- [架构说明](docs/ARCHITECTURE.md)
- [Satori 接入说明](docs/SATORI.md)
- [插件兼容性审计](docs/SATORI_PLUGIN_AUDIT.md)

## QQ 群

956758505
