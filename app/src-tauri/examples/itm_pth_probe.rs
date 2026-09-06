//! HDRTVNet Ensemble_AGCM_LE.pth 变量表探针
//!
//! 运行：cargo run --release --example itm_pth_probe

use app_lib::upscale::pth::PthFile;

fn main() {
    let path = std::path::Path::new("e:/jietu/app/src-tauri/models/itm/Ensemble_AGCM_LE.pth");
    let mut pth = match PthFile::open(path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("打开失败: {}", e);
            // 兜底：打印 zip 内条目名，确认前缀布局
            if let Ok(f) = std::fs::File::open(path) {
                if let Ok(mut a) = zip::ZipArchive::new(f) {
                    let n_files = a.len();
                    for i in 0..n_files.min(9) {
                        if let Ok(n) = a.by_index(i).map(|z| z.name().to_string()) {
                            eprintln!("zip entry: {}", n);
                        }
                    }
                }
            }
            std::process::exit(1);
        }
    };
    let mut names: Vec<String> = pth.names().into_iter().cloned().collect();
    names.sort();
    println!("总变量数: {}", names.len());
    for n in &names {
        let t = pth.tensor(n).unwrap();
        let shape: Vec<String> = t.shape.iter().map(|d| d.to_string()).collect();
        println!("{}  [{}] {}", n, shape.join(","), t.dtype);
    }
}
