//! 录屏模块（JXL 动图，v3 设计：GPU 预处理前置 + 三线程全解耦）
//!
//! 设计文档：docs/录屏JXL动图设计方案.md
//!
//! - `gpu`：D3D11 compute 预处理（区域裁剪 + scRGB→PQ16 BT.2020 + 帧哈希）
//! - `recorder`：三线程流水线（抓帧 / 回拷入环 / 编码）+ 时间环形缓冲
//! - `commands`：Tauri 命令（record_start / record_stop / record_cancel / record_status）

pub mod commands;
pub mod gpu;
pub mod recorder;
