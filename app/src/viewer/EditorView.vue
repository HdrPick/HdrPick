<script setup lang="ts">
// 滤镜编辑页（设计稿 六·编辑页）：左功能菜单（滤镜/调节/裁剪三段切换）+ 中间预览区 + 右侧参数面板 + 底部操作条
// 数据流：open_image(path) → url/width/height → 预览 <img> 走 CSS filter 实时渲染（与 Canvas ctx.filter 同语法）
// 保存：plugin-dialog 选路径 → Canvas2D 以原始尺寸合成（预设 css + 调节 css 叠加 + 裁剪区域）→
//       canvas.toBlob → ArrayBuffer → invoke("save_image_blob", { path, data }) 字节透传后端落盘
// 注意：ctx.filter 在 WebView2 支持良好可放心用；blur 类滤镜合成前先底填白，避免边缘透明发黑
import { ref, computed, watch, onMounted, onUnmounted } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { save as saveFileDialog } from "@tauri-apps/plugin-dialog";
import { NButton, NIcon, NSlider, NSpin, useMessage } from "naive-ui";
import { Wand, Sun, Crop, Eye, Rotate, Check } from "@vicons/tabler";
import { FILTER_PRESETS, adjustToCss } from "./filters";

const props = defineProps<{
  /** 待编辑图片的完整路径 */
  path: string;
}>();

const emit = defineEmits<{
  (e: "close"): void;
  /** 保存成功（参数为另存路径） */
  (e: "saved", path: string): void;
}>();

const message = useMessage();

// === 图片状态（open_image 返回，仅取 url/width/height，has_exif 等字段不用） ===
const imageUrl = ref("");
const imgW = ref(0);
const imgH = ref(0);
const loading = ref(false);

// === 三段功能切换 ===
const TABS = [
  { id: "filter", label: "滤镜", icon: Wand },
  { id: "adjust", label: "调节", icon: Sun },
  { id: "crop", label: "裁剪", icon: Crop },
] as const;
type TabId = "filter" | "adjust" | "crop";
const tab = ref<TabId>("filter");
const currentTabLabel = computed(
  () => TABS.find((t) => t.id === tab.value)?.label ?? "",
);

// === 滤镜 / 调节状态 ===
/** 选中滤镜 id（"" = 无滤镜） */
const presetId = ref("");
const preset = computed(
  () => FILTER_PRESETS.find((p) => p.id === presetId.value) ?? null,
);
const adjust = ref({ brightness: 100, contrast: 100, saturation: 100 });
const adjustCss = computed(() =>
  adjustToCss(adjust.value.brightness, adjust.value.contrast, adjust.value.saturation),
);
/** 完整 CSS filter 串：预设在前、调节在后叠加（"" = 无任何处理） */
const fullCss = computed(() =>
  [preset.value?.css ?? "", adjustCss.value].filter(Boolean).join(" "),
);

// === 调节滑杆配置（50-150 / 50-150 / 0-200，100 = 中性） ===
const ADJ_ROWS = [
  { key: "brightness", label: "亮度", min: 50, max: 150 },
  { key: "contrast", label: "对比度", min: 50, max: 150 },
  { key: "saturation", label: "饱和度", min: 0, max: 200 },
] as const;
const fmtTooltip = (v: number) => String(v);

// === 裁剪状态（归一化坐标 0-1，相对原图；换算像素 = n × imgW/imgH） ===
const crop = ref({ x: 0, y: 0, w: 1, h: 1 });
/** 锁定比例（null = 自由） */
const ratio = ref<number | null>(null);
const RATIOS: Array<{ label: string; value: number | null }> = [
  { label: "自由", value: null },
  { label: "1:1", value: 1 },
  { label: "4:3", value: 4 / 3 },
  { label: "3:4", value: 3 / 4 },
  { label: "16:9", value: 16 / 9 },
];
const EPS = 0.001;
const cropActive = computed(
  () =>
    crop.value.x > EPS ||
    crop.value.y > EPS ||
    crop.value.w < 1 - EPS ||
    crop.value.h < 1 - EPS,
);
/** 裁剪框像素值（原始尺寸坐标系，参数面板实时显示用） */
const cropPx = computed(() => ({
  x: Math.round(crop.value.x * imgW.value),
  y: Math.round(crop.value.y * imgH.value),
  w: Math.round(crop.value.w * imgW.value),
  h: Math.round(crop.value.h * imgH.value),
}));

/** 选比例：锁定时重置为该比例下的最大居中框；自由仅解除锁定 */
function applyRatio(r: number | null) {
  ratio.value = r;
  if (r == null || imgW.value <= 0 || imgH.value <= 0) return;
  const a = imgW.value / imgH.value; // 原图宽高比
  // 归一化宽高满足 (w·imgW)/(h·imgH) = r → w/h = r/a
  let h = 0.9;
  let w = (h * r) / a;
  if (w > 0.9) {
    w = 0.9;
    h = (w * a) / r;
  }
  crop.value = { x: (1 - w) / 2, y: (1 - h) / 2, w, h };
}

/** 重置裁剪（回到整图 + 自由比例） */
function resetCrop() {
  ratio.value = null;
  crop.value = { x: 0, y: 0, w: 1, h: 1 };
}

/** 重置全部：滤镜 + 调节 + 裁剪 */
function resetAll() {
  presetId.value = "";
  adjust.value = { brightness: 100, contrast: 100, saturation: 100 };
  resetCrop();
}

// === 按住查看原图（原图对比：按住时移除全部滤镜/裁剪显示原图） ===
const showOriginal = ref(false);
const previewFilter = computed(() =>
  showOriginal.value ? "none" : fullCss.value,
);
function holdPeek(e: PointerEvent) {
  showOriginal.value = true;
  // 捕获指针，保证移出按钮也能收到抬起事件
  (e.currentTarget as HTMLElement).setPointerCapture?.(e.pointerId);
}
function releasePeek() {
  showOriginal.value = false;
}

// === 预览区自适应：contain 适配（留 28px 呼吸边距），裁剪层与图片精确对齐 ===
const stageRef = ref<HTMLElement | null>(null);
const fitBoxRef = ref<HTMLElement | null>(null);
const stageW = ref(0);
const stageH = ref(0);
const PAD = 28;
const fit = computed(() => {
  const sw = stageW.value - PAD * 2;
  const sh = stageH.value - PAD * 2;
  if (imgW.value <= 0 || imgH.value <= 0 || sw <= 0 || sh <= 0) {
    return { w: 0, h: 0 };
  }
  const k = Math.min(sw / imgW.value, sh / imgH.value);
  return { w: Math.floor(imgW.value * k), h: Math.floor(imgH.value * k) };
});

let resizeObserver: ResizeObserver | null = null;

// === 裁剪框拖拽（整体移动 + 四角手柄缩放，Canvas 坐标换算的逆过程：屏幕 → 归一化 → 像素） ===
type Corner = "nw" | "ne" | "sw" | "se";
const CORNERS: Corner[] = ["nw", "ne", "sw", "se"];
interface DragState {
  mode: "move" | Corner;
  /** 起始指针（归一化） */
  baseX: number;
  baseY: number;
  /** 起始裁剪框 */
  orig: { x: number; y: number; w: number; h: number };
  /** 缩放锚点 = 对角（归一化） */
  ax: number;
  ay: number;
}
let drag: DragState | null = null;

const clamp01 = (v: number) => Math.min(1, Math.max(0, v));

/** 屏幕坐标 → 图片归一化坐标（以预览贴合框为基准） */
function ptrNorm(e: PointerEvent) {
  const el = fitBoxRef.value;
  if (!el) return { x: 0, y: 0 };
  const r = el.getBoundingClientRect();
  return {
    x: clamp01((e.clientX - r.left) / r.width),
    y: clamp01((e.clientY - r.top) / r.height),
  };
}

function startMove(e: PointerEvent) {
  if (e.button !== 0) return;
  const p = ptrNorm(e);
  drag = { mode: "move", baseX: p.x, baseY: p.y, orig: { ...crop.value }, ax: 0, ay: 0 };
  (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
}

function startResize(e: PointerEvent, corner: Corner) {
  if (e.button !== 0) return;
  e.stopPropagation(); // 手柄优先于框体的移动逻辑
  const c = crop.value;
  // 锚点 = 被拖拽角的对角
  const ax = corner === "nw" || corner === "sw" ? c.x + c.w : c.x;
  const ay = corner === "nw" || corner === "ne" ? c.y + c.h : c.y;
  const p = ptrNorm(e);
  drag = { mode: corner, baseX: p.x, baseY: p.y, orig: { ...c }, ax, ay };
  (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
}

function onDragMove(e: PointerEvent) {
  if (!drag) return;
  const p = ptrNorm(e);
  if (drag.mode === "move") {
    // 整体移动：起点 + 位移，夹在画面内
    const nx = clamp01(drag.orig.x + (p.x - drag.baseX));
    const ny = clamp01(drag.orig.y + (p.y - drag.baseY));
    crop.value = {
      x: Math.min(nx, 1 - drag.orig.w),
      y: Math.min(ny, 1 - drag.orig.h),
      w: drag.orig.w,
      h: drag.orig.h,
    };
    return;
  }
  // 四角缩放
  const a = imgW.value / imgH.value;
  const r = ratio.value;
  const MIN_PX = 12; // 最小手柄行程（显示像素）
  const fw = fitBoxRef.value?.clientWidth ?? 1;
  const fh = fitBoxRef.value?.clientHeight ?? 1;
  const minW = MIN_PX / fw;
  const minH = MIN_PX / fh;
  const sx = p.x >= drag.ax ? 1 : -1;
  const sy = p.y >= drag.ay ? 1 : -1;
  const availW = sx > 0 ? 1 - drag.ax : drag.ax;
  const availH = sy > 0 ? 1 - drag.ay : drag.ay;
  let wN = Math.max(Math.abs(p.x - drag.ax), minW);
  let hN = Math.max(Math.abs(p.y - drag.ay), minH);
  if (r != null) {
    // 锁定比例：h = w·a/r（由 (w·imgW)/(h·imgH)=r 推得），先保最小再夹可用空间
    hN = (wN * a) / r;
    if (hN < minH) {
      hN = minH;
      wN = (hN * r) / a;
    }
    if (wN > availW) {
      wN = availW;
      hN = (wN * a) / r;
    }
    if (hN > availH) {
      hN = availH;
      wN = (hN * r) / a;
    }
  }
  wN = Math.min(wN, availW);
  hN = Math.min(hN, availH);
  if (wN <= 0 || hN <= 0) return;
  crop.value = {
    x: sx > 0 ? drag.ax : drag.ax - wN,
    y: sy > 0 ? drag.ay : drag.ay - hN,
    w: wN,
    h: hN,
  };
}

function endDrag() {
  drag = null;
}

const cropBoxStyle = computed(() => ({
  left: `${crop.value.x * 100}%`,
  top: `${crop.value.y * 100}%`,
  width: `${crop.value.w * 100}%`,
  height: `${crop.value.h * 100}%`,
}));

// === 图片加载（沿用 open_image；宽高以解码结果为准） ===
async function load() {
  loading.value = true;
  try {
    const info = await invoke<{ url: string; width: number; height: number }>(
      "open_image",
      { path: props.path },
    );
    imageUrl.value = info.url;
  } catch (e) {
    imageUrl.value = "";
    imgW.value = 0;
    imgH.value = 0;
    message.error(`打开图片失败: ${e}`, { duration: 2200 });
  } finally {
    loading.value = false;
  }
}

/** 独立解码一张图（保存合成用，也用于取 naturalWidth/Height） */
function loadHtmlImage(url: string): Promise<HTMLImageElement> {
  return new Promise((res, rej) => {
    const im = new Image();
    im.onload = () => res(im);
    im.onerror = () => rej(new Error("图片解码失败"));
    im.src = url;
  });
}

// url 变化：重取原始尺寸 + 重置裁剪
watch(imageUrl, async (u) => {
  resetCrop();
  if (!u) {
    imgW.value = 0;
    imgH.value = 0;
    return;
  }
  try {
    const im = await loadHtmlImage(u);
    if (imageUrl.value === u) {
      imgW.value = im.naturalWidth;
      imgH.value = im.naturalHeight;
    }
  } catch {
    imgW.value = 0;
    imgH.value = 0;
  }
});

watch(
  () => props.path,
  () => load(),
);

onMounted(() => {
  load();
  resizeObserver = new ResizeObserver((entries) => {
    const r = entries[0].contentRect;
    stageW.value = r.width;
    stageH.value = r.height;
  });
  if (stageRef.value) resizeObserver.observe(stageRef.value);
});
onUnmounted(() => {
  resizeObserver?.disconnect();
});

// === 保存另存为（转格式仅 PNG / JPEG） ===
const SAVE_FILTERS = [
  { name: "PNG 图片", extensions: ["png"] },
  { name: "JPEG 图片", extensions: ["jpg", "jpeg"] },
];
const saving = ref(false);

async function handleSave() {
  if (!imageUrl.value || saving.value) return;
  saving.value = true;
  try {
    const fileName = props.path.split(/[\\/]/).pop() ?? "image.png";
    const base = fileName.replace(/\.[^.]+$/, "") || "image";
    const dst = await saveFileDialog({
      filters: SAVE_FILTERS,
      defaultPath: `${base}-编辑.png`,
    });
    if (!dst) return;
    // 按扩展名决定 mime（png/jpeg；其余按 png 兜底）
    const ext = (dst.split(".").pop() ?? "").toLowerCase();
    const mime =
      ext === "jpg" || ext === "jpeg" ? "image/jpeg" : "image/png";

    // 1) 独立解码原图（原始尺寸）
    const im = await loadHtmlImage(imageUrl.value);
    const iw = im.naturalWidth;
    const ih = im.naturalHeight;
    if (iw <= 0 || ih <= 0) throw new Error("图片尺寸无效");

    // 2) 全图滤镜合成：先底填白（blur 采样到边缘透明时防黑边），再 ctx.filter 画原图
    const tmp = document.createElement("canvas");
    tmp.width = iw;
    tmp.height = ih;
    const tc = tmp.getContext("2d");
    if (!tc) throw new Error("画布初始化失败");
    tc.fillStyle = "#ffffff";
    tc.fillRect(0, 0, iw, ih);
    tc.filter = fullCss.value || "none";
    tc.drawImage(im, 0, 0);
    tc.filter = "none";

    // 3) 裁剪区域 → 输出画布（归一化 → 像素坐标换算，夹在画面内）
    const sx = Math.min(iw - 1, Math.max(0, Math.round(crop.value.x * iw)));
    const sy = Math.min(ih - 1, Math.max(0, Math.round(crop.value.y * ih)));
    const cw = Math.min(iw - sx, Math.max(1, Math.round(crop.value.w * iw)));
    const ch = Math.min(ih - sy, Math.max(1, Math.round(crop.value.h * ih)));
    const out = document.createElement("canvas");
    out.width = cw;
    out.height = ch;
    const oc = out.getContext("2d");
    if (!oc) throw new Error("画布初始化失败");
    oc.fillStyle = "#ffffff";
    oc.fillRect(0, 0, cw, ch);
    oc.drawImage(tmp, sx, sy, cw, ch, 0, 0, cw, ch);

    // 4) 导出 blob → 字节透传后端落盘
    const blob = await new Promise<Blob>((res, rej) =>
      out.toBlob(
        (b) => (b ? res(b) : rej(new Error("导出失败"))),
        mime,
        0.92,
      ),
    );
    const buf = await blob.arrayBuffer();
    await invoke("save_image_blob", {
      path: dst,
      data: [...new Uint8Array(buf)],
    });
    message.success("已保存", { duration: 2200 });
    emit("saved", dst);
  } catch (e) {
    message.error(`保存失败: ${e}`, { duration: 2200 });
  } finally {
    saving.value = false;
  }
}
</script>

<template>
  <div class="editor-root">
    <div class="editor-main">
      <!-- 左功能菜单：滤镜 / 调节 / 裁剪 三段切换 -->
      <nav class="editor-rail">
        <button
          v-for="t in TABS"
          :key="t.id"
          class="rail-btn"
          :class="{ active: tab === t.id }"
          @click="tab = t.id"
        >
          <n-icon :component="t.icon" :size="18" />
          <span>{{ t.label }}</span>
        </button>
      </nav>

      <!-- 中间预览区 -->
      <div ref="stageRef" class="editor-stage">
        <div
          v-if="imageUrl && fit.w > 0"
          ref="fitBoxRef"
          class="fit-box"
          :style="{ width: `${fit.w}px`, height: `${fit.h}px` }"
        >
          <img
            class="preview-img"
            :src="imageUrl"
            :style="{ filter: previewFilter }"
            draggable="false"
          />
          <!-- 裁剪覆盖层（仅裁剪页且非「按住查看原图」时显示） -->
          <div
            v-if="tab === 'crop' && !showOriginal"
            class="crop-layer"
          >
            <div
              class="crop-box"
              :style="cropBoxStyle"
              @pointerdown="startMove"
              @pointermove="onDragMove"
              @pointerup="endDrag"
              @pointercancel="endDrag"
            >
              <span class="crop-grid" />
              <span
                v-for="c in CORNERS"
                :key="c"
                class="crop-handle"
                :class="c"
                @pointerdown.stop="startResize($event, c)"
                @pointermove="onDragMove"
                @pointerup="endDrag"
                @pointercancel="endDrag"
              />
            </div>
          </div>
        </div>

        <!-- 状态浮层 -->
        <div v-if="loading" class="stage-state">
          <n-spin :size="18" />
          <span>正在解码…</span>
        </div>
        <div v-else-if="!imageUrl" class="stage-state">
          <span>图片加载失败</span>
        </div>

        <!-- 按住查看原图（原图对比：按住时移除全部滤镜与裁剪） -->
        <button
          v-if="imageUrl"
          class="peek-btn"
          :class="{ holding: showOriginal }"
          title="按住对比原图"
          @pointerdown.prevent="holdPeek"
          @pointerup="releasePeek"
          @pointerleave="releasePeek"
          @pointercancel="releasePeek"
          @contextmenu.prevent
        >
          <n-icon :component="Eye" :size="14" />
          按住查看原图
        </button>
      </div>

      <!-- 右侧参数面板 -->
      <aside class="editor-panel">
        <div class="panel-head">{{ currentTabLabel }}</div>
        <div class="panel-body">
          <!-- 滤镜 Tab：12 预设缩略网格，单选（再次点击取消），缩略图同图 CSS filter 示意 -->
          <template v-if="tab === 'filter'">
            <p class="panel-hint">点击缩略图应用滤镜，再次点击取消</p>
            <div class="preset-grid">
              <button
                v-for="p in FILTER_PRESETS"
                :key="p.id"
                class="preset-item"
                :class="{ active: presetId === p.id }"
                :title="p.name"
                @click="presetId = presetId === p.id ? '' : p.id"
              >
                <span class="preset-thumb">
                  <img :src="imageUrl" :style="{ filter: p.css }" draggable="false" />
                </span>
                <span class="preset-name">{{ p.name }}</span>
                <span v-if="presetId === p.id" class="preset-check">
                  <n-icon :component="Check" :size="11" />
                </span>
              </button>
            </div>
          </template>

          <!-- 调节 Tab：亮度/对比度/饱和度三滑杆 + 每段重置 -->
          <template v-else-if="tab === 'adjust'">
            <p class="panel-hint">微调与滤镜实时叠加生效</p>
            <div v-for="row in ADJ_ROWS" :key="row.key" class="adj-row">
              <div class="adj-head">
                <span class="adj-label">{{ row.label }}</span>
                <span class="adj-val">{{ adjust[row.key] }}</span>
                <button
                  class="adj-reset"
                  title="重置"
                  :disabled="adjust[row.key] === 100"
                  @click="adjust[row.key] = 100"
                >
                  <n-icon :component="Rotate" :size="12" />
                </button>
              </div>
              <n-slider
                v-model:value="adjust[row.key]"
                :min="row.min"
                :max="row.max"
                :step="1"
                :format-tooltip="fmtTooltip"
              />
            </div>
          </template>

          <!-- 裁剪 Tab：比例预设 + 重置 -->
          <template v-else>
            <p class="panel-hint">拖动裁剪框移动 · 拉拽四角手柄调整</p>
            <div class="ratio-row">
              <button
                v-for="r in RATIOS"
                :key="r.label"
                class="ratio-chip"
                :class="{ active: ratio === r.value }"
                @click="applyRatio(r.value)"
              >
                {{ r.label }}
              </button>
            </div>
            <button class="crop-reset" @click="resetCrop">
              <n-icon :component="Rotate" :size="13" />
              重置裁剪
            </button>
          </template>
        </div>

        <!-- 实时参数：当前 CSS filter 串 + 裁剪框像素值 -->
        <div class="live-params">
          <div class="lp-title">实时参数</div>
          <div class="lp-row">
            <span class="lp-label">当前滤镜</span>
            <span class="lp-val">{{ preset ? preset.name : "无" }}</span>
          </div>
          <div class="lp-block">
            <span class="lp-label">CSS filter</span>
            <pre class="lp-css">{{ fullCss || "none" }}</pre>
          </div>
          <div class="lp-row">
            <span class="lp-label">裁剪框</span>
            <span class="lp-val">
              {{
                cropActive
                  ? `${cropPx.x}, ${cropPx.y} · ${cropPx.w}×${cropPx.h} px`
                  : "未裁剪"
              }}
            </span>
          </div>
          <div class="lp-row">
            <span class="lp-label">原始尺寸</span>
            <span class="lp-val">{{ imgW }}×{{ imgH }} px</span>
          </div>
        </div>
      </aside>
    </div>

    <!-- 底部操作条：重置全部 / 取消 / 保存另存为 -->
    <footer class="editor-footer">
      <button class="foot-ghost" @click="resetAll">
        <n-icon :component="Rotate" :size="14" />
        重置全部
      </button>
      <div class="foot-actions">
        <n-button quaternary size="small" @click="emit('close')">
          取消
        </n-button>
        <n-button
          size="small"
          type="primary"
          :loading="saving"
          @click="handleSave"
        >
          保存另存为
        </n-button>
      </div>
    </footer>
  </div>
</template>

<style scoped>
/* === 根布局：由宿主窗口提供满高容器；左菜单 + 中预览 + 右面板 + 底部操作条 === */
.editor-root {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
  overflow: hidden;
  background: var(--jb-bg);
  color: var(--jb-text);
  font-size: 12px;
  user-select: none;
}
.editor-main {
  flex: 1;
  display: flex;
  min-height: 0;
}

/* === 左功能菜单 === */
.editor-rail {
  width: 76px;
  flex-shrink: 0;
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 12px 8px;
  background: var(--jb-bg-card);
  border-right: 1px solid var(--jb-border);
}
.rail-btn {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 4px;
  padding: 10px 0;
  border: 1px solid transparent;
  border-radius: 12px;
  background: transparent;
  color: var(--jb-text-mute);
  font-size: 11px;
  cursor: pointer;
  transition: color 210ms ease, background-color 210ms ease,
    border-color 210ms ease;
}
.rail-btn:hover {
  color: var(--jb-text);
  background: color-mix(in srgb, var(--jb-primary) 6%, transparent);
}
.rail-btn.active {
  color: var(--jb-primary);
  background: color-mix(in srgb, var(--jb-primary) 12%, transparent);
  border-color: color-mix(in srgb, var(--jb-primary) 35%, transparent);
}

/* === 中间预览区 === */
.editor-stage {
  flex: 1;
  position: relative;
  min-width: 0;
  min-height: 0;
  overflow: hidden;
}
/* 图片贴合框：contain 适配后精确居中，裁剪层与图片像素级对齐 */
.fit-box {
  position: absolute;
  left: 50%;
  top: 50%;
  transform: translate(-50%, -50%);
  border-radius: 4px;
  overflow: hidden;
}
.preview-img {
  display: block;
  width: 100%;
  height: 100%;
  object-fit: fill; /* 框已按原图比例计算，填充即无失真 */
  user-select: none;
  -webkit-user-drag: none;
  transition: filter 200ms ease; /* 滤镜切换柔和过渡 */
}

/* === 裁剪覆盖层：框外压暗（box-shadow 大投影技巧）+ 九宫格参考线 + 四角手柄 === */
.crop-layer {
  position: absolute;
  inset: 0;
  overflow: hidden;
  touch-action: none;
  z-index: 2;
}
.crop-box {
  position: absolute;
  border: 1px solid rgba(255, 255, 255, 0.9);
  box-shadow: 0 0 0 9999px rgba(0, 0, 0, 0.45); /* 框外压暗 */
  cursor: move;
  touch-action: none;
}
.crop-grid {
  position: absolute;
  inset: 0;
  pointer-events: none;
  background-image:
    linear-gradient(rgba(255, 255, 255, 0.3) 1px, transparent 1px),
    linear-gradient(90deg, rgba(255, 255, 255, 0.3) 1px, transparent 1px);
  background-size: 33.333% 33.333%;
}
.crop-handle {
  position: absolute;
  width: 12px;
  height: 12px;
  background: #fff;
  border-radius: 3px;
  box-shadow: 0 1px 4px rgba(0, 0, 0, 0.35);
  z-index: 3;
}
.crop-handle.nw {
  left: -7px;
  top: -7px;
  cursor: nwse-resize;
}
.crop-handle.ne {
  right: -7px;
  top: -7px;
  cursor: nesw-resize;
}
.crop-handle.sw {
  left: -7px;
  bottom: -7px;
  cursor: nesw-resize;
}
.crop-handle.se {
  right: -7px;
  bottom: -7px;
  cursor: nwse-resize;
}

/* === 预览区状态浮层 === */
.stage-state {
  position: absolute;
  inset: 0;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 10px;
  color: var(--jb-text-mute);
  font-size: 12px;
  pointer-events: none;
  z-index: 5;
}

/* === 按住查看原图（原图对比） === */
.peek-btn {
  position: absolute;
  left: 50%;
  bottom: 16px;
  transform: translateX(-50%);
  z-index: 6;
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 7px 14px;
  border-radius: 999px;
  border: 1px solid var(--jb-border);
  background: color-mix(in srgb, var(--jb-bg-card) 82%, transparent);
  backdrop-filter: blur(14px) saturate(1.3);
  color: var(--jb-text-soft);
  font-size: 11px;
  letter-spacing: 0.5px;
  cursor: pointer;
  user-select: none;
  touch-action: none;
  transition: color 200ms ease, border-color 200ms ease,
    background-color 200ms ease;
}
.peek-btn:hover {
  color: var(--jb-text);
  border-color: color-mix(in srgb, var(--jb-primary) 40%, transparent);
}
.peek-btn.holding {
  color: var(--jb-primary);
  border-color: var(--jb-primary);
  background: color-mix(in srgb, var(--jb-primary) 12%, transparent);
}

/* === 右侧参数面板 === */
.editor-panel {
  width: 292px;
  flex-shrink: 0;
  display: flex;
  flex-direction: column;
  min-height: 0;
  background: var(--jb-bg-card);
  border-left: 1px solid var(--jb-border);
}
.panel-head {
  flex-shrink: 0;
  padding: 12px 14px;
  font-size: 13px;
  font-weight: 600;
  color: var(--jb-text);
  border-bottom: 1px solid var(--jb-divider);
}
.panel-body {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: 12px 14px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.panel-body::-webkit-scrollbar {
  width: 6px;
}
.panel-body::-webkit-scrollbar-thumb {
  background: var(--jb-border);
  border-radius: 3px;
}
.panel-hint {
  margin: 0;
  font-size: 11px;
  color: var(--jb-text-mute);
}

/* 滤镜网格：3 列缩略，同图 CSS filter 示意 */
.preset-grid {
  display: grid;
  grid-template-columns: repeat(3, 1fr);
  gap: 10px;
}
.preset-item {
  position: relative;
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding: 4px;
  border: 1px solid var(--jb-border);
  border-radius: 12px;
  background: var(--jb-bg);
  cursor: pointer;
  transition: border-color 200ms ease, transform 200ms ease,
    box-shadow 200ms ease;
}
.preset-item:hover {
  transform: translateY(-1px);
  box-shadow: var(--jb-shadow);
}
.preset-item.active {
  border-color: var(--jb-primary);
  box-shadow: 0 0 0 3px color-mix(in srgb, var(--jb-primary) 20%, transparent);
}
.preset-thumb {
  width: 100%;
  aspect-ratio: 1 / 1;
  border-radius: 8px;
  overflow: hidden;
  background: var(--jb-bg);
}
.preset-thumb img {
  display: block;
  width: 100%;
  height: 100%;
  object-fit: cover;
  user-select: none;
  -webkit-user-drag: none;
}
.preset-name {
  font-size: 11px;
  color: var(--jb-text-soft);
  text-align: center;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.preset-item.active .preset-name {
  color: var(--jb-primary);
  font-weight: 500;
}
.preset-check {
  position: absolute;
  right: 6px;
  top: 6px;
  width: 16px;
  height: 16px;
  border-radius: 50%;
  background: var(--jb-primary);
  color: #fff;
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 2;
}

/* 调节滑杆行 */
.adj-row {
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.adj-head {
  display: flex;
  align-items: center;
  gap: 8px;
}
.adj-label {
  font-size: 12px;
  color: var(--jb-text);
}
.adj-val {
  margin-left: auto;
  font-size: 11px;
  color: var(--jb-text-mute);
  font-variant-numeric: tabular-nums;
}
.adj-reset {
  width: 20px;
  height: 20px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  border: 1px solid var(--jb-border);
  border-radius: 50%;
  background: transparent;
  color: var(--jb-text-mute);
  cursor: pointer;
  transition: color 200ms ease, border-color 200ms ease;
}
.adj-reset:hover:not(:disabled) {
  color: var(--jb-primary);
  border-color: var(--jb-primary);
}
.adj-reset:disabled {
  opacity: 0.35;
  cursor: default;
}

/* 裁剪比例预设 */
.ratio-row {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
}
.ratio-chip {
  padding: 5px 12px;
  border-radius: 999px;
  border: 1px solid var(--jb-border);
  background: transparent;
  color: var(--jb-text-soft);
  font-size: 11px;
  cursor: pointer;
  transition: color 200ms ease, border-color 200ms ease,
    background-color 200ms ease;
}
.ratio-chip:hover {
  color: var(--jb-text);
}
.ratio-chip.active {
  color: var(--jb-primary);
  border-color: var(--jb-primary);
  background: color-mix(in srgb, var(--jb-primary) 10%, transparent);
}
.crop-reset {
  align-self: flex-start;
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 5px 12px;
  border-radius: 999px;
  border: 1px dashed var(--jb-border);
  background: transparent;
  color: var(--jb-text-mute);
  font-size: 11px;
  cursor: pointer;
  transition: color 200ms ease, border-color 200ms ease;
}
.crop-reset:hover {
  color: var(--jb-primary);
  border-color: var(--jb-primary);
}

/* 实时参数区（面板底部常驻） */
.live-params {
  flex-shrink: 0;
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 12px 14px;
  border-top: 1px solid var(--jb-divider);
}
.lp-title {
  font-size: 11px;
  font-weight: 600;
  letter-spacing: 1px;
  color: var(--jb-text-mute);
}
.lp-row {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 10px;
}
.lp-label {
  flex-shrink: 0;
  font-size: 11px;
  color: var(--jb-text-mute);
}
.lp-val {
  font-size: 11px;
  color: var(--jb-text);
  text-align: right;
}
.lp-block {
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.lp-css {
  margin: 0;
  padding: 8px 10px;
  background: var(--jb-bg);
  border: 1px solid var(--jb-border);
  border-radius: 8px;
  font-family: Consolas, "Courier New", monospace;
  font-size: 10px;
  line-height: 1.6;
  color: var(--jb-text-soft);
  white-space: pre-wrap;
  word-break: break-all;
  max-height: 96px;
  overflow-y: auto;
}

/* === 底部操作条 === */
.editor-footer {
  flex-shrink: 0;
  height: 52px;
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 0 14px;
  background: var(--jb-bg-card);
  border-top: 1px solid var(--jb-divider);
}
.foot-ghost {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 6px 12px;
  border-radius: 999px;
  border: 1px solid var(--jb-border);
  background: transparent;
  color: var(--jb-text-mute);
  font-size: 11px;
  cursor: pointer;
  transition: color 200ms ease, border-color 200ms ease;
}
.foot-ghost:hover {
  color: var(--jb-primary);
  border-color: var(--jb-primary);
}
.foot-actions {
  display: flex;
  align-items: center;
  gap: 10px;
}
</style>
