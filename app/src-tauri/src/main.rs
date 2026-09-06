// Prevents additional console window on Windows, DO NOT REMOVE!!
// debug 模式也使用 GUI 子系统，日志写入文件（jietu-hdr.log）
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() {
    app_lib::run()
}
