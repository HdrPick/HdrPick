//! P2 GPU 对拍：GPU 输出 vs CPU 输出（同模型同图）
//! 验收：PSNR > 60dB（FMA 收缩级差异）+ GPU 正确处理分块/边缘
//! 附带性能基准（1080p / 4K 合成图）

use app_lib::upscale::cpu::{self, Tensor};
use app_lib::upscale::model::{ModelKind, WaifuModel};
use app_lib::upscale::pipeline::{image_to_tensor, run_once_gpu, tensor_to_image};

fn psnr_tensors(a: &Tensor, b: &Tensor) -> f64 {
    assert_eq!((a.c, a.h, a.w), (b.c, b.h, b.w));
    let mut mse = 0.0f64;
    for (x, y) in a.data.iter().zip(b.data.iter()) {
        mse += (x - y) as f64 * (x - y) as f64;
    }
    mse /= a.data.len() as f64;
    // 值域 [0,1] → 等效 8bit：×255 后算
    mse *= 255.0 * 255.0;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

/// 合成基准图（渐变 + 色块 + 线条）
fn synth_image(w: u32, h: u32) -> image::RgbImage {
    let mut img = image::RgbImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let r = (x * 255 / w) as u8;
            let g = (y * 255 / h) as u8;
            let b = ((x + y) * 255 / (w + h)) as u8;
            img.put_pixel(x, y, image::Rgb([r, g, b]));
        }
    }
    // 色块 + 线
    for y in (h / 4)..(h / 4 + h / 8) {
        for x in (w / 4)..(w / 4 + w / 8) {
            img.put_pixel(x, y, image::Rgb([232, 60, 60]));
        }
    }
    for i in 0..w.min(h) {
        let p = img.get_pixel_mut(i, i);
        *p = image::Rgb([255, 255, 255]);
    }
    img
}

#[test]
fn p2_gpu_vs_cpu() {
    let model_root = std::path::Path::new(r"e:\jietu\图片放大器\models");
    let model = WaifuModel::load(
        ModelKind::Upconv7AnimeStyleArtRgb,
        model_root,
        "scale2.0x_model.json.caffemodel",
    )
    .unwrap();

    // 96×96（单 tile，含全部边缘情形）
    let img = image::open(r"e:\jietu\unused\waifu2x-test-input.png")
        .unwrap()
        .to_rgb8();
    let t = image_to_tensor(&img);

    let cpu_out = app_lib::upscale::pipeline::run_once(&model, &t).unwrap();
    let (gpu_out, ms) = match run_once_gpu(&model, &t) {
        Ok(v) => v,
        Err(e) => panic!("GPU 路径失败: {}", e),
    };
    let p = psnr_tensors(&gpu_out, &cpu_out);
    println!("96×96: GPU {}ms, PSNR(GPU vs CPU) = {:.2}dB", ms, p);
    assert!(p > 60.0, "GPU/CPU 对拍 PSNR 过低: {:.2}dB", p);

    // 600×400（多 tile + 非整除 tile + 边界 replicate）
    let img2 = synth_image(600, 400);
    let t2 = image_to_tensor(&img2);
    let cpu2 = app_lib::upscale::pipeline::run_once(&model, &t2).unwrap();
    let (gpu2, ms2) = run_once_gpu(&model, &t2).unwrap();
    let p2 = psnr_tensors(&gpu2, &cpu2);
    println!("600×400: GPU {}ms, PSNR = {:.2}dB", ms2, p2);
    assert!(p2 > 60.0, "多 tile PSNR 过低: {:.2}dB", p2);

    // GPU 输出与原版基准三向对拍（96×96 用原版 ref）
    let reference = image::open(r"e:\jietu\unused\waifu2x-test-ref.png")
        .unwrap()
        .to_rgb8();
    let gpu_img = tensor_to_image(&gpu_out);
    let mut mse = 0.0f64;
    let mut maxd = 0i32;
    for (a, b) in gpu_img.pixels().zip(reference.pixels()) {
        for i in 0..3 {
            let d = a.0[i] as i32 - b.0[i] as i32;
            mse += (d * d) as f64;
            maxd = maxd.max(d.abs());
        }
    }
    mse /= (192 * 192 * 3) as f64;
    let psnr_ref = 10.0 * (255.0 * 255.0 / mse).log10();
    println!(
        "GPU vs 原版 exe: PSNR = {:.2}dB, max_diff = {}",
        psnr_ref, maxd
    );
    assert!(psnr_ref > 60.0, "GPU vs 原版 PSNR 过低: {:.2}", psnr_ref);
    assert!(maxd <= 3, "GPU vs 原版最大误差过大: {}", maxd);
}

#[test]
fn p2_gpu_benchmark() {
    let model_root = std::path::Path::new(r"e:\jietu\图片放大器\models");
    let model = WaifuModel::load(
        ModelKind::Upconv7AnimeStyleArtRgb,
        model_root,
        "scale2.0x_model.json.caffemodel",
    )
    .unwrap();

    for (w, h) in [(1920u32, 1080u32), (3840, 2160)] {
        let img = synth_image(w, h);
        let t = image_to_tensor(&img);
        let t0 = std::time::Instant::now();
        let (out, gpu_ms) = match run_once_gpu(&model, &t) {
            Ok(v) => v,
            Err(e) => {
                println!("{}×{}: GPU 不可用（{}）跳过", w, h, e);
                return;
            }
        };
        // 保存样张（人工验收用）
        let _ =
            tensor_to_image(&out).save(format!("e:\\jietu\\unused\\waifu2x-gpu-{}x{}.png", w, h));
        println!(
            "{}×{} → {}×{}: GPU 内部 {}ms（含会话+读回总 {:.0}ms）",
            w,
            h,
            out.w,
            out.h,
            gpu_ms,
            t0.elapsed().as_millis()
        );
    }
}
