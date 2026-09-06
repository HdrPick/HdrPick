//! 屏幕捕获模块
//!
//! 基于 DXGI Desktop Duplication API，支持 SDR 与 HDR 内容捕获。

pub mod brightness;
pub mod dxgi_duplication;
pub mod edid;
pub mod hdr_pipeline;
pub mod hdr_toggle;
pub mod monitor;
pub mod ocr;
pub mod wgc;

pub use brightness::*;
pub use dxgi_duplication::*;
pub use hdr_pipeline::*;
pub use hdr_toggle::*;
pub use monitor::*;
pub use ocr::*;

/// 捕获的单帧原始数据
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub format: crate::color::PixelFormat,
    /// 像素数据（行优先，从左上角开始）
    pub data: Vec<u8>,
    /// 每行字节数
    pub row_pitch: usize,
}

impl CapturedFrame {
    /// 获取指定像素的字节偏移
    pub fn pixel_offset(&self, x: u32, y: u32) -> usize {
        y as usize * self.row_pitch + x as usize * self.format.bytes_per_pixel()
    }
}
