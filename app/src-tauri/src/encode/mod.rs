//! 图像编码模块
//!
//! 支持多种输出格式：
//! - **SDR PNG**：标准 8bit sRGB PNG
//! - **HDR PNGv3**：16bit PNG + cICP chunk（BT.2020 + PQ）
//! - **JPEG XL**：HDR 16bit（BT.2020 + PQ，vendor jxl-sys）
//! - **JPEG XR**：HDR 128bppRGBFloat scRGB（Windows WIC）
//! - **OpenEXR**：32bit 浮点专业 HDR

pub mod exr;
pub mod jxr;
pub mod png;

// JPEG XL / AVIF 通过 feature 门控（jxl 默认启用）
#[cfg(feature = "avif")]
pub mod avif;
#[cfg(feature = "jxl")]
pub mod jxl;

pub use exr::*;
pub use jxr::*;
pub use png::*;

#[cfg(feature = "avif")]
pub use avif::*;
#[cfg(feature = "jxl")]
pub use jxl::*;

/// 输出格式
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum OutputFormat {
    /// 标准 8bit sRGB PNG（色调映射后）
    PngSdr,
    /// 16bit HDR PNG（BT.2020 + PQ，保留高动态范围）
    PngHdr,
    /// HDR JPEG XL（16bit BT.2020/PQ）
    Jxl,
    /// HDR JPEG XR（128bppRGBFloat scRGB，WIC）
    Jxr,
    /// AVIF HDR
    Avif,
    /// OpenEXR 32bit 浮点
    Exr,
}

impl Default for OutputFormat {
    fn default() -> Self {
        OutputFormat::PngSdr
    }
}

impl OutputFormat {
    pub fn extension(self) -> &'static str {
        match self {
            OutputFormat::PngSdr | OutputFormat::PngHdr => "png",
            OutputFormat::Jxl => "jxl",
            OutputFormat::Jxr => "jxr",
            OutputFormat::Avif => "avif",
            OutputFormat::Exr => "exr",
        }
    }

    pub fn is_hdr(self) -> bool {
        matches!(
            self,
            OutputFormat::PngHdr
                | OutputFormat::Jxl
                | OutputFormat::Jxr
                | OutputFormat::Avif
                | OutputFormat::Exr
        )
    }
}

/// 编码质量档（JXL / JXR 通用；5 档可切换）
///
/// - JXL：libjxl distance（0.0 = 数学无损，modular 编码；其余视觉有损 VarDCT）
/// - JXR：WIC ImageQuality（1.0 = 无损；其余频率域量化）
/// - PNG/EXR/PngHdr 恒定无损，不受此档影响
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum QualityLevel {
    /// 无损（数学可逆；体积最大）
    Lossless,
    /// 极高（几乎无损观感，体积减半）
    VeryHigh,
    /// 高（肉眼难辨差异）
    High,
    /// 平衡（体积/质量折中）
    Balanced,
    /// 体积优先（明显压缩，体积最小）
    Compact,
}

impl Default for QualityLevel {
    fn default() -> Self {
        QualityLevel::Lossless
    }
}

impl QualityLevel {
    /// libjxl 距离（d=0 无损；d 越大压缩越狠）
    pub fn jxl_distance(self) -> f32 {
        match self {
            QualityLevel::Lossless => 0.0,
            QualityLevel::VeryHigh => 0.1,
            QualityLevel::High => 0.3,
            QualityLevel::Balanced => 0.5,
            QualityLevel::Compact => 1.0,
        }
    }

    /// WIC ImageQuality（1.0 = 无损；越小量化越狠）
    pub fn wic_quality(self) -> f32 {
        match self {
            QualityLevel::Lossless => 1.0,
            QualityLevel::VeryHigh => 0.95,
            QualityLevel::High => 0.9,
            QualityLevel::Balanced => 0.8,
            QualityLevel::Compact => 0.7,
        }
    }

    pub fn is_lossless(self) -> bool {
        matches!(self, QualityLevel::Lossless)
    }
}
