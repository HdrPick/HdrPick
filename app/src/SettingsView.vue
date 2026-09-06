<script setup lang="ts">
// 设置中心：侧导航 + 分组页 + 自动保存（800ms 防抖）
// 所有配置项集中于此，主面板保持极简
import { ref, computed, watch, h, onMounted, onUnmounted } from "vue";
import {
  NSelect,
  NSwitch,
  NInput,
  NInputNumber,
  NButton,
  NIcon,
  NCheckbox,
  NSlider,
  NTooltip,
  NColorPicker,
  useMessage,
  useDialog,
} from "naive-ui";
import {
  Palette,
  Camera,
  Bolt,
  Contrast,
  Language,
  DeviceMobile,
  Photo,
  Folder,
  DeviceFloppy,
  Trash,
  Logout,
  Check,
  ArrowsMaximize,
  Settings,
  FileExport,
  FileImport,
  Database,
  Cpu,
  Plug,
  Wand,
  Brush,
  ShieldCheck,
  AlertTriangle,
  MessageCircle,
  Adjustments,
  Video,
  Movie,
} from "@vicons/tabler";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
import type { ThemeMode } from "./useTheme";
import TitleBar from "./TitleBar.vue";
import {
  config,
  saveConfig,
  loadConfig,
  configLoaded,
  TONEMAP_BUILTIN_PRESETS,
} from "./configStore";
import type { ToneMapPreset, ToneMapOverrides } from "./configStore";
import {
  THEME_PRESETS,
  primaryColor,
  setThemeColor,
  saveScheme,
  removeScheme,
} from "./themeColor";

const presets = THEME_PRESETS;

const props = defineProps<{
  mode: ThemeMode;
  isDark: boolean;
  setMode: (m: ThemeMode) => void;
  /** 打开时定位到的分组 */
  initialSection?: string;
}>();

const emit = defineEmits<{
  (e: "back"): void;
}>();

const message = useMessage();

// ====== 侧导航 ======
const sections = [
  { id: "appearance", label: "外观", icon: Palette },
  { id: "behavior", label: "截图行为", icon: Camera },
  { id: "output", label: "输出", icon: Photo },
  { id: "quality", label: "画质", icon: Adjustments },
  { id: "hotkey", label: "热键", icon: Bolt },
  { id: "record", label: "录制动图", icon: Video },
  { id: "video", label: "录制视频", icon: Movie },
  { id: "display", label: "显示器", icon: Contrast },
  { id: "ocr", label: "OCR", icon: Language },
  { id: "background", label: "后台运行", icon: DeviceMobile },
  { id: "system", label: "系统集成", icon: Plug },
  { id: "config", label: "配置管理", icon: Database },
  // 内存清理（@vicons/tabler 无 Memory 图标，用 Cpu）
  { id: "memory", label: "内存清理", icon: Cpu },
];
const activeSection = ref(
  sections.some((s) => s.id === props.initialSection)
    ? (props.initialSection as string)
    : "appearance",
);
// 切到显示器分区时自动加载面板色域（只读信息，按需拉取）
watch(activeSection, (s) => {
  if (s === "display") loadGamut();
  if (s === "video") {
    loadVideoStatus();
    loadVideoMonitors();
    loadVideoCameras();
  }
});

// ====== 录制视频（videorec：MKV 长录制 + 播放器） ======
const videoCodecOptions = [
  { label: "H.264（兼容性最好）", value: "h264" },
  { label: "HEVC / H.265（同画质更小）", value: "hevc" },
];
const videoHwOptions = [
  { label: "自动（N卡→A卡→Intel→软编）", value: "auto" },
  { label: "NVIDIA NVENC", value: "nvenc" },
  { label: "AMD AMF", value: "amf" },
  { label: "Intel QSV", value: "qsv" },
  { label: "CPU 软编（libopenh264）", value: "cpu" },
];
const videoFpsOptions = [
  { label: "30 fps", value: 30 },
  { label: "60 fps", value: 60 },
  { label: "120 fps", value: 120 },
];
const videoAudioOptions = [
  { label: "关闭", value: "off" },
  { label: "系统声音（回环）", value: "system" },
  { label: "系统 + 麦克风", value: "both" },
];
// 自动停止阈值（0 = 不限）：n-input-number 需要可写 computed 桥接 config 对象
const autoStopSeconds = computed({
  get: () => config.value.video.max_seconds ?? 0,
  set: (v: number | null) => {
    config.value.video.max_seconds = Math.max(0, Math.round(v ?? 0));
  },
});
const autoStopMB = computed({
  get: () => config.value.video.max_size_mb ?? 0,
  set: (v: number | null) => {
    config.value.video.max_size_mb = Math.max(0, Math.round(v ?? 0));
  },
});
const splitSizeGB = computed({
  get: () => config.value.video.split_size_gb ?? 0,
  set: (v: number | null) => {
    config.value.video.split_size_gb = Math.max(0, Math.round(v ?? 0));
  },
});
const silenceStopSeconds = computed({
  get: () => config.value.video.silence_stop_seconds ?? 0,
  set: (v: number | null) => {
    config.value.video.silence_stop_seconds = Math.max(0, Math.round(v ?? 0));
  },
});
const silenceCountdownSeconds = computed({
  get: () => config.value.video.silence_countdown_seconds ?? 10,
  set: (v: number | null) => {
    config.value.video.silence_countdown_seconds = Math.min(120, Math.max(1, Math.round(v ?? 10)));
  },
});
const videoBitrateOptions = [
  { label: "8 Mbps（日常）", value: 8 },
  { label: "12 Mbps", value: 12 },
  { label: "16 Mbps", value: 16 },
  { label: "20 Mbps（默认）", value: 20 },
  { label: "30 Mbps（高动态）", value: 30 },
  { label: "50 Mbps（高码率）", value: 50 },
];
/** 视频组件（FFmpeg DLL）探测状态：null=未探测 */
const videoComponent = ref<{ available: boolean; version?: string; message?: string } | null>(null);

async function loadVideoStatus() {
  try {
    videoComponent.value = await invoke<{ available: boolean; version?: string; message?: string }>(
      "video_component_status",
    );
  } catch {
    videoComponent.value = { available: false, message: "探测失败" };
  }
}

// ====== 视频录制：显示器列表（monitor_index 下拉数据源） ======
const videoMonitorOptions = ref<{ label: string; value: number }[]>([
  { label: "主显示器", value: 0 },
]);

async function loadVideoMonitors() {
  try {
    const r = await invoke<{ monitors: { index: number; label: string }[] }>(
      "video_list_monitors",
    );
    if (r.monitors?.length) {
      videoMonitorOptions.value = r.monitors.map((m) => ({ label: m.label, value: m.index }));
    }
  } catch {
    // 枚举失败保持主显示器兜底
  }
}

// ====== 视频录制：摄像头列表（camera_device 下拉数据源） ======
const videoCameraOptions = ref<{ label: string; value: string }[]>([
  { label: "关闭（不叠加摄像头）", value: "" },
]);

async function loadVideoCameras() {
  try {
    const r = await invoke<{ cameras: { id: string; label: string }[] }>(
      "video_list_cameras",
    );
    videoCameraOptions.value = [
      { label: "关闭（不叠加摄像头）", value: "" },
      ...(r.cameras ?? []).map((c) => ({ label: c.label, value: c.id })),
    ];
  } catch {
    // 枚举失败保持关闭兜底
  }
}

const videoModeOptions = [
  { label: "全屏（显示器整屏）", value: "fullscreen" },
  { label: "追随鼠标（窗口跟随光标）", value: "follow" },
];

// 录制完成动作
const completeActionOptions = [
  { label: "无动作", value: "none" },
  { label: "录新视频（自动重新开始）", value: "new" },
  { label: "退出程序", value: "exit" },
  { label: "关机（倒计时内可取消）", value: "shutdown" },
];

// 计划录制
const scheduleRepeatOptions = [
  { label: "一次（触发后自动关闭）", value: "once" },
  { label: "每天", value: "daily" },
  { label: "每周（按星期）", value: "weekly" },
];
const weekdayOptions = [
  { label: "周一", value: 1 },
  { label: "周二", value: 2 },
  { label: "周三", value: 3 },
  { label: "周四", value: 4 },
  { label: "周五", value: 5 },
  { label: "周六", value: 6 },
  { label: "周日", value: 7 },
];
// schedule_weekdays 逗号字符串 ↔ 多选数组
const scheduleWeekdays = computed({
  get: () =>
    (config.value.video.schedule_weekdays || "")
      .split(",")
      .map((s) => parseInt(s.trim(), 10))
      .filter((n) => n >= 1 && n <= 7),
  set: (v: number[] | null) => {
    config.value.video.schedule_weekdays = (v ?? [])
      .filter((n) => n >= 1 && n <= 7)
      .sort((a, b) => a - b)
      .join(",");
  },
});

// 水印九宫格
const watermarkPosOptions = [
  { label: "左上", value: "tl" },
  { label: "上中", value: "tc" },
  { label: "右上", value: "tr" },
  { label: "左中", value: "ml" },
  { label: "居中", value: "mc" },
  { label: "右中", value: "mr" },
  { label: "左下", value: "bl" },
  { label: "下中", value: "bc" },
  { label: "右下", value: "br" },
];

// 追随窗口尺寸（偶数化 + 钳制 [64, 4096]）
const followW = computed({
  get: () => config.value.video.follow_w ?? 1280,
  set: (v: number | null) => {
    config.value.video.follow_w = Math.min(4096, Math.max(64, Math.round(v ?? 1280))) & ~1;
  },
});
const followH = computed({
  get: () => config.value.video.follow_h ?? 720,
  set: (v: number | null) => {
    config.value.video.follow_h = Math.min(4096, Math.max(64, Math.round(v ?? 720))) & ~1;
  },
});

// ====== 自动保存（800ms 防抖） ======
const saveState = ref<"idle" | "saving" | "saved" | "error">("idle");

// ====== 系统集成：文件关联 + 右键菜单（assoc.rs，注册表操作不走 configStore） ======
const fileAssoc = ref(false);
const fileAssocBusy = ref(false);

/** 逐扩展名关联明细（UserChoice 只读检测） */
interface AssocExtStatus {
  ext: string;
  ours: boolean;
  owner: string | null;
  progId: string | null;
}
const assocDetail = ref<AssocExtStatus[]>([]);

async function loadFileAssoc() {
  try {
    fileAssoc.value = await invoke<boolean>("is_file_assoc");
  } catch {
    fileAssoc.value = false;
  }
  loadAssocDetail();
}

async function loadAssocDetail() {
  try {
    assocDetail.value = await invoke<AssocExtStatus[]>("get_assoc_detail");
  } catch {
    assocDetail.value = [];
  }
}

/** 汇总回显：已设默认格式数 X / N（由明细派生） */
const defaultCount = computed(() =>
  assocDetail.value.length
    ? {
        set: assocDetail.value.filter((a) => a.ours).length,
        total: assocDetail.value.length,
      }
    : null,
);
const defaultBusy = ref(false);

/** 设为默认看图应用：调起系统确认界面（模态），关闭后自动刷新明细 */
async function makeDefault() {
  if (defaultBusy.value) return;
  defaultBusy.value = true;
  try {
    // 关联页依赖 Capabilities 登记：未开启关联时先注册
    if (!fileAssoc.value) {
      await invoke("set_file_assoc", { enabled: true });
      fileAssoc.value = true;
    }
    await invoke("open_default_apps");
    await loadAssocDetail();
    if (defaultCount.value && defaultCount.value.set > 0) {
      message.success(
        `已设为默认看图应用（${defaultCount.value.set}/${defaultCount.value.total} 个格式）`,
        { duration: 2400 },
      );
    } else {
      message.info("在系统界面确认后，双击图片即用 jietu-hdr 打开", { duration: 2800 });
    }
  } catch (e) {
    message.error(`打开系统设置失败: ${e}`, { duration: 2400 });
  } finally {
    defaultBusy.value = false;
  }
}

async function onFileAssocChange(v: boolean) {
  if (fileAssocBusy.value) return;
  fileAssocBusy.value = true;
  const prev = fileAssoc.value;
  fileAssoc.value = v; // 乐观更新
  try {
    await invoke("set_file_assoc", { enabled: v });
    message.success(
      v ? "已注册文件关联与右键菜单" : "已取消文件关联与右键菜单",
      { duration: 2200 },
    );
  } catch (e) {
    fileAssoc.value = prev; // 失败回滚
    message.error(`操作失败: ${e}`, { duration: 2600 });
  } finally {
    fileAssocBusy.value = false;
  }
}
let saveTimer: number | null = null;
let saveStateTimer: number | null = null;

async function doSave() {
  saveState.value = "saving";
  try {
    await saveConfig();
    saveState.value = "saved";
  } catch (e) {
    saveState.value = "error";
    message.error("自动保存失败: " + e, { duration: 2200 });
  }
  if (saveStateTimer) window.clearTimeout(saveStateTimer);
  saveStateTimer = window.setTimeout(() => {
    saveState.value = "idle";
  }, 1600);
}

watch(
  config,
  () => {
    if (!configLoaded.value) return;
    if (saveTimer) window.clearTimeout(saveTimer);
    saveTimer = window.setTimeout(doSave, 800);
  },
  { deep: true },
);

onUnmounted(() => {
  if (saveTimer) window.clearTimeout(saveTimer);
  if (saveStateTimer) window.clearTimeout(saveStateTimer);
});

// ====== 外观：主题模式 ======
const themeOptions = [
  { label: "跟随系统", value: "follow" },
  { label: "浅色", value: "light" },
  { label: "深色", value: "dark" },
];
const themeMode = computed({
  get: () => props.mode,
  set: (v: ThemeMode) => props.setMode(v),
});

// ====== 外观：主题色（预设 / 自定义 / 方案） ======
const customHex = ref("");
watch(
  () => config.value.theme_color,
  (v) => {
    if (/^#[0-9A-Fa-f]{6}$/.test(v)) {
      // 与输入框内容一致（忽略大小写）时不回写，避免打断实时输入
      if (customHex.value.toUpperCase() !== v.toUpperCase()) {
        customHex.value = v.toUpperCase();
      }
    } else {
      customHex.value = "";
    }
  },
  { immediate: true },
);

function applyPreset(id: string) {
  setThemeColor(id);
  config.value.theme_color = id;
}

function applyCustomHex(hex: string) {
  const v = hex.trim().toUpperCase();
  if (!/^#[0-9A-F]{6}$/.test(v)) return;
  setThemeColor(v);
  config.value.theme_color = v;
}

function onColorInput(e: Event) {
  const v = (e.target as HTMLInputElement).value.toUpperCase();
  customHex.value = v;
  applyCustomHex(v);
}

// hex 输入实时预览：输入满合法 6 位即应用（无需回车）
function onHexInput(v: string) {
  customHex.value = v;
  applyCustomHex(v);
}

// ====== 主面板实时预览（配色跟随当前主题色即时变化） ======
const pvFormat = computed(() => {
  const m: Record<string, string> = {
    PngSdr: "SDR PNG",
    PngHdr: "HDR PNG",
    Exr: "EXR",
    Jxl: "HDR JXL",
    Jxr: "HDR JXR",
    Avif: "AVIF",
  };
  return m[config.value.output_format] ?? config.value.output_format;
});

const schemeName = ref("");
function saveCurrentScheme() {
  const name = schemeName.value.trim();
  if (!name) {
    message.warning("请先输入方案名称", { duration: 1800 });
    return;
  }
  if (config.value.theme_schemes[name]) {
    message.warning("已存在同名方案，将覆盖", { duration: 1800 });
  }
  const color = primaryColor.value;
  saveScheme(name, color);
  config.value.theme_schemes = {
    ...config.value.theme_schemes,
    [name]: { primary: color },
  };
  schemeName.value = "";
  message.success(`已保存方案「${name}」`, { duration: 1800 });
}

function applyScheme(primary: string) {
  setThemeColor(primary);
  config.value.theme_color = primary;
}

function deleteScheme(name: string) {
  removeScheme(name);
  const next = { ...config.value.theme_schemes };
  delete next[name];
  config.value.theme_schemes = next;
}

const schemeList = computed(() =>
  Object.entries(config.value.theme_schemes || {}).map(([name, s]) => ({
    name,
    primary: s.primary,
  })),
);

// ====== 输出：格式 ======
const formatOptions = [
  { label: "SDR PNG（通用兼容）", value: "PngSdr" },
  { label: "HDR PNG（BT.2020/PQ）", value: "PngHdr" },
  { label: "HDR JPEG XL（BT.2020/PQ）", value: "Jxl" },
  { label: "HDR JPEG XR（scRGB 浮点）", value: "Jxr" },
  { label: "OpenEXR（浮点 HDR）", value: "Exr" },
];

// ====== 截图行为：标注截图方式 ======
const annotationCaptureOptions = [
  { label: "区域选择（拖选范围后标注）", value: "region" },
  { label: "整屏快照（截取主屏直接标注）", value: "fullscreen" },
];

// ====== 输出：编码质量档（仅 JXL/JXR 受影响；PNG/EXR 恒定无损） ======
const qualityOptions = [
  { label: "无损（数学可逆，体积最大）", value: "Lossless" },
  { label: "极高（几乎无损，体积减半）", value: "VeryHigh" },
  { label: "高（肉眼难辨差异）", value: "High" },
  { label: "平衡（体积/质量折中）", value: "Balanced" },
  { label: "体积优先（体积最小）", value: "Compact" },
];
/** 当前格式是否支持质量档调节 */
const qualityApplicable = computed(() =>
  ["Jxl", "Jxr"].includes(config.value.output_format),
);

// HDR 位深（仅 JXL：libjxl 支持任意整数位深；PNG 规范限 8/16 不适用）
const depthApplicable = computed(() => config.value.output_format === "Jxl");
const depthOptions = [
  { label: "12bit（体积优先）", value: 12 },
  { label: "16bit（精度优先）", value: 16 },
];

// ====== 录制动图：质量档 / 帧率 ======
const recordQualityOptions = [
  { label: "无损（体积大，编码慢）", value: "Lossless" },
  { label: "极高", value: "VeryHigh" },
  { label: "高（推荐，速度/体积平衡）", value: "High" },
  { label: "均衡", value: "Balanced" },
  { label: "紧凑（体积优先）", value: "Compact" },
];
const recordFpsOptions = [
  { label: "自动（30fps · 与回放速率匹配）", value: 0 },
  { label: "60fps（回放可能卡顿）", value: 60 },
  { label: "30fps", value: 30 },
  { label: "24fps（影视感）", value: 24 },
];
// ====== 录制动图：录制时长上限（预设 6/8/10/15/20/30 秒 + 自定义整数 1..=30） ======
const RECORD_MAX_PRESETS = [6, 8, 10, 15, 20, 30];
const RECORD_MAX_CUSTOM = "custom";
const recordMaxOptions = [
  { label: "6 秒", value: "6" },
  { label: "8 秒", value: "8" },
  { label: "10 秒", value: "10" },
  { label: "15 秒", value: "15" },
  { label: "20 秒", value: "20" },
  { label: "30 秒（默认）", value: "30" },
  { label: "自定义", value: RECORD_MAX_CUSTOM },
];
/** 自定义档态：手动切入后保持；config 值非预设时也视为自定义（回填逻辑） */
const recordMaxCustomMode = ref(false);
const recordMaxIsCustom = computed(
  () =>
    recordMaxCustomMode.value ||
    !RECORD_MAX_PRESETS.includes(config.value.recording.max_seconds),
);
/** 档位选择：预设档直接写入秒数；「自定义」切入行内输入（当前值非法时落到 1） */
const recordMaxSelect = computed<string>({
  get: () =>
    recordMaxIsCustom.value
      ? RECORD_MAX_CUSTOM
      : String(config.value.recording.max_seconds),
  set: (v: string) => {
    if (v === RECORD_MAX_CUSTOM) {
      recordMaxCustomMode.value = true;
      const cur = config.value.recording.max_seconds;
      if (!Number.isInteger(cur) || cur < 1 || cur > 30) {
        config.value.recording.max_seconds = 1;
      }
    } else {
      const n = Number(v);
      if (Number.isInteger(n) && n >= 1 && n <= 30) {
        recordMaxCustomMode.value = false;
        config.value.recording.max_seconds = n;
      }
    }
  },
});
/** 保存前兜底钳制：非有限数 → 30；非整数 floor；越界钳到 1..=30 */
function clampRecordMax(v: number): number {
  if (!Number.isFinite(v)) return 30;
  return Math.min(30, Math.max(1, Math.floor(v)));
}
/** 自定义输入：写入 configStore 前兜底钳制（空输入不写入，失焦回填合法值） */
function onRecordMaxInput(v: number | null) {
  if (v == null) return;
  config.value.recording.max_seconds = clampRecordMax(v);
}
/** 失焦钳制：<1 → 1、>30 → 30、非整数 floor（二道保险） */
function onRecordMaxBlur() {
  config.value.recording.max_seconds = clampRecordMax(
    config.value.recording.max_seconds,
  );
}
// ====== 录制动图：播放解码后端 / 播放模式 / 显存预算（与 Rust ViewerConfig 对应） ======
const animBackendOptions: { label: string; value: "native" | "hybrid" }[] = [
  { label: "native（兼容优先）", value: "native" },
  { label: "hybrid（GPU 混合 · 实验特性）", value: "hybrid" },
];
const animPlaybackOptions: { label: string; value: "gpu" | "ring" }[] = [
  { label: "GPU 常驻显存（默认）", value: "gpu" },
  { label: "内存环形缓冲（旧行为回退）", value: "ring" },
];
const animVramOptions = [
  { label: "自动（按显卡专用显存钳制）", value: 0 },
  { label: "2048 MB", value: 2048 },
  { label: "3072 MB", value: 3072 },
  { label: "4096 MB", value: 4096 },
  { label: "6144 MB", value: 6144 },
];

// ====== 显示器：面板色域（自动识别，只读展示） ======
interface PanelGamut {
  r: [number, number];
  g: [number, number];
  b: [number, number];
  white: [number, number];
  max_luminance: number;
  max_full_frame_luminance: number;
  name: string;
  p3_coverage: number;
  bt2020_coverage: number;
  source: string;
  adapter_name: string;
  monitor_name: string | null;
  edid_max_luminance: number | null;
  measured_max_luminance: number;
}
interface HdrDisplayInfo {
  any_hdr: boolean;
  max_luminance: number;
  sdr_white_nits: number;
}
const panelGamut = ref<PanelGamut | null>(null);
const sdrWhiteNits = ref<number | null>(null);
const gamutLoading = ref(false);
/** 覆盖率显示（百分比，保留 2 位小数） */
const fmtCoverage = (v: number | null | undefined) =>
  v == null ? "—" : `${(v * 100).toFixed(2)}%`;

async function loadGamut() {
  gamutLoading.value = true;
  try {
    const [gamut, hdr] = await Promise.all([
      invoke<PanelGamut | null>("panel_gamut"),
      invoke<HdrDisplayInfo>("hdr_display_info"),
    ]);
    panelGamut.value = gamut;
    sdrWhiteNits.value = hdr?.sdr_white_nits ?? null;
  } catch {
    panelGamut.value = null;
    sdrWhiteNits.value = null;
  } finally {
    gamutLoading.value = false;
  }
}

// ====== 画质与色调映射（v2 预设系统） ======
const tonemapPresets = computed<ToneMapPreset[]>(() => [
  ...TONEMAP_BUILTIN_PRESETS,
  ...(config.value.tonemap_settings?.custom_presets ?? []),
]);
const activePreset = computed(
  () =>
    tonemapPresets.value.find(
      (p) => p.id === config.value.tonemap_settings.active_preset,
    ) ?? TONEMAP_BUILTIN_PRESETS[0],
);
const hasOverrides = computed(() => {
  const ov = config.value.tonemap_settings.overrides;
  return ov && Object.values(ov).some((v) => v !== null);
});

function emptyOverrides(): ToneMapOverrides {
  return {
    source_peak_nits: null,
    output_diffuse_white: null,
    exposure_ev: null,
    saturation: null,
    contrast: null,
    gamut_strength: null,
    dither: null,
    adaptive_peak: null,
  };
}

function selectPreset(p: ToneMapPreset) {
  config.value.tonemap_settings.active_preset = p.id;
  config.value.tonemap_settings.overrides = emptyOverrides();
  message.success(`已切换到「${p.name}」`, { duration: 1800 });
}

function resetOverrides() {
  config.value.tonemap_settings.overrides = emptyOverrides();
  message.success("已恢复预设参数", { duration: 1500 });
}

/** 有效值 = 预设 + overrides 合成（滑杆双向绑定；写即进入「已自定义」态） */
function numEff(
  key: "source_peak_nits" | "output_diffuse_white" | "exposure_ev" | "saturation" | "contrast" | "gamut_strength",
) {
  return computed<number>({
    get: () =>
      (config.value.tonemap_settings.overrides?.[key] ??
        activePreset.value[key]) as number,
    set: (v: number) => {
      config.value.tonemap_settings.overrides[key] = v;
    },
  });
}
function boolEff(key: "dither" | "adaptive_peak") {
  return computed<boolean>({
    get: () =>
      (config.value.tonemap_settings.overrides?.[key] ??
        activePreset.value[key]) as boolean,
    set: (v: boolean) => {
      config.value.tonemap_settings.overrides[key] = v;
    },
  });
}
const effPeak = numEff("source_peak_nits");
const effDiffuse = numEff("output_diffuse_white");
const effEv = numEff("exposure_ev");
const effSat = numEff("saturation");
const effContrast = numEff("contrast");
const effGamut = numEff("gamut_strength");
const effDither = boolEff("dither");
const effAdaptive = boolEff("adaptive_peak");

/** 步进元信息：-/+ 按钮共用（min/max/step 与滑杆保持一致；diffuse 用 0.01 细步进） */
const stepMeta = {
  peak: { min: 100, max: 4000, step: 50 },
  diffuse: { min: 0.6, max: 1, step: 0.01 },
  ev: { min: -1, max: 1, step: 0.05 },
  sat: { min: 0.8, max: 1.3, step: 0.01 },
  contrast: { min: 0.8, max: 1.2, step: 0.01 },
  gamut: { min: 0, max: 1, step: 0.05 },
} as const;

/** 步进调整（-/+ 共用）：钳制到 [min,max]，浮点步进取整消除误差累积。
 *  Vue 模板对 computed 自动解包，故传当前值 + 写回函数 */
function stepVal(
  cur: number,
  meta: { min: number; max: number; step: number },
  dir: 1 | -1,
): number {
  const decimals = (meta.step.toString().split(".")[1] || "").length;
  const raw = cur + dir * meta.step;
  return Number(Math.min(meta.max, Math.max(meta.min, raw)).toFixed(decimals));
}
const stepPeak = (d: 1 | -1) => (effPeak.value = stepVal(effPeak.value, stepMeta.peak, d));
const stepDiffuse = (d: 1 | -1) => (effDiffuse.value = stepVal(effDiffuse.value, stepMeta.diffuse, d));
const stepEv = (d: 1 | -1) => (effEv.value = stepVal(effEv.value, stepMeta.ev, d));
const stepSat = (d: 1 | -1) => (effSat.value = stepVal(effSat.value, stepMeta.sat, d));
const stepContrast = (d: 1 | -1) => (effContrast.value = stepVal(effContrast.value, stepMeta.contrast, d));
const stepGamut = (d: 1 | -1) => (effGamut.value = stepVal(effGamut.value, stepMeta.gamut, d));

/** 保存当前调整为自定义预设（预设值 + overrides 合并快照） */
const showPresetModal = ref(false);
const presetName = ref("");
function openSavePreset() {
  presetName.value = "";
  showPresetModal.value = true;
}
function saveAsPreset() {
  const name = presetName.value.trim() || "我的预设";
  const merged: ToneMapPreset = {
    ...activePreset.value,
    id: `custom:${Date.now()}`,
    name,
    desc: "自定义预设",
    builtin: false,
    source_peak_nits: effPeak.value,
    output_diffuse_white: effDiffuse.value,
    exposure_ev: effEv.value,
    saturation: effSat.value,
    contrast: effContrast.value,
    gamut_strength: effGamut.value,
    dither: effDither.value,
    adaptive_peak: effAdaptive.value,
  };
  config.value.tonemap_settings.custom_presets.push(merged);
  config.value.tonemap_settings.active_preset = merged.id;
  config.value.tonemap_settings.overrides = emptyOverrides();
  showPresetModal.value = false;
  message.success(`已保存预设「${name}」`, { duration: 1800 });
}
function deleteCustomPreset(p: ToneMapPreset) {
  const ts = config.value.tonemap_settings;
  ts.custom_presets = ts.custom_presets.filter((x) => x.id !== p.id);
  if (ts.active_preset === p.id) {
    ts.active_preset = "builtin:soft";
    ts.overrides = emptyOverrides();
  }
  message.success(`已删除「${p.name}」`, { duration: 1500 });
}

async function pickDir() {
  const selected = await openDialog({ directory: true, multiple: false });
  if (typeof selected === "string") {
    config.value.save_dir = selected;
  }
}

// ====== 显示器 ======
const silentMonitorOptions = [
  { label: "主显示器", value: "primary" },
  { label: "鼠标所在显示器", value: "cursor" },
];

// ====== OCR ======
const ocrLangOptions = ref([{ label: "自动（中文优先）", value: "" }]);
onMounted(async () => {
  try {
    const langs = await invoke<string[]>("ocr_languages");
    ocrLangOptions.value = [
      { label: "自动（中文优先）", value: "" },
      ...langs.map((t) => ({ label: t, value: t })),
    ];
  } catch {
    // 保持仅"自动"
  }
  loadFileAssoc();
});

// ====== 热键（点击录入 + 冲突检测） ======
const MOD_ALT = 1;
const MOD_CONTROL = 2;
const MOD_SHIFT = 4;
const MOD_WIN = 8;

const VK_TO_LABEL: Record<number, string> = {
  0x2c: "PrtSc",
  0x2d: "Ins",
  0x2e: "Del",
  0x24: "Home",
  0x23: "End",
  0x21: "PgUp",
  0x22: "PgDn",
  0x25: "←",
  0x26: "↑",
  0x27: "→",
  0x28: "↓",
  0x0d: "Enter",
  0x1b: "Esc",
  0x20: "Space",
  0x09: "Tab",
  0x08: "Bksp",
};
for (let i = 0x41; i <= 0x5a; i++) VK_TO_LABEL[i] = String.fromCharCode(i);
for (let i = 0x30; i <= 0x39; i++) VK_TO_LABEL[i] = String.fromCharCode(i);
for (let i = 0x70; i <= 0x7b; i++) VK_TO_LABEL[i] = "F" + (i - 0x6f);

function vkLabel(vk: number): string {
  return VK_TO_LABEL[vk] ?? `VK${vk.toString(16).toUpperCase()}`;
}
function modsLabel(mods: number): string {
  const parts: string[] = [];
  if (mods & MOD_WIN) parts.push("Win");
  if (mods & MOD_CONTROL) parts.push("Ctrl");
  if (mods & MOD_ALT) parts.push("Alt");
  if (mods & MOD_SHIFT) parts.push("Shift");
  return parts.join("+");
}
function hotkeyLabel(hk: { modifiers: number; vk: number }): string {
  const m = modsLabel(hk.modifiers);
  const k = vkLabel(hk.vk);
  return m ? `${m}+${k}` : k;
}

type HotkeyKind = "region" | "fullscreen" | "silent" | "clean" | "record_start" | "record_stop" | "video_start" | "video_stop";
const capturingHotkeyFor = ref<null | HotkeyKind>(null);
const hotkeyKinds: HotkeyKind[] = ["region", "fullscreen", "silent", "clean", "record_start", "record_stop", "video_start", "video_stop"];
const hotkeyField: Record<
  HotkeyKind,
  "region_hotkey" | "fullscreen_hotkey" | "silent_hotkey" | "clean" | "record_start_hotkey" | "record_stop_hotkey" | "video_start_hotkey" | "video_stop_hotkey"
> = {
  region: "region_hotkey",
  fullscreen: "fullscreen_hotkey",
  silent: "silent_hotkey",
  clean: "clean",
  record_start: "record_start_hotkey",
  record_stop: "record_stop_hotkey",
  video_start: "video_start_hotkey",
  video_stop: "video_stop_hotkey",
};
const hotkeyName: Record<HotkeyKind, string> = {
  region: "区域截图",
  fullscreen: "全屏截图",
  silent: "静默截图",
  clean: "清理内存",
  record_start: "开始录制（游戏模式）",
  record_stop: "停止录制",
  video_start: "开始录制视频（游戏模式）",
  video_stop: "停止录制视频",
};

// 读取当前热键值（clean 为嵌套 Option 字段：config.memory_clean.hotkey；录制两项为 Option）
function getHotkey(kind: HotkeyKind): { modifiers: number; vk: number } | null {
  if (kind === "clean") return config.value.memory_clean?.hotkey ?? null;
  return config.value[hotkeyField[kind] as "region_hotkey"] ?? null;
}
// 写入当前热键值（clean 写嵌套字段）
function setHotkey(kind: HotkeyKind, hk: { modifiers: number; vk: number }) {
  if (kind === "clean") {
    config.value.memory_clean = { ...config.value.memory_clean, hotkey: hk };
  } else {
    config.value[hotkeyField[kind] as "region_hotkey"] = hk;
  }
}

function startCaptureHotkey(kind: HotkeyKind) {
  if (capturingHotkeyFor.value === kind) return;
  if (capturingHotkeyFor.value !== null) {
    capturingHotkeyFor.value = null;
    window.removeEventListener("keydown", onKeyCapture, true);
    window.removeEventListener("blur", onCaptureBlur);
  }
  capturingHotkeyFor.value = kind;
  invoke("set_hotkey_capture_mode", { enabled: true }).catch(() => {});
  window.addEventListener("keydown", onKeyCapture, true);
  window.addEventListener("blur", onCaptureBlur);
}
async function stopCaptureHotkey() {
  if (capturingHotkeyFor.value === null) return;
  capturingHotkeyFor.value = null;
  window.removeEventListener("keydown", onKeyCapture, true);
  window.removeEventListener("blur", onCaptureBlur);
  try {
    await invoke("set_hotkey_capture_mode", { enabled: false });
  } catch {
    // ignore
  }
}
function onCaptureBlur() {
  stopCaptureHotkey();
}
function onKeyCapture(e: KeyboardEvent) {
  if (capturingHotkeyFor.value === null) return;
  if (e.key === "Escape") {
    e.preventDefault();
    e.stopPropagation();
    stopCaptureHotkey();
    return;
  }
  if (
    e.code === "AltLeft" || e.code === "AltRight" ||
    e.code === "ControlLeft" || e.code === "ControlRight" ||
    e.code === "ShiftLeft" || e.code === "ShiftRight" ||
    e.code === "MetaLeft" || e.code === "MetaRight"
  ) {
    return;
  }
  e.preventDefault();
  e.stopPropagation();

  const mods =
    (e.altKey ? MOD_ALT : 0) |
    (e.ctrlKey ? MOD_CONTROL : 0) |
    (e.shiftKey ? MOD_SHIFT : 0) |
    (e.metaKey ? MOD_WIN : 0);
  const vk = eventCodeToVk(e.code);
  if (vk === 0) {
    stopCaptureHotkey();
    return;
  }
  if (mods === 0 && vk !== 0x2c) {
    message.error("请至少搭配一个修饰键（Ctrl / Alt / Shift / Win）", { duration: 2200 });
    stopCaptureHotkey();
    return;
  }

  const hk = { modifiers: mods, vk };
  const kind = capturingHotkeyFor.value;
  const oldHk = getHotkey(kind);
  setHotkey(kind, hk);
  invoke("register_hotkey", { kind, hotkey: hk })
    .then(() => {
      message.success(`${hotkeyName[kind]}热键已更新: ${hotkeyLabel(hk)}`, { duration: 2200 });
    })
    .catch((err) => {
      // 冲突：回滚显示，旧热键继续生效（clean/录制项未配置时回滚为 null）
      if (kind === "clean") {
        config.value.memory_clean = { ...config.value.memory_clean, hotkey: oldHk };
      } else if (kind === "record_start" || kind === "record_stop") {
        (config.value as Record<string, unknown>)[hotkeyField[kind]] = oldHk;
      } else {
        config.value[hotkeyField[kind] as "region_hotkey"] =
          oldHk ?? { modifiers: 2 | 4, vk: 0x44 };
      }
      message.error(String(err), { duration: 2200 });
    })
    .finally(() => {
      stopCaptureHotkey();
    });
}
function eventCodeToVk(code: string): number {
  if (code.startsWith("Key")) return code.charCodeAt(3);
  if (code.startsWith("Digit")) return code.charCodeAt(5);
  if (code.startsWith("F") && /^F([1-9]|1[0-2])$/.test(code)) {
    return 0x6f + parseInt(code.slice(1));
  }
  const map: Record<string, number> = {
    PrintScreen: 0x2c,
    Insert: 0x2d,
    Delete: 0x2e,
    Home: 0x24,
    End: 0x23,
    PageUp: 0x21,
    PageDown: 0x22,
    ArrowLeft: 0x25,
    ArrowUp: 0x26,
    ArrowRight: 0x27,
    ArrowDown: 0x28,
    Enter: 0x0d,
    Escape: 0x1b,
    Space: 0x20,
    Tab: 0x09,
    Backspace: 0x08,
  };
  return map[code] ?? 0;
}
onUnmounted(stopCaptureHotkey);

// ====== 内存清理（对照设计文档 4.2.3 布局图，行为对齐 Mem Reduct） ======
// 内存分区占用（字节 + 百分比）
interface MemPart {
  percent: number;
  used: number;
  total: number;
}
// 内存信息（后端 get_memory_info 命令 / memory://status 每秒事件，值为字节）
interface MemoryInfo {
  physical: MemPart; // 物理内存
  commit: MemPart; // 提交内存
  system_cache: number; // 系统缓存
  standby: number; // 备用列表
  modified: number; // 已修改页
  paged_pool: number; // 页面池
  non_paged_pool: number; // 非页面池
}
// 单区域清理结果
interface CleanRegionResult {
  mask: number;
  name: string;
  ok: boolean;
  error?: string | null;
}
// 清理结果（clean_memory 命令返回）
interface CleanResult {
  freed_bytes: number;
  per_region: CleanRegionResult[];
}
// 清理统计（get_clean_stats 命令返回，与 config.memory_clean.stats 同构）
interface CleanStats {
  last_clean_ts: number;
  last_freed_bytes: number;
  total_clean_count: number;
  total_freed_bytes: number;
}

// 8 个清理区域（与 Mem Reduct main.h mask 位逐位对齐，顺序对照布局图）
const CLEAN_REGIONS: {
  mask: number;
  label: string;
  danger: boolean; // 危险项（REDUCT_MASK_FREEZES：0x08|0x10）
  req?: string; // 系统版本要求
  tip?: string; // 危险项 tooltip 说明
}[] = [
  { mask: 0x01, label: "工作集", danger: false },
  { mask: 0x02, label: "系统文件缓存", danger: false },
  { mask: 0x04, label: "低优先级备用列表", danger: false },
  { mask: 0x20, label: "合并物理内存页", danger: false, req: "Win10+" },
  { mask: 0x40, label: "注册表缓存", danger: false, req: "Win8.1+" },
  { mask: 0x80, label: "卷修改缓存", danger: false },
  { mask: 0x08, label: "备用列表", danger: true, tip: "清空缓存后短期内系统可能变卡" },
  { mask: 0x10, label: "已修改页列表", danger: true, tip: "强制写回磁盘，可能造成短暂磁盘高负载" },
];

const dialog = useDialog();
const memInfo = ref<MemoryInfo | null>(null); // 内存信息（后端未就绪时为 null → 占位 --）
const elevated = ref(false); // 是否管理员权限
const cleaning = ref(false); // 立即清理执行中
const cleanStats = ref<CleanStats | null>(null); // 后端实时统计（未拉到时回退 config 持久化值）
let unlistenMemoryFn: (() => void) | null = null; // memory://status 事件清理函数

// 字节格式化：≥1GB 用 GB（1 位小数）/ ≥1MB 用 MB / 其余 KB、B
function formatBytes(bytes?: number | null): string {
  if (bytes == null || !Number.isFinite(bytes)) return "--";
  if (bytes >= 1024 ** 3) return `${(bytes / 1024 ** 3).toFixed(1)} GB`;
  if (bytes >= 1024 ** 2) return `${(bytes / 1024 ** 2).toFixed(0)} MB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${bytes} B`;
}

// 清理时间戳格式化（秒/毫秒自适应）→ HH:MM:SS
function formatClock(ts: number): string {
  if (!ts) return "尚未清理";
  const ms = ts > 1e12 ? ts : ts * 1000;
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

// 分级：normal 主题色 / warn 奶茶橘 --jb-milk / danger 豆沙红 --jb-red（阈值来自配置）
const memLevel = computed<"normal" | "warn" | "danger">(() => {
  const p = memInfo.value?.physical.percent ?? 0;
  const warn = config.value.memory_clean?.danger_warning ?? 70;
  const crit = config.value.memory_clean?.danger_critical ?? 90;
  if (p >= crit) return "danger";
  if (p >= warn) return "warn";
  return "normal";
});

// 大数字百分比文案（后端未就绪 → --）
const memPercentText = computed(() =>
  memInfo.value ? `${Math.round(memInfo.value.physical.percent)}%` : "--",
);

// 统计展示：优先后端实时值，命令不可用时回退 config 持久化值
const statsView = computed<CleanStats>(
  () => cleanStats.value ?? config.value.memory_clean.stats,
);

// 清理区域勾选状态（位运算读写 config.memory_clean.mask，变更走 deep watch 自动保存）
function regionChecked(mask: number): boolean {
  return (config.value.memory_clean.mask & mask) !== 0;
}
function toggleRegion(mask: number, checked: boolean) {
  const cur = config.value.memory_clean.mask;
  config.value.memory_clean.mask = checked ? cur | mask : cur & ~mask;
}

// 立即清理：勾选含危险区域（0x08|0x10）时先二次确认列清单（对齐 memreduct freezes 行为）
function cleanNow() {
  const mask = config.value.memory_clean.mask;
  const dangerList = CLEAN_REGIONS.filter((r) => r.danger && (mask & r.mask) !== 0);
  if (dangerList.length > 0) {
    dialog.warning({
      title: "确认执行危险清理？",
      content: () =>
        h("div", { style: "font-size:12px; line-height:1.7" }, [
          h("div", "即将执行以下危险清理："),
          h(
            "ul",
            { style: "margin:6px 0; padding-left:18px" },
            dangerList.map((r) => h("li", `${r.label}（${r.tip}）`)),
          ),
          h("div", { style: "color: var(--jb-red)" }, "清理后短期内系统可能变卡或磁盘负载升高"),
        ]),
      positiveText: "仍要清理",
      negativeText: "取消",
      onPositiveClick: () => {
        doClean(mask);
      },
    });
    return;
  }
  doClean(mask);
}

// 执行清理（手动来源）：成功 toast「已释放 X」，失败区域逐项 toast，stats 重新拉取
async function doClean(mask: number) {
  if (cleaning.value) return;
  cleaning.value = true;
  try {
    const result = await invoke<CleanResult>("clean_memory", { mask, source: "manual" });
    message.success(`已释放 ${formatBytes(result.freed_bytes)}`, { duration: 2200 });
    // 权限类失败（0xC0000061/0xC0000350）聚合引导提权，其余失败区域逐项提示
    const denied = (result.per_region ?? []).filter(
      (r) => !r.ok && /0xC0000061|0xC0000350/.test(r.error ?? ""),
    );
    if (denied.length > 0) {
      message.warning(
        `${denied.length} 个清理区域需要管理员权限，请点击下方「以管理员身份重启」后重试`,
        { duration: 4200 },
      );
    }
    for (const r of result.per_region ?? []) {
      if (!r.ok && !denied.includes(r)) {
        message.error(`${r.name} 清理失败: ${r.error ?? "未知错误"}`, { duration: 2200 });
      }
    }
    await refreshStats();
  } catch (e) {
    message.error("清理失败: " + e, { duration: 2200 });
  } finally {
    cleaning.value = false;
  }
}

// 只清自己：SetProcessWorkingSetSize(自身)，无需管理员权限
async function cleanOwn() {
  try {
    await invoke("clean_own_working_set");
    message.success("已清理本进程工作集", { duration: 2200 });
  } catch (e) {
    message.error("清理失败: " + e, { duration: 2200 });
  }
}

// 重新拉取统计（命令失败静默，保持 config 持久化值展示）
async function refreshStats() {
  try {
    cleanStats.value = await invoke<CleanStats>("get_clean_stats");
  } catch {
    // 后端未就绪：保持现状
  }
}

// 以管理员身份重启（ShellExecuteW "runas"）
async function restartElevated() {
  try {
    await invoke("restart_elevated");
  } catch (e) {
    message.error("提权重启失败: " + e, { duration: 2200 });
  }
}

// 初始化：内存信息快照 + 每秒事件刷新 + 权限 + 统计（全部容错，后端未就绪不阻断 UI）
onMounted(async () => {
  try {
    memInfo.value = await invoke<MemoryInfo>("get_memory_info");
  } catch {
    // 后端未就绪：大数字区显示 --
  }
  try {
    unlistenMemoryFn = await listen<MemoryInfo>("memory://status", (ev) => {
      memInfo.value = ev.payload;
    });
  } catch {
    // 事件不可用：仅保留初始快照
  }
  try {
    elevated.value = await invoke<boolean>("is_elevated");
  } catch {
    elevated.value = false;
  }
  refreshStats();
});
onUnmounted(() => {
  if (unlistenMemoryFn) unlistenMemoryFn();
});

// ====== 配置管理：备份 / 导出 / 导入 ======
function fileTimestamp(): string {
  const d = new Date();
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}_${p(d.getHours())}${p(d.getMinutes())}${p(d.getSeconds())}`;
}

// 一键备份到 %APPDATA%\jietu-hdr\backups\
async function backupNow() {
  try {
    const path = await invoke<string>("backup_config");
    const name = path.split(/[\\\/]/).pop() || path;
    message.success(`已备份 ${name}`, { duration: 2200 });
  } catch (e) {
    message.error("备份失败: " + e, { duration: 2200 });
  }
}

// 导出：另存为 TOML 副本到任意位置
async function exportNow() {
  try {
    const path = await saveDialog({
      defaultPath: `jietu-config_${fileTimestamp()}.toml`,
      filters: [{ name: "TOML 配置", extensions: ["toml"] }],
    });
    if (typeof path !== "string" || !path) return;
    await invoke("export_config", { path });
    message.success("配置已导出", { duration: 2200 });
  } catch (e) {
    message.error("导出失败: " + e, { duration: 2200 });
  }
}

// 导入：读取 TOML → 保存 → 重载配置并应用主题色/热键
async function importNow() {
  try {
    const path = await openDialog({
      multiple: false,
      filters: [{ name: "TOML 配置", extensions: ["toml"] }],
    });
    if (typeof path !== "string" || !path) return;
    await invoke("import_config", { path });
    // 重载配置（get_config + autostart 实际状态）并应用主题色
    await loadConfig();
    setThemeColor(config.value.theme_color || "taro");
    message.success("配置已导入并应用", { duration: 2200 });
  } catch (e) {
    message.error("导入失败: " + e, { duration: 2200 });
  }
}

// ====== 关于 / 退出 ======
const appVersion = "0.1.0";
async function quitApp() {
  await invoke("quit_app");
}

// ====== QQ 交流群：点击复制群号（与主面板状态条一致） ======
const QQ_GROUP = "1094033308";
async function copyQqGroup() {
  try {
    await navigator.clipboard.writeText(QQ_GROUP);
    message.success(`群号 ${QQ_GROUP} 已复制`, { duration: 2200 });
  } catch {
    message.error("复制失败", { duration: 1800 });
  }
}
</script>

<template>
  <div class="settings-root">
    <TitleBar
      :mode="props.mode"
      :is-dark="props.isDark"
      :set-mode="props.setMode"
      show-back
      title="设置中心"
      @back="emit('back')"
    />

    <div class="settings-body">
      <!-- 侧导航 -->
      <aside class="settings-nav">
        <button
          v-for="s in sections"
          :key="s.id"
          class="nav-item"
          :class="{ active: activeSection === s.id }"
          @click="activeSection = s.id"
        >
          <n-icon :component="s.icon" size="16" />
          <span>{{ s.label }}</span>
        </button>
        <div class="nav-footer">
          <button
            class="nav-qq"
            :title="`QQ交流群：${QQ_GROUP}（点击复制）`"
            @click="copyQqGroup"
          >
            <n-icon :component="MessageCircle" size="13" />
            <span>{{ QQ_GROUP }}</span>
          </button>
          <span class="nav-ver">v{{ appVersion }}</span>
          <n-button text size="tiny" @click="quitApp">
            <template #icon>
              <n-icon :component="Logout" />
            </template>
            退出
          </n-button>
        </div>
      </aside>

      <!-- 分组内容 -->
      <main class="settings-content">
        <!-- 自动保存指示 -->
        <div class="save-hint" :class="saveState">
          <template v-if="saveState === 'saving'">保存中…</template>
          <template v-else-if="saveState === 'saved'">
            <n-icon :component="Check" size="12" /> 已自动保存
          </template>
          <template v-else>更改会自动保存</template>
        </div>

        <!-- 外观 -->
        <div v-if="activeSection === 'appearance'" class="group">
          <div class="group-title">外观</div>
          <div class="row">
            <span class="label">主题模式</span>
            <n-select v-model:value="themeMode" :options="themeOptions" size="small" style="flex: 1" />
          </div>

          <div class="sub-title">预设主题色</div>
          <div class="swatch-grid">
            <button
              v-for="p in presets"
              :key="p.id"
              class="swatch"
              :class="{ active: config.theme_color === p.id }"
              :title="p.name"
              @click="applyPreset(p.id)"
            >
              <span class="swatch-dot" :style="{ background: p.primary }" />
              <span class="swatch-name">{{ p.name }}</span>
            </button>
          </div>

          <div class="sub-title">自定义主题色</div>
          <div class="row">
            <label class="color-native" :style="{ background: primaryColor }">
              <input type="color" :value="primaryColor" @input="onColorInput" />
            </label>
            <n-input
              :value="customHex"
              size="small"
              placeholder="#RRGGBB"
              style="flex: 1"
              @update:value="onHexInput"
            />
          </div>

          <div class="sub-title">主面板实时预览</div>
          <div class="panel-preview">
            <div class="pv-titlebar">
              <n-icon :component="Camera" size="12" color="var(--jb-primary)" />
              <span class="pv-title-text">jietu-hdr</span>
              <span class="pv-win-dots"><i></i><i></i><i></i></span>
            </div>
            <div class="pv-body">
              <div class="pv-greeting">准备好捕捉美好瞬间了吗</div>
              <div class="pv-hero">
                <n-icon :component="Camera" size="14" />
                <span>区域截图</span>
              </div>
              <div class="pv-sub-row">
                <span class="pv-sub">
                  <n-icon :component="ArrowsMaximize" size="12" />
                  全屏截图
                </span>
                <span class="pv-sub">
                  <n-icon :component="DeviceFloppy" size="12" />
                  静默保存
                </span>
              </div>
              <div class="pv-chips">
                <span class="pv-chip">
                  <n-icon :component="Photo" size="11" />
                  {{ pvFormat }}
                </span>
                <span class="pv-chip">
                  <n-icon :component="Bolt" size="11" />
                  {{ hotkeyLabel(config.region_hotkey) }}
                </span>
                <span class="pv-chip pv-chip-settings">
                  <n-icon :component="Settings" size="11" />
                  设置
                </span>
              </div>
            </div>
          </div>
          <div class="hint-text">调整取色器或输入 hex 时，预览与全站配色实时更新</div>

          <div class="sub-title">保存为方案</div>
          <div class="row">
            <n-input
              v-model:value="schemeName"
              size="small"
              placeholder="方案名称（如：我的粉色）"
              style="flex: 1"
              @keyup.enter="saveCurrentScheme"
            />
            <n-button size="small" @click="saveCurrentScheme">保存方案</n-button>
          </div>

          <template v-if="schemeList.length > 0">
            <div class="sub-title">我的方案</div>
            <div class="scheme-list">
              <div
                v-for="s in schemeList"
                :key="s.name"
                class="scheme-chip"
                :class="{ active: config.theme_color === s.primary }"
                :title="`应用「${s.name}」`"
                @click="applyScheme(s.primary)"
              >
                <span class="scheme-dot" :style="{ background: s.primary }" />
                <span class="scheme-name">{{ s.name }}</span>
                <button class="scheme-del" title="删除方案" @click.stop="deleteScheme(s.name)">
                  <n-icon :component="Trash" size="12" />
                </button>
              </div>
            </div>
          </template>
        </div>

        <!-- 截图行为 -->
        <div v-else-if="activeSection === 'behavior'" class="group">
          <div class="group-title">截图行为</div>
          <div class="switch-row">
            <span>复制到剪贴板</span>
            <n-switch v-model:value="config.copy_to_clipboard" />
          </div>
          <div class="switch-row">
            <span>自动保存</span>
            <n-switch v-model:value="config.auto_save" />
          </div>
          <div class="switch-row">
            <span>显示预览</span>
            <n-switch v-model:value="config.show_preview" />
          </div>
          <div class="switch-row">
            <span>弹出标注工具栏</span>
            <n-switch v-model:value="config.show_toolbar" />
          </div>
          <div class="switch-row">
            <span>启用贴图</span>
            <n-switch v-model:value="config.enable_pin" />
          </div>
          <div
            class="switch-row"
            title="「截图标识」的截图方式：区域选择 = 透明覆盖层拖选范围后标注；整屏快照 = 跳过拖选，直接截取主显示器全屏进入标注"
          >
            <span>标注截图方式</span>
            <n-select
              v-model:value="config.annotation_capture_mode"
              :options="annotationCaptureOptions"
              size="small"
              style="width: 220px"
            />
          </div>
          <div class="switch-row">
            <span>截图提示音</span>
            <n-switch v-model:value="config.sound_enabled" />
          </div>
          <div class="switch-row" title="仅置顶（钉住）状态生效：面板不隐藏时是否一同截入画面">
            <span>面板一同截入</span>
            <n-switch v-model:value="config.capture_include_tool" />
          </div>
          <div class="switch-row" title="关闭后截图时主面板保持显示（会一同入镜）">
            <span>截图时自动隐藏主面板</span>
            <n-switch v-model:value="config.hide_main_on_capture" />
          </div>
          <div
            class="switch-row"
            title="保存截图后后台 AI 2x 放大生成增强副本（&lt;文件名&gt;_waifu2x_x2.png，不替换原图）"
          >
            <span>截图自动 AI 2x 增强</span>
            <n-switch v-model:value="config.upscale_after_capture" />
          </div>
        </div>

        <!-- 输出 -->
        <div v-else-if="activeSection === 'output'" class="group">
          <div class="group-title">输出</div>
          <div class="row">
            <span class="label">输出格式</span>
            <n-select
              v-model:value="config.output_format"
              :options="formatOptions"
              size="small"
              style="flex: 1"
            />
          </div>
          <div v-if="depthApplicable" class="row" title="JXL 位深：12bit 体积更小（PQ 曲线下视觉差异极小）；16bit 精度最高。PNG 规范仅支持 8/16bit，此项不生效">
            <span class="label">HDR 位深</span>
            <n-select
              v-model:value="config.output_depth"
              :options="depthOptions"
              size="small"
              style="flex: 1"
            />
          </div>
          <div v-if="qualityApplicable" class="row" title="JXL/JXR 专属质量档；PNG/EXR 输出恒定无损">
            <span class="label">编码质量</span>
            <n-select
              v-model:value="config.output_quality"
              :options="qualityOptions"
              size="small"
              style="flex: 1"
            />
          </div>
          <div class="row">
            <span class="label">保存路径</span>
            <n-input
              :value="config.save_dir ?? ''"
              placeholder="默认 Pictures/jietu-hdr"
              size="small"
              style="flex: 1"
              readonly
            />
            <n-button size="small" @click="pickDir">
              <template #icon>
                <n-icon :component="Folder" size="14" />
              </template>
            </n-button>
          </div>
          <div class="row">
            <span class="label">文件名模板</span>
            <n-input
              v-model:value="config.filename_template"
              size="small"
              placeholder="jietu_{date}_{time}"
              style="flex: 1"
            />
          </div>
          <div class="hint-text">HDR→SDR 色调映射参数请到「画质」分区调整</div>
        </div>

        <!-- 画质与色调映射（v2 预设系统） -->
        <div v-else-if="activeSection === 'quality'" class="group">
          <div class="group-title">画质与色调映射</div>
          <div class="sub-title">HDR 预设</div>
          <div class="tm-grid">
            <button
              v-for="p in tonemapPresets"
              :key="p.id"
              class="tm-card"
              :class="{ active: config.tonemap_settings.active_preset === p.id }"
              @click="selectPreset(p)"
            >
              <span v-if="p.adaptive_peak" class="tm-badge">智能</span>
              <span v-else-if="!p.builtin" class="tm-badge tm-badge-custom">自定义</span>
              <span class="tm-name">{{ p.name }}</span>
              <span class="tm-desc">{{ p.desc }}</span>
              <span
                v-if="!p.builtin"
                class="tm-del"
                title="删除预设"
                @click.stop="deleteCustomPreset(p)"
              >
                <n-icon :component="Trash" size="12" />
              </span>
            </button>
          </div>
          <div class="row" style="margin-top: 10px">
            <n-button size="small" @click="openSavePreset">
              <template #icon>
                <n-icon :component="DeviceFloppy" size="13" />
              </template>
              保存当前调整为预设
            </n-button>
          </div>

          <div class="sub-title" style="margin-top: 4px">
            高级参数
            <span v-if="hasOverrides" class="tm-customized">已自定义</span>
          </div>
          <div v-if="hasOverrides" class="tm-hint">
            基于「{{ activePreset.name }}」调整 ·
            <a class="tm-reset" @click="resetOverrides">恢复预设</a>
          </div>

          <div class="tm-adv">
            <div class="tm-slider">
              <span class="label">智能峰值</span>
              <n-switch v-model:value="effAdaptive" size="small" />
              <span class="tm-val">{{ effAdaptive ? "按画面自动" : "手动" }}</span>
            </div>
            <div class="tm-slider">
              <span class="label">场景峰值</span>
              <button class="tm-step" :disabled="effAdaptive" title="-50 nits" @click="stepPeak(-1)">−</button>
              <n-slider
                v-model:value="effPeak"
                :min="100"
                :max="4000"
                :step="50"
                :disabled="effAdaptive"
                :format-tooltip="(v: number) => `${v} nits`"
              />
              <button class="tm-step" :disabled="effAdaptive" title="+50 nits" @click="stepPeak(1)">＋</button>
              <span class="tm-val">{{ effAdaptive ? "自动" : `${effPeak} nits` }}</span>
            </div>
            <div class="tm-slider">
              <span class="label">SDR 白落点</span>
              <button class="tm-step" title="-0.01" @click="stepDiffuse(-1)">−</button>
              <n-slider
                v-model:value="effDiffuse"
                :min="0.6"
                :max="1"
                :step="0.01"
                :format-tooltip="(v: number) => v.toFixed(2)"
              />
              <button class="tm-step" title="+0.01" @click="stepDiffuse(1)">＋</button>
              <span class="tm-val">{{ effDiffuse.toFixed(2) }}</span>
            </div>
            <div class="tm-slider">
              <span class="label">曝光</span>
              <button class="tm-step" title="-0.05 EV" @click="stepEv(-1)">−</button>
              <n-slider
                v-model:value="effEv"
                :min="-1"
                :max="1"
                :step="0.05"
                :format-tooltip="(v: number) => `${v > 0 ? '+' : ''}${v.toFixed(2)} EV`"
              />
              <button class="tm-step" title="+0.05 EV" @click="stepEv(1)">＋</button>
              <span class="tm-val">{{ effEv > 0 ? "+" : "" }}{{ effEv.toFixed(2) }} EV</span>
            </div>
            <div class="tm-slider">
              <span class="label">饱和度</span>
              <button class="tm-step" title="-0.01" @click="stepSat(-1)">−</button>
              <n-slider
                v-model:value="effSat"
                :min="0.8"
                :max="1.3"
                :step="0.01"
                :format-tooltip="(v: number) => `×${v.toFixed(2)}`"
              />
              <button class="tm-step" title="+0.01" @click="stepSat(1)">＋</button>
              <span class="tm-val">×{{ effSat.toFixed(2) }}</span>
            </div>
            <div class="tm-slider">
              <span class="label">对比度</span>
              <button class="tm-step" title="-0.01" @click="stepContrast(-1)">−</button>
              <n-slider
                v-model:value="effContrast"
                :min="0.8"
                :max="1.2"
                :step="0.01"
                :format-tooltip="(v: number) => `×${v.toFixed(2)}`"
              />
              <button class="tm-step" title="+0.01" @click="stepContrast(1)">＋</button>
              <span class="tm-val">×{{ effContrast.toFixed(2) }}</span>
            </div>
            <div class="tm-slider">
              <span class="label">色域压缩</span>
              <button class="tm-step" title="-0.05" @click="stepGamut(-1)">−</button>
              <n-slider
                v-model:value="effGamut"
                :min="0"
                :max="1"
                :step="0.05"
                :format-tooltip="(v: number) => v.toFixed(2)"
              />
              <button class="tm-step" title="+0.05" @click="stepGamut(1)">＋</button>
              <span class="tm-val">{{ effGamut.toFixed(2) }}</span>
            </div>
            <div class="tm-slider">
              <span class="label">量化抖动</span>
              <n-switch v-model:value="effDither" size="small" />
              <span class="tm-val">{{ effDither ? "开" : "关" }}</span>
            </div>
          </div>
          <div class="hint-text">
            <n-icon :component="Adjustments" size="12" />
            SDR 白落点：高 = 桌面还原更严格，低 = 高光细节空间更大
          </div>
        </div>

        <!-- 热键 -->
        <div v-else-if="activeSection === 'hotkey'" class="group">
          <div class="group-title">热键</div>
          <div v-for="kind in hotkeyKinds" :key="kind" class="hotkey-row">
            <span class="label">{{ hotkeyName[kind] }}</span>
            <div
              class="hk-field"
              :class="{ editing: capturingHotkeyFor === kind }"
              :title="capturingHotkeyFor === kind ? '按下新组合键，Esc 取消' : '点击修改热键'"
              @click="
                capturingHotkeyFor === kind
                  ? stopCaptureHotkey()
                  : startCaptureHotkey(kind)
              "
            >
              <span v-if="capturingHotkeyFor === kind" class="hk-hint">
                按下新组合键 · Esc 取消
              </span>
              <span v-else class="hk-value">
                  {{ getHotkey(kind) ? hotkeyLabel(getHotkey(kind)!) : "未设置" }}
                </span>
            </div>
          </div>
        </div>

        <!-- 录制动图 -->
        <div v-else-if="activeSection === 'record'" class="group">
          <div class="group-title">录制动图</div>
          <div class="row" title="JXL 动图编码质量档（映射 libjxl effort）：High 为速度/体积平衡点；Lossless 数学无损但体积大、编码慢">
            <span class="label">编码质量</span>
            <n-select
              v-model:value="config.recording.quality"
              :options="recordQualityOptions"
              size="small"
              style="flex: 1"
            />
          </div>
          <div class="row" title="帧率上限；auto = 输出区域 ≥4K → 30fps，其余 60fps">
            <span class="label">帧率</span>
            <n-select
              v-model:value="config.recording.fps"
              :options="recordFpsOptions"
              size="small"
              style="flex: 1"
            />
          </div>
          <div class="row" title="最长录制时长（秒），到时自动停止并保存；范围 1–30">
            <span class="label">录制时长上限</span>
            <n-select
              v-model:value="recordMaxSelect"
              :options="recordMaxOptions"
              size="small"
              :style="recordMaxIsCustom ? 'width: 116px' : 'flex: 1'"
            />
            <n-input-number
              v-if="recordMaxIsCustom"
              :value="config.recording.max_seconds"
              :min="1"
              :max="30"
              :precision="0"
              :show-button="false"
              size="small"
              style="flex: 1"
              @update:value="onRecordMaxInput"
              @blur="onRecordMaxBlur"
            >
              <template #suffix>秒</template>
            </n-input-number>
          </div>
          <div class="record-sub">
            最长录制时长（秒），到时自动停止并保存；范围 1–30。
          </div>
          <div class="row" title="画面静止的帧自动去重，不占体积；游戏画面/视频几乎每帧都有效">
            <span class="label">静止帧去重</span>
            <span class="readonly-value">自动开启</span>
          </div>
          <div class="row" title="输出固定为 HDR 容器：JPEG XL 16bit + PQ/BT.2020（HDR10），HDR 屏上回放为真实物理亮度">
            <span class="label">输出规格</span>
            <span class="readonly-value">JXL 动图 · 16bit PQ/BT.2020</span>
          </div>
          <div class="row" title="动图播放解码后端：native 纯 CPU 解码兼容性最好；hybrid 为实验特性（CPU 熵解码 + GPU 像素重建），异常时自动回退 native">
            <span class="label">播放解码后端</span>
            <n-select
              v-model:value="config.viewer.anim_backend"
              :options="animBackendOptions"
              size="small"
              style="flex: 1"
            />
          </div>
          <div class="record-sub">
            hybrid 为实验特性：熵解码在 CPU、像素重建在 GPU，出现异常自动回退 native。
          </div>
          <div class="row" title="播放模式：gpu = 显存常驻池，短动图循环播放零重复解码；ring = 内存环形缓冲（旧行为回退）">
            <span class="label">播放模式</span>
            <n-select
              v-model:value="config.viewer.anim_playback"
              :options="animPlaybackOptions"
              size="small"
              style="flex: 1"
            />
          </div>
          <div class="row" title="GPU 池显存预算（MB），0 = auto：按显卡专用显存自动钳制">
            <span class="label">显存预算</span>
            <n-select
              v-model:value="config.viewer.anim_vram_mb"
              :options="animVramOptions"
              size="small"
              style="flex: 1"
            />
          </div>
          <div class="record-sub">
            实际池容量 = 预算 ÷ 单帧大小；自动模式按显卡专用显存钳制（约 50% − 512MB，上限 4096MB）。
          </div>
          <div class="row" title="纯录制模式（游戏场景）：开 = 录制期间零编码、停止后全核编码；关 = 边录边编">
            <span class="label">编码模式</span>
            <n-switch v-model:value="config.recording.deferred_encode" size="small" />
          </div>
          <div class="record-sub">
            开 = 纯录制（游戏），关 = 边录边编。与主面板录制弹窗的「游戏模式」同源：游戏模式开启时强制纯录制。
          </div>
          <div class="record-hint">
            录制时长上限可在上方设置（1–30 秒，默认 30）· 全屏 · 画面静止自动去重 · 保存到截图目录（record_时间戳.jxl）。
            「游戏模式」开关在主面板的录制动图弹窗内：录制期间零编码不影响游戏帧数，
            停止后全核编码需等待片刻。开始/停止热键在「热键」分组配置。
            解码后端 / 播放模式 / 编码模式在此配置，改后下次打开播放器生效。
          </div>
        </div>

        <!-- 录制视频（MKV 长录制 + 播放器） -->
        <div v-else-if="activeSection === 'video'" class="group">
          <div class="group-title">录制视频</div>
          <div class="row" title="视频编码格式：HEVC 同画质体积约为 H.264 的一半，但老设备播放兼容性略差">
            <span class="label">编码格式</span>
            <n-select
              v-model:value="config.video.codec"
              :options="videoCodecOptions"
              size="small"
              style="flex: 1"
            />
          </div>
          <div class="row" title="硬件编码器：自动按 显卡能力 依次探测 NVENC→AMF→QSV，都不可用时回退 CPU 软编（帧率会明显下降）">
            <span class="label">硬件编码</span>
            <n-select
              v-model:value="config.video.hw"
              :options="videoHwOptions"
              size="small"
              style="flex: 1"
            />
          </div>
          <div class="row" title="目标帧率（10–144，实际受桌面刷新率与画面变化影响）">
            <span class="label">帧率</span>
            <n-select
              v-model:value="config.video.fps"
              :options="videoFpsOptions"
              size="small"
              style="flex: 1"
            />
          </div>
          <div class="row" title="码率（Mbps）：越高画质越好、文件越大；1080p60 建议 ≥12，4K60 建议 ≥30">
            <span class="label">码率</span>
            <n-select
              v-model:value="config.video.bitrate_mbps"
              :options="videoBitrateOptions"
              size="small"
              style="flex: 1"
            />
          </div>
          <div class="row" title="音轨：系统声音 = 录制电脑播放的声音（回环）；系统+麦克风 = 两者混录">
            <span class="label">音频</span>
            <n-select
              v-model:value="config.video.audio"
              :options="videoAudioOptions"
              size="small"
              style="flex: 1"
            />
          </div>
          <div class="row" title="录制模式：全屏 = 整个显示器；追随鼠标 = 固定尺寸窗口跟随光标（钳制在显示器内，光标离屏贴边跟随）">
            <span class="label">录制模式</span>
            <n-select
              v-model:value="config.video.mode"
              :options="videoModeOptions"
              size="small"
              style="flex: 1"
            />
          </div>
          <div class="row" v-if="config.video.mode !== 'follow'" title="全屏录制的目标显示器（多显示器环境选择；主显示器 = 系统主屏）">
            <span class="label">目标显示器</span>
            <n-select
              v-model:value="config.video.monitor_index"
              :options="videoMonitorOptions"
              size="small"
              style="flex: 1"
            />
          </div>
          <div class="row" v-if="config.video.mode === 'follow'" title="追随鼠标的窗口尺寸（像素，自动偶数化并钳制到显示器内）">
            <span class="label">跟随区域</span>
            <div class="auto-stop-inputs">
              <n-input-number
                v-model:value="followW"
                size="small"
                :min="64"
                :max="4096"
                :step="64"
                style="width: 110px"
              />
              <span class="unit-label">×</span>
              <n-input-number
                v-model:value="followH"
                size="small"
                :min="64"
                :max="4096"
                :step="64"
                style="width: 110px"
              />
              <span class="unit-label">px</span>
            </div>
          </div>
          <div class="row" title="自动停止：录制时长达到上限（秒）自动保存结束；0 = 无限制。暂停期间不计时">
            <span class="label">时长上限</span>
            <div class="auto-stop-inputs">
              <n-input-number
                v-model:value="autoStopSeconds"
                size="small"
                :min="0"
                :max="86400"
                :step="60"
                style="width: 130px"
              />
              <span class="unit-label">{{ autoStopSeconds === 0 ? "不限" : "秒" }}</span>
            </div>
          </div>
          <div class="row" title="自动停止：文件大小达到上限（MB）自动保存结束；0 = 无限制">
            <span class="label">大小上限</span>
            <div class="auto-stop-inputs">
              <n-input-number
                v-model:value="autoStopMB"
                size="small"
                :min="0"
                :max="102400"
                :step="256"
                style="width: 130px"
              />
              <span class="unit-label">{{ autoStopMB === 0 ? "不限" : "MB" }}</span>
            </div>
          </div>
          <div class="row" title="分卷录制：单文件大小达到阈值（GB）自动保存并继续录到新文件（-part2 后缀）；0 = 不分卷">
            <span class="label">分卷大小</span>
            <div class="auto-stop-inputs">
              <n-input-number
                v-model:value="splitSizeGB"
                size="small"
                :min="0"
                :max="1024"
                :step="1"
                style="width: 130px"
              />
              <span class="unit-label">{{ splitSizeGB === 0 ? "不分卷" : "GB" }}</span>
            </div>
          </div>
          <div class="row" title="静默自动停止：持续无声达到秒数后弹倒计时，期间声音恢复自动取消续录，倒计时结束保存停止；0 = 关闭。适合会议/网课（录完忘了停的场景）">
            <span class="label">静默停止</span>
            <div class="auto-stop-inputs">
              <n-input-number
                v-model:value="silenceStopSeconds"
                size="small"
                :min="0"
                :max="3600"
                :step="30"
                style="width: 130px"
              />
              <span class="unit-label">{{ silenceStopSeconds === 0 ? "关闭" : "秒" }}</span>
            </div>
          </div>
          <div class="row" v-if="silenceStopSeconds > 0" title="静默触发后的倒计时秒数：倒计时内声音恢复则取消停止继续录制">
            <span class="label">停止倒计时</span>
            <div class="auto-stop-inputs">
              <n-input-number
                v-model:value="silenceCountdownSeconds"
                size="small"
                :min="1"
                :max="120"
                :step="1"
                style="width: 130px"
              />
              <span class="unit-label">秒</span>
            </div>
          </div>
          <div class="row" title="录制画面叠加鼠标指针：屏幕采集（DDA）不含鼠标——教程/演示录制建议开启">
            <span class="label">鼠标指针</span>
            <n-switch v-model:value="config.video.mouse_cursor" size="small" />
            <span class="row-hint-inline">画面中显示鼠标指针</span>
          </div>
          <div class="row" title="录制画面叠加点击效果：左键 = 青色扩散圈 / 右键 = 红色扩散圈（0.5 秒）">
            <span class="label">点击效果</span>
            <n-switch v-model:value="config.video.mouse_click" size="small" />
            <span class="row-hint-inline">点击时显示扩散圈</span>
          </div>
          <div class="row" title="录制画面叠加高亮效果：点击位置显示半透明色块（1 秒渐隐）——重点标注操作位置">
            <span class="label">高亮效果</span>
            <n-switch v-model:value="config.video.mouse_highlight" size="small" />
            <span class="row-hint-inline">点击位置显示高亮块</span>
          </div>
          <template v-if="config.video.mouse_highlight">
            <div class="row" title="高亮块颜色（#RRGGBB）">
              <span class="label">高亮颜色</span>
              <n-color-picker
                v-model:value="config.video.mouse_highlight_color"
                size="small"
                :show-alpha="false"
                :modes="['hex']"
                style="flex: 1"
              />
            </div>
            <div class="row" title="高亮块边长（像素，以点击位置为中心）">
              <span class="label">高亮大小</span>
              <div class="auto-stop-inputs">
                <n-input-number
                  v-model:value="config.video.mouse_highlight_size"
                  size="small"
                  :min="16"
                  :max="300"
                  :step="20"
                  style="width: 130px"
                />
                <span class="unit-label">px</span>
              </div>
            </div>
          </template>
          <div class="row" title="水印文字：录制画面叠加文字（留空关闭）。{ts} = 录制时间戳（每秒更新）；换行 = 多条；白字黑描边">
            <span class="label">水印文字</span>
            <n-input
              v-model:value="config.video.watermark_text"
              size="small"
              placeholder="留空关闭；{ts} = 时间戳"
              style="flex: 1"
            />
          </div>
          <div class="row" title="水印图片：录制画面叠加 PNG 图片（留空关闭；与水印文字共用位置/不透明度/边距，图片在上文字在下）">
            <span class="label">水印图片</span>
            <n-input
              v-model:value="config.video.watermark_image"
              size="small"
              placeholder="PNG 路径（留空关闭）"
              style="flex: 1"
              clearable
            />
          </div>
          <template v-if="config.video.watermark_text !== '' || config.video.watermark_image.trim() !== ''">
            <div class="row" title="水印九宫格定位（相对输出画面）">
              <span class="label">水印位置</span>
              <n-select
                v-model:value="config.video.watermark_pos"
                :options="watermarkPosOptions"
                size="small"
                style="flex: 1"
              />
            </div>
            <div class="row" title="水印不透明度（1-100%）">
              <span class="label">不透明度</span>
              <div class="auto-stop-inputs">
                <n-input-number
                  v-model:value="config.video.watermark_opacity"
                  size="small"
                  :min="1"
                  :max="100"
                  :step="10"
                  style="width: 130px"
                />
                <span class="unit-label">%</span>
              </div>
            </div>
            <div class="row" title="水印与画面边缘的距离（像素）">
              <span class="label">水印边距</span>
              <div class="auto-stop-inputs">
                <n-input-number
                  v-model:value="config.video.watermark_margin"
                  size="small"
                  :min="0"
                  :max="200"
                  :step="4"
                  style="width: 130px"
                />
                <span class="unit-label">px</span>
              </div>
            </div>
            <div class="row" v-if="config.video.watermark_text !== ''" title="水印文字字号（像素）">
              <span class="label">水印字号</span>
              <div class="auto-stop-inputs">
                <n-input-number
                  v-model:value="config.video.watermark_font_size"
                  size="small"
                  :min="12"
                  :max="72"
                  :step="2"
                  style="width: 130px"
                />
                <span class="unit-label">px</span>
              </div>
            </div>
          </template>
          <div class="row" title="摄像头画中画：录制画面叠加摄像头画面（教程/游戏解说）。设备打开失败时录制正常继续（仅无叠加）">
            <span class="label">摄像头</span>
            <n-select
              v-model:value="config.video.camera_device"
              :options="videoCameraOptions"
              size="small"
              style="flex: 1"
            />
          </div>
          <template v-if="config.video.camera_device !== ''">
            <div class="row" title="摄像头画中画九宫格定位（相对输出画面）">
              <span class="label">摄像头位置</span>
              <n-select
                v-model:value="config.video.camera_pos"
                :options="watermarkPosOptions"
                size="small"
                style="flex: 1"
              />
            </div>
            <div class="row" title="画中画显示宽（像素，高按摄像头宽高比自适应；0 = 自适应 = 输出宽 1/4）">
              <span class="label">画面宽度</span>
              <div class="auto-stop-inputs">
                <n-input-number
                  v-model:value="config.video.camera_width"
                  size="small"
                  :min="0"
                  :max="1920"
                  :step="40"
                  style="width: 130px"
                />
                <span class="unit-label">px</span>
              </div>
            </div>
            <div class="row" title="画中画与画面边缘的距离（像素）">
              <span class="label">摄像头边距</span>
              <div class="auto-stop-inputs">
                <n-input-number
                  v-model:value="config.video.camera_margin"
                  size="small"
                  :min="0"
                  :max="200"
                  :step="4"
                  style="width: 130px"
                />
                <span class="unit-label">px</span>
              </div>
            </div>
            <div class="row" title="水平翻转（前置摄像头镜像习惯：画面左右镜像）">
              <span class="label">水平翻转</span>
              <n-switch v-model:value="config.video.camera_flip_h" size="small" />
              <span class="row-hint-inline">左右镜像</span>
            </div>
            <div class="row" title="色度键抠像：将纯色背景（默认绿色）透明化——绿幕/纯色墙场景">
              <span class="label">色度键抠像</span>
              <n-switch v-model:value="config.video.camera_chroma_key" size="small" />
              <span class="row-hint-inline">纯色背景透明</span>
            </div>
            <template v-if="config.video.camera_chroma_key">
              <div class="row" title="键控颜色（#RRGGBB）：与摄像头背景色一致的颜色被抠除">
                <span class="label">键控颜色</span>
                <n-color-picker
                  v-model:value="config.video.camera_key_color"
                  size="small"
                  :show-alpha="false"
                  :modes="['hex']"
                  style="flex: 1"
                />
              </div>
              <div class="row" title="相似度（0-100）：越大抠除的颜色范围越广（过高会误抠人像边缘）">
                <span class="label">相似度</span>
                <div class="auto-stop-inputs">
                  <n-input-number
                    v-model:value="config.video.camera_similarity"
                    size="small"
                    :min="0"
                    :max="100"
                    :step="5"
                    style="width: 130px"
                  />
                  <span class="unit-label">%</span>
                </div>
              </div>
            </template>
          </template>
          <div class="row" title="录制完成动作：录制正常结束（停止/自动停止）后自动执行。取消录制不触发">
            <span class="label">完成动作</span>
            <n-select
              v-model:value="config.video.complete_action"
              :options="completeActionOptions"
              size="small"
              style="flex: 1"
            />
          </div>
          <div class="row" v-if="config.video.complete_action === 'shutdown'" title="关机倒计时（秒）：停止录制后弹提示，倒计时内可点击取消">
            <span class="label">关机倒计时</span>
            <div class="auto-stop-inputs">
              <n-input-number
                v-model:value="config.video.shutdown_countdown_seconds"
                size="small"
                :min="5"
                :max="600"
                :step="15"
                style="width: 130px"
              />
              <span class="unit-label">s</span>
            </div>
          </div>
          <div class="row" title="计划录制：到达开始时间自动开始录制（无会话时），到结束时间自动停止。后台每 30 秒检查一次；应用需保持运行">
            <span class="label">计划录制</span>
            <n-switch v-model:value="config.video.schedule_enabled" size="small" />
            <span class="row-hint-inline">按计划自动开始/停止</span>
          </div>
          <template v-if="config.video.schedule_enabled">
            <div class="row" title="重复方式：一次（触发后计划自动关闭）/ 每天 / 每周（按勾选的星期）">
              <span class="label">重复方式</span>
              <n-select
                v-model:value="config.video.schedule_repeat"
                :options="scheduleRepeatOptions"
                size="small"
                style="flex: 1"
              />
            </div>
            <div class="row" v-if="config.video.schedule_repeat === 'weekly'" title="每周触发的星期（多选）">
              <span class="label">触发星期</span>
              <n-select
                v-model:value="scheduleWeekdays"
                :options="weekdayOptions"
                size="small"
                multiple
                style="flex: 1"
              />
            </div>
            <div class="row" title="计划开始时间（HH:MM，24 小时制）">
              <span class="label">开始时间</span>
              <n-input
                v-model:value="config.video.schedule_start"
                size="small"
                placeholder="HH:MM"
                style="width: 130px"
              />
              <span class="row-hint-inline">到点自动开始</span>
            </div>
            <div class="row" title="计划结束时间（HH:MM）——到点自动停止录制；早于开始时间 = 跨天（+24 小时）">
              <span class="label">结束时间</span>
              <n-input
                v-model:value="config.video.schedule_end"
                size="small"
                placeholder="HH:MM"
                style="width: 130px"
              />
              <span class="row-hint-inline">到点自动停止</span>
            </div>
          </template>
          <div class="row" title="视频组件（FFmpeg 动态库）探测结果：缺 DLL 时录制/播放不可用，但不影响截图/看图/动图">
            <span class="label">视频组件</span>
            <span class="readonly-value" :class="{ 'video-comp-ok': videoComponent?.available, 'video-comp-bad': videoComponent && !videoComponent.available }">
              {{ videoComponent ? (videoComponent.available ? `已就绪（${videoComponent.version ?? "FFmpeg"}）` : "未安装") : "检测中…" }}
            </span>
          </div>
          <div v-if="videoComponent && !videoComponent.available" class="record-sub">
            视频组件不可用：{{ videoComponent.message }}。需在程序目录 ffmpeg\bin 放置 FFmpeg 运行库。
          </div>
          <div class="record-hint">
            视频录制输出 MKV（崩溃安全容器）· 全屏 · 保存到截图目录（video_时间戳.mkv）。
            时长/大小上限设为 0 即不限；录制中可随时暂停（面板按钮或再按 Alt+F7）。
            入口在主面板「录制视频」弹窗；录制参数也可在弹窗内临时调整（保存后以本页为准）。
            播放器键位：空格=暂停 · 双击/F=全屏 · ←→=±5s · ↑↓=音量 · S=截图 · 数字=跳书签。
            HDR 显示器自动走 HDR10 直录（HEVC main10 + BT.2020/PQ，HDR 屏以真实物理亮度回放）。
          </div>
          <div class="record-hint" title="FFmpeg 动态链接的 LGPL 2.1+ 合规声明">
            本产品视频功能使用 FFmpeg（ffmpeg.org），依 LGPL 2.1+ 授权动态链接；
            许可证文本随附于程序目录 ffmpeg\LICENSE.txt，源码可从 ffmpeg.org 获取，DLL 可自行替换同版本。
          </div>
        </div>

        <!-- 显示器 -->
        <div v-else-if="activeSection === 'display'" class="group">
          <div class="group-title">显示器</div>
          <div class="row">
            <span class="label">静默截图显示器</span>
            <n-select
              v-model:value="config.silent_monitor"
              :options="silentMonitorOptions"
              size="small"
              style="flex: 1"
            />
          </div>

          <!-- 面板色域（自动识别，只读） -->
          <div class="gamut-card">
            <div class="gamut-head">
              <span class="gamut-title">面板色域</span>
              <span class="gamut-mode-badge" title="色域由系统 WinRT AdvancedColorInfo 自动识别，无需手动配置；识别结果用于 HDR 徽标与色彩管理">自动识别</span>
              <n-button
                size="tiny"
                quaternary
                :loading="gamutLoading"
                @click="loadGamut"
              >
                重新检测
              </n-button>
            </div>
            <div v-if="panelGamut" class="gamut-grid">
              <div class="gamut-item wide" title="该显示器输出所在的显卡（DXGI 适配器枚举），DDA 捕获与 HDR 渲染均在此 GPU 上执行">
                <span class="gi-label">输出显卡</span>
                <span class="gi-value">{{ panelGamut.adapter_name || "—" }}</span>
              </div>
              <div class="gamut-item wide" title="EDID 厂商标称显示器名（基础块 0xFC 描述符）">
                <span class="gi-label">显示器</span>
                <span class="gi-value">{{ panelGamut.monitor_name || "—" }}</span>
              </div>
              <div class="gamut-item">
                <span class="gi-label">色域分类</span>
                <span class="gi-value primary">{{ panelGamut.name || "未识别" }}</span>
              </div>
              <div class="gamut-item" title="数据来源：EDID = 厂商标称（注册表 EDID 解析）；WinRT/DXGI = 系统/驱动实测">
                <span class="gi-label">数据来源</span>
                <span class="gi-value">{{ panelGamut.source || "—" }}</span>
              </div>
              <div class="gamut-item">
                <span class="gi-label">DCI-P3 覆盖</span>
                <span class="gi-value">{{ fmtCoverage(panelGamut.p3_coverage) }}</span>
              </div>
              <div class="gamut-item">
                <span class="gi-label">BT.2020 覆盖</span>
                <span class="gi-value">{{ fmtCoverage(panelGamut.bt2020_coverage) }}</span>
              </div>
              <div class="gamut-item" title="厂商标称峰值（EDID CTA-861 HDR 静态元数据）：面板宣称的最大亮度，与其它软件的『峰值亮度』口径一致；『未标称』= 该面板 EDID 未发布 HDR 元数据（笔记本内屏常见）">
                <span class="gi-label">标称峰值</span>
                <span class="gi-value primary">{{ panelGamut.edid_max_luminance ? Math.round(panelGamut.edid_max_luminance) + " nits" : "未标称" }}</span>
              </div>
              <div class="gamut-item" title="驱动实测峰值（DXGI MaxLuminance）：当前模式下 HDR 高光能实际点亮的亮度上限，驱动可能按保守值报告">
                <span class="gi-label">实测峰值</span>
                <span class="gi-value">{{ Math.round(panelGamut.measured_max_luminance || panelGamut.max_luminance) }} nits</span>
              </div>
              <div class="gamut-item" title="全屏持续亮度（MaxFullFrameLuminance）：整屏全白能维持的亮度，恒低于峰值">
                <span class="gi-label">全屏亮度</span>
                <span class="gi-value">{{ Math.round(panelGamut.max_full_frame_luminance) }} nits</span>
              </div>
              <div class="gamut-item">
                <span class="gi-label">SDR 白电平</span>
                <span class="gi-value">{{ sdrWhiteNits ? Math.round(sdrWhiteNits) + " nits" : "—" }}</span>
              </div>
            </div>
            <div v-else class="gamut-empty">
              {{ gamutLoading ? "正在识别…" : "未能识别（系统未报告面板色域信息）" }}
            </div>
            <div class="gamut-note">
              色域自动识别（只读）：实测值来自 WinRT/DXGI，标称值来自 EDID 解析（厂商宣称，通常高于驱动实测）；
              覆盖率按色度图面积近似（≈），与逐像素法可能有数个百分点差异；全屏亮度恒低于峰值（面板物理特性）
            </div>
          </div>

          <div class="hint-text">HDR 开关与屏幕亮度请在标题栏的显示器面板中调节；色调映射参数在「画质」分区</div>
        </div>

        <!-- OCR -->
        <div v-else-if="activeSection === 'ocr'" class="group">
          <div class="group-title">OCR</div>
          <div class="row">
            <span class="label">识别语言</span>
            <n-select
              v-model:value="config.ocr_language"
              :options="ocrLangOptions"
              size="small"
              style="flex: 1"
            />
          </div>
          <div class="hint-text">标注模式中使用「文字识别」工具后，可框选识别出的文本并 Ctrl+C 复制</div>
        </div>

        <!-- 后台运行 -->
        <div v-else-if="activeSection === 'background'" class="group">
          <div class="group-title">后台运行</div>
          <div class="switch-row">
            <span>最小化到托盘</span>
            <n-switch v-model:value="config.minimize_to_tray" />
          </div>
          <div class="switch-row">
            <span>开机自启</span>
            <n-switch v-model:value="config.autostart" />
          </div>
          <div class="switch-row">
            <span>缩略图预热
              <span class="row-hint-inline">启动后后台生成，机械硬盘建议关闭</span>
            </span>
            <n-switch v-model:value="config.viewer.thumb_prewarm" />
          </div>
          <div class="hint-text">
            <n-icon :component="DeviceFloppy" size="12" />
            关闭主窗口时保留托盘运行，右键托盘图标可退出
          </div>
          <div class="hint-text">
            缩略图预热在启动 2 秒后后台解码相册填充缓存，已看过的图滚动浏览零延迟；
            机械硬盘/低配机上会与启动争抢磁盘 IO 建议关闭（关闭后首次浏览按需生成），改动重启应用后生效
          </div>
        </div>

        <!-- 系统集成：文件关联 + 右键菜单 -->
        <div v-else-if="activeSection === 'system'" class="group">
          <div class="group-title">系统集成</div>
          <div class="switch-row">
            <span>文件关联 + 资源管理器右键菜单</span>
            <n-switch :value="fileAssoc" :loading="fileAssocBusy" @update:value="onFileAssocChange" />
          </div>
          <div class="switch-row">
            <span>设为默认图片 / 视频应用
              <span v-if="defaultCount" class="assoc-count">
                （{{ defaultCount.set }}/{{ defaultCount.total }} 个格式）
              </span>
            </span>
            <n-button size="tiny" :loading="defaultBusy" @click="makeDefault">
              一键设置全部…
            </n-button>
          </div>

          <!-- 逐格式归属明细：本应用高亮，其余显示当前处理方 -->
          <div v-if="assocDetail.length > 0" class="assoc-grid">
            <div
              v-for="a in assocDetail"
              :key="a.ext"
              class="assoc-chip"
              :class="{ ours: a.ours }"
              :title="a.progId ? `${a.ext} · 当前默认：${a.owner}（${a.progId}）` : `${a.ext} · 未设置默认`"
            >
              <span class="assoc-ext">{{ a.ext.toUpperCase() }}</span>
              <span class="assoc-owner ellip">{{ a.ours ? "本应用" : a.owner ?? "未设置" }}</span>
            </div>
          </div>
          <div v-if="assocDetail.length > 0 && defaultCount && defaultCount.set < defaultCount.total" class="hint-text">
            <n-icon :component="ShieldCheck" size="12" />
            灰色格式的当前默认为其它应用；点上方「一键设置全部」在系统确认页一次勾选全部格式
          </div>

          <div class="hint-text">
            <n-icon :component="Plug" size="12" />
            注册到当前用户（无需管理员）：图片/视频「打开方式」与 Win11 默认应用列表可见；右键图片/视频/文件夹直达打开、播放、隐私相册、相册浏览
          </div>
          <div class="hint-text">
            <n-icon :component="ShieldCheck" size="12" />
            Windows 规定默认程序须经系统界面确认（防劫持），确认页一次勾选全部格式；Win11 右键菜单项位于「显示更多选项」
          </div>
        </div>

        <!-- 配置管理 -->
        <div v-else-if="activeSection === 'config'" class="group">
          <div class="group-title">配置管理</div>
          <div class="config-actions">
            <button class="config-btn" @click="backupNow">
              <n-icon :component="Database" size="18" />
              <span class="config-btn-title">备份配置</span>
              <span class="config-btn-desc">保存到 AppData 备份目录</span>
            </button>
            <button class="config-btn" @click="exportNow">
              <n-icon :component="FileExport" size="18" />
              <span class="config-btn-title">导出配置</span>
              <span class="config-btn-desc">另存为 TOML 文件到任意位置</span>
            </button>
            <button class="config-btn" @click="importNow">
              <n-icon :component="FileImport" size="18" />
              <span class="config-btn-title">导入配置</span>
              <span class="config-btn-desc">从 TOML 文件恢复全部设置</span>
            </button>
          </div>
          <div class="hint-text">导入后立即生效：热键自动重注册，主题与主题色同步应用</div>
        </div>

        <!-- 内存清理（对照设计文档 4.2.3 布局图） -->
        <div v-else-if="activeSection === 'memory'" class="group">
          <div class="group-title">内存清理</div>

          <!-- ① 大数字卡：百分比大字 + 已用/共 + 分级变色进度条 -->
          <div class="mem-hero" :class="memLevel">
            <div class="mem-percent">{{ memPercentText }}</div>
            <div class="mem-used">
              已用 {{ formatBytes(memInfo?.physical.used) }} / 共
              {{ formatBytes(memInfo?.physical.total) }}
            </div>
            <div class="mem-bar">
              <div
                class="mem-bar-fill"
                :style="{
                  width: (memInfo
                    ? Math.min(100, Math.max(0, memInfo.physical.percent))
                    : 0) + '%',
                }"
              ></div>
            </div>
          </div>

          <!-- ② 操作：立即清理（危险项先二次确认）+ 只清自己 -->
          <div class="mem-actions">
            <button class="mem-primary-btn" :disabled="cleaning" @click="cleanNow">
              <n-icon :component="Wand" size="16" />
              <span>{{ cleaning ? "清理中…" : "立即清理" }}</span>
            </button>
            <button class="mem-own-btn" @click="cleanOwn">
              <n-icon :component="Brush" size="14" />
              <span>只清自己</span>
            </button>
          </div>
          <div class="hint-text">
            <n-icon :component="AlertTriangle" size="12" />
            「只清自己」仅收缩本进程工作集，无需管理员权限，效果立竿见影
          </div>

          <!-- ③ 内存详情（等价 memreduct「内存信息」组） -->
          <div class="sub-title">内存详情</div>
          <div class="mem-detail-grid">
            <div class="mem-detail-row">
              <span>物理内存</span>
              <span>
                {{ formatBytes(memInfo?.physical.used) }} /
                {{ formatBytes(memInfo?.physical.total) }}（{{ memPercentText }}）
              </span>
            </div>
            <div class="mem-detail-row">
              <span>系统缓存</span>
              <span>{{ formatBytes(memInfo?.system_cache) }}</span>
            </div>
            <div class="mem-detail-row">
              <span>备用列表</span>
              <span>{{ formatBytes(memInfo?.standby) }}</span>
            </div>
            <div class="mem-detail-row">
              <span>已修改页</span>
              <span>{{ formatBytes(memInfo?.modified) }}</span>
            </div>
            <div class="mem-detail-row">
              <span>提交内存</span>
              <span>
                {{ formatBytes(memInfo?.commit.used) }} /
                {{ formatBytes(memInfo?.commit.total) }}
              </span>
            </div>
            <div class="mem-detail-row">
              <span>页面池</span>
              <span>{{ formatBytes(memInfo?.paged_pool) }}</span>
            </div>
            <div class="mem-detail-row">
              <span>非页面池</span>
              <span>{{ formatBytes(memInfo?.non_paged_pool) }}</span>
            </div>
          </div>

          <!-- ④ 清理区域：8 项两列勾选（危险项豆沙红 ✱ + tooltip） -->
          <div class="sub-title">清理区域</div>
          <div class="region-grid">
            <div
              v-for="r in CLEAN_REGIONS"
              :key="r.mask"
              class="region-item"
              :class="{ danger: r.danger }"
            >
              <n-tooltip v-if="r.danger" trigger="hover" placement="top">
                <template #trigger>
                  <n-checkbox
                    :checked="regionChecked(r.mask)"
                    @update:checked="(v: boolean) => toggleRegion(r.mask, v)"
                  >
                    {{ r.label }} <span class="danger-star">✱</span>
                  </n-checkbox>
                </template>
                危险项：{{ r.tip }}
              </n-tooltip>
              <n-checkbox
                v-else
                :checked="regionChecked(r.mask)"
                @update:checked="(v: boolean) => toggleRegion(r.mask, v)"
              >
                {{ r.label }}
                <span v-if="r.req" class="region-req">{{ r.req }}</span>
              </n-checkbox>
            </div>
          </div>
          <div class="hint-text">
            <n-icon :component="AlertTriangle" size="12" />
            ✱ 危险项：清后短期内系统可能变卡 / 强制写回磁盘
          </div>

          <!-- ⑤ 自动清理（未提权时禁用） -->
          <div class="sub-title">自动清理</div>
          <div class="switch-row">
            <span>阈值触发</span>
            <n-switch
              v-model:value="config.memory_clean.auto_enable"
              :disabled="!elevated"
            />
          </div>
          <div class="row">
            <span class="label">清理阈值</span>
            <n-slider
              v-model:value="config.memory_clean.threshold_percent"
              :min="50"
              :max="100"
              :step="1"
              :disabled="!elevated"
              :format-tooltip="(v: number) => `${v}%`"
              style="flex: 1"
            />
            <span class="mem-num">{{ config.memory_clean.threshold_percent }}%</span>
          </div>
          <div class="switch-row">
            <span>间隔触发</span>
            <n-switch
              v-model:value="config.memory_clean.interval_enable"
              :disabled="!elevated"
            />
          </div>
          <div class="row">
            <span class="label">清理间隔</span>
            <n-input-number
              v-model:value="config.memory_clean.interval_minutes"
              :min="1"
              :max="1440"
              :disabled="!elevated"
              size="small"
              style="width: 130px"
            >
              <template #suffix>分钟</template>
            </n-input-number>
          </div>
          <div class="row">
            <span class="label">清理冷却</span>
            <n-input-number
              v-model:value="config.memory_clean.cooldown_seconds"
              :min="10"
              :max="3600"
              :disabled="!elevated"
              size="small"
              style="width: 130px"
            >
              <template #suffix>秒</template>
            </n-input-number>
          </div>
          <div class="hint-text">
            <n-icon :component="AlertTriangle" size="12" />
            {{ elevated
              ? "两开关均开时任一满足即清理；两次清理至少间隔冷却秒数"
              : "自动清理需要管理员权限，请先以管理员身份重启" }}
          </div>

          <!-- ⑥ 通知 / 日志 -->
          <div class="switch-row">
            <span>清理完成时通知我</span>
            <n-switch v-model:value="config.memory_clean.notify_enable" />
          </div>
          <div class="switch-row">
            <span>清理结果写入日志</span>
            <n-switch v-model:value="config.memory_clean.log_results" />
          </div>

          <!-- ⑦ 统计 -->
          <div class="sub-title">统计</div>
          <div class="mem-detail-grid">
            <div class="mem-detail-row">
              <span>最近清理</span>
              <span :title="formatClock(statsView.last_clean_ts)">
                {{ formatClock(statsView.last_clean_ts) }}（已释放
                {{ formatBytes(statsView.last_freed_bytes) }}）
              </span>
            </div>
            <div class="mem-detail-row">
              <span>累计清理</span>
              <span>
                {{ statsView.total_clean_count }} 次 · 累计释放
                {{ formatBytes(statsView.total_freed_bytes) }}
              </span>
            </div>
          </div>

          <!-- ⑧ 权限状态徽标 + 以管理员身份重启 -->
          <div class="mem-perm">
            <span class="mem-perm-badge" :class="{ elevated }">
              <n-icon :component="ShieldCheck" size="13" />
              {{ elevated ? "管理员权限" : "普通权限" }}
            </span>
            <n-button v-if="!elevated" size="small" @click="restartElevated">
              以管理员身份重启
            </n-button>
          </div>
        </div>
      </main>
    </div>

    <!-- 保存为预设弹窗（画质分区） -->
    <div v-if="showPresetModal" class="tm-modal-mask" @click.self="showPresetModal = false">
      <div class="tm-modal">
        <div class="tm-modal-title">保存为自定义预设</div>
        <div class="tm-modal-desc">
          将当前「{{ activePreset.name }}」+ 高级调整的参数组合保存为独立预设
        </div>
        <n-input
          v-model:value="presetName"
          size="small"
          placeholder="预设名称（如：夜间看图）"
          style="width: 100%"
          @keyup.enter="saveAsPreset"
        />
        <div class="tm-modal-actions">
          <n-button size="small" quaternary @click="showPresetModal = false">取消</n-button>
          <n-button size="small" type="primary" @click="saveAsPreset">保存</n-button>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.settings-root {
  display: flex;
  flex-direction: column;
  height: 100vh;
  overflow: hidden;
  background: var(--jb-bg);
  /* GPU 空转治理：全窗口 blur 40→16（合成成本约降 60%，视觉近似） */
  backdrop-filter: blur(16px) saturate(1.2);
  position: relative;
}

/* === 极淡主题色极光晕染（柔光滤镜，不抢内容） === */
.settings-root::before {
  content: "";
  position: absolute;
  inset: 0;
  pointer-events: none;
  z-index: 0;
  background:
    radial-gradient(
      46% 38% at 14% 6%,
      color-mix(in srgb, var(--jb-primary) 11%, transparent),
      transparent 72%
    ),
    radial-gradient(
      40% 34% at 90% 98%,
      color-mix(in srgb, var(--jb-primary) 9%, transparent),
      transparent 72%
    );
  /* GPU 空转治理：停用 aurora-breathe 呼吸动画——可见窗口的 infinite 动画在
     集显办公机上持续产生合成成本；静态渐变保留（Mica 质感不依赖动画） */
}

.settings-body {
  flex: 1;
  display: flex;
  min-height: 0;
  position: relative;
  z-index: 1;
}

/* === 侧导航 === */
.settings-nav {
  width: 108px;
  flex-shrink: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding: 10px 8px;
  border-right: 1px solid var(--jb-divider);
  overflow-y: auto;
}
.nav-item {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 10px;
  border: none;
  border-radius: 8px;
  background: transparent;
  color: var(--jb-text-soft);
  font-size: 12px;
  cursor: pointer;
  transition: background-color 0.15s, color 0.15s;
  text-align: left;
}
.nav-item:hover {
  background: var(--jb-titlebar-btn-hover);
  color: var(--jb-text);
}
.nav-item.active {
  background: color-mix(in srgb, var(--jb-primary) 16%, transparent);
  color: var(--jb-primary);
  font-weight: 600;
}
.nav-item.active .n-icon {
  color: var(--jb-primary);
}
.nav-footer {
  margin-top: auto;
  padding: 8px 10px 2px;
  display: flex;
  align-items: center;
  justify-content: space-between;
  font-size: 11px;
  color: var(--jb-text-mute);
}
/* QQ 交流群：群号一行，点击复制（淡青蓝，与主面板 chip 同色系） */
.nav-qq {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 2px 7px;
  border: 1px solid color-mix(in srgb, #6aa8b8 35%, transparent);
  border-radius: 999px;
  background: color-mix(in srgb, #6aa8b8 8%, transparent);
  color: #6aa8b8;
  font-size: 10.5px;
  font-variant-numeric: tabular-nums;
  cursor: pointer;
  white-space: nowrap;
  transition: color 0.15s, background-color 0.15s, border-color 0.15s;
}
.nav-qq:hover {
  color: #7fbccb;
  border-color: color-mix(in srgb, #6aa8b8 55%, transparent);
  background: color-mix(in srgb, #6aa8b8 14%, transparent);
}

/* === 内容区 === */
.settings-content {
  flex: 1;
  min-width: 0;
  overflow-y: auto;
  padding: 14px 16px 18px;
  position: relative;
}
.settings-content::-webkit-scrollbar {
  width: 6px;
}
.settings-content::-webkit-scrollbar-thumb {
  background: var(--jb-scrollbar);
  border-radius: 3px;
}

.save-hint {
  position: sticky;
  top: -14px;
  z-index: 5;
  display: flex;
  align-items: center;
  gap: 4px;
  justify-content: flex-end;
  margin: -14px -16px 6px;
  padding: 6px 16px;
  font-size: 11px;
  color: var(--jb-text-mute);
  background: color-mix(in srgb, var(--jb-bg) 82%, transparent);
  backdrop-filter: blur(12px);
  transition: color 0.2s;
}
.save-hint.saved {
  color: var(--jb-mint);
}
.save-hint.error {
  color: var(--jb-red);
}

.group {
  display: flex;
  flex-direction: column;
  gap: 12px;
  animation: jb-group-in 220ms ease-out;
}
@keyframes jb-group-in {
  from {
    opacity: 0;
    transform: translateY(6px);
  }
  to {
    opacity: 1;
    transform: translateY(0);
  }
}
.group-title {
  font-size: 15px;
  font-weight: 600;
  color: var(--jb-text);
}
.sub-title {
  font-size: 12px;
  font-weight: 500;
  color: var(--jb-text-soft);
  margin-top: 2px;
}
.hint-text {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 11px;
  color: var(--jb-text-mute);
  line-height: 1.5;
}

/* ====== 面板色域信息卡（自动识别，只读） ====== */
.gamut-card {
  display: flex;
  flex-direction: column;
  gap: 10px;
  margin: 4px 0 2px;
  padding: 12px 14px;
  border: 1px solid var(--jb-border);
  border-radius: 12px;
  background: color-mix(in srgb, var(--jb-bg-card) 88%, transparent);
}
.gamut-head {
  display: flex;
  align-items: center;
  gap: 8px;
}
.gamut-title {
  font-size: 12px;
  font-weight: 600;
  color: var(--jb-text);
}
.gamut-mode-badge {
  padding: 1px 8px;
  border-radius: 999px;
  font-size: 10px;
  font-weight: 600;
  color: var(--jb-primary);
  background: color-mix(in srgb, var(--jb-primary) 14%, transparent);
  border: 1px solid color-mix(in srgb, var(--jb-primary) 30%, transparent);
}
.gamut-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(128px, 1fr));
  gap: 8px 14px;
}
.gamut-item {
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.gamut-item.wide {
  grid-column: span 2;
  min-width: 0;
}
.gamut-item.wide .gi-value {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.gi-label {
  font-size: 10px;
  color: var(--jb-text-mute);
}
.gi-value {
  font-size: 13px;
  font-weight: 600;
  color: var(--jb-text);
  font-variant-numeric: tabular-nums;
}
.gi-value.primary {
  color: var(--jb-primary);
}
.gamut-empty {
  font-size: 12px;
  color: var(--jb-text-mute);
  padding: 6px 0;
}
.gamut-note {
  font-size: 11px;
  color: var(--jb-text-mute);
  line-height: 1.5;
}

/* 默认应用回显计数（系统集成组） */
.assoc-count {
  font-size: 11px;
  color: var(--jb-text-mute);
  margin-left: 6px;
}

/* === 逐格式关联明细 chips === */
.assoc-grid {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  margin: 2px 0 10px;
}
.assoc-chip {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 3px 9px;
  border-radius: 999px;
  border: 1px solid var(--jb-border);
  background: var(--jb-bg-card);
  font-size: 10.5px;
  color: var(--jb-text-mute);
  cursor: default;
  max-width: 170px;
}
.assoc-ext {
  font-weight: 600;
  letter-spacing: 0.5px;
  flex-shrink: 0;
}
.assoc-owner {
  max-width: 110px;
}
/* 本应用默认：主题色高亮 */
.assoc-chip.ours {
  border-color: color-mix(in srgb, var(--jb-primary) 40%, transparent);
  background: color-mix(in srgb, var(--jb-primary) 10%, transparent);
  color: var(--jb-primary);
}

/* === 通用行 === */
.row {
  display: flex;
  align-items: center;
  gap: 10px;
}
.label {
  width: 88px;
  flex-shrink: 0;
  font-size: 12px;
  color: var(--jb-text-soft);
}
.switch-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  font-size: 13px;
  color: var(--jb-text);
}
/* 自动停止阈值输入行（数字框 + 单位/不限标记） */
.auto-stop-inputs {
  flex: 1;
  display: flex;
  align-items: center;
  gap: 8px;
}
.unit-label {
  font-size: 12px;
  color: var(--jb-text-soft);
  min-width: 26px;
}
.row-hint-inline {
  font-size: 11px;
  color: var(--jb-text-soft);
  margin-left: 6px;
}
.bri-pair {
  display: flex;
  gap: 6px;
  flex: 1;
}

/* === 画质：色调映射预设卡片 === */
.tm-grid {
  display: grid;
  grid-template-columns: repeat(3, 1fr);
  gap: 8px;
}
.tm-card {
  position: relative;
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: 3px;
  padding: 10px 10px 9px;
  border: 1px solid var(--jb-border);
  border-radius: 10px;
  background: var(--jb-bg-card);
  cursor: pointer;
  text-align: left;
  transition: border-color 0.15s, box-shadow 0.15s, background-color 0.15s;
}
.tm-card:hover {
  background: var(--jb-bg-card-hover);
}
.tm-card.active {
  border-color: var(--jb-primary);
  box-shadow: 0 0 0 2px color-mix(in srgb, var(--jb-primary) 22%, transparent);
}
.tm-badge {
  position: absolute;
  top: -6px;
  right: 8px;
  padding: 1px 7px;
  border-radius: 999px;
  background: var(--jb-primary);
  color: #fff;
  font-size: 10px;
  line-height: 15px;
  letter-spacing: 0.5px;
}
.tm-badge-custom {
  background: var(--jb-milk);
  color: #fff;
}
.tm-name {
  font-size: 12.5px;
  font-weight: 500;
  color: var(--jb-text);
}
.tm-desc {
  font-size: 10.5px;
  color: var(--jb-text-mute);
  line-height: 1.35;
}
.tm-del {
  position: absolute;
  right: 6px;
  bottom: 6px;
  width: 20px;
  height: 20px;
  display: flex;
  align-items: center;
  justify-content: center;
  border-radius: 6px;
  color: var(--jb-text-mute);
  opacity: 0;
  transition: opacity 0.15s, color 0.15s, background-color 0.15s;
}
.tm-card:hover .tm-del {
  opacity: 1;
}
.tm-del:hover {
  color: var(--jb-red);
  background: color-mix(in srgb, var(--jb-red) 12%, transparent);
}
.tm-customized {
  margin-left: 8px;
  padding: 1px 8px;
  border-radius: 999px;
  background: color-mix(in srgb, var(--jb-primary) 16%, transparent);
  color: var(--jb-primary);
  font-size: 10.5px;
}
.tm-hint {
  margin: -4px 0 8px;
  font-size: 11px;
  color: var(--jb-text-mute);
}
.tm-reset {
  color: var(--jb-primary);
  cursor: pointer;
}
.tm-reset:hover {
  text-decoration: underline;
}
.tm-adv {
  display: flex;
  flex-direction: column;
  gap: 10px;
  padding: 12px 12px 6px;
  border: 1px solid var(--jb-border);
  border-radius: 10px;
  background: color-mix(in srgb, var(--jb-bg-card) 60%, transparent);
}
.tm-slider {
  display: flex;
  align-items: center;
  gap: 10px;
}
.tm-slider .label {
  width: 76px;
  flex-shrink: 0;
  font-size: 12px;
  color: var(--jb-text-soft);
}
.tm-slider .n-slider {
  flex: 1;
  min-width: 0;
}
/* −/+ 步进按钮：紧凑圆形，贴滑杆两端 */
.tm-step {
  width: 20px;
  height: 20px;
  flex-shrink: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  border: 1px solid var(--jb-border);
  border-radius: 50%;
  background: var(--jb-bg-card);
  color: var(--jb-text-soft);
  font-size: 13px;
  line-height: 1;
  padding: 0;
  cursor: pointer;
  transition: color 0.15s, border-color 0.15s, background-color 0.15s;
}
.tm-step:hover:not(:disabled) {
  color: var(--jb-primary);
  border-color: var(--jb-primary);
  background: var(--jb-bg-card-hover);
}
.tm-step:active:not(:disabled) {
  transform: scale(0.92);
}
.tm-step:disabled {
  opacity: 0.35;
  cursor: default;
}
.tm-val {
  width: 84px;
  flex-shrink: 0;
  text-align: right;
  font-size: 11px;
  color: var(--jb-text-mute);
  font-variant-numeric: tabular-nums;
}
/* 保存为预设弹窗 */
.tm-modal-mask {
  position: fixed;
  inset: 0;
  z-index: 100;
  display: flex;
  align-items: center;
  justify-content: center;
  background: rgba(0, 0, 0, 0.28);
  backdrop-filter: blur(6px);
}
.tm-modal {
  width: 320px;
  display: flex;
  flex-direction: column;
  gap: 10px;
  padding: 18px;
  border-radius: 12px;
  border: 1px solid var(--jb-border);
  background: var(--jb-bg-card);
  box-shadow: var(--jb-shadow);
}
.tm-modal-title {
  font-size: 13px;
  font-weight: 600;
  color: var(--jb-text);
}
.tm-modal-desc {
  font-size: 11px;
  color: var(--jb-text-mute);
  line-height: 1.4;
}
.tm-modal-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  margin-top: 2px;
}

/* === 主题色 swatch === */
.swatch-grid {
  display: grid;
  grid-template-columns: repeat(3, 1fr);
  gap: 8px;
}
.swatch {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 10px;
  border: 1px solid var(--jb-border);
  border-radius: 10px;
  background: var(--jb-bg-card);
  cursor: pointer;
  transition: border-color 0.15s, box-shadow 0.15s, background-color 0.15s;
}
.swatch:hover {
  background: var(--jb-bg-card-hover);
}
.swatch.active {
  border-color: var(--jb-primary);
  box-shadow: 0 0 0 2px color-mix(in srgb, var(--jb-primary) 22%, transparent);
}
.swatch-dot {
  width: 18px;
  height: 18px;
  border-radius: 50%;
  flex-shrink: 0;
  box-shadow: inset 0 0 0 1px rgba(0, 0, 0, 0.08);
}
.swatch-name {
  font-size: 12px;
  color: var(--jb-text);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

/* 原生取色器：圆形主题色按钮 */
.color-native {
  width: 30px;
  height: 30px;
  border-radius: 50%;
  border: 1px solid var(--jb-border);
  overflow: hidden;
  cursor: pointer;
  flex-shrink: 0;
  position: relative;
  transition: box-shadow 0.15s;
}
.color-native:hover {
  box-shadow: 0 0 0 2px color-mix(in srgb, var(--jb-primary) 25%, transparent);
}
.color-native input[type="color"] {
  position: absolute;
  inset: -8px;
  width: calc(100% + 16px);
  height: calc(100% + 16px);
  border: none;
  padding: 0;
  cursor: pointer;
  opacity: 0;
}

/* === 主面板实时预览（mini 复刻 MainView 布局，颜色全走 CSS 变量） === */
.panel-preview {
  width: 100%;
  max-width: 272px;
  margin: 0 auto;
  border: 1px solid var(--jb-border);
  border-radius: 14px;
  /* 与主面板一致的极光晕染底 */
  background:
    radial-gradient(
      46% 38% at 14% 6%,
      color-mix(in srgb, var(--jb-primary) 11%, transparent),
      transparent 72%
    ),
    radial-gradient(
      40% 34% at 90% 98%,
      color-mix(in srgb, var(--jb-primary) 9%, transparent),
      transparent 72%
    ),
    var(--jb-bg);
  box-shadow: var(--jb-shadow);
  overflow: hidden;
  user-select: none;
}
.pv-titlebar {
  display: flex;
  align-items: center;
  gap: 6px;
  height: 26px;
  padding: 0 10px;
  background: var(--jb-titlebar);
  border-bottom: 1px solid var(--jb-divider);
}
.pv-title-text {
  font-size: 10px;
  color: var(--jb-text-soft);
}
.pv-win-dots {
  margin-left: auto;
  display: flex;
  gap: 5px;
}
.pv-win-dots i {
  width: 7px;
  height: 7px;
  border-radius: 2px;
  background: var(--jb-switch-track);
}
.pv-body {
  padding: 13px 14px 11px;
  display: flex;
  flex-direction: column;
  gap: 9px;
}
.pv-greeting {
  font-size: 9px;
  color: var(--jb-text-soft);
  text-align: center;
  letter-spacing: 1px;
}
.pv-hero {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 8px;
  height: 34px;
  border-radius: 10px;
  color: #fff;
  font-size: 12px;
  font-weight: 600;
  letter-spacing: 1.5px;
  background: linear-gradient(
    135deg,
    var(--jb-primary) 0%,
    var(--jb-primary-pressed) 100%
  );
  box-shadow: 0 4px 12px color-mix(in srgb, var(--jb-primary) 40%, transparent);
}
.pv-sub-row {
  display: flex;
  gap: 8px;
}
.pv-sub {
  flex: 1;
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 5px;
  height: 24px;
  border: 1px solid var(--jb-border);
  border-radius: 8px;
  background: var(--jb-bg-card);
  color: var(--jb-text);
  font-size: 10px;
}
.pv-chips {
  display: flex;
  justify-content: center;
  flex-wrap: wrap;
  gap: 6px;
}
.pv-chip {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 3px 8px;
  border: 1px solid var(--jb-border);
  border-radius: 999px;
  background: var(--jb-bg-card);
  color: var(--jb-text-soft);
  font-size: 9px;
}
.pv-chip-settings {
  color: var(--jb-primary);
  border-color: color-mix(in srgb, var(--jb-primary) 45%, transparent);
  background: color-mix(in srgb, var(--jb-primary) 10%, transparent);
}

/* === 配置管理按钮 === */
.config-actions {
  display: grid;
  grid-template-columns: repeat(3, 1fr);
  gap: 10px;
}
.config-btn {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 6px;
  padding: 16px 10px 12px;
  border: 1px solid var(--jb-border);
  border-radius: 12px;
  background: var(--jb-bg-card);
  color: var(--jb-text);
  cursor: pointer;
  transition: border-color 0.15s, background-color 0.15s, transform 0.15s;
}
.config-btn:hover {
  background: var(--jb-bg-card-hover);
  border-color: var(--jb-primary);
  color: var(--jb-primary);
  transform: translateY(-1px);
}
.config-btn:active {
  transform: translateY(0);
}
.config-btn-title {
  font-size: 13px;
  font-weight: 600;
}
.config-btn-desc {
  font-size: 10px;
  color: var(--jb-text-mute);
  text-align: center;
  line-height: 1.4;
}

/* === 我的方案 chips === */
.scheme-list {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
}
.scheme-chip {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 5px 8px 5px 6px;
  border: 1px solid var(--jb-border);
  border-radius: 999px;
  background: var(--jb-bg-card);
  cursor: pointer;
  transition: border-color 0.15s, box-shadow 0.15s;
  max-width: 100%;
}
.scheme-chip:hover {
  border-color: var(--jb-primary);
}
.scheme-chip.active {
  border-color: var(--jb-primary);
  box-shadow: 0 0 0 2px color-mix(in srgb, var(--jb-primary) 22%, transparent);
}
.scheme-dot {
  width: 14px;
  height: 14px;
  border-radius: 50%;
  flex-shrink: 0;
  box-shadow: inset 0 0 0 1px rgba(0, 0, 0, 0.08);
}
.scheme-name {
  font-size: 12px;
  color: var(--jb-text);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  max-width: 120px;
}
.scheme-del {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 16px;
  height: 16px;
  border: none;
  border-radius: 50%;
  background: transparent;
  color: var(--jb-text-mute);
  cursor: pointer;
  flex-shrink: 0;
  transition: background-color 0.15s, color 0.15s;
}
.scheme-del:hover {
  background: var(--jb-red);
  color: #fff;
}

/* === 热键行 === */
.hotkey-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
}
.hotkey-row .label {
  width: auto;
}
/* 录制动图：只读项与说明文字 */
.readonly-value {
  color: var(--jb-text-2, inherit);
  opacity: 0.75;
  font-size: 12.5px;
}
/* 视频组件状态（FFmpeg DLL 探测结果） */
.video-comp-ok {
  color: #3a9d5d;
  opacity: 1;
}
.video-comp-bad {
  color: #d05656;
  opacity: 1;
}
.record-hint {
  margin-top: 10px;
  padding: 10px 12px;
  border-radius: 8px;
  background: color-mix(in srgb, var(--jb-primary) 7%, transparent);
  border: 1px solid color-mix(in srgb, var(--jb-primary) 18%, transparent);
  font-size: 12px;
  line-height: 1.7;
  opacity: 0.85;
}
/* 录制动图：行副文字（与 88px 标签 + 10px 间距对齐） */
.record-sub {
  margin: 2px 0 0 98px;
  font-size: 11px;
  line-height: 1.5;
  color: var(--jb-text-mute);
}
.hk-field {
  flex: 1;
  min-width: 0;
  height: 30px;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 0 12px;
  border: 1px solid var(--jb-border);
  border-radius: 8px;
  cursor: pointer;
  background: var(--jb-bg-card);
  user-select: none;
  transition: border-color 0.15s, background-color 0.15s;
}
.hk-field:hover {
  border-color: var(--jb-primary);
}
.hk-field.editing {
  border-color: var(--jb-primary);
  box-shadow: 0 0 0 2px color-mix(in srgb, var(--jb-primary) 18%, transparent);
}
.hk-value {
  font-size: 12px;
  font-weight: 600;
  color: var(--jb-text);
  font-family: Consolas, "Segoe UI", monospace;
  letter-spacing: 0.3px;
}
.hk-hint {
  font-size: 11px;
  color: var(--jb-primary);
  animation: hk-blink 1.2s ease-in-out infinite;
}
@keyframes hk-blink {
  0%, 100% { opacity: 1; }
  50% { opacity: 0.45; }
}

/* === 内存清理 === */
/* ① 大数字卡：32px 主题色大字 + 分级变色（<70 主题色 / ≥70 奶茶橘 / ≥90 豆沙红） */
.mem-hero {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 8px;
  padding: 18px 16px 16px;
  border: 1px solid var(--jb-border);
  border-radius: 14px;
  background: var(--jb-bg-card);
}
.mem-percent {
  font-size: 32px;
  font-weight: 700;
  line-height: 1.1;
  color: var(--jb-primary);
  font-variant-numeric: tabular-nums;
  transition: color 0.3s;
}
.mem-hero.warn .mem-percent {
  color: var(--jb-milk);
}
.mem-hero.danger .mem-percent {
  color: var(--jb-red);
}
.mem-used {
  font-size: 12px;
  color: var(--jb-text-soft);
}
.mem-bar {
  width: 100%;
  height: 8px;
  border-radius: 999px;
  background: var(--jb-switch-track);
  overflow: hidden;
}
.mem-bar-fill {
  height: 100%;
  border-radius: 999px;
  background: var(--jb-primary);
  transition: width 0.4s ease, background-color 0.3s;
}
.mem-hero.warn .mem-bar-fill {
  background: var(--jb-milk);
}
.mem-hero.danger .mem-bar-fill {
  background: var(--jb-red);
}

/* ② 操作按钮：主按钮主题色渐变（同 hero-btn 风格）+ 次按钮卡片风 */
.mem-actions {
  display: flex;
  gap: 10px;
}
.mem-primary-btn {
  flex: 1;
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 8px;
  height: 40px;
  border: none;
  border-radius: 12px;
  color: #fff;
  font-size: 14px;
  font-weight: 600;
  letter-spacing: 2px;
  cursor: pointer;
  background: linear-gradient(
    135deg,
    var(--jb-primary) 0%,
    var(--jb-primary-pressed) 100%
  );
  box-shadow: 0 4px 12px color-mix(in srgb, var(--jb-primary) 40%, transparent);
  transition: transform 0.16s ease, box-shadow 0.16s ease, filter 0.16s ease;
}
.mem-primary-btn:hover:not(:disabled) {
  transform: translateY(-1px);
  filter: brightness(1.05);
  box-shadow: 0 6px 16px color-mix(in srgb, var(--jb-primary) 52%, transparent);
}
.mem-primary-btn:disabled {
  opacity: 0.6;
  cursor: default;
}
.mem-own-btn {
  flex: 1;
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 7px;
  height: 40px;
  border: 1px solid var(--jb-border);
  border-radius: 12px;
  background: var(--jb-bg-card);
  color: var(--jb-text);
  font-size: 13px;
  cursor: pointer;
  transition: background-color 0.15s, border-color 0.15s, color 0.15s;
}
.mem-own-btn:hover {
  background: var(--jb-bg-card-hover);
  border-color: var(--jb-primary);
  color: var(--jb-primary);
}

/* ③/⑦ 详情与统计：两列卡片行 */
.mem-detail-grid {
  display: grid;
  grid-template-columns: repeat(2, 1fr);
  gap: 6px 14px;
  padding: 10px 12px;
  border: 1px solid var(--jb-border);
  border-radius: 10px;
  background: var(--jb-bg-card);
}
.mem-detail-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  font-size: 12px;
  min-width: 0;
}
.mem-detail-row > span:first-child {
  color: var(--jb-text-soft);
  flex-shrink: 0;
}
.mem-detail-row > span:last-child {
  color: var(--jb-text);
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

/* ④ 清理区域：两列勾选 grid（间距 8px，对照样式映射表） */
.region-grid {
  display: grid;
  grid-template-columns: repeat(2, 1fr);
  gap: 8px;
}
.region-item.danger :deep(.n-checkbox .n-checkbox__label) {
  color: var(--jb-red);
}
.danger-star {
  color: var(--jb-red);
  margin-left: 2px;
}
.region-req {
  color: var(--jb-text-mute);
  font-size: 11px;
  margin-left: 4px;
}

/* ⑤ 阈值滑杆数值 */
.mem-num {
  width: 38px;
  flex-shrink: 0;
  text-align: right;
  font-size: 12px;
  color: var(--jb-text);
  font-variant-numeric: tabular-nums;
}

/* ⑧ 权限徽标：提权后主题色高亮「管理员」 */
.mem-perm {
  display: flex;
  align-items: center;
  gap: 10px;
}
.mem-perm-badge {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  padding: 4px 10px;
  border-radius: 999px;
  border: 1px solid var(--jb-border);
  font-size: 12px;
  color: var(--jb-text-soft);
}
.mem-perm-badge.elevated {
  color: var(--jb-primary);
  border-color: color-mix(in srgb, var(--jb-primary) 45%, transparent);
  background: color-mix(in srgb, var(--jb-primary) 10%, transparent);
}
</style>
