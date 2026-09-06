//! P2 逐层对拍调试：找 GPU 首个错误层

use app_lib::upscale::cpu::{self, Tensor};
use app_lib::upscale::d3d11;
use app_lib::upscale::model::{ExecLayer, ModelKind, WaifuModel};
use app_lib::upscale::pipeline::{image_to_tensor, pad_replicate};

#[test]
fn p2_layer_timing() {
    let model_root = std::path::Path::new(r"e:\jietu\图片放大器\models");
    let model = WaifuModel::load(
        ModelKind::Upconv7AnimeStyleArtRgb,
        model_root,
        "scale2.0x_model.json.caffemodel",
    )
    .unwrap();

    let engine = d3d11::engine();
    let sync_engine = engine.as_ref().expect("GPU 引擎");
    let eng = sync_engine.0.lock().unwrap();
    // 大 tile 256 计时（release 下测）
    let mut t = Tensor::new(3, 256, 256);
    for (i, v) in t.data.iter_mut().enumerate() {
        *v = (i % 97) as f32 / 97.0;
    }
    let padded = pad_replicate(&t, model.net_offset);

    let t_sess = std::time::Instant::now();
    let sess = d3d11::GpuSession::new(&eng, &model, 256, 256).unwrap();
    println!("会话创建: {}ms", t_sess.elapsed().as_millis());

    // 全网跑 3 次计时（首次含编译/预热）
    for r in 0..3 {
        let t0 = std::time::Instant::now();
        let out = sess.run_tile(&padded).unwrap();
        println!(
            "第{}轮全网 run_tile: {}ms（输出 {}×{}×{}）",
            r + 1,
            t0.elapsed().as_millis(),
            out.c,
            out.w,
            out.h
        );
    }

    // 逐层增量计时：n 层 vs n-1 层（首次各层预热略过，跑两遍取第二遍）
    let mut prev_ms = 0u128;
    for n in 1..=model.layers.len() {
        let _ = sess.debug_run_layers(&padded, n).unwrap(); // 预热
        let t0 = std::time::Instant::now();
        let _ = sess.debug_run_layers(&padded, n).unwrap();
        let ms = t0.elapsed().as_millis();
        let kind = if matches!(model.layers[n - 1], ExecLayer::Deconv { .. }) {
            "deconv"
        } else {
            "conv"
        };
        println!(
            "层{} {}: 累计 {}ms（增量 {}ms）",
            n,
            kind,
            ms,
            ms.saturating_sub(prev_ms)
        );
        prev_ms = ms;
    }
}

#[test]
fn p2_layer_by_layer() {
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
    let t = image_to_tensor(&img);
    let padded = pad_replicate(&t, model.net_offset);

    let engine = d3d11::engine();
    let sync_engine = engine.as_ref().expect("GPU 引擎");
    let eng = sync_engine.0.lock().unwrap();
    let sess = d3d11::GpuSession::new(&eng, &model, 96, 96).unwrap();

    // n=0：输入上传→读回 roundtrip（特征值诊断：每通道不同常数）
    let mut probe = padded.clone();
    for c in 0..probe.c {
        let v = c as f32 * 0.25 + 0.1; // ch0=0.1 ch1=0.35 ch2=0.6
        for i in probe.data[c * probe.w * probe.h..(c + 1) * probe.w * probe.h].iter_mut() {
            *i = v;
        }
    }
    let rt = sess.debug_run_layers(&probe, 0).unwrap();
    for c in 0..rt.c {
        let m: f64 = rt.data[c * rt.w * rt.h..(c + 1) * rt.w * rt.h]
            .iter()
            .sum::<f32>() as f64
            / (rt.w * rt.h) as f64;
        println!("特征 roundtrip ch{} 均值 = {:.4}", c, m);
    }

    let rt = sess.debug_run_layers(&padded, 0).unwrap();
    let mut rt_maxd = 0.0f32;
    for (a, b) in rt.data.iter().zip(padded.data.iter()) {
        rt_maxd = rt_maxd.max((a - b).abs());
    }
    println!("输入 roundtrip maxd = {:.6}", rt_maxd);
    assert!(rt_maxd == 0.0, "输入上传/读回不一致");

    // CPU 逐层参照
    let mut cpu_cur = padded.clone();
    for n in 1..=model.layers.len() {
        // CPU 第 n 层输出
        let layer = &model.layers[n - 1];
        let cpu_out = match layer {
            ExecLayer::Conv { .. } | ExecLayer::Deconv { .. } => {
                // 槽位重映射：bottom→0、top→1（单层子模型只有两个槽）
                let mut l = layer.clone();
                match &mut l {
                    ExecLayer::Conv { bottom, top, .. } | ExecLayer::Deconv { bottom, top, .. } => {
                        *bottom = 0;
                        *top = 1;
                    }
                    _ => unreachable!(),
                }
                let sub = WaifuModel {
                    layers: vec![l],
                    in_ch: cpu_cur.c,
                    net_offset: 0,
                    inner_scale: 1,
                    kind: ModelKind::Upconv7AnimeStyleArtRgb,
                    n_slots: 2,
                    output_slot: 1,
                };
                cpu::forward(&sub, &cpu_cur).unwrap()
            }
            _ => unreachable!(),
        };

        let gpu_out = sess.debug_run_layers(&padded, n).unwrap();

        // 比较
        let mut mse = 0.0f64;
        let mut maxd = 0.0f32;
        let mut sum = 0.0f64;
        for (a, b) in gpu_out.data.iter().zip(cpu_out.data.iter()) {
            let d = a - b;
            mse += (d * d) as f64;
            maxd = maxd.max(d.abs());
            sum += *a as f64;
        }
        mse /= gpu_out.data.len() as f64;
        let kind = if matches!(layer, ExecLayer::Deconv { .. }) {
            "deconv"
        } else {
            "conv"
        };
        println!(
            "层{} {}: {}×{}×{} GPU均值={:.4} CPU均值={:.4} maxd={:.5} rmse={:.6}",
            n,
            kind,
            gpu_out.c,
            gpu_out.w,
            gpu_out.h,
            sum / gpu_out.data.len() as f64,
            cpu_out.data.iter().sum::<f32>() as f64 / cpu_out.data.len() as f64,
            maxd,
            mse.sqrt()
        );

        // GPU 尺寸与 CPU 尺寸一致性
        assert_eq!(
            (gpu_out.c, gpu_out.w, gpu_out.h),
            (cpu_out.c, cpu_out.w, cpu_out.h),
            "层{} 尺寸不符",
            n
        );

        cpu_cur = cpu_out;
    }
}
