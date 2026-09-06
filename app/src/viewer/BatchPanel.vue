<script setup lang="ts">
// 批量处理浮层（设计稿 六·批量处理）：批量重命名 / 转格式压缩 / 批量滤镜 三个 Tab
// 数据流：props.paths（父组件选中的图片列表）→ 各 Tab 规则 → 后端命令逐项处理
//   ① 重命名：batch_rename_preview 实时预览 → batch_rename 执行（返回失败列表）
//   ② 转格式：convert_image 逐张转换到输出目录（原名换扩展名），进度 + 错误累计
//   ③ 滤镜：open_image → convertFileSrc → Canvas 原尺寸合成（预设 css + 调节 css）→ save_image_blob 落盘
// 后端命令未就绪时 try/catch + useMessage 容错；全中文 UI
import { ref, computed, watch } from "vue";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
import {
  useMessage, NModal, NTabs, NTabPane, NInput, NInputNumber, NCheckbox,
  NSelect, NButton, NIcon, NSlider, NRadioGroup, NRadioButton,
} from "naive-ui";
import { Folder, PlayerPlay, Wand } from "@vicons/tabler";
import { FILTER_PRESETS, adjustToCss } from "./filters";

const props = defineProps<{
  /** 待处理的图片完整路径列表（为空时各执行按钮禁用） */
  paths: string[];
  /** 浮层显示态（父组件控制，关闭时 emit close 供父同步） */
  visible: boolean;
}>();

const emit = defineEmits<{
  (e: "close"): void;
  /** 任一批量处理完成后触发（供父组件刷新列表） */
  (e: "done"): void;
}>();

const message = useMessage();

// === 浮层显示态：本地 show 绑定 modal，跟随 visible；用户关闭时通知父组件 ===
const show = ref(props.visible);
watch(
  () => props.visible,
  (v) => {
    if (v !== show.value) show.value = v;
  },
  { immediate: true },
);
watch(show, (v) => {
  if (!v) emit("close");
});

// === 路径工具（兼容 / 与 \） ===
function baseName(p: string): string {
  return p.split(/[\\/]/).pop() ?? p;
}
function dirOf(p: string): string {
  const i = Math.max(p.lastIndexOf("\\"), p.lastIndexOf("/"));
  return i > 0 ? p.slice(0, i) : "";
}
/** 目录 + 文件名拼接（Windows 分隔符，目录尾部分隔符去重） */
function joinPath(dir: string, name: string): string {
  return `${dir.replace(/[\\/]+$/, "")}\\${name}`;
}

/** Windows 本地路径 → tauri asset URL（与 App.vue toAssetUrl 同规则） */
function toAssetUrl(p: string): string {
  if (!p) return "";
  if (/^https?:\/\//.test(p)) return p;
  if (/^[A-Za-z]:[\\/]/.test(p)) return convertFileSrc(p);
  return p;
}

/** 加载单张：cors 模式保证 Canvas 不被跨域污染（参考 CollageView） */
function loadImage(src: string): Promise<HTMLImageElement> {
  return new Promise((resolve, reject) => {
    const im = new Image();
    im.crossOrigin = "anonymous";
    im.onload = () => resolve(im);
    im.onerror = () => reject(new Error("图片解码失败"));
    im.src = src;
  });
}

const names = computed(() => props.paths.map(baseName));
const hasPaths = computed(() => props.paths.length > 0);

// ==================== Tab ① 批量重命名 ====================
const prefix = ref("");
const startNum = ref(1);
const digits = ref(3);
const useDate = ref(false);
const dateFmt = ref("yyyyMMdd");
const DATE_FMT_OPTS = [
  { label: "yyyyMMdd", value: "yyyyMMdd" },
  { label: "yyyy-MM-dd", value: "yyyy-MM-dd" },
  { label: "yyyy_MM_dd", value: "yyyy_MM_dd" },
];

/** 预览结果（与 names 一一对应的新文件名序列） */
const preview = ref<string[]>([]);
const renaming = ref(false);

/** 前 8 条新旧对照行 */
const previewRows = computed(() =>
  names.value.slice(0, 8).map((old, i) => ({ old, new: preview.value[i] ?? "" })),
);

/** 预览拉取（200ms 防抖，避免滑杆/输入连打时频繁 invoke） */
let pvTimer: number | null = null;
function schedulePreview() {
  if (pvTimer) window.clearTimeout(pvTimer);
  pvTimer = window.setTimeout(fetchPreview, 200);
}
async function fetchPreview() {
  if (!hasPaths.value) {
    preview.value = [];
    return;
  }
  try {
    preview.value = await invoke<string[]>("batch_rename_preview", {
      names: names.value,
      prefix: prefix.value,
      startNum: startNum.value,
      digits: digits.value,
      useDate: useDate.value,
      dateFmt: dateFmt.value,
    });
  } catch {
    preview.value = []; // 后端命令未就绪时静默容错（预览高频调用不弹 toast）
  }
}

const tab = ref<"rename" | "convert" | "filter">("rename");
watch(
  [() => props.paths, prefix, startNum, digits, useDate, dateFmt, show, tab],
  () => {
    if (show.value && tab.value === "rename") schedulePreview();
  },
  { immediate: true },
);

/** 执行重命名：pairs 的 old/new 均为完整路径（目录 + 预览新名） */
async function execRename() {
  if (!hasPaths.value || preview.value.length === 0 || renaming.value) return;
  renaming.value = true;
  try {
    const pairs = props.paths.map((p, i) => {
      const dir = dirOf(p);
      const newName = preview.value[i] ?? baseName(p);
      return { old: p, new: dir ? joinPath(dir, newName) : newName };
    });
    const failed = await invoke<string[]>("batch_rename", { pairs });
    const okN = pairs.length - failed.length;
    if (failed.length === 0) {
      message.success(`重命名成功 ${okN} 条`, { duration: 2200 });
    } else {
      message.warning(`成功 ${okN} 条 / 失败 ${failed.length} 条`, { duration: 2600 });
    }
    emit("done");
    show.value = false; // 关闭浮层（watch 会 emit close）
  } catch (e) {
    message.error(`重命名失败: ${e}`, { duration: 2200 });
  } finally {
    renaming.value = false;
  }
}

// ==================== Tab ② 转格式 / 压缩 ====================
const fmt = ref<"png" | "jpeg" | "webp">("png");
const quality = ref(85);
/** 输出目录（转格式 / 滤镜两个 Tab 共用一次选择） */
const outDir = ref("");
const converting = ref(false);
const conv = ref<{ done: number; errors: string[] }>({ done: 0, errors: [] });
const fmtTooltip = (v: number) => String(v);

async function pickOutDir() {
  try {
    const sel = await openFileDialog({ directory: true, multiple: false });
    if (typeof sel === "string" && sel) outDir.value = sel;
  } catch (e) {
    message.error(`打开目录选择失败: ${e}`, { duration: 2200 });
  }
}

const canConvert = computed(
  () => hasPaths.value && !!outDir.value && !converting.value,
);

/** 逐张转换：dst = 输出目录 / 原名换扩展名（jpeg 用 .jpg） */
async function execConvert() {
  if (!canConvert.value) return;
  converting.value = true;
  conv.value = { done: 0, errors: [] };
  const ext = fmt.value === "jpeg" ? "jpg" : fmt.value;
  for (const src of props.paths) {
    const base = baseName(src).replace(/\.[^.]+$/, "") || "image";
    const dst = joinPath(outDir.value, `${base}.${ext}`);
    try {
      await invoke("convert_image", {
        src,
        dst,
        format: fmt.value,
        quality: quality.value,
      });
    } catch (e) {
      conv.value.errors.push(`${baseName(src)}: ${e}`);
    }
    conv.value.done++;
  }
  converting.value = false;
  finishToast(conv.value, "转换");
}

/** 完成统一提示：全成功 success / 部分失败 warning；有产出即 emit done */
function finishToast(p: { done: number; errors: string[] }, label: string) {
  if (p.errors.length === 0) {
    message.success(`${label}完成，共 ${p.done} 张`, { duration: 2200 });
  } else {
    message.warning(
      `${label}完成：成功 ${p.done - p.errors.length} 张 / 失败 ${p.errors.length} 张`,
      { duration: 2600 },
    );
  }
  if (p.done > 0) emit("done");
}

// ==================== Tab ③ 批量滤镜 ====================
/** 选中滤镜 id（"" = 不用预设，仅调节；再次点击取消选中） */
const presetId = ref("");
const adjust = ref({ brightness: 100, contrast: 100, saturation: 100 });
const filtering = ref(false);
const filt = ref<{ done: number; errors: string[] }>({ done: 0, errors: [] });

/** 完整 CSS filter 串：预设在前、调节在后叠加（与 EditorView 同规则） */
const filterCss = computed(() => {
  const presetCss = FILTER_PRESETS.find((p) => p.id === presetId.value)?.css ?? "";
  return [
    presetCss,
    adjustToCss(adjust.value.brightness, adjust.value.contrast, adjust.value.saturation),
  ]
    .filter(Boolean)
    .join(" ");
});

const ADJ_ROWS = [
  { key: "brightness", label: "亮度", min: 50, max: 150 },
  { key: "contrast", label: "对比度", min: 50, max: 150 },
  { key: "saturation", label: "饱和度", min: 0, max: 200 },
] as const;

const canFilter = computed(
  () =>
    hasPaths.value &&
    !!outDir.value &&
    !!filterCss.value && // 无预设且调节全中性时无事可做
    !filtering.value,
);

/** 逐张合成：open_image → convertFileSrc → Image → Canvas 原尺寸底填白 → ctx.filter → drawImage
 *  → toBlob(png) → ArrayBuffer → save_image_blob 字节透传落盘（输出名：原名_滤镜id.png） */
async function execFilter() {
  if (!canFilter.value) return;
  filtering.value = true;
  filt.value = { done: 0, errors: [] };
  const css = filterCss.value;
  const suffix = presetId.value || "adjust"; // 无预设时以 _adjust 结缀
  for (const src of props.paths) {
    const base = baseName(src).replace(/\.[^.]+$/, "") || "image";
    try {
      const info = await invoke<{ url: string }>("open_image", { path: src });
      const im = await loadImage(toAssetUrl(info.url));
      const iw = im.naturalWidth;
      const ih = im.naturalHeight;
      if (iw <= 0 || ih <= 0) throw new Error("图片尺寸无效");
      const cv = document.createElement("canvas");
      cv.width = iw;
      cv.height = ih;
      const ctx = cv.getContext("2d");
      if (!ctx) throw new Error("画布初始化失败");
      // 先底填白（blur 类滤镜采样到边缘透明时防黑边），再整图滤镜绘制
      ctx.fillStyle = "#ffffff";
      ctx.fillRect(0, 0, iw, ih);
      ctx.filter = css || "none";
      ctx.drawImage(im, 0, 0);
      ctx.filter = "none";
      const blob = await new Promise<Blob>((res, rej) =>
        cv.toBlob((b) => (b ? res(b) : rej(new Error("导出失败"))), "image/png"),
      );
      const buf = await blob.arrayBuffer();
      await invoke("save_image_blob", {
        path: joinPath(outDir.value, `${base}_${suffix}.png`),
        data: [...new Uint8Array(buf)],
      });
    } catch (e) {
      filt.value.errors.push(`${baseName(src)}: ${e}`);
    }
    filt.value.done++;
  }
  filtering.value = false;
  finishToast(filt.value, "滤镜处理");
}
</script>

<template>
  <n-modal
    v-model:show="show"
    preset="card"
    title="批量处理"
    :bordered="false"
    style="width: 640px; max-width: 94vw"
  >
    <div class="bp-head">
      <span class="bp-count">已选 {{ paths.length }} 张图片</span>
      <span class="bp-note">转格式与滤镜输出为新文件，不修改原图</span>
    </div>

    <n-tabs v-model:value="tab" type="line" size="small" animated>
      <!-- ==================== ① 批量重命名 ==================== -->
      <n-tab-pane name="rename" tab="批量重命名">
        <div class="bp-form">
          <label class="field">
            <span class="f-label">前缀</span>
            <n-input v-model:value="prefix" size="small" placeholder="如 IMG_" clearable />
          </label>
          <label class="field">
            <span class="f-label">起始序号</span>
            <n-input-number
              v-model:value="startNum"
              size="small"
              :min="0"
              :show-button="false"
              style="width: 100%"
            />
          </label>
          <label class="field">
            <span class="f-label">序号位数</span>
            <n-input-number
              v-model:value="digits"
              size="small"
              :min="2"
              :max="6"
              style="width: 100%"
            />
          </label>
          <label class="field">
            <span class="f-label">日期格式</span>
            <n-select
              v-model:value="dateFmt"
              size="small"
              :options="DATE_FMT_OPTS"
              :disabled="!useDate"
            />
          </label>
          <div class="field">
            <span class="f-label" />
            <n-checkbox v-model:checked="useDate" size="small">
              文件名中使用日期
            </n-checkbox>
          </div>
        </div>

        <!-- 实时预览表：前 8 条新旧对照 + 共 N 条 -->
        <div class="pv-box">
          <div class="pv-head">
            预览（前 {{ previewRows.length }} 条 · 共 {{ preview.length }} 条）
          </div>
          <div v-if="previewRows.length" class="pv-list">
            <div v-for="(r, i) in previewRows" :key="i" class="pv-row">
              <span class="pv-old ellip" :title="r.old">{{ r.old }}</span>
              <span class="pv-arrow">→</span>
              <span class="pv-new ellip" :title="r.new">{{ r.new }}</span>
            </div>
          </div>
          <div v-else class="pv-empty">
            {{ hasPaths ? "正在生成预览…" : "还没有选中图片" }}
          </div>
        </div>

        <div class="bp-actions">
          <n-button
            size="small"
            type="primary"
            :loading="renaming"
            :disabled="!hasPaths || preview.length === 0"
            @click="execRename"
          >
            <template #icon>
              <n-icon :component="PlayerPlay" size="14" />
            </template>
            执行重命名
          </n-button>
          <span class="bp-warn">重命名直接作用于原文件，请先确认预览结果</span>
        </div>
      </n-tab-pane>

      <!-- ==================== ② 转格式 / 压缩 ==================== -->
      <n-tab-pane name="convert" tab="转格式 / 压缩">
        <div class="bp-form one">
          <div class="field">
            <span class="f-label">目标格式</span>
            <n-radio-group v-model:value="fmt" size="small">
              <n-radio-button value="png" label="PNG" />
              <n-radio-button value="jpeg" label="JPEG" />
              <n-radio-button value="webp" label="WebP" />
            </n-radio-group>
          </div>
          <div class="field">
            <span class="f-label">质量</span>
            <div class="q-wrap">
              <n-slider
                v-model:value="quality"
                :min="40"
                :max="100"
                :step="1"
                :disabled="fmt === 'png'"
                :format-tooltip="fmtTooltip"
              />
              <span class="q-val">{{ quality }}</span>
            </div>
            <span v-if="fmt === 'png'" class="q-hint">
              PNG 为无损格式，忽略质量参数
            </span>
          </div>
          <div class="field">
            <span class="f-label">输出目录</span>
            <div class="dir-row">
              <n-input
                v-model:value="outDir"
                size="small"
                readonly
                placeholder="选择转换结果的保存目录"
              />
              <n-button size="small" @click="pickOutDir">
                <template #icon>
                  <n-icon :component="Folder" size="14" />
                </template>
                选择目录
              </n-button>
            </div>
          </div>
        </div>

        <div class="bp-actions">
          <n-button
            size="small"
            type="primary"
            :loading="converting"
            :disabled="!canConvert"
            @click="execConvert"
          >
            <template #icon>
              <n-icon :component="PlayerPlay" size="14" />
            </template>
            开始转换
          </n-button>
          <span v-if="converting || conv.done > 0" class="prog">
            {{ conv.done }}/{{ paths.length }}
          </span>
        </div>
        <div v-if="conv.errors.length" class="err-list">
          <div
            v-for="(er, i) in conv.errors"
            :key="i"
            class="err-row ellip"
            :title="er"
          >
            {{ er }}
          </div>
        </div>
      </n-tab-pane>

      <!-- ==================== ③ 批量滤镜 ==================== -->
      <n-tab-pane name="filter" tab="批量滤镜">
        <!-- 滤镜预设网格（单选，再次点击取消） -->
        <div class="f-label-row">滤镜预设</div>
        <div class="preset-grid">
          <button
            v-for="p in FILTER_PRESETS"
            :key="p.id"
            class="preset-item"
            :class="{ active: presetId === p.id }"
            :title="p.name"
            @click="presetId = presetId === p.id ? '' : p.id"
          >
            <n-icon :component="Wand" size="12" class="pi-icon" />
            <span class="pi-name">{{ p.name }}</span>
          </button>
        </div>
        <div class="f-label-row">亮度 / 对比度 / 饱和度（可选，默认中性）</div>
        <div class="adj-box">
          <div v-for="row in ADJ_ROWS" :key="row.key" class="adj-row">
            <span class="adj-label">{{ row.label }}</span>
            <n-slider
              v-model:value="adjust[row.key]"
              :min="row.min"
              :max="row.max"
              :step="1"
              :format-tooltip="fmtTooltip"
            />
            <span class="adj-val">{{ adjust[row.key] }}</span>
          </div>
        </div>

        <div class="field dir-field">
          <span class="f-label">输出目录</span>
          <div class="dir-row">
            <n-input
              v-model:value="outDir"
              size="small"
              readonly
              placeholder="选择滤镜结果的保存目录"
            />
            <n-button size="small" @click="pickOutDir">
              <template #icon>
                <n-icon :component="Folder" size="14" />
              </template>
              选择目录
            </n-button>
          </div>
        </div>

        <div class="bp-actions">
          <n-button
            size="small"
            type="primary"
            :loading="filtering"
            :disabled="!canFilter"
            @click="execFilter"
          >
            <template #icon>
              <n-icon :component="PlayerPlay" size="14" />
            </template>
            开始处理
          </n-button>
          <span v-if="filtering || filt.done > 0" class="prog">
            {{ filt.done }}/{{ paths.length }}
          </span>
          <span class="bp-note">输出为 PNG：原名_滤镜id.png</span>
        </div>
        <div v-if="filt.errors.length" class="err-list">
          <div
            v-for="(er, i) in filt.errors"
            :key="i"
            class="err-row ellip"
            :title="er"
          >
            {{ er }}
          </div>
        </div>
      </n-tab-pane>
    </n-tabs>

    <template #footer>
      <div class="bp-footer">
        <span class="bp-note">处理完成后可在相册中刷新查看结果</span>
        <n-button size="small" quaternary @click="show = false">关闭</n-button>
      </div>
    </template>
  </n-modal>
</template>

<style scoped>
/* ==================== 头部说明条 ==================== */
.bp-head {
  display: flex;
  align-items: baseline;
  gap: 12px;
  margin-bottom: 10px;
}
.bp-count {
  font-size: 12px;
  font-weight: 600;
  color: var(--jb-primary);
}
.ellip {
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

/* ==================== 表单区（两列栅格） ==================== */
.bp-form {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 10px 14px;
  margin-bottom: 12px;
}
.bp-form.one {
  grid-template-columns: 1fr;
}
.field {
  display: flex;
  flex-direction: column;
  gap: 5px;
  min-width: 0;
}
.f-label {
  font-size: 11px;
  color: var(--jb-text-mute);
  letter-spacing: 1px;
  user-select: none;
}
.f-label-row {
  font-size: 11px;
  color: var(--jb-text-mute);
  letter-spacing: 1px;
  margin: 4px 0 8px;
  user-select: none;
}

/* 质量滑杆行 */
.q-wrap {
  display: flex;
  align-items: center;
  gap: 12px;
}
.q-wrap .n-slider {
  flex: 1;
}
.q-val {
  font-size: 11px;
  color: var(--jb-text);
  font-variant-numeric: tabular-nums;
  min-width: 26px;
  text-align: right;
}
.q-hint {
  font-size: 10.5px;
  color: var(--jb-text-mute);
}

/* 输出目录行 */
.dir-row {
  display: flex;
  gap: 8px;
  align-items: center;
}
.dir-row .n-input {
  flex: 1;
}
.dir-field {
  margin: 12px 0 0;
}

/* ==================== 重命名预览表 ==================== */
.pv-box {
  border: 1px solid var(--jb-border);
  border-radius: 12px;
  background: var(--jb-bg);
  overflow: hidden;
}
.pv-head {
  padding: 8px 12px;
  font-size: 11px;
  color: var(--jb-text-mute);
  border-bottom: 1px solid var(--jb-divider);
  background: var(--jb-bg-card);
}
.pv-list {
  max-height: 200px;
  overflow-y: auto;
  scrollbar-width: thin;
  scrollbar-color: var(--jb-scrollbar) transparent;
}
.pv-row {
  display: grid;
  grid-template-columns: minmax(0, 1fr) 20px minmax(0, 1fr);
  gap: 8px;
  align-items: center;
  padding: 6px 12px;
  font-size: 11.5px;
}
.pv-row:nth-child(odd) {
  background: color-mix(in srgb, var(--jb-bg-card) 55%, transparent);
}
.pv-old {
  color: var(--jb-text-mute);
}
.pv-arrow {
  color: var(--jb-primary);
  text-align: center;
}
.pv-new {
  color: var(--jb-text);
  font-weight: 500;
}
.pv-empty {
  padding: 20px 12px;
  text-align: center;
  font-size: 11.5px;
  color: var(--jb-text-mute);
}

/* ==================== 滤镜预设网格 ==================== */
.preset-grid {
  display: grid;
  grid-template-columns: repeat(4, 1fr);
  gap: 8px;
  margin-bottom: 14px;
}
.preset-item {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  gap: 6px;
  padding: 8px 6px;
  border: 1px solid var(--jb-border);
  border-radius: 12px;
  background: var(--jb-bg-card);
  color: var(--jb-text-soft);
  font-size: 11.5px;
  cursor: pointer;
  white-space: nowrap;
  overflow: hidden;
  transition: color 200ms ease, border-color 200ms ease,
    background-color 200ms ease, transform 200ms ease;
}
.preset-item:hover {
  color: var(--jb-text);
  border-color: var(--jb-primary);
  transform: translateY(-1px);
}
.preset-item.active {
  color: var(--jb-primary);
  border-color: var(--jb-primary);
  background: color-mix(in srgb, var(--jb-primary) 12%, transparent);
  font-weight: 600;
}
.pi-icon {
  flex-shrink: 0;
  opacity: 0.75;
}
.pi-name {
  overflow: hidden;
  text-overflow: ellipsis;
}

/* 调节滑杆区 */
.adj-box {
  display: flex;
  flex-direction: column;
  gap: 10px;
  border: 1px solid var(--jb-border);
  border-radius: 12px;
  padding: 12px 14px;
  background: var(--jb-bg);
  margin-bottom: 4px;
}
.adj-row {
  display: grid;
  grid-template-columns: 52px minmax(0, 1fr) 34px;
  gap: 12px;
  align-items: center;
}
.adj-label {
  font-size: 11.5px;
  color: var(--jb-text);
}
.adj-val {
  font-size: 11px;
  color: var(--jb-text-mute);
  font-variant-numeric: tabular-nums;
  text-align: right;
}

/* ==================== 操作行 / 进度 / 错误列表 ==================== */
.bp-actions {
  display: flex;
  align-items: center;
  gap: 12px;
  margin-top: 12px;
  flex-wrap: wrap;
}
.bp-warn {
  font-size: 10.5px;
  color: var(--jb-red);
}
.bp-note {
  font-size: 10.5px;
  color: var(--jb-text-mute);
}
.prog {
  font-size: 11.5px;
  font-weight: 600;
  color: var(--jb-primary);
  font-variant-numeric: tabular-nums;
}
.err-list {
  margin-top: 10px;
  max-height: 110px;
  overflow-y: auto;
  border: 1px solid color-mix(in srgb, var(--jb-red) 30%, transparent);
  border-radius: 12px;
  background: color-mix(in srgb, var(--jb-red) 6%, transparent);
  scrollbar-width: thin;
  scrollbar-color: var(--jb-scrollbar) transparent;
}
.err-row {
  padding: 5px 10px;
  font-size: 10.5px;
  color: var(--jb-red);
}

/* 底部 */
.bp-footer {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
}
</style>
