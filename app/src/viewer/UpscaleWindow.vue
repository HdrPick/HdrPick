<script setup lang="ts">
// 独立 AI 放大窗口（#/upscale，复刻原版 waifu2x-caffe GUI 布局）
// 输入文件/文件夹 → 输出目录 → 模型/方式/降噪/尺寸/TTA/格式 → 批量执行 + 实时日志
// 功能对齐原版：scale/noise_scale/noise_only/auto（JPEG 才降噪）、任意倍率或目标宽高、
// 降噪 0-3、输出 PNG8/PNG16/JXL(HDR)、GPU/CPU 基准测试
import { computed, nextTick, onMounted, onUnmounted, ref } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import {
  NButton,
  NIcon,
  NInputNumber,
  NProgress,
  NRadio,
  NRadioGroup,
  NSelect,
  NSwitch,
  useMessage,
} from "naive-ui";
import { File, Folder, FolderPlus, X } from "@vicons/tabler";
import { useTheme } from "../useTheme";
import TitleBar from "../TitleBar.vue";

const { mode, isDark, setMode } = useTheme();
const message = useMessage();

const IMAGE_FILTERS = [
  { name: "图片", extensions: ["png", "jpg", "jpeg", "webp", "bmp"] },
];

// === 输入 / 输出 ===
const selFiles = ref<string[]>([]);
const selFolder = ref("");
const outDir = ref(""); // 空 = 原图目录

async function pickFiles() {
  try {
    const sel = await openDialog({ multiple: true, filters: IMAGE_FILTERS });
    if (!sel) return;
    const arr = Array.isArray(sel) ? sel : [sel];
    selFiles.value = [...selFiles.value, ...arr];
  } catch (e) {
    message.error("选择文件失败: " + e, { duration: 2200 });
  }
}

async function pickFolder() {
  try {
    const dir = await openDialog({ directory: true });
    if (typeof dir === "string" && dir) selFolder.value = dir;
  } catch (e) {
    message.error("选择文件夹失败: " + e, { duration: 2200 });
  }
}

async function pickOutDir() {
  try {
    const dir = await openDialog({ directory: true });
    if (typeof dir === "string" && dir) outDir.value = dir;
  } catch (e) {
    message.error("选择输出目录失败: " + e, { duration: 2200 });
  }
}

// === 选项 ===
// 模型列表（后端动态枚举；invoke 失败回落硬编码）
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

type ConvMode = "scale" | "noise_scale" | "noise_only" | "auto";
const convMode = ref<ConvMode>("scale");

const noiseLevel = ref(1);
const noiseOptions = [
  { label: "最轻（等级 0）", value: 0 },
  { label: "轻度（等级 1）", value: 1 },
  { label: "中度（等级 2）", value: 2 },
  { label: "重度（等级 3）", value: 3 },
];

// 尺寸模式：ratio（倍率 1-16）/ size（目标宽高）
type SizeMode = "ratio" | "size";
const sizeMode = ref<SizeMode>("ratio");
const scale = ref(2.0);
const targetW = ref(1920);
const targetH = ref(1080);

const formatId = ref("png");
const formatOptions = [
  { label: "PNG 8bit", value: "png" },
  { label: "PNG 16bit", value: "png16" },
  { label: "HDR PNG（16bit PQ）", value: "png_hdr" },
  { label: "OpenEXR（32bit 浮点）", value: "exr" },
  { label: "JXL HDR 16bit", value: "jxl" },
  { label: "JXL HDR 12bit（体积优先）", value: "jxl12" },
];

// SDR→HDR 画质提升档位（仅 HDR 容器格式生效）
const hdrEnhanceId = ref("natural");
const hdrEnhanceOptions = [
  { label: "关闭（纯 SDR 等效）", value: "off" },
  { label: "自然（高光 2×）", value: "natural" },
  { label: "鲜艳（高光 3× + 色域外推）", value: "vivid" },
  { label: "AI 智能（HDRTVNet，需 models\\itm\\Ensemble_AGCM_LE.pth）", value: "ai" },
];
const isHdrFormat = computed(() =>
  ["png_hdr", "exr", "jxl", "jxl12"].includes(formatId.value),
);

const useTta = ref(false);

// === 执行状态 ===
const running = ref(false);
const cancelRequested = ref(false);
const doneCount = ref(0);
const totalCount = ref(0);
const currentName = ref("");
const benching = ref(false);

interface LogLine {
  time: string;
  kind: "info" | "ok" | "err";
  text: string;
}
const logs = ref<LogLine[]>([]);
const logBoxRef = ref<HTMLElement | null>(null);

function addLog(kind: LogLine["kind"], text: string) {
  const t = new Date();
  const hh = String(t.getHours()).padStart(2, "0");
  const mm = String(t.getMinutes()).padStart(2, "0");
  const ss = String(t.getSeconds()).padStart(2, "0");
  logs.value.push({ time: `${hh}:${mm}:${ss}`, kind, text });
  nextTick(() => {
    logBoxRef.value?.scrollTo({ top: logBoxRef.value.scrollHeight });
  });
}

const hasInput = computed(() => selFiles.value.length > 0 || !!selFolder.value);
const isNoiseOnly = computed(() => convMode.value === "noise_only");
const canRun = computed(
  () =>
    hasInput.value &&
    !running.value &&
    (isNoiseOnly.value ||
      (sizeMode.value === "ratio" && scale.value > 1.0 && scale.value <= 16.0) ||
      sizeMode.value === "size"),
);
const progressPct = computed(() =>
  totalCount.value > 0 ? Math.round((doneCount.value / totalCount.value) * 100) : 0,
);
const inputSummary = computed(() => {
  const parts: string[] = [];
  if (selFiles.value.length > 0) parts.push(`文件 ×${selFiles.value.length}`);
  if (selFolder.value) parts.push(`文件夹 ${selFolder.value}`);
  return parts.join(" + ");
});

async function run() {
  if (!canRun.value) return;
  running.value = true;
  cancelRequested.value = false;
  doneCount.value = 0;
  totalCount.value = 0;
  currentName.value = "";
  logs.value = [];
  addLog(
    "info",
    `开始：${inputSummary.value} · ${isNoiseOnly.value ? "纯降噪" : sizeMode.value === "ratio" ? `倍率 ${scale.value}×` : `目标 ${targetW.value}×${targetH.value}`}${useTta.value ? " · TTA" : ""}`,
  );

  try {
    const summary = await invoke<string>("upscale_window_run", {
      files: selFiles.value.length > 0 ? selFiles.value : null,
      folder: selFolder.value || null,
      outputDir: outDir.value || null,
      model: modelId.value,
      mode: convMode.value,
      noiseLevel: convMode.value === "scale" ? null : noiseLevel.value,
      // 纯降噪：倍率/宽高都传 null
      scale: isNoiseOnly.value ? null : sizeMode.value === "ratio" ? scale.value : null,
      targetW: isNoiseOnly.value || sizeMode.value === "ratio" ? null : targetW.value,
      targetH: isNoiseOnly.value || sizeMode.value === "ratio" ? null : targetH.value,
      useTta: useTta.value,
      format: formatId.value,
      hdrEnhance: isHdrFormat.value ? hdrEnhanceId.value : "off",
    });
    addLog("info", summary);
    message.success(summary, { duration: 4000 });
  } catch (e) {
    addLog("err", String(e));
    message.error(`AI 放大失败: ${e}`, { duration: 4000 });
  } finally {
    running.value = false;
    cancelRequested.value = false;
    currentName.value = "";
  }
}

async function cancel() {
  if (!running.value) return;
  cancelRequested.value = true;
  addLog("info", "正在取消（当前张完成后停止）…");
  try {
    await invoke("upscale_window_cancel");
  } catch {
    // 忽略
  }
}

/// GPU/CPU 基准（256×256 upconv7 2x）
async function benchmark() {
  if (benching.value || running.value) return;
  benching.value = true;
  addLog("info", "基准测试：256×256 → 2x（upconv7，GPU/CPU 各一次）…");
  try {
    const r = await invoke<{ gpu_ms: number | null; gpu_wall_ms: number; cpu_ms: number }>(
      "upscale_benchmark",
    );
    if (r.gpu_ms !== null) {
      addLog("ok", `基准：GPU ${r.gpu_ms}ms（含会话建立 ${r.gpu_wall_ms}ms）vs CPU ${r.cpu_ms}ms → 加速 ${(r.cpu_ms / Math.max(1, r.gpu_ms)).toFixed(1)}×`);
    } else {
      addLog("err", `基准：GPU 不可用（CPU ${r.cpu_ms}ms）`);
    }
  } catch (e) {
    addLog("err", `基准失败: ${e}`);
  } finally {
    benching.value = false;
  }
}

// === 进度事件 ===
interface ProgressPayload {
  index: number;
  total: number;
  name: string;
  state: string;
  message: string;
}

let unlisten: (() => void) | null = null;
onMounted(async () => {
  // 动态模型列表
  try {
    const models = await invoke<ModelInfo[]>("upscale_models");
    const opts = models
      .filter((m) => m.has_scale || m.has_noise_only)
      .map((m) => ({ label: m.label, value: m.id }));
    if (opts.length > 0) modelOptions.value = opts;
  } catch {
    // 回落默认
  }
  unlisten = await listen<ProgressPayload>("upscale://progress", (ev) => {
    const p = ev.payload;
    totalCount.value = p.total;
    if (p.state === "start") {
      currentName.value = p.name;
    } else if (p.state === "done") {
      doneCount.value = p.index;
      addLog("ok", `${p.name}：${p.message}`);
    } else if (p.state === "error") {
      doneCount.value = p.index;
      addLog("err", `${p.name}：${p.message}`);
    } else if (p.state === "cancelled") {
      addLog("info", p.message);
    } else if (p.state === "finish") {
      addLog("info", p.message);
    }
  });
});
onUnmounted(() => unlisten?.());
</script>

<template>
  <div class="up-window">
    <TitleBar :mode="mode" :is-dark="isDark" :set-mode="setMode" title="AI 放大" />

    <div class="up-body">
      <!-- 输入 / 输出 -->
      <div class="up-card">
        <div class="up-row">
          <span class="up-label">输入</span>
          <div class="up-inline">
            <n-button size="tiny" secondary @click="pickFiles">
              <template #icon><n-icon :component="File" size="14" /></template>
              选择文件
            </n-button>
            <n-button size="tiny" secondary @click="pickFolder">
              <template #icon><n-icon :component="Folder" size="14" /></template>
              选择文件夹
            </n-button>
            <button
              v-if="hasInput"
              class="up-clear"
              title="清空输入"
              @click="selFiles = []; selFolder = ''"
            >
              <n-icon :component="X" size="13" />
            </button>
          </div>
        </div>
        <div v-if="hasInput" class="up-input-summary" :title="inputSummary">
          {{ inputSummary }}
        </div>

        <div class="up-row">
          <span class="up-label">输出</span>
          <div class="up-inline">
            <div class="up-outdir" :title="outDir || '默认：原图所在目录'">
              {{ outDir || "默认：原图所在目录" }}
            </div>
            <n-button size="tiny" secondary @click="pickOutDir">
              <template #icon><n-icon :component="FolderPlus" size="14" /></template>
              浏览
            </n-button>
          </div>
        </div>
      </div>

      <!-- 选项 -->
      <div class="up-card">
        <div class="up-row">
          <span class="up-label">模型</span>
          <n-select v-model:value="modelId" :options="modelOptions" size="small" />
        </div>
        <div class="up-row">
          <span class="up-label">方式</span>
          <n-radio-group v-model:value="convMode" size="small">
            <n-radio value="scale">放大</n-radio>
            <n-radio value="noise_scale">降噪 + 放大</n-radio>
            <n-radio value="noise_only">纯降噪</n-radio>
            <n-radio value="auto">自动</n-radio>
          </n-radio-group>
        </div>
        <div v-if="convMode === 'auto'" class="up-row">
          <span class="up-label"></span>
          <span class="up-hint">JPEG 输入自动降噪（等级取下方选择），其余格式纯放大</span>
        </div>
        <div class="up-row">
          <span class="up-label">降噪</span>
          <div class="up-inline">
            <n-select
              v-model:value="noiseLevel"
              :options="noiseOptions"
              size="small"
              style="width: 140px"
              :disabled="convMode === 'scale'"
            />
          </div>
        </div>
        <div class="up-row">
          <span class="up-label">尺寸</span>
          <n-radio-group v-model:value="sizeMode" size="small" :disabled="isNoiseOnly">
            <n-radio value="ratio">倍率</n-radio>
            <n-radio value="size">目标宽×高</n-radio>
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
              style="width: 140px"
              :disabled="isNoiseOnly"
            />
            <span class="up-hint">1.0 – 16.0 任意倍率（非整数倍自动精确回缩）</span>
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
              style="width: 120px"
              placeholder="宽"
              :disabled="isNoiseOnly"
            />
            <span class="up-hint">×</span>
            <n-input-number
              v-model:value="targetH"
              size="small"
              :min="1"
              :max="32768"
              style="width: 120px"
              placeholder="高"
              :disabled="isNoiseOnly"
            />
            <span class="up-hint">双指定取较大比例（覆盖式）</span>
          </div>
        </div>
        <div class="up-row">
          <span class="up-label">格式</span>
          <div class="up-inline">
            <n-select
              v-model:value="formatId"
              :options="formatOptions"
              size="small"
              style="width: 200px"
            />
            <span class="up-hint">JXL HDR = SDR 内容装 HDR 容器（可调亮）</span>
          </div>
        </div>
        <div v-if="isHdrFormat" class="up-row">
          <span class="up-label" title="中间调保持 SDR 等效亮度，高光扩展到超参考白形成真 HDR 高光">HDR 增强</span>
          <div class="up-inline">
            <n-select
              v-model:value="hdrEnhanceId"
              :options="hdrEnhanceOptions"
              size="small"
              style="width: 200px"
            />
            <span class="up-hint">SDR 白映射到系统参考白，高光向上扩展</span>
          </div>
        </div>
        <div class="up-row">
          <span class="up-label" title="8 面体翻转增广平均：耗时 ×8，边缘质量略提升">TTA</span>
          <div class="up-inline">
            <n-switch v-model:value="useTta" size="small" />
            <span v-if="useTta" class="up-hint">耗时 ×8</span>
          </div>
        </div>
      </div>

      <!-- 执行 -->
      <div class="up-card">
        <div class="up-actions">
          <n-button
            type="primary"
            size="small"
            :loading="running"
            :disabled="!canRun && !running"
            @click="run"
          >
            {{ running ? "处理中…" : "开始" }}
          </n-button>
          <n-button
            v-if="running"
            size="small"
            :disabled="cancelRequested"
            @click="cancel"
          >
            {{ cancelRequested ? "取消中…" : "取消" }}
          </n-button>
          <n-button size="small" secondary :loading="benching" :disabled="running" @click="benchmark">
            基准测试
          </n-button>
          <span v-if="running && currentName" class="up-current" :title="currentName">
            [{{ doneCount }}/{{ totalCount }}] {{ currentName }}
          </span>
          <span v-else-if="totalCount > 0 && !running" class="up-current">
            {{ doneCount }}/{{ totalCount }}
          </span>
        </div>
        <n-progress
          v-if="totalCount > 0"
          type="line"
          :percentage="progressPct"
          :height="6"
          :show-indicator="false"
        />
      </div>

      <!-- 日志 -->
      <div class="up-card up-log-card">
        <div ref="logBoxRef" class="up-log">
          <div v-if="logs.length === 0" class="up-log-empty">日志（处理时逐张显示结果）</div>
          <div v-for="(l, i) in logs" :key="i" class="up-log-line" :class="l.kind">
            <span class="up-log-time">{{ l.time }}</span>
            <span class="up-log-text">{{ l.text }}</span>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.up-window {
  height: 100vh;
  display: flex;
  flex-direction: column;
  background: var(--jb-bg);
  border-radius: var(--jb-radius);
  overflow: hidden;
}
.up-body {
  flex: 1;
  overflow-y: auto;
  padding: 12px 14px 14px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.up-card {
  background: var(--jb-bg-card);
  border: 1px solid var(--jb-border);
  border-radius: var(--jb-radius-card);
  padding: 10px 12px;
  display: flex;
  flex-direction: column;
  gap: 9px;
}
.up-row {
  display: flex;
  align-items: center;
  gap: 10px;
}
.up-label {
  width: 36px;
  flex-shrink: 0;
  font-size: 12px;
  color: var(--jb-text-soft);
}
.up-inline {
  display: flex;
  align-items: center;
  gap: 8px;
  flex: 1;
  min-width: 0;
}
.up-input-summary {
  font-size: 11px;
  color: var(--jb-text-mute);
  padding: 5px 8px;
  background: color-mix(in srgb, var(--jb-primary) 7%, transparent);
  border-radius: 6px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.up-outdir {
  flex: 1;
  min-width: 0;
  font-size: 12px;
  color: var(--jb-text-soft);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  padding: 4px 8px;
  background: color-mix(in srgb, var(--jb-text-mute) 8%, transparent);
  border-radius: 6px;
}
.up-clear {
  border: none;
  background: transparent;
  color: var(--jb-text-mute);
  cursor: pointer;
  padding: 3px;
  border-radius: 5px;
  display: flex;
  flex-shrink: 0;
}
.up-clear:hover {
  background: var(--jb-titlebar-btn-hover);
  color: var(--jb-text);
}
.up-hint {
  font-size: 11px;
  color: var(--jb-text-mute);
}
.up-actions {
  display: flex;
  align-items: center;
  gap: 8px;
}
.up-current {
  font-size: 11px;
  color: var(--jb-text-mute);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.up-log-card {
  flex: 1;
  min-height: 120px;
}
.up-log {
  height: 100%;
  overflow-y: auto;
  font-family: Consolas, "Courier New", monospace;
  font-size: 11.5px;
  line-height: 1.7;
}
.up-log-empty {
  color: var(--jb-text-mute);
}
.up-log-line {
  display: flex;
  gap: 8px;
}
.up-log-time {
  color: var(--jb-text-mute);
  flex-shrink: 0;
}
.up-log-text {
  color: var(--jb-text-soft);
  word-break: break-all;
}
.up-log-line.ok .up-log-text {
  color: var(--jb-success, #18a058);
}
.up-log-line.err .up-log-text {
  color: var(--jb-error, #d03050);
}
</style>
