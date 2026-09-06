//! 微测试：黑图输入下单层 conv/deconv 输出必须为常数

use app_lib::upscale::cpu::{self, Tensor};
use app_lib::upscale::model::ExecLayer;

#[test]
fn conv_black_constant() {
    // 模拟 conv1: 3→16, 3x3, pad 0, leaky 0.1
    let weight = vec![0.01f32; 16 * 3 * 3 * 3];
    let bias: Vec<f32> = (0..16).map(|i| i as f32 * 0.1 - 0.5).collect();
    let layer = ExecLayer::Conv {
        weight,
        bias,
        in_ch: 3,
        out_ch: 16,
        kernel: 3,
        pad: 0,
        stride: 1,
        dilation: 1,
        leaky_slope: 0.1,
        bottom: 0,
        top: 1,
    };
    let model = app_lib::upscale::model::WaifuModel {
        layers: vec![layer],
        in_ch: 3,
        net_offset: 0,
        inner_scale: 1,
        kind: app_lib::upscale::model::ModelKind::Upconv7AnimeStyleArtRgb,
        n_slots: 2,
        output_slot: 1,
    };
    let mut input = Tensor::new(3, 100, 100); // 全 0
    let out = cpu::forward(&model, &input).unwrap();
    // 输出应为每通道常数（bias 经 leaky）
    let mut ok = true;
    for c in 0..16 {
        let expected = {
            let b = c as f32 * 0.1 - 0.5;
            if b >= 0.0 {
                b
            } else {
                b * 0.1
            }
        };
        let v0 = out.at(c, 0, 0);
        let v1 = out.at(c, 50, 50);
        let v2 = out.at(c, 97, 97);
        if (v0 - expected).abs() > 1e-6
            || (v1 - expected).abs() > 1e-6
            || (v2 - expected).abs() > 1e-6
        {
            println!(
                "通道{}: ({:.4},{:.4},{:.4}) 期望 {:.4}",
                c, v0, v1, v2, expected
            );
            ok = false;
        }
    }
    assert!(ok, "conv 黑图输出非常数 → conv 实现有 bug");
    println!("conv 黑图常数 ✓");

    // 输入非零常数（0.5）也应输出常数（输出 98×98，读界内）
    for v in input.data.iter_mut() {
        *v = 0.5;
    }
    let out2 = cpu::forward(&model, &input).unwrap();
    for c in 0..16 {
        let v0 = out2.at(c, 0, 0);
        let v1 = out2.at(c, 97, 97);
        assert!(
            (v0 - v1).abs() < 1e-5,
            "常数输入输出非常数: ch{} {} vs {}",
            c,
            v0,
            v1
        );
    }
    println!("conv 常数输入常数输出 ✓");
}

#[test]
fn deconv_black_constant() {
    // deconv: 4→3, k4 s2 p3（upconv_7 尾层样式）
    let weight = vec![0.005f32; 4 * 3 * 4 * 4];
    let bias = vec![0.1f32, -0.2, 0.3];
    let layer = ExecLayer::Deconv {
        weight,
        bias,
        in_ch: 4,
        out_ch: 3,
        kernel: 4,
        stride: 2,
        pad: 3,
        leaky_slope: 1.0,
        bottom: 0,
        top: 1,
    };
    let model = app_lib::upscale::model::WaifuModel {
        layers: vec![layer],
        in_ch: 4,
        net_offset: 0,
        inner_scale: 2,
        kind: app_lib::upscale::model::ModelKind::Upconv7AnimeStyleArtRgb,
        n_slots: 2,
        output_slot: 1,
    };
    let mut input = Tensor::new(4, 50, 50);
    for (i, v) in input.data.iter_mut().enumerate() {
        *v = (i % 4) as f32 * 0.25; // 每通道不同常数
    }
    let out = cpu::forward(&model, &input).unwrap();
    println!("deconv 输出 {}×{}×{}", out.c, out.w, out.h);
    // 每通道应常数（边缘除外——deconv 边缘 pad 丢弃不均）
    // 检查中心区
    for c in 0..3 {
        let v0 = out.at(c, 50, 50);
        let v1 = out.at(c, 60, 80);
        let v2 = out.at(c, 90, 40);
        println!("deconv ch{}: ({:.5},{:.5},{:.5})", c, v0, v1, v2);
        // 中心区域应严格相等
        assert!((v0 - v1).abs() < 1e-6, "deconv 中心非常数 ch{}", c);
    }
    println!("deconv 常数输出 ✓");
}
