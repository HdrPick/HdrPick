// 共享配置 store：App / MainView / SettingsView 共用同一份响应式配置
import { ref } from "vue";
import { invoke } from "@tauri-apps/api/core";

export interface Hotkey {
  modifiers: number; // bitfield: MOD_ALT=1, MOD_CONTROL=2, MOD_SHIFT=4, MOD_WIN=8
  vk: number; // Win32 VK_*
}

/** 录制动图配置（与 Rust RecordingConfig 一一对应） */
export interface RecordingConfig {
  /** 编码质量档：Lossless/VeryHigh/High/Balanced/Compact（映射 libjxl effort） */
  quality: string;
  /** 目标帧率上限（0 = auto：一律 30fps，与回放填充速率匹配） */
  fps: number;
  /** 环形缓冲字节保险丝（MB） */
  ring_bytes_mb: number;
  /** zstd 压缩水位（0..1） */
  zstd_watermark: number;
  /** 纯录制模式（录制期间零编码，停止后全核编码） */
  deferred_encode: boolean;
  /** 最长录制时长（秒，1..=30）：到时自动停止并保存（后端钳制，serde default=30） */
  max_seconds: number;
  /** 游戏模式静默延迟启动秒数（0..=10，0=立即；后端钳制，serde default=5）：
   *  按热键后先 OSD 倒计时再开始录制，给用户切回游戏的时间 */
  start_delay_seconds: number;
  /** 采集后端："auto"（游戏模式 WGC 优先 + DDA 兜底，桌面录制 DDA）|
   *  "wgc"（强制 WGC）| "dda"（强制 DDA）；后端解析容错：非 wgc/dda 一律 auto */
  capture_backend: string;
}

/** 视频录制/播放配置（与 Rust VideoConfig 一一对应，config.toml [video] 段） */
export interface VideoConfig {
  /** 编码器："h264" | "hevc"（HDR 强制 hevc） */
  codec: string;
  /** 硬编："auto"（nvenc→amf→qsv→libopenh264 探测链）| "nvenc" | "amf" | "qsv" | "cpu" */
  hw: string;
  /** 目标帧率（默认 60） */
  fps: number;
  /** 码率（Mbps，默认 20） */
  bitrate_mbps: number;
  /** 关键帧间隔（帧，默认 120 = 60fps 下 2s） */
  gop: number;
  /** 音频："off" | "system"（回环）| "both"（回环+麦克风混录） */
  audio: string;
  /** 录制模式："fullscreen"（全屏）| "follow"（追随鼠标） */
  mode: "fullscreen" | "follow";
  /** 全屏目标显示器索引（0 = 主显示器） */
  monitor_index: number;
  /** 追随鼠标：窗口宽（偶数像素） */
  follow_w: number;
  /** 追随鼠标：窗口高（偶数像素） */
  follow_h: number;
  /** 自动停止：时长上限（秒；0 = 无限。暂停期不计） */
  max_seconds: number;
  /** 自动停止：文件大小上限（MB；0 = 无限） */
  max_size_mb: number;
  /** 分卷阈值（GB；0 = 不分卷。单文件超阈值自动收尾 + 新文件 -partN 后缀） */
  split_size_gb: number;
  /** 静默自动停止：持续无声秒数触发（0 = 关闭） */
  silence_stop_seconds: number;
  /** 静默触发后的倒计时秒数（声音恢复自动取消） */
  silence_countdown_seconds: number;
  /** 录制叠加鼠标光标（DDA 帧不含指针） */
  mouse_cursor: boolean;
  /** 录制叠加点击效果（左=青圈 / 右=红圈） */
  mouse_click: boolean;
  /** 录制叠加高亮效果（点击点为中心半透明色块，1s 渐隐） */
  mouse_highlight: boolean;
  /** 高亮颜色 #RRGGBB */
  mouse_highlight_color: string;
  /** 高亮块边长（像素） */
  mouse_highlight_size: number;
  /** 水印文字（空=关；{ts}=时间戳；换行=多条） */
  watermark_text: string;
  /** 水印图片路径（空=关；PNG） */
  watermark_image: string;
  /** 水印九宫格："tl".."br" */
  watermark_pos: string;
  /** 水印不透明度 1..=100 */
  watermark_opacity: number;
  /** 水印边距（像素） */
  watermark_margin: number;
  /** 水印字号（像素） */
  watermark_font_size: number;
  /** 录制完成动作："none" | "new" | "exit" | "shutdown" */
  complete_action: string;
  /** 关机倒计时秒数（shutdown 动作时；期间可取消） */
  shutdown_countdown_seconds: number;
  /** 计划录制开关（后端 30s 轮询；到点自动开始） */
  schedule_enabled: boolean;
  /** 计划重复："once" | "daily" | "weekly" */
  schedule_repeat: string;
  /** 每周触发日（weekly）：逗号分隔 1-7（周一=1） */
  schedule_weekdays: string;
  /** 计划开始时间 "HH:MM" */
  schedule_start: string;
  /** 计划结束时间 "HH:MM"（到点自动停止） */
  schedule_end: string;
  /** 上次触发日 "YYYY-MM-DD"（防当日重复；后端维护） */
  schedule_last_fired: string;
  /** 摄像头画中画设备（symbolic link；空=关） */
  camera_device: string;
  /** 摄像头 PiP 显示宽（像素；0=自适应=输出宽 1/4） */
  camera_width: number;
  /** 摄像头九宫格："tl".."br" */
  camera_pos: string;
  /** 摄像头边距（像素） */
  camera_margin: number;
  /** 摄像头水平翻转（前置镜像习惯） */
  camera_flip_h: boolean;
  /** 色度键抠像开关 */
  camera_chroma_key: boolean;
  /** 键控颜色 #RRGGBB */
  camera_key_color: string;
  /** 键控相似度 0..=100 */
  camera_similarity: number;
  /** 播放位置记忆 */
  resume_playback: boolean;
}

/** 动图查看器配置（与 Rust ViewerConfig 一一对应） */
export interface ViewerConfig {
  /** 动图播放解码后端：native 兼容优先 / hybrid GPU 混合实验版（异常自动回退） */
  anim_backend: "native" | "hybrid";
  /** 播放模式：gpu 常驻显存池（短动图循环零解码）/ ring 内存环形缓冲 */
  anim_playback: "gpu" | "ring";
  /** GPU 池显存预算 MB；0 = auto（min(4096, 专用显存50%−512MB)） */
  anim_vram_mb: number;
  /** 缩略图预热：启动 2s 后后台全库解码填充 LRU（默认关——机械盘上抢 IO；关闭后首次浏览按需生成） */
  thumb_prewarm: boolean;
}

export interface ThemeSchemeData {
  primary: string; // #RRGGBB
}

// 内存清理统计（对照 memreduct StatisticLastReduct，扩展累计两项）
export interface MemoryCleanStats {
  last_clean_ts: number; // 上次清理时间戳（秒级，冷却判定 + UI「最近清理」显示）
  last_freed_bytes: number; // 上次清理释放字节数
  total_clean_count: number; // 累计清理次数（jietu 增强）
  total_freed_bytes: number; // 累计释放字节数（jietu 增强）
}

// 内存清理配置（键名与 memreduct ini 语义对齐，见设计文档 4.2.4）
export interface MemoryCleanConfig {
  mask: number; // 清理区域位掩码：默认 231 = 0x01|0x02|0x04|0x20|0x40|0x80（不含危险项 0x08|0x10）
  auto_enable: boolean; // 阈值触发自动清理（AutoreductEnable）
  threshold_percent: number; // 阈值百分比（AutoreductValue）
  interval_enable: boolean; // 间隔触发自动清理（AutoreductIntervalEnable）
  interval_minutes: number; // 间隔分钟（AutoreductIntervalValue，1-1440）
  cooldown_seconds: number; // 两次清理最小冷却间隔（秒，jietu 增强开放为配置）
  notify_enable: boolean; // 清理完成 toast 通知（BalloonCleanResults）
  log_results: boolean; // 清理结果写入日志（LogCleanResults）
  danger_warning: number; // 警告档百分比（≥ 该值显示奶茶橘）
  danger_critical: number; // 危险档百分比（≥ 该值显示豆沙红）
  hotkey: Hotkey | null; // 清理内存热键（完整复刻 SOURCE_HOTKEY；null = 未配置不注册）
  stats: MemoryCleanStats; // 清理统计（持久化）
}

// HDR→SDR 色调映射预设（与 Rust ToneMapPreset 一一对应，v2 文档 §6.1）
export interface ToneMapPreset {
  id: string; // builtin:soft / custom:<时间戳>
  name: string;
  desc: string;
  operator: "Bt2390" | "SmoothKnee" | "Reinhard" | "Aces";
  source_peak_nits: number;
  output_diffuse_white: number;
  exposure_ev: number;
  saturation: number;
  contrast: number;
  gamut_strength: number;
  dither: boolean;
  adaptive_peak: boolean;
  knee_start: number;
  builtin: boolean;
}

// 高级面板临时调整（None/null = 跟随预设）
export interface ToneMapOverrides {
  source_peak_nits: number | null;
  output_diffuse_white: number | null;
  exposure_ev: number | null;
  saturation: number | null;
  contrast: number | null;
  gamut_strength: number | null;
  dither: boolean | null;
  adaptive_peak: boolean | null;
}

// 色调映射配置段（config.toml [tonemap_settings]）
export interface ToneMapSettings {
  active_preset: string;
  overrides: ToneMapOverrides;
  custom_presets: ToneMapPreset[];
}

/** 内置预设（与 Rust ToneMapConfig::builtin_presets 保持一致） */
export const TONEMAP_BUILTIN_PRESETS: ToneMapPreset[] = [
  { id: "builtin:soft", name: "柔和日常", desc: "温柔不过曝，日常截图首选", operator: "Bt2390", source_peak_nits: 1000, output_diffuse_white: 0.75, exposure_ev: 0, saturation: 1.02, contrast: 1.0, gamut_strength: 0.8, dither: true, adaptive_peak: false, knee_start: 0.9, builtin: true },
  { id: "builtin:natural", name: "真实还原", desc: "忠实记录高光层次", operator: "Bt2390", source_peak_nits: 1000, output_diffuse_white: 0.8, exposure_ev: 0, saturation: 1.0, contrast: 1.0, gamut_strength: 0.8, dither: true, adaptive_peak: false, knee_start: 0.9, builtin: true },
  { id: "builtin:highlight", name: "高光保护", desc: "亮部细节优先，高光柔和不炸", operator: "Reinhard", source_peak_nits: 1000, output_diffuse_white: 0.75, exposure_ev: 0, saturation: 1.02, contrast: 0.98, gamut_strength: 0.8, dither: true, adaptive_peak: false, knee_start: 0.9, builtin: true },
  { id: "builtin:vivid", name: "游戏鲜艳", desc: "色彩饱满，画面通透明快", operator: "Bt2390", source_peak_nits: 1000, output_diffuse_white: 0.78, exposure_ev: 0.05, saturation: 1.15, contrast: 1.05, gamut_strength: 0.8, dither: true, adaptive_peak: false, knee_start: 0.9, builtin: true },
  { id: "builtin:cinema", name: "影视灰阶", desc: "电影感灰阶，柔和过渡", operator: "Aces", source_peak_nits: 1000, output_diffuse_white: 0.72, exposure_ev: 0, saturation: 0.92, contrast: 1.08, gamut_strength: 0.8, dither: true, adaptive_peak: false, knee_start: 0.9, builtin: true },
  { id: "builtin:auto", name: "智能自适应", desc: "自动分析画面亮度，免调参", operator: "Bt2390", source_peak_nits: 1000, output_diffuse_white: 0.75, exposure_ev: 0, saturation: 1.02, contrast: 1.0, gamut_strength: 0.8, dither: true, adaptive_peak: true, knee_start: 0.9, builtin: true },
];

// 与 Rust Config 一一对应
export interface Config {
  output_format: string;
  /** 编码质量档（JXL/JXR）：Lossless/VeryHigh/High/Balanced/Compact */
  output_quality: string;
  /** HDR 输出位深（JXL：12=体积优先 / 16=精度优先；PNG 规范限 16bit） */
  output_depth: number;
  tonemap_settings: ToneMapSettings;
  copy_to_clipboard: boolean;
  auto_save: boolean;
  show_preview: boolean;
  show_toolbar: boolean;
  enable_pin: boolean;
  /** 「截图标识」截图方式：region=拖选区域 / fullscreen=整屏快照直通标注 */
  annotation_capture_mode: string;
  sound_enabled: boolean;
  ocr_language: string;
  silent_monitor: string;
  capture_include_tool: boolean;
  hide_main_on_capture: boolean;
  filename_template: string;
  save_dir: string | null;
  region_hotkey: Hotkey;
  fullscreen_hotkey: Hotkey;
  silent_hotkey: Hotkey;
  /** 开始录制热键（游戏模式直启；null = 禁用） */
  record_start_hotkey: Hotkey | null;
  /** 停止录制热键（录制期间临时注册；null = 仅面板/OSD 停止） */
  record_stop_hotkey: Hotkey | null;
  /** 开始视频录制热键（游戏模式直启 MKV + OSD；null = 禁用） */
  video_start_hotkey: Hotkey | null;
  /** 停止视频录制热键（视频录制期间临时注册；null = 热键直启不可用） */
  video_stop_hotkey: Hotkey | null;
  /** 录制动图配置 */
  recording: RecordingConfig;
  /** 视频录制/播放配置（videorec：MP4/MKV 长录制 + 播放器） */
  video: VideoConfig;
  /** 动图查看器配置（解码后端 / 播放模式 / 显存预算） */
  viewer: ViewerConfig;
  minimize_to_tray: boolean;
  autostart: boolean;
  theme_color: string;
  theme_schemes: Record<string, ThemeSchemeData>;
  memory_clean: MemoryCleanConfig;
  /** 截图保存后自动 AI 2x 增强（后台生成增强副本，不替换原图） */
  upscale_after_capture: boolean;
}

function defaultConfig(): Config {
  return {
    output_format: "PngSdr",
    output_quality: "Lossless",
    output_depth: 16,
    tonemap_settings: {
      active_preset: "builtin:soft",
      overrides: {
        source_peak_nits: null,
        output_diffuse_white: null,
        exposure_ev: null,
        saturation: null,
        contrast: null,
        gamut_strength: null,
        dither: null,
        adaptive_peak: null,
      },
      custom_presets: [],
    },
    copy_to_clipboard: true,
    auto_save: true,
    show_preview: true,
    show_toolbar: true,
    enable_pin: true,
    annotation_capture_mode: "region",
    sound_enabled: false,
    ocr_language: "",
    silent_monitor: "primary",
    capture_include_tool: false,
    hide_main_on_capture: true,
    filename_template: "jietu_{date}_{time}",
    save_dir: null,
    region_hotkey: { modifiers: 2 | 4, vk: 0x53 }, // Ctrl+Shift+S
    fullscreen_hotkey: { modifiers: 2 | 4, vk: 0x46 }, // Ctrl+Shift+F
    silent_hotkey: { modifiers: 2 | 4, vk: 0x44 }, // Ctrl+Shift+D
    record_start_hotkey: { modifiers: 1, vk: 0x78 }, // Alt+F9
    record_stop_hotkey: { modifiers: 1, vk: 0x79 }, // Alt+F10
    video_start_hotkey: { modifiers: 1, vk: 0x76 }, // Alt+F7
    video_stop_hotkey: { modifiers: 1, vk: 0x77 }, // Alt+F8
    recording: {
      quality: "High",
      fps: 0, // auto：一律 30fps
      ring_bytes_mb: 40960,
      zstd_watermark: 0.4,
      deferred_encode: false,
      max_seconds: 30, // 最长录制时长（秒），默认 30
      start_delay_seconds: 5, // 游戏模式延迟启动（秒），默认 5，0=立即
      capture_backend: "auto", // 采集后端：auto（游戏模式 WGC 优先 + DDA 兜底）
    },
    video: {
      codec: "h264",
      hw: "auto", // nvenc→amf→qsv→libopenh264 探测链
      fps: 60,
      bitrate_mbps: 20,
      gop: 120, // 60fps 下 2s
      audio: "system", // 系统声音回环
      mode: "fullscreen",
      monitor_index: 0, // 0 = 主显示器
      follow_w: 1280,
      follow_h: 720,
      max_seconds: 0, // 0 = 无限
      max_size_mb: 0, // 0 = 无限
      split_size_gb: 0, // 0 = 不分卷
      silence_stop_seconds: 0, // 0 = 关闭
      silence_countdown_seconds: 10,
      mouse_cursor: true, // DDA 帧不含指针——默认叠加
      mouse_click: false,
      mouse_highlight: false,
      mouse_highlight_color: "#FFD400",
      mouse_highlight_size: 120,
      watermark_text: "", // 空=关
      watermark_image: "", // 空=关
      watermark_pos: "br",
      watermark_opacity: 100,
      watermark_margin: 16,
      watermark_font_size: 24,
      complete_action: "none",
      shutdown_countdown_seconds: 30,
      schedule_enabled: false,
      schedule_repeat: "daily",
      schedule_weekdays: "1,2,3,4,5",
      schedule_start: "09:00",
      schedule_end: "10:00",
      schedule_last_fired: "",
      camera_device: "", // 空=关
      camera_width: 320,
      camera_pos: "br",
      camera_margin: 16,
      camera_flip_h: false,
      camera_chroma_key: false,
      camera_key_color: "#00FF00",
      camera_similarity: 60,
      resume_playback: true,
    },
    viewer: {
      anim_backend: "native",
      anim_playback: "gpu",
      anim_vram_mb: 0, // auto
      thumb_prewarm: false, // 默认不预热（机械盘 IO 洪峰；首次浏览按需生成）
    },
    minimize_to_tray: true,
    autostart: false,
    theme_color: "taro",
    theme_schemes: {},
    upscale_after_capture: false,
    // 内存清理默认值（对照设计文档 4.2.4：mask=231 安全组合，阈值 90%，间隔 30 分钟，冷却 30 秒）
    memory_clean: {
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
      hotkey: { modifiers: 2 | 4, vk: 0x4D }, // 默认 Ctrl+Shift+M（SOURCE_HOTKEY）
      stats: {
        last_clean_ts: 0,
        last_freed_bytes: 0,
        total_clean_count: 0,
        total_freed_bytes: 0,
      },
    },
  };
}

export const config = ref<Config>(defaultConfig());
export const configLoaded = ref(false);

/** 加载后端配置（含 autostart 实际状态） */
export async function loadConfig() {
  try {
    const c = await invoke<Config>("get_config");
    const def = defaultConfig();
    config.value = {
      ...def,
      ...c,
      // tonemap_settings 深合并：旧配置缺失该段时兜底默认值
      tonemap_settings: {
        ...def.tonemap_settings,
        ...(c.tonemap_settings ?? {}),
        overrides: {
          ...def.tonemap_settings.overrides,
          ...(c.tonemap_settings?.overrides ?? {}),
        },
        custom_presets: c.tonemap_settings?.custom_presets ?? [],
      },
      // memory_clean 深合并：后端未就绪或部分字段缺失时兜底默认值
      memory_clean: {
        ...def.memory_clean,
        ...(c.memory_clean ?? {}),
        stats: { ...def.memory_clean.stats, ...(c.memory_clean?.stats ?? {}) },
      },
      // viewer 深合并：旧 config.toml 缺失该段时兜底默认值
      viewer: { ...def.viewer, ...(c.viewer ?? {}) },
      // recording 深合并：旧 config.toml 缺失 max_seconds 等字段时兜底默认值
      recording: { ...def.recording, ...(c.recording ?? {}) },
      // video 深合并：旧 config.toml 缺失 [video] 段时兜底默认值
      video: { ...def.video, ...(c.video ?? {}) },
    };
    try {
      const enabled = await invoke<boolean>("is_autostart_enabled");
      config.value.autostart = enabled;
    } catch {
      // ignore
    }
    configLoaded.value = true;
  } catch (e) {
    console.error("加载配置失败", e);
  }
}

/** 保存配置 + 同步热键注册 */
export async function saveConfig(): Promise<void> {
  await invoke("save_config", { config: config.value });
  // 同步开机自启（独立容错）
  try {
    if (config.value.autostart) {
      await invoke("enable_autostart");
    } else {
      await invoke("disable_autostart");
    }
  } catch (e) {
    console.warn("开机自启设置未生效:", e);
    throw new Error(`开机自启设置未生效: ${e}`);
  }
}
