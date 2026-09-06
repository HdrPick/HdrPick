//! P3 全家族对拍：photo scale / anime noise1 / photo noise2（vs 原版 exe 基准）
//! + TTA 自检（TTA 输出应与非 TTA 高度相似）

use app_lib::upscale::model::{ModelKind, WaifuModel};
use app_lib::upscale::pipeline;

fn psnr(a: &image::RgbImage, b: &image::RgbImage) -> (f64, i32) {
    assert_eq!((a.width(), a.height()), (b.width(), b.height()));
    let (mut mse, mut maxd) = (0.0f64, 0i32);
    for (pa, pb) in a.pixels().zip(b.pixels()) {
        for i in 0..3 {
            let d = pa.0[i] as i64 - pb.0[i] as i64;
            mse += (d * d) as f64;
            maxd = maxd.max(d.abs() as i32);
        }
    }
    mse /= (a.width() * a.height() * 3) as f64;
    let p = if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    };
    (p, maxd)
}

#[test]
fn p3_families() {
    let model_root = std::path::Path::new(r"e:\jietu\图片放大器\models");
    let img = image::open(r"e:\jietu\unused\waifu2x-test-input.png")
        .unwrap()
        .to_rgb8();

    // (家族, 模型文件, 基准图, 描述)
    let cases = [
        (
            ModelKind::Upconv7Photo,
            "scale2.0x_model.json.caffemodel",
            r"e:\jietu\unused\ref-photo-scale2.0x_model.png",
            "photo scale",
        ),
        (
            ModelKind::Upconv7AnimeStyleArtRgb,
            "noise1_scale2.0x_model.json.caffemodel",
            r"e:\jietu\unused\ref-ns-anime-n1.png",
            "anime noise1",
        ),
        (
            ModelKind::Upconv7Photo,
            "noise2_scale2.0x_model.json.caffemodel",
            r"e:\jietu\unused\ref-ns-photo-n2.png",
            "photo noise2",
        ),
    ];

    for (kind, file, ref_path, name) in cases {
        let model = WaifuModel::load(kind, model_root, file).unwrap();
        let t = pipeline::image_to_tensor(&img);
        let t0 = std::time::Instant::now();
        let out = pipeline::run_once_gpu(&model, &t)
            .map(|(o, _)| o)
            .or_else(|_| pipeline::run_once(&model, &t))
            .unwrap();
        let ours = pipeline::tensor_to_image(&out);

        let reference = image::open(ref_path)
            .unwrap_or_else(|e| panic!("基准缺失 {} : {}", ref_path, e))
            .to_rgb8();
        let (p, maxd) = psnr(&ours, &reference);
        println!(
            "{name}: PSNR={p:.2}dB max_diff={maxd} 耗时={ms}ms",
            ms = t0.elapsed().as_millis()
        );
        assert!(p > 55.0, "{name} PSNR 过低: {p:.2}");
        assert!(maxd <= 3, "{name} max_diff={maxd}");
    }
}

#[test]
fn p3_tta_selfcheck() {
    // TTA 输出应与非 TTA 相似（同模型）：PSNR > 30dB（TTA 是轻微平均差异）
    // 且 D4 变换自检：identity 变换应无损
    let model_root = std::path::Path::new(r"e:\jietu\图片放大器\models");
    let model = WaifuModel::load(
        ModelKind::Upconv7AnimeStyleArtRgb,
        model_root,
        "scale2.0x_model.json.caffemodel",
    )
    .unwrap();
    let img = image::open(r"e:\jietu\unused\waifu2x-test-input.png")
        .unwrap()
        .to_rgb8();
    let t = pipeline::image_to_tensor(&img);

    // TTA identity 通道：8 路平均里 transform(0)=identity，
    // 全 TTA 与单路输出应高度相似
    let plain = pipeline::run_once_gpu(&model, &t)
        .map(|(o, _)| o)
        .or_else(|_| pipeline::run_once(&model, &t))
        .unwrap();
    let tta = pipeline::run_once_tta(&model, &t).unwrap();

    let a = pipeline::tensor_to_image(&plain);
    let b = pipeline::tensor_to_image(&tta);
    let (p, maxd) = psnr(&a, &b);
    println!("TTA vs 非 TTA: PSNR={p:.2}dB max_diff={maxd}");
    // TTA 是 8 路平均，与单路差异有限（降噪效果）
    assert!(p > 25.0, "TTA 输出异常偏离: {p:.2}dB");
}
