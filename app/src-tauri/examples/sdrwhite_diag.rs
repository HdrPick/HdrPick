//! SDR 白电平（HDR 内容亮度）诊断：读取 → 设置 → 回读验证
//!
//! 用法：cargo run --example sdrwhite_diag [-- <nits>]
//! 不带参数时自动降一档（-4 nits）再验证回读。

use app_lib::capture::hdr_toggle;

fn main() {
    let states = hdr_toggle::get_hdr_states().expect("查询显示器失败");
    if states.is_empty() {
        println!("未检测到活动显示器");
        return;
    }
    for s in &states {
        println!(
            "{} [{}] supported={} enabled={} sdr_white={:.0} nits",
            s.friendly_name, s.device_name, s.hdr_supported, s.hdr_enabled, s.sdr_white_nits
        );
    }

    // 找一个 HDR 已开启的显示器做 SET → GET 回读验证
    let target = match states.iter().find(|s| s.hdr_enabled) {
        Some(m) => m.clone(),
        None => {
            println!("（无 HDR 开启的显示器，跳过 SET 验证）");
            return;
        }
    };

    // 目标值：命令行参数 or 当前值降一档（4 nits 步进）
    let new_val: f32 = match std::env::args().nth(1) {
        Some(v) => v.parse().unwrap_or(200.0),
        None => ((target.sdr_white_nits / 4.0).round() as i32 * 4 - 4).max(80) as f32,
    };

    println!(
        "\nSET {} ({}) → {:.0} nits ...",
        target.friendly_name, target.device_name, new_val
    );
    match hdr_toggle::set_sdr_white_level(&target.device_name, new_val) {
        Ok(_) => println!("SET OK"),
        Err(e) => {
            println!("SET FAIL: {}", e);
            return;
        }
    }

    // 回读验证
    match hdr_toggle::get_hdr_states() {
        Ok(states2) => {
            if let Some(m2) = states2.iter().find(|s| s.device_name == target.device_name) {
                println!(
                    "回读: {:.0} nits（期望 {:.0}）→ {}",
                    m2.sdr_white_nits,
                    new_val,
                    if (m2.sdr_white_nits - new_val).abs() < 1.0 {
                        "PASS"
                    } else {
                        "MISMATCH"
                    }
                );
            }
        }
        Err(e) => println!("回读失败: {}", e),
    }
}
