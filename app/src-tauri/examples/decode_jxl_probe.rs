//! 解码 JXL 探针：检查 AI 放大输出的 JXL 是否像素全黑
//!
//! 运行：cargo run --release --example decode_jxl_probe -- <file.jxl>
//!
//! 输出：
//! 1. 色调映射后 SDR 临时 PNG 的像素统计（渲染链路终态）
//! 2. HdrSource（PQ→scRGB 解码缓存）的 f32 统计（文件真实数据）

use std::path::Path;

fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) & 1) as u32;
    let exp = ((h >> 10) & 0x1F) as i32;
    let frac = (h & 0x3FF) as u32;
    let bits: u32 = if exp == 0 {
        if frac == 0 {
            sign << 31
        } else {
            // 次正规数
            let mut e = -1i32;
            let mut f = frac;
            while f & 0x400 == 0 {
                f <<= 1;
                e -= 1;
            }
            f &= 0x3FF;
            ((sign << 31) as i32 | ((127 - 15 + e + 1) << 23) as i32 | (f << 13) as i32) as u32
        }
    } else if exp == 0x1F {
        (sign << 31) | 0x7F80_0000 | (frac << 13)
    } else {
        ((sign << 31) | (((exp - 15 + 127) as u32) << 23) | (frac << 13)) as u32
    };
    f32::from_bits(bits as u32)
}

fn tensor_stats(t: &app_lib::upscale::cpu::Tensor, tag: &str) {
    let (mut mx, mut sum) = (0.0f32, 0.0f64);
    for v in &t.data {
        let v = v.clamp(0.0, 1.0);
        mx = mx.max(v);
        sum += v as f64;
    }
    println!(
        "[{}] {}x{}{} max={:.4} mean={:.6}",
        tag,
        t.w,
        t.h,
        if t.c == 1 { " (alpha)" } else { "" },
        mx,
        sum / t.data.len() as f64
    );
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_else(|| {
        "C:\\Users\\Administrator\\Desktop\\QQ图片20260810215154_waifu2x_noise1_x2.jxl".to_string()
    });
    let path = Path::new(&arg);

    // 0) 端到端复现：原图 → 放大 → 张量统计 → 接缝量化 → JXL 保存 → 解码统计
    if arg.ends_with(".jpg") || arg.ends_with(".jpeg") || arg.ends_with(".png") {
        let src_owned = arg.to_string();
        let fallback = "C:\\Users\\Administrator\\Desktop\\QQ图片20260810215154.jpg";
        let src: &str = if Path::new(&src_owned).exists() {
            &src_owned
        } else {
            fallback
        };
        if Path::new(src).exists() {
            println!("=== 端到端复现: {} ===", src);
            let img = image::open(src).expect("打开原图失败").to_rgba8();
            println!(
                "[输入] {}x{} alpha={:?}",
                img.width(),
                img.height(),
                img.pixels().any(|p| p.0[3] != 255)
            );
            let root = app_lib::upscale::pipeline::default_model_root();
            let kind = match std::env::args().nth(2).as_deref() {
                Some("realesrgan") => app_lib::upscale::model::ModelKind::RealEsrganX4,
                Some("anime6b") => app_lib::upscale::model::ModelKind::RealEsrganAnime6B,
                _ => app_lib::upscale::model::ModelKind::Upconv7AnimeStyleArtRgb,
            };
            let (out, used_gpu) = app_lib::upscale::pipeline::upscale_image_exact(
                &root,
                kind,
                &img,
                2.0,
                Some(1),
                false,
            )
            .expect("放大失败");
            println!("[推理] used_gpu={}", used_gpu);
            tensor_stats(&out.rgb, "输出 rgb");
            if let Some(a) = &out.alpha {
                tensor_stats(a, "输出 alpha");
            }
            // 接缝量化：相邻列/行平均绝对差，top-5 突刺位置。
            // 旧版 tile 接缝应出现在 ×4 网格（1024/2048…），修复后应消失
            {
                let t = &out.rgb;
                let col_diff = |x: usize| -> f64 {
                    let mut s = 0.0f64;
                    for c in 0..3 {
                        for y in 0..t.h {
                            s += (t.at(c, y, x) - t.at(c, y, x - 1)).abs() as f64;
                        }
                    }
                    s / (3.0 * t.h as f64)
                };
                let row_diff = |y: usize| -> f64 {
                    let mut s = 0.0f64;
                    for c in 0..3 {
                        for x in 0..t.w {
                            s += (t.at(c, y, x) - t.at(c, y - 1, x)).abs() as f64;
                        }
                    }
                    s / (3.0 * t.w as f64)
                };
                let mut cx: Vec<(usize, f64)> = (1..t.w).map(|x| (x, col_diff(x))).collect();
                let mut cy: Vec<(usize, f64)> = (1..t.h).map(|y| (y, row_diff(y))).collect();
                cx.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
                cy.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
                let avg_x: f64 = cx.iter().map(|p| p.1).sum::<f64>() / cx.len() as f64;
                let avg_y: f64 = cy.iter().map(|p| p.1).sum::<f64>() / cy.len() as f64;
                println!(
                    "[接缝] 列差均值 {:.5}，top5: {}",
                    avg_x,
                    cx[..5]
                        .iter()
                        .map(|(x, d)| format!("x{}={:.5}", x, d))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                println!(
                    "[接缝] 行差均值 {:.5}，top5: {}",
                    avg_y,
                    cy[..5]
                        .iter()
                        .map(|(y, d)| format!("y{}={:.5}", y, d))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                let cdx = |x: usize| (x < t.w).then(|| col_diff(x));
                let cdy = |y: usize| (y < t.h).then(|| row_diff(y));
                let fmt_opt = |v: Option<f64>| {
                    v.map(|d| format!("{:.5}", d))
                        .unwrap_or_else(|| "N/A".into())
                };
                println!(
                    "[接缝] 旧 tile 边界（输出 ×4 网格）x=1024 {} / x=2048 {} / y=1024 {} / y=2048 {}",
                    fmt_opt(cdx(1024)),
                    fmt_opt(cdx(2048)),
                    fmt_opt(cdy(1024)),
                    fmt_opt(cdy(2048))
                );
            }
            let tmp = std::env::temp_dir().join("jietu_upscale_e2e.jxl");
            out.save(&tmp, app_lib::upscale::pipeline::OutputFormat::JxlHdr16)
                .expect("JXL 保存失败");
            let tmp12 = std::env::temp_dir().join("jietu_upscale_e2e12.jxl");
            out.save(&tmp12, app_lib::upscale::pipeline::OutputFormat::JxlHdr12)
                .expect("JXL12 保存失败");
            println!(
                "[SDR白] get_sdr_white_level_nits() = {:.1} nits",
                app_lib::capture::monitor::get_sdr_white_level_nits()
            );
            println!(
                "[保存] 16bit {} bytes / 12bit {} bytes",
                std::fs::metadata(&tmp).unwrap().len(),
                std::fs::metadata(&tmp12).unwrap().len()
            );
            for (tag, p) in [("16bit", &tmp), ("12bit", &tmp12)] {
                if let Some(src) = app_lib::viewer::decode::ensure_hdr_source(p) {
                    let (mut mx, mut sum) = (0.0f32, 0.0f64);
                    let n = src.data.len() / 8;
                    for px in src.data.chunks_exact(8) {
                        let r = f16_to_f32(u16::from_le_bytes([px[0], px[1]]));
                        let g = f16_to_f32(u16::from_le_bytes([px[2], px[3]]));
                        let b = f16_to_f32(u16::from_le_bytes([px[4], px[5]]));
                        mx = mx.max(r.max(g).max(b));
                        sum += (r + g + b) as f64 / 3.0;
                    }
                    println!(
                        "[验证 {}] 解码 scRGB: max={:.4} mean={:.6}（黑图 max≈0）",
                        tag,
                        mx,
                        sum / n as f64
                    );
                }
            }
            println!("=== 端到端结束 ===");
            return;
        }
    }
    println!(
        "=== 探针: {} ({} bytes) ===",
        path.display(),
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    );

    // 1) HdrSource（PQ EOTF → scRGB，f16 存储未量化）
    if let Some(src) = app_lib::viewer::decode::ensure_hdr_source(path) {
        let n = src.data.len() / 8;
        let (mut mx, mut sum, mut nz) = (0.0f32, 0.0f64, 0usize);
        for px in src.data.chunks_exact(8) {
            let r = f16_to_f32(u16::from_le_bytes([px[0], px[1]]));
            let g = f16_to_f32(u16::from_le_bytes([px[2], px[3]]));
            let b = f16_to_f32(u16::from_le_bytes([px[4], px[5]]));
            let m = r.max(g).max(b);
            mx = mx.max(m);
            sum += (r + g + b) as f64 / 3.0;
            if m > 0.01 {
                nz += 1;
            }
        }
        println!(
            "[HdrSource] {}x{} scRGB: max={:.4} mean={:.6} 非黑像素 {:.2}%",
            src.width,
            src.height,
            mx,
            sum / n as f64,
            nz as f64 * 100.0 / n as f64
        );
        // 抽样 5 个像素
        for i in [0usize, n / 4, n / 2, 3 * n / 4, n - 1] {
            let px = &src.data[i * 8..i * 8 + 8];
            println!(
                "  px[{}]: ({:.4}, {:.4}, {:.4}, a={:.2})",
                i,
                f16_to_f32(u16::from_le_bytes([px[0], px[1]])),
                f16_to_f32(u16::from_le_bytes([px[2], px[3]])),
                f16_to_f32(u16::from_le_bytes([px[4], px[5]])),
                f16_to_f32(u16::from_le_bytes([px[6], px[7]]))
            );
        }
    } else {
        println!("[HdrSource] 未命中（非 HDR JXL 或解码失败）");
    }

    // 2) 编码函数往返测试：已知渐变输入 → save_sdr_as_jxl_hdr → 解码统计
    {
        let (w, h) = (8u32, 8u32);
        let mut rgba = vec![0f32; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                rgba[i] = x as f32 / (w - 1) as f32;
                rgba[i + 1] = y as f32 / (h - 1) as f32;
                rgba[i + 2] = 0.5;
                rgba[i + 3] = 1.0;
            }
        }
        let tmp = std::env::temp_dir().join("jietu_sdr_as_jxl_probe.jxl");
        match app_lib::encode::save_sdr_as_jxl_hdr(
            w,
            h,
            &rgba,
            false,
            &tmp,
            app_lib::encode::QualityLevel::VeryHigh,
            16,
            app_lib::color::SdrToHdrPreset::Natural
                .params(app_lib::capture::monitor::effective_sdr_white_nits()),
        ) {
            Ok(()) => {
                println!(
                    "[往返] save_sdr_as_jxl_hdr OK ({} bytes): {}",
                    std::fs::metadata(&tmp).unwrap().len(),
                    tmp.display()
                );
                if let Some(src) = app_lib::viewer::decode::ensure_hdr_source(&tmp) {
                    let (mut mx, mut sum) = (0.0f32, 0.0f64);
                    let n = src.data.len() / 8;
                    for px in src.data.chunks_exact(8) {
                        let r = f16_to_f32(u16::from_le_bytes([px[0], px[1]]));
                        let g = f16_to_f32(u16::from_le_bytes([px[2], px[3]]));
                        let b = f16_to_f32(u16::from_le_bytes([px[4], px[5]]));
                        mx = mx.max(r.max(g).max(b));
                        sum += (r + g + b) as f64 / 3.0;
                    }
                    println!(
                        "[往返] 解码 scRGB: max={:.4} mean={:.6}（SDR白={:.0}nits → 期望 max≈{:.2}）",
                        mx,
                        sum / n as f64,
                        app_lib::capture::monitor::get_sdr_white_level_nits(),
                        app_lib::capture::monitor::get_sdr_white_level_nits() / 80.0
                    );
                } else {
                    println!("[往返] 解码失败/未命中缓存");
                }
            }
            Err(e) => println!("[往返] save_sdr_as_jxl_hdr 失败: {}", e),
        }
    }

    // 3) 渲染终态：色调映射 SDR 临时 PNG
    match app_lib::viewer::decode::decode_to_temp_png(path) {
        Ok((temp, w, h)) => {
            let img = image::open(&temp).expect("打开临时 PNG 失败").to_rgba8();
            let (mut mx, mut sum, mut nz) = (0u8, 0u64, 0usize);
            for p in img.pixels() {
                let m = p.0[0].max(p.0[1]).max(p.0[2]);
                mx = mx.max(m);
                sum += m as u64;
                if m > 2 {
                    nz += 1;
                }
            }
            let n = (w * h) as usize;
            println!(
                "[SDR PNG] {}x{} max={} mean={:.2} 非黑像素 {:.2}%  temp={}",
                w,
                h,
                mx,
                sum as f64 / n as f64,
                nz as f64 * 100.0 / n as f64,
                temp.display()
            );
        }
        Err(e) => println!("[SDR PNG] 解码失败: {}", e),
    }
}
