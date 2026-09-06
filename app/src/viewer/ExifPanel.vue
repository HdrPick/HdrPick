<script setup lang="ts">
// 右侧 EXIF 抽屉：分组展示（设备/曝光/焦距/ISO/光圈/快门/GPS/时间，由后端 get_exif 返回）
// 滑出动画：translateX(100%) → 0 + visibility 过渡（等价 v-show + translate，离场动画结束后才隐藏）
import { NIcon } from "naive-ui";
import { X, Copy } from "@vicons/tabler";
import { useMessage } from "naive-ui";

interface ExifItem {
  label: string;
  value: string;
}
interface ExifGroup {
  title: string;
  items: ExifItem[];
}
interface ExifData {
  groups: ExifGroup[];
}

const emit = defineEmits<{
  (e: "close"): void;
}>();

const props = defineProps<{
  /** get_exif 返回的数据（null = 未获取或后端未就绪） */
  exif: ExifData | null;
  /** 抽屉展开态 */
  visible: boolean;
}>();

const message = useMessage();

/** 一键复制全部 EXIF 参数为多行文本（设计稿 3.1：信息预览支持一键复制参数） */
async function copyAll() {
  if (!props.exif || props.exif.groups.length === 0) return;
  const text = props.exif.groups
    .map((g) => `[${g.title}]\n${g.items.map((it) => `${it.label}: ${it.value}`).join("\n")}`)
    .join("\n\n");
  try {
    await navigator.clipboard.writeText(text);
    message.success("已复制全部 EXIF 参数", { duration: 2200 });
  } catch {
    message.error("复制失败", { duration: 2200 });
  }
}
</script>

<template>
  <div class="exif-panel" :class="{ visible }">
    <div class="exif-head">
      <span class="exif-title">EXIF 信息</span>
      <div class="exif-actions">
        <button
          class="exif-close"
          title="复制全部参数"
          :disabled="!exif || exif.groups.length === 0"
          @click="copyAll"
        >
          <n-icon :component="Copy" size="14" />
        </button>
        <button class="exif-close" title="关闭" @click="emit('close')">
          <n-icon :component="X" size="14" />
        </button>
      </div>
    </div>
    <div class="exif-body">
      <template v-if="exif && exif.groups.length > 0">
        <div v-for="g in exif.groups" :key="g.title" class="exif-group">
          <div class="exif-group-title">{{ g.title }}</div>
          <div v-for="it in g.items" :key="it.label" class="exif-row">
            <span class="exif-label">{{ it.label }}</span>
            <span class="exif-value">{{ it.value }}</span>
          </div>
        </div>
      </template>
      <div v-else class="exif-empty">此图片没有 EXIF 信息</div>
    </div>
  </div>
</template>

<style scoped>
/* 右侧滑出抽屉：320px，Mica 卡片底 + 毛玻璃 */
.exif-panel {
  position: absolute;
  top: 0;
  right: 0;
  bottom: 0;
  width: 320px;
  display: flex;
  flex-direction: column;
  background: color-mix(in srgb, var(--jb-bg-card) 92%, transparent);
  backdrop-filter: blur(16px) saturate(1.3);
  border-left: 1px solid var(--jb-border);
  box-shadow: var(--jb-shadow);
  transform: translateX(100%);
  visibility: hidden; /* 隐藏时不拦截主区交互 */
  transition: transform 220ms ease, visibility 220ms ease;
  z-index: 20;
}
.exif-panel.visible {
  transform: translateX(0);
  visibility: visible;
}
.exif-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 12px 14px;
  border-bottom: 1px solid var(--jb-divider);
  flex-shrink: 0;
}
.exif-title {
  font-size: 13px;
  font-weight: 600;
  color: var(--jb-text);
}
.exif-actions {
  display: flex;
  align-items: center;
  gap: 4px;
}
.exif-close {
  width: 26px;
  height: 26px;
  border: none;
  border-radius: 6px;
  background: transparent;
  color: var(--jb-text-soft);
  cursor: pointer;
  display: flex;
  align-items: center;
  justify-content: center;
  transition: background-color 0.15s;
}
.exif-close:hover:not(:disabled) {
  background: var(--jb-titlebar-btn-hover);
}
.exif-close:disabled {
  opacity: 0.4;
  cursor: default;
}
.exif-body {
  flex: 1;
  overflow-y: auto;
  padding: 12px 14px 16px;
  display: flex;
  flex-direction: column;
  gap: 14px;
  scrollbar-width: thin;
  scrollbar-color: var(--jb-scrollbar) transparent;
}
.exif-group {
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.exif-group-title {
  font-size: 11px;
  font-weight: 600;
  color: var(--jb-primary);
  letter-spacing: 1px;
  margin-bottom: 2px;
}
.exif-row {
  display: flex;
  justify-content: space-between;
  gap: 12px;
  font-size: 12px;
  line-height: 1.6;
}
.exif-label {
  color: var(--jb-text-mute);
  flex-shrink: 0;
}
.exif-value {
  color: var(--jb-text);
  text-align: right;
  word-break: break-all;
}
.exif-empty {
  margin-top: 24px;
  text-align: center;
  font-size: 12px;
  color: var(--jb-text-mute);
  user-select: none;
}
</style>
