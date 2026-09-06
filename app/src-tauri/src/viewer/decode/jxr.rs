//! JPEG XR（.jxr / .wdp）解码查看 —— Windows WIC（windowscodecs.dll）
//!
//! JPEG XR 是 Windows 生态的 HDR 图片格式（WMP-HDR / float 变体，
//! Windows HDR 壁纸即此格式）。WebView2 不能直接显示，统一走 WIC 解码：
//! - SDR 变体 → WIC 格式转换器 → RGBA8 → 临时 PNG 直出
//! - HDR 变体（128bppRGBFloat / 64bppRGBHalfFloat / 32bppRGBE）→
//!   线性 scRGB → 复用截图 v2 色调映射管线（to_sdr）→ SDR 临时 PNG 预览
//!
//! WIC 为 Win10/11 系统组件，无需第三方库。

use std::path::{Path, PathBuf};

use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppRGBA, IWICBitmapDecoder,
    IWICBitmapFrameDecode, IWICFormatConverter, IWICImagingFactory, WICBitmapDitherTypeNone,
    WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};

use crate::capture::hdr_pipeline::{f16_to_f32, to_sdr};
use crate::capture::CapturedTexture;
use crate::color::PixelFormat;

use super::exr::f32_to_f16;
use super::tiff::write_temp_png;

/// WIC 解码 JPEG XR → 临时 PNG，返回 (临时 PNG 路径, 宽, 高)
pub fn decode_to_temp_png(path: &Path) -> Result<(PathBuf, u32, u32), String> {
    match decode_frame(path)? {
        Frame::Sdr {
            width,
            height,
            rgba,
        } => {
            let img = image::RgbaImage::from_raw(width, height, rgba)
                .ok_or_else(|| "JXR 数据异常".to_string())?;
            let temp = write_temp_png(&image::DynamicImage::ImageRgba8(img), path)?;
            log::info!("JXR(SDR) 已解码: {} ({}x{})", path.display(), width, height);
            Ok((temp, width, height))
        }
        Frame::HdrFloat { width, height, rgb } => {
            cache_hdr_source(path, width, height, &rgb);
            let temp = hdr_to_temp_png(path, width, height, &rgb);
            log::info!(
                "JXR(HDR float) 已解码并色调映射: {} ({}x{})",
                path.display(),
                width,
                height
            );
            Ok((temp, width, height))
        }
    }
}

/// f32 线性 scRGB → f16 → 缓存 HdrSource（「亮度调节」重渲数据源）
fn cache_hdr_source(path: &Path, width: u32, height: u32, rgb: &[f32]) {
    let npx = (width * height) as usize;
    let mut data = vec![0u8; npx * 8];
    for i in 0..npx {
        let off = i * 8;
        let o3 = i * 3;
        data[off..off + 2].copy_from_slice(&f32_to_f16(rgb[o3]).to_le_bytes());
        data[off + 2..off + 4].copy_from_slice(&f32_to_f16(rgb[o3 + 1]).to_le_bytes());
        data[off + 4..off + 6].copy_from_slice(&f32_to_f16(rgb[o3 + 2]).to_le_bytes());
        data[off + 6..off + 8].copy_from_slice(&0x3C00u16.to_le_bytes());
    }
    super::cache_hdr_source(
        path,
        super::HdrSource {
            width,
            height,
            data,
        },
    );
}

/// 缩略图/相册网格用：解码为 DynamicImage（HDR 变体同样走色调映射 SDR）
pub fn decode_image(path: &Path) -> Result<image::DynamicImage, String> {
    match decode_frame(path)? {
        Frame::Sdr {
            width,
            height,
            rgba,
        } => {
            let img = image::RgbaImage::from_raw(width, height, rgba)
                .ok_or_else(|| "JXR 数据异常".to_string())?;
            Ok(image::DynamicImage::ImageRgba8(img))
        }
        Frame::HdrFloat { width, height, rgb } => {
            cache_hdr_source(path, width, height, &rgb);
            let temp = hdr_to_temp_png(path, width, height, &rgb);
            image::ImageReader::open(&temp)
                .map_err(|e| format!("打开临时文件失败: {}", e))?
                .with_guessed_format()
                .map_err(|e| format!("识别格式失败: {}", e))?
                .decode()
                .map_err(|e| format!("临时 PNG 解码失败: {}", e))
        }
    }
}

enum Frame {
    Sdr {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    HdrFloat {
        width: u32,
        height: u32,
        rgb: Vec<f32>,
    },
}

/// HDR 像素布局（GUID → 字节展开方式；bpp 用于计算行距）
#[derive(Clone, Copy, PartialEq, Eq)]
enum HdrLayout {
    /// 128bppRGBFloat：f32×3 + pad4（16B/px）
    F32Rgb,
    /// 128bppRGBAFloat：f32×4（16B/px）
    F32Rgba,
    /// 64bppRGBHalf：f16×3 + pad2（8B/px）
    F16Rgb,
    /// 64bppRGBAHalf：f16×4（8B/px）
    F16Rgba,
    /// 32bppRGBE：共享指数 4 字节（4B/px）
    Rgbe,
    /// 128bppRGB/RGBAFixedPoint：i32 s15.16（16B/px，pad 不读）
    I32FixedRgb,
    I32FixedRgba,
    /// 96bppRGBFixedPoint：i32 s15.16 ×3（12B/px）
    I32FixedRgb96,
    /// 48/64bppRGB(A)FixedPoint：i16 s2.14（6/8B/px）
    I16FixedRgb,
    I16FixedRgba,
}

/// WIC 像素格式 → HDR 布局判定
///
/// GUID 全部取自 windows-rs 导出常量（权威值；此前手写 GUID 后缀
/// 大多错误——如 128bppRGBFloat 实为 c91b 而非 c900——导致部分 HDR
/// JXR 被误判 SDR）。
fn hdr_layout(fmt: &GUID) -> Option<(HdrLayout, u32)> {
    use windows::Win32::Graphics::Imaging as wic;
    let (layout, bpp) = if *fmt == wic::GUID_WICPixelFormat128bppRGBFloat {
        (HdrLayout::F32Rgb, 128)
    } else if *fmt == wic::GUID_WICPixelFormat128bppRGBAFloat {
        (HdrLayout::F32Rgba, 128)
    } else if *fmt == wic::GUID_WICPixelFormat64bppRGBHalf {
        (HdrLayout::F16Rgb, 64)
    } else if *fmt == wic::GUID_WICPixelFormat64bppRGBAHalf {
        (HdrLayout::F16Rgba, 64)
    } else if *fmt == wic::GUID_WICPixelFormat32bppRGBE {
        (HdrLayout::Rgbe, 32)
    } else if *fmt == wic::GUID_WICPixelFormat128bppRGBFixedPoint {
        (HdrLayout::I32FixedRgb, 128)
    } else if *fmt == wic::GUID_WICPixelFormat128bppRGBAFixedPoint {
        (HdrLayout::I32FixedRgba, 128)
    } else if *fmt == wic::GUID_WICPixelFormat96bppRGBFixedPoint {
        (HdrLayout::I32FixedRgb96, 96)
    } else if *fmt == wic::GUID_WICPixelFormat64bppRGBFixedPoint {
        (HdrLayout::I16FixedRgb, 64)
    } else if *fmt == wic::GUID_WICPixelFormat64bppRGBAFixedPoint {
        (HdrLayout::I16FixedRgba, 64)
    } else if *fmt == wic::GUID_WICPixelFormat48bppRGBFixedPoint {
        (HdrLayout::I16FixedRgb, 48)
    } else {
        return None;
    };
    Some((layout, bpp))
}

/// COM STA 初始化守卫（当前线程已初始化时忽略失败）
struct CoInitGuard(bool);
impl CoInitGuard {
    fn new() -> Self {
        // S_FALSE(1) = 该线程已初始化，不需要我们 Uninitialize
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        CoInitGuard(hr.0 == 0)
    }
}
impl Drop for CoInitGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

/// 打开 JXR 并取第 0 帧
fn open_frame(path: &Path) -> Result<(IWICImagingFactory, IWICBitmapFrameDecode), String> {
    let factory: IWICImagingFactory = unsafe {
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("创建 WIC 工厂失败: {}", e))?
    };

    // CreateDecoderFromFilename（PCWSTR，UTF-16 以 0 结尾）
    use std::os::windows::ffi::OsStrExt;
    let path_w: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let decoder: IWICBitmapDecoder = unsafe {
        factory
            .CreateDecoderFromFilename(
                PCWSTR(path_w.as_ptr()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
            .map_err(|e| format!("WIC 打开 JXR 失败: {}", e))?
    };

    let frame = unsafe {
        decoder
            .GetFrame(0)
            .map_err(|e| format!("JXR 取帧失败: {}", e))?
    };
    Ok((factory, frame))
}

/// 解码整帧：HDR float 变体走原始像素，其余经格式转换器转 RGBA8
fn decode_frame(path: &Path) -> Result<Frame, String> {
    let _co = CoInitGuard::new();
    let (factory, frame) = open_frame(path)?;

    let (mut w, mut h) = (0u32, 0u32);
    unsafe { frame.GetSize(&mut w, &mut h) }.map_err(|e| format!("JXR 读尺寸失败: {}", e))?;
    if w == 0 || h == 0 {
        return Err("JXR 尺寸为 0".to_string());
    }

    let fmt: GUID =
        unsafe { frame.GetPixelFormat() }.map_err(|e| format!("JXR 读像素格式失败: {}", e))?;

    // HDR 变体判定：float（RGBFloat/RGBAHalf/RGBE）与定点（FixedPoint）家族，
    // 后者是 Edge/JXR 截图工具常用 HDR 容器（此前未识别 → 误走 SDR 路径）
    let hdr = hdr_layout(&fmt);

    if let Some((layout, bpp)) = hdr {
        // HDR：按原格式读原始字节再展开为 f32 线性 scRGB
        let stride = w * bpp / 8;
        let mut buf = vec![0u8; (stride * h) as usize];
        unsafe {
            frame
                .CopyPixels(std::ptr::null(), stride, &mut buf)
                .map_err(|e| format!("JXR HDR 像素读取失败: {}", e))?;
        }
        let px_count = (w * h) as usize;
        let mut rgb = Vec::with_capacity(px_count * 3);
        match layout {
            HdrLayout::F32Rgb | HdrLayout::F32Rgba => {
                for i in 0..px_count {
                    let off = i * 16;
                    for c in 0..3 {
                        rgb.push(f32::from_le_bytes(
                            buf[off + c * 4..off + c * 4 + 4].try_into().unwrap(),
                        ));
                    }
                }
            }
            HdrLayout::F16Rgb | HdrLayout::F16Rgba => {
                for i in 0..px_count {
                    let off = i * 8;
                    for c in 0..3 {
                        let bits = u16::from_le_bytes([buf[off + c * 2], buf[off + c * 2 + 1]]);
                        rgb.push(f16_to_f32(bits.to_le_bytes()));
                    }
                }
            }
            HdrLayout::Rgbe => {
                // RGBE：共享指数（mantissa×2^(e-128-1) 的线性近似）
                for i in 0..px_count {
                    let off = i * 4;
                    let scale = 2f32.powi(buf[off + 3] as i32 - 128 - 1);
                    for c in 0..3 {
                        rgb.push(buf[off + c] as f32 * scale / 255.0);
                    }
                }
            }
            HdrLayout::I32FixedRgb | HdrLayout::I32FixedRgba | HdrLayout::I32FixedRgb96 => {
                // s15.16 定点：i32 / 65536（96bpp 步进 12B，128bpp 步进 16B）
                let step = if layout == HdrLayout::I32FixedRgb96 { 12 } else { 16 };
                for i in 0..px_count {
                    let off = i * step;
                    for c in 0..3 {
                        let v = i32::from_le_bytes(
                            buf[off + c * 4..off + c * 4 + 4].try_into().unwrap(),
                        );
                        rgb.push(v as f32 / 65536.0);
                    }
                }
            }
            HdrLayout::I16FixedRgb | HdrLayout::I16FixedRgba => {
                // s2.14 定点：i16 / 32768（Rgb48 布局 6B/px 与 Rgba64 8B/px
                // 都按前 3 个 u16 通道读，与行距由 stride 保证对齐）
                let step = if layout == HdrLayout::I16FixedRgba { 8 } else { 6 };
                for i in 0..px_count {
                    let off = i * step;
                    for c in 0..3 {
                        let v = i16::from_le_bytes(
                            buf[off + c * 2..off + c * 2 + 2].try_into().unwrap(),
                        );
                        rgb.push(v as f32 / 32768.0);
                    }
                }
            }
        }
        Ok(Frame::HdrFloat {
            width: w,
            height: h,
            rgb,
        })
    } else {
        log::info!(
            "JXR 像素格式 {:?} 非 HDR 变体，走 SDR 转换路径: {}",
            fmt,
            path.display()
        );
        // SDR：格式转换器 → 32bppRGBA → CopyPixels
        let converter: IWICFormatConverter = unsafe {
            factory
                .CreateFormatConverter()
                .map_err(|e| format!("创建格式转换器失败: {}", e))?
        };
        unsafe {
            converter
                .Initialize(
                    &frame,
                    &GUID_WICPixelFormat32bppRGBA,
                    WICBitmapDitherTypeNone,
                    None,
                    0.0,
                    WICBitmapPaletteTypeCustom,
                )
                .map_err(|e| format!("JXR 格式转换初始化失败: {}", e))?;
        }
        let stride = w * 4;
        let mut buf = vec![0u8; (stride * h) as usize];
        unsafe {
            converter
                .CopyPixels(std::ptr::null(), stride, &mut buf)
                .map_err(|e| format!("JXR 像素读取失败: {}", e))?;
        }
        Ok(Frame::Sdr {
            width: w,
            height: h,
            rgba: buf,
        })
    }
}

/// HDR float 帧 → f16 scRGB 纹理 → v2 色调映射 → 临时 PNG
fn hdr_to_temp_png(src: &Path, width: u32, height: u32, rgb: &[f32]) -> PathBuf {
    let mut data = vec![0u8; (width * height) as usize * 8];
    for i in 0..(width * height) as usize {
        let off = i * 8;
        let o3 = i * 3;
        data[off..off + 2].copy_from_slice(&f32_to_f16(rgb[o3]).to_le_bytes());
        data[off + 2..off + 4].copy_from_slice(&f32_to_f16(rgb[o3 + 1]).to_le_bytes());
        data[off + 4..off + 6].copy_from_slice(&f32_to_f16(rgb[o3 + 2]).to_le_bytes());
        data[off + 6..off + 8].copy_from_slice(&0x3C00u16.to_le_bytes()); // A = 1.0
    }
    let captured = CapturedTexture {
        width,
        height,
        format: PixelFormat::R16g16b16a16Float,
        data,
        row_pitch: width as usize * 8,
        via_gdi: false,
    };
    // 文件解码路径：归一化基准用当前显示器实际 SDR 白（scRGB 1.0=80nits
    // 绝对语义，屏幕白(如200)应映射 y_rel=1.0；旧硬编码 80 会过曝 2.5 倍）
    let config = crate::config::Config::load();
    let params = config.active_tonemap_params(crate::capture::monitor::get_sdr_white_level_nits());
    let sdr = to_sdr(&captured, &params);

    match image::RgbaImage::from_raw(width, height, sdr.rgba) {
        Some(img) => write_temp_png(&image::DynamicImage::ImageRgba8(img), src)
            .unwrap_or_else(|_| src.to_path_buf()),
        None => src.to_path_buf(),
    }
}
