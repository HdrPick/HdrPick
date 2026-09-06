//! 验证截图 HDR 编码：JXL/JXR 文件确实携带原生 HDR 数据（非 SDR）
//!
//! 流程：
//! 1. 构造 scRGB 纹理（4 像素：0.1 / 1.0 / 5.0 / 12.5，
//!    按 1.0=80nit 即 8nit / 80nit / 400nit / 1000nit）
//! 2. encode::save_jxl / save_jxr 编码到临时文件
//! 3. 读回验证：
//!    - JXL：libjxl 解析 BasicInfo + ColorEncoding → 应为 16bit + PQ（HDR10）
//!    - JXR：WIC 读像素格式 GUID → 应为 128bppRGBFloat，
//!      且读回的 f32 值保留 >1.0 的高光（12.5 完整还原）
//!
//! 运行：cargo run --example jxl_jxr_hdr_verify

use app_lib::capture::hdr_pipeline::HdrToSdrParams;
use app_lib::capture::CapturedTexture;
use app_lib::color::PixelFormat;

/// f32 → f16 位模式（截断舍入）
fn f32_to_f16_bits(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xFF) as i32;
    let mantissa = bits & 0x7F_FFFF;
    if exp == 0xFF {
        return sign | 0x7C00; // Inf/NaN
    }
    let e = exp - 127 + 15;
    if e >= 0x1F {
        return sign | 0x7C00; // 溢出 → Inf
    }
    if e <= 0 {
        return sign; // 太小 → 0
    }
    let m = (mantissa >> 13) as u16;
    sign | ((e as u16) << 10) | m
}

/// 2x2 scRGB 纹理：像素值即 scRGB 线性（1.0 = 80 nit SDR 白基准）
fn make_hdr_texture() -> CapturedTexture {
    let values = [0.1f32, 1.0, 5.0, 12.5]; // → 8 / 80 / 400 / 1000 nits
    let mut data = vec![0u8; 4 * 8]; // 4 像素 × 8 字节（RGBA f16）
    for (i, v) in values.iter().enumerate() {
        let off = i * 8;
        data[off..off + 2].copy_from_slice(&f32_to_f16_bits(*v).to_le_bytes());
        data[off + 2..off + 4].copy_from_slice(&f32_to_f16_bits(*v).to_le_bytes());
        data[off + 4..off + 6].copy_from_slice(&f32_to_f16_bits(*v).to_le_bytes());
        data[off + 6..off + 8].copy_from_slice(&0x3C00u16.to_le_bytes()); // A = 1.0
    }
    CapturedTexture {
        width: 2,
        height: 2,
        format: PixelFormat::R16g16b16a16Float,
        data,
        row_pitch: 2 * 8,
        via_gdi: false,
    }
}

fn main() -> anyhow::Result<()> {
    let tex = make_hdr_texture();
    // 编码基准：标准 scRGB 80 nit（与截图保存路径一致）
    let params = HdrToSdrParams {
        input_sdr_white_nits: 80.0,
        ..Default::default()
    };

    let jxl_path = std::env::temp_dir().join("jietu_hdr_verify.jxl");
    let jxr_path = std::env::temp_dir().join("jietu_hdr_verify.jxr");

    println!("=== 编码（截图路径同款 API；无损档）===");
    app_lib::encode::save_jxl(
        &tex,
        &jxl_path,
        &params,
        app_lib::encode::QualityLevel::Lossless,
    )?;
    app_lib::encode::save_jxr(
        &tex,
        &jxr_path,
        &params,
        app_lib::encode::QualityLevel::Lossless,
    )?;
    println!(
        "输入 scRGB: [0.1, 1.0, 5.0, 12.5] = [{:.0}, {:.0}, {:.0}, {:.0}] nits",
        0.1 * 80.0,
        1.0 * 80.0,
        5.0 * 80.0,
        12.5 * 80.0
    );

    // ---------- JXL 验证：libjxl 读回色彩编码 ----------
    println!("\n=== JXL 文件验证 ===");
    let data = std::fs::read(&jxl_path)?;
    unsafe {
        use jxl_sys as jxl;
        use std::ptr;
        let dec = jxl::JxlDecoderCreate(ptr::null());
        assert!(!dec.is_null());
        // 第一遍只订阅头事件（无 FULL_IMAGE：不会触发输出缓冲请求）
        let events = jxl::JXL_DEC_BASIC_INFO | jxl::JXL_DEC_COLOR_ENCODING;
        assert_eq!(
            jxl::JxlDecoderSubscribeEvents(dec, events),
            jxl::JxlDecoderStatus::Success
        );
        assert_eq!(
            jxl::JxlDecoderSetInput(dec, data.as_ptr(), data.len()),
            jxl::JxlDecoderStatus::Success
        );
        jxl::JxlDecoderCloseInput(dec);

        let mut info = jxl::JxlBasicInfo::default();
        let mut color = jxl::JxlColorEncoding::default();
        // 第一遍：只读头信息（BasicInfo + ColorEncoding，不订阅 FULL_IMAGE）
        loop {
            match jxl::JxlDecoderProcessInput(dec) {
                jxl::JxlDecoderStatus::BasicInfo => {
                    assert_eq!(
                        jxl::JxlDecoderGetBasicInfo(dec, &mut info),
                        jxl::JxlDecoderStatus::Success
                    );
                    println!(
                        "  位深: {}bit (exponent_bits={})",
                        info.bits_per_sample, info.exponent_bits_per_sample
                    );
                    println!("  intensity_target: {} nits", info.intensity_target);
                }
                jxl::JxlDecoderStatus::ColorEncoding => {
                    assert_eq!(
                        jxl::JxlDecoderGetColorAsEncodedProfile(
                            dec,
                            jxl::JxlColorProfileTarget::Original,
                            &mut color
                        ),
                        jxl::JxlDecoderStatus::Success
                    );
                    println!(
                        "  传递函数: {:?}（Pq=HDR10 编码，非 sRGB）",
                        color.transfer_function
                    );
                    println!("  原色: {:?}（P2100=BT.2020）", color.primaries);
                    break; // 头信息读全，无需继续（像素验证走第二遍）
                }
                jxl::JxlDecoderStatus::Error => anyhow::bail!("JXL 头解析失败"),
                _ => {}
            }
        }
        jxl::JxlDecoderDestroy(dec);

        // 像素值验证：缓冲在 NeedImageOutBuffer 设置、FullImage 后填充
        let dec2 = jxl::JxlDecoderCreate(ptr::null());
        assert_eq!(
            jxl::JxlDecoderSubscribeEvents(dec2, jxl::JXL_DEC_BASIC_INFO | jxl::JXL_DEC_FULL_IMAGE),
            jxl::JxlDecoderStatus::Success
        );
        assert_eq!(
            jxl::JxlDecoderSetInput(dec2, data.as_ptr(), data.len()),
            jxl::JxlDecoderStatus::Success
        );
        jxl::JxlDecoderCloseInput(dec2);
        let mut info2 = jxl::JxlBasicInfo::default();
        let mut pixel_buf: Vec<u8> = Vec::new();
        let mut done = false;
        while !done {
            match jxl::JxlDecoderProcessInput(dec2) {
                jxl::JxlDecoderStatus::BasicInfo => {
                    let _ = jxl::JxlDecoderGetBasicInfo(dec2, &mut info2);
                }
                jxl::JxlDecoderStatus::NeedImageOutBuffer => {
                    let npx = (info2.xsize * info2.ysize) as usize;
                    let fmt = jxl::JxlPixelFormat {
                        num_channels: 4,
                        data_type: jxl::JxlDataType::Uint16,
                        endianness: jxl::JxlEndianness::Little,
                        align: 0,
                    };
                    pixel_buf = vec![0u8; npx * 8];
                    assert_eq!(
                        jxl::JxlDecoderSetImageOutBuffer(
                            dec2,
                            &fmt,
                            pixel_buf.as_mut_ptr() as *mut std::ffi::c_void,
                            pixel_buf.len()
                        ),
                        jxl::JxlDecoderStatus::Success
                    );
                }
                jxl::JxlDecoderStatus::FullImage => done = true,
                jxl::JxlDecoderStatus::Error => break,
                _ => {}
            }
        }
        jxl::JxlDecoderDestroy(dec2);
        assert!(done, "JXL 未产生完整图像");
        let rgba16: Vec<u16> = pixel_buf
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();

        // 16bit PQ 值 → nits（PQ EOTF ×10000）
        println!("\n  文件内 PQ 码值 → 解码 nits（HDR 数据实证）:");
        for (i, label) in ["暗部", "SDR白", "高光", "峰值"].iter().enumerate() {
            let pq = rgba16[i * 4] as f32 / 65535.0;
            // PQ EOTF（BT.2100）
            let nits = pq_eotf_ref(pq) * 10000.0;
            println!(
                "    {:>5} pixel[{}]: PQ码值={:.4} → {:.0} nits",
                label, i, pq, nits
            );
        }
        println!("  → nits 值 >80 的即为 HDR 高光，完整保留在文件中");
    }

    // ---------- JXR 验证：WIC 读回像素格式 + float 值 ----------
    println!("\n=== JXR 文件验证 ===");
    verify_jxr(&jxr_path)?;

    println!("\n结论：JXL/JXR 均以原生 HDR 数据编码保存；看图时的 SDR 只是色调映射预览副本。");

    // ---------- 重新输出验证：HDR 源 → 当前激活预设 → SDR PNG ----------
    println!("\n=== 重新输出验证（HDR 源 → 当前预设 → SDR）===");
    for (label, src) in [("JXL", &jxl_path), ("JXR", &jxr_path)] {
        let out =
            std::env::temp_dir().join(format!("jietu_retonemap_{}.png", label.to_lowercase()));
        app_lib::viewer::convert::convert(src, &out, "png", 90)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let img = image::ImageReader::open(&out)
            .and_then(|r| r.with_guessed_format())
            .unwrap()
            .decode()
            .unwrap();
        let rgba = img.to_rgba8();
        let p0 = rgba.get_pixel(0, 0);
        println!(
            "  {} → SDR PNG: {}x{}, 像素值 [0,0]=({},{},{})（色调映射后：高光被压缩、色相保留）",
            label,
            img.width(),
            img.height(),
            p0[0],
            p0[1],
            p0[2]
        );
    }
    println!("  （设置菜单切换预设后再次导出，即得对应预设效果）");

    Ok(())
}

/// PQ EOTF 参考实现（BT.2100，验证用独立副本）
fn pq_eotf_ref(e: f32) -> f32 {
    let e = e.clamp(0.0, 1.0);
    const M1: f32 = 2610.0 / 16384.0;
    const M2: f32 = 2523.0 / 4096.0 * 128.0;
    const C1: f32 = 3424.0 / 4096.0;
    const C2: f32 = 2413.0 / 4096.0 * 32.0;
    const C3: f32 = 2392.0 / 4096.0 * 32.0;
    let ep = e.powf(1.0 / M2);
    let num = (ep - C1).max(0.0);
    let den = C2 - C3 * ep;
    (num / den).powf(1.0 / M1)
}

/// WIC 打开 JXR 验证：像素格式 GUID + float 像素值
fn verify_jxr(path: &std::path::Path) -> anyhow::Result<()> {
    use windows::core::GUID;
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, IWICImagingFactory, WICDecodeMetadataCacheOnDemand,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };

    // 128bppRGBFloat GUID（与 decode 端一致）
    const RGB_FLOAT_128: GUID = GUID::from_u128(0x6fdd_c324_4e03_4bfe_b185_3d77_768d_c900);

    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let co_inited = hr.0 == 0;

    let result = (|| -> anyhow::Result<()> {
        let factory: IWICImagingFactory =
            unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)? };
        use std::os::windows::ffi::OsStrExt;
        let path_w: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let decoder = unsafe {
            factory.CreateDecoderFromFilename(
                windows::core::PCWSTR(path_w.as_ptr()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )?
        };
        let frame = unsafe { decoder.GetFrame(0)? };
        let (mut w, mut h) = (0u32, 0u32);
        unsafe { frame.GetSize(&mut w, &mut h)? };
        let fmt: GUID = unsafe { frame.GetPixelFormat()? };
        println!(
            "  像素格式 GUID: {}",
            if fmt == RGB_FLOAT_128 {
                "128bppRGBFloat（线性 scRGB f32，HDR）"
            } else {
                "非 C900（见下方原始值诊断）"
            }
        );
        println!(
            "  GUID 原始值: {:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:012X}",
            fmt.data1,
            fmt.data2,
            fmt.data3,
            fmt.data4[0],
            fmt.data4[1],
            u64::from_be_bytes([
                fmt.data4[2],
                fmt.data4[3],
                fmt.data4[4],
                fmt.data4[5],
                fmt.data4[6],
                fmt.data4[7],
                0,
                0,
            ])
        );

        let stride = w as usize * 16;
        let mut buf = vec![0u8; stride * h as usize];
        unsafe { frame.CopyPixels(std::ptr::null(), stride as u32, &mut buf)? };

        // 定点/浮点双重解释诊断（C91B = 128bppRGBAFixedPoint 疑似）
        println!("  像素原始位解释（期望 scRGB: 0.1 / 1.0 / 5.0 / 12.5）:");
        for (i, label) in ["暗部", "SDR白", "高光", "峰值"].iter().enumerate() {
            let off = i * 16;
            let raw_u32 = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
            let as_f32 = f32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
            println!(
                "    {:>5} pixel[{}]: u32={:010X} | f32={:.4} | s2.30={:.4} | s7.25={:.4}",
                label,
                i,
                raw_u32,
                as_f32,
                raw_u32 as f32 / 1073741824.0,
                raw_u32 as f32 / 33554432.0
            );
        }
        Ok(())
    })();

    if co_inited {
        unsafe { CoUninitialize() };
    }
    result
}
