<script setup lang="ts">
// 视频录制 OSD（游戏内叠加层）：沉浸式视频录制状态提示
//
// 形态与动图 OSD（RecordOsd.vue）一致：半透明小窗、右上角、置顶、不抢焦点、
// 点击穿透、不入截图（后端创建窗口时设置）。
// 内容：红点呼吸 + 计时 + 已编码帧数/体积；保存中转圈；完成/错误自动淡出。
//
// 交互（全屏游戏中无法点面板按钮）：
// - 停止由全局热键触发（后端 video_stop_hotkey，默认 Alt+F8）
// - OSD 本身仅展示，不接受任何输入
import { ref, computed, onMounted, onUnmounted } from "vue";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";

type Stats = {
  elapsed_ms: number;
  encoded_frames: number;
  dropped_frames: number;
  size_bytes: number;
  encoder: string;
  paused: boolean;
};

const phase = ref<"recording" | "stopping" | "done" | "error">("recording");
const stats = ref<Stats | null>(null);
const resultText = ref("");
const silenceCountdown = ref(0);
let fadeTimer: number | null = null;

const timeText = computed(() => {
  const ms = stats.value?.elapsed_ms ?? 0;
  const sec = Math.floor(ms / 1000);
  const h = Math.floor(sec / 3600);
  const m = Math.floor((sec % 3600) / 60);
  const s = sec % 60;
  return h > 0
    ? `${h}:${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}`
    : `${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}`;
});

const sizeText = computed(() => {
  const b = stats.value?.size_bytes ?? 0;
  if (b >= 1024 * 1024 * 1024) return `${(b / 1024 / 1024 / 1024).toFixed(2)}GB`;
  if (b > 0) return `${(b / 1024 / 1024).toFixed(0)}MB`;
  return "";
});

// 完成态短暂展示后自毁窗口
function scheduleClose() {
  if (fadeTimer) window.clearTimeout(fadeTimer);
  fadeTimer = window.setTimeout(() => {
    try {
      invoke("close_video_osd");
    } catch {
      // 窗口可能已被销毁
    }
  }, 2600);
}

let unStats: (() => void) | null = null;
let unStopping: (() => void) | null = null;
let unDone: (() => void) | null = null;
let unError: (() => void) | null = null;
let unSilence: (() => void) | null = null;
let alwaysOnTopTimer: number | null = null;

onMounted(async () => {
  // 置顶强化：每 2s 重申 always-on-top（对抗全屏游戏把置顶窗挤下去）
  alwaysOnTopTimer = window.setInterval(() => {
    getCurrentWindow().setAlwaysOnTop(true).catch(() => {
      // 窗口销毁竞态：忽略
    });
  }, 2000);

  try {
    unStats = await listen<Stats>("video://stats", (ev) => {
      if (!ev.payload) return;
      if (phase.value === "recording") stats.value = ev.payload;
    });
    // 停止热键按下 → 保存中（nvenc 排空 + MKV finalize，秒级）
    unStopping = await listen("video://stopping", () => {
      phase.value = "stopping";
    });
    unDone = await listen<{
      path: string; duration_ms: number; size_bytes: number;
    }>("video://done", (ev) => {
      const name = ev.payload.path.split(/[\\\/]/).pop() || ev.payload.path;
      const mb = (ev.payload.size_bytes / 1024 / 1024).toFixed(1);
      const sec = (ev.payload.duration_ms / 1000).toFixed(0);
      resultText.value = `已保存 ${name}（${sec}s / ${mb}MB）`;
      phase.value = "done";
      scheduleClose();
    });
    unError = await listen<string>("video://error", (ev) => {
      resultText.value = "录制出错: " + ev.payload;
      phase.value = "error";
      scheduleClose();
    });
    // 静默倒计时（>0 = 显示剩余秒；0 = 取消/隐藏）
    unSilence = await listen<number>("video://silence-countdown", (ev) => {
      silenceCountdown.value = ev.payload > 0 ? ev.payload : 0;
    });
  } catch {
    // 事件不可用：保持计时显示
  }
});

onUnmounted(() => {
  unStats?.();
  unStopping?.();
  unDone?.();
  unError?.();
  unSilence?.();
  if (alwaysOnTopTimer) window.clearInterval(alwaysOnTopTimer);
  if (fadeTimer) window.clearTimeout(fadeTimer);
});
</script>

<template>
  <div class="osd-root" :class="phase">
    <!-- 录制中：红点 + 计时 + 体积（暂停 = 琥珀静止点 + 已暂停标记） -->
    <template v-if="phase === 'recording'">
      <span class="osd-dot" :class="{ paused: stats?.paused }"></span>
      <span class="osd-time">{{ timeText }}</span>
      <span v-if="stats?.paused" class="osd-paused">已暂停</span>
      <span v-if="sizeText" class="osd-count">{{ sizeText }}</span>
      <span v-if="stats && stats.dropped_frames > 0" class="osd-warn">
        丢{{ stats.dropped_frames }}
      </span>
      <!-- 静默倒计时：无声 N 秒后自动停止（声音恢复自动取消） -->
      <span v-if="silenceCountdown > 0 && !stats?.paused" class="osd-silence">
        静默 {{ silenceCountdown }}s 后停止
      </span>
    </template>

    <!-- 保存中（停止后）：转圈 -->
    <template v-else-if="phase === 'stopping'">
      <span class="osd-spinner"></span>
      <span class="osd-encoding">正在保存…</span>
    </template>

    <!-- 完成/错误：结果文案淡出 -->
    <template v-else>
      <span :class="['osd-result', phase]">{{ resultText }}</span>
    </template>
  </div>
</template>

<style scoped>
.osd-root {
  display: inline-flex;
  align-items: center;
  gap: 8px;
  padding: 6px 14px;
  border-radius: 10px;
  /* 深色半透明胶囊：任何游戏画面上都可读 */
  background: rgba(18, 18, 22, 0.72);
  backdrop-filter: blur(14px) saturate(1.3);
  border: 1px solid rgba(255, 255, 255, 0.09);
  box-shadow: 0 4px 18px rgba(0, 0, 0, 0.35);
  font-family: "Segoe UI Variable", "Segoe UI", system-ui, sans-serif;
  color: #f2f2f5;
  -webkit-font-smoothing: antialiased;
  transition: opacity 400ms ease;
}

.osd-root.done,
.osd-root.error {
  opacity: 0.96;
}

/* 红点呼吸（录制中）；暂停 = 琥珀静止 */
.osd-dot {
  width: 9px;
  height: 9px;
  border-radius: 50%;
  background: #ff5257;
  box-shadow: 0 0 8px rgba(255, 82, 87, 0.7);
  animation: osd-pulse 1.2s ease-in-out infinite;
}
.osd-dot.paused {
  background: #f5a623;
  box-shadow: 0 0 8px rgba(245, 166, 35, 0.6);
  animation: none;
}
.osd-paused {
  font-size: 11px;
  padding: 1px 7px;
  border-radius: 4px;
  background: rgba(245, 166, 35, 0.22);
  color: #ffc861;
}
@keyframes osd-pulse {
  0%,
  100% {
    opacity: 1;
  }
  50% {
    opacity: 0.3;
  }
}

.osd-time {
  font-size: 17px;
  font-weight: 600;
  font-variant-numeric: tabular-nums;
  letter-spacing: 0.5px;
}
.osd-count {
  font-size: 12px;
  color: rgba(242, 242, 245, 0.62);
  font-variant-numeric: tabular-nums;
}
.osd-warn {
  font-size: 11px;
  color: #ffb86c;
}

/* 静默倒计时（琥珀醒目） */
.osd-silence {
  font-size: 11px;
  color: #ffcf6e;
  font-weight: 600;
}

/* 保存中转圈（细环） */
.osd-spinner {
  width: 14px;
  height: 14px;
  border-radius: 50%;
  border: 2px solid rgba(255, 255, 255, 0.22);
  border-top-color: #fff;
  animation: osd-spin 0.8s linear infinite;
}
@keyframes osd-spin {
  to {
    transform: rotate(360deg);
  }
}
.osd-encoding {
  font-size: 13px;
  color: rgba(242, 242, 245, 0.88);
}

.osd-result {
  font-size: 12.5px;
  max-width: 360px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.osd-result.done {
  color: #8ce99a;
}
.osd-result.error {
  color: #ff8789;
}
</style>
