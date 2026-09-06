<script setup lang="ts">
// 主面板（极简女生向）：大按钮 + 可点状态条 + 预览弹窗
// 配置入口全部收敛到设置中心（open-settings 事件）
import { ref, computed, onMounted, onUnmounted } from "vue";
import {
  NButton,
  NIcon,
  NModal,
  NSelect,
  NSwitch,
  NProgress,
  useMessage,
  useDialog,
} from "naive-ui";
// Tabler 线性图标（24 viewBox / stroke 2，符合规范 9.1/9.2）
import {
  Camera,
  ArrowsMaximize,
  DeviceFloppy,
  Settings,
  Photo,
  Bolt,
  Folder,
  Cpu,
  Wand,
  Video,
  PlayerStop,
  PlayerPlay,
  PlayerPause,
  Movie,
  MessageCircle,
  Adjustments,
} from "@vicons/tabler";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import type { ThemeMode } from "./useTheme";
import TitleBar from "./TitleBar.vue";
import FxPanel from "./video/FxPanel.vue";
import { config } from "./configStore";

// ====== 看图入口：选图 → 显示 viewer 窗口 → 广播打开事件 ======
const VIEWER_FILTERS = [
  {
    name: "图片",
    extensions: [
      "png", "jpg", "jpeg", "gif", "bmp", "webp", "avif",
      "tif", "tiff", "psd", "ico", "jxl", "jxr", "wdp", "exr",
    ],
  },
];

async function openInViewer() {
  try {
    const sel = await openDialog({ multiple: false, filters: VIEWER_FILTERS });
    if (typeof sel !== "string" || !sel) return;
    // 走 Rust emit_to 通道调起看图窗口（比 JS 跨窗口广播可靠）
    await invoke("open_viewer_image", { path: sel });
  } catch (e) {
    message.error("打开图片失败: " + e, { duration: 2200 });
  }
}

// ====== 打开相册入口：直接调起看图窗口相册页（不弹文件夹选择框，浏览已挂载的相册源） ======
async function openAlbum() {
  try {
    await invoke("open_viewer_library");
  } catch (e) {
    message.error("打开相册失败: " + e, { duration: 2200 });
  }
}

// ====== AI 放大入口：调起独立 AI 放大窗口（waifu2x 批量超分） ======
async function openUpscale() {
  try {
    await invoke("open_upscale_window");
  } catch (e) {
    message.error("打开 AI 放大失败: " + e, { duration: 2200 });
  }
}

// ====== 主题 props（从 App.vue 传入） ======
const props = defineProps<{
  mode: ThemeMode;
  isDark: boolean;
  setMode: (m: ThemeMode) => void;
}>();

const emit = defineEmits<{
  (e: "open-settings", section?: string): void;
}>();

const message = useMessage();
const shutdownDialog = useDialog();

// ====== 状态条显示（点击跳转设置中心对应分组） ======
const formatLabel = computed(() => {
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
  0x0d: "Enter",
  0x1b: "Esc",
  0x20: "Space",
};
for (let i = 0x41; i <= 0x5a; i++) VK_TO_LABEL[i] = String.fromCharCode(i);
for (let i = 0x30; i <= 0x39; i++) VK_TO_LABEL[i] = String.fromCharCode(i);
for (let i = 0x70; i <= 0x7b; i++) VK_TO_LABEL[i] = "F" + (i - 0x6f);

function hotkeyLabel(hk: { modifiers: number; vk: number }): string {
  const parts: string[] = [];
  if (hk.modifiers & MOD_WIN) parts.push("Win");
  if (hk.modifiers & MOD_CONTROL) parts.push("Ctrl");
  if (hk.modifiers & MOD_ALT) parts.push("Alt");
  if (hk.modifiers & MOD_SHIFT) parts.push("Shift");
  const k = VK_TO_LABEL[hk.vk] ?? `VK${hk.vk.toString(16).toUpperCase()}`;
  return parts.length ? `${parts.join("+")}+${k}` : k;
}

const hotkeyText = computed(() => hotkeyLabel(config.value.region_hotkey));

const saveDirLabel = computed(() => {
  const d = config.value.save_dir;
  if (!d) return "图片库";
  const idx = Math.max(d.lastIndexOf("\\"), d.lastIndexOf("/"));
  return idx >= 0 ? d.slice(idx + 1) : d;
});

// ====== QQ 交流群：点击复制群号 ======
const QQ_GROUP = "1094033308";
async function copyQqGroup() {
  try {
    await navigator.clipboard.writeText(QQ_GROUP);
    message.success(`群号 ${QQ_GROUP} 已复制，去 QQ 搜索加群吧`, { duration: 2600 });
  } catch {
    message.error("复制失败", { duration: 1800 });
  }
}

// ====== 内存 chip（实时百分比，来自 memory://status 每秒事件） ======
// 后端未就绪 / 命令失败时保持 null → 显示「内存 --」容错
const memPercent = ref<number | null>(null);
let unlistenMemFn: (() => void) | null = null;

// 分级：normal 主题色 / warn 奶茶橘 --jb-milk / danger 豆沙红 --jb-red（阈值来自 memory_clean 配置）
const memLevel = computed<"normal" | "warn" | "danger">(() => {
  const p = memPercent.value;
  if (p == null) return "normal";
  const warn = config.value.memory_clean?.danger_warning ?? 70;
  const crit = config.value.memory_clean?.danger_critical ?? 90;
  if (p >= crit) return "danger";
  if (p >= warn) return "warn";
  return "normal";
});

const memText = computed(() =>
  memPercent.value == null ? "内存 --" : `内存 ${Math.round(memPercent.value)}%`,
);

// ====== 截图 ======
// 区域截图：进入选择模式（可框选任意区域、单击识别窗口）
// 全屏截图：一键全屏选区进入标注（复用区域截图管线）
const capturing = ref(false);

// 区域截图（微信式：进入透明覆盖层选择模式）
async function captureRegion() {
  capturing.value = true;
  try {
    // fromPanel: 面板发起标记 —— 截图完成后主窗口还原并聚焦（热键发起则保持最小化）
    await invoke("enter_region_select", { fromPanel: true });
  } catch (e: any) {
    message.error("区域截图失败: " + e, { duration: 2200 });
    await getCurrentWindow().show();
  } finally {
    capturing.value = false;
  }
}

// 全屏截图：自动捕获全屏并保存 → 事件监听统一弹预览（初版行为）
async function captureFullscreen() {
  capturing.value = true;
  try {
    await invoke("capture_fullscreen");
  } catch (e: any) {
    message.error("全屏截图失败: " + e, { duration: 2200 });
  } finally {
    capturing.value = false;
  }
}

// 静默截图：一键保存主显示器（无标注无预览，跟随配置复制到剪贴板）
async function captureSilent() {
  capturing.value = true;
  try {
    const path = await invoke<string>("capture_silent");
    const name = path.split(/[\\\/]/).pop() || path;
    message.success(`已静默保存 ${name}`, { duration: 2200 });
  } catch (e: any) {
    message.error("静默截图失败: " + e, { duration: 2200 });
  } finally {
    capturing.value = false;
  }
}

// ====== 后端事件监听 ======
let unlistenFn: (() => void) | null = null;
let unlistenSavedFn: (() => void) | null = null;
let unlistenCopiedFn: (() => void) | null = null;
let unlistenCopyFailedFn: (() => void) | null = null;
let unlistenRecStatsFn: (() => void) | null = null;
let unlistenRecDoneFn: (() => void) | null = null;
let unlistenRecErrorFn: (() => void) | null = null;
let unlistenRecAutoFn: (() => void) | null = null;
let unlistenRecHotkeyFn: (() => void) | null = null;
let unlistenVideoStatsFn: (() => void) | null = null;
let unlistenVideoDoneFn: (() => void) | null = null;
let unlistenVideoPartFn: (() => void) | null = null;
let unlistenVideoSilenceFn: (() => void) | null = null;
let unlistenVideoHotkeyFn: (() => void) | null = null;
let unlistenVideoStoppingFn: (() => void) | null = null;
let unlistenVideoErrorFn: (() => void) | null = null;
let unlistenVideoShutdownFn: (() => void) | null = null;
let unlistenVideoRestartedFn: (() => void) | null = null;
let unlistenVideoCountdownFn: (() => void) | null = null;
onMounted(async () => {
  // 全屏截图/另存为 → 预览弹窗
  unlistenFn = await listen<string>("screenshot://captured", (ev) => {
    openPreview(ev.payload);
  });
  // 标注"保存"（静默保存语义）→ 仅 toast 文件名，不弹预览
  unlistenSavedFn = await listen<string>("screenshot://saved", (ev) => {
    const name = ev.payload.split(/[\\\/]/).pop() || ev.payload;
    message.success(`已保存 ${name}`, { duration: 2200 });
  });
  // 标注"复制" → toast 确认，不弹预览
  unlistenCopiedFn = await listen("screenshot://copied", () => {
    message.success("已复制到剪贴板", { duration: 2200 });
  });
  // 标注"复制"失败（剪贴板被占用等）→ 如实报错，不伪装成功
  unlistenCopyFailedFn = await listen<string>("screenshot://copy-failed", (ev) => {
    message.error(`复制失败: ${ev.payload}`, { duration: 4000 });
  });
  // 内存状态：先拉一次快照，再挂每秒事件刷新（全部容错，后端未就绪显示「内存 --」）
  try {
    const info = await invoke<{ physical: { percent: number } }>("get_memory_info");
    memPercent.value = info?.physical?.percent ?? null;
  } catch {
    // 后端未就绪：保持占位 --
  }
  try {
    unlistenMemFn = await listen<{ physical: { percent: number } }>(
      "memory://status",
      (ev) => {
        memPercent.value = ev.payload?.physical?.percent ?? null;
      },
    );
  } catch {
    // 事件不可用：仅保留初始快照
  }
  // 录制：每秒统计刷新（计时/倒计时/水位）
  try {
    unlistenRecStatsFn = await listen<typeof recordStats.value>(
      "record://stats",
      (ev) => {
        if (recording.value && ev.payload) recordStats.value = ev.payload;
      },
    );
    // 到达 30s 上限自动停止：自动触发 stop（含编码收尾）。
    // 沉浸模式（面板已最小化、OSD 在屏）跳过——OSD 自身监听 autostop 切编码态，
    // 停止由 OSD 侧 record://done 驱动面板状态复位
    unlistenRecAutoFn = await listen("record://autostop", () => {
      if (recording.value && !recordStopping.value && recordShow.value) {
        message.info("已达 30 秒上限，正在完成编码…", { duration: 3000 });
        stopRecord();
      }
    });
    // 编码线程完成/错误（record_stop 之外的双保险，如 ACCESS_LOST 中断）
    unlistenRecDoneFn = await listen<{
      path: string; frames: number; duration_ms: number; output_bytes: number;
    }>("record://done", (ev) => {
      if (recording.value) {
        const name = ev.payload.path.split(/[\\\/]/).pop() || ev.payload.path;
        const mb = (ev.payload.output_bytes / 1024 / 1024).toFixed(1);
        message.success(`已保存 ${name}（${ev.payload.frames} 帧 / ${mb}MB）`, { duration: 4000 });
        finishRecordUi();
      }
    });
    unlistenRecErrorFn = await listen<string>("record://error", (ev) => {
      if (recording.value) {
        message.error("录制出错: " + ev.payload, { duration: 5000 });
        finishRecordUi();
      }
    });
    // Alt+F9 热键开始录制（游戏模式直启）：面板同步 recording 态（面板可见时）
    unlistenRecHotkeyFn = await listen("record://hotkey-started", () => {
      if (!recording.value) {
        recording.value = true;
        recordInfo.value = { fps: 0, width: 0, height: 0, maxSeconds: 30, deferred: true };
        if (recordShow.value) recordShow.value = false;
      }
    });
  } catch {
    // 录制事件不可用：UI 仍可手动停止（轮询兜底）
  }
  // 视频录制事件（video://stats 每秒统计 / video://done finalize 完成）
  try {
    unlistenVideoStatsFn = await listen<typeof videoStats.value>(
      "video://stats",
      (ev) => {
        if (videoRecording.value && ev.payload) videoStats.value = ev.payload;
      },
    );
    // 停止按钮路径（videoStopping 已置位）由 invoke 结果展示消息，这里只复位；
    // 非停止路径（异常终止等）由事件兜底展示
    unlistenVideoDoneFn = await listen<{
      path: string; duration_ms: number; size_bytes: number;
    }>("video://done", (ev) => {
      if (!videoRecording.value) return;
      if (!videoStopping.value) {
        const name = ev.payload.path.split(/[\\\/]/).pop() || ev.payload.path;
        const mb = (ev.payload.size_bytes / 1024 / 1024).toFixed(1);
        const sec = (ev.payload.duration_ms / 1000).toFixed(1);
        message.success(`已保存 ${name}（${sec}s / ${mb}MB）`, { duration: 4000 });
      }
      finishVideoUi();
    });
    // 分卷 part 完成（录制继续；仅 toast，不重置录制 UI）
    unlistenVideoPartFn = await listen<{
      path: string; duration_ms: number; size_bytes: number;
    }>("video://part", (ev) => {
      if (!videoRecording.value) return;
      const name = ev.payload.path.split(/[\\\/]/).pop() || ev.payload.path;
      const mb = (ev.payload.size_bytes / 1024 / 1024).toFixed(1);
      const sec = (ev.payload.duration_ms / 1000).toFixed(0);
      message.info(`已分卷保存 ${name}（${sec}s / ${mb}MB），录制继续`, { duration: 4000 });
    });
    // 静默倒计时（>0 = 显示剩余秒；0 = 取消/隐藏）
    unlistenVideoSilenceFn = await listen<number>("video://silence-countdown", (ev) => {
      videoSilenceCountdown.value = ev.payload > 0 ? ev.payload : 0;
    });
    // 热键直启视频录制（游戏模式）：面板同步 recording 态（面板可见时）
    unlistenVideoHotkeyFn = await listen("video://hotkey-started", () => {
      if (!videoRecording.value) {
        videoRecording.value = true;
        videoStats.value = null;
        videoDelayCountdown.value = 0; // 延迟到点启动同样走本事件
        if (videoRecordShow.value) videoRecordShow.value = false;
      }
    });
    // 停止热键按下（OSD 路径）：面板录制态切"保存中"
    unlistenVideoStoppingFn = await listen("video://stopping", () => {
      if (videoRecording.value) videoStopping.value = true;
    });
    // 热键启动失败（面板可见时提示；OSD 不可用场景兜底）
    unlistenVideoErrorFn = await listen<string>("video://error", (ev) => {
      message.error("视频录制失败: " + ev.payload, { duration: 5000 });
      if (videoRecording.value) finishVideoUi();
    });
    // 关机倒计时（完成动作 shutdown）：录制结束后系统将关机——弹取消入口
    unlistenVideoShutdownFn = await listen<number>("video://shutdown-countdown", (ev) => {
      const secs = ev.payload;
      shutdownDialog.warning({
        title: "系统即将关机",
        content: `录制已完成，系统将在 ${secs} 秒后关机。`,
        positiveText: "取消关机",
        negativeText: "不取消",
        onPositiveClick: async () => {
          try {
            await invoke("video_cancel_shutdown");
            message.success("已取消关机", { duration: 2500 });
          } catch (e: any) {
            message.error("取消关机失败: " + e, { duration: 4000 });
          }
        },
      });
    });
    // 完成动作"录新视频"重启成功：面板同步录制态
    unlistenVideoRestartedFn = await listen("video://restarted", () => {
      if (!videoRecording.value) {
        videoRecording.value = true;
        videoStats.value = null;
        videoStopping.value = false;
      }
    });
    // 延迟启动倒计时（秒；0 = 取消/已开始）
    unlistenVideoCountdownFn = await listen<number>("video://countdown", (ev) => {
      videoDelayCountdown.value = ev.payload > 0 ? ev.payload : 0;
      if (ev.payload === 0) {
        // 取消（到点启动走 hotkey-started 事件）
        message.info("定时开始已取消", { duration: 2200 });
      }
    });
  } catch {
    // 视频事件不可用：UI 仍可手动停止（video_record_status 轮询兜底）
  }
});
onUnmounted(() => {
  if (unlistenFn) unlistenFn();
  if (unlistenSavedFn) unlistenSavedFn();
  if (unlistenCopiedFn) unlistenCopiedFn();
  if (unlistenCopyFailedFn) unlistenCopyFailedFn();
  if (unlistenMemFn) unlistenMemFn();
  if (unlistenRecStatsFn) unlistenRecStatsFn();
  if (unlistenRecDoneFn) unlistenRecDoneFn();
  if (unlistenRecErrorFn) unlistenRecErrorFn();
  if (unlistenRecAutoFn) unlistenRecAutoFn();
  if (unlistenRecHotkeyFn) unlistenRecHotkeyFn();
  if (unlistenVideoStatsFn) unlistenVideoStatsFn();
  if (unlistenVideoDoneFn) unlistenVideoDoneFn();
  if (unlistenVideoPartFn) unlistenVideoPartFn();
  if (unlistenVideoSilenceFn) unlistenVideoSilenceFn();
  if (unlistenVideoHotkeyFn) unlistenVideoHotkeyFn();
  if (unlistenVideoStoppingFn) unlistenVideoStoppingFn();
  if (unlistenVideoErrorFn) unlistenVideoErrorFn();
  if (unlistenVideoShutdownFn) unlistenVideoShutdownFn();
  if (unlistenVideoRestartedFn) unlistenVideoRestartedFn();
  if (unlistenVideoCountdownFn) unlistenVideoCountdownFn();
});

// ====== 截图预览弹窗（保存后查看） ======
const previewShow = ref(false);
const previewPath = ref("");
const previewSrc = ref("");
const previewName = ref("");
const previewDims = ref("");

function openPreview(path: string) {
  if (!path) return;
  // 遵循"显示预览"设置（设置中心可关闭）
  if (!config.value.show_preview) {
    message.success("截图已保存: " + path, { duration: 2200 });
    return;
  }
  previewPath.value = path;
  previewSrc.value = convertFileSrc(path);
  const idx = Math.max(path.lastIndexOf("\\"), path.lastIndexOf("/"));
  previewName.value = idx >= 0 ? path.slice(idx + 1) : path;
  previewDims.value = "";
  previewShow.value = true;
}

function onPreviewImgLoad(e: Event) {
  const img = e.target as HTMLImageElement;
  if (img.naturalWidth > 0) {
    previewDims.value = `${img.naturalWidth} × ${img.naturalHeight}`;
  }
}

async function previewCopy() {
  try {
    await invoke("copy_image_file_to_clipboard", { path: previewPath.value });
    message.success("已复制到剪贴板", { duration: 2200 });
  } catch (e) {
    message.error("复制失败: " + e, { duration: 2200 });
  }
}

async function previewReveal() {
  try {
    const { revealItemInDir } = await import("@tauri-apps/plugin-opener");
    await revealItemInDir(previewPath.value);
  } catch (e: any) {
    message.error("打开位置失败: " + e, { duration: 2200 });
  }
}

// ====== 录制动图（JXL；游戏模式 = 纯录制延迟编码，录制期间零编码不干扰游戏） ======
const recordShow = ref(false);        // 录制弹窗
const recording = ref(false);         // 录制进行中
const recordStopping = ref(false);    // 停止中（含编码 finalize，deferred 模式可达数十秒）
const gameMode = ref(false);          // 游戏模式开关（deferred_encode）
const recordInfo = ref<{ fps: number; width: number; height: number; maxSeconds: number; deferred: boolean } | null>(null);
const recordStats = ref<{
  elapsed_ms: number;
  captured_frames: number;
  deduped_frames: number;
  encoded_frames: number;
  ring_bytes: number;
  compression_active: boolean;
  evicted_frames: number;
} | null>(null);

const recordCountdown = computed(() => {
  const s = recordStats.value;
  if (!s || !recordInfo.value) return null;
  return Math.max(0, recordInfo.value.maxSeconds - Math.floor(s.elapsed_ms / 1000));
});

const recordTimeText = computed(() => {
  const s = recordStats.value;
  if (!s) return "00:00";
  const sec = Math.floor(s.elapsed_ms / 1000);
  return `${String(Math.floor(sec / 60)).padStart(2, "0")}:${String(sec % 60).padStart(2, "0")}`;
});

const ringGB = computed(() => {
  const b = recordStats.value?.ring_bytes ?? 0;
  return (b / 1024 / 1024 / 1024).toFixed(2);
});

async function openRecord() {
  recordShow.value = true;
}

// 开始录制（全屏；游戏模式 = deferred + osd：面板最小化 + OSD 叠加层 + 停止热键）
async function startRecord() {
  try {
    const info = await invoke<{ fps: number; width: number; height: number; maxSeconds: number; deferred: boolean; stopHotkey: string | null }>(
      "record_start",
      { deferred: gameMode.value, osd: gameMode.value },
    );
    recordInfo.value = info;
    recordStats.value = null;
    recording.value = true;
    if (gameMode.value) {
      // 沉浸式：面板已最小化 + OSD 已上屏 → 关闭面板内弹窗（状态由 OSD 呈现）
      recordShow.value = false;
      message.info(
        info.stopHotkey
          ? `录制已开始，按 ${info.stopHotkey} 停止`
          : "录制已开始（未启用停止热键，可从面板停止）",
        { duration: 3000 },
      );
    }
  } catch (e: any) {
    message.error("录制启动失败: " + e, { duration: 3000 });
  }
}

// 停止录制（阻塞至编码完成：实时模式通常数秒；游戏模式全量编码 20-90s，按钮转圈）
async function stopRecord() {
  recordStopping.value = true;
  try {
    const result = await invoke<{
      path: string; frames: number; duration_ms: number;
      deduped_frames: number; output_bytes: number; auto_stopped: boolean;
    }>("record_stop");
    const name = result.path.split(/[\\\/]/).pop() || result.path;
    const mb = (result.output_bytes / 1024 / 1024).toFixed(1);
    const sec = (result.duration_ms / 1000).toFixed(1);
    message.success(`已保存 ${name}（${result.frames} 帧 / ${sec}s / ${mb}MB）`, { duration: 4000 });
    finishRecordUi();
  } catch (e: any) {
    message.error("录制失败: " + e, { duration: 5000 });
    finishRecordUi();
  }
}

async function cancelRecord() {
  recordStopping.value = true;
  try {
    await invoke("record_cancel");
    message.info("已取消录制", { duration: 2200 });
  } catch (e: any) {
    message.error("取消失败: " + e, { duration: 2200 });
  }
  finishRecordUi();
}

function finishRecordUi() {
  recordStopping.value = false;
  recording.value = false;
  recordStats.value = null;
  recordInfo.value = null;
}

// ====== 视频录制（videorec：MKV 长录制 + 硬件编码，与 JXL 动图全局互斥） ======
const videoRecordShow = ref(false);   // 视频录制弹窗
// V18 画质增强弹窗（视频播放动态降噪/锐化/色彩参数板）
const fxShow = ref(false);
const videoRecording = ref(false);    // 录制进行中
const videoStopping = ref(false);     // 停止中（nvenc 排空 + MKV finalize，秒级）
const videoStats = ref<{
  elapsed_ms: number;
  encoded_frames: number;
  dropped_frames: number;
  size_bytes: number;
  encoder: string;
  paused: boolean;
  fps: number;
} | null>(null);

// 长录制计时（支持小时段）
const videoTimeText = computed(() => {
  const s = videoStats.value;
  if (!s) return "00:00";
  const sec = Math.floor(s.elapsed_ms / 1000);
  const h = Math.floor(sec / 3600);
  const m = Math.floor((sec % 3600) / 60);
  const ss = sec % 60;
  return h > 0
    ? `${h}:${String(m).padStart(2, "0")}:${String(ss).padStart(2, "0")}`
    : `${String(m).padStart(2, "0")}:${String(ss).padStart(2, "0")}`;
});

const videoSizeMB = computed(() =>
  ((videoStats.value?.size_bytes ?? 0) / 1024 / 1024).toFixed(1),
);

// 延迟启动倒计时文本（秒 → m:ss）
const videoDelayText = computed(() => {
  const s = videoDelayCountdown.value;
  if (s <= 0) return "00:00";
  const m = Math.floor(s / 60);
  const ss = s % 60;
  return `${String(m).padStart(2, "0")}:${String(ss).padStart(2, "0")}`;
});

// 静默倒计时（0 = 无）：录制状态区提示
const videoSilenceCountdown = ref(0);

const videoCodecOptions = [
  { label: "H.264（兼容性最好）", value: "h264" },
  { label: "HEVC / H.265（同画质更小）", value: "hevc" },
];
const videoFpsOptions = [
  { label: "30 fps", value: 30 },
  { label: "60 fps", value: 60 },
  { label: "120 fps", value: 120 },
];
const videoAudioOptions = [
  { label: "关闭", value: "off" },
  { label: "系统声音", value: "system" },
  { label: "系统 + 麦克风", value: "both" },
];

function openVideoRecord() {
  videoRecordShow.value = true;
}

// 定时器（延迟启动）：0 = 立即；>0 分钟后自动开始（倒计时中可取消）
const videoDelayMinutes = ref(0);
const videoDelayCountdown = ref(0);

async function startVideoRecord() {
  const delay = videoDelayMinutes.value || 0;
  try {
    const info = await invoke<{ path: string; fps: number; encoder: string; audio: string }>(
      "video_record_start",
      {
        opts: {
          codec: config.value.video.codec,
          fps: config.value.video.fps,
          audio: config.value.video.audio,
          delayMinutes: delay > 0 ? delay : null,
        },
      },
    );
    if (delay > 0) {
      // 延迟模式：info.delayed=true——弹窗切倒计时态（countdown 事件驱动）
      videoDelayCountdown.value = delay * 60;
      message.info(`将在 ${delay} 分钟后自动开始录制`, { duration: 3000 });
      return;
    }
    videoStats.value = null;
    videoRecording.value = true;
    message.info(`视频录制已开始（${info.encoder}）`, { duration: 2500 });
  } catch (e: any) {
    message.error("视频录制启动失败: " + e, { duration: 4000 });
  }
}

async function cancelVideoDelay() {
  try {
    await invoke("video_cancel_delayed_start");
    videoDelayCountdown.value = 0;
    message.info("已取消定时开始", { duration: 2200 });
  } catch (e: any) {
    message.error("取消失败: " + e, { duration: 3000 });
  }
}

async function stopVideoRecord() {
  videoStopping.value = true;
  try {
    const r = await invoke<{
      path: string; duration_ms: number; size_bytes: number; codec: string; encoder: string;
    }>("video_record_stop");
    const name = r.path.split(/[\\\/]/).pop() || r.path;
    const mb = (r.size_bytes / 1024 / 1024).toFixed(1);
    const sec = (r.duration_ms / 1000).toFixed(1);
    message.success(`已保存 ${name}（${sec}s / ${mb}MB）`, { duration: 4000 });
  } catch (e: any) {
    message.error("视频录制失败: " + e, { duration: 5000 });
  }
  finishVideoUi();
}

async function cancelVideoRecord() {
  videoStopping.value = true;
  try {
    await invoke("video_record_cancel");
    message.info("已取消视频录制", { duration: 2200 });
  } catch (e: any) {
    message.error("取消失败: " + e, { duration: 2200 });
  }
  finishVideoUi();
}

function finishVideoUi() {
  videoStopping.value = false;
  videoRecording.value = false;
  videoStats.value = null;
  videoSilenceCountdown.value = 0;
  videoDelayCountdown.value = 0;
}

// 暂停/恢复录制（时间戳后端 MediaClock 折叠，恢复后音视频无缝续接）
async function toggleVideoPause() {
  const paused = videoStats.value?.paused ?? false;
  try {
    if (paused) {
      await invoke("video_record_resume");
    } else {
      await invoke("video_record_pause");
    }
  } catch (e: any) {
    message.error((paused ? "恢复" : "暂停") + "失败: " + e, { duration: 2200 });
  }
}

// 播放视频文件（videorec 原生播放器：硬解/倍速/字幕/书签，30+ 键位开箱即得）
async function openVideoPlayer() {
  try {
    const sel = await openDialog({
      multiple: true,
      filters: [
        {
          name: "视频",
          extensions: ["mp4", "mkv", "mov", "avi", "webm", "flv", "ts", "m4v"],
        },
      ],
    });
    if (!sel || (Array.isArray(sel) && sel.length === 0)) return;
    const paths = Array.isArray(sel) ? sel : [sel];
    await invoke("video_play_open", { paths, opts: {} });
  } catch (e: any) {
    message.error("播放失败: " + e, { duration: 3000 });
  }
}
</script>

<template>
  <div class="app-root">
    <TitleBar :mode="props.mode" :is-dark="props.isDark" :set-mode="props.setMode" />

    <!-- n-modal 挂载锚点：让预览弹窗渲染在 app-root 内（标题栏 z-index 之下），
         预览打开时标题栏不被遮罩覆盖，仍可拖动窗口 -->
    <div id="modal-anchor" class="modal-anchor"></div>

    <!-- 主内容（极简） -->
    <div class="content">
      <!-- 轻问候语：弱化工具生硬感 -->
      <div class="greeting">准备好捕捉美好瞬间了吗</div>

      <!-- 主按钮：区域截图 -->
      <button
        class="hero-btn"
        :disabled="capturing"
        @click="captureRegion"
      >
        <n-icon :component="Camera" size="22" />
        <span>区域截图</span>
      </button>

      <!-- 次级按钮：全屏 / 静默 / 打开相册 / 看图 -->
      <div class="sub-actions">
        <button class="sub-btn" :disabled="capturing" @click="captureFullscreen">
          <n-icon :component="ArrowsMaximize" size="16" />
          <span>全屏截图</span>
        </button>
        <button class="sub-btn" :disabled="capturing" @click="captureSilent">
          <n-icon :component="DeviceFloppy" size="16" />
          <span>静默保存</span>
        </button>
        <button class="sub-btn" @click="openAlbum">
          <n-icon :component="Folder" size="16" />
          <span>打开相册</span>
        </button>
        <button class="sub-btn" @click="openInViewer">
          <n-icon :component="Photo" size="16" />
          <span>打开图片</span>
        </button>
        <button class="sub-btn" @click="openUpscale">
          <n-icon :component="Wand" size="16" />
          <span>AI 放大</span>
        </button>
        <button class="sub-btn" @click="fxShow = true">
          <n-icon :component="Adjustments" size="16" />
          <span>画质增强</span>
        </button>
        <button class="sub-btn" :disabled="capturing" @click="openVideoRecord">
          <n-icon :component="Movie" size="16" />
          <span>录制视频</span>
        </button>
        <button class="sub-btn" :disabled="capturing" @click="openRecord">
          <n-icon :component="Video" size="16" />
          <span>录制动图</span>
        </button>
      </div>

      <div class="hero-hint">框选任意区域 · 单击识别窗口</div>

      <!-- 可点状态条：点击跳转设置中心对应分组 -->
      <div class="status-bar">
        <button class="status-chip" title="输出设置" @click="emit('open-settings', 'output')">
          <n-icon :component="Photo" size="14" />
          <span>{{ formatLabel }}</span>
        </button>
        <button class="status-chip" title="热键设置" @click="emit('open-settings', 'hotkey')">
          <n-icon :component="Bolt" size="14" />
          <span>{{ hotkeyText }}</span>
        </button>
        <!-- 内存 chip：实时百分比，分级变色（≥70 奶茶橘 / ≥90 豆沙红 + 呼吸提醒），点击进入内存清理分组 -->
        <button
          class="status-chip mem-chip"
          :class="[memLevel, { breathing: memLevel === 'danger' }]"
          title="内存清理"
          @click="emit('open-settings', 'memory')"
        >
          <n-icon :component="Cpu" size="14" />
          <span>{{ memText }}</span>
        </button>
        <button class="status-chip" title="保存路径" @click="emit('open-settings', 'output')">
          <n-icon :component="Folder" size="14" />
          <span>{{ saveDirLabel }}</span>
        </button>
        <!-- QQ 交流群：点击复制群号 -->
        <button class="status-chip qq-chip" :title="`QQ交流群：${QQ_GROUP}（点击复制）`" @click="copyQqGroup">
          <n-icon :component="MessageCircle" size="14" />
          <span>QQ交流群</span>
        </button>
        <button class="status-chip settings-chip" title="设置中心" @click="emit('open-settings')">
          <n-icon :component="Settings" size="14" />
          <span>设置</span>
        </button>
      </div>
    </div>

    <!-- 截图预览弹窗 -->
    <n-modal
      v-model:show="previewShow"
      preset="card"
      title="截图预览"
      class="preview-modal"
      :bordered="false"
      size="huge"
      to="#modal-anchor"
      :mask-closable="false"
      :close-on-esc="false"
      style="max-width: min(92vw, 960px)"
    >
      <div class="preview-body">
        <div class="preview-img-wrap">
          <img
            v-if="previewSrc"
            :src="previewSrc"
            class="preview-img"
            @load="onPreviewImgLoad"
            alt="截图预览"
          />
        </div>
        <div class="preview-meta">
          <span class="preview-name" :title="previewPath">{{ previewName }}</span>
          <span v-if="previewDims" class="preview-dims">{{ previewDims }}</span>
        </div>
      </div>
      <template #footer>
        <div class="preview-actions">
          <n-button size="small" @click="previewCopy">
            复制到剪贴板
          </n-button>
          <n-button size="small" @click="previewReveal">
            打开所在位置
          </n-button>
          <n-button size="small" type="primary" @click="previewShow = false">
            关闭
          </n-button>
        </div>
      </template>
    </n-modal>

    <!-- 录制动图弹窗（JXL 动图；游戏模式 = 纯录制延迟编码） -->
    <n-modal
      v-model:show="recordShow"
      preset="card"
      title="录制动图（JXL）"
      class="record-modal"
      :bordered="false"
      to="#modal-anchor"
      :mask-closable="false"
      :close-on-esc="false"
      style="max-width: min(92vw, 460px)"
    >
      <!-- 未录制：模式选择 + 开始 -->
      <div v-if="!recording" class="record-setup">
        <div class="game-mode-row" :class="{ active: gameMode }">
          <div class="game-mode-text">
            <div class="game-mode-title">游戏模式</div>
            <div class="game-mode-desc">
              录制期间零编码，不影响游戏帧数；停止后全核编码需等待片刻
            </div>
          </div>
          <n-switch v-model:value="gameMode" />
        </div>
        <div class="record-meta-hint">
          最长 30 秒 · 全屏 · HDR JXL 动图 · 画面静止时自动去重
        </div>
      </div>

      <!-- 录制中：计时 + 统计 + 停止 -->
      <div v-else class="record-live">
        <div class="record-timer">
          <span class="rec-dot"></span>
          <span class="rec-time">{{ recordTimeText }}</span>
          <span v-if="recordCountdown != null" class="rec-countdown">剩余 {{ recordCountdown }}s</span>
          <span v-if="recordInfo?.deferred" class="rec-mode-tag">游戏模式</span>
        </div>
        <n-progress
          type="line"
          :percentage="recordStats ? Math.min(100, (recordStats.elapsed_ms / (recordInfo!.maxSeconds * 1000)) * 100) : 0"
          :show-indicator="false"
          :height="6"
          color="#e5484d"
          rail-color="color-mix(in srgb, #e5484d 15%, transparent)"
        />
        <div v-if="recordStats" class="record-stats">
          <span>{{ recordStats.captured_frames }} 帧</span>
          <span v-if="recordStats.deduped_frames">去重 {{ recordStats.deduped_frames }}</span>
          <span v-if="recordInfo?.deferred">缓冲 {{ ringGB }}GB</span>
          <span v-else>已编 {{ recordStats.encoded_frames }}</span>
          <span v-if="recordStats.evicted_frames > 0" class="rec-warn">淘汰 {{ recordStats.evicted_frames }}</span>
        </div>
      </div>

      <template #footer>
        <div class="record-actions">
          <template v-if="!recording">
            <n-button size="small" @click="recordShow = false">取消</n-button>
            <n-button size="small" type="primary" @click="startRecord">
              开始录制
            </n-button>
          </template>
          <template v-else>
            <n-button size="small" :disabled="recordStopping" @click="cancelRecord">
              放弃
            </n-button>
            <n-button
              size="small"
              type="primary"
              :loading="recordStopping"
              @click="stopRecord"
            >
              <template #icon>
                <n-icon v-if="!recordStopping" :component="PlayerStop" />
              </template>
              {{ recordStopping ? "正在编码…" : "停止并保存" }}
            </n-button>
          </template>
        </div>
      </template>
    </n-modal>

    <!-- 画质增强弹窗（V18：视频播放动态降噪/锐化/色彩参数板） -->
    <FxPanel v-model:show="fxShow" />

    <!-- 录制视频弹窗（MKV 长录制 · 硬件编码 · 可选音频） -->
    <n-modal
      v-model:show="videoRecordShow"
      preset="card"
      title="录制视频（MKV）"
      class="record-modal"
      :bordered="false"
      to="#modal-anchor"
      :mask-closable="false"
      :close-on-esc="false"
      style="max-width: min(92vw, 460px)"
    >
      <!-- 未录制：参数选择 + 开始 -->
      <div v-if="!videoRecording && videoDelayCountdown === 0" class="record-setup">
        <div class="video-opt-row">
          <span class="video-opt-label">编码</span>
          <n-select
            v-model:value="config.video.codec"
            :options="videoCodecOptions"
            size="small"
            style="width: 220px"
          />
        </div>
        <div class="video-opt-row">
          <span class="video-opt-label">帧率</span>
          <n-select
            v-model:value="config.video.fps"
            :options="videoFpsOptions"
            size="small"
            style="width: 220px"
          />
        </div>
        <div class="video-opt-row">
          <span class="video-opt-label">音频</span>
          <n-select
            v-model:value="config.video.audio"
            :options="videoAudioOptions"
            size="small"
            style="width: 220px"
          />
        </div>
        <div class="video-opt-row">
          <span class="video-opt-label">定时</span>
          <div class="video-delay-inputs">
            <n-input-number
              v-model:value="videoDelayMinutes"
              size="small"
              :min="0"
              :max="720"
              :step="5"
              style="width: 100px"
            />
            <span class="video-delay-unit">分钟后开始（0 = 立即）</span>
          </div>
        </div>
        <div class="record-meta-hint">
          全屏录制 · MKV 崩溃安全容器 · 硬件编码（自动探测）· 时长不限
        </div>
      </div>

      <!-- 延迟启动倒计时中 -->
      <div v-else-if="!videoRecording && videoDelayCountdown > 0" class="record-live">
        <div class="record-timer">
          <span class="rec-dot"></span>
          <span class="rec-time">{{ videoDelayText }}</span>
        </div>
        <div class="video-play-hint">
          到点后自动开始录制（按当前编码/帧率/音频设置）
        </div>
      </div>

      <!-- 录制中：计时 + 统计 + 停止 -->
      <div v-else class="record-live">
        <div class="record-timer">
          <span class="rec-dot" :class="{ paused: videoStats?.paused }"></span>
          <span class="rec-time">{{ videoTimeText }}</span>
          <span v-if="videoStats?.paused" class="rec-paused-tag">已暂停</span>
        </div>
        <div v-if="videoStats" class="record-stats video-stats">
          <span v-if="!videoStats.paused && videoStats.fps > 0">{{ videoStats.fps }} fps</span>
          <span>{{ videoStats.encoded_frames }} 帧</span>
          <span>{{ videoSizeMB }}MB</span>
          <span v-if="videoStats.dropped_frames > 0" class="rec-warn">
            丢帧 {{ videoStats.dropped_frames }}
          </span>
        </div>
        <div class="video-play-hint">
          {{ videoStats?.paused ? "已暂停，计时冻结中——恢复后时间轴无缝续接" : "录制中，点击停止并保存生成 MKV 文件" }}
        </div>
        <!-- 静默倒计时：无声 N 秒后自动停止（声音恢复自动取消） -->
        <div v-if="videoSilenceCountdown > 0 && !videoStats?.paused" class="video-play-hint silence-hint">
          无声持续——{{ videoSilenceCountdown }}s 后自动停止（声音恢复将取消）
        </div>
      </div>

      <template #footer>
        <div class="record-actions">
          <template v-if="!videoRecording && videoDelayCountdown > 0">
            <n-button size="small" @click="videoRecordShow = false">后台等待</n-button>
            <n-button size="small" type="warning" @click="cancelVideoDelay">
              取消定时
            </n-button>
          </template>
          <template v-else-if="!videoRecording">
            <n-button size="small" @click="openVideoPlayer">
              <template #icon>
                <n-icon :component="PlayerPlay" />
              </template>
              播放视频
            </n-button>
            <n-button size="small" @click="videoRecordShow = false">取消</n-button>
            <n-button size="small" type="primary" @click="startVideoRecord">
              开始录制
            </n-button>
          </template>
          <template v-else>
            <n-button size="small" :disabled="videoStopping" @click="cancelVideoRecord">
              放弃
            </n-button>
            <n-button size="small" :disabled="videoStopping" @click="toggleVideoPause">
              <template #icon>
                <n-icon :component="videoStats?.paused ? PlayerPlay : PlayerPause" />
              </template>
              {{ videoStats?.paused ? "继续" : "暂停" }}
            </n-button>
            <n-button
              size="small"
              type="primary"
              :loading="videoStopping"
              @click="stopVideoRecord"
            >
              <template #icon>
                <n-icon v-if="!videoStopping" :component="PlayerStop" />
              </template>
              {{ videoStopping ? "正在保存…" : "停止并保存" }}
            </n-button>
          </template>
        </div>
      </template>
    </n-modal>
  </div>
</template>

<style scoped>
.app-root {
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
.app-root::before {
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

/* === n-modal 挂载锚点：零尺寸不占布局 === */
.modal-anchor {
  position: absolute;
  width: 0;
  height: 0;
  overflow: visible;
}

/* === 主内容（极简居中） === */
.content {
  flex: 1;
  overflow-y: auto;
  padding: 20px 22px 18px;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 16px;
  position: relative;
  z-index: 1;
}

/* 轻问候语 */
.greeting {
  font-size: 13px;
  color: var(--jb-text-soft);
  letter-spacing: 2px;
}

/* 主按钮：大圆角 + 主题色渐变 + 柔和投影 */
.hero-btn {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 10px;
  width: 100%;
  height: 56px;
  border: none;
  border-radius: 18px;
  font-size: 16px;
  font-weight: 600;
  letter-spacing: 2px;
  color: #fff;
  cursor: pointer;
  background: linear-gradient(
    135deg,
    var(--jb-primary) 0%,
    var(--jb-primary-pressed) 100%
  );
  box-shadow:
    0 6px 18px color-mix(in srgb, var(--jb-primary) 42%, transparent),
    inset 0 1px 0 rgba(255, 255, 255, 0.28);
  transition: transform 0.16s ease, box-shadow 0.16s ease, filter 0.16s ease;
}
.hero-btn:hover:not(:disabled) {
  transform: translateY(-1px);
  filter: brightness(1.05);
  box-shadow:
    0 10px 24px color-mix(in srgb, var(--jb-primary) 52%, transparent),
    inset 0 1px 0 rgba(255, 255, 255, 0.28);
}
.hero-btn:active:not(:disabled) {
  transform: translateY(0.5px);
  filter: brightness(0.97);
}
.hero-btn:disabled {
  opacity: 0.6;
  cursor: default;
}

/* 次级按钮 */
.sub-actions {
  display: flex;
  gap: 10px;
  width: 100%;
}
.sub-btn {
  flex: 1;
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 7px;
  height: 36px;
  border: 1px solid var(--jb-border);
  border-radius: 12px;
  background: var(--jb-bg-card);
  color: var(--jb-text);
  font-size: 13px;
  cursor: pointer;
  transition: background-color 0.15s, border-color 0.15s, transform 0.15s;
}
.sub-btn:hover:not(:disabled) {
  background: var(--jb-bg-card-hover);
  border-color: var(--jb-primary);
  color: var(--jb-primary);
}
.sub-btn:active:not(:disabled) {
  transform: scale(0.98);
}
.sub-btn:disabled {
  opacity: 0.6;
  cursor: default;
}

.hero-hint {
  font-size: 11px;
  color: var(--jb-text-mute);
  letter-spacing: 1px;
}

/* === 可点状态条 === */
.status-bar {
  display: flex;
  flex-wrap: wrap;
  justify-content: center;
  gap: 8px;
  margin-top: 4px;
}
.status-chip {
  display: flex;
  align-items: center;
  gap: 5px;
  padding: 5px 11px;
  border: 1px solid var(--jb-border);
  border-radius: 999px;
  background: var(--jb-bg-card);
  color: var(--jb-text-soft);
  font-size: 11px;
  cursor: pointer;
  transition: color 0.15s, border-color 0.15s, background-color 0.15s;
}
.status-chip:hover {
  color: var(--jb-primary);
  border-color: var(--jb-primary);
  background: color-mix(in srgb, var(--jb-primary) 8%, transparent);
}
.settings-chip {
  color: var(--jb-primary);
  border-color: color-mix(in srgb, var(--jb-primary) 45%, transparent);
  background: color-mix(in srgb, var(--jb-primary) 10%, transparent);
}
.settings-chip:hover {
  background: color-mix(in srgb, var(--jb-primary) 16%, transparent);
}
/* === QQ 交流群 chip：淡青蓝区分（社交属性，弱于设置的强调感） === */
.qq-chip {
  color: #6aa8b8;
  border-color: color-mix(in srgb, #6aa8b8 45%, transparent);
  background: color-mix(in srgb, #6aa8b8 8%, transparent);
}
.qq-chip:hover {
  color: #7fbccb;
  background: color-mix(in srgb, #6aa8b8 14%, transparent);
}

/* === 内存 chip：分级变色（warn 奶茶橘 / danger 豆沙红），≥90% 呼吸提醒 === */
.mem-chip.warn {
  color: var(--jb-milk);
  border-color: color-mix(in srgb, var(--jb-milk) 45%, transparent);
  background: color-mix(in srgb, var(--jb-milk) 10%, transparent);
}
.mem-chip.warn:hover {
  color: var(--jb-milk);
  border-color: var(--jb-milk);
  background: color-mix(in srgb, var(--jb-milk) 16%, transparent);
}
.mem-chip.danger {
  color: var(--jb-red);
  border-color: color-mix(in srgb, var(--jb-red) 45%, transparent);
  background: color-mix(in srgb, var(--jb-red) 10%, transparent);
}
.mem-chip.danger:hover {
  color: var(--jb-red);
  border-color: var(--jb-red);
  background: color-mix(in srgb, var(--jb-red) 16%, transparent);
}
/* 呼吸动画（复用 hk-blink 模式，1.2s 循环） */
.mem-chip.breathing {
  animation: mem-blink 1.2s ease-in-out infinite;
}
@keyframes mem-blink {
  0%, 100% { opacity: 1; }
  50% { opacity: 0.45; }
}

/* === 截图预览弹窗 === */
.preview-body {
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.preview-img-wrap {
  display: flex;
  align-items: center;
  justify-content: center;
  background: var(--jb-bg);
  border-radius: 8px;
  padding: 8px;
  min-height: 200px;
  max-height: 62vh;
  overflow: hidden;
}
.preview-img {
  max-width: 100%;
  max-height: 60vh;
  object-fit: contain;
  border-radius: 4px;
}
.preview-meta {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  font-size: 12px;
  color: var(--jb-text-soft);
}
.preview-name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.preview-dims {
  flex-shrink: 0;
}
.preview-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}

/* === 录制动图弹窗 === */
.record-modal {
  animation: jb-fade-up 220ms ease-out;
}

.record-setup {
  display: flex;
  flex-direction: column;
  gap: 14px;
}

/* 游戏模式行：卡片式开关（激活时主题色描边 + 柔光底） */
.game-mode-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 14px;
  padding: 14px 16px;
  border-radius: 12px;
  border: 1px solid var(--jb-border, rgba(0, 0, 0, 0.08));
  background: color-mix(in srgb, var(--jb-primary) 4%, transparent);
  transition: border-color 160ms ease, background 160ms ease;
}
.game-mode-row.active {
  border-color: color-mix(in srgb, var(--jb-primary) 55%, transparent);
  background: color-mix(in srgb, var(--jb-primary) 10%, transparent);
}
.game-mode-text {
  display: flex;
  flex-direction: column;
  gap: 4px;
  min-width: 0;
}
.game-mode-title {
  font-size: 14px;
  font-weight: 600;
  color: var(--jb-text, #333);
}
.game-mode-desc {
  font-size: 12px;
  line-height: 1.5;
  color: var(--jb-text-soft, #888);
}

.record-meta-hint {
  font-size: 12px;
  color: var(--jb-text-soft, #888);
  text-align: center;
  letter-spacing: 0.5px;
}

/* 视频录制弹窗：参数行（label + select） */
.video-opt-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 6px 2px;
}
.video-opt-label {
  font-size: 13px;
  color: var(--jb-text, #444);
  flex-shrink: 0;
}
.video-delay-inputs {
  display: flex;
  align-items: center;
  gap: 8px;
  flex: 1;
  justify-content: flex-end;
}
.video-delay-unit {
  font-size: 12px;
  color: var(--jb-text-soft, #888);
  white-space: nowrap;
}
.video-stats {
  justify-content: flex-start;
}
.video-play-hint {
  font-size: 12px;
  color: var(--jb-text-soft, #888);
  text-align: center;
}

/* 静默倒计时提示（琥珀醒目） */
.silence-hint {
  color: #d99a2b;
  font-weight: 600;
}

/* 录制中：红点呼吸 + 计时 + 进度条 + 统计 */
.record-live {
  display: flex;
  flex-direction: column;
  gap: 12px;
  padding: 4px 2px;
}
.record-timer {
  display: flex;
  align-items: center;
  gap: 10px;
}
.rec-dot {
  width: 10px;
  height: 10px;
  border-radius: 50%;
  background: #e5484d;
  box-shadow: 0 0 8px rgba(229, 72, 77, 0.6);
  animation: rec-pulse 1.2s ease-in-out infinite;
}
/* 暂停态：呼吸停跳 + 琥珀色（区别录制红点的进行中语义） */
.rec-dot.paused {
  background: #f5a623;
  box-shadow: 0 0 8px rgba(245, 166, 35, 0.55);
  animation: none;
}
.rec-paused-tag {
  font-size: 11px;
  padding: 2px 8px;
  border-radius: 4px;
  background: rgba(245, 166, 35, 0.14);
  color: #b9770e;
}
@keyframes rec-pulse {
  0%,
  100% {
    opacity: 1;
  }
  50% {
    opacity: 0.35;
  }
}
.rec-time {
  font-size: 26px;
  font-weight: 600;
  font-variant-numeric: tabular-nums;
  color: var(--jb-text, #333);
  letter-spacing: 1px;
}
.rec-countdown {
  font-size: 12px;
  color: var(--jb-text-soft, #888);
  font-variant-numeric: tabular-nums;
}
.rec-mode-tag {
  font-size: 11px;
  padding: 2px 8px;
  border-radius: 999px;
  color: var(--jb-primary, #b06ab3);
  background: color-mix(in srgb, var(--jb-primary) 12%, transparent);
  font-weight: 500;
}
.record-stats {
  display: flex;
  gap: 14px;
  font-size: 12px;
  color: var(--jb-text-soft, #888);
  font-variant-numeric: tabular-nums;
}
.rec-warn {
  color: #e5484d;
}

.record-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}

/* === 预览弹窗淡入 + 位移（8.1，220ms） === */
.preview-modal {
  animation: jb-fade-up 220ms ease-out;
}
@keyframes jb-fade-up {
  from {
    opacity: 0;
    transform: translateY(12px);
  }
  to {
    opacity: 1;
    transform: translateY(0);
  }
}
</style>
