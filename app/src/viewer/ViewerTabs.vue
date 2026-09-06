<script setup lang="ts">
// 浏览器式标签栏（UI 细节：多文件打开各占一 tab，重开同文件切回）
// 纯展示组件：切换/关闭全部 emit 给 ViewerWindow 编排
import { NIcon } from "naive-ui";
import { X } from "@vicons/tabler";

/** 单个标签的展示数据（由 ViewerTab 派生） */
export interface ViewerTabItem {
  /** 唯一键（图片路径） */
  key: string;
  /** 标签标题（文件名） */
  title: string;
  /** 悬浮提示（完整路径） */
  tip: string;
}

defineProps<{
  tabs: ViewerTabItem[];
  /** 激活标签下标 */
  active: number;
}>();

const emit = defineEmits<{
  /** 切换到指定标签 */
  (e: "select", idx: number): void;
  /** 关闭指定标签 */
  (e: "close", idx: number): void;
}>();

/** 中键关闭（浏览器习惯，auxclick 不触发 click） */
function onAuxclick(idx: number, e: MouseEvent) {
  if (e.button === 1) {
    e.preventDefault();
    emit("close", idx);
  }
}
</script>

<template>
  <div class="vtab-bar">
    <div
      v-for="(t, i) in tabs"
      :key="t.key"
      class="vtab"
      :class="{ active: i === active }"
      :title="t.tip"
      @click="emit('select', i)"
      @auxclick="onAuxclick(i, $event)"
    >
      <span class="vtab-title">{{ t.title }}</span>
      <button
        class="vtab-close"
        title="关闭（中键亦可）"
        @click.stop="emit('close', i)"
      >
        <n-icon :component="X" size="12" />
      </button>
    </div>
  </div>
</template>

<style scoped>
/* 标签栏：TitleBar 下方一行，Acrylic 材质，仅多标签时显示 */
.vtab-bar {
  display: flex;
  align-items: flex-end;
  gap: 4px;
  height: 36px;
  flex-shrink: 0;
  padding: 4px 10px 0;
  background: color-mix(in srgb, var(--jb-bg-card) 72%, transparent);
  backdrop-filter: blur(16px) saturate(1.3);
  border-bottom: 1px solid var(--jb-border);
  overflow-x: auto;
  scrollbar-width: none;
  user-select: none;
}
.vtab-bar::-webkit-scrollbar {
  display: none; /* tab 少时不显滚动条，超出可拖拽滚动 */
}
.vtab {
  display: flex;
  align-items: center;
  gap: 6px;
  max-width: 180px;
  min-width: 96px;
  height: 30px;
  padding: 0 6px 0 12px;
  border-radius: 8px 8px 0 0;
  border: 1px solid transparent;
  border-bottom: none;
  background: transparent;
  color: var(--jb-text-mute);
  font-size: 12px;
  cursor: pointer;
  flex-shrink: 0;
  transition: background-color 180ms ease, color 180ms ease, border-color 180ms ease;
}
.vtab:hover {
  background: color-mix(in srgb, var(--jb-bg) 55%, transparent);
  color: var(--jb-text-soft);
}
/* 激活：卡片底色 + 主题色顶部指示条（浏览器式当前页） */
.vtab.active {
  background: var(--jb-bg-card);
  border-color: var(--jb-border);
  color: var(--jb-text);
  box-shadow: inset 0 2px 0 -1px var(--jb-primary);
}
.vtab-title {
  flex: 1;
  min-width: 0;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.vtab-close {
  width: 18px;
  height: 18px;
  flex-shrink: 0;
  border: none;
  border-radius: 5px;
  background: transparent;
  color: inherit;
  opacity: 0; /* hover 标签才显示，保持简洁 */
  cursor: pointer;
  display: flex;
  align-items: center;
  justify-content: center;
  transition: opacity 150ms ease, background-color 150ms ease;
}
.vtab:hover .vtab-close,
.vtab.active .vtab-close {
  opacity: 0.75;
}
.vtab-close:hover {
  opacity: 1;
  background: var(--jb-titlebar-btn-hover);
}
</style>
