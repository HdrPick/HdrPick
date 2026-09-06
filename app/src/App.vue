<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted } from "vue";
import { NConfigProvider, NMessageProvider, NDialogProvider } from "naive-ui";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { LogicalSize, PhysicalSize } from "@tauri-apps/api/dpi";
import { listen } from "@tauri-apps/api/event";
import { buildOverrides } from "./theme";
import { useTheme } from "./useTheme";
import { primaryColor, setThemeColor } from "./themeColor";
import { config, loadConfig } from "./configStore";
import MainView from "./MainView.vue";
import SettingsView from "./SettingsView.vue";
import AnnotationOverlay from "./annotation/AnnotationOverlay.vue";
import PinWindow from "./pin/PinWindow.vue";
import RegionSelectOverlay from "./region/RegionSelectOverlay.vue";
import ViewerWindow from "./viewer/ViewerWindow.vue";
import UpscaleWindow from "./viewer/UpscaleWindow.vue";
import RecordOsd from "./record/RecordOsd.vue";
import VideoOsd from "./record/VideoOsd.vue";
import AnimToolbar from "./viewer/AnimToolbar.vue";

const { mode, isDark, naiveTheme, setMode } = useTheme();

// Naive-UI 覆盖随亮/暗 + 主题色动态切换
const activeOverrides = computed(() =>
  buildOverrides(isDark.value, primaryColor.value),
);

// === 主面板 / 设置中心 视图切换（沉浸式：过渡动画 + 窗口尺寸适配） ===
const panelView = ref<"main" | "settings">("main");
const settingsSection = ref<string | undefined>(undefined);

const MAIN_SIZE = { width: 440, height: 520 };
const SETTINGS_SIZE = { width: 640, height: 560 };

async function resizeWindow(to: "main" | "settings") {
  try {
    const win = getCurrentWindow();
    // 最大化时不调整（还原后仍是用户偏好尺寸）
    if (await win.isMaximized()) return;
    if (to === "settings") {
      await win.setSize(new LogicalSize(SETTINGS_SIZE.width, SETTINGS_SIZE.height));
    } else {
      // 返回主面板：优先恢复记忆的尺寸（Rust 侧 config.main_window，物理像素）
      const remembered = await invoke<{
        main_window?: { x: number; y: number; width: number; height: number } | null;
      } | null>("get_config")
        .then((c) => c?.main_window ?? null).catch(() => null);
      if (remembered && remembered.width > 0 && remembered.height > 0) {
        await win.setSize(new PhysicalSize(remembered.width, remembered.height));
      } else {
        await win.setSize(new LogicalSize(MAIN_SIZE.width, MAIN_SIZE.height));
      }
    }
  } catch {
    // 非 Tauri 环境忽略
  }
}

function openSettings(section?: string) {
  settingsSection.value = section;
  panelView.value = "settings";
  // 暂停几何录制（防止设置页尺寸被记成主面板尺寸）
  invoke("set_main_geometry_recording", { enabled: false }).catch(() => {});
  resizeWindow("settings");
}
async function backToMain() {
  panelView.value = "main";
  await resizeWindow("main");
  // 恢复几何录制（主面板视图下正常记忆用户调整）
  invoke("set_main_geometry_recording", { enabled: true }).catch(() => {});
}

// === 视图切换 ===
// 主窗口通过事件切换视图，不创建新窗口（避免新窗口 Vue 重新加载导致白屏死锁）
// - "main"          → 主面板
// - "settings"      → 设置中心（沉浸式切换）
// - "annotation"     → 标注覆盖层（主窗口最大化）
// - "pin"            → 贴图模式（通过 hash 路由，仅用于独立贴图窗口）
// - "region-select"  → 区域选择覆盖层（独立透明窗口，hash 路由）
// - "viewer"         → 看图窗口（独立 WebviewWindow，hash 路由）
// - "upscale"        → AI 放大窗口（独立 WebviewWindow，hash 路由）
type Route =
  | { name: "main" }
  | { name: "annotation"; img: string }
  | { name: "pin"; img: string; w: number; h: number }
  | { name: "region-select" }
  | { name: "viewer" }
  | { name: "upscale" }
  | { name: "record-osd" }
  | { name: "video-osd" }
  | { name: "anim-toolbar" };

const route = ref<Route>({ name: "main" });

// 贴图窗口和区域选择窗口用 hash 路由（独立窗口）
function parseHash(): Route {
  const h = window.location.hash || "#/";
  const [path, queryStr] = h.slice(1).split("?");
  const query = new URLSearchParams(queryStr || "");
  if (path === "/pin") {
    return {
      name: "pin",
      // URLSearchParams.get 已做 percent 解码，无需再次 decodeURIComponent
      img: query.get("img") || "",
      w: parseFloat(query.get("w") || "400"),
      h: parseFloat(query.get("h") || "300"),
    };
  }
  if (path === "/region-select") {
    return { name: "region-select" };
  }
  if (path === "/viewer") {
    return { name: "viewer" };
  }
  if (path === "/upscale") {
    return { name: "upscale" };
  }
  if (path === "/record-osd") {
    return { name: "record-osd" };
  }
  if (path === "/video-osd") {
    return { name: "video-osd" };
  }
  if (path === "/anim-toolbar") {
    return { name: "anim-toolbar" };
  }
  return { name: "main" };
}

// 标注图像路径（由 Rust 端事件设置）
const annotationImg = ref("");

function onHashChange() {
  const parsed = parseHash();
  if (
    parsed.name === "pin" ||
    parsed.name === "region-select" ||
    parsed.name === "viewer" ||
    parsed.name === "upscale" ||
    parsed.name === "record-osd" ||
    parsed.name === "video-osd" ||
    parsed.name === "anim-toolbar"
  ) {
    route.value = parsed;
  }
}

// 当前视图
const currentView = computed(() => {
  if (annotationImg.value) return "annotation";
  return route.value.name;
});

// Windows 本地路径 → tauri asset URL
function toAssetUrl(p: string): string {
  if (!p) return "";
  if (/^https?:\/\//.test(p)) return p;
  if (/^[A-Za-z]:[\\/]/.test(p)) {
    return convertFileSrc(p);
  }
  return p;
}

const annotationUrl = computed(() => toAssetUrl(annotationImg.value));
// 贴图窗口传原始文件路径（PinWindow 用 plugin-fs readFile 读取，不走 asset 协议）
const pinImg = computed(() =>
  route.value.name === "pin" ? route.value.img : "",
);
const pinW = computed(() => (route.value.name === "pin" ? route.value.w : 400));
const pinH = computed(() => (route.value.name === "pin" ? route.value.h : 300));

// === 事件监听 ===
let unlistenStart: (() => void) | null = null;
let unlistenDone: (() => void) | null = null;

onMounted(async () => {
  onHashChange();
  window.addEventListener("hashchange", onHashChange);

  // 启动动画收尾：主窗口就绪（Vue 已挂载）后淡出移除
  // 最低展示 600ms——太短显得闪烁；WebView 初始化慢时自然覆盖全程
  const splashEl = document.getElementById("app-splash");
  if (splashEl) {
    let splashDone = false;
    const hideSplash = () => {
      if (splashDone) return;
      splashDone = true;
      splashEl.classList.add("splash-out");
      window.setTimeout(() => splashEl.remove(), 420);
    };
    if (document.hidden) {
      // 延迟显示模式（窗口 visible:false + 就绪后 show）：挂载时窗口尚未可见，
      // 等 visibilitychange 翻转后再开始计 600ms，避免启动页还没露脸就倒计时完
      const onVisible = () => {
        if (document.hidden) return;
        document.removeEventListener("visibilitychange", onVisible);
        window.setTimeout(hideSplash, 600);
      };
      document.addEventListener("visibilitychange", onVisible);
    } else {
      const t0 =
        (window as unknown as { __SPLASH_T0?: number }).__SPLASH_T0 ??
        performance.now();
      const wait = Math.max(0, 600 - (performance.now() - t0));
      window.setTimeout(hideSplash, wait);
    }
  }

  // 加载配置 → 初始化主题色（CSS 变量 + Naive 覆盖）
  await loadConfig();
  setThemeColor(config.value.theme_color || "taro");

  // 监听标注开始事件
  unlistenStart = await listen<string>("annotation://start", (ev) => {
    console.log("收到 annotation://start 事件", ev.payload);
    annotationImg.value = ev.payload;
  });

  // 监听标注结束事件
  unlistenDone = await listen("annotation://done", () => {
    console.log("收到 annotation://done 事件");
    annotationImg.value = "";
  });
});

onUnmounted(() => {
  window.removeEventListener("hashchange", onHashChange);
  unlistenStart?.();
  unlistenDone?.();
});
</script>

<template>
  <n-config-provider :theme="naiveTheme" :theme-overrides="activeOverrides">
    <n-message-provider placement="top" :max="4">
      <n-dialog-provider>
        <!-- 主面板 ↔ 设置中心：沉浸式过渡（220ms 淡入上移） -->
        <template v-if="currentView === 'main'">
          <transition name="jb-view" mode="out-in">
            <MainView
              v-if="panelView === 'main'"
              :mode="mode"
              :is-dark="isDark"
              :set-mode="setMode"
              @open-settings="openSettings"
            />
            <SettingsView
              v-else
              :key="settingsSection ?? 'default'"
              :mode="mode"
              :is-dark="isDark"
              :set-mode="setMode"
              :initial-section="settingsSection"
              @back="backToMain"
            />
          </transition>
        </template>
        <AnnotationOverlay
          v-else-if="currentView === 'annotation'"
          :image-url="annotationUrl"
        />
        <PinWindow
          v-else-if="currentView === 'pin'"
          :image-url="pinImg"
          :initial-w="pinW"
          :initial-h="pinH"
        />
        <RegionSelectOverlay
          v-else-if="currentView === 'region-select'"
        />
        <!-- 看图窗口（独立 viewer WebviewWindow，#/viewer） -->
        <ViewerWindow v-else-if="currentView === 'viewer'" />
        <!-- AI 放大窗口（独立 upscale WebviewWindow，#/upscale） -->
        <UpscaleWindow v-else-if="currentView === 'upscale'" />
        <!-- 录制 OSD（独立透明置顶小窗，#/record-osd；游戏内沉浸提示） -->
        <RecordOsd v-else-if="currentView === 'record-osd'" />
        <!-- 视频录制 OSD（独立透明置顶小窗，#/video-osd；MKV 录制沉浸提示） -->
        <VideoOsd v-else-if="currentView === 'video-osd'" />
        <!-- 动画播放悬浮工具栏（独立透明置顶小窗，#/anim-toolbar；JXL 回放控制） -->
        <AnimToolbar v-else-if="currentView === 'anim-toolbar'" />
      </n-dialog-provider>
    </n-message-provider>
  </n-config-provider>
</template>

<style>
:root {
  --jb-radius: 8px;
  --jb-radius-card: 10px;
  --jb-radius-small: 4px;
  --jb-titlebar-h: 32px;
  --jb-gap: 16px;
  /* 主题色默认值（themeColor.ts 会用内联样式动态覆盖） */
  --jb-primary: #c8a2c8;
  --jb-primary-hover: #b88fb8;
  --jb-primary-pressed: #a478a4;
  --jb-mint: #98c9b8;
  --jb-milk: #e8c49e;
  --jb-red: #d89898;
}

html.light,
:root {
  --jb-bg: rgba(252, 248, 250, 0.82);
  /* 2.3 规范卡片色 #FAF8FA（半透保留 Mica 透出） */
  --jb-bg-card: rgba(250, 248, 250, 0.78);
  --jb-bg-card-hover: rgba(250, 248, 250, 0.92);
  /* 2.5 一级文字 #2C2C34 / 2.6 二级 #6E6E7C / 2.7 占位 #A9A9B8 */
  --jb-text: #2c2c34;
  --jb-text-soft: #6e6e7c;
  --jb-text-mute: #a9a9b8;
  --jb-placeholder: #a9a9b8;
  /* 2.8 分割线 #E8E4EC */
  --jb-divider: #e8e4ec;
  --jb-border: rgba(180, 162, 184, 0.22);
  --jb-shadow: 0 2px 12px rgba(184, 143, 184, 0.1);
  --jb-titlebar: rgba(255, 255, 255, 0.32);
  --jb-titlebar-btn-hover: rgba(180, 162, 184, 0.18);
  --jb-close-hover: #e81123;
  --jb-scrollbar: rgba(180, 162, 184, 0.3);
  --jb-switch-track: rgba(180, 162, 184, 0.3);
}

html.dark {
  --jb-bg: rgba(28, 22, 32, 0.78);
  /* 2.4 规范深色卡片 #2A2730 */
  --jb-bg-card: rgba(42, 39, 48, 0.82);
  --jb-bg-card-hover: rgba(58, 54, 66, 0.92);
  /* 2.9 一级文字 #EDEBF2 / 2.10 二级 #A8A5B4 */
  --jb-text: #edebf2;
  --jb-text-soft: #a8a5b4;
  --jb-text-mute: #78758a;
  --jb-placeholder: #78758a;
  /* 2.11 分割线 #423E4B */
  --jb-divider: #423e4b;
  --jb-border: rgba(180, 162, 184, 0.18);
  --jb-shadow: 0 2px 14px rgba(0, 0, 0, 0.32);
  --jb-titlebar: rgba(36, 28, 42, 0.45);
  --jb-titlebar-btn-hover: rgba(180, 162, 184, 0.16);
  --jb-close-hover: #e81123;
  --jb-scrollbar: rgba(180, 162, 184, 0.24);
  --jb-switch-track: rgba(180, 162, 184, 0.22);
}

html,
body,
#app {
  margin: 0;
  padding: 0;
  height: 100%;
  font-family: "Segoe UI Variable", "Segoe UI", "Microsoft YaHei", system-ui,
    sans-serif;
  font-size: 13px;
  /* 3.5 行高 1.4-1.5 */
  line-height: 1.5;
  background: transparent;
  color: var(--jb-text);
}

/* === 主面板 ↔ 设置中心 沉浸式过渡（220ms） === */
.jb-view-enter-active,
.jb-view-leave-active {
  transition: opacity 220ms ease, transform 220ms ease;
}
.jb-view-enter-from {
  opacity: 0;
  transform: translateY(12px);
}
.jb-view-leave-to {
  opacity: 0;
  transform: translateY(-8px);
}

/* === 窗口不可见 → 冻结全部动画（GPU 空转治理） ===
   Chromium 对隐藏窗口仍持续合成 infinite 动画（aurora-breathe / mem-blink /
   polaroid-float 等），msedgewebview2.exe 静默后台 ~6% GPU 的根源。
   main.ts 监听 visibilitychange 挂/摘此类；paused 保留动画进度，可见即续播 */
html.anim-paused *,
html.anim-paused *::before,
html.anim-paused *::after {
  animation-play-state: paused !important;
}
</style>
