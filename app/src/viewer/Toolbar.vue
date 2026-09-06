<script setup lang="ts">
// 看图窗口顶部浮动工具栏（44px Acrylic 毛玻璃条）
// 纯展示组件：所有动作 emit 给 ViewerWindow 编排
import { NIcon } from "naive-ui";
import {
  Folder,
  LayoutGrid,
  ChevronLeft,
  ChevronRight,
  ZoomIn,
  ZoomOut,
  ArrowsMaximize,
  Rotate2,
  RotateClockwise,
  Maximize,
  Minimize,
  InfoCircle,
  Download,
  Copy,
  Wallpaper,
  Printer,
  PlayerPlay,
  PlayerStop,
  Trash,
  Edit,
  Sun,
  Bolt,
  Wand,
  Stars,
} from "@vicons/tabler";

defineProps<{
  /** 当前缩放比例（0.1-8，用于百分比显示） */
  scale: number;
  /** 是否存在上一张 */
  canPrev: boolean;
  /** 是否存在下一张 */
  canNext: boolean;
  /** 全屏（最大化）状态：切换全屏图标 */
  fullscreen: boolean;
  /** EXIF 面板展开态（按钮激活高亮） */
  exifActive: boolean;
  /** 幻灯片放映中（按钮切换图标） */
  slideshow: boolean;
  /** HDR 亮度调节面板展开态（按钮激活高亮；仅 HDR 源显示） */
  toneActive: boolean;
  /** AI 放大面板展开态（按钮激活高亮） */
  upscaleActive: boolean;
  /** 当前图为 HDR 源（EXR/HDR PNG/JXL/JXR：显示亮度调节按钮） */
  isHdr: boolean;
  /** 可用真 HDR 查看（HDR 源 + 系统存在已开启 HDR 的显示器） */
  hdrAvailable: boolean;
  /** 真 HDR 嵌入模式进行中（Bolt 高亮为退出态；隐藏作用于 SDR 预览的按钮） */
  hdrEmbedded: boolean;
  /** HDR 显示器峰值亮度（nits；嵌入模式在工具栏显示状态徽标） */
  hdrNits: number;
  /** 面板原生色域分类（如 DCI-P3；空 = 未识别不显示） */
  hdrGamut: string;
  /** 面板对 P3 的覆盖率（0-1；色域非 P3 时显示） */
  hdrP3Coverage: number;
  /** 当前图为 JXL 动图（显示「播放动图」按钮） */
  isAnimation: boolean;
  /** 系统存在已开启 HDR 的显示器（SDR→HDR AI 反转按钮显示条件，与当前图无关） */
  hdrAny: boolean;
  /** AI 反转进行中（HDRTVNet CPU 推理数秒；按钮转圈禁用） */
  itmBusy: boolean;
  /** 当前嵌入画布为 ITM 合成源（显示 AI 强度滑杆） */
  itmEmbedded: boolean;
  /** AI 反转强度（0-1，滑杆显示用） */
  itmStrength: number;
}>();

const emit = defineEmits<{
  (e: "open"): void;
  (e: "open-folder"): void;
  (e: "prev"): void;
  (e: "next"): void;
  (e: "zoom-in"): void;
  (e: "zoom-out"): void;
  (e: "fit"): void;
  (e: "actual-size"): void;
  (e: "rotate-left"): void;
  (e: "rotate-right"): void;
  (e: "edit"): void;
  (e: "hdr-view"): void;
  (e: "play-animation"): void;
  (e: "fullscreen"): void;
  (e: "toggle-exif"): void;
  (e: "save"): void;
  (e: "copy"): void;
  (e: "print"): void;
  (e: "wallpaper"): void;
  (e: "slideshow"): void;
  (e: "recycle"): void;
  (e: "tone"): void;
  (e: "upscale"): void;
  (e: "hdr-itm"): void;
  (e: "itm-strength", v: number): void;
}>();
</script>

<template>
  <div class="viewer-toolbar">
    <button class="tb-btn" title="打开图片" @click="emit('open')">
      <n-icon :component="Folder" size="16" />
    </button>
    <button class="tb-btn" title="打开相册（选择文件夹浏览）" @click="emit('open-folder')">
      <n-icon :component="LayoutGrid" size="16" />
    </button>
    <span class="tb-sep" />
    <button
      class="tb-btn"
      title="上一张（←）"
      :disabled="!canPrev"
      @click="emit('prev')"
    >
      <n-icon :component="ChevronLeft" size="16" />
    </button>
    <button
      class="tb-btn"
      title="下一张（→）"
      :disabled="!canNext"
      @click="emit('next')"
    >
      <n-icon :component="ChevronRight" size="16" />
    </button>
    <span v-if="!hdrEmbedded" class="tb-sep" />
    <template v-if="!hdrEmbedded">
      <!-- 缩放/旋转/编辑/亮度调节/EXIF 作用于 SDR <img> 预览；嵌入模式下隐藏
           （真 HDR 画布由原生窗口自绘：滚轮缩放 · 拖动平移 · 双击 1:1 · Esc 退出） -->
      <button class="tb-btn" title="放大" @click="emit('zoom-in')">
        <n-icon :component="ZoomIn" size="16" />
      </button>
      <span class="tb-scale">{{ Math.round(scale * 100) }}%</span>
      <button class="tb-btn" title="缩小" @click="emit('zoom-out')">
        <n-icon :component="ZoomOut" size="16" />
      </button>
      <button class="tb-btn" title="自适应窗口（容器内最大化）" @click="emit('fit')">
        <n-icon :component="ArrowsMaximize" size="16" />
      </button>
      <button class="tb-btn tb-1-1" title="1:1 实际像素" @click="emit('actual-size')">
        1:1
      </button>
      <span class="tb-sep" />
      <button class="tb-btn" title="向左旋转 90°" @click="emit('rotate-left')">
        <n-icon :component="Rotate2" size="16" />
      </button>
      <button class="tb-btn" title="向右旋转 90°" @click="emit('rotate-right')">
        <n-icon :component="RotateClockwise" size="16" />
      </button>
      <button class="tb-btn" title="滤镜编辑（亮度/对比度/裁剪）" @click="emit('edit')">
        <n-icon :component="Edit" size="16" />
      </button>
      <button
        class="tb-btn"
        :class="{ active: upscaleActive }"
        title="AI 放大（waifu2x 神经网络超分：2x/4x + 降噪，GPU 加速）"
        @click="emit('upscale')"
      >
        <n-icon :component="Wand" size="16" />
      </button>
      <button
        v-if="isHdr"
        class="tb-btn"
        :class="{ active: toneActive }"
        title="HDR 亮度调节（预设 + 曝光/白落点，即时重渲）"
        @click="emit('tone')"
      >
        <n-icon :component="Sun" size="16" />
      </button>
    </template>
    <!-- HDR 嵌入模式状态徽标（替代缩放百分比位；画布操作提示见 title） -->
    <span
      v-else
      class="tb-hdr-badge"
      title="原生 scRGB 画布：滚轮缩放 · 拖动平移 · 双击 1:1 · Esc 退出"
    >
      {{
        hdrNits > 0
          ? `HDR${hdrGamut ? " · " + hdrGamut : ""} · 峰值 ${hdrNits} nits`
          : "HDR 已点亮"
      }}
    </span>
    <!-- ITM 合成源的 AI 反转强度滑杆（工具栏留在 WebView2 可见层，画布盖不到） -->
    <template v-if="hdrEmbedded && itmEmbedded">
      <span class="tb-itm-label">AI 强度</span>
      <input
        class="tb-itm-slider"
        type="range"
        min="0"
        max="100"
        step="5"
        :value="Math.round(itmStrength * 100)"
        :disabled="itmBusy"
        title="AI 反转强度：0% = 原 SDR 观感，100% = 纯模型输出（拖动后自动重跑）"
        @input="
          emit('itm-strength', Number(($event.target as HTMLInputElement).value) / 100)
        "
      />
      <span class="tb-itm-val">{{ Math.round(itmStrength * 100) }}%</span>
    </template>
    <button
      v-if="isAnimation"
      class="tb-btn"
      title="播放动图（JXL 动画 · 真 HDR 流式回放）"
      @click="emit('play-animation')"
    >
      <n-icon :component="PlayerPlay" size="16" />
    </button>
    <button
      v-if="hdrAvailable"
      class="tb-btn hdr-btn"
      :class="{ active: hdrEmbedded }"
      :title="hdrEmbedded
        ? '退出真 HDR 查看（返回 SDR 预览）'
        : '真 HDR 查看（原生 scRGB 画布嵌入本窗口，高光按真实 nits 点亮）'"
      @click="emit('hdr-view')"
    >
      <n-icon :component="Bolt" size="16" />
    </button>
    <!-- SDR→HDR（AI 反转）：HDRTVNet 逆色调映射；SDR 静态图 + HDR 显示器开启时显示 -->
    <button
      v-if="hdrAny && !isHdr && !isAnimation"
      class="tb-btn hdr-btn"
      :class="{ active: itmEmbedded, busy: itmBusy }"
      :disabled="itmBusy"
      :title="itmEmbedded
        ? '退出 AI 反转 HDR（返回 SDR 预览）'
        : 'SDR→HDR（AI 反转：HDRTVNet 逆色调映射重建高光，需 HDR 显示器）'"
      @click="emit('hdr-itm')"
    >
      <n-icon :component="Stars" size="16" />
    </button>
    <span class="tb-sep" />
    <button
      class="tb-btn"
      :title="fullscreen ? '退出全屏' : '全屏'"
      @click="emit('fullscreen')"
    >
      <n-icon :component="fullscreen ? Minimize : Maximize" size="16" />
    </button>
    <button
      v-if="!hdrEmbedded"
      class="tb-btn"
      :class="{ active: exifActive }"
      title="EXIF 信息"
      @click="emit('toggle-exif')"
    >
      <n-icon :component="InfoCircle" size="16" />
    </button>
    <span class="tb-sep" />
    <button class="tb-btn" title="另存为" @click="emit('save')">
      <n-icon :component="Download" size="16" />
    </button>
    <button class="tb-btn" title="复制到剪贴板" @click="emit('copy')">
      <n-icon :component="Copy" size="16" />
    </button>
    <button class="tb-btn" title="打印（图片 / PDF，支持批量队列）" @click="emit('print')">
      <n-icon :component="Printer" size="16" />
    </button>
    <span class="tb-sep" />
    <button class="tb-btn" title="设为桌面壁纸" @click="emit('wallpaper')">
      <n-icon :component="Wallpaper" size="16" />
    </button>
    <button
      class="tb-btn"
      :class="{ active: slideshow }"
      :title="slideshow ? '停止放映（F5）' : '幻灯片放映（F5）'"
      @click="emit('slideshow')"
    >
      <n-icon :component="slideshow ? PlayerStop : PlayerPlay" size="16" />
    </button>
    <button class="tb-btn" title="回收站（7 天内可恢复）" @click="emit('recycle')">
      <n-icon :component="Trash" size="16" />
    </button>
  </div>
</template>

<style scoped>
/* Acrylic 浮动条：毛玻璃 + 半透卡片底 + 柔和投影 */
.viewer-toolbar {
  height: 44px;
  display: flex;
  align-items: center;
  gap: 2px;
  padding: 0 10px;
  background: color-mix(in srgb, var(--jb-bg-card) 88%, transparent);
  backdrop-filter: blur(16px) saturate(1.3);
  border: 1px solid var(--jb-border);
  border-radius: 14px;
  box-shadow: var(--jb-shadow);
  user-select: none;
}
.tb-btn {
  width: 30px;
  height: 30px;
  border: none;
  border-radius: 6px;
  background: transparent;
  color: var(--jb-text);
  cursor: pointer;
  display: flex;
  align-items: center;
  justify-content: center;
  transition: background-color 0.15s;
}
.tb-btn:hover:not(:disabled) {
  background: var(--jb-titlebar-btn-hover);
}
.tb-btn:disabled {
  opacity: 0.32;
  cursor: default;
}
/* EXIF 激活态：主题色高亮 */
.tb-btn.active,
.tb-btn.active .n-icon {
  color: var(--jb-primary);
}
/* 真 HDR 按钮：主题色描边强调（区别于普通 SDR 预览工具） */
.tb-btn.hdr-btn {
  color: var(--jb-primary);
}
.tb-btn.hdr-btn:hover {
  background: color-mix(in srgb, var(--jb-primary) 16%, transparent);
}
/* AI 反转推理进行中：图标转圈（disabled 基础上保留可视反馈） */
.tb-btn.busy .n-icon {
  animation: tb-spin 1s linear infinite;
}
.tb-btn.busy {
  opacity: 0.7;
  cursor: progress;
}
@keyframes tb-spin {
  to {
    transform: rotate(360deg);
  }
}
/* ITM AI 强度滑杆（工具栏内联，主题色轨道） */
.tb-itm-label {
  font-size: 11px;
  color: var(--jb-text-soft);
  user-select: none;
  white-space: nowrap;
}
.tb-itm-slider {
  width: 96px;
  height: 3px;
  appearance: none;
  -webkit-appearance: none;
  border-radius: 999px;
  background: var(--jb-divider);
  outline: none;
  cursor: pointer;
}
.tb-itm-slider:disabled {
  cursor: progress;
  opacity: 0.6;
}
.tb-itm-slider::-webkit-slider-thumb {
  appearance: none;
  -webkit-appearance: none;
  width: 11px;
  height: 11px;
  border-radius: 50%;
  background: var(--jb-primary);
  border: none;
  box-shadow: 0 1px 3px rgba(0, 0, 0, 0.3);
}
.tb-itm-val {
  min-width: 32px;
  font-size: 11px;
  font-weight: 600;
  font-variant-numeric: tabular-nums;
  color: var(--jb-primary);
  user-select: none;
}
.tb-sep {
  width: 1px;
  height: 18px;
  background: var(--jb-divider);
  margin: 0 6px;
  flex-shrink: 0;
}
.tb-scale {
  min-width: 44px;
  text-align: center;
  font-size: 11px;
  color: var(--jb-text-soft);
  font-variant-numeric: tabular-nums;
  user-select: none;
}
/* 1:1 实际像素按钮（文字按钮，与百分比显示同风格） */
.tb-btn.tb-1-1 {
  font-size: 11px;
  font-weight: 600;
  font-variant-numeric: tabular-nums;
}
/* HDR 嵌入状态徽标：主题色小胶囊（峰值亮度 + 操作提示 title） */
.tb-hdr-badge {
  padding: 2px 10px;
  border-radius: 999px;
  font-size: 11px;
  font-weight: 600;
  letter-spacing: 0.02em;
  color: var(--jb-primary);
  background: color-mix(in srgb, var(--jb-primary) 14%, transparent);
  border: 1px solid color-mix(in srgb, var(--jb-primary) 30%, transparent);
  font-variant-numeric: tabular-nums;
  user-select: none;
  white-space: nowrap;
}
</style>
