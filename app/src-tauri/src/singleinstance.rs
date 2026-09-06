//! 自建单实例（替代 tauri-plugin-single-instance）
//!
//! 为什么不用官方插件：其 Windows 实现依赖"隐藏消息窗口 + FindWindowW"转发，
//! 本机实测该窗口创建失败（hwnd=0）且失败路径被静默吞掉——第二实例检测到
//! mutex 已存在却找不到窗口，既不转发也不退出，直接双开。
//!
//! 本实现（经典 Win32 三件套，无窗口依赖）：
//! - mutex `{id}-sim` 探测：已存在 → 第二实例把 argv 写入队列目录 → SetEvent 通知 → 退出
//! - event `{id}-sie`：首实例后台线程 WaitForSingleObject，被唤醒后扫描队列目录，
//!   逐条解析转发（handle_launch_action）并删除文件
//! - 队列文件名含 PID+时间戳：多路并发转发互不覆盖，无竞态

use std::path::PathBuf;

use tauri::AppHandle;

// V18.2 临时测试后缀：另一开发会话的实例占用原互斥并转发/互杀，测试期隔离
const MUTEX_NAME: &str = "com.jietu.hdr.jietu-sim-v18t";
const EVENT_NAME: &str = "com.jietu.hdr.jietu-sie-v18t";

/// 队列目录：%TEMP%\jietu-hdr\si_queue\
fn queue_dir() -> PathBuf {
    std::env::temp_dir().join("jietu-hdr").join("si_queue")
}

// ===== Win32 FFI（raw 声明，风格对齐 assoc.rs / set_wallpaper） =====
type Handle = *mut core::ffi::c_void;

extern "system" {
    fn CreateMutexW(
        attrs: *const core::ffi::c_void,
        initial_owner: i32,
        name: *const u16,
    ) -> Handle;
    fn CreateEventW(
        attrs: *const core::ffi::c_void,
        manual_reset: i32,
        initial_state: i32,
        name: *const u16,
    ) -> Handle;
    fn SetEvent(ev: Handle) -> i32;
    fn WaitForSingleObject(h: Handle, milliseconds: u32) -> u32;
    fn CloseHandle(h: Handle) -> i32;
    fn GetLastError() -> u32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

const ERROR_ALREADY_EXISTS: u32 = 183;
const WAIT_OBJECT_0: u32 = 0;

/// 启动早期调用：已是第二实例则转发 argv 并返回 true（调用方直接 exit）
pub fn try_forward_secondary() -> bool {
    let mutex = unsafe { CreateMutexW(core::ptr::null(), 1, wide(MUTEX_NAME).as_ptr()) };
    let already = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if !already {
        // 首实例：长期持有 mutex（句柄故意不关，进程退出由系统回收）
        return false;
    }
    if !mutex.is_null() {
        unsafe { CloseHandle(mutex) }; // 第二实例只探测，立即释放引用
    }

    // 提权重启接管：旧实例（普通权限）已发起 runas 并正在退出。
    // 此时 mutex 仍被旧实例持有，若按第二实例处理会转发后自杀 → 应用整体消失。
    // 带 --elevated-restart 标记的新实例等待旧实例退出释放 mutex，随后接管为首实例。
    if std::env::args().any(|a| a == "--elevated-restart") {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let m = unsafe { CreateMutexW(core::ptr::null(), 1, wide(MUTEX_NAME).as_ptr()) };
            if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
                // 旧实例已退出：本实例成为首实例，长期持有 mutex 继续运行
                log::info!("单实例：提权重启接管成功（已等待旧实例退出）");
                return false;
            }
            if !m.is_null() {
                unsafe { CloseHandle(m) };
            }
        }
        // 10s 仍未释放（旧实例卡死？）：保守按第二实例转发退出，避免双开
        log::warn!("单实例：提权重启等待 mutex 超时，按第二实例退出");
    }

    // 写队列文件：si_queue/<pid>_<纳秒>.json（内容 = 完整 argv JSON 数组）
    let dir = queue_dir();
    let _ = std::fs::create_dir_all(&dir);
    let args: Vec<String> = std::env::args().collect();
    let payload = serde_json::to_string(&args).unwrap_or_default();
    let file = dir.join(format!(
        "{}_{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    if let Ok(mut f) = std::fs::File::create(&file) {
        use std::io::Write;
        let _ = f.write_all(payload.as_bytes());
        let _ = f.sync_all();
    }

    // 通知首实例（事件不存在说明首实例正在退出，转发作废即可）
    let ev = unsafe { CreateEventW(core::ptr::null(), 0, 0, wide(EVENT_NAME).as_ptr()) };
    if !ev.is_null() {
        unsafe {
            SetEvent(ev);
            CloseHandle(ev);
        }
    }
    log::info!("单实例：第二实例已转发 argv（{} 项）并退出", args.len());
    true
}

/// 首实例调用：启动后台监听线程，被事件唤醒后处理队列
pub fn start_listener(app: AppHandle) {
    // 非手动重置事件：每次 SetEvent 唤醒一次等待（自动复位）
    let ev = unsafe { CreateEventW(core::ptr::null(), 0, 0, wide(EVENT_NAME).as_ptr()) };
    if ev.is_null() {
        log::warn!("单实例：创建通知事件失败，二次调用将各自启动（不影响功能）");
        return;
    }
    // 裸句柄跨线程：包装为 Send（WaitForSingleObject 线程安全）
    struct SendHandle(Handle);
    unsafe impl Send for SendHandle {}
    let ev = SendHandle(ev);
    std::thread::spawn(move || {
        let ev = ev; // 整体捕获 SendHandle（2021 精确捕获只取字段会绕过 Send 包装）
        loop {
            let rc = unsafe { WaitForSingleObject(ev.0, u32::MAX) };
            if rc != WAIT_OBJECT_0 {
                break; // 句柄异常，放弃监听
            }
            process_queue(&app);
        }
    });
}

/// 扫描队列目录：逐条解析 argv → 路由 → 删除
fn process_queue(app: &AppHandle) {
    let dir = queue_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            let _ = std::fs::remove_file(&path); // 损坏条目直接清理
            continue;
        };
        let _ = std::fs::remove_file(&path); // 先取走再处理，防重复消费
        let args: Vec<String> = match serde_json::from_str(&content) {
            Ok(a) => a,
            Err(_) => continue,
        };
        log::info!("单实例：收到转发 argv: {:?}", args);
        let action = super::parse_launch_args_public(&args);
        super::handle_launch_action_public(app, action);
    }
}
