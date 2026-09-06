<script setup lang="ts">
// 录制 OSD（游戏内叠加层）：沉浸式录制状态提示
//
// 形态：半透明小窗、右上角、置顶、不抢焦点、不拦截鼠标（点击穿透）、
// 不入截图（WDA_EXCLUDEFROMCAPTURE，由后端创建窗口时设置）。
// 内容：红点呼吸 + 启动倒计时 / 计时 + 剩余倒计时；编码中转圈；完成/错误自动淡出。
//
// 交互（全屏游戏中无法点面板按钮）：
// - 停止/取消由全局热键触发（后端 record_stop_hotkey）
// - OSD 本身仅展示，不接受任何输入
import { ref, computed, onMounted, onUnmounted } from "vue";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";

type Stats = {
  elapsed_ms: number;
  captured_frames: number;
  deduped_frames: number;
  ring_bytes: number;
};

const phase = ref<"countdown" | "recording" | "encoding" | "done" | "error">("recording");
const stats = ref<Stats | null>(null);
const maxSeconds = 30;
const resultText = ref("");
let fadeTimer: number | null = null;
// 延迟启动倒计时秒数（record://countdown 驱动）
const startCountdown = ref(0);

const timeText = computed(() => {
  const ms = stats.value?.elapsed_ms ?? 0;
  const sec = Math.floor(ms / 1000);
  return `${String(Math.floor(sec / 60)).padStart(2, "0")}:${String(sec % 60).padStart(2, "0")}`;
});

const countdown = computed(() =>
  Math.max(0, maxSeconds - Math.floor((stats.value?.elapsed_ms ?? 0) / 1000)),
);

// 完成态短暂展示后自毁窗口（后端监听销毁；此处仅 UI 淡出）
function scheduleClose() {
  if (fadeTimer) window.clearTimeout(fadeTimer);
  fadeTimer = window.setTimeout(() => {
    phase.value = "done";
    try {
      invoke("close_record_osd");
    } catch {
      // 窗口可能已被销毁
    }
  }, 2600);
}

let unStats: (() => void) | null = null;
let unDone: (() => void) | null = null;
let unError: (() => void) | null = null;
let unEncoding: (() => void) | null = null;
let unCountdown: (() => void) | null = null;
// 置顶强化定时器（onUnmounted 清除）
let alwaysOnTopTimer: number | null = null;

onMounted(async () => {
  // 定位由后端创建窗口时完成（backend owns positioning）；
  // 前端自定位与后端互相覆盖曾导致 OSD 落点错乱，此处不再干预。

  // 置顶强化：每 2s 重申 always-on-top——无边框全屏/独占全屏游戏可能把
  // 置顶窗口挤下去（"时间消失"），周期性重申温和对抗（不用 Win32 轮询）
  alwaysOnTopTimer = window.setInterval(() => {
    getCurrentWindow().setAlwaysOnTop(true).catch(() => {
      // 窗口销毁竞态：忽略
    });
  }, 2000);

  try {
    // 延迟启动倒计时（游戏模式）：n 秒后开始录制；seconds=0 = 结束切回计时态
    unCountdown = await listen<{ seconds: number }>("record://countdown", (ev) => {
      const s = ev.payload?.seconds ?? 0;
      if (s > 0) {
        if (phase.value === "recording" || phase.value === "countdown") {
          startCountdown.value = s;
          phase.value = "countdown";
        }
      } else if (phase.value === "countdown") {
        phase.value = "recording";
      }
    });
    unStats = await listen<Stats>("record://stats", (ev) => {
      if (!ev.payload) return;
      // 首个统计到达 = 抓帧已开始（兜底：countdown 结束事件丢失也能切回计时态）
      if (phase.value === "countdown") phase.value = "recording";
      if (phase.value === "recording") stats.value = ev.payload;
    });
    // autostop/done/error：后端事件直达（全屏游戏中原面板不可见）
    unEncoding = await listen("record://autostop", () => {
      phase.value = "encoding";
      // 关键：游戏模式下面板最小化、无人在调 record_stop——
      // 到达 30s 上限后由 OSD 驱动 finalize（record_stop 阻塞至编码完成，
      // async command 不冻结 UI）；结果经 record://done/error 事件回到这里展示
      invoke("record_stop").catch((e) => {
        resultText.value = "录制失败: " + e;
        phase.value = "error";
        scheduleClose();
      });
    });
    unDone = await listen<{
      path: string; frames: number; output_bytes: number;
    }>("record://done", (ev) => {
      const name = ev.payload.path.split(/[\\\/]/).pop() || ev.payload.path;
      const mb = (ev.payload.output_bytes / 1024 / 1024).toFixed(1);
      resultText.value = `已保存 ${name}（${ev.payload.frames} 帧 / ${mb}MB）`;
      phase.value = "done";
      scheduleClose();
    });
    unError = await listen<string>("record://error", (ev) => {
      resultText.value = "录制出错: " + ev.payload;
      phase.value = "error";
      scheduleClose();
    });
  } catch {
    // 事件不可用：保持计时显示
  }
});

onUnmounted(() => {
  unStats?.();
  unDone?.();
  unError?.();
  unEncoding?.();
  unCountdown?.();
  if (alwaysOnTopTimer) window.clearInterval(alwaysOnTopTimer);
  if (fadeTimer) window.clearTimeout(fadeTimer);
});
</script>

<template>
  <div class="osd-root" :class="phase">
    <!-- 延迟启动倒计时：红点呼吸 + N 秒后开始录制 -->
    <template v-if="phase === 'countdown'">
      <span class="osd-dot"></span>
      <span class="osd-start-countdown">{{ startCountdown }} 秒后开始录制…</span>
    </template>

    <!-- 录制中：红点 + 计时 + 倒计时 -->
    <template v-else-if="phase === 'recording'">
      <span class="osd-dot"></span>
      <span class="osd-time">{{ timeText }}</span>
      <span class="osd-count">{{ countdown }}s</span>
    </template>

    <!-- 编码中（停止后）：转圈 -->
    <template v-else-if="phase === 'encoding'">
      <span class="osd-spinner"></span>
      <span class="osd-encoding">正在编码…</span>
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

/* 红点呼吸（录制中） */
.osd-dot {
  width: 9px;
  height: 9px;
  border-radius: 50%;
  background: #ff5257;
  box-shadow: 0 0 8px rgba(255, 82, 87, 0.7);
  animation: osd-pulse 1.2s ease-in-out infinite;
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
/* 延迟启动倒计时文案 */
.osd-start-countdown {
  font-size: 13px;
  color: rgba(242, 242, 245, 0.88);
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
}
.osd-count {
  font-size: 12px;
  color: rgba(242, 242, 245, 0.62);
  font-variant-numeric: tabular-nums;
}

/* 编码转圈（细环） */
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
  max-width: 340px;
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
