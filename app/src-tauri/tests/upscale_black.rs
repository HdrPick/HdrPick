//! 黑图 bias 链对拍：全黑输入下网络输出应为常数（每通道），
//! 与原版 exe 输出逐通道对比 —— 精确定位差异层

use app_lib::upscale::cpu::{self, Tensor};
use app_lib::upscale::model::{ModelKind, WaifuModel};
use app_lib::upscale::pipeline::pad_replicate;

#[test]
fn p1_black_bias_chain() {
    let model_root = std::path::Path::new(r"e:\jietu\图片放大器\models");
    let model = WaifuModel::load(
        ModelKind::Upconv7AnimeStyleArtRgb,
        model_root,
        "scale2.0x_model.json.caffemodel",
    )
    .unwrap();

    // 全黑输入 96×96
    let mut t = Tensor::new(3, 96, 96);
    // data 默认全 0 即黑

    let padded = pad_replicate(&t, model.net_offset);
    let out = cpu::forward(&model, &padded).unwrap();
    println!("黑图输出: {}×{}×{}", out.c, out.w, out.h);

    // 黑图下输出应为每通道常数（pad 无影响）；采样中心与边缘验证
    for c in 0..out.c {
        let center = out.at(c, out.h / 2, out.w / 2);
        let corner = out.at(c, 1, 1);
        println!(
            "  通道{}: 中心 {:.6} / 角落 {:.6} {}",
            c,
            center,
            corner,
            if (center - corner).abs() < 1e-6 {
                "(常数✓)"
            } else {
                "(非常数!)"
            }
        );
    }

    // 与原版输出对比：中心区域取值（原版输出 192×192）
    let reference = image::open(r"e:\jietu\unused\waifu2x-black-ref.png")
        .unwrap()
        .to_rgb8();
    println!(
        "原版黑图输出尺寸 {}×{}",
        reference.width(),
        reference.height()
    );
    // 中心像素 + 输出范围统计
    let cx = reference.width() as usize / 2;
    let cy = reference.height() as usize / 2;
    let pc = reference.get_pixel(cx as u32, cy as u32);
    println!("原版中心像素 RGB=({},{},{})", pc.0[0], pc.0[1], pc.0[2]);
    // 我们输出中心裁剪后对应像素
    let crop_x = model.net_offset + 96; // 中心裁剪区中心
    let crop_y = model.net_offset + 96;
    for c in 0..3 {
        let v = out.at(c, crop_y, crop_x);
        println!(
            "  我们通道{} 中心 {:.6} → 8bit {}（原版 {}）",
            c,
            v,
            ((v.clamp(0.0, 1.0) * 255.0) + 0.5).min(255.0) as u8,
            pc.0[c]
        );
    }

    // 全图对比 PSNR
    let mut mse = 0.0f64;
    let mut n = 0usize;
    for y in 0..96 {
        for x in 0..96 {
            for c in 0..3 {
                let v = out.at(c, model.net_offset + y * 2, model.net_offset + x * 2);
                let ours = ((v.clamp(0.0, 1.0) * 255.0) + 0.5).min(255.0) as u8;
                let rp = reference.get_pixel((x * 2) as u32, (y * 2) as u32);
                let d = ours as f64 - rp.0[c] as f64;
                mse += d * d;
                n += 1;
            }
        }
    }
    mse /= n as f64;
    let psnr = if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    };
    println!("黑图对拍 PSNR={:.2}dB (mse={:.4})", psnr, mse);
}
