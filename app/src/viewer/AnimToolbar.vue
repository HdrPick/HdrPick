<script setup lang="ts">
// 动画播放悬浮工具栏（独立透明置顶小窗，#/anim-toolbar）
//
// 形态：半透明圆角深色条，悬浮于播放窗口底部居中（后端跟随线程对齐）；
// 光标移入播放窗口或工具栏自身时显示，移开后 600ms 淡出；暂停态常显。
// 交互：按钮 → invoke anim_toolbar_action → PostMessage 播放窗口（不抢焦点）。
import { ref, onMounted, onUnmounted, nextTick } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { PhysicalSize } from "@tauri-apps/api/dpi";

// 动作码（与后端 toolbar_action 一一对应）
const A = {
  PAUSE: 1,
  B1: 2, F1: 3,
  B3: 4, F3: 5,
  B5: 6, F5: 7,
  B10: 8, F10: 9,
  B1S: 10, F1S: 11,
  B3S: 12, F3S: 13,
  PNG: 14, JXL: 15,
  FULL: 16,
} as const;

const paused = ref(false);
const visible = ref(true);
const selfHover = ref(false);
let pollTimer: number | null = null;
let hideTimer: number | null = null;

function act(action: number) {
  invoke("anim_toolbar_action", { action }).catch(() => {});
}

// 自适应窗口宽度：按钮总宽超出窗口时右侧被裁（"JXL" 只剩 "JX"）。
// 测量陷阱：.tb-bar 是 flex 子项默认 flex-shrink:1——内容超窗宽时被压回
// 窗口宽，getBoundingClientRect 量到的永远是"当前窗口宽"（鸡生蛋）。
// 正解：① CSS 给 .tb-bar 加 flex-shrink:0（保持自然宽溢出可见）
// ② 量 scrollWidth（不受容器裁剪影响）③ 字体就绪后复测（主题字体晚到）。
async function fitWindowToContent() {
  await nextTick();
  const bar = document.querySelector<HTMLElement>(".tb-bar");
  if (!bar) return;
  try {
    const win = getCurrentWindow();
    const scale = await win.scaleFactor();
    // scrollWidth = 未压缩内容宽（按钮 nowrap 不折行）；+4 边框余量
    const cssW = Math.max(bar.scrollWidth, bar.getBoundingClientRect().width);
    const w = Math.ceil(cssW * scale) + 4;
    const h = await win.outerSize().then((s) => s.height);
    await win.setSize(new PhysicalSize(w, h));
  } catch {
    // 窗口竞态忽略
  }
}

onMounted(() => {
  // 自适应宽度（窗口创建时 560 预设 → 实测按钮总宽调整）
  fitWindowToContent();
  // 主题字体异步就绪后宽度会变 → 复测（300ms 后二跑兜底布局晚稳定）
  if (document.fonts?.ready) {
    document.fonts.ready.then(() => fitWindowToContent());
  }
  window.setTimeout(() => fitWindowToContent(), 300);
  // 状态轮询：光标在播放窗口上（后端 WindowFromPoint 判定）+ 暂停镜像
  pollTimer = window.setInterval(async () => {
    try {
      const st = await invoke<{ over: boolean; paused: boolean }>("anim_toolbar_state");
      paused.value = st.paused;
      const over = st.over || selfHover.value || paused.value; // 暂停时常显
      if (over) {
        visible.value = true;
        if (hideTimer) { window.clearTimeout(hideTimer); hideTimer = null; }
      } else if (visible.value && !hideTimer) {
        hideTimer = window.setTimeout(() => {
          visible.value = false;
          hideTimer = null;
        }, 600);
      }
    } catch {
      // 播放窗口已关（命令无目标）→ 忽略
    }
  }, 250);
});

onUnmounted(() => {
  if (pollTimer) window.clearInterval(pollTimer);
  if (hideTimer) window.clearTimeout(hideTimer);
});
</script>

<template>
  <div
    class="tb-root"
    :class="{ 'tb-hidden': !visible }"
    @mouseenter="selfHover = true"
    @mouseleave="selfHover = false"
  >
    <div class="tb-bar">
      <!-- 播放/暂停 -->
      <button class="tb-btn tb-main" :title="paused ? '播放（空格）' : '暂停（空格）'" @click="act(A.PAUSE)">
        <span v-if="paused">▶</span>
        <span v-else>❚❚</span>
      </button>
      <span class="tb-sep" />
      <!-- 帧步进：−10 −5 −3 −1 | +1 +3 +5 +10 -->
      <button class="tb-btn" title="后退 10 帧（Ctrl+Shift+←）" @click="act(A.B10)">−10</button>
      <button class="tb-btn" title="后退 5 帧（Shift+←）" @click="act(A.B5)">−5</button>
      <button class="tb-btn" title="后退 3 帧（Ctrl+←）" @click="act(A.B3)">−3</button>
      <button class="tb-btn" title="后退 1 帧（←）" @click="act(A.B1)">−1</button>
      <button class="tb-btn" title="前进 1 帧（→）" @click="act(A.F1)">+1</button>
      <button class="tb-btn" title="前进 3 帧（Ctrl+→）" @click="act(A.F3)">+3</button>
      <button class="tb-btn" title="前进 5 帧（Shift+→）" @click="act(A.F5)">+5</button>
      <button class="tb-btn" title="前进 10 帧（Ctrl+Shift+→）" @click="act(A.F10)">+10</button>
      <span class="tb-sep" />
      <!-- 秒级：−3s −1s | +1s +3s -->
      <button class="tb-btn" title="后退 3 秒（Shift+PgUp）" @click="act(A.B3S)">−3s</button>
      <button class="tb-btn" title="后退 1 秒（PgUp）" @click="act(A.B1S)">−1s</button>
      <button class="tb-btn" title="前进 1 秒（PgDn）" @click="act(A.F1S)">+1s</button>
      <button class="tb-btn" title="前进 3 秒（Shift+PgDn）" @click="act(A.F3S)">+3s</button>
      <span class="tb-sep" />
      <!-- 导出 / 全屏 -->
      <button class="tb-btn tb-export" title="导出当前帧为 HDR PNG（PrtSc）" @click="act(A.PNG)">PNG</button>
      <button class="tb-btn tb-export" title="导出当前帧为 HDR JXL 单图像（Ctrl+PrtSc）" @click="act(A.JXL)">JXL</button>
      <button class="tb-btn" title="全屏切换（F11）" @click="act(A.FULL)">⛶</button>
    </div>
  </div>
</template>

<style scoped>
.tb-root {
  width: 100vw;
  height: 100vh;
  display: flex;
  align-items: flex-end;
  justify-content: center;
  padding-bottom: 4px;
  background: transparent;
  transition: opacity 0.25s ease;
  opacity: 1;
}
.tb-hidden {
  opacity: 0;
  pointer-events: none;
}
.tb-bar {
  display: flex;
  align-items: center;
  flex-shrink: 0; /* 关键：保持自然内容宽（flex 默认 shrink 会压回窗宽 → 测量鸡生蛋 + 按钮被裁） */
  gap: 3px;
  padding: 6px 12px;
  border-radius: 12px;
  background: rgba(20, 22, 28, 0.82);
  backdrop-filter: blur(14px);
  box-shadow: 0 4px 16px rgba(0, 0, 0, 0.45);
}
.tb-btn {
  min-width: 34px;
  height: 30px;
  padding: 0 8px;
  border: none;
  border-radius: 8px;
  background: transparent;
  color: rgba(235, 238, 245, 0.92);
  font-size: 13px;
  line-height: 1;
  white-space: nowrap; /* 禁折行：保证 scrollWidth = 完整内容宽 */
  flex-shrink: 0;
  cursor: pointer;
  transition: background 0.15s ease, color 0.15s ease;
  user-select: none;
}
.tb-btn:hover {
  background: rgba(255, 255, 255, 0.14);
  color: #fff;
}
.tb-btn:active {
  background: rgba(255, 255, 255, 0.24);
  transform: translateY(1px);
}
.tb-main {
  font-size: 15px;
  min-width: 40px;
}
.tb-sep {
  width: 1px;
  height: 18px;
  margin: 0 4px;
  background: rgba(255, 255, 255, 0.18);
}
.tb-export {
  font-size: 12px;
  font-weight: 600;
  letter-spacing: 0.3px;
}
</style>
