//! 色彩科学模块
//!
//! 包含 HDR/SDR 色彩转换的核心数学：
//! - 传递函数（PQ/HLG/sRGB/BT.1886）
//! - 色域矩阵（BT.2020 ↔ BT.709）
//! - 色调映射（BT.2390）

pub mod matrix;
pub mod sdr_hdr;
pub mod tonemap;
pub mod transfer;

pub use matrix::*;
pub use sdr_hdr::*;
pub use tonemap::*;
pub use transfer::*;

/// RGB 浮点像素（线性光，0..1 表示 SDR 白，>1 表示 HDR 高光）
#[derive(Clone, Copy, Debug, Default)]
pub struct RgbLinear {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

/// 像素格式标识
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// 8bit BGRA，SDR
    Bgra8,
    /// 10bit R10G10B10A2，HDR10 (PQ)
    R10g10b10a2,
    /// 16bit float RGBA，scRGB
    R16g16b16a16Float,
}

impl PixelFormat {
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Bgra8 => 4,
            PixelFormat::R10g10b10a2 => 4,
            PixelFormat::R16g16b16a16Float => 8,
        }
    }

    pub fn is_hdr(self) -> bool {
        matches!(
            self,
            PixelFormat::R10g10b10a2 | PixelFormat::R16g16b16a16Float
        )
    }
}
