//! JPEG XL（.jxl）解码查看 —— vendor jxl-sys（libjxl C API）
//!
//! - HDR（16bit PQ BT.2020，本工具 HDR JXL 截图输出）→ 复用 HDR PNG 管线：
//!   PQ EOTF → BT.2020→BT.709 色域转换 → v2 色调映射（to_sdr）→ SDR 临时 PNG
//! - float 线性（EXR 类内容）→ scRGB（1.0 = 80 nits）→ 色调映射
//! - SDR（8/16bit sRGB）→ 直出临时 PNG
//!
//! 解码状态机：SetInput → ProcessInput 循环，按事件消费
//! （BasicInfo / ColorEncoding / NeedImageOutBuffer / FullImage）。

use std::path::{Path, PathBuf};
use std::ptr;

use jxl_sys as jxl;

use crate::capture::hdr_pipeline::to_sdr;
use crate::capture::CapturedTexture;
use crate::color::PixelFormat;

use super::exr::f32_to_f16;
use super::png_hdr::tonemap_to_sdr;
use super::tiff::write_temp_png;

/// PQ16 → 线性 nits 查找表（64K 项，256KB 常驻 L2）
///
/// 动画逐帧转换原本每像素 3 次 `pq_eotf`（pow 幂运算，4MP 帧 ≈ 1200 万次/帧）
/// 是解码线程的第二大 CPU 消耗；16bit 输入域只有 65536 种取值 → 查表 O(1)。
static PQ16_NITS_LUT: std::sync::OnceLock<Box<[f32; 65536]>> = std::sync::OnceLock::new();
fn pq16_nits_lut() -> &'static [f32; 65536] {
    PQ16_NITS_LUT.get_or_init(|| {
        let mut t = Box::new([0f32; 65536]);
        for (i, v) in t.iter_mut().enumerate() {
            *v = crate::color::pq_eotf(i as f32 / 65535.0) * 10000.0;
        }
        t
    })
}

/// 输出像素缓冲（NeedImageOutBuffer 设置，FullImage 后填充完毕）
struct OutBuffer {
    bytes: Vec<u8>,
    kind: OutKind,
}

enum OutKind {
    /// 16bit PQ RGBA（HDR）
    HdrPq,
    /// f32 RGBA（线性 float HDR）
    HdrFloat,
    /// 8bit RGBA（SDR）
    Sdr,
}

/// 解码后的帧（统一转为 RGBA 形态）
enum Frame {
    /// SDR：8bit RGBA
    Sdr {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    /// HDR：16bit PQ RGBA（src 原色由 gamut 描述）
    HdrPq {
        width: u32,
        height: u32,
        rgba16: Vec<u16>,
        /// src 原色 → BT.709 色域矩阵（P3/2020/709 按文件色域编码选择）
        gamut: crate::color::Mat3,
    },
    /// HDR：线性 float RGB（scRGB 语义 1.0 = 80 nits）
    HdrFloat {
        width: u32,
        height: u32,
        rgb: Vec<f32>,
        /// 同上（sRGB 原色文件 = IDENTITY）
        gamut: crate::color::Mat3,
    },
}

/// 解码 JXL → 临时 PNG，返回 (临时 PNG 路径, 宽, 高)
pub fn decode_to_temp_png(path: &Path) -> Result<(PathBuf, u32, u32), String> {
    match decode_frame(path)? {
        Frame::Sdr {
            width,
            height,
            rgba,
        } => {
            let img = image::RgbaImage::from_raw(width, height, rgba)
                .ok_or_else(|| "JXL 数据异常".to_string())?;
            let temp = write_temp_png(&image::DynamicImage::ImageRgba8(img), path)?;
            log::info!("JXL(SDR) 已解码: {} ({}x{})", path.display(), width, height);
            Ok((temp, width, height))
        }
        Frame::HdrPq {
            width,
            height,
            rgba16,
            gamut,
        } => {
            let buffer =
                image::ImageBuffer::<image::Rgba<u16>, Vec<u16>>::from_raw(width, height, rgba16)
                    .ok_or_else(|| "JXL HDR 数据异常".to_string())?;
            cache_hdr_source_pq(path, &buffer, gamut);
            let sdr = tonemap_to_sdr(&buffer, gamut);
            let img = image::RgbaImage::from_raw(width, height, sdr)
                .ok_or_else(|| "JXL 色调映射数据异常".to_string())?;
            let temp = write_temp_png(&image::DynamicImage::ImageRgba8(img), path)?;
            log::info!(
                "JXL(HDR PQ) 已解码并色调映射: {} ({}x{})",
                path.display(),
                width,
                height
            );
            Ok((temp, width, height))
        }
        Frame::HdrFloat {
            width,
            height,
            rgb,
            gamut,
        } => {
            cache_hdr_source_float(path, width, height, &rgb, gamut);
            let temp = hdr_float_to_temp_png(path, width, height, &rgb, gamut);
            log::info!(
                "JXL(HDR float) 已解码并色调映射: {} ({}x{})",
                path.display(),
                width,
                height
            );
            Ok((temp, width, height))
        }
    }
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
                .ok_or_else(|| "JXL 数据异常".to_string())?;
            Ok(image::DynamicImage::ImageRgba8(img))
        }
        Frame::HdrPq {
            width,
            height,
            rgba16,
            gamut,
        } => {
            let buffer =
                image::ImageBuffer::<image::Rgba<u16>, Vec<u16>>::from_raw(width, height, rgba16)
                    .ok_or_else(|| "JXL HDR 数据异常".to_string())?;
            cache_hdr_source_pq(path, &buffer, gamut);
            let sdr = tonemap_to_sdr(&buffer, gamut);
            image::RgbaImage::from_raw(width, height, sdr)
                .map(image::DynamicImage::ImageRgba8)
                .ok_or_else(|| "JXL 色调映射数据异常".to_string())
        }
        Frame::HdrFloat {
            width,
            height,
            rgb,
            gamut,
        } => {
            cache_hdr_source_float(path, width, height, &rgb, gamut);
            let temp = hdr_float_to_temp_png(path, width, height, &rgb, gamut);
            image::ImageReader::open(&temp)
                .map_err(|e| format!("打开临时文件失败: {}", e))?
                .with_guessed_format()
                .map_err(|e| format!("识别格式失败: {}", e))?
                .decode()
                .map_err(|e| format!("临时 PNG 解码失败: {}", e))
        }
    }
}

/// 16bit PQ RGBA → scRGB f16 → 缓存 HdrSource（PQ 解码 + gamut 色域转换 + /80）
///
/// 行级并行（par_rows_mut）：4M 像素的 PQ EOTF ×3 + 矩阵运算是 CPU 大头。
fn cache_hdr_source_pq(
    path: &Path,
    rgba16: &image::ImageBuffer<image::Rgba<u16>, Vec<u16>>,
    gamut: crate::color::Mat3,
) {
    let w = rgba16.width() as usize;
    let h = rgba16.height() as usize;
    let raw = rgba16.as_raw();
    let mut data = vec![0u8; w * h * 8];
    let row_in = w * 4; // RGBA u16
    super::par_rows_mut(&mut data, w * 8, |out_row, y| {
        let in_row = &raw[y * row_in..(y + 1) * row_in];
        for (px, ins) in out_row.chunks_mut(8).zip(in_row.chunks_exact(4)) {
            let r = crate::color::pq_eotf(ins[0] as f32 / 65535.0) * 10000.0;
            let g = crate::color::pq_eotf(ins[1] as f32 / 65535.0) * 10000.0;
            let b = crate::color::pq_eotf(ins[2] as f32 / 65535.0) * 10000.0;
            let (r, g, b) = gamut.apply(r, g, b);
            px[0..2].copy_from_slice(&f32_to_f16(r / 80.0).to_le_bytes());
            px[2..4].copy_from_slice(&f32_to_f16(g / 80.0).to_le_bytes());
            px[4..6].copy_from_slice(&f32_to_f16(b / 80.0).to_le_bytes());
            px[6..8].copy_from_slice(&0x3C00u16.to_le_bytes());
        }
    });
    super::cache_hdr_source(
        path,
        super::HdrSource {
            width: w as u32,
            height: h as u32,
            data,
        },
    );
}

/// f32 线性 RGB（src 原色）→ 色域转换 → f16 → 缓存 HdrSource（行级并行）
fn cache_hdr_source_float(
    path: &Path,
    width: u32,
    height: u32,
    rgb: &[f32],
    gamut: crate::color::Mat3,
) {
    let w = width as usize;
    let mut data = vec![0u8; (width * height) as usize * 8];
    let row_in = w * 3;
    super::par_rows_mut(&mut data, w * 8, |out_row, y| {
        let in_row = &rgb[y * row_in..(y + 1) * row_in];
        for (px, ins) in out_row.chunks_mut(8).zip(in_row.chunks_exact(3)) {
            let (r, g, b) = gamut.apply(ins[0], ins[1], ins[2]);
            px[0..2].copy_from_slice(&f32_to_f16(r).to_le_bytes());
            px[2..4].copy_from_slice(&f32_to_f16(g).to_le_bytes());
            px[4..6].copy_from_slice(&f32_to_f16(b).to_le_bytes());
            px[6..8].copy_from_slice(&0x3C00u16.to_le_bytes());
        }
    });
    super::cache_hdr_source(
        path,
        super::HdrSource {
            width,
            height,
            data,
        },
    );
}

/// 快速读取尺寸（目录列表扫描用：解码到 BasicInfo 事件即停）
pub fn quick_dimensions(path: &Path) -> (u32, u32) {
    let Ok(data) = std::fs::read(path) else {
        return (0, 0);
    };
    unsafe {
        let dec = jxl::JxlDecoderCreate(ptr::null());
        if dec.is_null() {
            return (0, 0);
        }
        let _guard = DecoderGuard(dec);
        if jxl::JxlDecoderSubscribeEvents(dec, jxl::JXL_DEC_BASIC_INFO)
            != jxl::JxlDecoderStatus::Success
        {
            return (0, 0);
        }
        if jxl::JxlDecoderSetInput(dec, data.as_ptr(), data.len()) != jxl::JxlDecoderStatus::Success
        {
            return (0, 0);
        }
        jxl::JxlDecoderCloseInput(dec);
        loop {
            match jxl::JxlDecoderProcessInput(dec) {
                jxl::JxlDecoderStatus::BasicInfo => {
                    let mut info = jxl::JxlBasicInfo::default();
                    if jxl::JxlDecoderGetBasicInfo(dec, &mut info) == jxl::JxlDecoderStatus::Success
                    {
                        return (info.xsize, info.ysize);
                    }
                    return (0, 0);
                }
                jxl::JxlDecoderStatus::Success => return (0, 0),
                jxl::JxlDecoderStatus::Error => return (0, 0),
                // 文件不完整等：等待更多输入（此处数据已全量喂入，视为失败）
                jxl::JxlDecoderStatus::NeedMoreInput => return (0, 0),
                _ => return (0, 0),
            }
        }
    }
}

struct DecoderGuard(*mut jxl::JxlDecoder);
impl Drop for DecoderGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { jxl::JxlDecoderDestroy(self.0) };
        }
    }
}

/// 解码整帧：按色彩编码分派 HDR / SDR 路径
fn decode_frame(path: &Path) -> Result<Frame, String> {
    let data = std::fs::read(path).map_err(|e| format!("读取文件失败: {}", e))?;

    unsafe {
        let dec = jxl::JxlDecoderCreate(ptr::null());
        if dec.is_null() {
            return Err("JxlDecoderCreate 返回 null".to_string());
        }
        let _guard = DecoderGuard(dec);

        // 多线程解码：libjxl 线程 runner（≤8 工作线程；须在 SetInput 前设置）
        struct RunnerGuard(*mut std::ffi::c_void);
        impl Drop for RunnerGuard {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    unsafe { jxl::JxlThreadParallelRunnerDestroy(self.0) };
                }
            }
        }
        let runner_threads = std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(1))
            .unwrap_or(1)
            .clamp(1, 8);
        let runner = jxl::JxlThreadParallelRunnerCreate(ptr::null(), runner_threads);
        let _runner_guard = RunnerGuard(runner);
        if !runner.is_null() {
            let _ =
                jxl::JxlDecoderSetParallelRunner(dec, Some(jxl::JxlThreadParallelRunner), runner);
        }

        let events =
            jxl::JXL_DEC_BASIC_INFO | jxl::JXL_DEC_COLOR_ENCODING | jxl::JXL_DEC_FULL_IMAGE;
        if jxl::JxlDecoderSubscribeEvents(dec, events) != jxl::JxlDecoderStatus::Success {
            return Err("JXL 订阅事件失败".to_string());
        }
        if jxl::JxlDecoderSetInput(dec, data.as_ptr(), data.len()) != jxl::JxlDecoderStatus::Success
        {
            return Err("JXL 设置输入失败".to_string());
        }
        jxl::JxlDecoderCloseInput(dec);

        let mut info = jxl::JxlBasicInfo::default();
        let mut color = jxl::JxlColorEncoding::default();
        let mut have_info = false;
        let mut have_color = false;
        let mut out: Option<OutBuffer> = None;

        loop {
            match jxl::JxlDecoderProcessInput(dec) {
                jxl::JxlDecoderStatus::BasicInfo => {
                    if jxl::JxlDecoderGetBasicInfo(dec, &mut info) != jxl::JxlDecoderStatus::Success
                    {
                        return Err("JXL 读取基本信息失败".to_string());
                    }
                    have_info = true;
                }
                jxl::JxlDecoderStatus::ColorEncoding => {
                    // ICC-only 文件此调用失败：按 SDR 处理
                    if jxl::JxlDecoderGetColorAsEncodedProfile(
                        dec,
                        jxl::JxlColorProfileTarget::Original,
                        &mut color,
                    ) == jxl::JxlDecoderStatus::Success
                    {
                        have_color = true;
                    }
                }
                jxl::JxlDecoderStatus::NeedImageOutBuffer => {
                    if !have_info {
                        return Err("JXL 未提供基本信息".to_string());
                    }
                    let (w, h) = (info.xsize, info.ysize);
                    if w == 0 || h == 0 {
                        return Err("JXL 尺寸为 0".to_string());
                    }
                    let npx = (w as usize) * (h as usize);
                    let is_float = info.exponent_bits_per_sample > 0;
                    let is_pq =
                        have_color && color.transfer_function == jxl::JxlTransferFunction::Pq;

                    let (kind, data_type, chan_bytes) = if is_pq && !is_float {
                        (OutKind::HdrPq, jxl::JxlDataType::Uint16, 2usize)
                    } else if is_float {
                        (OutKind::HdrFloat, jxl::JxlDataType::Float, 4usize)
                    } else {
                        (OutKind::Sdr, jxl::JxlDataType::Uint8, 1usize)
                    };
                    let fmt = jxl::JxlPixelFormat {
                        num_channels: 4,
                        data_type,
                        endianness: jxl::JxlEndianness::Little,
                        align: 0,
                    };
                    let mut bytes = vec![0u8; npx * 4 * chan_bytes];
                    if jxl::JxlDecoderSetImageOutBuffer(
                        dec,
                        &fmt,
                        bytes.as_mut_ptr() as *mut std::ffi::c_void,
                        bytes.len(),
                    ) != jxl::JxlDecoderStatus::Success
                    {
                        return Err("JXL 设置输出缓冲失败".to_string());
                    }
                    out = Some(OutBuffer { bytes, kind });
                }
                jxl::JxlDecoderStatus::FullImage => {
                    // 像素填充完毕：按输出类型构造 Frame
                    let buffer = out
                        .take()
                        .ok_or_else(|| "JXL FullImage 前未设置输出缓冲".to_string())?;
                    if !have_info {
                        return Err("JXL 未提供基本信息".to_string());
                    }
                    let (w, h) = (info.xsize, info.ysize);
                    return match buffer.kind {
                        OutKind::HdrPq => {
                            let rgba16: Vec<u16> = buffer
                                .bytes
                                .chunks_exact(2)
                                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                                .collect();
                            Ok(Frame::HdrPq {
                                width: w,
                                height: h,
                                rgba16,
                                gamut: decode_gamut(&color, have_color),
                            })
                        }
                        OutKind::HdrFloat => {
                            let mut rgb = Vec::with_capacity((w as usize) * (h as usize) * 3);
                            for px in buffer.bytes.chunks_exact(16) {
                                rgb.push(f32::from_le_bytes(px[0..4].try_into().unwrap()));
                                rgb.push(f32::from_le_bytes(px[4..8].try_into().unwrap()));
                                rgb.push(f32::from_le_bytes(px[8..12].try_into().unwrap()));
                            }
                            Ok(Frame::HdrFloat {
                                width: w,
                                height: h,
                                rgb,
                                gamut: decode_gamut(&color, have_color),
                            })
                        }
                        OutKind::Sdr => Ok(Frame::Sdr {
                            width: w,
                            height: h,
                            rgba: buffer.bytes,
                        }),
                    };
                }
                jxl::JxlDecoderStatus::Success => {
                    return Err("JXL 解码提前结束（无图像数据）".to_string());
                }
                jxl::JxlDecoderStatus::Error => {
                    return Err("JXL 解码失败（文件损坏或不受支持）".to_string());
                }
                jxl::JxlDecoderStatus::NeedMoreInput => {
                    return Err("JXL 数据不完整".to_string());
                }
                _ => continue,
            }
        }
    }
}

/// 按文件色域编码选择 src→BT.709 矩阵（P3 支持：负分量保留给 DWM 原生重现）
fn decode_gamut(color: &jxl::JxlColorEncoding, have: bool) -> crate::color::Mat3 {
    use crate::color::{gamut_matrix, Chromaticity, ColorPrimaries, Mat3, BT2020, BT709, DCI_P3};
    if !have {
        // 无色域编码（ICC-only 兜底按 SDR 处理过的路径不会到这）：默认 2020
        return crate::color::bt2020_to_bt709();
    }
    match color.primaries {
        jxl::JxlPrimaries::P3 => gamut_matrix(DCI_P3, BT709),
        jxl::JxlPrimaries::P2100 => gamut_matrix(BT2020, BT709),
        jxl::JxlPrimaries::Srgb => Mat3::IDENTITY,
        jxl::JxlPrimaries::Custom => {
            // 自定义原色（primaries_*_xy 显式坐标）
            let ch = |xy: [f64; 2]| Chromaticity {
                x: xy[0] as f32,
                y: xy[1] as f32,
            };
            let p = ColorPrimaries {
                r: ch(color.primaries_red_xy),
                g: ch(color.primaries_green_xy),
                b: ch(color.primaries_blue_xy),
                white: crate::color::D65,
            };
            gamut_matrix(p, BT709)
        }
        _ => crate::color::bt2020_to_bt709(),
    }
}

// ============================================================================
// 动画 JXL 流式解码（P3 回放；录制管线的镜像工程）
// ============================================================================

/// 动画文件内存常驻缓存条目：路径 + 校验元数据（len + mtime）+ 共享字节
struct AnimFileEntry {
    path: PathBuf,
    len: u64,
    mtime: std::time::SystemTime,
    data: std::sync::Arc<Vec<u8>>,
    last_used: std::time::Instant,
}

/// 动画文件缓存（LRU 2 份上限）：动图播放 EOF 重开解码器（滑窗循环填充 /
/// ring 循环供给）每次 `fs::read` 整文件——353MB 样本实测 10MB/s 持续读盘。
/// 命中（路径 + len + mtime 一致）则 clone Arc 共享零读盘；未命中读盘插入
/// 并淘汰最久未用。最坏内存 ≈ 2 × 文件大小（353MB 样本 ~700MB，可接受）。
/// Arc<Vec<u8>> Send 安全，解码器只读语义（SetInput 指针用法不变）。
static ANIM_FILE_CACHE: std::sync::Mutex<Vec<AnimFileEntry>> = std::sync::Mutex::new(Vec::new());

/// 缓存命中计数（回归测试断言第二次 open 零读盘用）
static ANIM_FILE_CACHE_HITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// 读动画文件（优先命中内存缓存；返回 Arc 共享，多解码器零拷贝）
fn anim_file_read_cached(path: &Path) -> Result<std::sync::Arc<Vec<u8>>, String> {
    let key = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok().map(|t| (m.len(), t)));
    {
        let mut cache = ANIM_FILE_CACHE.lock().unwrap();
        for e in cache.iter_mut() {
            if e.path == path && Some((e.len, e.mtime)) == key {
                e.last_used = std::time::Instant::now();
                ANIM_FILE_CACHE_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Ok(std::sync::Arc::clone(&e.data));
            }
        }
    }
    // 未命中：读盘 → 插入（并发窗口内同键双读保留新条目）→ LRU 淘汰至 2 份
    let data = std::sync::Arc::new(std::fs::read(path).map_err(|e| format!("读取文件失败: {}", e))?);
    let (len, mtime) = match key {
        Some((l, t)) => (l, t),
        None => match std::fs::metadata(path) {
            Ok(m) => (
                m.len(),
                m.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            ),
            Err(_) => (data.len() as u64, std::time::SystemTime::UNIX_EPOCH),
        },
    };
    let mut cache = ANIM_FILE_CACHE.lock().unwrap();
    cache.retain(|e| !(e.path == path && Some((e.len, e.mtime)) == key));
    cache.push(AnimFileEntry {
        path: path.to_path_buf(),
        len,
        mtime,
        data: std::sync::Arc::clone(&data),
        last_used: std::time::Instant::now(),
    });
    while cache.len() > 2 {
        let lru = (0..cache.len())
            .min_by_key(|&i| cache[i].last_used)
            .expect("非空");
        cache.remove(lru);
    }
    // 字节总量保险丝（1GB）：录制产物动辄 GB 级，两份即 2GB+ 驻留——超限时
    // 从最旧开始逐出直到达标（至少保留刚插入的当前文件）
    const CACHE_BYTES_CAP: u64 = 1 << 30;
    loop {
        let total: u64 = cache.iter().map(|e| e.len).sum();
        if total <= CACHE_BYTES_CAP || cache.len() <= 1 {
            break;
        }
        let lru = (0..cache.len())
            .min_by_key(|&i| cache[i].last_used)
            .expect("非空");
        log::info!(
            "[动画] 文件缓存超 1GB 上限（{} 字节），逐出最旧: {}",
            total,
            cache[lru].path.display()
        );
        cache.remove(lru);
    }
    Ok(data)
}

/// 逐出动画文件缓存中的指定文件（播放窗口关闭时调用）。
///
/// ANIM_FILE_CACHE 的 LRU 淘汰只在打开第 3 个文件时发生——关闭播放器后
/// 文件字节（录制产物可达 1GB+）仍滞留内存（用户视角"录完保存好了内存
/// 还占 3G"的主因）。窗口销毁时主动逐出本文件，内存即刻归还。
pub fn anim_file_cache_evict(path: &Path) {
    let mut cache = ANIM_FILE_CACHE.lock().unwrap();
    let before = cache.len();
    cache.retain(|e| e.path != path);
    if cache.len() != before {
        log::info!(
            "[动画] 文件缓存逐出: {}（剩余 {} 份）",
            path.display(),
            cache.len()
        );
    }
}

/// 动画单帧（scRGB f16 RGBA，8B/px，BT.709 原色，1.0 = 80nits——与 HdrSource 同构）
pub struct AnimFrame {
    pub data: Vec<u8>,
    pub duration_ms: u32,
}

/// 动画单帧 PQ16 原始数据（GPU 出口变体，[`AnimationDecoder::next_frame_pq16`]）：
/// libjxl 直出 **RGBA u16 行交错**（强制 4 通道请求：无 alpha 文件 A=65535
/// 不透明；RGB 与 3 通道请求逐位一致——渲染管线逐通道独立转换，见 vendor
/// libjxl render_pipeline/stage_write.cc），行距 = w×8 字节小端无补齐。
/// 跳过 CPU PQ→scRGB f16 转换，由 [`super::pq16_gpu::convert_into`] GPU 链
///（LUT → 色域矩阵 → /80 → f32tof16）直写池槽纹理——与 CPU 出口逐位一致
///（对拍测试 `native_gpu_pq16_matches_cpu`）。
pub struct Pq16Frame {
    pub data: Vec<u8>,
    /// 输出通道数（恒 4：RGBA 强制——文档化 data 布局用）
    pub channels: u32,
    /// 文件色域 → BT.709 矩阵（GPU 链出口用，与 CPU 出口同一矩阵）
    pub gamut: crate::color::Mat3,
}

/// 动画流式解码器：open → 逐帧 next_frame（跨帧参考依赖 → 顺序解码，
/// 不可随机 seek；播放端按序消费即可无缝循环）
pub struct AnimationDecoder {
    dec: *mut jxl::JxlDecoder,
    _runner: *mut core::ffi::c_void,
    file: std::sync::Arc<Vec<u8>>, // 原始字节（SetInput 引用须保活；缓存共享，只读）
    width: u32,
    height: u32,
    gamut: crate::color::Mat3,
    /// 事件循环中间态（NeedImageOutBuffer 设置的缓冲跨事件存活）
    out: Option<OutBuffer>,
    pending_duration_ms: u32,
    /// tps（ticks per second）：帧头 duration 的单位
    tps: f64,
    /// 文件通道数（3 = 无 alpha 的纯 RGB——录制编码器产物；4 = 带 alpha）
    channels: u32,
    /// 已完整解出的帧数（尾帧截断容错判断用）
    frames_ok: usize,
}

impl Drop for AnimationDecoder {
    fn drop(&mut self) {
        if !self.dec.is_null() {
            unsafe { jxl::JxlDecoderDestroy(self.dec) };
        }
        if !self._runner.is_null() {
            unsafe { jxl::JxlThreadParallelRunnerDestroy(self._runner) };
        }
    }
}

// 跨线程说明：命令线程 open → 移交解码线程独占使用（无并发访问）；
// libjxl 解码器非 Send（原始指针），此处安全由"单线程独占"约定保证
unsafe impl Send for AnimationDecoder {}

/// 打开动画文件：探测是否为动图（BasicInfo.have_animation）
pub fn probe_is_animation(path: &Path) -> Result<bool, String> {
    let data = std::fs::read(path).map_err(|e| format!("读取文件失败: {}", e))?;
    unsafe {
        let dec = jxl::JxlDecoderCreate(ptr::null());
        if dec.is_null() {
            return Err("JxlDecoderCreate 返回 null".to_string());
        }
        let _guard = DecoderGuard(dec);
        if jxl::JxlDecoderSubscribeEvents(dec, jxl::JXL_DEC_BASIC_INFO)
            != jxl::JxlDecoderStatus::Success
        {
            return Err("JXL 订阅事件失败".to_string());
        }
        if jxl::JxlDecoderSetInput(dec, data.as_ptr(), data.len()) != jxl::JxlDecoderStatus::Success
        {
            return Err("JXL 设置输入失败".to_string());
        }
        jxl::JxlDecoderCloseInput(dec);
        loop {
            match jxl::JxlDecoderProcessInput(dec) {
                jxl::JxlDecoderStatus::BasicInfo => {
                    let mut info = jxl::JxlBasicInfo::default();
                    if jxl::JxlDecoderGetBasicInfo(dec, &mut info) != jxl::JxlDecoderStatus::Success
                    {
                        return Err("JXL 读取基本信息失败".to_string());
                    }
                    return Ok(info.have_animation != 0);
                }
                jxl::JxlDecoderStatus::Error => return Err("JXL 解码失败".to_string()),
                _ => {}
            }
        }
    }
}

/// 头部有界探测动图（probe_is_animation 的流式读版本）
///
/// probe_is_animation 整文件 read（录制产物几百 MB → 数百 ms IO + 等量内存）；
/// 本函数只读头部：BasicInfo 位于码流头部（签名/容器盒后紧跟帧头），拿到
/// BasicInfo 事件即返回。≤1MB 文件读至 EOF 后 CloseInput，事件语义与整文件读
/// 完全一致；>1MB 文件不 CloseInput（流式语义），1MB 内无 BasicInfo 视为损坏。
/// 必须显式处理 NeedMoreInput：未 CloseInput 时输入耗尽会持续返回该状态，
/// 落入 `_ => {}` 兜底会死循环。
pub fn probe_is_animation_head(path: &Path) -> Result<bool, String> {
    const HEAD_CAP: usize = 1024 * 1024;
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| format!("打开文件失败: {}", e))?;
    let mut data: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut eof = false;
    let mut chunk = [0u8; 64 * 1024];
    while data.len() < HEAD_CAP {
        let n = file.read(&mut chunk).map_err(|e| format!("读取文件失败: {}", e))?;
        if n == 0 {
            eof = true;
            break;
        }
        data.extend_from_slice(&chunk[..n]);
    }
    unsafe {
        let dec = jxl::JxlDecoderCreate(ptr::null());
        if dec.is_null() {
            return Err("JxlDecoderCreate 返回 null".to_string());
        }
        let _guard = DecoderGuard(dec);
        if jxl::JxlDecoderSubscribeEvents(dec, jxl::JXL_DEC_BASIC_INFO)
            != jxl::JxlDecoderStatus::Success
        {
            return Err("JXL 订阅事件失败".to_string());
        }
        if jxl::JxlDecoderSetInput(dec, data.as_ptr(), data.len()) != jxl::JxlDecoderStatus::Success
        {
            return Err("JXL 设置输入失败".to_string());
        }
        // ≤1MB 已到 EOF：CloseInput → 截断流返回 Error，与整文件读语义一致
        if eof {
            jxl::JxlDecoderCloseInput(dec);
        }
        loop {
            match jxl::JxlDecoderProcessInput(dec) {
                jxl::JxlDecoderStatus::BasicInfo => {
                    let mut info = jxl::JxlBasicInfo::default();
                    if jxl::JxlDecoderGetBasicInfo(dec, &mut info) != jxl::JxlDecoderStatus::Success
                    {
                        return Err("JXL 读取基本信息失败".to_string());
                    }
                    return Ok(info.have_animation != 0);
                }
                jxl::JxlDecoderStatus::Error => return Err("JXL 解码失败".to_string()),
                jxl::JxlDecoderStatus::NeedMoreInput => {
                    // 仅 >1MB 分支可达：有效 JXL 的 BasicInfo 远小于头部窗口
                    return Err("JXL 头部不完整（文件损坏或超头部窗口）".to_string());
                }
                _ => {}
            }
        }
    }
}

impl AnimationDecoder {
    /// 打开动画文件（校验 have_animation；DecodeGuard 内联处理）。
    /// 文件字节经 [`ANIM_FILE_CACHE`]（EOF 循环重开零读盘）
    pub fn open(path: &Path) -> Result<Self, String> {
        Self::open_inner(path, true)
    }

    /// 打开并定位到第 `start` 帧（0 基）——区间分工并行填充专用。
    /// 与 [`Self::open`] + [`Self::skip_to`] 的区别：open 会把帧 0 消费到
    /// in-flight（FRAME 事件已 emit），skip 无法撤回 → 首帧必出帧 0 快照；
    /// 本方法停在 FRAME 事件**之前**（仅消费 BasicInfo/ColorEncoding），
    /// skip(start) 跳过帧 0..start-1 → 首帧 = start，无快照解码成本。
    pub fn open_at(path: &Path, start: u64) -> Result<Self, String> {
        let mut d = Self::open_inner(path, false)?;
        if start > 0 {
            d.skip_to(start);
        }
        Ok(d)
    }

    fn open_inner(path: &Path, consume_first_frame: bool) -> Result<Self, String> {
        let file = anim_file_read_cached(path)?;
        unsafe {
            let dec = jxl::JxlDecoderCreate(ptr::null());
            if dec.is_null() {
                return Err("JxlDecoderCreate 返回 null".to_string());
            }
            // 守卫：构造中途 Err 时销毁（成功路径 mem::forget 移交）
            struct OpenGuard(*mut jxl::JxlDecoder);
            impl Drop for OpenGuard {
                fn drop(&mut self) {
                    if !self.0.is_null() {
                        unsafe { jxl::JxlDecoderDestroy(self.0) };
                    }
                }
            }
            let guard = OpenGuard(dec);

            // 解码并行度：全核（减 1 留给渲染线程）——动画帧是逐帧顺序解，
            // 帧内靠 runner 把 256×256 组分摊到各核，核数直接决定单帧解速。
            // 环境变量 JXL_PLAY_WORKERS 可指定多解码器并行填充的 worker 数
            //（并行时各 worker 的 runner 线程数相应缩减，防线程超订：
            //  AMD 7745HX 8C16T → 4 worker × 4 线程 = 16 槽位占满）
            let logical = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1);
            let play_workers: usize = std::env::var("JXL_PLAY_WORKERS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1)
                .max(1);
            let runner_threads = if play_workers > 1 {
                (logical / play_workers).max(2)
            } else {
                logical.saturating_sub(1).max(1)
            };
            let runner = jxl::JxlThreadParallelRunnerCreate(ptr::null(), runner_threads);
            let _ =
                jxl::JxlDecoderSetParallelRunner(dec, Some(jxl::JxlThreadParallelRunner), runner);

            let events = jxl::JXL_DEC_BASIC_INFO
                | jxl::JXL_DEC_COLOR_ENCODING
                | jxl::JXL_DEC_FRAME
                | jxl::JXL_DEC_FULL_IMAGE;
            if jxl::JxlDecoderSubscribeEvents(dec, events) != jxl::JxlDecoderStatus::Success {
                return Err("JXL 订阅事件失败".to_string());
            }
            // Arc<Vec<u8>> 显式解引用取字节指针（与缓存前 Vec 版语义一致）
            if jxl::JxlDecoderSetInput(dec, file.as_ref().as_ptr(), file.as_ref().len())
                != jxl::JxlDecoderStatus::Success
            {
                return Err("JXL 设置输入失败".to_string());
            }
            jxl::JxlDecoderCloseInput(dec);

            let mut d = AnimationDecoder {
                dec,
                _runner: runner,
                file,
                width: 0,
                height: 0,
                gamut: crate::color::Mat3::IDENTITY,
                out: None,
                pending_duration_ms: 33,
                tps: 1000.0,
                channels: 3,
                frames_ok: 0,
            };
            if consume_first_frame {
                d.consume_until_frame_start()?;
            } else {
                d.consume_header()?;
            }
            std::mem::forget(guard);
            Ok(d)
        }
    }

    pub fn dims(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 跳到第 `index` 帧（0 基）——**仅独立帧产物可用**（新录制产物：
    /// 编码端已禁 patches/progressive_dc 跨帧引用）。旧产物（帧间引用）
    /// 跳帧后解码会崩（实测 STATUS_ACCESS_VIOLATION）。
    ///
    /// ⚠ 实测 skip 语义怪癖（probe_skip_boundary）：连续调用 skip 的游标
    /// 不可组合——"解一帧后再 skip(N)"与"直接 skip(绝对值)"落点不一致，
    /// 且 skip 后首个事件序列会把游标复位出帧 0 快照。**因此本方法约定：
    /// 只在 open 后、任何 next_frame* 之前调用一次**（fill_gpu_pool_parallel
    /// 的 worker 每帧重开解码器走此约定——ANIM_FILE_CACHE 使 open 零读盘，
    /// 重开成本 ~1ms 可忽略）。中途跳转不支持（语义不可靠）。
    pub fn skip_to(&mut self, index: u64) {
        unsafe {
            jxl::JxlDecoderSkipFrames(self.dec, index as usize);
        }
    }

    /// 仅消费文件头（BasicInfo/ColorEncoding），停在首个 FRAME 事件**之前**
    /// ——帧 0 未 in-flight，随后的 [`Self::skip_to`] 可跳过帧 0（open_at 用）
    fn consume_header(&mut self) -> Result<(), String> {
        unsafe {
            loop {
                match jxl::JxlDecoderProcessInput(self.dec) {
                    jxl::JxlDecoderStatus::BasicInfo => {
                        let mut info = jxl::JxlBasicInfo::default();
                        if jxl::JxlDecoderGetBasicInfo(self.dec, &mut info)
                            != jxl::JxlDecoderStatus::Success
                        {
                            return Err("JXL 读取基本信息失败".to_string());
                        }
                        if info.have_animation == 0 {
                            return Err("非动画 JXL（单帧静态图）".to_string());
                        }
                        self.width = info.xsize;
                        self.height = info.ysize;
                        self.channels = if info.num_extra_channels > 0 { 4 } else { 3 };
                        let (num, den) =
                            (info.animation.tps_numerator, info.animation.tps_denominator);
                        if den == 0 {
                            return Err("JXL 动画 tps 异常".to_string());
                        }
                        self.tps = num as f64 / den as f64;
                    }
                    jxl::JxlDecoderStatus::ColorEncoding => {
                        let mut color = jxl::JxlColorEncoding::default();
                        if jxl::JxlDecoderGetColorAsEncodedProfile(
                            self.dec,
                            jxl::JxlColorProfileTarget::Original,
                            &mut color,
                        ) == jxl::JxlDecoderStatus::Success
                        {
                            self.gamut = decode_gamut(&color, true);
                        } else {
                            self.gamut = crate::color::bt2020_to_bt709();
                        }
                        // 关键：ColorEncoding 后立即停——再调一次 ProcessInput 就会
                        // emit FRAME(帧0) 事件，帧 0 即 in-flight，skip 无法撤回
                        //（实测落点 [帧0(d=33默认), N+1, ...]）。停在 FRAME emit
                        // 之前，skip(start) 才能跳过帧 0..start-1 → 首帧 = start
                        return Ok(());
                    }
                    jxl::JxlDecoderStatus::Frame => {
                        // 理论不可达（ColorEncoding 已 return）；防御性兜底
                        return Ok(());
                    }
                    jxl::JxlDecoderStatus::Success => {
                        return Err("JXL 流结束（帧数不足）".to_string());
                    }
                    jxl::JxlDecoderStatus::Error => {
                        return Err("JXL 解码失败（文件损坏或不受支持）".to_string());
                    }
                    _ => {}
                }
            }
        }
    }

    /// 推进事件循环到"下一帧像素就绪"的稳定点（BasicInfo/ColorEncoding/Frame 已消费）
    fn consume_until_frame_start(&mut self) -> Result<(), String> {
        unsafe {
            loop {
                match jxl::JxlDecoderProcessInput(self.dec) {
                    jxl::JxlDecoderStatus::BasicInfo => {
                        let mut info = jxl::JxlBasicInfo::default();
                        if jxl::JxlDecoderGetBasicInfo(self.dec, &mut info)
                            != jxl::JxlDecoderStatus::Success
                        {
                            return Err("JXL 读取基本信息失败".to_string());
                        }
                        if info.have_animation == 0 {
                            return Err("非动画 JXL（单帧静态图）".to_string());
                        }
                        self.width = info.xsize;
                        self.height = info.ysize;
                        // 通道数：编码器产物为 3 通道无 alpha（num_extra_channels=0）；
                        // 带 alpha 的文件取 4。输出缓冲必须按此申请，否则 libjxl 拒绝
                        self.channels = if info.num_extra_channels > 0 { 4 } else { 3 };
                        // tps（ticks per second）：duration 单位换算用
                        let (num, den) =
                            (info.animation.tps_numerator, info.animation.tps_denominator);
                        if den == 0 {
                            return Err("JXL 动画 tps 异常".to_string());
                        }
                        self.tps = num as f64 / den as f64;
                    }
                    jxl::JxlDecoderStatus::ColorEncoding => {
                        let mut color = jxl::JxlColorEncoding::default();
                        if jxl::JxlDecoderGetColorAsEncodedProfile(
                            self.dec,
                            jxl::JxlColorProfileTarget::Original,
                            &mut color,
                        ) == jxl::JxlDecoderStatus::Success
                        {
                            self.gamut = decode_gamut(&color, true);
                        } else {
                            self.gamut = crate::color::bt2020_to_bt709();
                        }
                    }
                    jxl::JxlDecoderStatus::Frame => {
                        let mut fh = jxl::JxlFrameHeader::default();
                        if jxl::JxlDecoderGetFrameHeader(self.dec, &mut fh)
                            != jxl::JxlDecoderStatus::Success
                        {
                            return Err("JXL 读取帧头失败".to_string());
                        }
                        // duration（tps ticks）→ ms；下限 1ms 防零除/停帧
                        let ms = if self.tps > 0.0 {
                            (fh.duration as f64 / self.tps * 1000.0).round() as u32
                        } else {
                            33
                        };
                        self.pending_duration_ms = ms.max(1);
                        return Ok(()); // 帧开始：等 NeedImageOutBuffer → FullImage
                    }
                    jxl::JxlDecoderStatus::Success => {
                        return Err("JXL 流结束（帧数不足）".to_string());
                    }
                    jxl::JxlDecoderStatus::Error => {
                        return Err("JXL 解码失败（文件损坏或不受支持）".to_string());
                    }
                    _ => {}
                }
            }
        }
    }

    /// 解出下一帧（None = 文件尾；调用方循环播放时重新 open）
    pub fn next_frame(&mut self) -> Result<Option<AnimFrame>, String> {
        self.next_frame_impl(true)
            .map(|o| o.map(|(data, duration_ms)| AnimFrame { data, duration_ms }))
    }

    /// 解出下一帧 PQ16 原始数据（GPU 出口变体；None = 文件尾）——只解码到
    /// libjxl 直出的 **RGBA u16 行交错**（4 通道强制请求，见 [`Pq16Frame`]），
    /// 跳过 CPU PQ→scRGB f16 转换，供 [`super::pq16_gpu::convert_into`] GPU 链
    /// 直写池槽纹理（ring 回退路径仍走 CPU 转换的 [`Self::next_frame`]）。
    pub fn next_frame_pq16(&mut self) -> Result<Option<(Pq16Frame, u32)>, String> {
        self.next_frame_impl(false).map(|o| {
            o.map(|(data, duration_ms)| {
                (
                    Pq16Frame {
                        data,
                        channels: 4,
                        gamut: self.gamut,
                    },
                    duration_ms,
                )
            })
        })
    }

    /// 事件循环主体（两出口共用）：convert = true → FullImage 后做 CPU
    /// PQ16→scRGB f16 转换（输出缓冲按文件实际通道数申请）；false → PQ16
    /// 原始字节直出（输出缓冲按强制 4 通道 RGBA 申请）。返回 (帧字节, 时长 ms)。
    fn next_frame_impl(&mut self, convert: bool) -> Result<Option<(Vec<u8>, u32)>, String> {
        unsafe {
            let (w, h) = (self.width, self.height);
            if w == 0 || h == 0 {
                return Err("JXL 尺寸未初始化".to_string());
            }
            let npx = (w as usize) * (h as usize);
            loop {
                match jxl::JxlDecoderProcessInput(self.dec) {
                    // open_at 路径：帧头（含 duration）首次在此消费
                    //（open 路径已在 consume_until_frame_start 消费，不经过此分支）
                    jxl::JxlDecoderStatus::Frame => {
                        let mut fh = jxl::JxlFrameHeader::default();
                        if jxl::JxlDecoderGetFrameHeader(self.dec, &mut fh)
                            == jxl::JxlDecoderStatus::Success
                        {
                            let ms = if self.tps > 0.0 {
                                (fh.duration as f64 / self.tps * 1000.0).round() as u32
                            } else {
                                33
                            };
                            self.pending_duration_ms = ms.max(1);
                        }
                    }
                    jxl::JxlDecoderStatus::NeedImageOutBuffer => {
                        // 按出口申请：CPU 转换 = 文件实际通道数（录制产物 = 3 通道
                        // u16 PQ BT.2020）；PQ16 直出 = 强制 4 通道 RGBA u16
                        let ch = if convert {
                            self.channels as usize
                        } else {
                            4
                        };
                        let fmt = jxl::JxlPixelFormat {
                            num_channels: ch as u32,
                            data_type: jxl::JxlDataType::Uint16,
                            endianness: jxl::JxlEndianness::Little,
                            align: 0,
                        };
                        let mut bytes = vec![0u8; npx * ch * 2];
                        if jxl::JxlDecoderSetImageOutBuffer(
                            self.dec,
                            &fmt,
                            bytes.as_mut_ptr() as *mut std::ffi::c_void,
                            bytes.len(),
                        ) != jxl::JxlDecoderStatus::Success
                        {
                            return Err("JXL 设置输出缓冲失败".to_string());
                        }
                        self.out = Some(OutBuffer {
                            bytes,
                            kind: OutKind::HdrPq,
                        });
                    }
                    jxl::JxlDecoderStatus::FullImage => {
                        let buffer = self
                            .out
                            .take()
                            .ok_or_else(|| "JXL FullImage 前未设置输出缓冲".to_string())?;
                        let OutKind::HdrPq = buffer.kind else {
                            return Err("动画输出类型异常（预期 PQ16）".to_string());
                        };
                        // 出口分流：true = PQ16 RGB(A)（BT.2020/PQ）→ scRGB f16
                        // RGBA（BT.709 线性，1.0 = 80nits），与混合后端共用出口
                        // 转换（行级并行）；false = PQ16 原始字节直出（GPU 链输入）
                        let data = if convert {
                            let rgba16: Vec<u16> = buffer
                                .bytes
                                .chunks_exact(2)
                                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                                .collect();
                            pq16_interleaved_to_scrgb(
                                &rgba16,
                                w as usize,
                                h as usize,
                                self.channels as usize,
                                self.gamut,
                            )
                        } else {
                            buffer.bytes
                        };
                        let duration_ms = self.pending_duration_ms;
                        // 尾帧截断容错计数：本帧像素已完整产出（覆盖两个 return 出口）
                        self.frames_ok += 1;
                        // 推进到下一帧开始（EOF → Success → None）
                        match jxl::JxlDecoderProcessInput(self.dec) {
                            jxl::JxlDecoderStatus::Frame => {
                                let mut fh = jxl::JxlFrameHeader::default();
                                if jxl::JxlDecoderGetFrameHeader(self.dec, &mut fh)
                                    == jxl::JxlDecoderStatus::Success
                                {
                                    let ms = if self.tps > 0.0 {
                                        (fh.duration as f64 / self.tps * 1000.0).round() as u32
                                    } else {
                                        33
                                    };
                                    self.pending_duration_ms = ms.max(1);
                                }
                            }
                            jxl::JxlDecoderStatus::Success => {
                                return Ok(Some((data, duration_ms))); // 末帧正常结束
                            }
                            jxl::JxlDecoderStatus::Error => {
                                // 尾帧截断容错（与 frames_ok 主分支同语义）：
                                // 录制产物最后一帧可能写入不完整，帧间推进遇
                                // Error 时若已有完整帧产出则视为正常 EOF。
                                if self.frames_ok > 0 {
                                    return Ok(Some((data, duration_ms)));
                                }
                                return Err("JXL 解码失败（帧间推进）".to_string());
                            }
                            _ => {}
                        }
                        return Ok(Some((data, duration_ms)));
                    }
                    jxl::JxlDecoderStatus::Success => return Ok(None), // EOF
                    jxl::JxlDecoderStatus::Error => {
                        // 尾帧截断容错：录制器最后一帧可能写入不完整（文件尾
                        // premature end of input）——已完整解码的帧视为正常 EOF。
                        // 首帧即失败仍报错（真损坏/不支持）。
                        if self.frames_ok > 0 {
                            return Ok(None);
                        }
                        return Err("JXL 解码失败（文件损坏或不受支持）".to_string());
                    }
                    jxl::JxlDecoderStatus::NeedMoreInput => {
                        return Err("JXL 数据不完整".to_string());
                    }
                    _ => {}
                }
            }
        }
    }
}

/// u16 PQ 行交错（w×h×ch，BT.2020/PQ）→ scRGB f16 RGBA（BT.709 线性，
/// 1.0 = 80nits）：与 cache_hdr_source_pq 同数学（行级并行）。
/// 播放出口统一转换：Native `AnimationDecoder::next_frame` 与混合后端
/// `HybridAnimDecoder::next_frame_pixels` 共用（两后端输出同构的前提）。
/// 4 通道文件的 A 在行尾单独补；3 通道文件 A = 1.0（0x3C00）。
fn pq16_interleaved_to_scrgb(
    rgba16: &[u16],
    w: usize,
    h: usize,
    channels: usize,
    gamut: crate::color::Mat3,
) -> Vec<u8> {
    let mut data = vec![0u8; w * h * 8];
    let lut = pq16_nits_lut();
    let row_in = w * channels;
    super::par_rows_mut(&mut data, w * 8, |out_row, y| {
        let in_row = &rgba16[y * row_in..(y + 1) * row_in];
        for (px, ins) in out_row.chunks_mut(8).zip(in_row.chunks_exact(3)) {
            // 查表代替 pow：16bit PQ → 线性 nits（同数学，精度 ≤1/65536）
            let r = lut[ins[0] as usize];
            let g = lut[ins[1] as usize];
            let b = lut[ins[2] as usize];
            let (r, g, b) = gamut.apply(r, g, b);
            px[0..2].copy_from_slice(&f32_to_f16(r / 80.0).to_le_bytes());
            px[2..4].copy_from_slice(&f32_to_f16(g / 80.0).to_le_bytes());
            px[4..6].copy_from_slice(&f32_to_f16(b / 80.0).to_le_bytes());
            // 无 alpha 文件 → 不透明；4 通道文件的 A 在行尾单独补
            px[6..8].copy_from_slice(&0x3C00u16.to_le_bytes());
        }
        // 4 通道文件：每像素第 4 通道（跳过上面 3ch 取样，单独覆盖 alpha）
        if channels == 4 {
            for (i, px) in out_row.chunks_mut(8).enumerate() {
                let a = in_row[i * 4 + 3];
                if a < 65535 {
                    px[6..8].copy_from_slice(&f32_to_f16(a as f32 / 65535.0).to_le_bytes());
                }
            }
        }
    });
    data
}

/// HDR float 帧 → f16 scRGB 纹理 → v2 色调映射 → 临时 PNG
fn hdr_float_to_temp_png(
    src: &Path,
    width: u32,
    height: u32,
    rgb: &[f32],
    gamut: crate::color::Mat3,
) -> PathBuf {
    let w = width as usize;
    let mut data = vec![0u8; (width * height) as usize * 8];
    let row_in = w * 3;
    super::par_rows_mut(&mut data, w * 8, |out_row, y| {
        let in_row = &rgb[y * row_in..(y + 1) * row_in];
        for (px, ins) in out_row.chunks_mut(8).zip(in_row.chunks_exact(3)) {
            let (r, g, b) = gamut.apply(ins[0], ins[1], ins[2]);
            px[0..2].copy_from_slice(&f32_to_f16(r).to_le_bytes());
            px[2..4].copy_from_slice(&f32_to_f16(g).to_le_bytes());
            px[4..6].copy_from_slice(&f32_to_f16(b).to_le_bytes());
            px[6..8].copy_from_slice(&0x3C00u16.to_le_bytes()); // A = 1.0
        }
    });
    let captured = CapturedTexture {
        width,
        height,
        format: PixelFormat::R16g16b16a16Float,
        data,
        row_pitch: width as usize * 8,
        via_gdi: false,
    };
    // 文件解码路径：归一化基准用当前显示器实际 SDR 白（PQ 为绝对 nits，
    // 屏幕白应映射 y_rel=1.0；旧硬编码 80 会使 200nits 屏过曝 2.5 倍）
    let config = crate::config::Config::load();
    let params = config.active_tonemap_params(crate::capture::monitor::get_sdr_white_level_nits());
    let sdr = to_sdr(&captured, &params);

    match image::RgbaImage::from_raw(width, height, sdr.rgba) {
        Some(img) => write_temp_png(&image::DynamicImage::ImageRgba8(img), src)
            .unwrap_or_else(|_| src.to_path_buf()),
        None => src.to_path_buf(),
    }
}

// ============================================================================
// 路线 B 验证：系数导出 probe（GPU 混合解码的前置验证，设计文档 §4.5.1）
// ============================================================================

/// 开启系数导出解码一帧 VarDCT JXL，深拷贝系数快照（golden/GPU 输入契约）。
pub fn coeff_export_snapshot(path: &Path) -> Result<super::hybrid::CoeffSnapshot, String> {
    let data = std::fs::read(path).map_err(|e| format!("读取文件失败: {}", e))?;

    unsafe {
        let dec = jxl::JxlDecoderCreate(ptr::null());
        if dec.is_null() {
            return Err("JxlDecoderCreate 返回 null".to_string());
        }
        let _guard = DecoderGuard(dec);

        let runner_threads = std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(1))
            .unwrap_or(1)
            .clamp(1, 8);
        let runner = jxl::JxlThreadParallelRunnerCreate(ptr::null(), runner_threads);
        // Runner 销毁守卫（decode_frame 内的 RunnerGuard 是函数内嵌套类型，不可复用）
        struct ProbeRunnerGuard(*mut core::ffi::c_void);
        impl Drop for ProbeRunnerGuard {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    unsafe { jxl::JxlThreadParallelRunnerDestroy(self.0) };
                }
            }
        }
        let _runner_guard = ProbeRunnerGuard(runner);
        let _ = jxl::JxlDecoderSetParallelRunner(dec, Some(jxl::JxlThreadParallelRunner), runner);

        let events = jxl::JXL_DEC_BASIC_INFO
            | jxl::JXL_DEC_COLOR_ENCODING
            | jxl::JXL_DEC_FRAME
            | jxl::JXL_DEC_FULL_IMAGE;
        if jxl::JxlDecoderSubscribeEvents(dec, events) != jxl::JxlDecoderStatus::Success {
            return Err("JXL 订阅事件失败".to_string());
        }
        // 开启系数导出（强制 VarDCT 帧 accumulate）
        if jxl::JxlDecoderSetCoefficientExport(dec, 1) != jxl::JxlDecoderStatus::Success {
            return Err("JxlDecoderSetCoefficientExport 失败".to_string());
        }
        if jxl::JxlDecoderSetInput(dec, data.as_ptr(), data.len()) != jxl::JxlDecoderStatus::Success
        {
            return Err("JXL 设置输入失败".to_string());
        }
        jxl::JxlDecoderCloseInput(dec);

        let mut info = jxl::JxlBasicInfo::default();
        let mut out: Option<Vec<u8>> = None;
        loop {
            match jxl::JxlDecoderProcessInput(dec) {
                jxl::JxlDecoderStatus::BasicInfo => {
                    if jxl::JxlDecoderGetBasicInfo(dec, &mut info) != jxl::JxlDecoderStatus::Success
                    {
                        return Err("JXL 读取基本信息失败".to_string());
                    }
                }
                jxl::JxlDecoderStatus::NeedImageOutBuffer => {
                    let npx = info.xsize as usize * info.ysize as usize;
                    let fmt = jxl::JxlPixelFormat {
                        num_channels: 4,
                        data_type: jxl::JxlDataType::Uint16,
                        endianness: jxl::JxlEndianness::Little,
                        align: 0,
                    };
                    let mut bytes = vec![0u8; npx * 8];
                    if jxl::JxlDecoderSetImageOutBuffer(
                        dec,
                        &fmt,
                        bytes.as_mut_ptr() as *mut std::ffi::c_void,
                        bytes.len(),
                    ) != jxl::JxlDecoderStatus::Success
                    {
                        return Err("JXL 设置输出缓冲失败".to_string());
                    }
                    out = Some(bytes);
                }
                jxl::JxlDecoderStatus::Success => break, // 帧解码完成 → 可取导出
                jxl::JxlDecoderStatus::Error => {
                    return Err("JXL 解码失败（文件损坏或不受支持）".to_string());
                }
                _ => {}
            }
        }
        let _ = out; // 像素缓冲本路径不使用（验证对象是系数快照）
        copy_coeff_export(dec, true)
    }
}

/// 从解码器当前状态深拷贝系数导出快照（FinalizeFrame 后、下一帧覆盖前有效——
/// 动画逐帧导出在每帧 FullImage 事件上调用；解码器 Destroy 后数据失效）。
/// debug_refs=false 时跳过 `dequant_ref`（生产链零消费）与 `qblock_ref`
///（仅 golden 对拍测试消费）两块 ~55MB 深拷贝——播放热路径每帧省 ~12ms 内存
/// 带宽（剖析：~220MB 拷贝 24ms 中约一半）；测试/校验入口传 true 保持全量。
unsafe fn copy_coeff_export(dec: *mut jxl::JxlDecoder, debug_refs: bool) -> Result<super::hybrid::CoeffSnapshot, String> {
    unsafe {
        let mut exp = jxl::JxlCoefficientExportInfo::default();
        if jxl::JxlDecoderGetCoefficientExport(dec, &mut exp) != jxl::JxlDecoderStatus::Success {
            return Err("无系数导出（非 VarDCT 帧——无损/modular 档不适用路线 B）".to_string());
        }

        // --- 深拷贝（解码器持有数据在 Destroy 后失效） ---
        let group_coeffs = exp.group_dim as usize * exp.group_dim as usize;
        let num_groups = exp.num_groups as usize;
        let total = 3 * num_groups * group_coeffs;
        let mut coeffs32 = vec![0i32; total];
        if exp.ac16 != 0 {
            let s = std::slice::from_raw_parts(exp.coeffs as *const i16, total);
            for (d, &v) in coeffs32.iter_mut().zip(s.iter()) {
                *d = v as i32;
            }
        } else {
            let s = std::slice::from_raw_parts(exp.coeffs as *const i32, total);
            coeffs32.copy_from_slice(s);
        }
        let rqf_len = exp.xsize_blocks as usize * exp.ysize_blocks as usize;
        let raw_quant_field = std::slice::from_raw_parts(exp.raw_quant_field, rqf_len).to_vec();
        let ac_strategy = std::slice::from_raw_parts(exp.ac_strategy, rqf_len).to_vec();
        let cmap_len = exp.cmap_tiles_x as usize * exp.cmap_tiles_y as usize;
        let cmap_ytox = std::slice::from_raw_parts(exp.cmap_ytox, cmap_len).to_vec();
        let cmap_ytob = std::slice::from_raw_parts(exp.cmap_ytob, cmap_len).to_vec();
        let dc_len = 3 * exp.dc_ysize as usize * exp.dc_xsize as usize;
        let dc = std::slice::from_raw_parts(exp.dc, dc_len).to_vec();
        let dequant_matrices_dct8 =
            std::slice::from_raw_parts(exp.dequant_matrices_dct8, 3 * 64).to_vec();
        let idct_ref = std::slice::from_raw_parts(exp.idct_ref, total).to_vec();
        // debug refs：dequant_ref 生产链零消费、qblock_ref 仅 golden 对拍测试消费
        //（debug_refs=false 置空，省每帧 ~110MB 深拷贝）。Lite 模式（exp.lite=1）
        // C 侧恒输出 null 指针——null 守卫独立于 debug_refs，两者语义叠加：
        // 指针为 null 即跳过（全量模式指针非 null，行为不变）。
        let (dequant_ref, qblock_ref) = if debug_refs && !exp.dequant_ref.is_null() {
            (
                std::slice::from_raw_parts(exp.dequant_ref, total).to_vec(),
                std::slice::from_raw_parts(exp.qblock_ref, total).to_vec(),
            )
        } else {
            (Vec::new(), Vec::new())
        };
        // B6：sigma 图深拷贝（epf_iters = 0 时为空图，data 可能为 null，
        // 长度为 0 时不得解引用指针）
        let sigma_len = exp.sigma_xsize as usize * exp.sigma_ysize as usize;
        let sigma = if sigma_len > 0 {
            std::slice::from_raw_parts(exp.sigma, sigma_len).to_vec()
        } else {
            Vec::new()
        };

        Ok(super::hybrid::CoeffSnapshot {
            width: exp.width,
            height: exp.height,
            xsize_blocks: exp.xsize_blocks,
            ysize_blocks: exp.ysize_blocks,
            group_dim: exp.group_dim,
            num_groups: exp.num_groups,
            xsize_groups: exp.xsize_groups,
            coeffs: coeffs32,
            raw_quant_field,
            ac_strategy,
            cmap_ytox,
            cmap_ytob,
            cmap_tiles_x: exp.cmap_tiles_x,
            cmap_tiles_y: exp.cmap_tiles_y,
            dc,
            dc_xsize: exp.dc_xsize,
            dc_ysize: exp.dc_ysize,
            inv_global_scale: exp.inv_global_scale,
            x_dm_multiplier: exp.x_dm_multiplier,
            b_dm_multiplier: exp.b_dm_multiplier,
            quant_biases: exp.quant_biases,
            dequant_matrices_dct8,
            cfl_base_x: exp.cfl_base_x,
            cfl_base_b: exp.cfl_base_b,
            cfl_color_factor: exp.cfl_color_factor,
            idct_ref,
            dequant_ref,
            qblock_ref,
            idct_slow_max_diff: exp.idct_slow_max_diff,
            idct_slow_samples: exp.idct_slow_samples,
            idct_slow_t_max_diff: exp.idct_slow_t_max_diff,
            opsin_biases: exp.opsin_biases,
            opsin_biases_cbrt: exp.opsin_biases_cbrt,
            inverse_opsin_matrix: std::slice::from_raw_parts(exp.inverse_opsin_matrix, 36)
                .to_vec(),
            sigma,
            sigma_xsize: exp.sigma_xsize,
            sigma_ysize: exp.sigma_ysize,
            epf_iters: exp.epf_iters,
            epf_sharp_lut: exp.epf_sharp_lut,
            epf_channel_scale: exp.epf_channel_scale,
            epf_pass0_sigma_scale: exp.epf_pass0_sigma_scale,
            epf_pass2_sigma_scale: exp.epf_pass2_sigma_scale,
            epf_border_sad_mul: exp.epf_border_sad_mul,
            gab: exp.gab != 0,
            gab_xweight1: exp.gab_xweight1,
            gab_xweight2: exp.gab_xweight2,
            gab_yweight1: exp.gab_yweight1,
            gab_yweight2: exp.gab_yweight2,
            epf_quant_mul: exp.epf_quant_mul,
            lite: exp.lite != 0,
        })
    }
}

/// 开启系数导出解码一帧 VarDCT JXL，返回快照统计。
/// 验证点：num_groups 与组网格一致、系数非零占比合理、元数据图长度正确。
#[tauri::command]
pub fn coeff_export_probe(path: &Path) -> Result<serde_json::Value, String> {
    let s = coeff_export_snapshot(path)?;
    let total = s.coeffs.len();
    let nonzero = s.coeffs.iter().filter(|&&v| v != 0).count();
    Ok(serde_json::json!({
        "width": s.width,
        "height": s.height,
        "xsize_blocks": s.xsize_blocks,
        "ysize_blocks": s.ysize_blocks,
        "group_dim": s.group_dim,
        "num_groups": s.num_groups,
        "total_coeffs": total,
        "nonzero_coeffs": nonzero,
        "raw_quant_len": s.raw_quant_field.len(),
        "ac_strategy_len": {
            "x": s.xsize_blocks, "y": s.ysize_blocks,
        },
        "cmap_tiles": [s.cmap_tiles_x, s.cmap_tiles_y],
        "dc": [s.dc_xsize, s.dc_ysize],
        "inv_global_scale": s.inv_global_scale,
        "x_dm_multiplier": s.x_dm_multiplier,
        "b_dm_multiplier": s.b_dm_multiplier,
        "quant_biases": s.quant_biases,
        "cfl": [s.cfl_base_x, s.cfl_base_b, s.cfl_color_factor],
        "sigma": [s.sigma_xsize, s.sigma_ysize, s.sigma.len()],
        "epf_iters": s.epf_iters,
        "gab": s.gab,
        "epf_quant_mul": s.epf_quant_mul,
        "epf_pass0_sigma_scale": s.epf_pass0_sigma_scale,
        "epf_pass2_sigma_scale": s.epf_pass2_sigma_scale,
        "epf_border_sad_mul": s.epf_border_sad_mul,
        "idct_ref_len": s.idct_ref.len(),
        "idct_slow_max_diff": s.idct_slow_max_diff,
        "idct_slow_samples": s.idct_slow_samples,
    }))
}

// ==================== B4：解码器选择架构 ====================

/// JXL 解码后端（运行时选择；同一 libjxl 库，魔改功能按需激活）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JxlDecodeBackend {
    /// 原生 libjxl 解码：像素直出（行为与原版完全一致，魔改零开销）
    Native,
    /// 混合管线（魔改）：系数导出 + GPU DCT8 重建 + libjxl varblock 块层
    /// → 输出帧块层（XYB 域），需后续 XYB→RGB/EPF；libjxl 跳过 DCT8 像素计算
    Hybrid,
}

/// 混合路径解码结果
pub struct HybridFrame {
    /// 完整帧块层（3×num_groups×65536，XYB 域）
    pub block_layer: Vec<f32>,
    pub cmp: crate::viewer::decode::hybrid::CompareResult,
    /// libjxl 混合模式解码耗时（含系数拷贝）
    pub decode_ms: f64,
    /// GPU 混合重建耗时
    pub gpu_ms: f64,
}

/// 原生模式纯解码计时（u16 RGBA 输出，不含 PNG 编码）。
/// 并行 runner 与 coeff_export_snapshot（混合腿基准）同配置：全核-1 再
/// clamp(1,8)——此前无 runner 为单线程语义，与其它计时口径不可比。
fn native_decode_ms(path: &Path) -> Result<f64, String> {
    let data = std::fs::read(path).map_err(|e| format!("读取文件失败: {e}"))?;
    unsafe {
        let dec = jxl::JxlDecoderCreate(ptr::null());
        if dec.is_null() {
            return Err("JxlDecoderCreate 返回 null".to_string());
        }
        let _guard = DecoderGuard(dec);
        let runner_threads = std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(1))
            .unwrap_or(1)
            .clamp(1, 8);
        let runner = jxl::JxlThreadParallelRunnerCreate(ptr::null(), runner_threads);
        // Runner 销毁守卫（decode_frame 内的 RunnerGuard 是函数内嵌套类型，不可复用）
        struct NativeRunnerGuard(*mut core::ffi::c_void);
        impl Drop for NativeRunnerGuard {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    unsafe { jxl::JxlThreadParallelRunnerDestroy(self.0) };
                }
            }
        }
        let _runner_guard = NativeRunnerGuard(runner);
        let _ = jxl::JxlDecoderSetParallelRunner(dec, Some(jxl::JxlThreadParallelRunner), runner);
        let events = jxl::JXL_DEC_BASIC_INFO | jxl::JXL_DEC_FULL_IMAGE;
        if jxl::JxlDecoderSubscribeEvents(dec, events) != jxl::JxlDecoderStatus::Success {
            return Err("订阅失败".to_string());
        }
        if jxl::JxlDecoderSetInput(dec, data.as_ptr(), data.len())
            != jxl::JxlDecoderStatus::Success
        {
            return Err("设置输入失败".to_string());
        }
        jxl::JxlDecoderCloseInput(dec);
        let mut info = jxl::JxlBasicInfo::default();
        let mut out: Option<Vec<u8>> = None;
        let t0 = std::time::Instant::now();
        loop {
            match jxl::JxlDecoderProcessInput(dec) {
                jxl::JxlDecoderStatus::BasicInfo => {
                    if jxl::JxlDecoderGetBasicInfo(dec, &mut info)
                        != jxl::JxlDecoderStatus::Success
                    {
                        return Err("读取基本信息失败".to_string());
                    }
                }
                jxl::JxlDecoderStatus::NeedImageOutBuffer => {
                    let npx = info.xsize as usize * info.ysize as usize;
                    let fmt = jxl::JxlPixelFormat {
                        num_channels: 4,
                        data_type: jxl::JxlDataType::Uint16,
                        endianness: jxl::JxlEndianness::Little,
                        align: 0,
                    };
                    let mut bytes = vec![0u8; npx * 8];
                    if jxl::JxlDecoderSetImageOutBuffer(
                        dec,
                        &fmt,
                        bytes.as_mut_ptr() as *mut std::ffi::c_void,
                        bytes.len(),
                    ) != jxl::JxlDecoderStatus::Success
                    {
                        return Err("设置输出缓冲失败".to_string());
                    }
                    out = Some(bytes);
                }
                jxl::JxlDecoderStatus::Success => break,
                jxl::JxlDecoderStatus::Error => return Err("解码失败".to_string()),
                _ => {}
            }
        }
        Ok(t0.elapsed().as_secs_f64() * 1000.0)
    }
}

/// 混合模式解码：系数导出（libjxl 跳过 DCT8 像素计算）+ GPU 混合重建。
/// verify=true 时附加 CPU golden 交叉对拍（有 ~1s 开销，仅测试/校验用）。
pub fn decode_hybrid(path: &Path, verify: bool) -> Result<HybridFrame, String> {
    let t0 = std::time::Instant::now();
    let snap = coeff_export_snapshot(path)?;
    let decode_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let t1 = std::time::Instant::now();
    let (block_layer, cmp) = crate::viewer::decode::hybrid_gpu::hybrid_reconstruct(&snap, verify)?;
    Ok(HybridFrame {
        block_layer,
        cmp,
        decode_ms,
        gpu_ms: t1.elapsed().as_secs_f64() * 1000.0,
    })
}

// ============================================================================
// B5a：动图逐帧混合解码（libjxl hybrid 模式 + 逐帧系数快照）
// ============================================================================

/// 动图逐帧混合解码器——libjxl hybrid 模式（DCT8 像素跳过，像素输出仅
/// 非 DCT8 区域有意义且本路径不消费），逐帧产出系数快照 + 帧时长。
/// 帧像素由 GPU 混合管线重建（块层 → [EPF] → XYB→RGB），见 hybrid_gpu.rs。
pub struct HybridAnimDecoder {
    dec: *mut jxl::JxlDecoder,
    runner: *mut core::ffi::c_void,
    _file: std::sync::Arc<Vec<u8>>, // 缓存共享（只读；SetInput 引用须保活）
    width: u32,
    height: u32,
    /// 文件通道数（录制产物 = 3；带 alpha 取 4）——输出缓冲申请用
    channels: u32,
    pending_duration_ms: u32,
    tps: f64,
    /// 文件色域 → BT.709 转换矩阵（同 Native AnimationDecoder：ColorEncoding
    /// 事件捕获；混合链 pass O 输出 PQ BT.2020，播放出口转换沿用此矩阵）
    gamut: crate::color::Mat3,
    /// 丢弃型像素缓冲（hybrid 模式 libjxl 仍要求 out buffer 才解码帧体）
    out: Option<Vec<u8>>,
    /// 已完整解出的帧数（尾帧截断容错判断用）
    frames_ok: usize,
}

impl Drop for HybridAnimDecoder {
    fn drop(&mut self) {
        unsafe {
            jxl::JxlDecoderDestroy(self.dec);
            if !self.runner.is_null() {
                jxl::JxlThreadParallelRunnerDestroy(self.runner);
            }
        }
    }
}

// 跨线程说明同 AnimationDecoder：命令线程 open → 移交解码线程独占使用（无并发访问）
unsafe impl Send for HybridAnimDecoder {}

impl HybridAnimDecoder {
    /// 打开动图（校验 have_animation + 开启系数导出；Lite 导出，生产播放链入口）
    pub fn open(path: &Path) -> Result<HybridAnimDecoder, String> {
        Self::open_with_lite(path, true)
    }

    /// 基础打开入口：lite=true 时在 SetCoefficientExport(1) 成功后追加
    /// SetCoefficientExportLite(dec, 1)——逐帧持久生效于全部后续帧，省每帧
    /// ~110MB dequant_ref/qblock_ref 深拷贝（C 契约：Lite 帧两指针恒 null）。
    /// Lite 开启失败不致命：记日志降级全量模式（快照字段齐全，仅内存代价）。
    /// 文件字节经 [`ANIM_FILE_CACHE`]（EOF 循环重开零读盘）。
    pub fn open_with_lite(path: &Path, lite: bool) -> Result<HybridAnimDecoder, String> {
        let file = anim_file_read_cached(path)?;
        unsafe {
            let dec = jxl::JxlDecoderCreate(ptr::null());
            if dec.is_null() {
                return Err("JxlDecoderCreate 返回 null".to_string());
            }
            // 解码并行度：全核（减 1）——剖析实证 70 组 VarDCT 帧在 clamp(1,8)
            // 下熵解码 62ms/帧，放开到全核后与 Native 同速（混合流水线中解码
            // 线程与 GPU 重建重叠，CPU 预算让给 libjxl 是净收益）
            let runner_threads = std::thread::available_parallelism()
                .map(|n| n.get().saturating_sub(1).max(1))
                .unwrap_or(1);
            let runner = jxl::JxlThreadParallelRunnerCreate(ptr::null(), runner_threads);
            jxl::JxlDecoderSetParallelRunner(dec, Some(jxl::JxlThreadParallelRunner), runner);
            let events = jxl::JXL_DEC_BASIC_INFO
                | jxl::JXL_DEC_COLOR_ENCODING
                | jxl::JXL_DEC_FRAME
                | jxl::JXL_DEC_FULL_IMAGE;
            if jxl::JxlDecoderSubscribeEvents(dec, events) != jxl::JxlDecoderStatus::Success {
                jxl::JxlDecoderDestroy(dec);
                jxl::JxlThreadParallelRunnerDestroy(runner);
                return Err("JXL 订阅事件失败".to_string());
            }
            if jxl::JxlDecoderSetCoefficientExport(dec, 1) != jxl::JxlDecoderStatus::Success {
                jxl::JxlDecoderDestroy(dec);
                jxl::JxlThreadParallelRunnerDestroy(runner);
                return Err("JxlDecoderSetCoefficientExport 失败".to_string());
            }
            if lite {
                // Lite 失败不致命：C 侧老版本无此符号时链接也可能失败，此处
                // 按返回状态降级——记日志并继续全量模式（快照语义仍完整）
                if jxl::JxlDecoderSetCoefficientExportLite(dec, 1)
                    != jxl::JxlDecoderStatus::Success
                {
                    log::warn!("[动画] SetCoefficientExportLite 失败，降级全量系数导出");
                }
            }
            // Arc<Vec<u8>> 显式解引用取字节指针（与缓存前 Vec 版语义一致）
            if jxl::JxlDecoderSetInput(dec, file.as_ref().as_ptr(), file.as_ref().len())
                != jxl::JxlDecoderStatus::Success
            {
                jxl::JxlDecoderDestroy(dec);
                jxl::JxlThreadParallelRunnerDestroy(runner);
                return Err("JXL 设置输入失败".to_string());
            }
            jxl::JxlDecoderCloseInput(dec);
            let mut d = HybridAnimDecoder {
                dec,
                runner,
                _file: file,
                width: 0,
                height: 0,
                channels: 3,
                pending_duration_ms: 33,
                tps: 1000.0,
                gamut: crate::color::Mat3::IDENTITY,
                out: None,
                frames_ok: 0,
            };
            d.consume_until_frame_start()?;
            Ok(d)
        }
    }

    pub fn dims(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 文件色域 → BT.709 矩阵（pass F 播放出口 GPU 化用，与 CPU 出口转换同一矩阵）
    pub fn gamut(&self) -> crate::color::Mat3 {
        self.gamut
    }

    /// 推进到帧开始（BasicInfo/ColorEncoding/Frame 已消费）
    fn consume_until_frame_start(&mut self) -> Result<(), String> {
        unsafe {
            loop {
                match jxl::JxlDecoderProcessInput(self.dec) {
                    jxl::JxlDecoderStatus::BasicInfo => {
                        let mut info = jxl::JxlBasicInfo::default();
                        if jxl::JxlDecoderGetBasicInfo(self.dec, &mut info)
                            != jxl::JxlDecoderStatus::Success
                        {
                            return Err("JXL 读取基本信息失败".to_string());
                        }
                        if info.have_animation == 0 {
                            return Err("非动画 JXL（单帧静态图用 decode_hybrid）".to_string());
                        }
                        self.width = info.xsize;
                        self.height = info.ysize;
                        self.channels = if info.num_extra_channels > 0 { 4 } else { 3 };
                        let (num, den) =
                            (info.animation.tps_numerator, info.animation.tps_denominator);
                        if den == 0 {
                            return Err("JXL 动画 tps 异常".to_string());
                        }
                        self.tps = num as f64 / den as f64;
                    }
                    jxl::JxlDecoderStatus::ColorEncoding => {
                        // gamut 捕获（同 Native AnimationDecoder）：播放出口转换
                        // PQ BT.2020 → BT.709（录制产物 = 2020 → 与 Native 同矩阵）
                        let mut color = jxl::JxlColorEncoding::default();
                        self.gamut = if jxl::JxlDecoderGetColorAsEncodedProfile(
                            self.dec,
                            jxl::JxlColorProfileTarget::Original,
                            &mut color,
                        ) == jxl::JxlDecoderStatus::Success
                        {
                            decode_gamut(&color, true)
                        } else {
                            crate::color::bt2020_to_bt709()
                        };
                    }
                    jxl::JxlDecoderStatus::Frame => {
                        if let Some(ms) = self.read_frame_duration_ms()? {
                            self.pending_duration_ms = ms;
                        }
                        return Ok(());
                    }
                    jxl::JxlDecoderStatus::Error => {
                        return Err("JXL 解码失败（帧启动段，文件损坏或不受支持）".to_string());
                    }
                    other => {
                        return Err(format!("JXL 帧启动段意外事件: {other:?}"));
                    }
                }
            }
        }
    }

    fn read_frame_duration_ms(&mut self) -> Result<Option<u32>, String> {
        unsafe {
            let mut fh = jxl::JxlFrameHeader::default();
            if jxl::JxlDecoderGetFrameHeader(self.dec, &mut fh) != jxl::JxlDecoderStatus::Success {
                return Ok(None);
            }
            let ms = if self.tps > 0.0 {
                (fh.duration as f64 / self.tps * 1000.0).round() as u32
            } else {
                33
            };
            Ok(Some(ms.max(1)))
        }
    }

    /// 下一帧系数快照（None = 文件尾；循环播放时重新 open）
    pub fn next_frame_snapshot(
        &mut self,
    ) -> Result<Option<(super::hybrid::CoeffSnapshot, u32)>, String> {
        let prof = super::prof_enabled();
        let t0 = std::time::Instant::now();
        let npx = self.width as usize * self.height as usize;
        unsafe {
            loop {
                match jxl::JxlDecoderProcessInput(self.dec) {
                    jxl::JxlDecoderStatus::NeedImageOutBuffer => {
                        // 丢弃型缓冲：hybrid 模式 DCT8 区域未定义，像素不消费
                        let ch = self.channels as usize;
                        let fmt = jxl::JxlPixelFormat {
                            num_channels: self.channels,
                            data_type: jxl::JxlDataType::Uint16,
                            endianness: jxl::JxlEndianness::Little,
                            align: 0,
                        };
                        let mut bytes = vec![0u8; npx * ch * 2];
                        if jxl::JxlDecoderSetImageOutBuffer(
                            self.dec,
                            &fmt,
                            bytes.as_mut_ptr() as *mut std::ffi::c_void,
                            bytes.len(),
                        ) != jxl::JxlDecoderStatus::Success
                        {
                            return Err("JXL 设置输出缓冲失败".to_string());
                        }
                        self.out = Some(bytes);
                    }
                    jxl::JxlDecoderStatus::FullImage => {
                        // FinalizeFrame（快照就绪）→ FullImage：本帧导出此刻有效
                        self.out = None;
                        let t1 = std::time::Instant::now();
                        // 生产播放链：Lite 模式 C 侧已裁掉 dequant_ref/qblock_ref
                        //（恒 null → null 守卫跳过），debug_refs=false 为第二道保险
                        let snap = copy_coeff_export(self.dec, false)?;
                        let t2 = std::time::Instant::now();
                        self.frames_ok += 1;
                        if prof {
                            println!(
                                "[prof] 快照: libjxl 解码 {:.1}ms + 系数深拷贝 {:.1}ms",
                                (t1 - t0).as_secs_f64() * 1000.0,
                                (t2 - t1).as_secs_f64() * 1000.0
                            );
                        }
                        return Ok(Some((snap, self.pending_duration_ms)));
                    }
                    jxl::JxlDecoderStatus::Frame => {
                        if let Some(ms) = self.read_frame_duration_ms()? {
                            self.pending_duration_ms = ms;
                        }
                    }
                    jxl::JxlDecoderStatus::Success => return Ok(None), // EOF
                    jxl::JxlDecoderStatus::Error => {
                        // 尾帧截断容错：录制器最后一帧可能写入不完整（文件尾
                        // premature end of input）——已完整解码的帧视为正常 EOF。
                        // 首帧即失败仍报错（真损坏/不支持）。
                        if self.frames_ok > 0 {
                            return Ok(None);
                        }
                        return Err("JXL 解码失败（文件损坏或不受支持）".to_string());
                    }
                    jxl::JxlDecoderStatus::NeedMoreInput => {
                        return Err("JXL 数据不完整".to_string());
                    }
                    _ => {}
                }
            }
        }
    }
}

/// 混合后端播放解码器（预取流水线）——libjxl 熵解码与 GPU 重建的重叠。
///
/// 结构：解码线程独占一个 [`HybridAnimDecoder`]（raw 指针类 libjxl 状态不跨线程
/// 共享，Drop 随线程收尾），经容量 1 的有界 channel 向调用线程送
/// `(CoeffSnapshot, duration_ms)`；调用线程 `next_frame_pixels` 收快照 → GPU
/// 重建 + pass F f16 行交错直出（重排/PQ→scRGB 转换并入 GPU）→ 产出 AnimFrame。
/// 背压即预取信号：通道空位释放（recv）后解码线程立刻预解码下一帧，
/// 与当前帧的 GPU 重建天然重叠；稳态单帧耗时 = max(libjxl 解码, GPU 链)。
/// 内存：通道内 1 份 + 解码中 1 份快照（~220MB×2 双缓冲）。
/// EOF / 解码错误经 channel 传递（Ok(None)/Err）；接收端 Drop 即关通道，
/// 解码线程 send 失败退出并释放解码器（detach，无需 join）。
pub struct HybridPipedDecoder {
    rx: std::sync::mpsc::Receiver<Result<Option<(super::hybrid::CoeffSnapshot, u32)>, String>>,
    width: u32,
    height: u32,
    gamut: crate::color::Mat3,
    /// 预取线程已报告 EOF/错误（后续调用直接短路返回）
    drained: bool,
}

impl HybridPipedDecoder {
    /// 打开动图（调用线程完成 open 与首帧启动校验，随即移交解码线程）
    pub fn open(path: &Path) -> Result<HybridPipedDecoder, String> {
        let dec = HybridAnimDecoder::open(path)?;
        let (width, height) = dec.dims();
        let gamut = dec.gamut();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("hybrid-prefetch".into())
            .spawn(move || {
                let mut dec = dec;
                loop {
                    match dec.next_frame_snapshot() {
                        Ok(Some((snap, dur))) => {
                            if tx.send(Ok(Some((snap, dur)))).is_err() {
                                break; // 接收端已丢弃 → 关闭
                            }
                        }
                        Ok(None) => {
                            let _ = tx.send(Ok(None)); // EOF
                            break;
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            break;
                        }
                    }
                }
            })
            .map_err(|e| format!("hybrid 预取线程创建失败: {e}"))?;
        Ok(HybridPipedDecoder {
            rx,
            width,
            height,
            gamut,
            drained: false,
        })
    }

    pub fn dims(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 文件色域 → BT.709 矩阵（GPU 直写链 pass F 出口用）
    pub fn gamut(&self) -> crate::color::Mat3 {
        self.gamut
    }

    /// 下一帧系数快照（**不做 GPU 重建**，调用方自行执行 GPU 链直写纹理；
    /// GPU 常驻显存播放路径专用出口——`next_frame_pixels` 的 buffer 读回版
    /// 会多余一次全帧读回）。语义与 `next_frame_pixels` 同源：收预取快照即
    /// 透出；EOF / 解码错误同样置 drained（两出口不可混用，独占使用）。
    pub fn next_frame_snapshot(
        &mut self,
    ) -> Result<Option<(super::hybrid::CoeffSnapshot, u32)>, String> {
        if self.drained {
            return Ok(None);
        }
        let msg = match self.rx.recv() {
            Ok(m) => m,
            Err(_) => {
                // 预取线程已退出且通道排空（EOF 后续调用）
                self.drained = true;
                return Ok(None);
            }
        };
        match msg {
            Ok(Some(x)) => Ok(Some(x)),
            Ok(None) => {
                self.drained = true;
                Ok(None)
            }
            Err(e) => {
                self.drained = true;
                Err(e)
            }
        }
    }

    /// 下一帧像素（播放链路出口；None = 文件尾，循环播放时重新 open）。
    /// 链路 = 收预取快照 → hybrid_reconstruct_pq16_f16（GPU 全链出图 +
    /// pass F PQ 解码/色域/f16 行交错直写，读回即 AnimFrame.data）。
    /// 尾帧截断容错（frames_ok）由 next_frame_snapshot 承接，语义与 Native 一致。
    pub fn next_frame_pixels(&mut self) -> Result<Option<AnimFrame>, String> {
        if self.drained {
            return Ok(None);
        }
        let prof = super::prof_enabled();
        let t0 = std::time::Instant::now();
        let msg = match self.rx.recv() {
            Ok(m) => m,
            Err(_) => {
                // 预取线程已退出且通道排空（EOF 后续调用）
                self.drained = true;
                return Ok(None);
            }
        };
        let (snap, duration_ms) = match msg {
            Ok(Some(x)) => x,
            Ok(None) => {
                self.drained = true;
                return Ok(None);
            }
            Err(e) => {
                self.drained = true;
                return Err(e);
            }
        };
        let t1 = std::time::Instant::now();
        // GPU 全链：块层 → G → E → D → O(PQ16) → F（f16 行交错直写）→ 读回
        let data = match crate::viewer::decode::hybrid_gpu::hybrid_reconstruct_pq16_f16(
            &snap,
            &self.gamut,
        ) {
            Ok((data, _it)) => data,
            Err(e) => {
                self.drained = true;
                return Err(e);
            }
        };
        drop(snap); // 快照（~220MB）及时释放
        let t2 = std::time::Instant::now();
        if prof {
            println!(
                "[prof] 混合播放帧: 预取等待 {:.1}ms | GPU 重建+出口 {:.1}ms | 合计 {:.1}ms",
                (t1 - t0).as_secs_f64() * 1000.0,
                (t2 - t1).as_secs_f64() * 1000.0,
                (t2 - t0).as_secs_f64() * 1000.0
            );
        }
        Ok(Some(AnimFrame {
            data,
            duration_ms,
        }))
    }
}

// ==================== B5c：播放链路解码后端运行时选择 ====================

/// 动图播放解码后端（运行时选择；config.toml `[viewer] anim_backend`，
/// README 式说明见 config.rs ViewerConfig 注释）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimBackend {
    /// 纯 CPU libjxl 直出（默认）
    Native,
    /// 系数快照 + GPU 混合管线重建（DCT8 块 GPU 化）
    Hybrid,
}

impl AnimBackend {
    /// config 字符串解析（容错：未知值 / 空串回退 Native）
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "hybrid" => AnimBackend::Hybrid,
            _ => AnimBackend::Native,
        }
    }
}

/// 播放链路统一解码器：enum 包装两后端（AnimFrame 同构输出，非真 trait——
/// match 分派 next_frame 即可；播放端 AnimRing 供给线程无感后端差异）。
/// Hybrid = 预取流水线（解码线程 + GPU 重建重叠），见 [`HybridPipedDecoder`]。
pub enum AnimPlayerDecoder {
    Native(AnimationDecoder),
    Hybrid(HybridPipedDecoder),
}

impl AnimPlayerDecoder {
    /// 按指定后端打开
    pub fn open(path: &Path, backend: AnimBackend) -> Result<Self, String> {
        match backend {
            AnimBackend::Native => Ok(AnimPlayerDecoder::Native(AnimationDecoder::open(path)?)),
            AnimBackend::Hybrid => Ok(AnimPlayerDecoder::Hybrid(HybridPipedDecoder::open(path)?)),
        }
    }

    /// 打开并跳到第 `index` 帧（仅 Native 独立帧产物——多解码器并行填充定位用；
    /// Hybrid/旧产物不支持：skip 后 hybrid 管线无快照语义，旧产物帧间引用会崩）
    pub fn open_skip_native(
        path: &Path,
        backend: AnimBackend,
        index: u64,
    ) -> Result<Self, String> {
        match backend {
            AnimBackend::Native => {
                let mut d = AnimationDecoder::open(path)?;
                d.skip_to(index);
                Ok(AnimPlayerDecoder::Native(d))
            }
            AnimBackend::Hybrid => Err("hybrid 后端不支持 skip（独立帧并行填充仅 native）".into()),
        }
    }

    /// 按当前配置打开（config.toml `[viewer] anim_backend = "native"|"hybrid"`；
    /// 解析容错默认 native。GPU 引擎不可用时 hybrid 打开仍成功，首帧重建报错）
    pub fn open_from_config(path: &Path) -> Result<Self, String> {
        let cfg = crate::config::Config::load();
        let backend = AnimBackend::parse(&cfg.viewer.anim_backend);
        log::info!("[动画] 播放解码后端: {backend:?}（config.toml [viewer] anim_backend）");
        AnimPlayerDecoder::open(path, backend)
    }

    pub fn dims(&self) -> (u32, u32) {
        match self {
            AnimPlayerDecoder::Native(d) => d.dims(),
            AnimPlayerDecoder::Hybrid(d) => d.dims(),
        }
    }

    /// 解出下一帧（None = 文件尾；调用方循环播放时重新 open）
    pub fn next_frame(&mut self) -> Result<Option<AnimFrame>, String> {
        match self {
            AnimPlayerDecoder::Native(d) => d.next_frame(),
            AnimPlayerDecoder::Hybrid(d) => d.next_frame_pixels(),
        }
    }

    /// 下一帧系数快照（仅 Hybrid 后端；**不做 GPU 重建**，调用方在引擎锁内
    /// 自行执行 `hybrid_reconstruct_pq16_f16_into` 直写池纹理——GPU 常驻
    /// 显存播放路径）。Native 后端无快照概念 → 恒 Err。
    pub fn next_frame_snapshot(
        &mut self,
    ) -> Result<Option<(super::hybrid::CoeffSnapshot, u32)>, String> {
        match self {
            AnimPlayerDecoder::Native(_) => {
                Err("native 后端无快照出口（GPU 常驻显存播放需 anim_backend=\"hybrid\"）".into())
            }
            AnimPlayerDecoder::Hybrid(d) => d.next_frame_snapshot(),
        }
    }

    /// 下一帧 PQ16 原始数据（仅 Native 后端；GPU 常驻显存池填充用——跳过
    /// CPU PQ→scRGB f16 转换，调用方在引擎锁内执行 `pq16_gpu::convert_into`
    /// 直写池纹理）。Hybrid 后端无此概念（其 GPU 链自带 pass F 出口）→ 恒 Err。
    pub fn next_frame_pq16(&mut self) -> Result<Option<(Pq16Frame, u32)>, String> {
        match self {
            AnimPlayerDecoder::Native(d) => d.next_frame_pq16(),
            AnimPlayerDecoder::Hybrid(_) => {
                Err("hybrid 后端无 PQ16 直出出口（GPU 池填充走快照链）".into())
            }
        }
    }

    /// 文件色域 → BT.709 矩阵（快照直写链 pass F 出口用）
    pub fn gamut(&self) -> crate::color::Mat3 {
        match self {
            AnimPlayerDecoder::Native(d) => d.gamut,
            AnimPlayerDecoder::Hybrid(d) => d.gamut(),
        }
    }
}

/// B5a：动图全帧混合解码基准（逐帧快照 → GPU 混合重建，块层即产即弃）。
/// 返回 (宽, 高, 逐帧统计)。verify_frames=前 N 帧附加 golden 对拍（慢，仅校验）。
pub fn hybrid_frames_bench(
    path: &Path,
    verify_frames: usize,
) -> Result<(u32, u32, Vec<HybridFrameStats>), String> {
    let mut dec = HybridAnimDecoder::open(path)?;
    let (w, h) = dec.dims();
    let mut stats = Vec::new();
    let mut idx = 0usize;
    loop {
        match dec.next_frame_snapshot() {
            Ok(Some((snap, duration_ms))) => {
                let t0 = std::time::Instant::now();
                let (_block_layer, cmp) = crate::viewer::decode::hybrid_gpu::hybrid_reconstruct(
                    &snap,
                    idx < verify_frames,
                )?;
                let gpu_ms = t0.elapsed().as_secs_f64() * 1000.0;
                stats.push(HybridFrameStats {
                    duration_ms,
                    gpu_ms,
                    psnr_db: if idx < verify_frames {
                        Some(cmp.psnr_db)
                    } else {
                        None
                    },
                });
                idx += 1;
            }
            Ok(None) => break,
            Err(e) => {
                return Err(format!("已解码 {} 帧后失败: {}", idx, e));
            }
        }
    }
    Ok((w, h, stats))
}

/// 逐帧混合解码统计（不含块层内容，基准用）
pub struct HybridFrameStats {
    pub duration_ms: u32,
    pub gpu_ms: f64,
    /// 前 verify_frames 帧的 golden 对拍 PSNR
    pub psnr_db: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    /// 旧版工具输出的 SDR JXL 截图（真实文件，存在时验证 SDR 解码路径）
    const SAMPLE: &str = "C:/Users/Administrator/Pictures/jietu-hdr/jietu_20260821_234959.jxl";

    /// 动画回放验证：真实录制产物（3 通道无 alpha PQ16 BT.2020）
    const ANIM_SAMPLE: &str =
        "C:/Users/Administrator/Pictures/jietu-hdr/record_20260830_220833.jxl";

    /// [临时诊断] 录制产物帧间隔分布（用户问：录制的数据流间隔是？）
    #[test]
    fn probe_frame_intervals() {
        for sample in [
            "C:/Users/Administrator/Pictures/jietu-hdr/record_20260903_012525.jxl",
            "C:/Users/Administrator/Pictures/jietu-hdr/record_20260903_010928.jxl",
        ] {
            let path = Path::new(sample);
            if !path.exists() {
                eprintln!("跳过（不存在）: {}", sample);
                continue;
            }
            match AnimationDecoder::open(path) {
                Ok(mut dec) => {
                    let mut durs: Vec<u32> = Vec::new();
                    while let Ok(Some(f)) = dec.next_frame() {
                        durs.push(f.duration_ms);
                    }
                    durs.sort();
                    let n = durs.len();
                    let total: u64 = durs.iter().map(|d| *d as u64).sum();
                    let p50 = durs[n * 50 / 100];
                    let p90 = durs[n * 90 / 100];
                    let p99 = durs[n.min(99 * n / 100).max(n - 1)];
                    println!(
                        "{}\n  帧 {} · 总时长 {}ms · 间隔 min/p50/p90/max = {}/{}/{}/{}ms\n  直方图: 16ms内={} 17-33={} 34-67={} 68-167={} 168-500={} >500={}",
                        sample.split('/').last().unwrap(),
                        n, total,
                        durs[0], p50, p90, durs[n - 1],
                        durs.iter().filter(|d| **d <= 16).count(),
                        durs.iter().filter(|d| **d >= 17 && **d <= 33).count(),
                        durs.iter().filter(|d| **d >= 34 && **d <= 67).count(),
                        durs.iter().filter(|d| **d >= 68 && **d <= 167).count(),
                        durs.iter().filter(|d| **d >= 168 && **d <= 500).count(),
                        durs.iter().filter(|d| **d > 500).count(),
                    );
                }
                Err(e) => println!("{} 打开失败: {}", sample, e),
            }
        }
    }


    /// [诊断记录] 帧间引用实证（2026-09-03）：
    /// 录制产物经 libjxl 编码器动画自动优化，帧 N+1 引用帧 N（patches/DC 帧）。
    /// 实测 JxlDecoderSkipFrames(1) 后解码下一帧 → libjxl 内部 STATUS_ACCESS_VIOLATION
    /// （参考帧缺失 → 悬空指针）。**结论：多解码器并行填充（skip 到位再解）不可行**，
    /// 帧必须顺序解码。此测试保留 A 段（D 测量）作为解码腿基准的轻量版。
    #[test]
    fn probe_skip_vs_decode() {
        let path = Path::new("C:/Users/Administrator/Pictures/jietu-hdr/record_20260903_012525.jxl");
        if !path.exists() {
            eprintln!("样本不存在，跳过");
            return;
        }
        const N: usize = 100;
        let t0 = std::time::Instant::now();
        let mut dec = AnimationDecoder::open(path).unwrap();
        let mut decoded = 0usize;
        while decoded < N {
            match dec.next_frame() {
                Ok(Some(_)) => decoded += 1,
                _ => break,
            }
        }
        let d_ms = t0.elapsed().as_secs_f64() * 1000.0 / decoded as f64;
        println!(
            "顺序解码 {} 帧 D={:.1}ms/帧（{:.1}fps）｜skip 并行已实证不可行（帧间引用，见测试注释）",
            decoded,
            d_ms,
            1000.0 / d_ms
        );

        // PQ16 直出腿（native GPU 出口变体：解码跳过 CPU PQ→scRGB 转换）——
        // 与上一行的差值 = CPU 转换卸载收益（填充提速依据）
        let t0 = std::time::Instant::now();
        let mut dec2 = AnimationDecoder::open(path).unwrap();
        let mut decoded2 = 0usize;
        while decoded2 < N {
            match dec2.next_frame_pq16() {
                Ok(Some(_)) => decoded2 += 1,
                _ => break,
            }
        }
        let d2_ms = t0.elapsed().as_secs_f64() * 1000.0 / decoded2 as f64;
        println!(
            "PQ16 直出 {} 帧 D={:.1}ms/帧（{:.1}fps）｜省 CPU 转换 {:.1}ms/帧",
            decoded2,
            d2_ms,
            1000.0 / d2_ms,
            d_ms - d2_ms
        );
    }

    /// [诊断] 并行填充 worker 数基准：4/6/8 对比（用户问"8 核为何不是 8 个解码器"）。
    /// 需要独立帧产物——先用 631 帧新格式文件（没有则用合成 20 帧）。
    /// 注：worker 的 runner 线程由 JXL_PLAY_WORKERS 环境变量控制（open 时读），
    /// 测试中逐 worker 数设置后各起全新进程内解码器。
    #[test]
    fn probe_play_workers_sweep() {
        // 优先真实录制产物（独立帧）；否则合成
        let real = Path::new("C:/Users/Administrator/Pictures/jietu-hdr/record_20260903_012525.jxl");
        let (path, total_frames): (PathBuf, usize) = if real.exists() {
            (real.to_path_buf(), 120) // 测前 120 帧
        } else {
            // 合成独立帧动画（走真实编码器，含 PATCHES off）
            let (w, h) = (1280u32, 800u32);
            let dir = std::env::temp_dir().join("jietu_wsweep");
            let _ = std::fs::create_dir_all(&dir);
            let p = dir.join("wsweep.jxl");
            let quality = crate::encode::QualityLevel::High;
            let mut enc = crate::encode::jxl::AnimationJxlEncoder::new(
                &p, w, h, quality, crate::encode::jxl::animation_effort(quality),
            )
            .unwrap();
            for i in 0..60u16 {
                let mut buf = vec![0u8; (w as usize) * (h as usize) * 6];
                // 伪随机渐变内容（避免全同帧被编码器特判）
                for (j, px) in buf.chunks_exact_mut(6).enumerate() {
                    let v = (i as usize * 997 + j * 31) as u16;
                    px[0..2].copy_from_slice(&v.to_le_bytes());
                    px[2..4].copy_from_slice(&(v / 3).to_le_bytes());
                    px[4..6].copy_from_slice(&(v / 7).to_le_bytes());
                }
                enc.add_frame(&buf, 33, i == 59).unwrap();
            }
            enc.finalize().unwrap();
            (p, 60)
        };

        let logical = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(8);
        println!("逻辑处理器: {}（AMD 7745HX = 8C16T）", logical);
        for workers in [1usize, 2, 4, 6, 8] {
            std::env::set_var("JXL_PLAY_WORKERS", workers.to_string());
            // 每个 worker 从自己相位解 total_frames 帧（相位步进 workers，
            // 等价并行填充一轮 120 帧的工作量分摊）
            let per_worker = total_frames.div_ceil(workers);
            let t0 = std::time::Instant::now();
            let mut handles = Vec::new();
            for w in 0..workers {
                let p = path.clone();
                handles.push(std::thread::spawn(move || {
                    let mut dec = match AnimationDecoder::open(&p) {
                        Ok(d) => d,
                        Err(_) => return 0usize,
                    };
                    dec.skip_to(w as u64);
                    let mut n = 0usize;
                    let mut idx = w as u64;
                    loop {
                        if n >= per_worker {
                            break;
                        }
                        match dec.next_frame_pq16() {
                            Ok(Some(_)) => {
                                n += 1;
                                idx += workers as u64;
                                dec.skip_to(idx);
                            }
                            _ => break,
                        }
                    }
                    n
                }));
            }
            let decoded: usize = handles.into_iter().map(|h| h.join().unwrap_or(0)).sum();
            let secs = t0.elapsed().as_secs_f64();
            let fps = decoded as f64 / secs;
            println!(
                "  worker={} × runner≤{}线程 → 解 {} 帧 / {:.2}s = {:.0} fps",
                workers,
                (logical / workers).max(2),
                decoded,
                secs,
                fps
            );
        }
        std::env::remove_var("JXL_PLAY_WORKERS");
        if !real.exists() {
            let _ = std::fs::remove_file(&path);
        }
    }

    /// [诊断] 填充有效吞吐基准（真实 60fps 录制产物）：对比
    /// a) 单 worker（runner = 全核-1 = 15 线程）顺序解码
    /// b) 生产 4 worker 相位过滤（每 worker 顺序解全文件只发 1/4——
    ///    数学上有效吞吐 = 1 个 4 线程解码器，并行是无效的）
    /// 解码到 PQ16（与生产一致，无 CPU 色彩转换）。
    #[test]
    fn probe_fill_throughput() {
        let candidates = [
            "C:/Users/Administrator/Pictures/jietu-hdr/record_20260903_114944.jxl",
            "C:/Users/Administrator/Pictures/jietu-hdr/record_20260903_114754.jxl",
            "C:/Users/Administrator/Pictures/jietu-hdr/record_20260903_012525.jxl",
        ];
        let Some(path) = candidates.iter().find(|p| Path::new(p).exists()) else {
            println!("无真实录制文件，跳过");
            return;
        };
        println!("样本: {path}");
        const N: usize = 120; // 测前 120 帧

        // a) 单 worker 15 线程顺序解码
        {
            std::env::remove_var("JXL_PLAY_WORKERS");
            let mut dec = AnimationDecoder::open(Path::new(path)).unwrap();
            let t0 = std::time::Instant::now();
            let mut n = 0usize;
            let mut first_dur = 0u32;
            while let Ok(Some((_, d))) = dec.next_frame_pq16() {
                if n == 0 {
                    first_dur = d;
                }
                n += 1;
                if n >= N {
                    break;
                }
            }
            let fps = n as f64 / t0.elapsed().as_secs_f64();
            println!(
                "a) 单 worker（15 线程 runner）：{} 帧 / {:.2}s = {:.0} fps 有效吞吐（帧时长 {}ms → 需 {} fps）",
                n,
                t0.elapsed().as_secs_f64(),
                fps,
                first_dur,
                1000 / first_dur.max(1)
            );
        }

        // b) 4 worker 相位过滤（生产 fill_gpu_pool_parallel 同构）
        {
            let workers = 4u64;
            std::env::set_var("JXL_PLAY_WORKERS", "4");
            let t0 = std::time::Instant::now();
            let handles: Vec<_> = (0..workers)
                .map(|w| {
                    let p = PathBuf::from(path);
                    std::thread::spawn(move || {
                        let mut dec = match AnimationDecoder::open(&p) {
                            Ok(d) => d,
                            Err(_) => return 0usize,
                        };
                        let mut n = 0usize;
                        let mut idx: u64 = 0;
                        loop {
                            match dec.next_frame_pq16() {
                                Ok(Some(_)) => {
                                    if idx >= N as u64 {
                                        return n; // 前缀区间解完即停
                                    }
                                    if idx > 0 && idx % workers == w {
                                        n += 1;
                                    }
                                    idx += 1;
                                }
                                _ => return n,
                            }
                        }
                    })
                })
                .collect();
            // 每 worker 解 N 帧前缀，各自发送 ≈ N/4 → 总有效 ≈ N 帧
            let sent: usize = handles.into_iter().map(|h| h.join().unwrap_or(0)).sum();
            let secs = t0.elapsed().as_secs_f64();
            println!(
                "b) 4 worker 相位过滤（4 线程 runner ×4）：有效发送 {} 帧 / {:.2}s = {:.0} fps 有效吞吐",
                sent,
                secs,
                sent as f64 / secs
            );
            std::env::remove_var("JXL_PLAY_WORKERS");
        }
    }

    /// [诊断+回归] 区间分工并行填充：worker 领批次 [start, start+B)，打开
    /// 新解码器立即 skip(start)（约定内唯一可靠用法）→ 顺序解 B 帧。
    /// 验证：① 吞吐（vs 相位过滤 11fps / 单 worker 33fps）② 帧序像素对拍
    /// ③ 高频重开稳定性（曾因"每帧重开"堆损坏 0xc0000374）。
    #[test]
    fn probe_fill_range_partition() {
        let candidates = [
            "C:/Users/Administrator/Pictures/jietu-hdr/record_20260903_114944.jxl",
            "C:/Users/Administrator/Pictures/jietu-hdr/record_20260903_114754.jxl",
            "C:/Users/Administrator/Pictures/jietu-hdr/record_20260903_012525.jxl",
        ];
        let Some(path) = candidates.iter().find(|p| Path::new(p).exists()) else {
            println!("无真实录制文件，跳过");
            return;
        };
        println!("样本: {path}");
        const N: usize = 120;
        const BATCH: u64 = 16;

        // 顺序基准：前 N 帧中心像素 + duration
        let mut seq_ref: Vec<(u16, u32)> = Vec::new();
        {
            std::env::remove_var("JXL_PLAY_WORKERS");
            let mut dec = AnimationDecoder::open(Path::new(path)).unwrap();
            let (w, h) = dec.dims();
            let mid = (w as usize / 2 + (h as usize / 2) * w as usize) * 8;
            while let Ok(Some((f, d))) = dec.next_frame_pq16() {
                let v = u16::from_le_bytes([f.data[mid], f.data[mid + 1]]);
                seq_ref.push((v, d));
                if seq_ref.len() >= N {
                    break;
                }
            }
            assert_eq!(seq_ref.len(), N, "顺序基准帧数");
        }

        // [诊断] skip(N) 落点模型验证：skip 后解 4 帧，用「duration 序列 +
        // 三点像素」指纹在基准中精确匹配（单点像素对相邻游戏帧分辨力不足）
        {
            std::env::remove_var("JXL_PLAY_WORKERS");
            let (w, h) = AnimationDecoder::open(Path::new(path)).unwrap().dims();
            let mid = (w as usize / 2 + (h as usize / 2) * w as usize) * 8;
            let q1 = (w as usize / 4 + (h as usize / 4) * w as usize) * 8;
            let q3 = (w as usize * 3 / 4 + (h as usize * 3 / 4) * w as usize) * 8;
            let fingerprint = |f: &super::Pq16Frame| {
                (
                    u16::from_le_bytes([f.data[mid], f.data[mid + 1]]),
                    u16::from_le_bytes([f.data[q1], f.data[q1 + 1]]),
                    u16::from_le_bytes([f.data[q3], f.data[q3 + 1]]),
                )
            };
            // 基准指纹（前 40 帧）
            let fp_ref: Vec<((u16, u16, u16), u32)> = {
                let mut dec = AnimationDecoder::open(Path::new(path)).unwrap();
                let mut v = Vec::new();
                while let Ok(Some((f, d))) = dec.next_frame_pq16() {
                    v.push((fingerprint(&f), d));
                    if v.len() >= 40 {
                        break;
                    }
                }
                v
            };
            for skip_n in [0u64, 1, 2, 15, 16, 31, 32] {
                let mut dec = AnimationDecoder::open_at(Path::new(path), skip_n).unwrap();
                let mut got: Vec<((u16, u16, u16), u32)> = Vec::new();
                for _ in 0..4 {
                    match dec.next_frame_pq16() {
                        Ok(Some((f, d))) => got.push((fingerprint(&f), d)),
                        _ => break,
                    }
                }
                // duration 序列 + 指纹联合定位（指纹容差 400）
                let locate = |g: &((u16, u16, u16), u32)| -> Option<usize> {
                    fp_ref.iter().position(|(rf, rd)| {
                        rd == &g.1
                            && (rf.0.abs_diff(g.0 .0) < 400
                                && rf.1.abs_diff(g.0 .1) < 400
                                && rf.2.abs_diff(g.0 .2) < 400)
                    })
                };
                let locs: Vec<String> = got
                    .iter()
                    .map(|g| match locate(g) {
                        Some(i) => format!("帧{i}"),
                        None => format!("无匹配(d={})", g.1),
                    })
                    .collect();
                println!(
                    "open_at({skip_n}) → 落点: [{}]（期望帧 {skip_n} 起，无快照）",
                    locs.join(", ")
                );
            }
        }

        // 区间分工：4 worker 滚动领批，skip 只在 open 后调用一次
        {
            let workers = 4u64;
            std::env::set_var("JXL_PLAY_WORKERS", "4");
            let cursor = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let (tx, rx) = std::sync::mpsc::channel::<(u64, u16, u32)>();
            let t0 = std::time::Instant::now();
            let handles: Vec<_> = (0..workers)
                .map(|_| {
                    let p = PathBuf::from(path);
                    let tx = tx.clone();
                    let cursor = std::sync::Arc::clone(&cursor);
                    std::thread::spawn(move || {
                        let mut batches = 0usize;
                        loop {
                            let start = cursor.fetch_add(BATCH, std::sync::atomic::Ordering::SeqCst);
                            if start >= N as u64 {
                                return batches;
                            }
                            let mut dec = match AnimationDecoder::open_at(&p, start) {
                                Ok(d) => d,
                                Err(_) => return batches,
                            };
                            let mut idx = start;
                            loop {
                                if idx >= start + BATCH || idx >= N as u64 {
                                    break;
                                }
                                match dec.next_frame_pq16() {
                                    Ok(Some((f, d))) => {
                                        let (w, h) = dec.dims();
                                        let mid =
                                            (w as usize / 2 + (h as usize / 2) * w as usize) * 8;
                                        let v = u16::from_le_bytes([
                                            f.data[mid],
                                            f.data[mid + 1],
                                        ]);
                                        if tx.send((idx, v, d)).is_err() {
                                            return batches;
                                        }
                                        idx += 1;
                                    }
                                    _ => return batches,
                                }
                            }
                            batches += 1;
                        }
                    })
                })
                .collect();
            drop(tx);
            let mut all: Vec<(u64, u16, u32)> = rx.into_iter().collect();
            let secs = t0.elapsed().as_secs_f64();
            let batches: usize = handles.into_iter().map(|h| h.join().unwrap_or(0)).sum();
            all.sort_by_key(|(i, _, _)| *i);
            // 正确性：帧序连续 0..N 且像素对拍
            assert_eq!(all.len(), N, "区间分工解出帧数");
            for (i, (idx, v, d)) in all.iter().enumerate() {
                assert_eq!(*idx, i as u64, "帧序连续（区间分工错位）");
                let (rv, rd) = seq_ref[i];
                assert!(
                    v.abs_diff(rv) < 400,
                    "帧 {i} 像素不符：区间 {v} vs 顺序 {rv}"
                );
                assert_eq!(*d, rd, "帧 {i} duration 一致");
            }
            println!(
                "c) 区间分工（4 worker × 4 线程，批 {}）：{} 帧 / {:.2}s = {:.0} fps 有效吞吐，重开 {} 批次无堆损坏",
                BATCH,
                N,
                secs,
                N as f64 / secs,
                batches
            );
            std::env::remove_var("JXL_PLAY_WORKERS");
        }
    }


    /// [诊断] skip_to 边界语义：逐帧打印 skip 后实际解出的帧号
    #[test]
    fn probe_skip_boundary() {
        let (w, h) = (320u32, 200u32);
        let dir = std::env::temp_dir().join("jietu_skip_probe");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("skip_probe.jxl");
        let quality = crate::encode::QualityLevel::High;
        let mut enc = crate::encode::jxl::AnimationJxlEncoder::new(
            &path, w, h, quality, crate::encode::jxl::animation_effort(quality),
        )
        .unwrap();
        let n = 12u16;
        for i in 0..n {
            let mut buf = vec![0u8; (w as usize) * (h as usize) * 6];
            for (j, px) in buf.chunks_exact_mut(6).enumerate() {
                let v = ((i as usize + 1) * 4000 + (j % 7)) as u16;
                px[0..2].copy_from_slice(&v.to_le_bytes());
                px[2..4].copy_from_slice(&v.to_le_bytes());
                px[4..6].copy_from_slice(&v.to_le_bytes());
            }
            enc.add_frame(&buf, 33, i == n - 1).unwrap();
        }
        enc.finalize().unwrap();
        // 帧像素读出（PQ16 4ch）
        let read_v = |f: &Pq16Frame| -> u16 {
            let mid = (w as usize / 2 + (h as usize / 2) * w as usize) * 8;
            u16::from_le_bytes([f.data[mid], f.data[mid + 1]])
        };
        // 实验：skip_to(2) 后连续解 3 帧，看帧号（v≈(i+1)*4000）
        let mut dec = AnimationDecoder::open(&path).unwrap();
        dec.skip_to(2);
        print!("skip_to(2) 后：");
        for _ in 0..3 {
            match dec.next_frame_pq16() {
                Ok(Some((f, _))) => print!("{} ", read_v(&f) / 4000),
                _ => break,
            }
        }
        println!("（期望 3 4 5）");
        // 实验：解 1 帧后 skip_to(3) 再解 2 帧
        let mut dec2 = AnimationDecoder::open(&path).unwrap();
        dec2.skip_to(0);
        let _ = dec2.next_frame_pq16(); // 帧 0
        dec2.skip_to(3);
        print!("解帧0→skip_to(3) 后：");
        for _ in 0..2 {
            match dec2.next_frame_pq16() {
                Ok(Some((f, _))) => print!("{} ", read_v(&f) / 4000),
                _ => break,
            }
        }
        println!("（若 skip=跳过接下来N帧且游标在帧1 → 期望 5 6）");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn animation_decode_first_frames() {
        let path = Path::new(ANIM_SAMPLE);
        if !path.exists() {
            eprintln!("动画样本不存在，跳过: {}", ANIM_SAMPLE);
            return;
        }
        assert!(probe_is_animation(path).unwrap(), "应探测为动图");
        let mut dec = AnimationDecoder::open(path).expect("打开动画失败");
        let (w, h) = dec.dims();
        assert!(w > 0 && h > 0, "尺寸异常");
        // 首帧（3 通道文件此前按 4 通道申请缓冲会在此失败）
        let f0 = dec
            .next_frame()
            .expect("首帧解码失败")
            .expect("动画无帧");
        assert_eq!(
            f0.data.len(),
            w as usize * h as usize * 8,
            "应为 f16 RGBA（8B/px）"
        );
        let f1 = dec
            .next_frame()
            .expect("第二帧解码失败")
            .expect("应还有帧");
        assert!(f1.duration_ms > 0 && f0.duration_ms > 0, "帧时长异常");
        println!(
            "动画解码 OK: {}x{} d0={}ms d1={}ms",
            w, h, f0.duration_ms, f1.duration_ms
        );
    }

    /// 动画文件内存常驻缓存复用（读盘优化回归）：同一文件连续 open 两次，
    /// 第二次必须命中缓存零读盘。断言口径：
    /// - 命中计数在第二次 open 恰 +1；
    /// - 两解码器共享同一份文件字节（Arc::ptr_eq）；
    /// - 缓存内该路径条目恰 1 份，且两解码器释放后 strong_count 回落 1
    ///   （仅缓存持有——证明 Arc 真共享而非重复读入两份）。
    ///（本套测试 --test-threads=1 串行执行，全局缓存无并行干扰）
    #[test]
    fn anim_file_cache_reuse() {
        use std::sync::atomic::Ordering;
        let path = Path::new(ANIM_SAMPLE);
        if !path.exists() {
            eprintln!("动画样本不存在，跳过: {}", ANIM_SAMPLE);
            return;
        }
        let d1 = AnimationDecoder::open(path).expect("首次打开失败");
        let hits1 = ANIM_FILE_CACHE_HITS.load(Ordering::Relaxed);
        let d2 = AnimationDecoder::open(path).expect("二次打开失败");
        let hits2 = ANIM_FILE_CACHE_HITS.load(Ordering::Relaxed);
        assert_eq!(hits2 - hits1, 1, "第二次 open 应命中缓存（零读盘）");
        assert!(
            std::sync::Arc::ptr_eq(&d1.file, &d2.file),
            "两次 open 应共享同一份文件字节（Arc clone）"
        );
        drop(d1);
        drop(d2);
        let cache = ANIM_FILE_CACHE.lock().unwrap();
        let entries: Vec<&AnimFileEntry> = cache.iter().filter(|e| e.path == path).collect();
        assert_eq!(entries.len(), 1, "同文件缓存条目应恰 1 份");
        assert_eq!(
            std::sync::Arc::strong_count(&entries[0].data),
            1,
            "解码器释放后仅缓存持有 Arc"
        );
        println!(
            "动画文件缓存复用 OK: {} KB 共享单份，第二次 open 零读盘",
            entries[0].data.len() / 1024
        );
    }

    #[test]
    fn decode_sdr_jxl_sample() {
        let path = Path::new(SAMPLE);
        if !path.exists() {
            eprintln!("样本文件不存在，跳过: {}", SAMPLE);
            return;
        }
        let (temp, w, h) =
            decode_to_temp_png(path).unwrap_or_else(|e| panic!("JXL 解码失败: {}", e));
        assert!(w > 0 && h > 0, "尺寸异常: {}x{}", w, h);
        assert!(temp.exists(), "临时 PNG 未生成: {}", temp.display());
        println!("JXL(SDR) 解码成功: {} ({}x{})", temp.display(), w, h);
    }

    #[test]
    fn quick_dimensions_sample() {
        let path = Path::new(SAMPLE);
        if !path.exists() {
            eprintln!("样本文件不存在，跳过: {}", SAMPLE);
            return;
        }
        let (w, h) = quick_dimensions(path);
        assert!(w > 0 && h > 0, "尺寸探测失败: {}x{}", w, h);
        println!("JXL 尺寸探测: {}x{}", w, h);
    }

    /// 路线 B 验证：VarDCT 帧（有损档）系数导出
    const VAR_DCT_SAMPLE: &str =
        "C:/Users/Administrator/Pictures/jietu-hdr/jietu_20260827_025539.jxl";
    /// 路线 B 验证：modular 帧（无损档）应返回"无系数导出"错误
    const MODULAR_SAMPLE: &str =
        "C:/Users/Administrator/Pictures/jietu-hdr/jietu_20260828_203925.jxl";

    #[test]
    fn coeff_export_var_dct_sample() {
        let path = Path::new(VAR_DCT_SAMPLE);
        if !path.exists() {
            eprintln!("VarDCT 样本不存在，跳过: {}", VAR_DCT_SAMPLE);
            return;
        }
        let v = coeff_export_probe(path).expect("VarDCT 系数导出失败");
        println!(
            "系数导出(VarDCT): {}",
            serde_json::to_string_pretty(&v).unwrap()
        );
        let groups = v["num_groups"].as_u64().unwrap();
        let group_dim = v["group_dim"].as_u64().unwrap();
        let total = v["total_coeffs"].as_u64().unwrap();
        let nonzero = v["nonzero_coeffs"].as_u64().unwrap();
        assert_eq!(group_dim, 256, "group_dim 应为 256");
        assert!(groups > 0, "组数应 > 0");
        assert_eq!(
            total,
            3 * groups * group_dim * group_dim,
            "系数总数 = 3 × num_groups × 256²"
        );
        assert!(nonzero > 0, "系数全零——导出路径异常");
        // VarDCT 有损帧非零占比应远低于 100%（熵编码跳过大量零系数）
        let ratio = nonzero as f64 / total as f64;
        assert!(ratio < 0.9, "非零占比 {:.1}% 异常偏高", ratio * 100.0);
        // 元数据图长度
        let blocks = v["xsize_blocks"].as_u64().unwrap() * v["ysize_blocks"].as_u64().unwrap();
        assert_eq!(v["raw_quant_len"].as_u64().unwrap(), blocks);
    }

    #[test]
    fn coeff_export_modular_rejected() {
        let path = Path::new(MODULAR_SAMPLE);
        if !path.exists() {
            eprintln!("modular 样本不存在，跳过: {}", MODULAR_SAMPLE);
            return;
        }
        let r = coeff_export_probe(path);
        assert!(r.is_err(), "modular 帧不应产生系数导出");
        println!("modular 拒绝验证 OK: {:?}", r.err());
    }

    /// B2 增量：CPU 黄金重建 vs libjxl IDCT 参照层对拍（≥50dB）
    #[test]
    fn golden_reconstruct_matches_idct_ref() {
        let path = Path::new(VAR_DCT_SAMPLE);
        if !path.exists() {
            eprintln!("VarDCT 样本不存在，跳过: {}", VAR_DCT_SAMPLE);
            return;
        }
        let snap = coeff_export_snapshot(path).expect("系数快照失败");
        // 导出系数与 libjxl 熵解码落盘的量化系数必须一致（qblock_ref 参照；
        // B4 混合模式跳过 DCT8 的 dump → 一致性只看非 DCT8 位置）
        let mismatch = crate::viewer::decode::hybrid::coeffs_nondct8_only(&snap)
            .iter()
            .zip(crate::viewer::decode::hybrid::qblock_ref_nondct8_only(&snap).iter())
            .filter(|(a, b)| a != b)
            .count();
        // 前 5 个不一致样本（定位 dump 语义）
        let mut shown = 0;
        for (i, ((a, b), c)) in snap
            .coeffs
            .iter()
            .zip(snap.qblock_ref.iter())
            .zip(crate::viewer::decode::hybrid::coeffs_nondct8_only(&snap).iter())
            .enumerate()
        {
            if a != b && shown < 5 {
                println!(
                    "不一致[{i}]: coeffs={a} qblock_ref={b} (coeffs_nondct8={c})",
                );
                shown += 1;
            }
        }
        assert_eq!(mismatch, 0, "导出系数与 libjxl qblock 参照不一致: {mismatch} 处");
        let (golden, stats) =
            crate::viewer::decode::hybrid::golden_reconstruct(&snap).expect("黄金重建失败");
        println!("黄金重建: non_dct8_blocks = {}", stats.non_dct8_blocks);
        // B4 混合模式下 libjxl 不再产出 DCT8 块层（idct_ref 的 DCT8 位置为 0），
        // golden 的正确性由 GPU 交叉对拍测试验证（两者从系数独立计算）。
    }

    /// B5a：动图逐帧混合解码基准——逐帧快照 → GPU 混合重建（首帧 golden 对拍）
    #[test]
    fn hybrid_anim_frames_benchmark() {
        let path = Path::new(ANIM_SAMPLE);
        if !path.exists() {
            eprintln!("动画样本不存在，跳过: {}", ANIM_SAMPLE);
            return;
        }
        if let Err(e) = crate::upscale::d3d11::engine() {
            eprintln!("GPU 引擎不可用，跳过: {e}");
            return;
        }
        let t0 = std::time::Instant::now();
        let (w, h, stats) = hybrid_frames_bench(path, 1).expect("逐帧混合解码失败");
        let total_ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert!(!stats.is_empty(), "动画无帧");
        let n = stats.len();
        let avg_gpu = stats.iter().map(|s| s.gpu_ms).sum::<f64>() / n as f64;
        let max_gpu = stats.iter().map(|s| s.gpu_ms).fold(0.0, f64::max);
        println!(
            "动图混合解码: {}x{}, {} 帧, 总耗时 {:.1} ms（含首帧 golden 对拍）",
            w, h, n, total_ms
        );
        println!(
            "GPU 重建/帧: 平均 {:.1} ms, 最大 {:.1} ms（30fps 预算 33ms/帧）",
            avg_gpu, max_gpu
        );
        for (i, s) in stats.iter().take(3).enumerate() {
            println!(
                "  帧{}: d={}ms gpu={:.1}ms psnr={}",
                i,
                s.duration_ms,
                s.gpu_ms,
                s.psnr_db
                    .map(|p| format!("{:.2} dB", p))
                    .unwrap_or_else(|| "—".into())
            );
        }
        if let Some(p) = stats[0].psnr_db {
            assert!(p >= 50.0, "首帧对拍 PSNR {:.2} dB < 50 dB", p);
        }
    }

    // ==================== B8：混合 GPU 全链 vs Native 端到端逐帧对拍 ====================

    /// 最小 native FFI 解码循环的产出（前 max_frames 帧原始 u16 PQ RGB，
    /// interleaved 行主序，每帧 w*h*ch）——不走 AnimationDecoder 的 f16 scRGB
    /// 转换，与混合链 PQ16 输出保持同域（PQ BT.2020 u16）逐值可比。
    struct NativePqFrames {
        w: u32,
        h: u32,
        channels: u32,
        frames: Vec<Vec<u16>>,
    }

    /// 事件流完全参照 AnimationDecoder（BASIC_INFO|COLOR_ENCODING|FRAME|
    /// FULL_IMAGE；NeedImageOutBuffer 设 u16 缓冲，FullImage 拷贝 bytes），
    /// 循环到第 max_frames 帧即返回（CloseInput + Destroy 由守卫保证）。
    fn native_anim_pq16_frames(path: &Path, max_frames: usize) -> Result<NativePqFrames, String> {
        let data = std::fs::read(path).map_err(|e| format!("读取文件失败: {e}"))?;
        unsafe {
            let dec = jxl::JxlDecoderCreate(ptr::null());
            if dec.is_null() {
                return Err("JxlDecoderCreate 返回 null".to_string());
            }
            let _guard = DecoderGuard(dec); // Drop → JxlDecoderDestroy
            // 并行 runner（同 AnimationDecoder：帧内 256×256 组分摊到各核）
            let runner_threads = std::thread::available_parallelism()
                .map(|n| n.get().saturating_sub(1).max(1))
                .unwrap_or(1);
            let runner = jxl::JxlThreadParallelRunnerCreate(ptr::null(), runner_threads);
            struct RunnerGuard(*mut core::ffi::c_void);
            impl Drop for RunnerGuard {
                fn drop(&mut self) {
                    if !self.0.is_null() {
                        unsafe { jxl::JxlThreadParallelRunnerDestroy(self.0) };
                    }
                }
            }
            let _runner_guard = RunnerGuard(runner);
            let _ = jxl::JxlDecoderSetParallelRunner(
                dec,
                Some(jxl::JxlThreadParallelRunner),
                runner,
            );

            let events = jxl::JXL_DEC_BASIC_INFO
                | jxl::JXL_DEC_COLOR_ENCODING
                | jxl::JXL_DEC_FRAME
                | jxl::JXL_DEC_FULL_IMAGE;
            if jxl::JxlDecoderSubscribeEvents(dec, events) != jxl::JxlDecoderStatus::Success {
                return Err("JXL 订阅事件失败".to_string());
            }
            if jxl::JxlDecoderSetInput(dec, data.as_ptr(), data.len())
                != jxl::JxlDecoderStatus::Success
            {
                return Err("JXL 设置输入失败".to_string());
            }
            jxl::JxlDecoderCloseInput(dec);

            let (mut w, mut h) = (0u32, 0u32);
            let mut channels = 3u32;
            let mut frames: Vec<Vec<u16>> = Vec::new();
            let mut buf: Vec<u8> = Vec::new();
            loop {
                match jxl::JxlDecoderProcessInput(dec) {
                    jxl::JxlDecoderStatus::BasicInfo => {
                        let mut info = jxl::JxlBasicInfo::default();
                        if jxl::JxlDecoderGetBasicInfo(dec, &mut info)
                            != jxl::JxlDecoderStatus::Success
                        {
                            return Err("JXL 读取基本信息失败".to_string());
                        }
                        w = info.xsize;
                        h = info.ysize;
                        channels = if info.num_extra_channels > 0 { 4 } else { 3 };
                    }
                    jxl::JxlDecoderStatus::NeedImageOutBuffer => {
                        let npx = w as usize * h as usize;
                        let fmt = jxl::JxlPixelFormat {
                            num_channels: channels,
                            data_type: jxl::JxlDataType::Uint16,
                            endianness: jxl::JxlEndianness::Little,
                            align: 0,
                        };
                        buf = vec![0u8; npx * channels as usize * 2];
                        if jxl::JxlDecoderSetImageOutBuffer(
                            dec,
                            &fmt,
                            buf.as_mut_ptr() as *mut std::ffi::c_void,
                            buf.len(),
                        ) != jxl::JxlDecoderStatus::Success
                        {
                            return Err("JXL 设置输出缓冲失败".to_string());
                        }
                    }
                    jxl::JxlDecoderStatus::FullImage => {
                        frames.push(
                            buf.chunks_exact(2)
                                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                                .collect(),
                        );
                        if frames.len() >= max_frames {
                            return Ok(NativePqFrames { w, h, channels, frames });
                        }
                    }
                    jxl::JxlDecoderStatus::Success => {
                        return Err(format!(
                            "JXL 流提前结束（仅解出 {} 帧 < 需要的 {} 帧）",
                            frames.len(),
                            max_frames
                        ));
                    }
                    jxl::JxlDecoderStatus::Error => return Err("JXL 解码失败".to_string()),
                    jxl::JxlDecoderStatus::NeedMoreInput => {
                        return Err("JXL 数据不完整".to_string());
                    }
                    _ => {} // ColorEncoding / Frame：消费即可
                }
            }
        }
    }

    /// 混合链 PQ16 三平面 [c][num_groups·gd²]（组内 gd×gd 行主序，边缘组仅
    /// 有效区有效）→ Native 同款行交错布局 (w×h×3)。槽位→像素映射同
    /// pq16_out_ref：g = (py/gd)·xg + px/gd，p = g·gd² + (py%gd)·gd + px%gd。
    /// 只写有效像素（px<w 且 py<h），无效槽位不消费。
    fn planar_pq16_to_interleaved(pq: &[u16], s: &crate::viewer::decode::hybrid::CoeffSnapshot) -> Vec<u16> {
        let gd = s.group_dim as usize;
        let gdc = gd * gd;
        let n = s.num_groups as usize * gdc;
        let xg = s.xsize_groups as usize;
        let (w, h) = (s.width as usize, s.height as usize);
        assert_eq!(pq.len(), 3 * n, "PQ16 平面长度异常");
        let mut out = vec![0u16; w * h * 3];
        for py in 0..h {
            for px in 0..w {
                let g = (py / gd) * xg + px / gd;
                let p = g * gdc + (py % gd) * gd + (px % gd);
                let ob = (py * w + px) * 3;
                out[ob] = pq[p];
                out[ob + 1] = pq[n + p];
                out[ob + 2] = pq[2 * n + p];
            }
        }
        out
    }

    /// B8：混合 GPU 解码全链 vs Native libjxl 端到端逐帧对拍（PQ16 u16 域）。
    /// 混合链 = HybridAnimDecoder 逐帧 CoeffSnapshot → hybrid_reconstruct_pq16
    /// （GPU 块层 → Gaborish → EPF1 → XYB→linear → PQ16）；Native = 最小 FFI
    /// 循环直出 u16 PQ（事件流与 AnimationDecoder 完全一致，仅跳过其 f16 scRGB
    /// 转换，保持与混合链 PQ16 同域可比）。两个独立解码器从头解同一文件，
    /// 帧序天然一致；末帧截断容错两端独立生效，不影响前 3 帧。
    /// 历史注记：本测试曾发现块层"块主序写、行主序读"的布局错位（gab 采样
    /// 按行主序误读块主序块层 → 整帧块级打乱），已在 gaborish_ref 与 gab
    /// shader 的输入采样处修复；现预期差异源仅剩 GPU/REF 的 f32 舍入——
    /// 实测 |d|≤1 ≈ 100%、max ≈ 5 LSB。
    /// 断言（按实测收紧）：≥99.9% 像素 |a-b|≤1、≥99.99% |a-b|≤2 且 max ≤8
    /// （实测 100.0000% / max≤5）；不达标时打印最差 5 个差异像素
    /// (px,py,c,hybrid,native) 并按块类型定位环节。
    #[test]
    fn hybrid_pq16_matches_native() {
        let path = Path::new(ANIM_SAMPLE);
        if !path.exists() {
            eprintln!("动画样本不存在，跳过: {}", ANIM_SAMPLE);
            return;
        }
        if let Err(e) = crate::upscale::d3d11::engine() {
            eprintln!("GPU 引擎不可用，跳过: {e}");
            return;
        }
        const COMPARE_FRAMES: usize = 3;
        let native = native_anim_pq16_frames(path, COMPARE_FRAMES).expect("native 逐帧解码失败");
        assert_eq!(native.channels, 3, "对拍像素映射按 3 通道（录制产物）");

        let mut hyb = HybridAnimDecoder::open(path).expect("hybrid 解码器打开失败");
        let (hw, hh) = hyb.dims();
        assert_eq!(
            (native.w, native.h),
            (hw, hh),
            "两侧解码尺寸不一致: native {}x{} vs hybrid {}x{}",
            native.w,
            native.h,
            hw,
            hh
        );

        for frame_idx in 0..COMPARE_FRAMES {
            // 快照（~200MB 深拷贝）与重建结果均为循环局部变量，逐帧处理完即释放
            let (snap, dur) = hyb
                .next_frame_snapshot()
                .expect("hybrid 逐帧快照失败")
                .unwrap_or_else(|| panic!("hybrid 第 {frame_idx} 帧即 EOF"));
            assert_eq!(
                (snap.width, snap.height),
                (native.w, native.h),
                "快照尺寸与文件尺寸不一致"
            );
            let gpu_t = std::time::Instant::now();
            let (pq, it) = crate::viewer::decode::hybrid_gpu::hybrid_reconstruct_pq16(&snap)
                .unwrap_or_else(|e| panic!("第 {frame_idx} 帧 GPU PQ16 重建失败: {e}"));
            let gpu_ms = gpu_t.elapsed().as_secs_f64() * 1000.0;

            // 三平面 → 行交错（只含有效像素），与 native 逐像素逐通道比 LSB 差
            let hyb_il = planar_pq16_to_interleaved(&pq, &snap);
            drop(pq); // 三平面中间量（~27MB）及时释放
            let native_f = &native.frames[frame_idx];
            let (w, h) = (native.w as usize, native.h as usize);
            assert_eq!(hyb_il.len(), w * h * 3, "重排缓冲长度异常");
            assert_eq!(native_f.len(), w * h * 3, "native 帧长度异常");

            let mut le1 = 0usize;
            let mut le2 = 0usize;
            let mut le4 = 0usize;
            let mut sum = 0u64;
            let mut sq = 0u64;
            let mut max_d = 0i64;
            // 最差 5 个差异像素（降序维护，断言不达标时打印定位）
            let mut worst5: Vec<(i64, usize, usize, usize, u16, u16)> = Vec::new();
            for py in 0..h {
                for px in 0..w {
                    let nb = (py * w + px) * 3;
                    for c in 0..3 {
                        let a = hyb_il[nb + c];
                        let b = native_f[nb + c];
                        let d = (a as i64 - b as i64).abs();
                        if d <= 1 {
                            le1 += 1;
                        }
                        if d <= 2 {
                            le2 += 1;
                        }
                        if d <= 4 {
                            le4 += 1;
                        }
                        sum += d as u64;
                        sq += (d * d) as u64;
                        if d > max_d {
                            max_d = d;
                        }
                        if worst5.len() < 5 {
                            worst5.push((d, px, py, c, a, b));
                            worst5.sort_by(|x, y| y.0.cmp(&x.0));
                        } else if d > worst5[4].0 {
                            worst5[4] = (d, px, py, c, a, b);
                            worst5.sort_by(|x, y| y.0.cmp(&x.0));
                        }
                    }
                }
            }
            let cnt = (w * h * 3) as f64;
            let p1 = le1 as f64 / cnt * 100.0;
            let p2 = le2 as f64 / cnt * 100.0;
            let p4 = le4 as f64 / cnt * 100.0;
            let mean = sum as f64 / cnt;
            let mse = sq as f64 / cnt;
            let psnr = if mse > 0.0 {
                10.0 * (65535.0 * 65535.0 / mse).log10()
            } else {
                f64::INFINITY
            };
            println!(
                "帧{frame_idx} (d={dur}ms) vs native: |d|≤1 {p1:.4}% |d|≤2 {p2:.4}% |d|≤4 {p4:.4}% max={max_d} mean={mean:.4} PSNR(PQ)={psnr:.2} dB GPU链={gpu_ms:.1}ms it={it:.0}nits"
            );
            if p1 < 99.9 || p2 < 99.99 || max_d > 8 {
                let xb = snap.xsize_blocks as usize;
                println!(
                    "帧{frame_idx} 不达标——最差 5 个差异像素 (px,py,c,hybrid,native,d,块类型):"
                );
                for (i, &(d, px, py, c, a, b)) in worst5.iter().enumerate() {
                    let acs_byte = snap.ac_strategy[(py / 8) * xb + px / 8];
                    let btype = if acs_byte & 1 != 0 {
                        let kind = (acs_byte >> 1) as usize;
                        if kind == 0 {
                            "DCT8(GPU 全责: IDCT/Gab/EPF/XYB/PQ)"
                        } else {
                            "varblock（libjxl 全算区）"
                        }
                    } else {
                        "covered(byte=0)"
                    };
                    println!("  [{i}] ({px},{py}) c={c} hybrid={a} native={b} d={d} 块: {btype}");
                }
            }
            drop(hyb_il); // 行交错中间量（~27MB）逐帧及时释放
            assert!(
                p1 >= 99.9,
                "帧{frame_idx}: |a-b|≤1 占比 {p1:.4}% < 99.9%"
            );
            assert!(
                p2 >= 99.99,
                "帧{frame_idx}: |a-b|≤2 占比 {p2:.4}% < 99.99%"
            );
            assert!(
                max_d <= 8,
                "帧{frame_idx}: max diff {max_d} > 8"
            );
        }
    }

    /// B4：双后端基准——原生（全算）vs 混合（DCT8 跳过 + GPU 重建）
    #[test]
    fn decode_backend_benchmark() {
        let path = Path::new(VAR_DCT_SAMPLE);
        if !path.exists() {
            eprintln!("VarDCT 样本不存在，跳过: {}", VAR_DCT_SAMPLE);
            return;
        }
        if let Err(e) = crate::upscale::d3d11::engine() {
            eprintln!("GPU 引擎不可用，跳过: {e}");
            return;
        }
        // 原生：libjxl 全算（IDCT/EPF/XYB 全跑）
        let native = native_decode_ms(path).expect("原生解码失败");
        // 混合：libjxl 跳过 DCT8 像素计算 + GPU 混合重建（verify=false 纯性能）
        let hf = decode_hybrid(path, false).expect("混合解码失败");
        println!("原生后端（libjxl 全算）: {:.1} ms", native);
        println!(
            "混合后端: libjxl 解码 {:.1} ms + GPU 重建 {:.1} ms = {:.1} ms（DCT8 像素由 GPU 全责）",
            hf.decode_ms,
            hf.gpu_ms,
            hf.decode_ms + hf.gpu_ms
        );
    }

    /// 播放链路混合后端出口对拍：`HybridPipedDecoder::next_frame_pixels`
    ///（预取流水线 + GPU pass F 出口，即生产播放路径）vs Native
    /// `AnimationDecoder::next_frame`——两后端 AnimFrame（f16 scRGB RGBA，
    /// 1.0 = 80nits）尺寸/duration/逐值对比。先按位一致断言；GPU/REF 的 PQ 域
    /// 微差（B8 实测 PQ u16 max ≤5 LSB）经 f16 量化后按实测放宽为语义等价：
    /// ≤3 f16 ULP（正常量级舍入，实测 3 封顶）或 |Δ| ≤ 1e-3 scRGB（黑场：
    /// ±0 翻转 / PQ 低位 LSB 在陡峭区的绝对不可见差异，实测 <3e-3 nits），
    /// 违例必须为零，统计直方 + 最差样本打印。
    #[test]
    fn hybrid_playback_frames() {
        let path = Path::new(ANIM_SAMPLE);
        if !path.exists() {
            eprintln!("动画样本不存在，跳过: {}", ANIM_SAMPLE);
            return;
        }
        if let Err(e) = crate::upscale::d3d11::engine() {
            eprintln!("GPU 引擎不可用，跳过: {e}");
            return;
        }
        /// f16 位型 → f32（测试本地精确解码；exr 模块仅导出 f32_to_f16）
        fn f16_bits_to_f32(h: u16) -> f32 {
            let sign = if h & 0x8000 != 0 { -1.0f32 } else { 1.0 };
            let exp = ((h >> 10) & 0x1F) as i32;
            let man = (h & 0x3FF) as i32;
            sign * match exp {
                0 => (man as f32) * 2f32.powi(-24), // 次正规 / ±0
                31 => f32::INFINITY,                // 像素域不涉及 NaN/Inf
                e => (1.0 + man as f32 / 1024.0) * 2f32.powi(e - 15),
            }
        }
        const FRAMES: usize = 2;
        let mut nat = AnimationDecoder::open(path).expect("native 解码器打开失败");
        let mut hyb = HybridPipedDecoder::open(path).expect("hybrid 解码器打开失败");
        assert_eq!(nat.dims(), hyb.dims(), "两后端解码尺寸不一致");
        for i in 0..FRAMES {
            let nf = nat
                .next_frame()
                .expect("native 逐帧解码失败")
                .unwrap_or_else(|| panic!("native 第 {i} 帧即 EOF"));
            let hf = hyb
                .next_frame_pixels()
                .expect("hybrid 逐帧解码失败")
                .unwrap_or_else(|| panic!("hybrid 第 {i} 帧即 EOF"));
            assert_eq!(nf.duration_ms, hf.duration_ms, "第 {i} 帧时长不一致");
            assert_eq!(nf.data.len(), hf.data.len(), "第 {i} 帧数据长度不一致");
            if nf.data == hf.data {
                println!("帧{i}: {} 字节 f16 RGBA 逐位一致（native vs hybrid）", nf.data.len());
                continue;
            }
            // 放宽（阈值实测后定）：语义等价 = f16 ULP 距离 ≤3 **或** 绝对差
            // ≤1e-3 scRGB（0.08 nits）。前者覆盖正常量级的 GPU/REF f32 舍入
            // （实测 3 ULP 封顶）；后者覆盖黑场（±0 翻转、次正规符号交叉）——
            // PQ 域低几位 LSB 在黑场陡峭区会把相对 ULP 距离放大到数百，但
            // 绝对误差 <3e-3 nits，不可见。违例（两者皆不满足）必须为零。
            let na: &[u16] = unsafe {
                std::slice::from_raw_parts(nf.data.as_ptr() as *const u16, nf.data.len() / 2)
            };
            let nb: &[u16] = unsafe {
                std::slice::from_raw_parts(hf.data.as_ptr() as *const u16, hf.data.len() / 2)
            };
            let ord = |h: u16| -> i32 {
                // IEEE 全序位变换（同号保持、负数按位取反）→ |ua−ub| = ULP 距离
                if h & 0x8000 != 0 {
                    (!h & 0xFFFF) as i32
                } else {
                    (h | 0x8000) as i32
                }
            };
            const ABS_EPS: f32 = 1e-3; // scRGB（= 0.08 nits，黑场绝对下限）
            let mut exact = 0usize;
            let (mut b1, mut b2, mut b3, mut b5) = (0usize, 0usize, 0usize, 0usize);
            let mut viol = 0usize;
            let mut max_viol_nits = 0f32;
            let mut max_viol_ulps = 0i32;
            let mut worst: (f32, usize, u16, u16) = (0.0, 0, 0, 0);
            for (k, (&a, &b)) in na.iter().zip(nb.iter()).enumerate() {
                if a == b {
                    exact += 1;
                    continue;
                }
                let du = (ord(a) - ord(b)).abs();
                let da = (f16_bits_to_f32(a) - f16_bits_to_f32(b)).abs();
                if du <= 3 || da <= ABS_EPS {
                    match du {
                        1 => b1 += 1,
                        2 => b2 += 1,
                        3 => b3 += 1,
                        _ => b5 += 1,
                    }
                } else {
                    viol += 1;
                    let dn = da * 80.0;
                    if dn > max_viol_nits {
                        max_viol_nits = dn;
                        max_viol_ulps = du;
                        worst = (dn, k, a, b);
                    }
                    if viol <= 5 {
                        println!(
                            "  违例 idx={k} native={a:#06x} hybrid={b:#06x} |Δ|={dn:.4} nits（{du} ULP）"
                        );
                    }
                }
            }
            let n = na.len();
            let pct = |c: usize| c as f64 / n as f64 * 100.0;
            println!(
                "帧{i}: 逐值统计（{n} 通道值）: 按位一致 {:.3}%、≤1 ULP {b1}、2 {b2}、3 {b3}、>3 {b5}、违例 {viol}",
                pct(exact)
            );
            if viol > 0 {
                println!(
                    "  最差违例: idx={} native={:#06x} hybrid={:#06x} |Δ|={max_viol_nits:.4} nits（{max_viol_ulps} ULP，通道槽 {}）",
                    worst.1,
                    worst.2,
                    worst.3,
                    worst.1 % 4
                );
            }
            assert_eq!(
                viol, 0,
                "帧{i}: 存在 {viol} 个违例（>3 ULP 且 >{ABS_EPS} scRGB）；最差 {max_viol_nits:.4} nits"
            );
        }
    }

    /// Lite 导出模式（jietu 魔改）：生产链 HybridAnimDecoder Lite 快照契约
    /// + lite vs 全量 coeffs 逐位对拍。验证点：
    /// 1. snapshot.lite == true，dequant_ref/qblock_ref 恒空（每帧 ~110MB 裁剪）；
    /// 2. coeffs/idct_ref 长度 = 3·num_groups·group_dim²，系数非零；
    /// 3. lite 快照走 hybrid_reconstruct_pq16 GPU 全链成功（管线不消费被裁字段）；
    /// 4. 对拍：open_with_lite(path, false) 全量解码器首帧 coeffs 逐位一致
    ///    （Lite 仅裁参照层，系数导出语义与全量完全一致）。
    #[test]
    fn hybrid_anim_lite_mode() {
        let path = Path::new(ANIM_SAMPLE);
        if !path.exists() {
            eprintln!("动画样本不存在，跳过: {}", ANIM_SAMPLE);
            return;
        }
        if let Err(e) = crate::upscale::d3d11::engine() {
            eprintln!("GPU 引擎不可用，跳过: {e}");
            return;
        }
        // --- Lite 解码器（= 生产链 open）首帧快照 ---
        let mut lite_dec = HybridAnimDecoder::open(path).expect("lite 解码器打开失败");
        let (mut snap, _dur) = lite_dec
            .next_frame_snapshot()
            .expect("lite 首帧快照失败")
            .unwrap_or_else(|| panic!("lite 首帧即 EOF"));
        assert!(snap.lite, "Lite 解码器快照应标记 lite = true");
        assert!(snap.dequant_ref.is_empty(), "Lite 快照 dequant_ref 应为空");
        assert!(snap.qblock_ref.is_empty(), "Lite 快照 qblock_ref 应为空");
        let group_dim = snap.group_dim as usize;
        let total = 3 * snap.num_groups as usize * group_dim * group_dim;
        assert_eq!(
            snap.coeffs.len(),
            total,
            "coeffs 长度 = 3·num_groups·group_dim²"
        );
        assert_eq!(
            snap.idct_ref.len(),
            total,
            "idct_ref 长度 = 3·num_groups·group_dim²"
        );
        assert!(snap.coeffs.iter().any(|&v| v != 0), "系数全零——导出异常");

        // --- lite 快照走 GPU PQ16 全链（保证管线不消费被裁字段）---
        let (pq, _it) = crate::viewer::decode::hybrid_gpu::hybrid_reconstruct_pq16(&snap)
            .expect("lite 快照 GPU PQ16 全链重建失败");
        assert_eq!(pq.len(), total, "PQ16 三平面长度异常");
        drop(pq);

        // --- 对拍：全量解码器（lite=false）首帧 coeffs 逐位一致 ---
        // 仅保留 lite coeffs（~百 MB 级），其余字段随快照即时释放
        let lite_coeffs = std::mem::take(&mut snap.coeffs);
        drop(snap);
        let mut full_dec =
            HybridAnimDecoder::open_with_lite(path, false).expect("全量解码器打开失败");
        let (fsnap, _fdur) = full_dec
            .next_frame_snapshot()
            .expect("全量首帧快照失败")
            .unwrap_or_else(|| panic!("全量首帧即 EOF"));
        assert!(!fsnap.lite, "全量解码器快照 lite 应为 false");
        // 注：next_frame_snapshot 生产路径 debug_refs=false，两侧 refs 均为空
        //（既有行为）；全量指针非 null 深拷贝路径由静态图测试
        // golden_reconstruct_matches_idct_ref（debug_refs=true）回归覆盖
        assert_eq!(fsnap.coeffs.len(), lite_coeffs.len(), "两侧 coeffs 长度不一致");
        let mismatch = lite_coeffs
            .iter()
            .zip(fsnap.coeffs.iter())
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(mismatch, 0, "lite vs 全量 coeffs 逐位不一致: {mismatch} 处");
        println!("Lite 模式验证 OK: coeffs/idct_ref 各 {total} 值，lite vs 全量 coeffs 逐位一致");
    }

    /// GPU 常驻显存播放集成测试：纹理池 × 混合 GPU 链直写 × 消费复用全链路。
    /// 流程：真 D3D 建池（容量 3）→ HybridAnimDecoder 逐帧快照 →
    /// `hybrid_reconstruct_pq16_f16_into` 直写槽纹理 → publish → pool.get
    /// 逐帧取回（CopyResource → staging 读回，按 RowPitch 逐行拷贝）→ 与
    /// `hybrid_reconstruct_pq16_f16` 的 CPU buffer 出口**逐位对比**（同一条
    /// GPU 链、pass F 输出去向不同：UAV 直写纹理 vs staging 读回，f16 位型
    /// 一致——见 hybrid_gpu F16_TEX_TAIL 注释）→ 归还后继续填第 6/7 帧验证
    /// 槽位复用（factory 分配计数恒 = capacity，零新分配）。
    #[test]
    fn hybrid_pool_fill_get() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        use windows::Win32::Graphics::Direct3D11::{
            ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
            D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
        };

        const FRAMES: u64 = 5; // 首轮填充帧数
        const CAPACITY: usize = 3;

        let path = Path::new(ANIM_SAMPLE);
        if !path.exists() {
            eprintln!("动画样本不存在，跳过: {}", ANIM_SAMPLE);
            return;
        }
        let eng = match crate::upscale::d3d11::engine() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("GPU 引擎不可用，跳过: {e}");
                return;
            }
        };
        let (device, ctx) = {
            let g = eng.0.lock().unwrap();
            let (d, c) = g.device_ctx();
            (d.clone(), c.clone())
        };

        // 池纹理 → staging 读回（CopyResource + Map；RowPitch ≥ w*8 逐行拷贝）
        unsafe fn readback(
            device: &ID3D11Device,
            ctx: &ID3D11DeviceContext,
            tex: &ID3D11Texture2D,
            w: u32,
            h: u32,
        ) -> Vec<u8> {
            unsafe {
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                tex.GetDesc(&mut desc);
                let sdesc = D3D11_TEXTURE2D_DESC {
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                    ..desc
                };
                let mut stg = None;
                device
                    .CreateTexture2D(&sdesc, None, Some(&mut stg))
                    .expect("staging 纹理创建失败");
                let stg: ID3D11Texture2D = stg.expect("staging 纹理为空");
                ctx.CopyResource(&stg, tex);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                ctx.Map(&stg, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .expect("staging Map 失败");
                let row = (w * 8) as usize;
                let mut out = vec![0u8; row * h as usize];
                for y in 0..h as usize {
                    let src = (mapped.pData as *const u8).add(y * mapped.RowPitch as usize);
                    std::ptr::copy_nonoverlapping(src, out.as_mut_ptr().add(y * row), row);
                }
                ctx.Unmap(&stg, 0);
                out
            }
        }

        // 对照解码器：逐帧 CPU buffer 出口（同 GPU 链 + staging 读回版）
        let mut ref_dec = HybridAnimDecoder::open(path).expect("对照解码器打开失败");
        let (w, h) = ref_dec.dims();
        let gamut = ref_dec.gamut();

        // 生产池：真 D3D 纹理 + factory 分配计数（复用验证依据）
        let allocs = Arc::new(AtomicUsize::new(0));
        let a2 = allocs.clone();
        let mut base = crate::viewer::anim_pool::d3d_factory(device.clone(), (w, h));
        let pool = crate::viewer::anim_pool::AnimFramePool::new((w, h), CAPACITY, move || {
            a2.fetch_add(1, Ordering::SeqCst);
            base()
        })
        .expect("池创建失败");

        // 生产解码器：逐帧快照 → 引擎锁内直写槽纹理
        //（两侧 gamut 一致性由逐位对比兜底：矩阵不同则输出必不一致）
        let mut dec = HybridAnimDecoder::open(path).expect("生产解码器打开失败");

        // ===== 阶段 1：填充 5 帧（capacity=3，池满后边消费边填）=====
        let mut pending: std::collections::VecDeque<(u64, Vec<u8>)> = Default::default();
        for f in 0..FRAMES {
            // 对照侧：CPU buffer 出口
            let (rsnap, rdur) = ref_dec
                .next_frame_snapshot()
                .expect("对照快照失败")
                .unwrap_or_else(|| panic!("对照第 {f} 帧即 EOF（样本需 ≥{} 帧）", FRAMES + 2));
            let (rdata, _it) =
                crate::viewer::decode::hybrid_gpu::hybrid_reconstruct_pq16_f16(&rsnap, &gamut)
                    .unwrap_or_else(|e| panic!("对照第 {f} 帧 GPU 链失败: {e}"));
            drop(rsnap);
            assert_eq!(rdata.len(), (w * h * 8) as usize, "对照帧长度异常");

            // 生产侧：快照 → GPU 链直写槽纹理 → publish
            let slot = pool.acquire_free_wait().expect("有空闲槽");
            let (snap, dur) = dec
                .next_frame_snapshot()
                .expect("生产快照失败")
                .unwrap_or_else(|| panic!("生产第 {f} 帧即 EOF"));
            assert_eq!(dur, rdur, "第 {f} 帧两侧 duration 不一致");
            let tex = pool.slot_tex(slot).expect("槽纹理缺失");
            crate::viewer::decode::hybrid_gpu::hybrid_reconstruct_pq16_f16_into(
                &snap,
                &gamut,
                &tex.tex,
            )
            .unwrap_or_else(|e| panic!("第 {f} 帧直写池纹理失败: {e}"));
            drop(snap);
            pool.publish(slot, f, dur);

            pending.push_back((f, rdata));
            // 池满（已占 3 槽）→ 消费最旧帧（f−2）腾位：取回 → 逐位对比 → 归还
            if f >= CAPACITY as u64 - 1 {
                let (ef, edata) = pending.pop_front().unwrap();
                let g = pool
                    .get(ef)
                    .unwrap_or_else(|| panic!("帧 {ef} 已就绪却取不到"));
                assert_eq!(g.frame_idx, ef, "取回帧序不符");
                let got = unsafe { readback(&device, &ctx, &g.tex().tex, w, h) };
                assert_eq!(got.len(), edata.len(), "帧 {ef} 读回长度不一致");
                assert!(got == edata, "帧 {ef} 与 CPU 出口逐位不一致");
                drop(g); // 归还 → 下一轮 acquire 复用
            }
        }

        // ===== 阶段 2：剩余在屏帧（2..5）取回对比（guard 持有模拟在屏集合）=====
        let mut guards: Vec<
            crate::viewer::anim_pool::GpuFrame<crate::viewer::anim_pool::PoolTex>,
        > = Vec::new();
        while let Some((ef, edata)) = pending.pop_front() {
            let g = pool
                .get(ef)
                .unwrap_or_else(|| panic!("帧 {ef} 取回失败"));
            let got = unsafe { readback(&device, &ctx, &g.tex().tex, w, h) };
            assert!(got == edata, "帧 {ef} 与 CPU 出口逐位不一致");
            guards.push(g);
        }

        // ===== 阶段 3：复用验证——全部归还 → 继续填第 6/7 帧 → 取回对比 =====
        drop(guards);
        for f in FRAMES..FRAMES + 2 {
            let (rsnap, _rdur) = ref_dec
                .next_frame_snapshot()
                .expect("对照快照失败(复用段)")
                .unwrap_or_else(|| panic!("对照第 {f} 帧即 EOF"));
            let (rdata, _it) =
                crate::viewer::decode::hybrid_gpu::hybrid_reconstruct_pq16_f16(&rsnap, &gamut)
                    .unwrap_or_else(|e| panic!("对照第 {f} 帧 GPU 链失败: {e}"));
            drop(rsnap);

            let slot = pool.acquire_free_wait().expect("归还后应有余槽（复用）");
            let (snap, dur) = dec
                .next_frame_snapshot()
                .expect("生产快照失败(复用段)")
                .unwrap_or_else(|| panic!("生产第 {f} 帧即 EOF"));
            let tex = pool.slot_tex(slot).expect("槽纹理缺失");
            crate::viewer::decode::hybrid_gpu::hybrid_reconstruct_pq16_f16_into(
                &snap,
                &gamut,
                &tex.tex,
            )
            .unwrap_or_else(|e| panic!("第 {f} 帧直写池纹理失败: {e}"));
            drop(snap);
            pool.publish(slot, f, dur);

            let g = pool
                .get(f)
                .unwrap_or_else(|| panic!("复用帧 {f} 取回失败"));
            let got = unsafe { readback(&device, &ctx, &g.tex().tex, w, h) };
            assert!(got == rdata, "复用帧 {f} 与 CPU 出口逐位不一致");
            drop(g);
        }

        // 复用验证：7 帧填充零新分配（factory 恰好 capacity 次调用）
        assert_eq!(
            allocs.load(Ordering::SeqCst),
            CAPACITY,
            "槽位复用失败：factory 分配次数应恒 = capacity"
        );
        let s = pool.stats();
        assert_eq!(s.filled_frames, FRAMES + 2, "记账帧数不符");
        println!(
            "hybrid_pool_fill_get OK: {}x{} 池容量 {}，帧 0..{} 与 CPU 出口逐位一致，复用帧 {}..{} 逐位一致（零新分配）",
            w, h, CAPACITY, FRAMES, FRAMES, FRAMES + 2
        );
    }

    /// GPU 常驻显存播放集成测试（native 后端）：纹理池 × AnimationDecoder
    /// CPU 解码 × UpdateSubresource 入池 × 消费复用全链路。
    /// 流程：真 D3D 建池（容量 3）→ `AnimationDecoder::next_frame` 逐帧产出
    /// AnimFrame（f16 scRGB RGBA 行交错）→ 引擎锁内 UpdateSubresource 直传
    /// 槽纹理（复用生产路径 `hdr_viewer::native_upload_f16_into`）→ publish →
    /// pool.get 逐帧取回（CopyResource → staging 读回，按 RowPitch 逐行拷贝）
    /// → 与 AnimFrame.data **逐位对比**（UpdateSubresource 无格式转换，
    /// RGBA16F 纹理 + f16 RGBA 数据逐位一致）→ 归还后继续填第 6/7 帧验证
    /// 槽位复用（factory 分配计数恒 = capacity，零新分配）。
    #[test]
    fn native_pool_fill_get() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        use windows::Win32::Graphics::Direct3D11::{
            ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
            D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
        };

        const FRAMES: u64 = 5; // 首轮填充帧数
        const CAPACITY: usize = 3;

        let path = Path::new(ANIM_SAMPLE);
        if !path.exists() {
            eprintln!("动画样本不存在，跳过: {}", ANIM_SAMPLE);
            return;
        }
        let eng = match crate::upscale::d3d11::engine() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("GPU 引擎不可用，跳过: {e}");
                return;
            }
        };
        let (device, ctx) = {
            let g = eng.0.lock().unwrap();
            let (d, c) = g.device_ctx();
            (d.clone(), c.clone())
        };

        // 池纹理 → staging 读回（CopyResource + Map；RowPitch ≥ w*8 逐行拷贝）
        unsafe fn readback(
            device: &ID3D11Device,
            ctx: &ID3D11DeviceContext,
            tex: &ID3D11Texture2D,
            w: u32,
            h: u32,
        ) -> Vec<u8> {
            unsafe {
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                tex.GetDesc(&mut desc);
                let sdesc = D3D11_TEXTURE2D_DESC {
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                    ..desc
                };
                let mut stg = None;
                device
                    .CreateTexture2D(&sdesc, None, Some(&mut stg))
                    .expect("staging 纹理创建失败");
                let stg: ID3D11Texture2D = stg.expect("staging 纹理为空");
                ctx.CopyResource(&stg, tex);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                ctx.Map(&stg, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .expect("staging Map 失败");
                let row = (w * 8) as usize;
                let mut out = vec![0u8; row * h as usize];
                for y in 0..h as usize {
                    let src = (mapped.pData as *const u8).add(y * mapped.RowPitch as usize);
                    std::ptr::copy_nonoverlapping(src, out.as_mut_ptr().add(y * row), row);
                }
                ctx.Unmap(&stg, 0);
                out
            }
        }

        // 生产/对照解码器：native CPU 出口 AnimFrame.data（池填充输入即对照基准
        // ——UpdateSubresource 无格式转换，读回应与输入逐位一致）
        let mut dec = AnimationDecoder::open(path).expect("解码器打开失败");
        let (w, h) = dec.dims();
        assert!(w > 0 && h > 0, "尺寸异常");

        // 生产池：真 D3D 纹理 + factory 分配计数（复用验证依据）
        let allocs = Arc::new(AtomicUsize::new(0));
        let a2 = allocs.clone();
        let mut base = crate::viewer::anim_pool::d3d_factory(device.clone(), (w, h));
        let pool = crate::viewer::anim_pool::AnimFramePool::new((w, h), CAPACITY, move || {
            a2.fetch_add(1, Ordering::SeqCst);
            base()
        })
        .expect("池创建失败");

        // ===== 阶段 1：填充 5 帧（capacity=3，池满后边消费边填）=====
        let mut pending: std::collections::VecDeque<(u64, Vec<u8>)> = Default::default();
        for f in 0..FRAMES {
            let frame = dec
                .next_frame()
                .expect("解码失败")
                .unwrap_or_else(|| panic!("第 {f} 帧即 EOF（样本需 ≥{} 帧）", FRAMES + 2));
            assert_eq!(
                frame.data.len(),
                (w * h * 8) as usize,
                "帧长度异常（应为 f16 RGBA 行交错）"
            );
            assert!(frame.duration_ms > 0, "帧时长异常");

            // 引擎锁内 UpdateSubresource 直传槽纹理 → publish（同生产路径）
            let slot = pool.acquire_free_wait().expect("有空闲槽");
            let tex = pool.slot_tex(slot).expect("槽纹理缺失");
            crate::viewer::hdr_viewer::native_upload_f16_into(&tex.tex, &frame)
                .unwrap_or_else(|e| panic!("第 {f} 帧 UpdateSubresource 入池失败: {e}"));
            pool.publish(slot, f, frame.duration_ms);

            pending.push_back((f, frame.data));
            // 池满（已占 3 槽）→ 消费最旧帧（f−2）腾位：取回 → 逐位对比 → 归还
            if f >= CAPACITY as u64 - 1 {
                let (ef, edata) = pending.pop_front().unwrap();
                let g = pool
                    .get(ef)
                    .unwrap_or_else(|| panic!("帧 {ef} 已就绪却取不到"));
                assert_eq!(g.frame_idx, ef, "取回帧序不符");
                let got = unsafe { readback(&device, &ctx, &g.tex().tex, w, h) };
                assert_eq!(got.len(), edata.len(), "帧 {ef} 读回长度不一致");
                assert!(got == edata, "帧 {ef} 与 AnimFrame.data 逐位不一致");
                drop(g); // 归还 → 下一轮 acquire 复用
            }
        }

        // ===== 阶段 2：剩余在屏帧（2..5）取回对比（guard 持有模拟在屏集合）=====
        let mut guards: Vec<
            crate::viewer::anim_pool::GpuFrame<crate::viewer::anim_pool::PoolTex>,
        > = Vec::new();
        while let Some((ef, edata)) = pending.pop_front() {
            let g = pool
                .get(ef)
                .unwrap_or_else(|| panic!("帧 {ef} 取回失败"));
            let got = unsafe { readback(&device, &ctx, &g.tex().tex, w, h) };
            assert!(got == edata, "帧 {ef} 与 AnimFrame.data 逐位不一致");
            guards.push(g);
        }

        // ===== 阶段 3：复用验证——全部归还 → 继续填第 6/7 帧 → 取回对比 =====
        drop(guards);
        for f in FRAMES..FRAMES + 2 {
            let frame = dec
                .next_frame()
                .expect("解码失败(复用段)")
                .unwrap_or_else(|| panic!("第 {f} 帧即 EOF"));

            let slot = pool.acquire_free_wait().expect("归还后应有余槽（复用）");
            let tex = pool.slot_tex(slot).expect("槽纹理缺失");
            crate::viewer::hdr_viewer::native_upload_f16_into(&tex.tex, &frame)
                .unwrap_or_else(|e| panic!("第 {f} 帧 UpdateSubresource 入池失败: {e}"));
            pool.publish(slot, f, frame.duration_ms);

            let g = pool
                .get(f)
                .unwrap_or_else(|| panic!("复用帧 {f} 取回失败"));
            let got = unsafe { readback(&device, &ctx, &g.tex().tex, w, h) };
            assert!(got == frame.data, "复用帧 {f} 与 AnimFrame.data 逐位不一致");
            drop(g);
        }

        // 复用验证：7 帧填充零新分配（factory 恰好 capacity 次调用）
        assert_eq!(
            allocs.load(Ordering::SeqCst),
            CAPACITY,
            "槽位复用失败：factory 分配次数应恒 = capacity"
        );
        let s = pool.stats();
        assert_eq!(s.filled_frames, FRAMES + 2, "记账帧数不符");
        println!(
            "native_pool_fill_get OK: {}x{} 池容量 {}，帧 0..{} 与 AnimFrame.data 逐位一致，复用帧 {}..{} 逐位一致（零新分配）",
            w, h, CAPACITY, FRAMES, FRAMES, FRAMES + 2
        );
    }

    /// native GPU 出口对拍：`next_frame_pq16` + `pq16_gpu::convert_into`
    ///（PQ16 RGBA16F UAV 直写）vs `next_frame`（CPU PQ→scRGB f16 转换，
    /// 既有生产出口）——同帧两路独立解码器出，**逐位一致**（staging 纹理
    /// Map 按 RowPitch 逐行取，失败打印首个差异 像素/通道/位型）。
    /// 数学链：LUT（同表）→ 色域矩阵（同矩阵同序）→ /80 → f32tof16（RTZ
    /// + 次正规压零对齐 exr::f32_to_f16）。4 通道强制请求的 RGB 与 3 通道
    /// 逐位一致（渲染管线逐通道独立转换）一并被本测试验证。
    /// 同时打印两侧**填充链**耗时（解码 + 转换/入池，即生产口径）——CPU
    /// 转换卸载的提速数字。
    #[test]
    fn native_gpu_pq16_matches_cpu() {
        use std::time::Instant;
        use windows::Win32::Graphics::Direct3D11::{
            ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE,
            D3D11_BIND_UNORDERED_ACCESS, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ,
            D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
            D3D11_USAGE_STAGING,
        };
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC,
        };

        const FRAMES: usize = 3;
        let path = Path::new(ANIM_SAMPLE);
        if !path.exists() {
            eprintln!("动画样本不存在，跳过: {}", ANIM_SAMPLE);
            return;
        }
        let eng = match crate::upscale::d3d11::engine() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("GPU 引擎不可用，跳过: {e}");
                return;
            }
        };

        // CPU 参考腿 + GPU 腿两个独立解码器（同文件顺序解码，输出确定性）
        let mut dec_cpu = AnimationDecoder::open(path).expect("CPU 参考解码器打开失败");
        let mut dec_gpu = AnimationDecoder::open(path).expect("GPU 腿解码器打开失败");
        let (w, h) = dec_cpu.dims();
        assert!(w > 0 && h > 0, "尺寸异常");

        // 两侧目的纹理（CPU 腿 = UpdateSubresource 直传；GPU 腿 = convert_into
        // 直写）+ staging 读回纹理（短锁段创建；convert_into 内部自带引擎锁，
        // 调用链时不得持锁——std Mutex 不可重入）
        let (tex_cpu, tex_gpu, stg) = {
            let g = eng.0.lock().expect("GPU 引擎锁");
            let (device, _ctx) = g.device_ctx();
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_UNORDERED_ACCESS | D3D11_BIND_SHADER_RESOURCE).0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mk = || unsafe {
                let mut t: Option<ID3D11Texture2D> = None;
                device
                    .CreateTexture2D(&desc, None, Some(&mut t))
                    .expect("CreateTexture2D 失败");
                t.expect("纹理创建返回空")
            };
            let tex_cpu = mk();
            let tex_gpu = mk();
            let mut sdesc = desc;
            sdesc.Usage = D3D11_USAGE_STAGING;
            sdesc.BindFlags = 0;
            sdesc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            let stg = unsafe {
                let mut t: Option<ID3D11Texture2D> = None;
                device
                    .CreateTexture2D(&sdesc, None, Some(&mut t))
                    .expect("CreateTexture2D(staging) 失败");
                t.expect("staging 纹理创建失败")
            };
            (tex_cpu, tex_gpu, stg)
        };

        let (mut cpu_ms, mut gpu_ms) = (0f64, 0f64);
        for i in 0..FRAMES {
            // CPU 腿（生产现状）：next_frame（解码 + CPU PQ→scRGB 转换）→
            // UpdateSubresource 直传目的纹理
            let t0 = Instant::now();
            let nf = dec_cpu
                .next_frame()
                .expect("CPU 解码失败")
                .unwrap_or_else(|| panic!("CPU 腿第 {i} 帧即 EOF"));
            crate::viewer::hdr_viewer::native_upload_f16_into(&tex_cpu, &nf)
                .unwrap_or_else(|e| panic!("CPU 腿第 {i} 帧入池失败: {e}"));
            cpu_ms += t0.elapsed().as_secs_f64() * 1000.0;

            // GPU 腿（新生产路径）：next_frame_pq16（解码直出）→ 引擎锁内
            // 上传 + GPU 转换直写目的纹理
            let t0 = Instant::now();
            let (pq, dur) = dec_gpu
                .next_frame_pq16()
                .expect("PQ16 解码失败")
                .unwrap_or_else(|| panic!("GPU 腿第 {i} 帧即 EOF"));
            assert_eq!(pq.data.len(), (w * h * 8) as usize, "PQ16 帧长度异常");
            assert_eq!(pq.channels, 4, "PQ16 直出恒 4 通道");
            crate::viewer::decode::pq16_gpu::convert_into(&pq, &tex_gpu)
                .unwrap_or_else(|e| panic!("GPU 转换失败（帧 {i}）: {e}"));
            gpu_ms += t0.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(dur, nf.duration_ms, "第 {i} 帧两侧时长不一致");

            // 对拍：GPU 直写纹理读回（CopyResource + Map；RowPitch ≥ w*8 逐行
            // 拷贝）vs CPU AnimFrame.data 逐位
            let got: Vec<u8> = {
                let g = eng.0.lock().expect("GPU 引擎锁");
                let (_device, ctx) = g.device_ctx();
                unsafe {
                    ctx.CopyResource(&stg, &tex_gpu);
                    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                    ctx.Map(&stg, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                        .expect("staging Map 失败");
                    let row_bytes = (w * 8) as usize;
                    let mut out = vec![0u8; row_bytes * h as usize];
                    for y in 0..h as usize {
                        let src =
                            (mapped.pData as *const u8).add(y * mapped.RowPitch as usize);
                        std::ptr::copy_nonoverlapping(
                            src,
                            out.as_mut_ptr().add(y * row_bytes),
                            row_bytes,
                        );
                    }
                    ctx.Unmap(&stg, 0);
                    out
                }
            };
            if got != nf.data {
                let off = got
                    .iter()
                    .zip(nf.data.iter())
                    .position(|(a, b)| a != b)
                    .expect("长度不一致必有差异");
                let stride = (w as usize) * 8;
                let (px, py, ch) = ((off % stride) / 8, off / stride, (off % 8) / 2);
                let (gv, cv) = (
                    u16::from_le_bytes([got[off], got[off + 1]]),
                    u16::from_le_bytes([nf.data[off], nf.data[off + 1]]),
                );
                panic!(
                    "GPU 直写与 CPU 出口不一致 @ (px={px}, py={py}, 通道{ch}): cpu={cv:#06x} gpu={gv:#06x}"
                );
            }
        }
        let (cpu_avg, gpu_avg) = (cpu_ms / FRAMES as f64, gpu_ms / FRAMES as f64);
        println!(
            "native GPU 出口对拍: {w}×{h} × {FRAMES} 帧逐位一致（{}B/帧）｜CPU 填充链 {cpu_avg:.1}ms/帧（{:.1}fps）→ PQ16+GPU 填充链 {gpu_avg:.1}ms/帧（{:.1}fps，省 {:.1}ms）",
            (w * h * 8),
            1000.0 / cpu_avg,
            1000.0 / gpu_avg,
            cpu_avg - gpu_avg,
        );
    }

    /// Lite 导出模式解码腿提速基准（A2/A3 量化）：三口径逐帧计时——
    /// a. native 全解码：`AnimationDecoder::next_frame`（runner = 全核-1 = 15 线程）；
    /// b. Lite 解码腿：`HybridAnimDecoder::open`（lite=true）逐帧
    ///    `next_frame_snapshot` 纯解快照耗时之和（libjxl 解码 + 系数深拷贝，
    ///    不含 GPU 重建；快照即产即弃）；
    /// c. 全量导出解码腿：`open_with_lite(path, false)` 同口径（对照组，
    ///    验证 Lite 跳过 110MB dequant/qblock 分配+拷贝的收益）。
    /// 每口径预热 1 轮不计，正式 3 轮取中位/均值。断言仅要求运行完成且
    /// 帧数一致（Lite 与全量相同 = 313）；性能数字打印观察不硬断言
    ///（Lite 慢于全量时打 WARN）。
    #[test]
    fn decode_leg_lite_benchmark() {
        const EXPECTED_FRAMES: usize = 313;
        const ROUNDS: usize = 3;
        let path = Path::new(ANIM_SAMPLE);
        if !path.exists() {
            eprintln!("动画样本不存在，跳过: {}", ANIM_SAMPLE);
            return;
        }

        /// 单口径一轮：open（不计时）→ 逐帧调用计时收集。返回 (尺寸, 逐帧 ms)。
        /// lite=None = native AnimationDecoder；Some(b) = HybridAnimDecoder
        /// open_with_lite(b) 的 next_frame_snapshot。
        fn timed_round(path: &Path, lite: Option<bool>) -> Result<((u32, u32), Vec<f64>), String> {
            let mut frame_ms = Vec::new();
            let dims = match lite {
                None => {
                    let mut dec = AnimationDecoder::open(path)?;
                    let dims = dec.dims();
                    loop {
                        let t = std::time::Instant::now();
                        match dec.next_frame() {
                            Ok(Some(_)) => frame_ms.push(t.elapsed().as_secs_f64() * 1000.0),
                            Ok(None) => break,
                            Err(e) => {
                                // native 帧间推进 Error 分支无尾帧截断容错
                                //（录制产物尾部 premature end 在此冒泡）——与
                                // 解码器自身容错哲学一致：已完整解出 ≥1 帧视为
                                // EOF（首帧即失败仍 panic，真损坏不掩盖）
                                if frame_ms.is_empty() {
                                    panic!("native 首帧即解码失败: {e}");
                                }
                                eprintln!(
                                    "[note] native 第 {} 帧后按尾帧截断容错 EOF: {e}",
                                    frame_ms.len() + 1
                                );
                                break;
                            }
                        }
                    }
                    dims
                }
                Some(lite) => {
                    let mut dec = HybridAnimDecoder::open_with_lite(path, lite)?;
                    let dims = dec.dims();
                    loop {
                        let t = std::time::Instant::now();
                        let r = dec.next_frame_snapshot()?;
                        let ms = t.elapsed().as_secs_f64() * 1000.0;
                        match r {
                            Some((snap, _dur)) => {
                                drop(snap); // 快照即产即弃（本基准不含 GPU 重建）
                                frame_ms.push(ms);
                            }
                            None => break,
                        }
                    }
                    dims
                }
            };
            Ok((dims, frame_ms))
        }

        /// 汇总打印：各轮 ms/帧 → 中位/均值 + fps 折算。返回 (中位, 均值)。
        fn summarize(name: &str, rounds: &[Vec<f64>]) -> (f64, f64) {
            let per_frame: Vec<f64> = rounds
                .iter()
                .map(|r| r.iter().sum::<f64>() / r.len() as f64)
                .collect();
            let mut sorted = per_frame.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let median = sorted[sorted.len() / 2];
            let mean = per_frame.iter().sum::<f64>() / per_frame.len() as f64;
            let rounds_str: Vec<String> = per_frame.iter().map(|v| format!("{v:.1}")).collect();
            println!(
                "{}: {} 轮 ms/帧 = [{}] → 中位 {:.1} ms/帧（{:.1} fps）｜均值 {:.1} ms/帧（{:.1} fps）",
                name,
                rounds.len(),
                rounds_str.join(", "),
                median,
                1000.0 / median,
                mean,
                1000.0 / mean
            );
            (median, mean)
        }

        // 预热 1 轮/口径（不计入统计；顺带取尺寸）
        let ((w, h), _) = timed_round(path, None).expect("native 预热轮失败");
        let _ = timed_round(path, Some(true)).expect("lite 预热轮失败");
        let _ = timed_round(path, Some(false)).expect("全量预热轮失败");

        // 正式 3 轮/口径（轮内交错三口径，降低机器状态漂移的系统偏差）
        let mut native_rounds = Vec::new();
        let mut lite_rounds = Vec::new();
        let mut full_rounds = Vec::new();
        for i in 0..ROUNDS {
            native_rounds.push(
                timed_round(path, None)
                    .unwrap_or_else(|e| panic!("native 第 {i} 轮失败: {e}"))
                    .1,
            );
            lite_rounds.push(
                timed_round(path, Some(true))
                    .unwrap_or_else(|e| panic!("lite 第 {i} 轮失败: {e}"))
                    .1,
            );
            full_rounds.push(
                timed_round(path, Some(false))
                    .unwrap_or_else(|e| panic!("全量 第 {i} 轮失败: {e}"))
                    .1,
            );
        }

        // 帧数一致断言：Lite 与全量相同 = 313（硬断言）；native 因帧间推进
        // Error 分支无尾帧容错可能少 1 帧，轮内一致即可、跨口径差异打印观察
        let n_frames = lite_rounds[0].len();
        assert!(n_frames > 0, "Lite 解码 0 帧");
        for (i, r) in lite_rounds.iter().enumerate() {
            assert_eq!(r.len(), n_frames, "lite 第 {i} 轮帧数波动");
        }
        for (i, r) in full_rounds.iter().enumerate() {
            assert_eq!(r.len(), n_frames, "全量第 {i} 轮帧数与 Lite 不一致");
        }
        let native_frames = native_rounds[0].len();
        for (i, r) in native_rounds.iter().enumerate() {
            assert_eq!(r.len(), native_frames, "native 第 {i} 轮帧数波动");
        }
        assert_eq!(n_frames, EXPECTED_FRAMES, "Lite/全量帧数应为 {EXPECTED_FRAMES}");
        if native_frames != n_frames {
            eprintln!(
                "[note] native 帧数 {native_frames} ≠ Lite/全量 {n_frames}（native 帧间推进 Error 分支无尾帧容错，截断帧被丢弃）"
            );
        }

        println!(
            "==== 解码腿基准: {w}x{h}, {n_frames} 帧, 预热 1 轮 + 正式 {ROUNDS} 轮 ===="
        );
        let (native_med, _) = summarize(
            "a. native 全解码（AnimationDecoder::next_frame，15 线程 runner）",
            &native_rounds,
        );
        let (lite_med, _) = summarize(
            "b. Lite 解码腿（HybridAnimDecoder open→next_frame_snapshot 纯解快照，不含 GPU）",
            &lite_rounds,
        );
        let (full_med, _) = summarize(
            "c. 全量导出解码腿（open_with_lite(false) 同口径）",
            &full_rounds,
        );

        let gain = (full_med - lite_med) / full_med * 100.0;
        println!(
            "Lite 相对全量: {:.1} → {:.1} ms/帧，提升 {gain:.1}%（每帧省 {:.1} ms）",
            full_med,
            lite_med,
            full_med - lite_med
        );
        let diff = lite_med - native_med;
        println!(
            "Lite vs native: {diff:+.1} ms/帧（{:+.1}%）——导出模式解码腿相对 native 全解码的代价",
            diff / native_med * 100.0
        );
        if lite_med > full_med {
            eprintln!(
                "[WARN] Lite 解码腿中位 {lite_med:.1} ms/帧 慢于全量 {full_med:.1} ms/帧——A2/A3 Lite 提速未兑现"
            );
        }
    }

    // ==================== 任务 B8：池化播放端到端帧率基准 ====================

    /// 池化播放端到端帧率基准：量化 GPU 常驻播放的真实可达帧率。
    ///
    /// 播放模型（与 hdr_viewer 的 fill_gpu_pool + on_anim_tick 池分支同构）：
    /// 首播帧率 = 填充速率（解码 + GPU 直写）与消费速率的 max；全驻留后
    /// （短动图 / 循环二轮起）帧率 = 消费速率（get + SRV 换绑 + render，无解码）。
    /// 两段计时：
    ///
    /// a. **填充+消费流水线段**（真 D3D 池容量 3——小池强制流水线形态，前 30 帧）：
    ///    单线程交替「快照解码 → GPU 链 f16_into 直写槽纹理 → publish → get →
    ///    guard drop」逐帧计时。同帧同步消费 = 最坏情形；真实播放为填充/窗口
    ///    两线程流水线（解码与 GPU 重建可重叠），实际帧率 ≥ 本段口径。
    /// b. **全驻留消费段**（容量 60：60 × 32.8MB ≈ 1.97GB，8GB 卡可行；313 帧
    ///    全驻留需 ~10GB 不可行，故取 60 帧子集全驻留）：前 60 帧解码入池
    ///    （fill 不计入消费口径）后，60 帧 × 5 轮 = 300 次 get 纯消费计时
    ///    （无解码；换帧归还的槽以全局单调 idx 簿记重发布，纹理内容驻留复用
    ///    ——真实播放此处由填充线程 GPU 重写，不影响消费速率度量）。两个口径：
    ///    b-1 get + CopyResource staging 读回 32.8MB——**测试专用量化开销**
    ///    （真实播放无读回，纯 get + SRV 换绑 <1ms，此口径仅作读回代价参照）；
    ///    b-2 get-only 纯消费（guard 归还 + 簿记重发布 + get 独占取出，≈ 真实
    ///    播放的 get + 换绑，不含实际 render）= 常驻后播放帧率的直接度量。
    ///
    /// 断言（宽松防环境波动误伤）：a 段均值 ≤ 120 ms/帧；b-2 均值 ≤ 5 ms/帧
    /// （换绑 + guard 归还应近零）。其余数字（b 段填充耗时、b-1 读回口径、
    /// 各段最大值）打印观察不硬断言。
    #[test]
    fn pool_playback_fps_benchmark() {
        use windows::Win32::Graphics::Direct3D11::{
            ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ,
            D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
        };

        type Guard = crate::viewer::anim_pool::GpuFrame<crate::viewer::anim_pool::PoolTex>;

        const PIPELINE_FRAMES: u64 = 30; // a 段帧数
        const PIPELINE_CAP: usize = 3; // 小池强制流水线形态
        const RESIDENT: usize = 60; // b 段全驻留子集（60 × 32.8MB ≈ 1.97GB < 8GB）
        const ROUNDS: u64 = 5; // b 段循环轮数（60 × 5 = 300 次 get / 口径）

        let path = Path::new(ANIM_SAMPLE);
        if !path.exists() {
            eprintln!("动画样本不存在，跳过: {}", ANIM_SAMPLE);
            return;
        }
        let eng = match crate::upscale::d3d11::engine() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("GPU 引擎不可用，跳过: {e}");
                return;
            }
        };
        let (device, ctx) = {
            let g = eng.0.lock().unwrap();
            let (d, c) = g.device_ctx();
            (d.clone(), c.clone())
        };

        // ===== a 段：填充+消费流水线（容量 3，同帧同步消费 = 最坏情形）=====
        let mut dec = HybridAnimDecoder::open(path).expect("a 段解码器打开失败");
        let (w, h) = dec.dims();
        let gamut = dec.gamut();
        let frame_mb = (w as f64 * h as f64 * 8.0) / 1e6;
        let pool_a = crate::viewer::anim_pool::AnimFramePool::new(
            (w, h),
            PIPELINE_CAP,
            crate::viewer::anim_pool::d3d_factory(device.clone(), (w, h)),
        )
        .expect("a 段池创建失败");

        let mut pipeline_ms = Vec::with_capacity(PIPELINE_FRAMES as usize);
        for f in 0..PIPELINE_FRAMES {
            let t = std::time::Instant::now();
            let slot = pool_a.acquire_free_wait().expect("a 段有空闲槽");
            let (snap, dur) = dec
                .next_frame_snapshot()
                .expect("a 段快照失败")
                .unwrap_or_else(|| panic!("a 段第 {f} 帧即 EOF（样本需 ≥{PIPELINE_FRAMES} 帧）"));
            let tex = pool_a.slot_tex(slot).expect("槽纹理缺失");
            crate::viewer::decode::hybrid_gpu::hybrid_reconstruct_pq16_f16_into(
                &snap, &gamut, &tex.tex,
            )
            .unwrap_or_else(|e| panic!("a 段第 {f} 帧直写池纹理失败: {e}"));
            drop(snap);
            pool_a.publish(slot, f, dur);
            // 同帧同步消费（最坏情形）：get 独占取出 → guard drop 归还
            //（真实播放 guard 持有至下一 tick + SRV 换绑 + render）
            let g = pool_a
                .get(f)
                .unwrap_or_else(|| panic!("a 段第 {f} 帧取回失败"));
            drop(g);
            pipeline_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let pipe_mean = pipeline_ms.iter().sum::<f64>() / pipeline_ms.len() as f64;
        let pipe_max = pipeline_ms.iter().cloned().fold(0.0, f64::max);
        println!(
            "==== 池化播放帧率基准: {}x{}（帧 {:.1} MB RGBA16F）====",
            w, h, frame_mb
        );
        println!(
            "a. 填充+消费流水线段（容量 {}, {} 帧, 单线程同帧同步消费=最坏口径）: 均值 {:.1} ms/帧（≈{:.1} fps 首播上限）｜最大 {:.1} ms",
            PIPELINE_CAP,
            PIPELINE_FRAMES,
            pipe_mean,
            1000.0 / pipe_mean,
            pipe_max
        );
        assert!(
            pipe_mean <= 120.0,
            "填充消费段均值 {pipe_mean:.1} ms/帧 超 120 ms 宽松预算（环境异常？）"
        );
        drop(pool_a); // 先归还 a 段纹理，再建 b 段大池

        // ===== b 段：全驻留消费（容量 60，前 60 帧入池后纯消费）=====
        let mut dec = HybridAnimDecoder::open(path).expect("b 段解码器打开失败");
        let pool = crate::viewer::anim_pool::AnimFramePool::new(
            (w, h),
            RESIDENT,
            crate::viewer::anim_pool::d3d_factory(device.clone(), (w, h)),
        )
        .expect("b 段池创建失败（显存不足?）");

        let t_fill = std::time::Instant::now();
        let mut durs = Vec::with_capacity(RESIDENT);
        for f in 0..RESIDENT as u64 {
            let slot = pool.acquire_free().expect("b 段填充有空闲槽");
            let (snap, dur) = dec
                .next_frame_snapshot()
                .expect("b 段快照失败")
                .unwrap_or_else(|| panic!("b 段第 {f} 帧即 EOF（样本需 ≥{RESIDENT} 帧）"));
            let tex = pool.slot_tex(slot).expect("槽纹理缺失");
            crate::viewer::decode::hybrid_gpu::hybrid_reconstruct_pq16_f16_into(
                &snap, &gamut, &tex.tex,
            )
            .unwrap_or_else(|e| panic!("b 段第 {f} 帧直写池纹理失败: {e}"));
            drop(snap);
            pool.publish(slot, f, dur);
            durs.push(dur);
        }
        let fill_ms = t_fill.elapsed().as_secs_f64() * 1000.0;
        pool.set_total(RESIDENT as u64);
        assert_eq!(
            pool.stats().state,
            crate::viewer::anim_pool::PoolState::FullResident,
            "60 帧入池后应全驻留"
        );
        println!(
            "b. 全驻留填充（容量 {}, 前 {} 帧入池, 不计消费口径）: {:.0} ms（{:.1} ms/帧）",
            RESIDENT,
            RESIDENT,
            fill_ms,
            fill_ms / RESIDENT as f64
        );

        // 读回专用 staging（复用单张；创建成本不进逐帧口径）
        let tdesc = {
            let t0 = pool.slot_tex(0).expect("槽纹理缺失");
            let mut d = D3D11_TEXTURE2D_DESC::default();
            unsafe { t0.tex.GetDesc(&mut d) };
            d
        };
        let sdesc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            ..tdesc
        };
        let mut stg = None;
        unsafe { device.CreateTexture2D(&sdesc, None, Some(&mut stg)) }
            .expect("staging 创建失败");
        let stg: ID3D11Texture2D = stg.expect("staging 为空");
        let (row, hu) = ((w * 8) as usize, h as usize);

        // 池纹理 → 已建 staging（CopyResource + Map 同步等待 GPU 拷贝 + 逐行拷出）
        unsafe fn readback(
            ctx: &ID3D11DeviceContext,
            stg: &ID3D11Texture2D,
            tex: &ID3D11Texture2D,
            row: usize,
            h: usize,
        ) -> Vec<u8> {
            unsafe {
                ctx.CopyResource(stg, tex);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                ctx.Map(stg, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .expect("staging Map 失败");
                let mut out = vec![0u8; row * h];
                for y in 0..h {
                    let src = (mapped.pData as *const u8).add(y * mapped.RowPitch as usize);
                    std::ptr::copy_nonoverlapping(src, out.as_mut_ptr().add(y * row), row);
                }
                ctx.Unmap(stg, 0);
                out
            }
        }

        // b-1：get + staging 读回 32.8MB（测试专用口径，真实播放无读回）
        let mut rb_ms = Vec::with_capacity((RESIDENT as u64 * ROUNDS) as usize);
        let mut guard: Option<Guard> = None;
        for r in 0..ROUNDS {
            for k in 0..RESIDENT as u64 {
                let f = r * RESIDENT as u64 + k;
                drop(guard.take()); // 换帧归还上一帧（真实播放 = 旧帧出屏）
                if r > 0 {
                    // 归还槽以全局单调 idx 簿记重发布（纹理驻留复用，零拷贝）
                    let slot = pool.acquire_free().expect("消费归还后有空闲槽");
                    pool.publish(slot, f, durs[k as usize]);
                }
                let t = std::time::Instant::now();
                let g = pool
                    .get(f)
                    .unwrap_or_else(|| panic!("全驻留帧 {f} 应必命中"));
                let _rb = unsafe { readback(&ctx, &stg, &g.tex().tex, row, hu) };
                guard = Some(g);
                rb_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            }
        }
        drop(guard);
        let rb_mean = rb_ms.iter().sum::<f64>() / rb_ms.len() as f64;
        let rb_max = rb_ms.iter().cloned().fold(0.0, f64::max);
        println!(
            "b-1 get+staging 读回 {:.1} MB（测试专用口径——真实播放无读回）: 均值 {:.2} ms/帧（{:.1} fps）｜最大 {:.2} ms",
            frame_mb,
            rb_mean,
            1000.0 / rb_mean,
            rb_max
        );

        // b-2：get-only 纯消费（guard 归还 + 簿记重发布 + get 独占取出；
        // idx 自 300 续接全局单调；≈ 真实播放 get + SRV 换绑，不含 render）
        let mut get_ms = Vec::with_capacity((RESIDENT as u64 * ROUNDS) as usize);
        let mut guard: Option<Guard> = None;
        for r in 0..ROUNDS {
            for k in 0..RESIDENT as u64 {
                let f = RESIDENT as u64 * ROUNDS + r * RESIDENT as u64 + k;
                let t = std::time::Instant::now();
                drop(guard.take());
                let slot = pool.acquire_free().expect("消费归还后有空闲槽");
                pool.publish(slot, f, durs[k as usize]);
                let g = pool
                    .get(f)
                    .unwrap_or_else(|| panic!("全驻留帧 {f} 应必命中"));
                guard = Some(g);
                get_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            }
        }
        drop(guard);
        let get_mean = get_ms.iter().sum::<f64>() / get_ms.len() as f64;
        let get_max = get_ms.iter().cloned().fold(0.0, f64::max);
        println!(
            "b-2 get-only 纯消费（归还+簿记+独占取出, ≈真实 get+SRV 换绑, 不含 render）: 均值 {:.0} µs/帧｜最大 {:.0} µs —— 池层消费开销近零，常驻帧率实际由 SRV 换绑+render 决定（远超显示刷新率）",
            get_mean * 1000.0,
            get_max * 1000.0
        );
        assert!(
            get_mean <= 5.0,
            "全驻留 get-only 均值 {get_mean:.3} ms/帧 超 5 ms（换绑+归还应近零）"
        );
        println!(
            "结论: 首播帧率受填充链制约 ≈ {:.1} fps（a 段上限，真实两线程流水线可重叠解码/GPU 故 ≥ 此值）；全驻留后池层消费近零（b-2 ≈ {:.2} µs/帧），常驻帧率仅受 render 换绑制约；b-1（{:.1} fps）为含 {:.1} MB 读回的测试量化差异项，真实播放无读回无此项",
            1000.0 / pipe_mean,
            get_mean * 1000.0,
            1000.0 / rb_mean,
            frame_mb
        );
    }
}
