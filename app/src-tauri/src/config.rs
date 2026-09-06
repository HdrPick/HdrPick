//! 配置管理
//!
//! 使用 TOML 持久化用户配置：保存路径、热键、输出格式、色调映射参数等。

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::capture::hdr_pipeline::HdrToSdrParams;
use crate::color::TonemapOperator;
use crate::encode::{OutputFormat, QualityLevel};

/// 全局配置
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    /// 截图保存目录（None = 默认 Pictures/jietu-hdr）
    #[serde(default)]
    pub save_dir: Option<PathBuf>,

    /// 默认输出格式
    #[serde(default)]
    pub output_format: OutputFormat,

    /// 编码质量档（JXL/JXR；默认无损）
    #[serde(default)]
    pub output_quality: QualityLevel,

    /// HDR 输出位深（JXL：12=体积优先 / 16=精度优先；PNG 规范限 16bit 不受影响）
    #[serde(default = "default_output_depth")]
    pub output_depth: u32,

    /// HDR→SDR 色调映射预设系统（v2；替代旧 tonemap/peak_nits/sdr_white_nits/brightness 散字段）
    #[serde(default)]
    pub tonemap_settings: ToneMapConfig,

    /// HDR 开启时读到的系统 SDR 参考白（DisplayConfig SDR_WHITE_LEVEL，nits）。
    /// SDR 模式下滑块不可读（raw 恒 1000→80nits），SDR→HDR 编码（AI 放大输出）
    /// 用此记忆值还原"用户 HDR 期的 SDR 白"，避免输出偏暗。
    #[serde(default)]
    pub last_hdr_sdr_white: Option<f32>,

    /// 截图热键（进入区域选择标注模式）
    #[serde(default = "default_region_hotkey")]
    pub region_hotkey: Hotkey,

    /// 全屏截图热键（直接捕获全屏并显示预览，初版行为）
    #[serde(default = "default_fullscreen_hotkey")]
    pub fullscreen_hotkey: Hotkey,

    /// 静默截图热键（直接捕获保存，无标注无预览）
    #[serde(default = "default_silent_hotkey")]
    pub silent_hotkey: Hotkey,

    /// 开始录制热键（游戏模式直启全屏 JXL 录制；None = 禁用热键）
    #[serde(default = "default_record_start_hotkey")]
    pub record_start_hotkey: Option<Hotkey>,
    /// 停止录制热键（录制期间临时注册；None = 仅面板/OSD 可停止）
    #[serde(default = "default_record_stop_hotkey")]
    pub record_stop_hotkey: Option<Hotkey>,

    /// 开始视频录制热键（游戏模式直启全屏 MKV 录制 + OSD；None = 禁用）
    #[serde(default = "default_video_start_hotkey")]
    pub video_start_hotkey: Option<Hotkey>,
    /// 停止视频录制热键（视频录制期间临时注册；无此热键则热键直启拒绝启动）
    #[serde(default = "default_video_stop_hotkey")]
    pub video_stop_hotkey: Option<Hotkey>,

    /// 捕获后复制到剪贴板
    #[serde(default = "default_copy_to_clipboard")]
    pub copy_to_clipboard: bool,

    /// 捕获后自动保存（关闭则仅显示预览）
    #[serde(default = "default_auto_save")]
    pub auto_save: bool,

    /// 是否显示截图预览窗口
    #[serde(default = "default_show_preview")]
    pub show_preview: bool,

    /// 截图确认后是否弹出标注工具栏（false = 仅保留确认/复制快捷操作）
    #[serde(default = "default_show_toolbar")]
    pub show_toolbar: bool,

    /// 是否启用贴图功能（false = 工具栏隐藏贴图按钮）
    #[serde(default = "default_enable_pin")]
    pub enable_pin: bool,

    /// 「截图标识」标注模式截图方式："region"（拖选区域）/"fullscreen"（整屏快照直接标注）
    #[serde(default = "default_annotation_capture_mode")]
    pub annotation_capture_mode: String,

    /// 截图保存/复制成功后的提示音（默认关闭；MessageBeep Asterisk）
    #[serde(default)]
    pub sound_enabled: bool,

    /// OCR 语言（空 = 自动：zh-Hans > zh-Hant > zh-* > en-US > 第一个可用）
    #[serde(default)]
    pub ocr_language: String,

    /// 静默截图目标显示器："primary"（主显示器）/ "cursor"（鼠标所在显示器）
    #[serde(default = "default_silent_monitor")]
    pub silent_monitor: String,

    /// 截图时是否将本工具面板一同截入画面
    /// 仅置顶（钉住）状态生效：面板不隐藏时，true=入镜 / false=WDA_EXCLUDEFROMCAPTURE 排除
    #[serde(default)]
    pub capture_include_tool: bool,

    /// 截图时是否自动隐藏（最小化）主面板（false = 面板保留在画面中）
    #[serde(default = "default_hide_main_on_capture")]
    pub hide_main_on_capture: bool,

    /// 文件命名模板（支持 {date}, {time}, {index}）
    #[serde(default = "default_filename_template")]
    pub filename_template: String,

    /// 关闭主窗口时最小化到托盘（true）而非退出（false）
    #[serde(default = "default_minimize_to_tray")]
    pub minimize_to_tray: bool,

    /// 开机自启动
    #[serde(default)]
    pub autostart: bool,

    /// 主题色（预设 id 或 #RRGGBB 自定义色）
    #[serde(default = "default_theme_color")]
    pub theme_color: String,

    /// 自定义主题色方案（方案名 → 主题色）
    #[serde(default)]
    pub theme_schemes: HashMap<String, ThemeScheme>,

    /// 内存清理配置（键名与 memreduct ini 语义对齐，见 viewer-design.md 4.2.4）
    #[serde(default)]
    pub memory_clean: MemoryCleanConfig,

    /// 系统集成：文件关联 + 资源管理器右键菜单（默认关，开启时写 HKCU 注册表）
    #[serde(default)]
    pub file_assoc: bool,

    /// 主窗口上次的位置与大小（启动时恢复；None = 使用 tauri.conf 默认值）
    #[serde(default)]
    pub main_window: Option<MainWindowState>,

    /// 录屏（JXL 动图）配置
    #[serde(default)]
    pub recording: RecordingConfig,

    /// 视频录制/播放配置（video/ 模块，videorec 接入）
    #[serde(default)]
    pub video: VideoConfig,

    /// 查看器配置段（动图播放解码后端等）
    #[serde(default)]
    pub viewer: ViewerConfig,

    /// 截图保存后自动 AI 2x 增强（后台生成 <stem>_waifu2x_x2.png 增强副本，不替换原图）
    #[serde(default)]
    pub upscale_after_capture: bool,
}

/// 录屏配置（record/ 模块；设计文档 docs/录屏JXL动图设计方案.md §4.7）
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordingConfig {
    /// 质量档（默认 High：录制实时性与质量的平衡点）
    pub quality: QualityLevel,
    /// 目标帧率上限（0 = auto：输出区域 ≥4K → 30fps，其余 60fps）
    pub fps: u32,
    /// 录制时长上限（秒；默认 30，解析时钳制 1..=30——环形缓冲内存预算以
    /// recorder::MAX_RECORD_MS 的 30s 绝对上限为设计依据）
    #[serde(default = "default_recording_max_seconds", deserialize_with = "clamp_recording_max_seconds")]
    pub max_seconds: u32,
    /// 游戏模式静默延迟启动（秒；默认 5，解析时钳制 0..=10，0 = 立即启动）。
    /// 按热键后先显示 OSD 倒计时，给用户切回游戏的时间（期间不抢焦点、
    /// 不抓帧，30s 上限从抓帧 start 计时，延迟不计入）。仅 osd 路径生效。
    #[serde(default = "default_recording_start_delay_seconds", deserialize_with = "clamp_recording_start_delay_seconds")]
    pub start_delay_seconds: u32,
    /// 环形缓冲字节保险丝（MB；30s 硬上限下满变化率预算，64GB 机器默认 40GB）
    pub ring_bytes_mb: usize,
    /// zstd 水位阈值（0..1；环形水位超阈值后新帧压缩，常态零压缩成本）
    pub zstd_watermark: f64,
    /// 纯录制模式（游戏场景）：录制期间不编码（CPU 全归前台），停止后全核编码
    pub deferred_encode: bool,
    /// 采集后端："auto"（游戏模式 WGC 优先 + DDA 兜底，桌面录制 DDA）/
    /// "wgc"（强制 WGC，失败即报错）/"dda"（强制 DDA）。
    /// 解析容错：非 "wgc"/"dda" 一律按 auto 处理（record/commands.rs 解析）。
    pub capture_backend: String,
    /// 保存目录（None = 沿用截图保存目录）
    pub save_dir: Option<PathBuf>,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            quality: QualityLevel::High,
            fps: 0,
            max_seconds: default_recording_max_seconds(),
            start_delay_seconds: default_recording_start_delay_seconds(),
            ring_bytes_mb: 40960,
            zstd_watermark: 0.4,
            deferred_encode: false,
            capture_backend: "auto".to_string(),
            save_dir: None,
        }
    }
}

fn default_recording_max_seconds() -> u32 {
    30
}

/// 视频录制/播放配置（videorec 接入；docs/视频模块接入主程序方案.md 第七章）
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoConfig {
    /// 编码器："h264" | "hevc"（HDR 强制 hevc，S3 接入）
    pub codec: String,
    /// 硬编："auto"（nvenc→amf→qsv→libopenh264 探测链）| "nvenc" | "amf" | "qsv" | "cpu"
    pub hw: String,
    /// 目标帧率（默认 60）
    pub fps: u32,
    /// 码率（Mbps，默认 20）
    pub bitrate_mbps: f64,
    /// 关键帧间隔（帧，默认 120 = 60fps 下 2s）
    pub gop: u32,
    /// 音频："off" | "system"（回环）| "both"（回环+麦克风混录）
    pub audio: String,
    /// 录制模式："fullscreen"（全屏）| "follow"（追随鼠标，窗口居中光标并钳制在显示器内）
    pub mode: String,
    /// 全屏目标显示器索引（0 = 主显示器；video_list_monitors 枚举序）
    #[serde(default)]
    pub monitor_index: u32,
    /// 追随鼠标：窗口宽（像素，偶数；默认 1280）
    #[serde(default = "default_follow_w")]
    pub follow_w: u32,
    /// 追随鼠标：窗口高（像素，偶数；默认 720）
    #[serde(default = "default_follow_h")]
    pub follow_h: u32,
    /// 自动停止：时长上限（秒；0 = 无限。暂停期不计——按媒体时长判定）
    #[serde(default)]
    pub max_seconds: u32,
    /// 自动停止：文件大小上限（MB；0 = 无限）
    #[serde(default)]
    pub max_size_mb: u64,
    /// 分卷阈值（GB；0 = 不分卷。单文件超阈值自动收尾 + 新文件 -partN 后缀）
    #[serde(default)]
    pub split_size_gb: u64,
    /// 静默自动停止：持续无声秒数触发（0 = 关闭。会议/网课场景）
    #[serde(default)]
    pub silence_stop_seconds: u32,
    /// 静默触发后的倒计时秒数（期间声音恢复自动取消续录）
    #[serde(default = "default_silence_countdown")]
    pub silence_countdown_seconds: u32,
    /// 录制叠加鼠标光标（DDA 帧不含指针——教程录制默认开）
    #[serde(default = "default_true")]
    pub mouse_cursor: bool,
    /// 录制叠加点击效果（左=青圈 / 右=红圈，0.5s 扩散）
    #[serde(default)]
    pub mouse_click: bool,
    /// 录制叠加高亮效果（点击点为中心半透明色块，1s 渐隐）
    #[serde(default)]
    pub mouse_highlight: bool,
    /// 高亮颜色 "#RRGGBB"（默认黄）
    #[serde(default = "default_mouse_highlight_color")]
    pub mouse_highlight_color: String,
    /// 高亮块边长（像素，16..=300）
    #[serde(default = "default_mouse_highlight_size")]
    pub mouse_highlight_size: u32,
    /// 水印文字（空 = 关；\n 多条；{ts} = 本地时间戳）
    #[serde(default)]
    pub watermark_text: String,
    /// 水印图片路径（空 = 关；PNG，WIC 解码）
    #[serde(default)]
    pub watermark_image: String,
    /// 水印九宫格："tl".."br"（默认 br 右下）
    #[serde(default = "default_watermark_pos")]
    pub watermark_pos: String,
    /// 水印不透明度（1..=100）
    #[serde(default = "default_watermark_opacity")]
    pub watermark_opacity: u32,
    /// 水印边距（像素）
    #[serde(default = "default_watermark_margin")]
    pub watermark_margin: u32,
    /// 水印字号（像素，12..=72）
    #[serde(default = "default_watermark_font")]
    pub watermark_font_size: u32,
    /// 录制完成动作："none"（默认）| "new"（录新视频）| "exit"（退出程序）| "shutdown"（关机）
    #[serde(default)]
    pub complete_action: String,
    /// 关机倒计时秒数（complete_action=shutdown 时；期间可取消）
    #[serde(default = "default_shutdown_countdown")]
    pub shutdown_countdown_seconds: u32,
    /// 计划录制开关（后端调度线程 30s 轮询；到点自动开始）
    #[serde(default)]
    pub schedule_enabled: bool,
    /// 计划重复："once"（触发一次后自动关闭）| "daily" | "weekly"
    #[serde(default = "default_schedule_repeat")]
    pub schedule_repeat: String,
    /// 每周触发日（weekly 时）：逗号分隔 1-7（周一=1；空 = 不触发）
    #[serde(default)]
    pub schedule_weekdays: String,
    /// 计划开始时间 "HH:MM"
    #[serde(default = "default_schedule_start")]
    pub schedule_start: String,
    /// 计划结束时间 "HH:MM"（到点自动停止——换算时长上限）
    #[serde(default = "default_schedule_end")]
    pub schedule_end: String,
    /// 上次触发日 "YYYY-MM-DD"（防当日重复触发；daily/weekly 持久化记忆）
    #[serde(default)]
    pub schedule_last_fired: String,
    /// 摄像头画中画设备（symbolic link；空 = 关）
    #[serde(default)]
    pub camera_device: String,
    /// 摄像头 PiP 显示宽（像素；0 = 自适应 = 输出宽 1/4）
    #[serde(default = "default_camera_width")]
    pub camera_width: u32,
    /// 摄像头九宫格："tl".."br"（默认 br 右下）
    #[serde(default = "default_camera_pos")]
    pub camera_pos: String,
    /// 摄像头边距（像素）
    #[serde(default = "default_camera_margin")]
    pub camera_margin: u32,
    /// 摄像头水平翻转（前置摄像头镜像习惯）
    #[serde(default)]
    pub camera_flip_h: bool,
    /// 色度键抠像开关（绿幕/纯色背景透明化）
    #[serde(default)]
    pub camera_chroma_key: bool,
    /// 键控颜色 "#RRGGBB"（默认绿）
    #[serde(default = "default_camera_key_color")]
    pub camera_key_color: String,
    /// 键控相似度（0..=100 → 阈值 0..=255；越大抠除范围越广）
    #[serde(default = "default_camera_similarity")]
    pub camera_similarity: u32,
    /// 播放位置记忆
    pub resume_playback: bool,
    /// 播放器画质增强 FX 名（V18；"OFF" = 关闭；缺省 ENHANCE-ALL）
    #[serde(default = "default_fx_shader")]
    pub player_fx_shader: String,
    /// 播放器 FX 参数（8 槽位；配合 player_fx_shader）
    #[serde(default = "default_fx_params")]
    pub player_fx_params: [f32; 8],
    /// 视频渲染后端（V18.6）："internal" = 内置 D3D11；"mpcvr" = MPC Video Renderer
    /// （色彩管线由 MPCVR 承担；未安装自动回退 internal）
    #[serde(default = "default_player_renderer")]
    pub player_renderer: String,
}

fn default_player_renderer() -> String {
    "internal".to_string()
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            codec: "h264".to_string(),
            hw: "auto".to_string(),
            fps: 60,
            bitrate_mbps: 20.0,
            gop: 120,
            audio: "system".to_string(),
            mode: "fullscreen".to_string(),
            monitor_index: 0,
            follow_w: default_follow_w(),
            follow_h: default_follow_h(),
            max_seconds: 0,
            max_size_mb: 0,
            split_size_gb: 0,
            silence_stop_seconds: 0,
            silence_countdown_seconds: default_silence_countdown(),
            mouse_cursor: true,
            mouse_click: false,
            mouse_highlight: false,
            mouse_highlight_color: default_mouse_highlight_color(),
            mouse_highlight_size: default_mouse_highlight_size(),
            watermark_text: String::new(),
            watermark_image: String::new(),
            watermark_pos: default_watermark_pos(),
            watermark_opacity: default_watermark_opacity(),
            watermark_margin: default_watermark_margin(),
            watermark_font_size: default_watermark_font(),
            complete_action: String::new(),
            shutdown_countdown_seconds: default_shutdown_countdown(),
            schedule_enabled: false,
            schedule_repeat: default_schedule_repeat(),
            schedule_weekdays: String::new(),
            schedule_start: default_schedule_start(),
            schedule_end: default_schedule_end(),
            schedule_last_fired: String::new(),
            camera_device: String::new(),
            camera_width: default_camera_width(),
            camera_pos: default_camera_pos(),
            camera_margin: default_camera_margin(),
            camera_flip_h: false,
            camera_chroma_key: false,
            camera_key_color: default_camera_key_color(),
            camera_similarity: default_camera_similarity(),
            resume_playback: true,
            player_fx_shader: default_fx_shader(),
            player_fx_params: default_fx_params(),
            player_renderer: default_player_renderer(),
        }
    }
}

fn default_fx_shader() -> String {
    "ENHANCE-ALL".to_string()
}

fn default_fx_params() -> [f32; 8] {
    [0.35, 0.4, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0]
}

fn default_true() -> bool {
    true
}

fn default_mouse_highlight_color() -> String {
    "#FFD400".to_string()
}

fn default_mouse_highlight_size() -> u32 {
    120
}

fn default_shutdown_countdown() -> u32 {
    30
}

fn default_schedule_repeat() -> String {
    "daily".to_string()
}

fn default_schedule_start() -> String {
    "09:00".to_string()
}

fn default_schedule_end() -> String {
    "10:00".to_string()
}

fn default_silence_countdown() -> u32 {
    10
}

fn default_follow_w() -> u32 {
    1280
}

fn default_follow_h() -> u32 {
    720
}

fn default_watermark_pos() -> String {
    "br".to_string()
}

fn default_watermark_opacity() -> u32 {
    100
}

fn default_watermark_margin() -> u32 {
    16
}

fn default_watermark_font() -> u32 {
    24
}

fn default_camera_width() -> u32 {
    320
}

fn default_camera_pos() -> String {
    "br".to_string()
}

fn default_camera_margin() -> u32 {
    16
}

fn default_camera_key_color() -> String {
    "#00FF00".to_string()
}

fn default_camera_similarity() -> u32 {
    60
}

fn default_recording_start_delay_seconds() -> u32 {
    5
}

/// max_seconds 解析钳制（1..=30）：serde 自定义反序列化在解析层统一生效，
/// 与加载路径无关（Config::load / 前端写回的 config.toml 均覆盖）
fn clamp_recording_max_seconds<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = u32::deserialize(deserializer)?;
    Ok(v.clamp(1, 30))
}

/// start_delay_seconds 解析钳制（0..=10；0 = 立即启动），同 max_seconds 模式
fn clamp_recording_start_delay_seconds<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = u32::deserialize(deserializer)?;
    Ok(v.clamp(0, 10))
}

/// 查看器配置段（config.toml `[viewer]`）
///
/// README 式说明（无前端 UI 开关，手动编辑 config.toml 生效）：
/// ```toml
/// [viewer]
/// # JXL 动图播放解码后端：
/// #   "native" = 纯 CPU libjxl 直出（默认；最稳，全平台可用）
/// #   "hybrid" = 系数快照 + GPU 混合管线重建（DCT8 块 GPU 化，实验性；
/// #              需 D3D11 可用，GPU 引擎初始化失败时打开播放器报错）
/// anim_backend = "native"
///
/// # 动图播放路径：
/// #   "gpu"  = GPU 常驻显存播放（池纹理直写 + 引擎设备交换链；仅 hybrid
/// #            后端生效，engine 不可用 / WARP / 显存预算 < 3 帧时回退 ring）
/// #   "ring" = 内存环形缓冲 + 逐帧 CPU 上传（原路径）
/// anim_playback = "gpu"
/// # GPU 池显存预算（MB；0 = auto = min(4096, 专用显存 50% − 512MB)，
/// # 非 0 时同样被"专用显存 50% − 512MB"钳制）
/// anim_vram_mb = 0
///
/// # 缩略图后台预热（启动 2s 后全库解码填充 LRU；默认 false——
/// # 机械硬盘/低配机上预热抢 IO 洪峰，关闭后首次浏览按需生成）
/// thumb_prewarm = false
/// ```
/// 解析容错：非法值 / 缺失 / 空串一律回退默认（anim_backend→"native"、
/// anim_playback→"gpu"、anim_vram_mb≤0→auto 预算）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewerConfig {
    /// 动图播放解码后端："native" | "hybrid"
    pub anim_backend: String,
    /// 动图播放路径："gpu"（GPU 常驻显存池）| "ring"（内存环形缓冲）
    pub anim_playback: String,
    /// GPU 池显存预算（MB；0 = auto）
    pub anim_vram_mb: u32,
    /// 缩略图后台预热（默认 false：启动 2s 后全库解码填 LRU 会抢机械盘 IO；
    /// 关闭后首次浏览按需生成，滚动浏览仍命中 LRU 缓存）
    pub thumb_prewarm: bool,
}

impl Default for ViewerConfig {
    fn default() -> Self {
        Self {
            anim_backend: "native".to_string(),
            anim_playback: "gpu".to_string(),
            anim_vram_mb: 0,
            thumb_prewarm: false,
        }
    }
}

/// 主窗口几何状态（物理像素）
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct MainWindowState {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// 内存清理配置段（对照 memreduct ini 配置项，全部带 serde 默认值向后兼容）
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryCleanConfig {
    /// 清理区域掩码（默认 231 = memory::MASK_DEFAULT，不含危险区域）
    pub mask: u32,
    /// 阈值触发开关（对照 AutoreductEnable，默认关）
    pub auto_enable: bool,
    /// 阈值百分比（对照 AutoreductValue，默认 90）
    pub threshold_percent: u32,
    /// 间隔触发开关（对照 AutoreductIntervalEnable，默认关）
    pub interval_enable: bool,
    /// 间隔分钟（对照 AutoreductIntervalValue，默认 30，范围 1-1440）
    pub interval_minutes: u32,
    /// 两次清理最小间隔秒数（对照 AUTOREDUCT_COOLDOWN=30，jietu 增强开放为配置）
    pub cooldown_seconds: u32,
    /// 清理完成通知（对照 BalloonCleanResults，默认开；jietu 为前端 toast）
    pub notify_enable: bool,
    /// 清理结果写日志（对照 LogCleanResults，默认关）
    pub log_results: bool,
    /// 托盘徽章警告档百分比（对照 TrayLevelWarning，默认 70）
    pub danger_warning: u32,
    /// 托盘徽章危险档百分比（对照 TrayLevelDanger，默认 90）
    pub danger_critical: u32,
    /// 清理内存热键（完整复刻 SOURCE_HOTKEY；None = 未配置不注册）
    /// 默认 Ctrl+Shift+M
    pub hotkey: Option<Hotkey>,
    /// 清理统计（持久化，冷却判定 + UI 显示）
    pub stats: CleanStats,
}

/// 清理统计（对照 memreduct StatisticLastReduct + jietu 增强的累计两项）
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CleanStats {
    /// 上次清理 Unix 时间戳（冷却判定依据 + UI「最近清理」显示）
    pub last_clean_ts: u64,
    /// 上次清理释放字节数
    pub last_freed_bytes: u64,
    /// 累计清理次数（jietu 增强）
    pub total_clean_count: u64,
    /// 累计释放字节数（jietu 增强）
    pub total_freed_bytes: u64,
}

/// 自定义主题色方案
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThemeScheme {
    /// 主题色（#RRGGBB）
    pub primary: String,
}

// ==================== HDR→SDR 色调映射预设系统（v2 文档 §6） ====================

/// 色调映射预设（完整参数快照；算子专属参数平铺存储，TOML 友好）
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ToneMapPreset {
    /// 稳定 ID：`builtin:soft` / `custom:<时间戳>`
    pub id: String,
    /// 显示名
    pub name: String,
    /// 一句话说明（UI 卡片副标题）
    pub desc: String,
    pub operator: TonemapOperator,
    /// HDR 内容假定峰值（nits）
    pub source_peak_nits: f32,
    /// SDR 白在输出中的落点（1−d 为高光 headroom）
    pub output_diffuse_white: f32,
    /// 曝光档（EV）
    pub exposure_ev: f32,
    /// 饱和度补偿
    pub saturation: f32,
    /// 编码域对比度
    pub contrast: f32,
    /// 中性轴色域压缩强度（0=关闭）
    pub gamut_strength: f32,
    /// 量化抖动
    pub dither: bool,
    /// 智能自适应源峰值
    pub adaptive_peak: bool,
    /// SmoothKnee 线性域拐点（仅 SmoothKnee 算子）
    pub knee_start: f32,
    /// 内置预设不可删除
    pub builtin: bool,
}

impl Default for ToneMapPreset {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            desc: String::new(),
            operator: TonemapOperator::Bt2390,
            source_peak_nits: 1000.0,
            output_diffuse_white: 0.75,
            exposure_ev: 0.0,
            saturation: 1.02,
            contrast: 1.0,
            gamut_strength: 0.8,
            dither: true,
            adaptive_peak: false,
            knee_start: 0.9,
            builtin: false,
        }
    }
}

/// 高级面板临时调整（draft；None = 跟随预设，不覆盖算子与算子专属参数）
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToneMapOverrides {
    pub source_peak_nits: Option<f32>,
    pub output_diffuse_white: Option<f32>,
    pub exposure_ev: Option<f32>,
    pub saturation: Option<f32>,
    pub contrast: Option<f32>,
    pub gamut_strength: Option<f32>,
    pub dither: Option<bool>,
    pub adaptive_peak: Option<bool>,
}

impl ToneMapOverrides {
    pub fn is_empty(&self) -> bool {
        self.source_peak_nits.is_none()
            && self.output_diffuse_white.is_none()
            && self.exposure_ev.is_none()
            && self.saturation.is_none()
            && self.contrast.is_none()
            && self.gamut_strength.is_none()
            && self.dither.is_none()
            && self.adaptive_peak.is_none()
    }
}

/// 色调映射配置段（config.toml `[tonemap_settings]`）
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ToneMapConfig {
    /// 当前激活预设 id
    pub active_preset: String,
    /// 未保存的临时调整
    pub overrides: ToneMapOverrides,
    /// 用户自定义预设
    pub custom_presets: Vec<ToneMapPreset>,
}

impl Default for ToneMapConfig {
    fn default() -> Self {
        Self {
            active_preset: "builtin:soft".to_string(),
            overrides: ToneMapOverrides::default(),
            custom_presets: Vec::new(),
        }
    }
}

impl ToneMapConfig {
    /// 内置预设（v2 文档 §6.2；soft 为默认，参数按当前观感校准）
    pub fn builtin_presets() -> Vec<ToneMapPreset> {
        let mk = |id: &str,
                  name: &str,
                  desc: &str,
                  op: TonemapOperator,
                  d: f32,
                  ev: f32,
                  sat: f32,
                  contrast: f32,
                  adaptive: bool| ToneMapPreset {
            id: id.to_string(),
            name: name.to_string(),
            desc: desc.to_string(),
            operator: op,
            source_peak_nits: 1000.0,
            output_diffuse_white: d,
            exposure_ev: ev,
            saturation: sat,
            contrast,
            gamut_strength: 0.8,
            dither: true,
            adaptive_peak: adaptive,
            knee_start: 0.9,
            builtin: true,
        };
        vec![
            mk(
                "builtin:soft",
                "柔和日常",
                "温柔不过曝，日常截图首选",
                TonemapOperator::Bt2390,
                0.75,
                0.0,
                1.02,
                1.0,
                false,
            ),
            mk(
                "builtin:natural",
                "真实还原",
                "忠实记录高光层次",
                TonemapOperator::Bt2390,
                0.80,
                0.0,
                1.00,
                1.0,
                false,
            ),
            mk(
                "builtin:highlight",
                "高光保护",
                "亮部细节优先，高光柔和不炸",
                TonemapOperator::Reinhard,
                0.75,
                0.0,
                1.02,
                0.98,
                false,
            ),
            mk(
                "builtin:vivid",
                "游戏鲜艳",
                "色彩饱满，画面通透明快",
                TonemapOperator::Bt2390,
                0.78,
                0.05,
                1.15,
                1.05,
                false,
            ),
            mk(
                "builtin:cinema",
                "影视灰阶",
                "电影感灰阶，柔和过渡",
                TonemapOperator::Aces,
                0.72,
                0.0,
                0.92,
                1.08,
                false,
            ),
            mk(
                "builtin:auto",
                "智能自适应",
                "自动分析画面亮度，免调参",
                TonemapOperator::Bt2390,
                0.75,
                0.0,
                1.02,
                1.0,
                true,
            ),
        ]
    }

    /// 解析当前激活预设（内置 → 自定义 → fallback soft）
    pub fn resolve_active_preset(&self) -> ToneMapPreset {
        ToneMapConfig::builtin_presets()
            .into_iter()
            .find(|p| p.id == self.active_preset)
            .or_else(|| {
                self.custom_presets
                    .iter()
                    .find(|p| p.id == self.active_preset)
                    .cloned()
            })
            .unwrap_or_else(|| ToneMapPreset {
                id: "builtin:soft".to_string(),
                name: "柔和日常".to_string(),
                desc: String::new(),
                ..Default::default()
            })
    }
}

/// 热键定义
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hotkey {
    /// 修饰符（MOD_ALT=1, MOD_CONTROL=2, MOD_SHIFT=4, MOD_WIN=8）
    pub modifiers: u32,
    /// 虚拟键码（VK_*）
    pub vk: u32,
}

impl Hotkey {
    pub const MOD_ALT: u32 = 0x0001;
    pub const MOD_CONTROL: u32 = 0x0002;
    pub const MOD_SHIFT: u32 = 0x0004;
    pub const MOD_WIN: u32 = 0x0008;

    /// VK_SNAPSHOT = PrintScreen (0x2C)
    pub const VK_SNAPSHOT: u32 = 0x2C;
    /// 'P' = 0x50
    pub const VK_P: u32 = 0x50;
    /// 'S' = 0x53
    pub const VK_S: u32 = 0x53;
    /// 'D' = 0x44
    pub const VK_D: u32 = 0x44;
    /// 'F' = 0x46
    pub const VK_F: u32 = 0x46;
    /// F7 = 0x76（默认开始录制视频）
    pub const VK_F7: u32 = 0x76;
    /// F8 = 0x77（默认停止录制视频）
    pub const VK_F8: u32 = 0x77;
    /// F9 = 0x78（默认开始录制）
    pub const VK_F9: u32 = 0x78;
    /// F10 = 0x79（默认停止录制）
    pub const VK_F10: u32 = 0x79;
}

fn default_region_hotkey() -> Hotkey {
    // Ctrl+Shift+S（Win+Shift+S 与系统截图冲突不可用）
    Hotkey {
        modifiers: Hotkey::MOD_CONTROL | Hotkey::MOD_SHIFT,
        vk: Hotkey::VK_S,
    }
}
fn default_fullscreen_hotkey() -> Hotkey {
    // Ctrl+Shift+F（F = Fullscreen）
    Hotkey {
        modifiers: Hotkey::MOD_CONTROL | Hotkey::MOD_SHIFT,
        vk: Hotkey::VK_F,
    }
}
fn default_silent_hotkey() -> Hotkey {
    // Ctrl+Shift+D（D = Direct 直接保存）
    Hotkey {
        modifiers: Hotkey::MOD_CONTROL | Hotkey::MOD_SHIFT,
        vk: Hotkey::VK_D,
    }
}
fn default_record_start_hotkey() -> Option<Hotkey> {
    // Alt+F9（游戏模式直启）
    Some(Hotkey {
        modifiers: Hotkey::MOD_ALT,
        vk: Hotkey::VK_F9,
    })
}
fn default_record_stop_hotkey() -> Option<Hotkey> {
    // Alt+F10（沉浸式录制期间临时注册）
    Some(Hotkey {
        modifiers: Hotkey::MOD_ALT,
        vk: Hotkey::VK_F10,
    })
}
fn default_video_start_hotkey() -> Option<Hotkey> {
    // Alt+F7（游戏模式视频直启；与动图 Alt+F9 区分）
    Some(Hotkey {
        modifiers: Hotkey::MOD_ALT,
        vk: Hotkey::VK_F7,
    })
}
fn default_video_stop_hotkey() -> Option<Hotkey> {
    // Alt+F8（视频录制期间临时注册；视频无时长上限，此热键必配）
    Some(Hotkey {
        modifiers: Hotkey::MOD_ALT,
        vk: Hotkey::VK_F8,
    })
}
fn default_copy_to_clipboard() -> bool {
    true
}
fn default_auto_save() -> bool {
    true
}
fn default_show_preview() -> bool {
    true
}
fn default_show_toolbar() -> bool {
    true
}
fn default_enable_pin() -> bool {
    true
}
fn default_annotation_capture_mode() -> String {
    "region".to_string()
}
fn default_silent_monitor() -> String {
    "primary".to_string()
}
fn default_filename_template() -> String {
    "jietu_{date}_{time}".to_string()
}
fn default_output_depth() -> u32 {
    16
}

fn default_minimize_to_tray() -> bool {
    true
}
fn default_hide_main_on_capture() -> bool {
    true
}
fn default_theme_color() -> String {
    "taro".to_string()
}

impl Default for MemoryCleanConfig {
    fn default() -> Self {
        Self {
            // 231 = memory::MASK_DEFAULT（0x01|0x02|0x04|0x20|0x40|0x80，对照 REDUCT_MASK_DEFAULT）
            mask: 231,
            auto_enable: false,
            threshold_percent: 90,
            interval_enable: false,
            interval_minutes: 30,
            cooldown_seconds: 30,
            notify_enable: true,
            log_results: false,
            danger_warning: 70,
            danger_critical: 90,
            // 默认清理热键 Ctrl+Shift+M（'M' = 0x4D，完整复刻 SOURCE_HOTKEY）
            hotkey: Some(Hotkey {
                modifiers: Hotkey::MOD_CONTROL | Hotkey::MOD_SHIFT,
                vk: 0x4D,
            }),
            stats: CleanStats::default(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            save_dir: None,
            output_format: OutputFormat::PngSdr,
            output_quality: QualityLevel::Lossless,
            output_depth: default_output_depth(),
            tonemap_settings: ToneMapConfig::default(),
            last_hdr_sdr_white: None,
            annotation_capture_mode: default_annotation_capture_mode(),
            region_hotkey: default_region_hotkey(),
            fullscreen_hotkey: default_fullscreen_hotkey(),
            silent_hotkey: default_silent_hotkey(),
            record_start_hotkey: default_record_start_hotkey(),
            record_stop_hotkey: default_record_stop_hotkey(),
            video_start_hotkey: default_video_start_hotkey(),
            video_stop_hotkey: default_video_stop_hotkey(),
            copy_to_clipboard: default_copy_to_clipboard(),
            auto_save: default_auto_save(),
            show_preview: default_show_preview(),
            show_toolbar: default_show_toolbar(),
            enable_pin: default_enable_pin(),
            sound_enabled: false,
            ocr_language: String::new(),
            silent_monitor: default_silent_monitor(),
            capture_include_tool: false,
            hide_main_on_capture: default_hide_main_on_capture(),
            filename_template: default_filename_template(),
            minimize_to_tray: default_minimize_to_tray(),
            autostart: false,
            theme_color: default_theme_color(),
            theme_schemes: HashMap::new(),
            memory_clean: MemoryCleanConfig::default(),
            file_assoc: false,
            main_window: None,
            recording: RecordingConfig::default(),
            video: VideoConfig::default(),
            viewer: ViewerConfig::default(),
            upscale_after_capture: false,
        }
    }
}

impl Config {
    /// 统一运行时色调映射参数入口（v2 文档 §6.5）
    ///
    /// 所有捕获/解码路径统一调用：激活预设 → 应用 overrides → 绑定输入 SDR 白。
    /// `input_sdr_white_nits`：捕获路径传显示器实际读数；文件解码路径传 80（标称基准）。
    pub fn active_tonemap_params(&self, input_sdr_white_nits: f32) -> HdrToSdrParams {
        let mut preset = self.tonemap_settings.resolve_active_preset();
        let ov = &self.tonemap_settings.overrides;
        if let Some(v) = ov.source_peak_nits {
            preset.source_peak_nits = v;
        }
        if let Some(v) = ov.output_diffuse_white {
            preset.output_diffuse_white = v;
        }
        if let Some(v) = ov.exposure_ev {
            preset.exposure_ev = v;
        }
        if let Some(v) = ov.saturation {
            preset.saturation = v;
        }
        if let Some(v) = ov.contrast {
            preset.contrast = v;
        }
        if let Some(v) = ov.gamut_strength {
            preset.gamut_strength = v;
        }
        if let Some(v) = ov.dither {
            preset.dither = v;
        }
        if let Some(v) = ov.adaptive_peak {
            preset.adaptive_peak = v;
        }
        HdrToSdrParams {
            operator: preset.operator,
            input_sdr_white_nits,
            source_peak_nits: preset.source_peak_nits,
            output_diffuse_white: preset.output_diffuse_white,
            exposure_ev: preset.exposure_ev,
            saturation: preset.saturation,
            contrast: preset.contrast,
            gamut_strength: preset.gamut_strength,
            dither: preset.dither,
            adaptive_peak: preset.adaptive_peak,
            knee_start: preset.knee_start,
        }
    }

    /// 获取实际保存目录（默认 Pictures/jietu-hdr）
    pub fn resolved_save_dir(&self) -> PathBuf {
        if let Some(d) = &self.save_dir {
            d.clone()
        } else {
            let pictures = dirs::picture_dir().unwrap_or_else(|| PathBuf::from("."));
            pictures.join("jietu-hdr")
        }
    }

    /// 配置文件路径：%APPDATA%\jietu-hdr\config.toml
    pub fn config_path() -> PathBuf {
        let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        base.join("jietu-hdr").join("config.toml")
    }

    /// 加载配置（文件不存在则返回默认值）
    pub fn load() -> Config {
        let path = Self::config_path();
        match std::fs::read_to_string(&path) {
            Ok(content) => match toml::from_str(&content) {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("配置文件解析失败，使用默认值: {}", e);
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        }
    }

    /// 保存配置
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        // V18.3：原子写（临时文件 + rename）——直接覆写在进程退出竞态/并发保存下
        // 会产生撕裂文件（实测：config.toml 半截写入 → 解析失败 → 默认值
        // ENHANCE-ALL 生效 + FX 参数全 0 → 播放器全屏纯灰）
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, &content)?;
        std::fs::rename(&tmp, &path)?;
        log::info!("配置已保存: {}", path.display());
        Ok(())
    }

    /// 生成带时间戳的文件名（不含扩展名）
    pub fn generate_filename(&self) -> String {
        let now = chrono::Local::now();
        let date = now.format("%Y%m%d").to_string();
        let time = now.format("%H%M%S").to_string();
        self.filename_template
            .replace("{date}", &date)
            .replace("{time}", &time)
            .replace("{index}", "001")
    }
}
