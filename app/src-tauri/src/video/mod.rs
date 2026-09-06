//! 视频模块（videorec 接入）：MP4/MKV 长录制 + 播放器
//!
//! 架构（docs/视频模块接入主程序方案.md）：
//! - 复用主程序 capture 层（DDA/WGC 抓帧，零改动）
//! - 编码/封装/播放全部走 videorec（独立 crate，零侵入）
//! - 与 JXL 动图（record/）完全并行，互不改对方核心
//!
//! 模块拓扑：
//! - [`commands`]：Tauri 命令（video_record_* / video_play_*）
//! - [`session`]：视频录制会话（单线程循环：抓帧→BGRA 读回→nvenc→MKV 增量写盘）
//! - [`player`]：播放器宿主（P0 独立窗口，videorec PlayerWindow 自持消息循环）
//!
//! FFmpeg DLL 分发：exe 同目录 `ffmpeg\bin\`（FFmpegLib::probe 候选首位）；
//! 缺 DLL 时全部命令优雅报错，不影响截图/看图/动图（模块级隔离）。

pub mod camera;
pub mod commands;
pub mod cursor_overlay;
pub mod p010_gpu;
pub mod player;
pub mod session;
pub mod watermark;
