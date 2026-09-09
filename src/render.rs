//! 卡片渲染工具。help / ctl 使用 web 的 HTML 排版与浏览器截图；
//! canvas、font、kit 保留为原生绘图工具。

// 渲染层是一套「画卡片用的工具箱」，成套提供才好用：`hgrad` 有 `vgrad`、
// 宋黑两族各有常规与加粗、Block 里留着 `Gap`。某一件暂时没人调用不代表它多余，
// 下一张卡片就可能要它，所以这里不按调用数裁剪 API。
#![allow(dead_code, unused_imports)]

pub mod canvas;
pub mod font;
pub mod kit;
pub mod web;

pub use canvas::{Canvas, Ink};
pub use font::Fonts;
