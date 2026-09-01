ayjx
====

[<img alt="github" src="https://img.shields.io/badge/github-araea/ayjx-8da0cb?style=for-the-badge&labelColor=555555&logo=github" height="20">](https://github.com/araea/ayjx)

基于 Satori v1 的 QQ 机器人框架，默认连接本机
[`satori-qq`](https://github.com/araea/satori-qq) 实现端。

## 使用

1. 确保 `satori-qq` 已在本机 QQ 中运行，默认地址为
   `http://127.0.0.1:3001`。
2. 复制 `config.example.toml` 为 `config.toml`，按实现端配置填写
   `access_token`。也可通过环境变量 `AYJX_SATORI_TOKEN` 注入；环境变量优先，
   适合避免将 token 写入文件。
3. 运行 `cargo run --release`，发送 `/help` 查看插件。

默认指令前缀为 `/`。首次启动会把所有插件的默认配置自动补入
`config.toml`；该文件已被 Git 忽略。

## Satori 接入

`ayjx` 作为 Satori 客户端工作：

- `GET /v1/events` WebSocket：发送 `IDENTIFY`，处理 `READY`、`EVENT`、
  `PING/PONG` 和自动重连；
- `POST /v1/{resource}.{method}`：消息发送、查询、撤回、表态和群资料读取；
- `POST /v1/internal/*`：群头衔、点赞等 `satori-qq` 提供的 QQ 特有能力；
- Satori 元素串与内部消息链双向转换，覆盖文本、@、引用、表情、图片、
  语音、视频、文件、JSON、戳一戳和合并转发；
- QQ NT 消息 ID 全程使用 64 位整数，避免引用、撤回和表态时截断。

适配器只注册 `satori` 和本地测试用的 `console`，不再包含 OneBot 接入。
协议字段及 API 映射详见 [Satori 接入说明](docs/SATORI.md)。

插件在 `src/plugins/`。

## QQ 群

956758505
