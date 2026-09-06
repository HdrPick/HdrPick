<script setup lang="ts">
// AI 放大面板（waifu2x 神经网络超分）：模式/模型/尺寸/降噪/TTA/格式 + 执行
// 右侧浮动卡片（与 TonePanel 同布局风格）；执行经 upscale_p1_test 命令
// 功能对齐原版：方式 scale/noise_scale/noise_only/auto（JPEG 才降噪）、
// 任意倍率 1-16 或目标宽高（CalcScaleRatio）、降噪 0-3、输出 PNG8/PNG16/JXL(HDR)
import { computed, onMounted, ref } from "vue";
import { invoke } from "@tauri-apps/api/core";
import {
  NButton,
  NIcon,
  NInputNumber,
  NRadio,
  NRadioGroup,
  NSelect,
  NSwitch,
  useMessage,
} from "naive-ui";
import { X, Wand } from "@vicons/tabler";

const props = defineProps<{
  /** 当前图片绝对路径 */
  path: string;
  /** 当前图宽高（预估输出尺寸展示） */
  width: number;
  height: number;
}>();

const emit = defineEmits<{
  (e: "close"): void;
}>();

const message = useMessage();

// === 选项状态 ===
type ConvMode = "scale" | "noise_scale" | "noise_only" | "auto";
const convMode = ref<ConvMode>("scale");

// 模型列表（后端动态枚举，按实际模型文件可用性过滤）
interface ModelInfo {
  id: string;
  label: string;
  dir: string;
  has_scale: boolean;
  has_noise_scale: boolean;
  has_noise_only: boolean;
  size_mb: number;
}
const modelOptions = ref<{ label: string; value: string }[]>([
  { label: "照片 · Real-ESRGAN（推荐）", value: "realesrgan" },
  { label: "动漫 · Real-ESRGAN", value: "realesrgan_anime" },
  { label: "动漫图 · Upconv7", value: "upconv7_anime" },
  { label: "照片 · Upconv7", value: "upconv7_photo" },
  { label: "通用 · UpResNet10", value: "upresnet10" },
  { label: "动漫图 · CUnet（最高质量，较慢）", value: "cunet" },
]);
const modelId = ref("upconv7_anime");

// 尺寸模式：ratio（倍率）/ size（目标宽高）
type SizeMode = "ratio" | "size";
const sizeMode = ref<SizeMode>("ratio");
const scale = ref(2.0);
const targetW = ref(1920);
const targetH = ref(1080);

const noiseLevel = ref(1);
const noiseOptions = [
  { label: "最轻（等级 0）", value: 0 },
  { label: "轻度（等级 1）", value: 1 },
  { label: "中度（等级 2）", value: 2 },
  { label: "重度（等级 3）", value: 3 },
];

const formatId = ref("png");
const formatOptions = [
  { label: "PNG 8bit", value: "png" },
  { label: "PNG 16bit", value: "png16" },
  { label: "HDR PNG（16bit PQ）", value: "png_hdr" },
  { label: "OpenEXR（32bit 浮点）", value: "exr" },
  { label: "JXL HDR 16bit", value: "jxl" },
  { label: "JXL HDR 12bit（体积优先）", value: "jxl12" },
];

const useTta = ref(false);
const running = ref(false);

const isNoiseOnly = computed(() => convMode.value === "noise_only");
const noiseEnabled = computed(() => convMode.value === "noise_scale" || convMode.value === "auto");
// 预估输出尺寸（宽高模式换算 CalcScaleRatio：双指定取 max 覆盖）
const outSize = computed(() => {
  if (isNoiseOnly.value) return `${props.width}×${props.height}`;
  if (sizeMode.value === "ratio") {
    const f = scale.value;
    return `${Math.round(props.width * f)}×${Math.round(props.height * f)}`;
  }
  const sw = targetW.value / Math.max(1, props.width);
  const sh = targetH.value / Math.max(1, props.height);
  const f = Math.max(sw, sh);
  return `${Math.round(props.width * f)}×${Math.round(props.height * f)}`;
});

onMounted(async () => {
  // 宽高模式默认值 = 当前图 2x
  if (props.width > 0 && props.height > 0) {
    targetW.value = props.width * 2;
    targetH.value = props.height * 2;
  }
  // 动态模型列表（模型目录缺失时过滤）
  try {
    const models = await invoke<ModelInfo[]>("upscale_models");
    const opts = models
      .filter((m) => m.has_scale || m.has_noise_only)
      .map((m) => ({ label: m.label, value: m.id }));
    if (opts.length > 0) modelOptions.value = opts;
  } catch {
    // 回落默认列表
  }
  // 上次参数回填（sidecar 历史）
  if (props.path) {
    try {
      const hist = await invoke<{ entries?: unknown[] }>("upscale_history", { path: props.path });
      const entries = (hist.entries ?? []) as unknown[];
      const last = (entries.length > 0 ? entries[entries.length - 1] : undefined) as
        | { model?: string; scale?: number | null; noise?: number | null; tta?: boolean; format?: string }
        | undefined;
      if (last) {
        if (last.model) modelId.value = last.model;
        if (typeof last.scale === "number" && last.scale > 1) {
          sizeMode.value = "ratio";
          scale.value = Math.min(16, last.scale);
        }
        if (typeof last.noise === "number") {
          noiseLevel.value = last.noise;
          if (last.scale === null || last.scale === undefined) convMode.value = "noise_only";
          else convMode.value = "noise_scale";
        }
        if (typeof last.tta === "boolean") useTta.value = last.tta;
        if (last.format) {
          const f = last.format.toLowerCase();
          if (f.startsWith("png16")) formatId.value = "png16";
          else if (f.startsWith("pnghdr")) formatId.value = "png_hdr";
          else if (f.startsWith("exr")) formatId.value = "exr";
          else if (f.startsWith("jxlhdr12") || f === "jxl12") formatId.value = "jxl12";
          else if (f.startsWith("jxl")) formatId.value = "jxl";
          else formatId.value = "png";
        }
      }
    } catch {
      // 无历史（正常）
    }
  }
});

async function run() {
  if (!props.path || running.value) return;
  running.value = true;
  try {
    const msg = await invoke<string>("upscale_p1_test", {
      path: props.path,
      model: modelId.value,
      mode: convMode.value,
      // 纯降噪：倍率/宽高都传 null
      scale: isNoiseOnly.value ? null : sizeMode.value === "ratio" ? scale.value : null,
      targetW: isNoiseOnly.value || sizeMode.value === "ratio" ? null : targetW.value,
      targetH: isNoiseOnly.value || sizeMode.value === "ratio" ? null : targetH.value,
      noiseLevel: noiseEnabled.value || isNoiseOnly.value ? noiseLevel.value : null,
      useTta: useTta.value,
      format: formatId.value,
    });
    message.success(msg, { duration: 5000 });
    // 输出在原图同目录：看图窗口 2s 目录轮询自动把新图收进缩略图条
  } catch (e) {
    message.error(`AI 放大失败: ${e}`, { duration: 4000 });
  } finally {
    running.value = false;
  }
}
</script>

<template>
  <div class="upscale-panel">
    <div class="up-head">
      <span class="up-title">
        <n-icon :component="Wand" size="15" style="vertical-align: -2px" />
        AI 放大
      </span>
      <button class="up-close" title="关闭" @click="emit('close')">
        <n-icon :component="X" size="15" />
      </button>
    </div>

    <div class="up-body">
      <div class="up-row">
        <span class="up-label">模型</span>
        <n-select v-model:value="modelId" :options="modelOptions" size="small" />
      </div>
      <div class="up-row">
        <span class="up-label">方式</span>
        <n-radio-group v-model:value="convMode" size="small">
          <n-radio value="scale">放大</n-radio>
          <n-radio value="noise_scale">降噪+放大</n-radio>
          <n-radio value="noise_only">降噪</n-radio>
          <n-radio value="auto">自动</n-radio>
        </n-radio-group>
      </div>
      <div class="up-row" :title="convMode === 'auto' ? 'JPEG 输入自动降噪，其余纯放大' : ''">
        <span class="up-label"></span>
        <span v-if="convMode === 'auto'" class="up-hint">JPEG 输入自动降噪，其余纯放大</span>
      </div>
      <div class="up-row">
        <span class="up-label">尺寸</span>
        <n-radio-group v-model:value="sizeMode" size="small" :disabled="isNoiseOnly">
          <n-radio value="ratio">倍率</n-radio>
          <n-radio value="size">宽×高</n-radio>
        </n-radio-group>
      </div>
      <div v-if="sizeMode === 'ratio'" class="up-row">
        <span class="up-label">倍率</span>
        <div class="up-inline">
          <n-input-number
            v-model:value="scale"
            size="small"
            :min="1"
            :max="16"
            :step="0.1"
            :precision="2"
            :disabled="isNoiseOnly"
            style="width: 110px"
          />
          <span v-if="!isNoiseOnly" class="up-hint">1.0 – 16.0</span>
        </div>
      </div>
      <div v-else class="up-row">
        <span class="up-label">宽高</span>
        <div class="up-inline">
          <n-input-number
            v-model:value="targetW"
            size="small"
            :min="1"
            :max="32768"
            :disabled="isNoiseOnly"
            style="width: 100px"
            placeholder="宽"
          />
          <span class="up-hint">×</span>
          <n-input-number
            v-model:value="targetH"
            size="small"
            :min="1"
            :max="32768"
            :disabled="isNoiseOnly"
            style="width: 100px"
            placeholder="高"
          />
        </div>
      </div>
      <div class="up-row">
        <span class="up-label">降噪</span>
        <div class="up-inline">
          <n-select
            v-model:value="noiseLevel"
            :options="noiseOptions"
            size="small"
            style="width: 132px"
            :disabled="convMode === 'scale'"
          />
        </div>
      </div>
      <div class="up-row">
        <span class="up-label">格式</span>
        <div class="up-inline">
          <n-select
            v-model:value="formatId"
            :options="formatOptions"
            size="small"
            style="width: 168px"
          />
        </div>
      </div>
      <div class="up-row">
        <span class="up-label" title="8 面体翻转增广平均：耗时 ×8，边缘质量略提升">TTA</span>
        <div class="up-inline">
          <n-switch v-model:value="useTta" size="small" />
          <span v-if="useTta" class="up-hint">耗时 ×8</span>
        </div>
      </div>

      <div class="up-preview">
        输出 <b>{{ isNoiseOnly ? "同尺寸" : "约" }} {{ outSize }}</b> · GPU 加速 · 保存到原图同目录
      </div>

      <n-button
        type="primary"
        size="small"
        block
        :loading="running"
        :disabled="!path"
        @click="run"
      >
        {{ running ? "处理中…" : isNoiseOnly ? "开始降噪" : "开始放大" }}
      </n-button>
    </div>
  </div>
</template>

<style scoped>
.upscale-panel {
  width: 268px;
  background: color-mix(in srgb, var(--jb-bg-card) 94%, transparent);
  backdrop-filter: blur(16px) saturate(1.3);
  border: 1px solid var(--jb-border);
  border-radius: 14px;
  box-shadow: var(--jb-shadow);
  padding: 12px 14px 14px;
  user-select: none;
}
.up-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 10px;
}
.up-title {
  font-size: 13px;
  font-weight: 700;
  color: var(--jb-primary);
  display: inline-flex;
  align-items: center;
  gap: 5px;
}
.up-close {
  border: none;
  background: transparent;
  color: var(--jb-text-mute);
  cursor: pointer;
  padding: 3px;
  border-radius: 5px;
  display: flex;
}
.up-close:hover {
  background: var(--jb-titlebar-btn-hover);
  color: var(--jb-text);
}
.up-body {
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.up-row {
  display: flex;
  align-items: center;
  gap: 8px;
}
.up-label {
  width: 34px;
  flex-shrink: 0;
  font-size: 12px;
  color: var(--jb-text-soft);
}
.up-inline {
  display: flex;
  align-items: center;
  gap: 8px;
  flex: 1;
}
.up-hint {
  font-size: 11px;
  color: var(--jb-text-mute);
}
.up-preview {
  font-size: 11px;
  color: var(--jb-text-mute);
  padding: 7px 9px;
  background: color-mix(in srgb, var(--jb-primary) 7%, transparent);
  border-radius: 8px;
}
.up-preview b {
  color: var(--jb-primary);
  font-variant-numeric: tabular-nums;
}
</style>
