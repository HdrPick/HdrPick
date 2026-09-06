//! JPEG XR（.jxr）HDR 编码器 —— Windows WIC（windowscodecs.dll）
//!
//! 输出 128bppRGBFloat（每像素 4×f32，RGB + 保留位），标准 scRGB 语义
//! （IEC 61966-2-2，线性光，1.0 = 80 nits D65 白）。
//! 与解码端（viewer/decode/jxr.rs HdrFloat 路径）构成闭环：
//! 解码时按 scRGB 1.0=80 基准走 v2 色调映射管线。
//!
//! 内部捕获纹理 scRGB 的 1.0 = input_sdr_white_nits（显示器 SDR 白），
//! 写入文件前统一换算到标准 80 nits 基准。

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::GENERIC_WRITE;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatWmp, GUID_WICPixelFormat128bppRGBFloat,
    IWICBitmapEncoder, IWICBitmapFrameEncode, IWICImagingFactory, IWICStream,
    WICBitmapEncoderNoCache,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};

use crate::capture::hdr_pipeline::{f16_to_f32, HdrToSdrParams};
use crate::capture::CapturedTexture;
use crate::color::{pq_eotf, PixelFormat};
use crate::encode::QualityLevel;

/// 保存 HDR JPEG XR（128bppRGBFloat，标准 scRGB 1.0=80 nits）
///
/// quality：5 档（WIC ImageQuality 属性；1.0 = 无损，越小频率域量化越狠）
pub fn save_jxr(
    captured: &CapturedTexture,
    path: &Path,
    params: &HdrToSdrParams,
    quality: QualityLevel,
) -> anyhow::Result<()> {
    // 1. 像素转换：捕获纹理 → 标准 scRGB f32（RGB + 保留位 4×f32/px）
    let rgbaf = to_scrgb_f32(captured, params);

    let _co = CoInitGuard::new();

    // 2. WIC 工厂 + 输出流
    let factory: IWICImagingFactory = unsafe {
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| anyhow::anyhow!("创建 WIC 工厂失败: {}", e))?
    };
    let stream: IWICStream = unsafe {
        factory
            .CreateStream()
            .map_err(|e| anyhow::anyhow!("创建 WIC 流失败: {}", e))?
    };
    let path_w: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        stream
            .InitializeFromFilename(PCWSTR(path_w.as_ptr()), GENERIC_WRITE.0)
            .map_err(|e| anyhow::anyhow!("WIC 打开输出文件失败: {}: {}", path.display(), e))?;
    }

    // 3. JXR（WMP 容器）编码器
    let encoder: IWICBitmapEncoder = unsafe {
        factory
            .CreateEncoder(&GUID_ContainerFormatWmp, std::ptr::null())
            .map_err(|e| anyhow::anyhow!("创建 JXR 编码器失败: {}", e))?
    };
    unsafe {
        encoder
            .Initialize(&stream, WICBitmapEncoderNoCache)
            .map_err(|e| anyhow::anyhow!("JXR 编码器初始化失败: {}", e))?;
    }

    // 4. 帧：尺寸 + float 像素格式 + 像素数据
    let mut frame_opt: Option<IWICBitmapFrameEncode> = None;
    let mut bag = None;
    unsafe {
        encoder
            .CreateNewFrame(&mut frame_opt, &mut bag)
            .map_err(|e| anyhow::anyhow!("创建 JXR 帧失败: {}", e))?;
    }
    let frame = frame_opt.ok_or_else(|| anyhow::anyhow!("JXR 帧创建返回空"))?;

    // 质量档：WIC 属性袋 "ImageQuality"（VT_R4，0-1；1.0 = 无损）。
    // 仅有损档设置（属性写失败仅降级日志，不阻断保存）
    if !quality.is_lossless() {
        if let Some(bag) = bag.as_ref() {
            set_wic_image_quality(bag, quality.wic_quality());
        }
    }

    unsafe {
        frame
            .Initialize(None)
            .map_err(|e| anyhow::anyhow!("JXR 帧初始化失败: {}", e))?;
        frame
            .SetSize(captured.width, captured.height)
            .map_err(|e| anyhow::anyhow!("JXR SetSize 失败: {}", e))?;
    }

    // SetPixelFormat 输入输出同参数：传入期望格式，返回实际支持格式
    let mut fmt: GUID = GUID_WICPixelFormat128bppRGBFloat;
    unsafe {
        frame
            .SetPixelFormat(&mut fmt)
            .map_err(|e| anyhow::anyhow!("JXR SetPixelFormat 失败: {}", e))?;
    }
    if fmt != GUID_WICPixelFormat128bppRGBFloat {
        anyhow::bail!("WIC JXR 编码器不支持 128bppRGBFloat（返回 {:?}）", fmt);
    }

    // 5. 写入像素（stride = 宽 × 16 字节）
    let stride = captured.width as usize * 16;
    unsafe {
        frame
            .WritePixels(captured.height, stride as u32, &rgbaf)
            .map_err(|e| anyhow::anyhow!("JXR 像素写入失败: {}", e))?;
        frame
            .Commit()
            .map_err(|e| anyhow::anyhow!("JXR 帧提交失败: {}", e))?;
        encoder
            .Commit()
            .map_err(|e| anyhow::anyhow!("JXR 编码提交失败: {}", e))?;
    }

    log::info!(
        "HDR JXR 已保存: {} ({}x{}, 128bppRGBFloat scRGB)",
        path.display(),
        captured.width,
        captured.height
    );
    Ok(())
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

/// WIC 帧属性袋写入 "ImageQuality"（0.0-1.0，1.0 = 无损）
///
/// 失败仅记日志（编码继续走编码器默认质量，不阻断截图保存）
fn set_wic_image_quality(
    bag: &windows::Win32::System::Com::StructuredStorage::IPropertyBag2,
    quality: f32,
) {
    use windows::Win32::System::Com::StructuredStorage::PROPBAG2;
    use windows::Win32::System::Variant::VT_R4;
    let name: Vec<u16> = "ImageQuality\0".encode_utf16().collect();
    let prop = PROPBAG2 {
        vt: VT_R4,
        pstrName: windows::core::PWSTR(name.as_ptr() as *mut u16),
        ..Default::default()
    };
    let val = windows::core::VARIANT::from(quality);
    let hr = unsafe { bag.Write(1, &prop, &val) };
    if let Err(e) = hr {
        log::warn!("[JXR] ImageQuality 属性写入失败（回退默认质量）: {}", e);
    }
}

/// 捕获纹理 → 标准 scRGB f32 数组（每像素 4×f32：R,G,B + 保留 0.0）
///
/// 输出基准：1.0 = 80 nits（JXR float / scRGB 标准）。
/// 捕获纹理 scRGB 同为 1.0 = 80 nits（物理定义），直通无换算；
/// params.input_sdr_white_nits 不参与（仅 sidecar 记录/SDR 管线用）。
fn to_scrgb_f32(captured: &CapturedTexture, params: &HdrToSdrParams) -> Vec<u8> {
    let _ = params;
    let width = captured.width as usize;
    let height = captured.height as usize;
    let row_pitch = captured.row_pitch;
    let npx = width * height;
    let mut out = vec![0u8; npx * 16]; // 4×f32 = 16 bytes/px

    let mut put = |i: usize, r: f32, g: f32, b: f32| {
        let off = i * 16;
        out[off..off + 4].copy_from_slice(&r.to_le_bytes());
        out[off + 4..off + 8].copy_from_slice(&g.to_le_bytes());
        out[off + 8..off + 12].copy_from_slice(&b.to_le_bytes());
        out[off + 12..off + 16].copy_from_slice(&0f32.to_le_bytes()); // 保留位
    };

    match captured.format {
        PixelFormat::R16g16b16a16Float => {
            // scRGB f16 → f32 直通（两者 1.0 = 80 nits，同语义）
            for y in 0..height {
                let src_off = y * row_pitch;
                for x in 0..width {
                    let s = src_off + x * 8;
                    let i = y * width + x;
                    let r = f16_to_f32(captured.data[s..s + 2].try_into().unwrap_or([0; 2]));
                    let g = f16_to_f32(captured.data[s + 2..s + 4].try_into().unwrap_or([0; 2]));
                    let b = f16_to_f32(captured.data[s + 4..s + 6].try_into().unwrap_or([0; 2]));
                    put(i, r, g, b);
                }
            }
        }
        PixelFormat::R10g10b10a2 => {
            // HDR10 PQ：EOTF 解码到线性（0-1 = 0-10000 nits）→ scRGB（nits/80）
            for y in 0..height {
                let src_off = y * row_pitch;
                for x in 0..width {
                    let s = src_off + x * 4;
                    let i = y * width + x;
                    let packed = u32::from_le_bytes([
                        captured.data[s],
                        captured.data[s + 1],
                        captured.data[s + 2],
                        captured.data[s + 3],
                    ]);
                    let r10 = (packed & 0x3FF) as f32 / 1023.0;
                    let g10 = ((packed >> 10) & 0x3FF) as f32 / 1023.0;
                    let b10 = ((packed >> 20) & 0x3FF) as f32 / 1023.0;
                    // PQ 线性 1.0 = 10000 nits → scRGB = nits / 80 = ×125
                    put(
                        i,
                        pq_eotf(r10) * 125.0,
                        pq_eotf(g10) * 125.0,
                        pq_eotf(b10) * 125.0,
                    );
                }
            }
        }
        PixelFormat::Bgra8 => {
            // SDR 回退：sRGB EOTF → 线性 scRGB（1.0 = 80 nits 标称白）
            for y in 0..height {
                let src_off = y * row_pitch;
                for x in 0..width {
                    let s = src_off + x * 4;
                    let i = y * width + x;
                    let r = crate::color::srgb_eotf(captured.data[s + 2] as f32 / 255.0);
                    let g = crate::color::srgb_eotf(captured.data[s + 1] as f32 / 255.0);
                    let b = crate::color::srgb_eotf(captured.data[s] as f32 / 255.0);
                    put(i, r, g, b);
                }
            }
        }
    }
    out
}
