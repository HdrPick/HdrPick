<!-- 画质增强参数调节卡片（V18）——视频播放动态降噪 / 锐化 / 色彩
     数据源：video_play_fx_status（config 持久值 + 播放器运行态）
     写入：video_play_fx_set（实时 PostMessage 生效 + config 持久化）、video_play_fx_reset
     键位联动：播放器内 P 循环 FX、; / ' 档位、\ 复位（详见播放器 F1 帮助） -->
<template>
  <n-modal
    :show="props.show"
    preset="card"
    title="画质增强（视频播放）"
    class="fx-modal"
    :bordered="false"
    to="#modal-anchor"
    style="max-width: min(92vw, 440px)"
    @update:show="(v: boolean) => emit('update:show', v)"
  >
    <div class="fx-body">
      <div v-if="!status.running" class="fx-off-hint">
        播放器未运行——当前设置将在下次打开播放器时生效
      </div>

      <div class="fx-row">
        <span class="fx-label">效果</span>
        <n-select
          v-model:value="curShader"
          :options="shaderOptions"
          size="small"
          style="width: 240px"
          @update:value="onShaderChange"
        />
      </div>

      <template v-if="curShader && FX_DEFS[curShader]?.params.length">
        <div v-for="p in FX_DEFS[curShader].params" :key="p.slot" class="fx-row">
          <span class="fx-label">{{ p.label }}</span>
          <div class="fx-slider">
            <n-slider
              :value="params[p.slot] ?? p.def"
              :min="p.min"
              :max="p.max"
              :step="p.step"
              :format-tooltip="fmtTip"
              size="small"
              @update:value="(v: number) => onParamChange(p, v)"
            />
            <span class="fx-val">{{ (params[p.slot] ?? p.def).toFixed(2) }}</span>
          </div>
        </div>
      </template>

      <div v-else-if="curShader && curShader !== 'OFF'" class="fx-no-params">
        该效果无可调参数
      </div>

      <div class="fx-foot">
        <span class="fx-hint">
          {{ curDesc || "静止画面时间平均消噪 · 运动画面自动防拖影" }}
        </span>
        <n-button size="tiny" quaternary @click="onReset">复位</n-button>
      </div>
      <div class="fx-key-hint">播放器内：P 循环效果 · ; / ' 调主参数 · \ 复位 · F1 全部快捷键</div>
    </div>
  </n-modal>
</template>

<script setup lang="ts">
import { ref, computed, watch } from "vue";
import { NModal, NSelect, NSlider, NButton } from "naive-ui";
import { invoke } from "@tauri-apps/api/core";

interface FxStatus {
  running: boolean;
  shader: string;
  params: number[];
}
interface FxParamDef {
  slot: number;
  label: string;
  min: number;
  max: number;
  step: number;
  def: number;
}
interface FxDef {
  label: string;
  desc: string;
  params: FxParamDef[];
}

// FX 参数定义（与 videorec renderer.rs 内建 shader // param<N> 元数据一致）
const FX_DEFS: Record<string, FxDef> = {
  "ENHANCE-ALL": {
    label: "一键画质增强",
    desc: "动态降噪 + CAS 锐化 + 色彩（推荐日常使用）",
    params: [
      { slot: 0, label: "降噪强度", min: 0, max: 1, step: 0.01, def: 0.35 },
      { slot: 1, label: "锐化量", min: 0, max: 1, step: 0.01, def: 0.4 },
      { slot: 2, label: "饱和度", min: 0, max: 2, step: 0.01, def: 1 },
      { slot: 3, label: "对比度", min: 0.5, max: 1.5, step: 0.01, def: 1 },
    ],
  },
  "DENOISE-DYN": {
    label: "动态降噪",
    desc: "时域运动自适应 + 空间双边滤波",
    params: [
      { slot: 0, label: "降噪强度", min: 0, max: 1, step: 0.01, def: 0.35 },
      { slot: 1, label: "运动阈值", min: 0, max: 1, step: 0.01, def: 0.12 },
    ],
  },
  "CAS-SHARPEN": {
    label: "自适应锐化",
    desc: "细节区强锐化 · 高对比边缘防 halo",
    params: [{ slot: 0, label: "锐化量", min: 0, max: 1, step: 0.01, def: 0.4 }],
  },
  "COLOR-TUNE": {
    label: "色彩调整",
    desc: "亮度 → 对比度 → 饱和度 → 伽马",
    params: [
      { slot: 0, label: "饱和度", min: 0, max: 2, step: 0.01, def: 1 },
      { slot: 1, label: "对比度", min: 0.5, max: 1.5, step: 0.01, def: 1 },
      { slot: 2, label: "亮度", min: -0.25, max: 0.25, step: 0.01, def: 0 },
      { slot: 3, label: "伽马", min: 0.5, max: 2, step: 0.01, def: 1 },
    ],
  },
  "SHARPEN": { label: "锐化（轻量）", desc: "", params: [] },
  "DEINT-BLEND": { label: "反交错混合", desc: "", params: [] },
  "GRAY": { label: "黑白", desc: "", params: [] },
  "INVERT": { label: "反色", desc: "", params: [] },
};

const props = defineProps<{ show: boolean }>();
const emit = defineEmits<{ (e: "update:show", v: boolean): void }>();

const status = ref<FxStatus>({ running: false, shader: "ENHANCE-ALL", params: [0.35, 0.4, 1, 1, 0, 0, 0, 0] });
const curShader = ref<string>("ENHANCE-ALL");
const params = ref<number[]>([0.35, 0.4, 1, 1, 0, 0, 0, 0]);

const shaderOptions = computed(() => [
  { label: "关闭", value: "OFF" },
  ...Object.entries(FX_DEFS).map(([v, d]) => ({ label: d.label, value: v })),
]);
const curDesc = computed(() => (curShader.value ? FX_DEFS[curShader.value]?.desc ?? "" : ""));

const fmtTip = (v: number) => v.toFixed(2);

async function refresh() {
  try {
    const s = await invoke<FxStatus>("video_play_fx_status");
    status.value = s;
    curShader.value = s.shader || "OFF";
    params.value = [...(s.params ?? [])];
  } catch (_) {
    /* 配置读取失败保持默认 */
  }
}
watch(() => props.show, (v) => { if (v) refresh(); });

// FX 切换：该 FX 全部参数槽回默认值后发送
function onShaderChange(v: string) {
  const next = [0, 0, 0, 0, 0, 0, 0, 0];
  const defs = FX_DEFS[v]?.params ?? [];
  for (const p of defs) next[p.slot] = p.def;
  // 未被当前 FX 声明的槽位保留原值（切换回 ENHANCE-ALL 等时不丢用户调整）
  for (let i = 0; i < 8; i++) if (next[i] === 0 && params.value[i] !== undefined) next[i] = params.value[i];
  params.value = next;
  void push(curShader.value, next);
}

// 滑条调整：节流 ~80ms 实时生效（PotPlayer 即调即见）
let timer: ReturnType<typeof setTimeout> | null = null;
function onParamChange(p: FxParamDef, v: number) {
  params.value[p.slot] = v;
  if (timer) return;
  timer = setTimeout(() => {
    timer = null;
    void push(curShader.value, params.value);
  }, 80);
}

async function onReset() {
  try {
    await invoke("video_play_fx_reset");
    // 复位后回读当前 FX 元数据默认（简化：按前端定义重置）
    const defs = FX_DEFS[curShader.value]?.params ?? [];
    for (const p of defs) params.value[p.slot] = p.def;
  } catch (_) {
    /* 播放器未运行 */
  }
}

async function push(shader: string, ps: number[]) {
  try {
    await invoke("video_play_fx_set", { shader, params: ps });
  } catch (_) {
    /* 持久化失败静默 */
  }
}
</script>

<style scoped>
.fx-body {
  display: flex;
  flex-direction: column;
  gap: 12px;
  padding: 2px 0;
}
.fx-off-hint {
  font-size: 12px;
  color: #d48806;
  background: #fffbe6;
  border: 1px solid #ffe58f;
  border-radius: 6px;
  padding: 6px 10px;
}
.fx-row {
  display: flex;
  align-items: center;
  gap: 12px;
}
.fx-label {
  width: 64px;
  flex: none;
  font-size: 13px;
  text-align: right;
  color: var(--fx-label, #555);
}
.fx-slider {
  flex: 1;
  display: flex;
  align-items: center;
  gap: 8px;
}
.fx-val {
  width: 40px;
  flex: none;
  font-size: 12px;
  font-variant-numeric: tabular-nums;
  color: #888;
}
.fx-no-params {
  font-size: 12px;
  color: #999;
  text-align: center;
  padding: 4px 0;
}
.fx-foot {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
}
.fx-hint {
  font-size: 12px;
  color: #999;
}
.fx-key-hint {
  font-size: 11px;
  color: #bbb;
  border-top: 1px dashed #eee;
  padding-top: 8px;
}
</style>
