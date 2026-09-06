//! 通道响应矩阵：纯 R/G/B 平坦图输入 → 测 3×3 色彩变换矩阵 M
//! 正确实现 M ≈ 单位阵（放大不换色）；M 为置换/混叠 → 权重通道错位

use app_lib::upscale::cpu::{self, Tensor};
use app_lib::upscale::model::{ModelKind, WaifuModel};
use app_lib::upscale::pipeline::pad_replicate;

fn flat_output(model: &WaifuModel, rgb: [f32; 3]) -> [f32; 3] {
    let mut t = Tensor::new(3, 96, 96);
    for y in 0..96 {
        for x in 0..96 {
            t.set(0, y, x, rgb[0]);
            t.set(1, y, x, rgb[1]);
            t.set(2, y, x, rgb[2]);
        }
    }
    let padded = pad_replicate(&t, model.net_offset);
    let out = cpu::forward(model, &padded).unwrap();
    let c = model.net_offset + 96;
    [out.at(0, c, c), out.at(1, c, c), out.at(2, c, c)]
}

#[test]
fn p1_channel_matrix() {
    let model_root = std::path::Path::new(r"e:\jietu\图片放大器\models");
    let model = WaifuModel::load(
        ModelKind::Upconv7AnimeStyleArtRgb,
        model_root,
        "scale2.0x_model.json.caffemodel",
    )
    .unwrap();

    // 基线（黑图）
    let base = flat_output(&model, [0.0, 0.0, 0.0]);
    println!("基线 b = ({:.4},{:.4},{:.4})", base[0], base[1], base[2]);

    // 三原色响应
    for (name, rgb) in [
        ("R", [1.0f32, 0.0, 0.0]),
        ("G", [0.0, 1.0, 0.0]),
        ("B", [0.0, 0.0, 1.0]),
    ] {
        let out = flat_output(&model, rgb);
        let resp = [out[0] - base[0], out[1] - base[1], out[2] - base[2]];
        println!(
            "输入纯{} → 响应 M·{} = ({:.4},{:.4},{:.4})",
            name, name, resp[0], resp[1], resp[2]
        );
    }

    // 完整矩阵打印
    let r = flat_output(&model, [1.0, 0.0, 0.0]);
    let g = flat_output(&model, [0.0, 1.0, 0.0]);
    let b = flat_output(&model, [0.0, 0.0, 1.0]);
    let m = [
        [r[0] - base[0], g[0] - base[0], b[0] - base[0]],
        [r[1] - base[1], g[1] - base[1], b[1] - base[1]],
        [r[2] - base[2], g[2] - base[2], b[2] - base[2]],
    ];
    println!("M = [");
    for row in &m {
        println!("  [{:.4},{:.4},{:.4}]", row[0], row[1], row[2]);
    }
    println!("]");
}
