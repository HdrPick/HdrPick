<script setup lang="ts">
// 模板化拼图编辑页：左模板选择 + 中间等比 DOM 预览 + 右边框/背景设置 + 底部操作条
// 数据流：props.paths（1-9 张）→ open_image 预取 url（convertFileSrc 转 asset 协议）
//   → new Image 预加载（画布导出用，全部完成前禁用导出）
// 导出：Canvas 合成（先铺背景色 → 每槽 roundRect 裁剪 + cover-crop drawImage）
//   → toBlob → ArrayBuffer → invoke("save_image_blob", { path, data })
// 槽位坐标模型：slots 分数（0-1）× 画布像素；边框粗细 bw 同时作为外衬距与槽间距
//   （每槽四边内缩 bw/2 → 相邻间距 = bw，外沿留白 = bw + bw/2，预览按同公式等比缩放，所见即所得）
import { ref, computed, watch, onMounted, onUnmounted } from "vue";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { open as openFileDialog, save as saveFileDialog } from "@tauri-apps/plugin-dialog";
import { useMessage, NButton, NIcon, NSlider, NSpin, NInputNumber } from "naive-ui";
import { LayoutGrid, Palette, Plus, Refresh, X, Download } from "@vicons/tabler";
import type { CSSProperties } from "vue";

// ==================== Props / Emits ====================
const props = defineProps<{
  /** 参与拼图的图片路径（1-9 张，超过 9 张截断） */
  paths: string[];
}>();

const emit = defineEmits<{
  /** 请求关闭拼图编辑页 */
  (e: "close"): void;
  /** 导出保存成功 */
  (e: "saved"): void;
}>();

const message = useMessage();

// ==================== 内置布局（20+ 参数化模板） ====================
/** 槽位：画布内容区内的归一化矩形（x/y/w/h 均为 0-1 分数） */
interface CgSlot {
  x: number;
  y: number;
  w: number;
  h: number;
}
/** 模板布局 */
interface CgLayout {
  id: string;
  name: string;
  slots: CgSlot[];
}

/** cols×rows 均分网格生成器 */
function grid(cols: number, rows: number): CgSlot[] {
  const out: CgSlot[] = [];
  for (let r = 0; r < rows; r++) {
    for (let c = 0; c < cols; c++) {
      out.push({ x: c / cols, y: r / rows, w: 1 / cols, h: 1 / rows });
    }
  }
  return out;
}

const LAYOUTS: CgLayout[] = [
  // --- 1 张（心形留白简化为全幅 + 圆角） ---
  { id: "p1-full", name: "全幅圆角", slots: [{ x: 0, y: 0, w: 1, h: 1 }] },
  // --- 2 张：横 / 竖 / 主次 ---
  { id: "p2-lr", name: "左右对称", slots: [{ x: 0, y: 0, w: 0.5, h: 1 }, { x: 0.5, y: 0, w: 0.5, h: 1 }] },
  { id: "p2-tb", name: "上下对称", slots: [{ x: 0, y: 0, w: 1, h: 0.5 }, { x: 0, y: 0.5, w: 1, h: 0.5 }] },
  { id: "p2-lmain", name: "主次·左主", slots: [{ x: 0, y: 0, w: 0.62, h: 1 }, { x: 0.62, y: 0, w: 0.38, h: 1 }] },
  { id: "p2-tmain", name: "主次·上主", slots: [{ x: 0, y: 0, w: 1, h: 0.62 }, { x: 0, y: 0.62, w: 1, h: 0.38 }] },
  // --- 3 张：三联 / 主次组合 ---
  { id: "p3-cols", name: "三联竖排", slots: grid(3, 1) },
  { id: "p3-rows", name: "三联横排", slots: grid(1, 3) },
  { id: "p3-lmain", name: "左主+右叠", slots: [{ x: 0, y: 0, w: 0.5, h: 1 }, { x: 0.5, y: 0, w: 0.5, h: 0.5 }, { x: 0.5, y: 0.5, w: 0.5, h: 0.5 }] },
  { id: "p3-rmain", name: "右主+左叠", slots: [{ x: 0, y: 0, w: 0.5, h: 0.5 }, { x: 0, y: 0.5, w: 0.5, h: 0.5 }, { x: 0.5, y: 0, w: 0.5, h: 1 }] },
  { id: "p3-tmain", name: "上主+下叠", slots: [{ x: 0, y: 0, w: 1, h: 0.5 }, { x: 0, y: 0.5, w: 0.5, h: 0.5 }, { x: 0.5, y: 0.5, w: 0.5, h: 0.5 }] },
  // --- 4 张 ---
  { id: "p4-grid", name: "2×2 四宫格", slots: grid(2, 2) },
  { id: "p4-lmain", name: "左主+右列三", slots: [{ x: 0, y: 0, w: 0.5, h: 1 }, { x: 0.5, y: 0, w: 0.5, h: 1 / 3 }, { x: 0.5, y: 1 / 3, w: 0.5, h: 1 / 3 }, { x: 0.5, y: 2 / 3, w: 0.5, h: 1 / 3 }] },
  { id: "p4-rmain", name: "右主+左列三", slots: [{ x: 0, y: 0, w: 0.5, h: 1 / 3 }, { x: 0, y: 1 / 3, w: 0.5, h: 1 / 3 }, { x: 0, y: 2 / 3, w: 0.5, h: 1 / 3 }, { x: 0.5, y: 0, w: 0.5, h: 1 }] },
  { id: "p4-tmain", name: "上主+下行三", slots: [{ x: 0, y: 0, w: 1, h: 0.5 }, { x: 0, y: 0.5, w: 1 / 3, h: 0.5 }, { x: 1 / 3, y: 0.5, w: 1 / 3, h: 0.5 }, { x: 2 / 3, y: 0.5, w: 1 / 3, h: 0.5 }] },
  { id: "p4-cols", name: "四联竖排", slots: grid(4, 1) },
  // --- 5 张 ---
  { id: "p5-lmain", name: "左主+右四宫", slots: [{ x: 0, y: 0, w: 0.5, h: 1 }, { x: 0.5, y: 0, w: 0.25, h: 0.5 }, { x: 0.75, y: 0, w: 0.25, h: 0.5 }, { x: 0.5, y: 0.5, w: 0.25, h: 0.5 }, { x: 0.75, y: 0.5, w: 0.25, h: 0.5 }] },
  { id: "p5-rmain", name: "右主+左四宫", slots: [{ x: 0, y: 0, w: 0.25, h: 0.5 }, { x: 0.25, y: 0, w: 0.25, h: 0.5 }, { x: 0, y: 0.5, w: 0.25, h: 0.5 }, { x: 0.25, y: 0.5, w: 0.25, h: 0.5 }, { x: 0.5, y: 0, w: 0.5, h: 1 }] },
  { id: "p5-tmain", name: "上主+下四宫", slots: [{ x: 0, y: 0, w: 1, h: 0.5 }, { x: 0, y: 0.5, w: 0.5, h: 0.25 }, { x: 0.5, y: 0.5, w: 0.5, h: 0.25 }, { x: 0, y: 0.75, w: 0.5, h: 0.25 }, { x: 0.5, y: 0.75, w: 0.5, h: 0.25 }] },
  // --- 6 张 ---
  { id: "p6-32", name: "六宫格 3×2", slots: grid(3, 2) },
  { id: "p6-23", name: "六宫格 2×3", slots: grid(2, 3) },
  { id: "p6-top2", name: "上二+下四", slots: [{ x: 0, y: 0, w: 0.5, h: 0.5 }, { x: 0.5, y: 0, w: 0.5, h: 0.5 }, { x: 0, y: 0.5, w: 0.5, h: 0.25 }, { x: 0.5, y: 0.5, w: 0.5, h: 0.25 }, { x: 0, y: 0.75, w: 0.5, h: 0.25 }, { x: 0.5, y: 0.75, w: 0.5, h: 0.25 }] },
  // --- 7 张 ---
  { id: "p7-top", name: "上主+下六宫", slots: [{ x: 0, y: 0, w: 1, h: 0.4 }, { x: 0, y: 0.4, w: 1 / 3, h: 0.3 }, { x: 1 / 3, y: 0.4, w: 1 / 3, h: 0.3 }, { x: 2 / 3, y: 0.4, w: 1 / 3, h: 0.3 }, { x: 0, y: 0.7, w: 1 / 3, h: 0.3 }, { x: 1 / 3, y: 0.7, w: 1 / 3, h: 0.3 }, { x: 2 / 3, y: 0.7, w: 1 / 3, h: 0.3 }] },
  { id: "p7-lmain", name: "左主+右六宫", slots: [{ x: 0, y: 0, w: 0.4, h: 1 }, { x: 0.4, y: 0, w: 0.2, h: 0.5 }, { x: 0.6, y: 0, w: 0.2, h: 0.5 }, { x: 0.8, y: 0, w: 0.2, h: 0.5 }, { x: 0.4, y: 0.5, w: 0.2, h: 0.5 }, { x: 0.6, y: 0.5, w: 0.2, h: 0.5 }, { x: 0.8, y: 0.5, w: 0.2, h: 0.5 }] },
  // --- 8 张 ---
  { id: "p8-42", name: "八宫格 4×2", slots: grid(4, 2) },
  { id: "p8-24", name: "八宫格 2×4", slots: grid(2, 4) },
  { id: "p8-top2", name: "上二+下六", slots: [{ x: 0, y: 0, w: 0.5, h: 0.4 }, { x: 0.5, y: 0, w: 0.5, h: 0.4 }, { x: 0, y: 0.4, w: 1 / 3, h: 0.3 }, { x: 1 / 3, y: 0.4, w: 1 / 3, h: 0.3 }, { x: 2 / 3, y: 0.4, w: 1 / 3, h: 0.3 }, { x: 0, y: 0.7, w: 1 / 3, h: 0.3 }, { x: 1 / 3, y: 0.7, w: 1 / 3, h: 0.3 }, { x: 2 / 3, y: 0.7, w: 1 / 3, h: 0.3 }] },
  // --- 9 张 ---
  { id: "p9-grid", name: "九宫格 3×3", slots: grid(3, 3) },
  { id: "p9-lmain", name: "左主+右八宫", slots: [{ x: 0, y: 0, w: 0.4, h: 1 }, { x: 0.4, y: 0, w: 0.3, h: 0.25 }, { x: 0.7, y: 0, w: 0.3, h: 0.25 }, { x: 0.4, y: 0.25, w: 0.3, h: 0.25 }, { x: 0.7, y: 0.25, w: 0.3, h: 0.25 }, { x: 0.4, y: 0.5, w: 0.3, h: 0.25 }, { x: 0.7, y: 0.5, w: 0.3, h: 0.25 }, { x: 0.4, y: 0.75, w: 0.3, h: 0.25 }, { x: 0.7, y: 0.75, w: 0.3, h: 0.25 }] },
];

/** 按图片张数自动挑选默认模板 */
function defaultLayoutFor(n: number): string {
  const prefer: Record<number, string> = {
    1: "p1-full",
    2: "p2-lr",
    3: "p3-lmain",
    4: "p4-grid",
    5: "p5-lmain",
    6: "p6-32",
    7: "p7-top",
    8: "p8-42",
    9: "p9-grid",
  };
  if (prefer[n]) return prefer[n];
  // 兜底：槽位数 ≥ n 的最小模板，否则取最大
  const enough = LAYOUTS.filter((l) => l.slots.length >= n).sort(
    (a, b) => a.slots.length - b.slots.length,
  );
  if (enough.length > 0) return enough[0].id;
  return LAYOUTS[LAYOUTS.length - 1].id;
}

// 模板按张数分组（左栏展示）
const layoutGroups = computed(() => {
  const m = new Map<number, CgLayout[]>();
  for (const l of LAYOUTS) {
    if (!m.has(l.slots.length)) m.set(l.slots.length, []);
    m.get(l.slots.length)!.push(l);
  }
  return [...m.entries()]
    .sort((a, b) => a[0] - b[0])
    .map(([count, items]) => ({ count, items }));
});

// ==================== 画布尺寸预设 ====================
interface SizePreset {
  id: string;
  name: string;
  w: number;
  h: number;
}
const SIZE_PRESETS: SizePreset[] = [
  { id: "xhs", name: "小红书封面 1080×1440", w: 1080, h: 1440 },
  { id: "pyq", name: "朋友圈九图 1080×1080", w: 1080, h: 1080 },
  { id: "square", name: "方形 1080×1080", w: 1080, h: 1080 },
  { id: "r43", name: "4:3 1600×1200", w: 1600, h: 1200 },
  { id: "r169", name: "16:9 1920×1080", w: 1920, h: 1080 },
  { id: "custom", name: "自定义", w: 1080, h: 1080 },
];

// ==================== 柔色背景色板 ====================
const BG_COLORS = [
  { name: "纯白", v: "#ffffff" },
  { name: "奶油", v: "#faf3e8" },
  { name: "樱粉", v: "#fdecf1" },
  { name: "暖灰", v: "#f4efea" },
  { name: "薄荷", v: "#eaf3ec" },
  { name: "雾蓝", v: "#eaf1f8" },
  { name: "淡紫", v: "#f5eff8" },
  { name: "夜墨", v: "#211f26" },
];

// 打开对话框过滤器（与 ViewerWindow 一致）
const IMAGE_FILTERS = [
  {
    name: "图片",
    extensions: [
      "png", "jpg", "jpeg", "gif", "bmp", "webp", "avif",
      "tif", "tiff", "ico", "psd", "jxl",
    ],
  },
];

// ==================== 编辑状态 ====================
/** 本地图片顺序（拖拽交换 / 双击替换都改这里；空槽总在尾部） */
const imgs = ref<string[]>([...props.paths].slice(0, 9));
const layoutId = ref(defaultLayoutFor(imgs.value.length));
const sizeId = ref("xhs");
const canvasWRaw = ref(1080);
const canvasHRaw = ref(1440);
/** 边框粗细（同时作为槽间距 gap，联动） */
const borderW = ref(8);
/** 槽位圆角 */
const radius = ref(8);
/** 背景色 */
const bgColor = ref("#ffffff");
/** 导出中 */
const exporting = ref(false);

const layout = computed(
  () => LAYOUTS.find((l) => l.id === layoutId.value) ?? LAYOUTS[0],
);

/** 自定义输入兜底钳制（n-input-number 可能给出 null/越界值） */
function clampNum(v: number | null, def: number): number {
  const n = typeof v === "number" && isFinite(v) ? Math.round(v) : def;
  return Math.min(4096, Math.max(200, n));
}
const canvasW = computed(() => clampNum(canvasWRaw.value, 1080));
const canvasH = computed(() => clampNum(canvasHRaw.value, 1440));

function pickSize(p: SizePreset) {
  sizeId.value = p.id;
  if (p.id !== "custom") {
    canvasWRaw.value = p.w;
    canvasHRaw.value = p.h;
  }
}

// ==================== 槽位像素几何（预览与导出共用） ====================
interface PixelRect {
  x: number;
  y: number;
  w: number;
  h: number;
}
/**
 * 槽位在 W×H 画布上的像素矩形：
 * 内容区 = 画布内缩 bw（外衬距）→ 槽位按分数落位 → 四边再内缩 bw/2（相邻间距 = bw）
 */
function slotRect(s: CgSlot, W: number, H: number, bw: number): PixelRect {
  const iw = W - bw * 2;
  const ih = H - bw * 2;
  const g = bw / 2;
  return {
    x: bw + s.x * iw + g,
    y: bw + s.y * ih + g,
    w: s.w * iw - g * 2,
    h: s.h * ih - g * 2,
  };
}

// ==================== 图片预取与预加载 ====================
/** path → 展示 url（asset 协议或 blob） */
const imgUrls = ref<Record<string, string>>({});
/** path → 预加载完成的 HTMLImageElement（Canvas 导出用） */
const imgEls = ref<Record<string, HTMLImageElement>>({});
/** 进行中的请求去重 */
const inflight = new Set<string>();

/** Windows 本地路径 → tauri asset URL（与 App.vue toAssetUrl 同规则） */
function toAssetUrl(p: string): string {
  if (!p) return "";
  if (/^https?:\/\//.test(p)) return p;
  if (/^[A-Za-z]:[\\/]/.test(p)) return convertFileSrc(p);
  return p;
}

/** 加载单张：cors 模式保证 Canvas 不被跨域污染（blob URL 同源无需） */
function loadImage(src: string, cors = true): Promise<HTMLImageElement> {
  return new Promise((resolve, reject) => {
    const im = new Image();
    if (cors) im.crossOrigin = "anonymous";
    im.onload = () => resolve(im);
    im.onerror = () => reject(new Error("图片解码失败"));
    im.src = src;
  });
}

/** 预取一张：open_image → url → new Image；失败时回退 plugin-fs 读字节转 blob URL */
async function ensureImage(path: string) {
  if (!path || imgUrls.value[path] || inflight.has(path)) return;
  inflight.add(path);
  try {
    const info = await invoke<{ url: string }>("open_image", { path });
    const url = toAssetUrl(info.url);
    const el = await loadImage(url);
    imgUrls.value = { ...imgUrls.value, [path]: url };
    imgEls.value = { ...imgEls.value, [path]: el };
  } catch {
    // 兜底：直接读文件字节 → blob URL（同源，画布必定干净）
    try {
      const { readFile } = await import("@tauri-apps/plugin-fs");
      const bytes = await readFile(path);
      const blobUrl = URL.createObjectURL(new Blob([bytes]));
      const el = await loadImage(blobUrl, false);
      imgUrls.value = { ...imgUrls.value, [path]: blobUrl };
      imgEls.value = { ...imgEls.value, [path]: el };
    } catch (e2) {
      message.error(`图片加载失败：${nameOf(path)}（${e2}）`, { duration: 2200 });
    }
  } finally {
    inflight.delete(path);
  }
}

// 图片列表变化 → 增量预取（含挂载时首拉）
watch(
  imgs,
  (list) => {
    list.forEach((p) => ensureImage(p));
  },
  { immediate: true, deep: true },
);

/** 全部预加载完成才允许导出 */
const allLoaded = computed(
  () => imgs.value.length > 0 && imgs.value.every((p) => !!imgEls.value[p]),
);

function nameOf(p: string): string {
  return p.split(/[\\/]/).pop() ?? p;
}

// ==================== 中间预览（等比缩放 DOM 渲染） ====================
const stageRef = ref<HTMLElement | null>(null);
const stageW = ref(0);
const stageH = ref(0);
let resizeObserver: ResizeObserver | null = null;

onMounted(() => {
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

/** 预览显示尺寸与缩放比（contain 适配舞台，四周留 24px 呼吸空间） */
const preview = computed(() => {
  const aw = Math.max(stageW.value - 48, 60);
  const ah = Math.max(stageH.value - 48, 60);
  const k = Math.min(aw / canvasW.value, ah / canvasH.value);
  return {
    w: Math.max(Math.floor(canvasW.value * k), 40),
    h: Math.max(Math.floor(canvasH.value * k), 40),
    k,
  };
});

const canvasStyle = computed<CSSProperties>(() => ({
  width: `${preview.value.w}px`,
  height: `${preview.value.h}px`,
  background: bgColor.value,
}));

/** 槽位预览样式（坐标 = 导出像素矩形 × 缩放比，所见即所得） */
function previewSlotStyle(s: CgSlot): CSSProperties {
  const k = preview.value.k;
  const r = slotRect(s, canvasW.value, canvasH.value, borderW.value);
  const w = r.w * k;
  const h = r.h * k;
  const rr = Math.min(radius.value * k, w / 2, h / 2);
  return {
    left: `${r.x * k}px`,
    top: `${r.y * k}px`,
    width: `${w}px`,
    height: `${h}px`,
    borderRadius: `${rr}px`,
  };
}

// ==================== 槽位拖拽换图（HTML5 DnD 交换顺序） ====================
/** 拖拽起点槽位下标 */
const dragFrom = ref(-1);
/** 拖拽悬停槽位下标（高亮） */
const dragOverIdx = ref(-1);

function onSlotDragStart(i: number, e: DragEvent) {
  dragFrom.value = i;
  if (e.dataTransfer) {
    e.dataTransfer.setData("text/plain", String(i));
    e.dataTransfer.effectAllowed = "move";
  }
}
function onSlotDragEnter(i: number) {
  if (dragFrom.value >= 0 && dragFrom.value !== i) dragOverIdx.value = i;
}
function onSlotDrop(i: number) {
  const from = dragFrom.value;
  dragOverIdx.value = -1;
  dragFrom.value = -1;
  if (from < 0 || from === i) return;
  const arr = [...imgs.value];
  if (i < arr.length) {
    // 双方都有图 → 交换顺序
    const t = arr[i];
    arr[i] = arr[from];
    arr[from] = t;
  } else {
    // 拖到空槽 → 移动到该位置（空槽总在尾部，等价追加/挪位）
    const [it] = arr.splice(from, 1);
    arr.splice(i, 0, it);
  }
  imgs.value = arr;
}
function onSlotDragEnd() {
  dragFrom.value = -1;
  dragOverIdx.value = -1;
}

// ==================== 双击槽位替换单张 ====================
async function replaceSlot(i: number) {
  try {
    const sel = await openFileDialog({ multiple: false, filters: IMAGE_FILTERS });
    if (typeof sel !== "string" || !sel) return;
    const arr = [...imgs.value];
    if (i < arr.length) {
      arr[i] = sel; // 替换该槽
    } else if (arr.length < 9) {
      arr.push(sel); // 空槽 → 追加
    } else {
      message.info("最多 9 张图片", { duration: 2200 });
      return;
    }
    imgs.value = arr;
  } catch (e) {
    message.error(`打开对话框失败: ${e}`, { duration: 2200 });
  }
}

// ==================== 重置 ====================
function handleReset() {
  imgs.value = [...props.paths].slice(0, 9);
  layoutId.value = defaultLayoutFor(imgs.value.length);
  sizeId.value = "xhs";
  canvasWRaw.value = 1080;
  canvasHRaw.value = 1440;
  borderW.value = 8;
  radius.value = 8;
  bgColor.value = "#ffffff";
  message.info("已恢复初始设置", { duration: 1800 });
}

// ==================== Canvas 合成导出 ====================
/** 时间戳文件名（拼图_20260824_153012） */
function stampName(): string {
  const d = new Date();
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}_${p(d.getHours())}${p(d.getMinutes())}${p(d.getSeconds())}`;
}

/** arcTo 版圆角矩形路径（兼容性确定） */
function roundRectPath(
  ctx: CanvasRenderingContext2D,
  x: number,
  y: number,
  w: number,
  h: number,
  r: number,
) {
  const rr = Math.max(0, Math.min(r, w / 2, h / 2));
  ctx.beginPath();
  ctx.moveTo(x + rr, y);
  ctx.arcTo(x + w, y, x + w, y + h, rr);
  ctx.arcTo(x + w, y + h, x, y + h, rr);
  ctx.arcTo(x, y + h, x, y, rr);
  ctx.arcTo(x, y, x + w, y, rr);
  ctx.closePath();
}

/** cover 裁剪绘制：等比放大裁中，铺满目标矩形 */
function drawCover(
  ctx: CanvasRenderingContext2D,
  img: HTMLImageElement,
  r: PixelRect,
) {
  const iw = img.naturalWidth;
  const ih = img.naturalHeight;
  if (!iw || !ih) return;
  const k = Math.max(r.w / iw, r.h / ih);
  const sw = r.w / k;
  const sh = r.h / k;
  ctx.drawImage(img, (iw - sw) / 2, (ih - sh) / 2, sw, sh, r.x, r.y, r.w, r.h);
}

async function handleExport() {
  if (!allLoaded.value || exporting.value) return;
  exporting.value = true;
  try {
    // 保存路径（默认名 拼图_时间戳.png）
    const dst = await saveFileDialog({
      filters: [{ name: "PNG 图片", extensions: ["png"] }],
      defaultPath: `拼图_${stampName()}.png`,
    });
    if (!dst) return;

    // Canvas 合成：先铺背景色，再逐槽圆角裁剪 + cover 绘制
    const canvas = document.createElement("canvas");
    canvas.width = canvasW.value;
    canvas.height = canvasH.value;
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("无法创建画布上下文");
    ctx.fillStyle = bgColor.value;
    ctx.fillRect(0, 0, canvas.width, canvas.height);
    const n = Math.min(imgs.value.length, layout.value.slots.length);
    for (let i = 0; i < n; i++) {
      const el = imgEls.value[imgs.value[i]];
      if (!el) continue;
      const r = slotRect(layout.value.slots[i], canvas.width, canvas.height, borderW.value);
      ctx.save();
      roundRectPath(ctx, r.x, r.y, r.w, r.h, radius.value);
      ctx.clip();
      drawCover(ctx, el, r);
      ctx.restore();
    }

    // toBlob → ArrayBuffer → 字节数组交后端落盘
    const blob = await new Promise<Blob | null>((res) =>
      canvas.toBlob(res, "image/png"),
    );
    if (!blob) throw new Error("画布编码失败");
    const buf = await blob.arrayBuffer();
    await invoke("save_image_blob", {
      path: dst,
      data: [...new Uint8Array(buf)],
    });
    message.success(`拼图已保存：${nameOf(dst)}`, { duration: 2200 });
    emit("saved");
  } catch (e) {
    message.error(`导出失败: ${e}`, { duration: 2200 });
  } finally {
    exporting.value = false;
  }
}

/** 模板小示意单元格定位 */
function tplCellStyle(s: CgSlot): CSSProperties {
  return {
    left: `${s.x * 100}%`,
    top: `${s.y * 100}%`,
    width: `${s.w * 100}%`,
    height: `${s.h * 100}%`,
  };
}
</script>

<template>
  <div class="cg-root">
    <!-- 顶栏：标题 + 图片/槽位/尺寸概览 -->
    <header class="cg-header">
      <div class="cg-title">
        <n-icon :component="LayoutGrid" :size="16" class="cg-title-icon" />
        <span>拼图编辑</span>
        <span class="cg-sub">拖拽交换位置 · 双击替换图片</span>
      </div>
      <div class="cg-meta">
        图片 {{ imgs.length }} 张 · 模板 {{ layout.slots.length }} 槽 ·
        {{ canvasW }}×{{ canvasH }}
      </div>
    </header>

    <div class="cg-body">
      <!-- 左：模板选择（按张数分组，每项带 div 网格缩略示意） -->
      <aside class="cg-left">
        <div class="cg-panel-title">
          <n-icon :component="LayoutGrid" :size="13" />
          模板
        </div>
        <div class="cg-templates">
          <template v-for="g in layoutGroups" :key="g.count">
            <div class="cg-group-label">{{ g.count }} 张</div>
            <div class="cg-group-grid">
              <button
                v-for="t in g.items"
                :key="t.id"
                class="cg-tpl"
                :class="{ active: t.id === layoutId }"
                :title="t.name"
                @click="layoutId = t.id"
              >
                <div class="cg-tpl-thumb">
                  <div
                    v-for="(s, si) in t.slots"
                    :key="si"
                    class="cg-tpl-cell"
                    :style="tplCellStyle(s)"
                  />
                </div>
                <span class="cg-tpl-name">{{ t.name }}</span>
              </button>
            </div>
          </template>
        </div>
      </aside>

      <!-- 中：尺寸预设 + 等比缩放预览 -->
      <main class="cg-center">
        <div class="cg-sizes">
          <button
            v-for="p in SIZE_PRESETS"
            :key="p.id"
            class="cg-size-chip"
            :class="{ active: sizeId === p.id }"
            @click="pickSize(p)"
          >
            {{ p.name }}
          </button>
          <div v-if="sizeId === 'custom'" class="cg-custom-size">
            <n-input-number
              v-model:value="canvasWRaw"
              :min="200"
              :max="4096"
              size="small"
              style="width: 96px"
            >
              <template #suffix>宽</template>
            </n-input-number>
            <span class="cg-size-x">×</span>
            <n-input-number
              v-model:value="canvasHRaw"
              :min="200"
              :max="4096"
              size="small"
              style="width: 96px"
            >
              <template #suffix>高</template>
            </n-input-number>
          </div>
        </div>
        <div ref="stageRef" class="cg-stage">
          <div class="cg-canvas" :style="canvasStyle">
            <div
              v-for="(s, i) in layout.slots"
              :key="i"
              class="cg-slot"
              :class="{
                filled: i < imgs.length,
                empty: i >= imgs.length,
                over: dragOverIdx === i,
                dragging: dragFrom === i,
              }"
              :style="previewSlotStyle(s)"
              :draggable="i < imgs.length"
              :title="i < imgs.length ? '拖拽交换 · 双击替换' : '双击添加图片'"
              @dragstart="onSlotDragStart(i, $event)"
              @dragover.prevent
              @dragenter.prevent="onSlotDragEnter(i)"
              @dragleave="dragOverIdx = -1"
              @drop.prevent="onSlotDrop(i)"
              @dragend="onSlotDragEnd"
              @dblclick="replaceSlot(i)"
            >
              <img
                v-if="i < imgs.length && imgUrls[imgs[i]]"
                :src="imgUrls[imgs[i]]"
                class="cg-slot-img"
                draggable="false"
              />
              <div v-else-if="i < imgs.length" class="cg-slot-loading">
                <n-spin :size="14" />
              </div>
              <div v-else class="cg-slot-empty">
                <n-icon :component="Plus" :size="14" />
                <span>双击添加</span>
              </div>
            </div>
          </div>
        </div>
      </main>

      <!-- 右：边框 / 圆角 / 背景设置 -->
      <aside class="cg-right">
        <div class="cg-panel-title">
          <n-icon :component="Palette" :size="13" />
          边框与背景
        </div>

        <div class="cg-section">
          <div class="cg-row">
            <span class="cg-label">边框粗细</span>
            <span class="cg-value">{{ borderW }}px</span>
          </div>
          <n-slider
            v-model:value="borderW"
            :min="0"
            :max="24"
            :step="1"
            :format-tooltip="(v: number) => `${v}px`"
          />
          <div class="cg-hint">模板间距与边框粗细联动（gap = 边框）</div>
        </div>

        <div class="cg-section">
          <div class="cg-row">
            <span class="cg-label">槽位圆角</span>
            <span class="cg-value">{{ radius }}px</span>
          </div>
          <n-slider
            v-model:value="radius"
            :min="0"
            :max="32"
            :step="1"
            :format-tooltip="(v: number) => `${v}px`"
          />
        </div>

        <div class="cg-section">
          <div class="cg-row">
            <span class="cg-label">背景色</span>
            <span class="cg-value">{{ bgColor.toUpperCase() }}</span>
          </div>
          <div class="cg-swatches">
            <button
              v-for="c in BG_COLORS"
              :key="c.v"
              class="cg-swatch"
              :class="{ active: bgColor.toLowerCase() === c.v }"
              :style="{ background: c.v }"
              :title="c.name"
              @click="bgColor = c.v"
            />
          </div>
          <label class="cg-custom-color" title="自定义颜色">
            <input
              type="color"
              :value="bgColor"
              @input="bgColor = ($event.target as HTMLInputElement).value"
            />
            <span>自定义颜色</span>
          </label>
        </div>
      </aside>
    </div>

    <!-- 底部操作条：状态 + 重置 / 关闭 / 导出保存 -->
    <footer class="cg-footer">
      <div class="cg-status">
        <template v-if="exporting">
          <n-spin :size="12" />
          <span>正在合成导出…</span>
        </template>
        <template v-else-if="!allLoaded">
          <n-spin :size="12" />
          <span>图片加载中，完成后可导出…</span>
        </template>
        <template v-else>
          <span>就绪 · 导出尺寸 {{ canvasW }}×{{ canvasH }} PNG</span>
        </template>
      </div>
      <div class="cg-actions">
        <n-button size="small" quaternary @click="handleReset">
          <template #icon><n-icon :component="Refresh" /></template>
          重置
        </n-button>
        <n-button size="small" quaternary @click="emit('close')">
          <template #icon><n-icon :component="X" /></template>
          关闭
        </n-button>
        <n-button
          size="small"
          type="primary"
          :disabled="!allLoaded"
          :loading="exporting"
          @click="handleExport"
        >
          <template #icon><n-icon :component="Download" /></template>
          导出保存
        </n-button>
      </div>
    </footer>
  </div>
</template>

<style scoped>
/* 根：三栏主体 + 顶栏 + 底部操作条 */
.cg-root {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
  overflow: hidden;
  background: var(--jb-bg);
  color: var(--jb-text);
  user-select: none;
}

/* === 顶栏 === */
.cg-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 10px 16px;
  border-bottom: 1px solid var(--jb-border);
  flex-shrink: 0;
}
.cg-title {
  display: flex;
  align-items: center;
  gap: 7px;
  font-size: 13px;
  font-weight: 600;
}
.cg-title-icon {
  color: var(--jb-primary);
}
.cg-sub {
  font-size: 11px;
  font-weight: 400;
  color: var(--jb-text-mute);
  margin-left: 4px;
}
.cg-meta {
  font-size: 11px;
  color: var(--jb-text-mute);
  letter-spacing: 0.4px;
}

/* === 三栏主体 === */
.cg-body {
  flex: 1;
  display: flex;
  min-height: 0;
}

/* 左栏：模板列表 */
.cg-left {
  width: 196px;
  flex-shrink: 0;
  display: flex;
  flex-direction: column;
  border-right: 1px solid var(--jb-border);
  background: var(--jb-bg-card);
  min-height: 0;
}
.cg-panel-title {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 10px 12px 8px;
  font-size: 12px;
  font-weight: 600;
  color: var(--jb-text-soft);
  flex-shrink: 0;
}
.cg-templates {
  flex: 1;
  overflow-y: auto;
  padding: 0 10px 12px;
  min-height: 0;
}
.cg-templates::-webkit-scrollbar {
  width: 5px;
}
.cg-templates::-webkit-scrollbar-thumb {
  background: var(--jb-scrollbar);
  border-radius: 3px;
}
.cg-group-label {
  font-size: 10px;
  color: var(--jb-text-mute);
  padding: 8px 2px 5px;
  letter-spacing: 1px;
}
.cg-group-grid {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 7px;
}
.cg-tpl {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 5px;
  padding: 7px 5px 6px;
  border: 1px solid var(--jb-border);
  border-radius: 12px;
  background: var(--jb-bg);
  cursor: pointer;
  transition: border-color 200ms ease, box-shadow 200ms ease, transform 200ms ease;
}
.cg-tpl:hover {
  transform: translateY(-1px);
  box-shadow: var(--jb-shadow);
}
.cg-tpl.active {
  border-color: var(--jb-primary);
  box-shadow: 0 0 0 3px color-mix(in srgb, var(--jb-primary) 20%, transparent);
}
/* 模板小示意：div 网格缩略 */
.cg-tpl-thumb {
  position: relative;
  width: 54px;
  height: 54px;
  border-radius: 6px;
  background: var(--jb-bg);
  border: 1px solid var(--jb-divider);
  overflow: hidden;
}
.cg-tpl-cell {
  position: absolute;
  background: color-mix(in srgb, var(--jb-primary) 30%, var(--jb-bg-card));
  border-radius: 2.5px;
}
.cg-tpl.active .cg-tpl-cell {
  background: color-mix(in srgb, var(--jb-primary) 48%, var(--jb-bg-card));
}
.cg-tpl-name {
  font-size: 10px;
  color: var(--jb-text-soft);
  max-width: 100%;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

/* 中栏：尺寸 + 预览舞台 */
.cg-center {
  flex: 1;
  display: flex;
  flex-direction: column;
  min-width: 0;
  min-height: 0;
}
.cg-sizes {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 7px;
  padding: 10px 14px;
  border-bottom: 1px solid var(--jb-divider);
  flex-shrink: 0;
}
.cg-size-chip {
  padding: 4px 11px;
  border: 1px solid var(--jb-border);
  border-radius: 999px;
  background: var(--jb-bg-card);
  color: var(--jb-text-soft);
  font-size: 11px;
  cursor: pointer;
  transition: color 200ms ease, border-color 200ms ease, background 200ms ease;
}
.cg-size-chip:hover {
  color: var(--jb-primary);
  border-color: var(--jb-primary);
}
.cg-size-chip.active {
  color: #fff;
  background: var(--jb-primary);
  border-color: var(--jb-primary);
}
.cg-custom-size {
  display: flex;
  align-items: center;
  gap: 6px;
}
.cg-size-x {
  color: var(--jb-text-mute);
  font-size: 11px;
}
.cg-stage {
  flex: 1;
  min-height: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  overflow: hidden;
  padding: 12px;
}
/* 画布预览（等比缩放，所见即所得） */
.cg-canvas {
  position: relative;
  border: 1px solid var(--jb-border);
  border-radius: 12px;
  box-shadow: var(--jb-shadow);
  flex-shrink: 0;
}
.cg-slot {
  position: absolute;
  overflow: hidden;
  background: color-mix(in srgb, var(--jb-bg-card) 82%, transparent);
  transition: box-shadow 200ms ease, opacity 200ms ease;
}
.cg-slot.filled {
  cursor: grab;
}
.cg-slot.filled:active {
  cursor: grabbing;
}
.cg-slot.dragging {
  opacity: 0.45;
}
.cg-slot.over {
  box-shadow: 0 0 0 2.5px var(--jb-primary);
}
.cg-slot.empty {
  border: 1.5px dashed var(--jb-border);
  background: transparent;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 3px;
  cursor: pointer;
}
.cg-slot.empty:hover {
  border-color: var(--jb-primary);
}
.cg-slot-img {
  width: 100%;
  height: 100%;
  object-fit: cover;
  display: block;
  pointer-events: none;
}
.cg-slot-loading,
.cg-slot-empty {
  position: absolute;
  inset: 0;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 3px;
  color: var(--jb-text-mute);
  font-size: 10px;
}

/* 右栏：设置面板 */
.cg-right {
  width: 212px;
  flex-shrink: 0;
  border-left: 1px solid var(--jb-border);
  background: var(--jb-bg-card);
  padding-bottom: 12px;
  overflow-y: auto;
}
.cg-right::-webkit-scrollbar {
  width: 5px;
}
.cg-right::-webkit-scrollbar-thumb {
  background: var(--jb-scrollbar);
  border-radius: 3px;
}
.cg-section {
  padding: 6px 14px 14px;
  border-bottom: 1px solid var(--jb-divider);
}
.cg-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 6px 0 8px;
}
.cg-label {
  font-size: 12px;
  color: var(--jb-text);
  font-weight: 500;
}
.cg-value {
  font-size: 11px;
  color: var(--jb-primary);
  font-variant-numeric: tabular-nums;
}
.cg-hint {
  margin-top: 7px;
  font-size: 10px;
  color: var(--jb-text-mute);
}
.cg-swatches {
  display: grid;
  grid-template-columns: repeat(4, 1fr);
  gap: 8px;
}
.cg-swatch {
  width: 32px;
  height: 32px;
  border-radius: 10px;
  border: 1px solid var(--jb-border);
  cursor: pointer;
  padding: 0;
  transition: transform 200ms ease, box-shadow 200ms ease;
}
.cg-swatch:hover {
  transform: scale(1.06);
}
.cg-swatch.active {
  box-shadow: 0 0 0 2.5px var(--jb-primary);
}
.cg-custom-color {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 11px;
  cursor: pointer;
  font-size: 11px;
  color: var(--jb-text-soft);
}
.cg-custom-color input[type="color"] {
  width: 32px;
  height: 24px;
  padding: 0;
  border: 1px solid var(--jb-border);
  border-radius: 8px;
  background: transparent;
  cursor: pointer;
}

/* === 底部操作条 === */
.cg-footer {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 9px 16px;
  border-top: 1px solid var(--jb-border);
  background: var(--jb-bg-card);
  flex-shrink: 0;
}
.cg-status {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 11px;
  color: var(--jb-text-mute);
  min-width: 0;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.cg-actions {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-shrink: 0;
}
</style>
