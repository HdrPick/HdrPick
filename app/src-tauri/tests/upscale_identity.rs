//! 恒等性实验：平坦图（全白/全灰）输入，网络输出中心值应 ≈ 输入值
//! waifu2x 网络对平坦区域近似恒等 —— 决定输入口径（0-1 vs 0-255）

use app_lib::upscale::cpu::{self, Tensor};
use app_lib::upscale::model::{ModelKind, WaifuModel};
use app_lib::upscale::pipeline::pad_replicate;

fn flat_output(model: &WaifuModel, value: f32) -> [f32; 3] {
    let mut t = Tensor::new(3, 96, 96);
    for v in t.data.iter_mut() {
        *v = value;
    }
    let padded = pad_replicate(&t, model.net_offset);
    let out = cpu::forward(model, &padded).unwrap();
    let c = model.net_offset + 96; // 中心
    [out.at(0, c, c), out.at(1, c, c), out.at(2, c, c)]
}

#[test]
fn p1_identity_flat() {
    let model_root = std::path::Path::new(r"e:\jietu\图片放大器\models");

    // 两个模型家族交叉验证
    for (kind, name) in [
        (ModelKind::Upconv7AnimeStyleArtRgb, "upconv_7_anime"),
        (ModelKind::Upconv7Photo, "upconv_7_photo"),
        (ModelKind::Upresnet10, "upresnet10"),
    ] {
        let model = match WaifuModel::load(kind, model_root, kind.scale_file()) {
            Ok(m) => m,
            Err(e) => {
                println!("{} 加载失败: {}", name, e);
                continue;
            }
        };
        println!(
            "--- {} (offset={}, inner={}) ---",
            name, model.net_offset, model.inner_scale
        );
        for v in [0.0f32, 0.25, 0.5, 0.75, 1.0] {
            let out = flat_output(&model, v);
            println!(
                "  输入 {:.3} → 输出 ({:.4},{:.4},{:.4})",
                v, out[0], out[1], out[2]
            );
        }
    }
}
