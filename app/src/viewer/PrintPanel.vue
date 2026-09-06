<script setup lang="ts">
// 打印面板（图片 + PDF）：打印机/纸张/方向/份数/缩放 + 模式（单页/一张多页/小册子）
//   + 页面顺序 + 双面翻转 + 打印边框 + PDF DPI + 批量待打印队列
// 数据流：list_printers / printer_papers 枚举 → print_files 批量提交
//   （PDF 由后端 WinRT 渲染逐页打印；图片走全格式解码管线）
// 队列支持：单文件多选添加 / 文件夹递归添加（scan_printable_files），
// 进度经 print://progress 事件实时推送（当前第几项 / 成功 / 失败原因）
import { ref, computed, watch, onMounted, onUnmounted } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
import {
  useMessage, NModal, NSelect, NInput, NInputNumber, NButton, NIcon, NRadioGroup, NRadioButton,
  NSpin, NCheckbox,
} from "naive-ui";
import { Printer, FilePlus, FolderPlus, Trash, X, Refresh, Adjustments } from "@vicons/tabler";

const props = defineProps<{
  /** 面板显示态（父组件控制，关闭时 emit close 同步） */
  visible: boolean;
  /** 打开时预置的路径（工具栏打印当前图）；队列会话内保留，追加不覆盖 */
  initialPaths: string[];
}>();

const emit = defineEmits<{
  (e: "close"): void;
}>();

const message = useMessage();

// === 后端数据结构（与 Rust 端 serde 命名对齐） ===
interface PrinterInfo { name: string; port: string; isDefault: boolean }
interface PaperInfo { id: number; name: string; widthMm: number; heightMm: number }
interface PrintFileResult { path: string; ok: boolean; pages: number; error: string | null }
interface PrintProgress {
  index: number; total: number; path: string;
  phase: "printing" | "done" | "error"; pages: number; error: string | null;
}
/** 预览物理页（print_preview 输出） */
interface PreviewPage { pngBase64: string; width: number; height: number }
/** 自由排版元素（画布拖拽产物；归一化坐标） */
interface FreeItemL {
  id: number; path: string; pageIndex: number;
  x: number; y: number; w: number; h: number;
}
/** 画布缩略图（free_thumbs 输出） */
interface FreeThumbL { width: number; height: number; pngBase64: string }
interface FileThumbsL { path: string; thumbs: FreeThumbL[] }
/** 排版模板（print_templates_* 输出） */
interface PrintTemplateL { name: string; aspect: number; items: Omit<FreeItemL, "id">[] }

// === 浮层显示态 ===
const show = ref(props.visible);
watch(() => props.visible, (v) => { if (v !== show.value) show.value = v; }, { immediate: true });
watch(show, (v) => { if (!v) emit("close"); });

// === 打印机 / 纸张 ===
const printers = ref<PrinterInfo[]>([]);
const printer = ref("");           // 选中打印机名
const papers = ref<PaperInfo[]>([]);
const paperId = ref(0);           // 0 = 打印机默认纸张
const orientation = ref<0 | 1 | 2>(1); // 0 自动 / 1 纵向 / 2 横向（需求文档2"方向：自动"）
const copies = ref(1);
const fit = ref<"contain" | "fill" | "actual">("contain");
// 四边独立页边距（需求文档1 页面设置：自定义上下左右）
const marginTopMm = ref(5);
const marginBottomMm = ref(5);
const marginLeftMm = ref(5);
const marginRightMm = ref(5);
/** 四边同步开关：勾选时改任意一边 → 其余三边同步（兼顾"统一边距"旧习惯） */
const uniformMargin = ref(true);
const loadingPrinters = ref(false);
const propsBusy = ref(false);      // 打印机属性对话框打开中

// === 模式 / 布局（需求文档 4/5 节 + 需求文档2 发票 Tab4） ===
const mode = ref<"single" | "nup" | "booklet" | "invoice" | "free">("single"); // 单页/一张多页/小册子/发票/自由排版
const nupAuto = ref(false);     // 自动网格：按总页数定列×行（0/0 传后端），4 页 → 2×2
const nupCols = ref(2);          // 一张多页：列
const nupRows = ref(1);          // 一张多页：行
const pageOrder = ref<"ltr-tb" | "rtl-tb" | "tb-ltr" | "tb-rtl">("ltr-tb"); // 网格页序
const duplex = ref<"off" | "long" | "short">("off"); // 双面翻转（小册子模式后端强制短边翻）
const drawBorder = ref(false);   // N-Up / 小册子分格边框
const pdfDpi = ref(200);         // PDF 渲染 DPI（72–600）

// === 打印范围（需求文档2：页码区间 / 奇偶 / 逆序） ===
const pageRange = ref("");       // "1-3,5"（空 = 全部）
const oddEven = ref<"all" | "odd" | "even">("all");
const reverse = ref(false);      // 逆序打印

// === 输出选项（需求文档2 一节：灰度 / 逐份 / 合并） ===
const grayscale = ref(false);   // 灰度打印
const collate = ref(false);      // 逐份打印（多份时）
const mergePrint = ref(false);  // 合并打印：多文件一个任务

// === 布局选项（需求文档1 4.4：自动居中 / 自动旋转） ===
const autoCenter = ref(true);
const autoRotate = ref(true);

// === 小册子（需求文档2 Tab3：子集 / 装订方向） ===
const bookletSubset = ref<"both" | "odd" | "even">("both"); // 双面 / 仅奇数面 / 仅偶数面
const bookletBind = ref<"left" | "right">("left");          // 左装订 / 右装订

// === 发票模式（需求文档2 Tab4） ===
const invoiceLayout = ref<"single" | "two" | "four">("two"); // 单张 / 1纸2张 / 1纸4张
const cutLine = ref(true);       // 虚线裁剪线
const dupOnSheet = ref(false);   // 同一发票在一张纸重复 2 次

// === 自由排版（画布拖拽 + 模板） ===
const freeItems = ref<FreeItemL[]>([]);
const selectedFreeId = ref(0);
let nextFreeId = 1;
const thumbsMap = ref<Map<string, FreeThumbL[]>>(new Map());
const paperEl = ref<HTMLElement | null>(null);
/** 拖拽上下文（move / resize） */
let dragCtx: {
  kind: "move" | "resize"; handle: string;
  it: FreeItemL; startX: number; startY: number;
  ox: number; oy: number; ow: number; oh: number; ax: number; ay: number;
} | null = null;
/** 模板 */
const templates = ref<PrintTemplateL[]>([]);
const selectedTpl = ref<string | null>(null);
const tplName = ref("");

// === 打印预览（右栏实时面板） ===
const previewing = ref(false);
const previewPages = ref<PreviewPage[]>([]);
const previewIdx = ref(0);
/** 预览选中的队列索引（点击队列行切换预览目标；默认第一项） */
const previewSelIdx = ref(0);
/** 当前预览目标路径（合并打印时预览全部，仍以选中项为标题锚点） */
const previewTarget = computed(
  () => queue.value[previewSelIdx.value] ?? queue.value[0] ?? "",
);

const dpiOptions = [
  { label: "150 DPI（快速）", value: 150 },
  { label: "200 DPI（推荐）", value: 200 },
  { label: "300 DPI（高清）", value: 300 },
  { label: "600 DPI（印刷）", value: 600 },
];

// 小册子：横向纸 + 双面短边翻（对折装订），后端会强制纠正；前端同步显示
watch(mode, (m) => {
  if (m === "booklet") orientation.value = 2;
});

const printerOptions = computed(() =>
  printers.value.map((p) => ({
    label: p.isDefault ? `${p.name}（默认）` : p.name,
    value: p.name,
  })),
);
const paperOptions = computed(() => [
  { label: "打印机默认", value: 0 },
  ...papers.value.map((p) => ({
    label: `${p.name}（${Math.round(p.widthMm)}×${Math.round(p.heightMm)}mm）`,
    value: p.id,
  })),
]);

async function loadPrinters() {
  loadingPrinters.value = true;
  try {
    const list = await invoke<PrinterInfo[]>("list_printers");
    printers.value = list ?? [];
    if (printers.value.length === 0) {
      message.warning("未发现系统打印机，请先安装打印机驱动", { duration: 3200 });
      return;
    }
    // 保持已选有效，否则选默认 → 第一个
    const cur = printers.value.find((p) => p.name === printer.value);
    if (!cur) {
      const def = printers.value.find((p) => p.isDefault) ?? printers.value[0];
      printer.value = def.name;
    }
    await loadPapers();
  } catch (e) {
    message.error(`读取打印机失败: ${e}`, { duration: 2200 });
  } finally {
    loadingPrinters.value = false;
  }
}

async function loadPapers() {
  if (!printer.value) {
    papers.value = [];
    return;
  }
  try {
    papers.value = await invoke<PaperInfo[]>("printer_papers", { printer: printer.value });
  } catch {
    papers.value = []; // 某些驱动不支持纸张枚举：仅保留"打印机默认"
  }
  if (!papers.value.some((p) => p.id === paperId.value)) paperId.value = 0;
}

watch(printer, () => { paperId.value = 0; loadPapers(); });

// === 待打印队列 ===
const queue = ref<string[]>([]);
const printing = ref(false);
const progress = ref<PrintProgress | null>(null);
const failures = ref<PrintProgress[]>([]); // 本次任务的失败项

/** 打开时用预置路径重置队列（新打印意图 = 新队列）。
 *  此前是「追加不覆盖」：上次的队列残留导致单选一个文件却把
 *  之前多个文件一并打印 / PDF 合并导出（根因修复） */
watch(
  () => props.visible,
  (v) => {
    if (v) {
      if (props.initialPaths.length > 0) resetQueue(props.initialPaths);
      // 按本次预置数量复位合并打印（>1 自动勾选；单文件绝不残留合并态）
      mergePrint.value = props.initialPaths.length > 1;
      if (printers.value.length === 0) loadPrinters();
    }
  },
  { immediate: true },
);

/** 重置队列为给定路径（去重保序），预览选中复位到第一项 */
function resetQueue(paths: string[]) {
  const seen = new Set<string>();
  const list: string[] = [];
  for (const p of paths) {
    const k = p.toLowerCase();
    if (!seen.has(k)) {
      seen.add(k);
      list.push(p);
    }
  }
  queue.value = list;
  previewSelIdx.value = 0;
  previewIdx.value = 0;
}

function addPaths(paths: string[]) {
  const known = new Set(queue.value.map((p) => p.toLowerCase()));
  for (const p of paths) {
    if (!known.has(p.toLowerCase())) {
      queue.value.push(p);
      known.add(p.toLowerCase());
    }
  }
}

function removeAt(i: number) {
  if (printing.value) return;
  queue.value.splice(i, 1);
  // 维护预览选中索引：删中前项左移，越界夹紧
  if (i < previewSelIdx.value) previewSelIdx.value--;
  if (previewSelIdx.value >= queue.value.length) previewSelIdx.value = queue.value.length - 1;
  if (previewSelIdx.value < 0) previewSelIdx.value = 0;
}

function clearQueue() {
  if (printing.value) return;
  queue.value = [];
  previewSelIdx.value = 0;
  previewIdx.value = 0;
}

function baseName(p: string): string {
  return p.split(/[\\/]/).pop() ?? p;
}

/** 文件类型角标（列表行首小标签） */
function kindOf(p: string): string {
  const ext = (p.split(".").pop() ?? "").toLowerCase();
  return ext === "pdf" ? "PDF" : ext.toUpperCase().slice(0, 4);
}

async function addFiles() {
  try {
    const sel = await openFileDialog({
      multiple: true,
      filters: [
        {
          name: "图片与 PDF",
          extensions: [
            "png", "jpg", "jpeg", "gif", "bmp", "webp", "avif",
            "tif", "tiff", "ico", "psd", "exr", "jxr", "wdp", "jxl", "pdf",
          ],
        },
      ],
    });
    if (Array.isArray(sel)) addPaths(sel);
    else if (typeof sel === "string" && sel) addPaths([sel]);
  } catch {
    // 用户取消等静默
  }
}

async function addFolder() {
  try {
    const sel = await openFileDialog({ directory: true, multiple: false });
    if (typeof sel !== "string" || !sel) return;
    const files = await invoke<string[]>("scan_printable_files", { dir: sel });
    if (files.length === 0) {
      message.info("该文件夹下没有可打印的图片或 PDF", { duration: 2200 });
      return;
    }
    addPaths(files);
    message.success(`已添加 ${files.length} 个文件（含子文件夹）`, { duration: 2000 });
  } catch (e) {
    message.error(`扫描文件夹失败: ${e}`, { duration: 2200 });
  }
}

/** 队列中含 PDF：DPI 选择仅对 PDF 生效 */
const hasPdf = computed(() => queue.value.some((p) => p.toLowerCase().endsWith(".pdf")));

/** 画布纸张比例（w/h）：优先后端预览结果（与打印一致）→ 纸张规格 → A4 */
const canvasAspect = computed(() => {
  const p = previewPages.value[0];
  if (p && p.width > 0) return p.width / p.height;
  const pa = papers.value.find((x) => x.id === paperId.value);
  let w = pa?.widthMm ?? 210;
  let h = pa?.heightMm ?? 297;
  if (orientation.value === 2) [w, h] = [h, w];
  return w / h;
});

/** 元素缩略图（无则空串 → 占位框显示文件名） */
function thumbOf(it: FreeItemL): string {
  const t = thumbsMap.value.get(it.path)?.[it.pageIndex];
  return t ? `data:image/png;base64,${t.pngBase64}` : "";
}

/** 拉取缺失缩略图（图片=1 张；PDF=每页 1 张） */
const wrapEl = ref<HTMLElement | null>(null);
const wrapSize = ref({ w: 0, h: 0 });
let canvasRO: ResizeObserver | null = null;
onMounted(() => {
  canvasRO = new ResizeObserver((entries) => {
    for (const e of entries) {
      wrapSize.value = { w: e.contentRect.width, h: e.contentRect.height };
    }
  });
});
// 画布 v-if 挂载/卸载时重挂 observer
watch(wrapEl, (el) => {
  if (!canvasRO) return;
  canvasRO.disconnect();
  if (el) canvasRO.observe(el);
});
onUnmounted(() => canvasRO?.disconnect());

/** 画布纸张像素尺寸（容器内适配纸张比例；JS 计算，避免 aspect-ratio 与 max 约束冲突） */
const paperStyle = computed(() => {
  const asp = canvasAspect.value;
  const { w, h } = wrapSize.value;
  if (!w || !h || asp <= 0) return { width: "0px", height: "0px" };
  let pw = w;
  let ph = w / asp;
  if (ph > h) {
    ph = h;
    pw = h * asp;
  }
  return { width: `${Math.floor(pw)}px`, height: `${Math.floor(ph)}px` };
});

async function ensureThumbs(): Promise<void> {
  const missing = queue.value.filter((p) => !thumbsMap.value.has(p));
  if (missing.length === 0) return;
  try {
    const list = await invoke<FileThumbsL[]>("free_thumbs", { paths: missing });
    const m = new Map(thumbsMap.value);
    for (const f of list) m.set(f.path, f.thumbs);
    thumbsMap.value = m;
  } catch (e) {
    message.error(`读取缩略图失败: ${e}`, { duration: 2600 });
  }
}

/** 添加队列全部文件到画布并自动网格排布（首次进入 free 的默认布局） */
async function addAllToCanvas() {
  if (queue.value.length === 0) {
    message.info("请先在左侧添加图片或 PDF", { duration: 2200 });
    return;
  }
  await ensureThumbs();
  const asp = canvasAspect.value;
  // 候选页：每文件每页（图片 1 项，PDF N 项）
  const cands: { path: string; pageIndex: number; a: number }[] = [];
  for (const p of queue.value) {
    const ts = thumbsMap.value.get(p);
    if (ts && ts.length > 0) {
      ts.forEach((t, i) => cands.push({ path: p, pageIndex: i, a: t.width / t.height }));
    } else {
      cands.push({ path: p, pageIndex: 0, a: 3 / 4 }); // 解码失败占位
    }
  }
  const n = cands.length;
  if (n === 0) return;
  const cols = Math.ceil(Math.sqrt(n));
  const rows = Math.ceil(n / cols);
  const m = 0.04; // 画布内边距（归一化）
  const cw = (1 - 2 * m) / cols;
  const ch = (1 - 2 * m) / rows;
  freeItems.value = cands.map((c, i) => {
    const col = i % cols;
    const row = Math.floor(i / cols);
    const cx = m + col * cw;
    const cy = m + row * ch;
    // 物理比例约束：(w·W)/(h·H) = a → h = w·asp/a
    let w = cw * 0.94;
    let h = (w * asp) / c.a;
    if (h > ch * 0.94) {
      h = ch * 0.94;
      w = (h * c.a) / asp;
    }
    return {
      id: nextFreeId++,
      path: c.path,
      pageIndex: c.pageIndex,
      x: cx + (cw - w) / 2,
      y: cy + (ch - h) / 2,
      w,
      h,
    };
  });
  selectedFreeId.value = 0;
}

function removeFreeItem(id: number) {
  freeItems.value = freeItems.value.filter((it) => it.id !== id);
  if (selectedFreeId.value === id) selectedFreeId.value = 0;
}

function clearCanvas() {
  freeItems.value = [];
  selectedFreeId.value = 0;
}

/** 归一化 → 画布像素（拖拽换算） */
function ptToNorm(e: PointerEvent): { nx: number; ny: number } {
  const el = paperEl.value;
  if (!el) return { nx: 0, ny: 0 };
  const r = el.getBoundingClientRect();
  return {
    nx: Math.min(1, Math.max(0, (e.clientX - r.left) / r.width)),
    ny: Math.min(1, Math.max(0, (e.clientY - r.top) / r.height)),
  };
}

/** 元素拖动（pointerdown 在元素上；capture 到元素自身） */
function onItemDown(e: PointerEvent, it: FreeItemL) {
  selectedFreeId.value = it.id;
  (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
  dragCtx = {
    kind: "move", handle: "", it,
    startX: e.clientX, startY: e.clientY,
    ox: it.x, oy: it.y, ow: it.w, oh: it.h, ax: 0, ay: 0,
  };
}

/** 缩放柄拖动（四角；以对角为锚，保持图片比例） */
function onHandleDown(e: PointerEvent, it: FreeItemL, handle: string) {
  selectedFreeId.value = it.id;
  (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
  // 对角锚点（归一化）
  const ax = handle.includes("w") ? it.x + it.w : it.x;
  const ay = handle.includes("n") ? it.y + it.h : it.y;
  dragCtx = {
    kind: "resize", handle, it,
    startX: e.clientX, startY: e.clientY,
    ox: it.x, oy: it.y, ow: it.w, oh: it.h, ax, ay,
  };
}

function onDragMove(e: PointerEvent) {
  if (!dragCtx) return;
  const { nx } = ptToNorm(e);
  const el = paperEl.value;
  if (!el) return;
  if (dragCtx.kind === "move") {
    const r = el.getBoundingClientRect();
    const dx = (e.clientX - dragCtx.startX) / r.width;
    const dy = (e.clientY - dragCtx.startY) / r.height;
    const it = dragCtx.it;
    it.x = Math.min(1 - it.w, Math.max(0, dragCtx.ox + dx));
    it.y = Math.min(1 - it.h, Math.max(0, dragCtx.oy + dy));
  } else {
    // resize：光标到锚点的距离决定新尺寸（保持图片比例），对角锚定
    const it = dragCtx.it;
    const wRaw = Math.abs(nx - dragCtx.ax);
    // 图片比例：h = w·asp/a（a = 缩略图像素比；画布 asp = W/H）
    const ts = thumbsMap.value.get(it.path)?.[it.pageIndex];
    const a = ts && ts.width > 0 ? ts.width / ts.height : 3 / 4;
    const asp = canvasAspect.value;
    let w = Math.max(0.03, wRaw);
    let h = (w * asp) / a;
    if (h > 1) {
      h = 1;
      w = (h * a) / asp;
    }
    const x = dragCtx.handle.includes("e") ? dragCtx.ax : Math.max(0, dragCtx.ax - w);
    // y 对齐锚边
    const y = dragCtx.handle.includes("n") ? Math.max(0, dragCtx.ay - h) : dragCtx.ay;
    it.x = Math.min(1 - w, Math.max(0, x));
    it.y = Math.min(1 - h, Math.max(0, y));
    it.w = w;
    it.h = h;
  }
}

function onDragEnd() {
  dragCtx = null;
}

/** Delete 键删除选中元素（输入框聚焦时忽略） */
function onFreeKeydown(e: KeyboardEvent) {
  if (mode.value !== "free" || !selectedFreeId.value) return;
  const t = e.target as HTMLElement;
  if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable)) return;
  if (e.key === "Delete" || e.key === "Backspace") {
    removeFreeItem(selectedFreeId.value);
    e.preventDefault();
  }
}

/** 模板：列表 / 保存 / 删除 / 加载 */
async function refreshTemplates() {
  try {
    templates.value = await invoke<PrintTemplateL[]>("print_templates_list");
  } catch {
    templates.value = [];
  }
}

async function saveTemplate() {
  if (freeItems.value.length === 0) {
    message.info("画布为空：先添加元素再保存模板", { duration: 2200 });
    return;
  }
  const name = tplName.value.trim() || `模板 ${templates.value.length + 1}`;
  const payload = {
    name,
    aspect: canvasAspect.value,
    items: freeItems.value.map(({ id, ...rest }) => {
      void id;
      return rest;
    }),
  };
  try {
    await invoke("print_templates_save", { template: payload });
    tplName.value = "";
    await refreshTemplates();
    selectedTpl.value = name;
    message.success(`模板「${name}」已保存`, { duration: 2000 });
  } catch (e) {
    message.error(`保存模板失败: ${e}`, { duration: 2600 });
  }
}

async function deleteTemplate() {
  const name = selectedTpl.value;
  if (!name) return;
  try {
    await invoke("print_templates_delete", { name });
    await refreshTemplates();
    selectedTpl.value = null;
    message.success(`模板「${name}」已删除`, { duration: 1800 });
  } catch (e) {
    message.error(`删除模板失败: ${e}`, { duration: 2600 });
  }
}

/** 选中模板即加载到画布 */
watch(selectedTpl, (name) => {
  if (!name) return;
  const t = templates.value.find((x) => x.name === name);
  if (!t) return;
  freeItems.value = t.items.map((it) => ({ ...it, id: nextFreeId++ }));
  selectedFreeId.value = 0;
  if (t.aspect > 0 && Math.abs(t.aspect - canvasAspect.value) / t.aspect > 0.05) {
    message.warning("模板纸张比例与当前不同，元素位置可能偏移", { duration: 3200 });
  }
});

/** 进入 free 模式：拉模板列表 + 确保缩略图（画布为空时不自动排布，等用户点「全部加入」） */
watch(mode, (m) => {
  if (m === "free") {
    refreshTemplates();
    ensureThumbs();
  }
});

/** 队列变化：free 模式下补拉缩略图（新加入的文件） */
watch(
  () => queue.value.join("|"),
  () => {
    if (mode.value === "free") ensureThumbs();
  },
);

const templateOptions = computed(() =>
  templates.value.map((t) => ({ label: t.name, value: t.name })),
);

/** 自动网格说明文本：估算总页数（图片 1 页/张；PDF 页数未知按文件计），给出列×行 */
const autoGridText = computed(() => {
  const n = mergePrint.value ? Math.max(1, queue.value.length) : 1;
  const cols = Math.ceil(Math.sqrt(n));
  const rows = Math.ceil(n / cols);
  return `${n} 页 → ${cols}×${rows}`;
});

/** 队列标题右侧的实时说明（随模式变化） */
const modeNote = computed(() => {
  switch (mode.value) {
    case "nup":
      if (nupAuto.value) {
        const n = mergePrint.value ? queue.value.length : 1;
        return `自动网格：${n} 页拼入一张纸${n > 1 ? `（约 ${Math.ceil(Math.sqrt(n))}×${Math.ceil(n / Math.ceil(Math.sqrt(n)))}）` : ""}`;
      }
      return `每张纸打印 ${nupCols.value}×${nupRows.value} 页`;
    case "booklet":
      return `横向纸每面 2 页 · ${bookletBind.value === "left" ? "左" : "右"}装订 · 对折成册${
        bookletSubset.value === "both" ? "" : ` · 仅${bookletSubset.value === "odd" ? "奇" : "偶"}数面`
      }`;
    case "invoice":
      return `发票排版：${
        invoiceLayout.value === "single" ? "单张" : invoiceLayout.value === "two" ? "1纸2张" : "1纸4张"
      }${cutLine.value ? " · 裁剪线" : ""}${dupOnSheet.value ? " · 每张重复2份" : ""}`;
    case "free":
      return `自由排版：画布 ${freeItems.value.length} 个元素 · 拖动定位 · 四角缩放`;
    default:
      return "每页一张纸；发票 PDF 选「铺满整页」效果最佳";
  }
});

/** 统一构造后端 PrintOptions（打印与预览共用，保证所见即所得） */
function buildOptions() {
  return {
    paperId: paperId.value,
    orientation: orientation.value,
    copies: copies.value,
    fit: fit.value,
    marginTopMm: marginTopMm.value,
    marginBottomMm: marginBottomMm.value,
    marginLeftMm: marginLeftMm.value,
    marginRightMm: marginRightMm.value,
    mode: mode.value,
    nupCols: nupAuto.value ? 0 : nupCols.value,
    nupRows: nupAuto.value ? 0 : nupRows.value,
    pageOrder: pageOrder.value,
    duplex: duplex.value,
    drawBorder: drawBorder.value,
    pdfDpi: pdfDpi.value,
    grayscale: grayscale.value,
    collate: collate.value,
    autoCenter: autoCenter.value,
    autoRotate: autoRotate.value,
    mergePrint: mergePrint.value,
    pageRange: pageRange.value,
    oddEven: oddEven.value,
    reverse: reverse.value,
    bookletSubset: bookletSubset.value,
    bookletBind: bookletBind.value,
    invoiceLayout: invoiceLayout.value,
    cutLine: cutLine.value,
    dupOnSheet: dupOnSheet.value,
    freeItems:
      mode.value === "free"
        ? freeItems.value.map(({ id, ...rest }) => {
            void id;
            return rest;
          })
        : [],
  };
}

/** 四边同步（uniformMargin 开启时任意一边变化 → 同步其余三边） */
function syncMargin(which: "top" | "bottom" | "left" | "right") {
  if (!uniformMargin.value) return;
  const v = { top: marginTopMm, bottom: marginBottomMm, left: marginLeftMm, right: marginRightMm }[which].value;
  marginTopMm.value = v;
  marginBottomMm.value = v;
  marginLeftMm.value = v;
  marginRightMm.value = v;
}

// === 实时预览（右栏）：面板开启后，配置 / 队列首项 / 打印机任一变化 → debounce 重绘 ===
let previewTimer: ReturnType<typeof setTimeout> | null = null;
let previewSeq = 0; // 竞态保护：丢弃过期请求的结果

/** 配置序列化（用于 watch 触发；保证字段顺序稳定） */
const optionsFingerprint = computed(() => JSON.stringify(buildOptions()));
/** 纸张相关指纹（free 模式的后端预览只随这些刷新；画布拖拽不触发重渲染） */
const paperFingerprint = computed(() =>
  JSON.stringify({
    pr: printer.value,
    pa: paperId.value,
    o: orientation.value,
    d: pdfDpi.value,
    m: mode.value,
  }),
);

/** 调度一次预览重绘（450ms 防抖合并连续变更） */
function schedulePreview() {
  if (previewTimer) clearTimeout(previewTimer);
  previewTimer = setTimeout(async () => {
    const path = previewTarget.value;
    if (!path || !printer.value) return;
    const paths = mergePrint.value ? queue.value.slice() : [path];
    const seq = ++previewSeq;
    previewing.value = true;
    try {
      const pages = await invoke<PreviewPage[]>("print_preview", {
        paths,
        printer: printer.value,
        options: buildOptions(),
      });
      if (seq !== previewSeq) return; // 过期结果丢弃
      previewPages.value = pages;
      if (previewIdx.value >= pages.length) previewIdx.value = 0;
    } catch (e) {
      if (seq !== previewSeq) return;
      previewPages.value = [];
      message.error(`生成预览失败: ${e}`, { duration: 3000 });
    } finally {
      if (seq === previewSeq) previewing.value = false;
    }
  }, 450);
}

/** 点击队列行：把该文件设为预览目标并立即重绘（合并打印时仅切标题锚点） */
function selectPreview(i: number) {
  if (i === previewSelIdx.value) return;
  previewSelIdx.value = i;
  previewIdx.value = 0;
  if (mode.value !== "free") schedulePreview();
}

/** 面板可见 + 非 free 模式：任何影响输出的状态变化都自动重绘预览 */
watch(
  [show, optionsFingerprint, () => queue.value[0], () => queue.value.length, () => printer.value],
  () => {
    if (!show.value) return;
    if (mode.value === "free") return; // free 画布自身即预览
    if (queue.value.length === 0 || !printer.value) {
      previewPages.value = [];
      return;
    }
    schedulePreview();
  },
  { immediate: true },
);

/** free 模式：后端预览仅随纸张/打印机/模式刷新（提供画布比例与最终校对） */
watch(
  [show, paperFingerprint],
  () => {
    if (!show.value) return;
    if (mode.value !== "free") return;
    if (queue.value.length === 0 || !printer.value) {
      previewPages.value = [];
      return;
    }
    schedulePreview();
  },
  { immediate: true },
);

/** 预览目标变化（换了文件）→ 回到第一张 */
watch(previewTarget, (nv, ov) => {
  if (nv !== ov) previewIdx.value = 0;
});

onUnmounted(() => {
  if (previewTimer) clearTimeout(previewTimer);
});

// cells：2×2 示意格的行主序页码（固定演示，不随当前列×行变化，保证四式可区分）
const pageOrderItems = [
  { value: "ltr-tb", label: "从左到右，从上到下", cells: [1, 2, 3, 4] },
  { value: "rtl-tb", label: "从右到左，从上到下", cells: [2, 1, 4, 3] },
  { value: "tb-ltr", label: "从上到下，从左到右", cells: [1, 3, 2, 4] },
  { value: "tb-rtl", label: "从上到下，从右到左", cells: [3, 1, 4, 2] },
] as const;

/** 打印机【属性】（需求文档2 七.4）：系统对话框改完回填纸张/方向/双面 */
async function openPrinterProps() {
  if (propsBusy.value || !printer.value) return;
  propsBusy.value = true;
  try {
    const r = await invoke<{ changed: boolean; paperId: number; orientation: number; duplex: string }>(
      "printer_properties",
      {
        printer: printer.value,
        paperId: paperId.value,
        orientation: orientation.value === 0 ? 1 : orientation.value,
        duplex: duplex.value,
      },
    );
    if (r.changed) {
      paperId.value = r.paperId;
      orientation.value = (r.orientation === 2 ? 2 : 1) as 0 | 1 | 2;
      duplex.value = (["off", "long", "short"].includes(r.duplex) ? r.duplex : "off") as
        | "off" | "long" | "short";
      message.success("已应用打印机属性设置", { duration: 1800 });
    }
  } catch (e) {
    message.error(`打开打印机属性失败: ${e}`, { duration: 2600 });
  } finally {
    propsBusy.value = false;
  }
}

// === 进度事件 ===
let unlistenProgress: (() => void) | null = null;
onMounted(async () => {
  window.addEventListener("keydown", onFreeKeydown);
  try {
    unlistenProgress = await listen<PrintProgress>("print://progress", (ev) => {
      const p = ev.payload;
      if (p.phase === "error") failures.value.push(p);
      progress.value = p;
    });
  } catch {
    // 非 Tauri 环境忽略
  }
});
onUnmounted(() => {
  window.removeEventListener("keydown", onFreeKeydown);
  unlistenProgress?.();
});

// === 执行打印 ===
const progressText = computed(() => {
  if (!printing.value || !progress.value) return "";
  const p = progress.value;
  return `正在打印 ${p.index + 1}/${p.total}：${baseName(p.path)}`;
});

async function startPrint() {
  if (printing.value) return;
  if (queue.value.length === 0) {
    message.info("请先添加要打印的文件", { duration: 2000 });
    return;
  }
  if (!printer.value) {
    message.warning("请选择打印机", { duration: 2000 });
    return;
  }
  printing.value = true;
  progress.value = null;
  failures.value = [];
  try {
    const results = await invoke<PrintFileResult[]>("print_files", {
      paths: queue.value,
      printer: printer.value,
      options: buildOptions(),
    });
    const okN = results.filter((r) => r.ok).length;
    const failN = results.length - okN;
    if (failN === 0) {
      message.success(`打印完成：已发送 ${okN} 个文档到「${printer.value}」`, { duration: 2600 });
    } else {
      message.warning(`打印完成：成功 ${okN} 个，失败 ${failN} 个（详见列表）`, { duration: 3600 });
    }
  } catch (e) {
    message.error(`打印失败: ${e}`, { duration: 3200 });
  } finally {
    printing.value = false;
  }
}

// === 预览渲染辅助（右栏） ===
const currentPreview = computed(() => {
  const p = previewPages.value[previewIdx.value];
  return p ? `data:image/png;base64,${p.pngBase64}` : "";
});

function dataUrl(p: PreviewPage): string {
  return `data:image/png;base64,${p.pngBase64}`;
}

/** 右栏标题：合并打印显示"N 个文件合并"，否则显示预览目标文件名 */
const previewFileLabel = computed(() => {
  if (mergePrint.value && queue.value.length > 1) {
    return `${queue.value.length} 个文件合并`;
  }
  const p = previewTarget.value;
  return p ? baseName(p) : "未选择文件";
});

/** 右栏占位提示（无打印机 / 队列为空时） */
const previewHint = computed(() => {
  if (!printer.value) return "请先选择打印机";
  if (queue.value.length === 0) return "在左侧添加图片或 PDF，此处将实时显示打印效果";
  return "暂无预览";
});

/** 手动刷新（右栏刷新按钮）：立即按当前配置重绘 */
function refreshPreview() {
  if (queue.value.length === 0 || !printer.value) return;
  schedulePreview();
}
</script>

<template>
  <n-modal
    v-model:show="show"
    preset="card"
    title="打印"
    :bordered="false"
    style="width: 90vw; max-width: 90vw"
  >
    <!-- ====== 左：打印调整控制面板 / 右：实时打印预览（约 90% 屏幕） ====== -->
    <div class="pp-split">
      <div class="pp-left">
    <!-- ==================== 打印设置 ==================== -->
    <div class="pp-form">
      <label class="field">
        <span class="f-label">打印机</span>
        <div class="row">
          <n-select
            v-model:value="printer"
            size="small"
            filterable
            :options="printerOptions"
            :loading="loadingPrinters"
            placeholder="选择打印机"
          />
          <button
            class="mini-btn"
            title="打印机属性（系统对话框）"
            :disabled="!printer || propsBusy"
            @click="openPrinterProps"
          >
            <n-icon :component="Adjustments" size="14" />
          </button>
          <button class="mini-btn" title="重新读取打印机列表" :disabled="printing" @click="loadPrinters">
            <n-icon :component="Refresh" size="14" :class="{ spin: loadingPrinters }" />
          </button>
        </div>
      </label>
      <label class="field">
        <span class="f-label">纸张大小</span>
        <n-select
          v-model:value="paperId"
          size="small"
          :options="paperOptions"
          :disabled="papers.length === 0"
          :placeholder="papers.length === 0 ? '打印机默认' : '选择纸张规格'"
        />
      </label>
      <label class="field">
        <span class="f-label">打印模式</span>
        <n-radio-group v-model:value="mode" size="small">
          <n-radio-button value="single">单页</n-radio-button>
          <n-radio-button value="nup">一张多页</n-radio-button>
          <n-radio-button value="booklet">小册子</n-radio-button>
          <n-radio-button value="invoice">发票</n-radio-button>
          <n-radio-button value="free">自由排版</n-radio-button>
        </n-radio-group>
      </label>
      <label v-if="mode === 'invoice'" class="field">
        <span class="f-label">发票排版</span>
        <n-radio-group v-model:value="invoiceLayout" size="small">
          <n-radio-button value="single">单张</n-radio-button>
          <n-radio-button value="two">1纸2张</n-radio-button>
          <n-radio-button value="four">1纸4张</n-radio-button>
        </n-radio-group>
      </label>
      <label class="field">
        <span class="f-label">双面打印</span>
        <n-radio-group v-model:value="duplex" size="small" :disabled="mode === 'booklet'">
          <n-radio-button value="off">关</n-radio-button>
          <n-radio-button value="long">长边翻转</n-radio-button>
          <n-radio-button value="short">短边翻转</n-radio-button>
        </n-radio-group>
      </label>
      <label class="field">
        <span class="f-label">方向</span>
        <n-radio-group v-model:value="orientation" size="small" :disabled="mode === 'booklet'">
          <n-radio-button :value="0">自动</n-radio-button>
          <n-radio-button :value="1">纵向</n-radio-button>
          <n-radio-button :value="2">横向</n-radio-button>
        </n-radio-group>
      </label>
      <label class="field">
        <span class="f-label">份数</span>
        <n-input-number v-model:value="copies" size="small" :min="1" :max="99" style="width: 100%" />
      </label>
      <label v-if="mode === 'nup'" class="field">
        <span class="f-label">
          每张排版（列 × 行）
          <n-checkbox
            v-model:checked="nupAuto"
            size="small"
            class="mg-uniform"
            title="按总页数自动定网格，全部拼入一张纸（4 页 → 2×2、9 页 → 3×3）"
          >自动</n-checkbox>
        </span>
        <div v-if="!nupAuto" class="row">
          <n-input-number v-model:value="nupCols" size="small" :min="1" :max="6" style="width: 100%" />
          <span class="x-sep">×</span>
          <n-input-number v-model:value="nupRows" size="small" :min="1" :max="6" style="width: 100%" />
        </div>
        <div v-else class="border-row nup-auto-note">
          按页数自动定网格：{{ autoGridText }}
        </div>
      </label>
      <label v-if="mode === 'nup'" class="field page-order-field">
        <span class="f-label">页面顺序（{{ pageOrderItems.find((i) => i.value === pageOrder)?.label }}）</span>
        <div class="po-group">
          <button
            v-for="item in pageOrderItems"
            :key="item.value"
            class="po-item"
            :class="{ active: pageOrder === item.value }"
            :title="item.label"
            @click="pageOrder = item.value as typeof pageOrder.value"
          >
            <span class="po-dot" />
            <span class="po-grid">
              <span v-for="(n, i) in item.cells" :key="i" class="po-cell">{{ n }}</span>
            </span>
          </button>
        </div>
        <div class="po-hint">
          图标以 2×2 示意填充顺序（数字小的先打印）<template v-if="nupCols === 1 || nupRows === 1">；当前 {{ nupCols }}×{{ nupRows }} 行/列为 1，方式 1≡3、2≡4 效果相同</template>
        </div>
      </label>
      <label class="field">
        <span class="f-label">缩放方式</span>
        <n-radio-group v-model:value="fit" size="small">
          <n-radio-button value="contain">适应页面</n-radio-button>
          <n-radio-button value="fill">铺满整页</n-radio-button>
          <n-radio-button value="actual">实际大小</n-radio-button>
        </n-radio-group>
      </label>
      <label class="field">
        <span class="f-label">
          页边距（毫米）
          <n-checkbox
            v-model:checked="uniformMargin"
            size="small"
            class="mg-uniform"
            @update:checked="syncMargin('top')"
          >四边同步</n-checkbox>
        </span>
        <div v-if="uniformMargin" class="row">
          <n-input-number
            v-model:value="marginTopMm"
            size="small"
            :min="0"
            :max="30"
            :step="1"
            style="width: 100%"
            @update:value="syncMargin('top')"
          />
        </div>
        <div v-else class="mg-grid">
          <n-input-number
            v-model:value="marginTopMm"
            size="small"
            :min="0"
            :max="30"
            :step="1"
            placeholder="上"
          />
          <n-input-number
            v-model:value="marginRightMm"
            size="small"
            :min="0"
            :max="30"
            :step="1"
            placeholder="右"
          />
          <n-input-number
            v-model:value="marginBottomMm"
            size="small"
            :min="0"
            :max="30"
            :step="1"
            placeholder="下"
          />
          <n-input-number
            v-model:value="marginLeftMm"
            size="small"
            :min="0"
            :max="30"
            :step="1"
            placeholder="左"
          />
        </div>
      </label>
      <label v-if="mode === 'booklet'" class="field">
        <span class="f-label">小册子子集</span>
        <n-radio-group v-model:value="bookletSubset" size="small">
          <n-radio-button value="both">双面</n-radio-button>
          <n-radio-button value="odd">仅奇数面</n-radio-button>
          <n-radio-button value="even">仅偶数面</n-radio-button>
        </n-radio-group>
      </label>
      <label v-if="mode === 'booklet'" class="field">
        <span class="f-label">装订方向</span>
        <n-radio-group v-model:value="bookletBind" size="small">
          <n-radio-button value="left">左装订</n-radio-button>
          <n-radio-button value="right">右装订</n-radio-button>
        </n-radio-group>
      </label>
      <label class="field">
        <span class="f-label">打印边框</span>
        <div class="border-row">
          <n-checkbox v-model:checked="drawBorder" size="small">
            一张多页 / 小册子时绘制分格线
          </n-checkbox>
        </div>
      </label>
      <label v-if="hasPdf" class="field">
        <span class="f-label">PDF 渲染 DPI</span>
        <n-select v-model:value="pdfDpi" size="small" :options="dpiOptions" />
      </label>
      <label class="field">
        <span class="f-label">页码范围（如 1-3,5）</span>
        <n-input v-model:value="pageRange" size="small" placeholder="全部页面" clearable />
      </label>
      <label class="field">
        <span class="f-label">奇偶页</span>
        <n-radio-group v-model:value="oddEven" size="small">
          <n-radio-button value="all">全部</n-radio-button>
          <n-radio-button value="odd">奇数页</n-radio-button>
          <n-radio-button value="even">偶数页</n-radio-button>
        </n-radio-group>
      </label>
      <label class="field">
        <span class="f-label">输出选项</span>
        <div class="border-row">
          <n-checkbox v-model:checked="grayscale" size="small">灰度打印</n-checkbox>
          <n-checkbox v-model:checked="collate" size="small" :disabled="copies < 2">
            逐份打印
          </n-checkbox>
          <n-checkbox
            v-if="queue.length > 1"
            v-model:checked="mergePrint"
            size="small"
            title="勾选后全部文件的页连续拼接（配合「一张多页」即可把多个文件拼到同一张纸上）"
          >
            合并打印
          </n-checkbox>
        </div>
      </label>
      <label class="field">
        <span class="f-label">顺序</span>
        <div class="border-row">
          <n-checkbox v-model:checked="reverse" size="small">逆序打印</n-checkbox>
          <n-checkbox v-if="mode === 'invoice'" v-model:checked="cutLine" size="small">
            裁剪线
          </n-checkbox>
          <n-checkbox v-if="mode === 'invoice'" v-model:checked="dupOnSheet" size="small">
            同票重复2份
          </n-checkbox>
        </div>
      </label>
      <label v-if="mode !== 'single'" class="field">
        <span class="f-label">布局适配</span>
        <div class="border-row">
          <n-checkbox v-model:checked="autoCenter" size="small">自动居中</n-checkbox>
          <n-checkbox v-model:checked="autoRotate" size="small">自动旋转</n-checkbox>
        </div>
      </label>
    </div>

    <!-- ==================== 待打印队列 ==================== -->
    <div class="q-head">
      <span class="q-title">待打印队列</span>
      <span class="q-count">{{ queue.length }} 个文件</span>
      <span class="q-note">{{ modeNote }}</span>
    </div>

    <div class="q-actions">
      <n-button size="small" secondary :disabled="printing" @click="addFiles">
        <template #icon><n-icon :component="FilePlus" size="15" /></template>
        添加文件
      </n-button>
      <n-button size="small" secondary :disabled="printing" @click="addFolder">
        <template #icon><n-icon :component="FolderPlus" size="15" /></template>
        添加文件夹
      </n-button>
      <div class="tb-spring" />
      <n-button size="small" quaternary type="error" :disabled="printing || queue.length === 0" @click="clearQueue">
        <template #icon><n-icon :component="Trash" size="15" /></template>
        清空
      </n-button>
    </div>

    <div class="q-box">
      <div v-if="queue.length === 0" class="q-empty">
        队列为空，点上方「添加文件 / 添加文件夹」加入要打印的图片或 PDF
      </div>
      <div v-else class="q-list">
        <div
          v-for="(p, i) in queue"
          :key="p"
          class="q-row"
          :class="{ active: i === previewSelIdx }"
          :title="`${p}（点击预览该文件）`"
          @click="selectPreview(i)"
        >
          <span class="q-kind" :class="{ pdf: kindOf(p) === 'PDF' }">{{ kindOf(p) }}</span>
          <span class="q-name ellip" :title="p">{{ baseName(p) }}</span>
          <span class="q-dir ellip" :title="p">{{ p }}</span>
          <button class="q-x" :disabled="printing" title="移出队列" @click.stop="removeAt(i)">
            <n-icon :component="X" size="13" />
          </button>
        </div>
      </div>
    </div>

    <!-- 失败明细（本次执行中的错误项） -->
    <div v-if="failures.length > 0" class="fail-box">
      <div v-for="(f, i) in failures" :key="i" class="fail-row">
        <span class="ellip">{{ baseName(f.path) }}</span>
        <span class="fail-err">{{ f.error }}</span>
      </div>
    </div>
      </div><!-- /pp-left -->

      <!-- ==================== 右：实时打印预览 / 自由排版画布 ==================== -->
      <div class="pp-right">
        <!-- ===== 自由排版画布（拖动 / 缩放 / 模板） ===== -->
        <div v-if="mode === 'free'" class="fc-box">
          <div class="fc-toolbar">
            <span class="pv-title">自由排版</span>
            <span class="pv-note">{{ freeItems.length }} 个元素</span>
            <div class="tb-spring" />
            <n-button size="tiny" secondary @click="addAllToCanvas">全部加入</n-button>
            <n-button size="tiny" secondary :disabled="freeItems.length === 0" @click="clearCanvas">
              清空
            </n-button>
          </div>
          <div ref="wrapEl" class="fc-canvas-wrap">
            <div
              ref="paperEl"
              class="fc-paper"
              :style="paperStyle"
              @pointerdown.self="selectedFreeId = 0"
            >
              <div
                v-for="it in freeItems"
                :key="it.id"
                class="fc-item"
                :class="{ sel: selectedFreeId === it.id }"
                :style="{
                  left: `${it.x * 100}%`,
                  top: `${it.y * 100}%`,
                  width: `${it.w * 100}%`,
                  height: `${it.h * 100}%`,
                }"
                @pointerdown="onItemDown($event, it)"
                @pointermove="onDragMove"
                @pointerup="onDragEnd"
                @pointercancel="onDragEnd"
              >
                <img
                  v-if="thumbOf(it)"
                  :src="thumbOf(it)"
                  class="fc-img"
                  draggable="false"
                  alt=""
                />
                <span v-else class="fc-item-name ellip">{{ baseName(it.path) }}</span>
                <button
                  v-if="selectedFreeId === it.id"
                  class="fc-x"
                  title="移除该元素"
                  @pointerdown.stop
                  @click.stop="removeFreeItem(it.id)"
                >×</button>
                <template v-if="selectedFreeId === it.id">
                  <span
                    v-for="h in ['nw', 'ne', 'sw', 'se']"
                    :key="h"
                    class="fc-handle"
                    :class="`fc-${h}`"
                    @pointerdown.stop.prevent="onHandleDown($event, it, h)"
                    @pointermove="onDragMove"
                    @pointerup="onDragEnd"
                    @pointercancel="onDragEnd"
                  />
                </template>
              </div>
              <div v-if="freeItems.length === 0" class="fc-empty">
                画布为空：点上方「全部加入」把队列文件排入画布，<br />
                再拖动 / 四角缩放自定义位置与大小
              </div>
            </div>
          </div>
          <div class="fc-tpl">
            <span class="f-label">模板</span>
            <n-select
              v-model:value="selectedTpl"
              size="tiny"
              clearable
              placeholder="选择模板载入"
              :options="templateOptions"
              style="flex: 1"
            />
            <n-input
              v-model:value="tplName"
              size="tiny"
              placeholder="模板名（空=自动编号）"
              style="width: 150px"
              @keydown.enter="saveTemplate"
            />
            <n-button size="tiny" secondary @click="saveTemplate">保存</n-button>
            <n-button
              size="tiny"
              quaternary
              type="error"
              :disabled="!selectedTpl"
              @click="deleteTemplate"
            >删除</n-button>
          </div>
        </div>

        <!-- ===== 普通模式：预览面板 ===== -->
        <div v-else class="pv-box">
          <div class="pv-head">
            <span class="pv-title">打印预览</span>
            <span class="pv-file ellip" :title="previewTarget">{{ previewFileLabel }}</span>
            <span class="pv-note">实时渲染</span>
            <div class="tb-spring" />
            <button
              class="mini-btn"
              title="上一张"
              :disabled="previewIdx === 0"
              @click="previewIdx--"
            >‹</button>
            <span class="pv-page">{{ previewing || previewPages.length === 0 ? "-" : `${previewIdx + 1} / ${previewPages.length}` }}</span>
            <button
              class="mini-btn"
              title="下一张"
              :disabled="previewIdx >= previewPages.length - 1"
              @click="previewIdx++"
            >›</button>
            <button
              class="mini-btn"
              title="重新生成预览"
              :disabled="previewing || queue.length === 0 || !printer"
              @click="refreshPreview"
            >
              <n-icon :component="Refresh" size="13" :class="{ spin: previewing }" />
            </button>
          </div>
          <div class="pv-body">
            <div v-if="previewing" class="pv-loading">
              <n-spin :size="22" />
              <span>正在生成预览…</span>
            </div>
            <img v-else-if="currentPreview" :src="currentPreview" class="pv-img" alt="打印预览" />
            <div v-else class="pv-empty">{{ previewHint }}</div>
          </div>
          <div v-if="!previewing && previewPages.length > 1" class="pv-thumbs">
            <img
              v-for="(p, i) in previewPages"
              :key="i"
              :src="dataUrl(p)"
              class="pv-thumb"
              :class="{ active: i === previewIdx }"
              @click="previewIdx = i"
            />
          </div>
        </div>
      </div>
    </div><!-- /pp-split -->

    <!-- ==================== 执行区 ==================== -->
    <div class="pp-foot">
      <div class="pp-progress">
        <template v-if="printing">
          <n-spin :size="16" />
          <span class="ellip">{{ progressText }}</span>
        </template>
        <template v-else-if="progress">
          <n-icon :component="Printer" size="15" class="ok-ic" />
          <span>就绪 · 上次 {{ progress.total }} 个文件</span>
        </template>
      </div>
      <n-button
        size="small"
        type="primary"
        :loading="printing"
        :disabled="queue.length === 0 || !printer"
        @click="startPrint"
      >
        <template #icon><n-icon :component="Printer" size="15" /></template>
        开始打印
      </n-button>
    </div>
  </n-modal>
</template>

<style scoped>
/* ==================== 左右分栏：左设置 / 右实时预览（窗口占屏约 90%） ==================== */
.pp-split {
  display: flex;
  gap: 14px;
  /* 弹窗 90vw：扣除标题栏/内边距/执行区后主区约 78vh，同时保底下限 */
  height: 78vh;
  min-height: 480px;
}
.pp-left {
  flex: 1;
  min-width: 0;
  overflow-y: auto;
  padding-right: 6px;
  scrollbar-width: thin;
  scrollbar-color: var(--jb-scrollbar) transparent;
}
.pp-right {
  flex: 0 0 520px;
  min-width: 0;
  display: flex;
}
/* ==================== 设置表单（两列栅格） ==================== */
.pp-form {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 10px 14px;
  margin-bottom: 14px;
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
.row {
  display: flex;
  gap: 6px;
  align-items: center;
}
.x-sep {
  font-size: 12px;
  color: var(--jb-text-mute);
  user-select: none;
}
.border-row {
  min-height: 28px;
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 4px 12px;
  font-size: 11.5px;
  color: var(--jb-text-soft);
}
/* 四边页边距（需求文档1：自定义上下左右） */
.mg-grid {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 4px;
}
.mg-uniform {
  margin-left: 8px;
  font-size: 10px;
}
.nup-auto-note {
  min-height: 28px;
  color: var(--jb-text-mute);
}
/* 页面顺序 4 箭头图标（需求文档1 5.1，仿 2345 看图王样式：圆点+方向箭头） */
.page-order-field {
  grid-column: 1 / -1;
}
.po-group {
  display: flex;
  gap: 4px;
}
.po-item {
  flex: 1;
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 6px 8px;
  border: 1px solid var(--jb-border);
  border-radius: 8px;
  background: var(--jb-bg-card);
  cursor: pointer;
  color: var(--jb-text-soft);
  transition: border-color 0.15s, background-color 0.15s, color 0.15s;
}
.po-item:hover {
  border-color: var(--jb-primary);
  color: var(--jb-text);
}
.po-item.active {
  border-color: var(--jb-primary);
  background: color-mix(in srgb, var(--jb-primary) 10%, transparent);
  color: var(--jb-primary);
}
.po-dot {
  width: 12px;
  height: 12px;
  flex-shrink: 0;
  border-radius: 50%;
  border: 1.8px solid currentColor;
  background: transparent;
  position: relative;
  box-sizing: border-box;
}
.po-item.active .po-dot {
  border-width: 3px;
  border-color: var(--jb-primary);
  background: var(--jb-primary);
  box-shadow: inset 0 0 0 2px var(--jb-bg-card);
}
/* 2×2 数字示意格（行主序页码，展示填充顺序） */
.po-grid {
  display: grid;
  grid-template-columns: repeat(2, 22px);
  grid-auto-rows: 22px;
  gap: 2px;
  flex-shrink: 0;
}
.po-cell {
  font-size: 11px;
  line-height: 20px;
  text-align: center;
  border: 1px solid color-mix(in srgb, currentColor 45%, transparent);
  border-radius: 4px;
  color: var(--jb-text-soft);
  font-variant-numeric: tabular-nums;
  user-select: none;
}
.po-item.active .po-cell {
  color: var(--jb-primary);
  border-color: color-mix(in srgb, var(--jb-primary) 55%, transparent);
  background: color-mix(in srgb, var(--jb-primary) 10%, transparent);
}
.po-hint {
  margin-top: 4px;
  font-size: 10.5px;
  color: var(--jb-text-mute);
  line-height: 1.5;
}
.row .n-select {
  flex: 1;
}
.mini-btn {
  width: 28px;
  height: 28px;
  flex-shrink: 0;
  border: 1px solid var(--jb-border);
  border-radius: 7px;
  background: var(--jb-bg-card);
  color: var(--jb-text-soft);
  cursor: pointer;
  display: flex;
  align-items: center;
  justify-content: center;
  transition: color 0.15s, border-color 0.15s;
}
.mini-btn:hover:not(:disabled) {
  color: var(--jb-primary);
  border-color: var(--jb-primary);
}
.mini-btn:disabled {
  opacity: 0.5;
  cursor: default;
}
.spin {
  animation: pp-rotate 0.9s linear infinite;
}
@keyframes pp-rotate {
  to {
    transform: rotate(360deg);
  }
}

/* ==================== 队列 ==================== */
.q-head {
  display: flex;
  align-items: baseline;
  gap: 10px;
  margin-bottom: 8px;
}
.q-title {
  font-size: 12.5px;
  font-weight: 600;
  color: var(--jb-text);
}
.q-count {
  font-size: 12px;
  font-weight: 600;
  color: var(--jb-primary);
}
.q-note {
  font-size: 10.5px;
  color: var(--jb-text-mute);
  ellipsis: true;
}
.tb-spring {
  flex: 1;
}
.q-actions {
  display: flex;
  gap: 8px;
  align-items: center;
  margin-bottom: 8px;
}
.q-box {
  border: 1px solid var(--jb-border);
  border-radius: 12px;
  background: var(--jb-bg);
  overflow: hidden;
}
.q-empty {
  padding: 26px 12px;
  text-align: center;
  font-size: 11.5px;
  color: var(--jb-text-mute);
}
.q-list {
  max-height: 210px;
  overflow-y: auto;
  scrollbar-width: thin;
  scrollbar-color: var(--jb-scrollbar) transparent;
}
.q-row {
  display: grid;
  grid-template-columns: 40px minmax(90px, 200px) minmax(0, 1fr) 24px;
  gap: 8px;
  align-items: center;
  padding: 6px 10px;
  font-size: 11.5px;
  cursor: pointer; /* 点击行 = 切换预览目标 */
  user-select: none;
}
.q-row:nth-child(odd) {
  background: color-mix(in srgb, var(--jb-bg-card) 55%, transparent);
}
/* 选中预览行：主题色高亮（置于奇偶行之后，覆盖条纹背景） */
.q-row.active {
  background: color-mix(in srgb, var(--jb-primary) 10%, transparent);
  box-shadow: inset 2px 0 0 var(--jb-primary);
}
.q-kind {
  font-size: 9.5px;
  font-weight: 600;
  letter-spacing: 0.5px;
  text-align: center;
  padding: 2px 0;
  border-radius: 5px;
  color: var(--jb-primary);
  background: color-mix(in srgb, var(--jb-primary) 12%, transparent);
}
.q-kind.pdf {
  color: #d89898;
  background: color-mix(in srgb, #d89898 14%, transparent);
}
.q-name {
  color: var(--jb-text);
  font-weight: 500;
}
.q-dir {
  font-size: 10.5px;
  color: var(--jb-text-mute);
}
.q-x {
  width: 20px;
  height: 20px;
  border: none;
  border-radius: 5px;
  background: transparent;
  color: var(--jb-text-mute);
  cursor: pointer;
  display: flex;
  align-items: center;
  justify-content: center;
  transition: color 0.15s, background-color 0.15s;
}
.q-x:hover:not(:disabled) {
  color: var(--jb-red);
  background: color-mix(in srgb, var(--jb-red) 12%, transparent);
}
.q-x:disabled {
  opacity: 0.4;
  cursor: default;
}

/* ==================== 右栏：自由排版画布 ==================== */
.fc-box {
  flex: 1;
  display: flex;
  flex-direction: column;
  border: 1px solid var(--jb-border);
  border-radius: 12px;
  background: var(--jb-bg);
  overflow: hidden;
}
.fc-toolbar {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 7px 10px;
  border-bottom: 1px solid var(--jb-border);
}
.fc-canvas-wrap {
  flex: 1;
  min-height: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 14px;
  background: color-mix(in srgb, var(--jb-bg-card) 45%, transparent);
}
.fc-paper {
  position: relative;
  background: #fff;
  border-radius: 3px;
  box-shadow: 0 2px 16px rgb(0 0 0 / 32%);
  overflow: hidden;
  touch-action: none;
}
.fc-item {
  position: absolute;
  display: flex;
  align-items: center;
  justify-content: center;
  border: 1px dashed color-mix(in srgb, var(--jb-text-mute) 55%, transparent);
  cursor: move;
  user-select: none;
  touch-action: none;
}
.fc-item.sel {
  border: 1.5px solid var(--jb-primary);
  z-index: 10;
}
.fc-img {
  width: 100%;
  height: 100%;
  object-fit: contain; /* 与打印端 contain 绘制一致（所见即所得） */
  pointer-events: none;
}
.fc-item-name {
  font-size: 10px;
  color: var(--jb-text-mute);
  max-width: 100%;
  padding: 0 4px;
}
.fc-x {
  position: absolute;
  top: -9px;
  right: -9px;
  width: 18px;
  height: 18px;
  border: none;
  border-radius: 50%;
  background: var(--jb-red);
  color: #fff;
  font-size: 12px;
  line-height: 16px;
  cursor: pointer;
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 11;
}
.fc-handle {
  position: absolute;
  width: 10px;
  height: 10px;
  border-radius: 2px;
  background: var(--jb-primary);
  border: 1.5px solid #fff;
  z-index: 12;
}
.fc-nw { left: -5px; top: -5px; cursor: nwse-resize; }
.fc-ne { right: -5px; top: -5px; cursor: nesw-resize; }
.fc-sw { left: -5px; bottom: -5px; cursor: nesw-resize; }
.fc-se { right: -5px; bottom: -5px; cursor: nwse-resize; }
.fc-empty {
  position: absolute;
  inset: 0;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  text-align: center;
  font-size: 12px;
  color: var(--jb-text-mute);
  line-height: 2;
  pointer-events: none;
}
.fc-tpl {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 7px 10px;
  border-top: 1px solid var(--jb-border);
}

/* ==================== 右栏：实时打印预览 ==================== */
.pv-box {
  flex: 1;
  display: flex;
  flex-direction: column;
  border: 1px solid var(--jb-border);
  border-radius: 12px;
  background: var(--jb-bg);
  overflow: hidden;
}
.pv-head {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 7px 10px;
  border-bottom: 1px solid var(--jb-border);
}
.pv-title {
  font-size: 12.5px;
  font-weight: 600;
  color: var(--jb-text);
}
.pv-file {
  max-width: 180px;
  font-size: 11px;
  color: var(--jb-text-soft);
}
.pv-note {
  font-size: 10px;
  color: var(--jb-text-mute);
}
.pv-page {
  min-width: 44px;
  text-align: center;
  font-size: 11px;
  color: var(--jb-text-soft);
  font-variant-numeric: tabular-nums;
}
.pv-body {
  flex: 1;
  min-height: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 14px;
  background: color-mix(in srgb, var(--jb-bg-card) 45%, transparent);
}
.pv-loading {
  display: flex;
  align-items: center;
  gap: 10px;
  font-size: 11.5px;
  color: var(--jb-text-mute);
}
.pv-empty {
  font-size: 12px;
  color: var(--jb-text-mute);
  text-align: center;
  line-height: 1.9;
  padding: 0 20px;
}
.pv-img {
  max-width: 100%;
  max-height: 100%;
  border-radius: 4px;
  box-shadow: 0 2px 14px rgb(0 0 0 / 28%);
  user-select: none;
}
.pv-thumbs {
  display: flex;
  gap: 6px;
  padding: 8px 10px;
  overflow-x: auto;
  scrollbar-width: thin;
  scrollbar-color: var(--jb-scrollbar) transparent;
}
.pv-thumb {
  width: 44px;
  border: 1px solid var(--jb-border);
  border-radius: 3px;
  opacity: 0.55;
  cursor: pointer;
  transition: opacity 0.15s, border-color 0.15s;
  flex-shrink: 0;
}
.pv-thumb:hover {
  opacity: 0.85;
}
.pv-thumb.active {
  opacity: 1;
  border-color: var(--jb-primary);
}

/* ==================== 失败明细 ==================== */
.fail-box {
  margin-top: 8px;
  border: 1px solid color-mix(in srgb, var(--jb-red) 35%, transparent);
  border-radius: 10px;
  background: color-mix(in srgb, var(--jb-red) 6%, transparent);
  padding: 6px 10px;
  max-height: 110px;
  overflow-y: auto;
  scrollbar-width: thin;
}
.fail-row {
  display: flex;
  gap: 10px;
  align-items: baseline;
  font-size: 11px;
  color: var(--jb-text);
  padding: 3px 0;
}
.fail-row > span:first-child {
  min-width: 120px;
  max-width: 220px;
}
.fail-err {
  color: var(--jb-red);
}

/* ==================== 执行区 ==================== */
.pp-foot {
  display: flex;
  align-items: center;
  gap: 10px;
  margin-top: 14px;
}
.pp-progress {
  flex: 1;
  min-width: 0;
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 11.5px;
  color: var(--jb-text-soft);
}
.ok-ic {
  color: var(--jb-primary);
}
.ellip {
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
</style>
