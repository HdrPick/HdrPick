//! P1 对拍测试：我们的 CPU 推理 vs 原版 waifu2x-caffe exe 输出
//!
//! 前置：
//! - 输入图 e:\jietu\unused\waifu2x-test-input.png
//! - 基准输出 e:\jietu\unused\waifu2x-test-ref.png（原版 cui exe 生成，scale 2x upconv_7）
//! - 模型 e:\jietu\图片放大器\models\upconv_7_anime_style_art_rgb\scale2.0x_model.json.caffemodel

use app_lib::upscale::model::{ModelKind, WaifuModel};
use app_lib::upscale::pipeline;

#[test]
fn p1_compare_upconv7_scale2x() {
    let input_path = r"e:\jietu\unused\waifu2x-test-input.png";
    let ref_path = r"e:\jietu\unused\waifu2x-test-ref.png";
    let model_root = std::path::Path::new(r"e:\jietu\图片放大器\models");

    // 1. 加载模型（打印层结构辅助诊断）
    let model = WaifuModel::load(
        ModelKind::Upconv7AnimeStyleArtRgb,
        model_root,
        "scale2.0x_model.json.caffemodel",
    )
    .expect("模型加载失败");
    println!(
        "模型: in_ch={} net_offset={} inner_scale={} layers={}",
        model.in_ch,
        model.net_offset,
        model.inner_scale,
        model.layers.len()
    );
    for (i, l) in model.layers.iter().enumerate() {
        match l {
            app_lib::upscale::model::ExecLayer::Conv { out_ch, kernel, .. } => {
                println!("  [{}] conv out={} k={}", i, out_ch, kernel)
            }
            app_lib::upscale::model::ExecLayer::Deconv { out_ch, kernel, .. } => {
                println!("  [{}] deconv out={} k={}", i, out_ch, kernel)
            }
            app_lib::upscale::model::ExecLayer::EltwiseAdd { .. } => println!("  [{}] eltwise", i),
            _ => println!("  [{}] se-op", i),
        }
    }
    assert_eq!(model.in_ch, 3, "RGB 模型应为 3 通道");
    assert_eq!(model.inner_scale, 2, "scale 模型应 2x");
    assert_eq!(
        model.net_offset, 14,
        "upconv_7 原版 offset 应为 14（info.json）"
    );

    // 2. 我们的推理
    let img = image::open(input_path).unwrap().to_rgb8();
    let t0 = std::time::Instant::now();
    let out = pipeline::upscale_image(model_root, ModelKind::Upconv7AnimeStyleArtRgb, &img, 1)
        .expect("推理失败");
    println!("推理耗时 {:.1}ms", t0.elapsed().as_millis());
    assert_eq!((out.width(), out.height()), (192, 192), "输出应为 2x");

    let _ = out.save(r"e:\jietu\unused\waifu2x-test-ours.png");

    // 3. 与基准对比（PSNR / 最大误差 / 完全一致像素占比）
    let reference = match image::open(ref_path) {
        Ok(r) => r.to_rgb8(),
        Err(e) => panic!("基准图缺失，先跑原版 cui 生成: {}", e),
    };
    assert_eq!(
        (reference.width(), reference.height()),
        (192, 192),
        "基准图尺寸异常"
    );

    let (mut mse, mut max_diff, mut exact, mut close1) = (0.0f64, 0i32, 0usize, 0usize);
    let n = (192 * 192 * 3) as f64;
    for (a, b) in out.pixels().zip(reference.pixels()) {
        for i in 0..3 {
            let d = a.0[i] as i32 - b.0[i] as i32;
            let ad = d.abs();
            mse += (d * d) as f64;
            if ad > max_diff {
                max_diff = ad;
            }
            if ad == 0 {
                exact += 1;
            }
            if ad <= 1 {
                close1 += 1;
            }
        }
    }
    mse /= n;
    let psnr = if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    };
    println!(
        "PSNR={:.2}dB max_diff={} exact={:.4}% close(±1)={:.4}%",
        psnr,
        max_diff,
        100.0 * exact as f64 / n,
        100.0 * close1 as f64 / n
    );

    // 验收标准（P1：CPU 直接卷积，同权重同顺序）
    assert!(psnr > 55.0, "PSNR 过低: {:.2}dB", psnr);
    assert!(max_diff <= 3, "最大误差过大: {}", max_diff);
}
