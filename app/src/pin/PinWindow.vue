<script setup lang="ts">
import { ref, onMounted, onUnmounted } from "vue";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { invoke } from "@tauri-apps/api/core";

const props = defineProps<{
  imageUrl: string;
  initialW: number;
  initialH: number;
}>();

const win = getCurrentWindow();
const loaded = ref(false);
const error = ref("");
const imgWidth = ref(0);
const imgHeight = ref(0);
const displaySrc = ref(""); // 实际显示的 data URL

// 控制点显示状态
const hoverControls = ref(false);
let hoverTimer: number | null = null;

function logToBackend(msg: string) {
  invoke("frontend_log", { msg: `[PinWindow] ${msg}` }).catch(() => {});
}

function showControls() {
  hoverControls.value = true;
  if (hoverTimer) window.clearTimeout(hoverTimer);
  hoverTimer = window.setTimeout(() => {
    hoverControls.value = false;
  }, 1500);
}

// 置顶切换
const alwaysOnTop = ref(true);
async function toggleAlwaysOnTop() {
  alwaysOnTop.value = !alwaysOnTop.value;
  await win.setAlwaysOnTop(alwaysOnTop.value);
  showControls();
}

// 缩放
const currentScale = ref(1);
function onWheel(e: WheelEvent) {
  e.preventDefault();
  const delta = e.deltaY > 0 ? -0.05 : 0.05;
  currentScale.value = Math.max(0.1, Math.min(5, currentScale.value + delta));
  applyScale();
  showControls();
}
function applyScale() {
  // 由 CSS transform: scale 处理
}

// 关闭
async function closePin() {
  await win.close();
}

// 拖动整窗
async function startDrag() {
  await win.startDragging();
}

function onImgLoad(e: Event) {
  const img = e.target as HTMLImageElement;
  imgWidth.value = img.naturalWidth;
  imgHeight.value = img.naturalHeight;
  loaded.value = true;
  logToBackend(`图片加载成功 ${img.naturalWidth}x${img.naturalHeight}`);
}

function onImgError() {
  error.value = "贴图加载失败";
  logToBackend(`图片加载失败 src=${displaySrc.value.slice(0, 100)}`);
}

// 通过 fs 插件读取文件为 base64 data URL，避免 asset 协议在新窗口中不可用
async function loadImageAsDataURL(path: string) {
  logToBackend(`loadImageAsDataURL path=${path}`);
  try {
    const { readFile } = await import("@tauri-apps/plugin-fs");
    const bytes = await readFile(path);
    logToBackend(`读取文件成功，字节数=${bytes.length}`);
    const blob = new Blob([bytes], { type: "image/png" });
    const dataUrl = await new Promise<string>((resolve, reject) => {
      const fr = new FileReader();
      fr.onload = () => resolve(fr.result as string);
      fr.onerror = () => reject(new Error("FileReader 失败"));
      fr.readAsDataURL(blob);
    });
    logToBackend(`data URL 长度=${dataUrl.length}`);
    return dataUrl;
  } catch (e) {
    logToBackend(`loadImageAsDataURL 失败: ${e}`);
    throw e;
  }
}

onMounted(async () => {
  // 默认置顶
  win.setAlwaysOnTop(true);
  // 鼠标进入显示控件
  window.addEventListener("mousemove", showControls);
  window.addEventListener("wheel", onWheel, { passive: false });

  logToBackend(`onMounted imageUrl=${props.imageUrl}`);
  if (props.imageUrl) {
    try {
      displaySrc.value = await loadImageAsDataURL(props.imageUrl);
    } catch {
      error.value = "贴图加载失败";
    }
  }
});
onUnmounted(() => {
  window.removeEventListener("mousemove", showControls);
  window.removeEventListener("wheel", onWheel);
  if (hoverTimer) window.clearTimeout(hoverTimer);
});
</script>

<template>
  <div class="pin-root" :class="{ 'has-controls': hoverControls }" @mousedown.left="startDrag">
    <img
      v-if="!error && displaySrc"
      :src="displaySrc"
      class="pin-image"
      :style="{ transform: `scale(${currentScale})` }"
      draggable="false"
      @load="onImgLoad"
      @error="onImgError"
    />
    <div v-else-if="error" class="pin-error">{{ error }}</div>
    <div v-else class="pin-loading">加载中...</div>

    <!-- 控制点（右上角） -->
    <div class="controls" :class="{ visible: hoverControls }" @mousedown.stop>
      <button
        class="ctrl-btn"
        :class="{ active: alwaysOnTop }"
        title="置顶"
        @click="toggleAlwaysOnTop"
      >
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M12 17v5" />
          <path d="M9 10.76V6a2 2 0 0 1 4 0v4.76a3 3 0 1 1-4 0z" />
        </svg>
      </button>
      <button class="ctrl-btn" title="关闭" @click="closePin">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <line x1="18" y1="6" x2="6" y2="18" />
          <line x1="6" y1="6" x2="18" y2="18" />
        </svg>
      </button>
    </div>

    <!-- 缩放比例（左下角） -->
    <div v-if="hoverControls" class="scale-hint">
      {{ Math.round(currentScale * 100) }}%
    </div>
  </div>
</template>

<style scoped>
.pin-root {
  position: fixed;
  inset: 0;
  overflow: hidden;
  border-radius: 12px;
  user-select: none;
  cursor: grab;
}
.pin-root:active {
  cursor: grabbing;
}

.pin-image {
  display: block;
  width: 100%;
  height: 100%;
  object-fit: contain;
  transform-origin: center center;
  transition: transform 0.05s ease-out;
  pointer-events: none;
}

.pin-error {
  display: flex;
  align-items: center;
  justify-content: center;
  height: 100%;
  color: #fff;
  background: rgba(216, 152, 152, 0.3);
  font-size: 12px;
}

.pin-loading {
  display: flex;
  align-items: center;
  justify-content: center;
  height: 100%;
  color: #fff;
  background: rgba(200, 162, 200, 0.3);
  font-size: 12px;
}

/* === 控制点 === */
.controls {
  position: absolute;
  top: 6px;
  right: 6px;
  display: flex;
  gap: 4px;
  opacity: 0;
  transition: opacity 0.22s;
}
.controls.visible {
  opacity: 1;
}

.ctrl-btn {
  width: 22px;
  height: 22px;
  display: flex;
  align-items: center;
  justify-content: center;
  border: none;
  background: rgba(40, 36, 46, 0.6);
  backdrop-filter: blur(10px);
  color: #fff;
  cursor: pointer;
  border-radius: 6px;
  padding: 0;
  transition: background-color 0.15s;
}
.ctrl-btn:hover {
  background: rgba(40, 36, 46, 0.9);
}
.ctrl-btn.active {
  background: rgba(200, 162, 200, 0.7);
}

.scale-hint {
  position: absolute;
  bottom: 6px;
  left: 6px;
  padding: 2px 8px;
  background: rgba(40, 36, 46, 0.7);
  color: #fff;
  border-radius: 10px;
  font-size: 11px;
  backdrop-filter: blur(10px);
}
</style>
