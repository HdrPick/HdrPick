<script setup lang="ts">
// HDR 亮度调节面板（看图工具栏「太阳」图标弹出）
//
// 数据流：预设卡片 / 滑杆 → 防抖 120ms → emit('rerender', presetId, overrides)
// → ViewerWindow 调 retonemap_image（后端缓存 HDR 源，仅重跑 tone map，毫秒级）
// → 替换显示 URL，即时预览。
// overrides 只含被调整过的字段（null = 跟随预设），与设置页语义一致。
import { computed, onUnmounted, ref, watch } from "vue";
import { NSlider, NButton, NIcon, NTag } from "naive-ui";
import { X, Refresh, History } from "@vicons/tabler";
import {
  TONEMAP_BUILTIN_PRESETS,
  type ToneMapOverrides,
} from "../configStore";

const emit = defineEmits<{
  (e: "rerender", preset: string, overrides: ToneMapOverrides): void;
  (e: "close"): void;
  (e: "history"): void;
}>();

const props = defineProps<{
  /** 初始激活预设 id */
  activePreset?: string;
}>();

const selected = ref(props.activePreset ?? "builtin:soft");

// 覆盖值（null = 跟随预设；本地 number | null，emit 时构造 overrides）
const exposureEv = ref<number | null>(null);
const diffuseWhite = ref<number | null>(null);
const peakNits = ref<number | null>(null);
const saturation = ref<number | null>(null);

// 防抖重渲（滑杆拖动密集触发）
let rerenderTimer: number | null = null;

function buildOverrides(): ToneMapOverrides {
  return {
    source_peak_nits: peakNits.value,
    output_diffuse_white: diffuseWhite.value,
    exposure_ev: exposureEv.value,
    saturation: saturation.value,
    contrast: null,
    gamut_strength: null,
    dither: null,
    adaptive_peak: null,
  };
}

function scheduleRerender() {
  if (rerenderTimer) window.clearTimeout(rerenderTimer);
  rerenderTimer = window.setTimeout(() => {
    emit("rerender", selected.value, buildOverrides());
  }, 120);
}

function selectPreset(id: string) {
  selected.value = id;
  // 切预设清空覆盖（回到预设基准观感）
  exposureEv.value = null;
  diffuseWhite.value = null;
  peakNits.value = null;
  saturation.value = null;
  scheduleRerender();
}

function resetOverrides() {
  exposureEv.value = null;
  diffuseWhite.value = null;
  peakNits.value = null;
  saturation.value = null;
  scheduleRerender();
}

// 滑杆 null 处理：显示预设值
const activePresetObj = computed(
  () => TONEMAP_BUILTIN_PRESETS.find((p) => p.id === selected.value),
);
const exposureDisplay = computed(() =>
  exposureEv.value ?? activePresetObj.value?.exposure_ev ?? 0,
);
const diffuseDisplay = computed(() =>
  diffuseWhite.value ?? activePresetObj.value?.output_diffuse_white ?? 0.75,
);
const peakDisplay = computed(() =>
  peakNits.value ?? activePresetObj.value?.source_peak_nits ?? 1000,
);
const satDisplay = computed(() =>
  saturation.value ?? activePresetObj.value?.saturation ?? 1,
);

// 任一覆盖被调整即显示"已自定义"角标
const hasOverride = computed(
  () =>
    exposureEv.value !== null ||
    diffuseWhite.value !== null ||
    peakNits.value !== null ||
    saturation.value !== null,
);

// props 变化（切换图片）时重置面板
watch(
  () => props.activePreset,
  (v) => {
    if (v) selected.value = v;
  },
);

onUnmounted(() => {
  if (rerenderTimer) window.clearTimeout(rerenderTimer);
});
</script>

<template>
  <div class="tone-panel">
    <div class="tp-head">
      <span class="tp-title">HDR 亮度调节</span>
      <n-tag v-if="hasOverride" size="tiny" type="warning" :bordered="false">已自定义</n-tag>
      <button class="tp-close" title="关闭" @click="emit('close')">
        <n-icon :component="X" size="14" />
      </button>
    </div>

    <!-- 预设卡片（紧凑横排两列） -->
    <div class="tp-presets">
      <button
        v-for="p in TONEMAP_BUILTIN_PRESETS"
        :key="p.id"
        class="tp-preset"
        :class="{ active: selected === p.id }"
        :title="p.desc"
        @click="selectPreset(p.id)"
      >
        {{ p.name }}
      </button>
    </div>

    <!-- 曝光 -->
    <div class="tp-row">
      <div class="tp-label">
        <span>曝光</span>
        <b>{{ exposureDisplay > 0 ? "+" : "" }}{{ exposureDisplay.toFixed(1) }} EV</b>
      </div>
      <n-slider
        :value="exposureDisplay"
        :min="-1"
        :max="1"
        :step="0.05"
        :format-tooltip="(v: number) => v.toFixed(2) + ' EV'"
        @update:value="(v: number) => { exposureEv = v; scheduleRerender(); }"
      />
    </div>

    <!-- SDR 白落点 -->
    <div class="tp-row">
      <div class="tp-label">
        <span>SDR 白落点</span>
        <b>{{ diffuseDisplay.toFixed(2) }}</b>
      </div>
      <n-slider
        :value="diffuseDisplay"
        :min="0.6"
        :max="1"
        :step="0.01"
        :format-tooltip="(v: number) => v.toFixed(2)"
        @update:value="(v: number) => { diffuseWhite = v; scheduleRerender(); }"
      />
    </div>

    <!-- 场景峰值 -->
    <div class="tp-row">
      <div class="tp-label">
        <span>场景峰值</span>
        <b>{{ peakDisplay }} nits</b>
      </div>
      <n-slider
        :value="peakDisplay"
        :min="300"
        :max="3000"
        :step="50"
        :format-tooltip="(v: number) => v + ' nits'"
        @update:value="(v: number) => { peakNits = v; scheduleRerender(); }"
      />
    </div>

    <!-- 饱和度 -->
    <div class="tp-row">
      <div class="tp-label">
        <span>饱和度</span>
        <b>{{ satDisplay.toFixed(2) }}</b>
      </div>
      <n-slider
        :value="satDisplay"
        :min="0.5"
        :max="1.5"
        :step="0.02"
        :format-tooltip="(v: number) => v.toFixed(2)"
        @update:value="(v: number) => { saturation = v; scheduleRerender(); }"
      />
    </div>

    <div class="tp-foot">
      <n-button size="tiny" quaternary @click="resetOverrides">
        <template #icon>
          <n-icon :component="Refresh" size="12" />
        </template>
        恢复预设值
      </n-button>
      <n-button size="tiny" quaternary title="查看调参历史（选任意历史条目重渲 / 四宫格对比）" @click="emit('history')">
        <template #icon>
          <n-icon :component="History" size="12" />
        </template>
        历史
      </n-button>
      <span class="tp-hint">调整即时生效（仅预览，不影响源文件）</span>
    </div>
  </div>
</template>

<style scoped>
.tone-panel {
  background: rgba(255, 255, 255, 0.88);
  backdrop-filter: blur(16px);
  border: 1px solid rgba(0, 0, 0, 0.08);
  border-radius: 10px;
  box-shadow: 0 8px 28px rgba(0, 0, 0, 0.16);
  padding: 12px 14px 10px;
}
:global(.dark) .tone-panel,
:global(html.dark) .tone-panel {
  background: rgba(30, 32, 38, 0.9);
  border-color: rgba(255, 255, 255, 0.08);
}

.tp-head {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 10px;
}
.tp-title {
  font-size: 13px;
  font-weight: 600;
}
.tp-close {
  margin-left: auto;
  border: none;
  background: none;
  cursor: pointer;
  padding: 4px;
  border-radius: 6px;
  color: inherit;
  opacity: 0.65;
}
.tp-close:hover {
  opacity: 1;
  background: rgba(0, 0, 0, 0.06);
}

.tp-presets {
  display: grid;
  grid-template-columns: 1fr 1fr 1fr;
  gap: 6px;
  margin-bottom: 12px;
}
.tp-preset {
  border: 1px solid rgba(0, 0, 0, 0.1);
  background: rgba(255, 255, 255, 0.55);
  border-radius: 7px;
  padding: 6px 2px;
  font-size: 12px;
  cursor: pointer;
  color: inherit;
  transition: all 120ms ease;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.tp-preset:hover {
  border-color: rgba(0, 0, 0, 0.25);
}
.tp-preset.active {
  background: rgba(64, 128, 255, 0.14);
  border-color: rgba(64, 128, 255, 0.55);
  font-weight: 600;
}
:global(html.dark) .tp-preset {
  background: rgba(255, 255, 255, 0.06);
  border-color: rgba(255, 255, 255, 0.12);
}

.tp-row {
  margin-bottom: 10px;
}
.tp-label {
  display: flex;
  justify-content: space-between;
  align-items: baseline;
  font-size: 12px;
  margin-bottom: 2px;
}
.tp-label b {
  font-variant-numeric: tabular-nums;
  font-weight: 600;
  font-size: 11.5px;
}

.tp-foot {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 2px;
}
.tp-hint {
  font-size: 11px;
  opacity: 0.55;
}
</style>
