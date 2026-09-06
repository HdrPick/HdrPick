<script setup lang="ts">
import {
  ref,
  reactive,
  computed,
  onMounted,
  onUnmounted,
  watch,
} from "vue";
import { invoke } from "@tauri-apps/api/core";

const props = defineProps<{
  imageUrl: string;
}>();

// === 加载截图 ===
const imgEl = ref<HTMLImageElement | null>(null);
const imgLoaded = ref(false);
const imgError = ref("");

function loadImage() {
  imgLoaded.value = false;
  imgError.value = "";
  const img = new Image();
  img.onload = () => {
    imgLoaded.value = true;
    imgEl.value = img;
    initCanvas();
  };
  img.onerror = (e) => {
    imgError.value = "截图加载失败";
    console.error("image load error", e);
  };
  img.src = props.imageUrl;
}
onMounted(loadImage);
watch(() => props.imageUrl, loadImage);

// === Canvas ===
const canvasRef = ref<HTMLCanvasElement | null>(null);
let ctx: CanvasRenderingContext2D | null = null;

function initCanvas() {
  if (!imgEl.value || !canvasRef.value) return;
  const w = window.innerWidth;
  const h = window.innerHeight;
  canvasRef.value.width = w;
  canvasRef.value.height = h;
  ctx = canvasRef.value.getContext("2d");
  draw();
}

// === 选区 ===
interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}
const selection = reactive<Rect>({ x: 0, y: 0, w: 0, h: 0 });
const isSelecting = ref(false);
const isMovingSelection = ref(false);
const isResizing = ref(false);
const resizeHandle = ref<"" | "nw" | "ne" | "sw" | "se">("");
const dragStart = reactive({ x: 0, y: 0 });
const selStart = reactive<Rect>({ x: 0, y: 0, w: 0, h: 0 });
const hasSelection = ref(false);

// === 工具栏状态 ===
type Tool =
  | "select"
  | "pen"
  | "text"
  | "arrow"
  | "rect"
  | "circle"
  | "mosaic";
const activeTool = ref<Tool>("pen");
const penColor = ref("#D89898"); // 莫兰迪豆沙红
const penSize = ref(4); // 1-20px
const mosaicMode = ref<"block" | "gaussian">("block");
const mosaicSize = ref(8);

// 莫兰迪色系调色板（6 色）
const palette = [
  { name: "豆沙红", color: "#D89898" },
  { name: "香芋紫", color: "#C8A2C8" },
  { name: "薄荷绿", color: "#98C9B8" },
  { name: "雾霾蓝", color: "#9DB4C0" },
  { name: "奶茶棕", color: "#C9A87C" },
  { name: "浅灰", color: "#B8B8C0" },
];

// === 标注绘制 ===
interface Annotation {
  tool: Tool;
  color: string;
  size: number;
  // pen: points 列表
  points?: { x: number; y: number }[];
  // rect / circle / arrow: start + end
  start?: { x: number; y: number };
  end?: { x: number; y: number };
  // text
  text?: string;
  // mosaic
  mosaicMode?: "block" | "gaussian";
  mosaicSize?: number;
}
const annotations = ref<Annotation[]>([]);
const redoStack = ref<Annotation[]>([]);
const drawingAnnotation = ref<Annotation | null>(null);

// === 文字输入（简化版：prompt） ===
async function inputText(): Promise<string> {
  return new Promise((resolve) => {
    const inp = document.createElement("input");
    inp.type = "text";
    inp.placeholder = "输入文字";
    inp.style.position = "fixed";
    inp.style.left = "50%";
    inp.style.top = "50%";
    inp.style.transform = "translate(-50%, -50%)";
    inp.style.padding = "8px 12px";
    inp.style.borderRadius = "8px";
    inp.style.border = "1px solid #c8a2c8";
    inp.style.fontSize = "14px";
    inp.style.zIndex = "9999";
    inp.style.background = "#fff";
    inp.style.color = "#2c2c34";
    document.body.appendChild(inp);
    inp.focus();
    const done = (v: string) => {
      document.body.removeChild(inp);
      resolve(v);
    };
    inp.addEventListener("keydown", (e) => {
      if (e.key === "Enter") done(inp.value);
      if (e.key === "Escape") done("");
    });
    inp.addEventListener("blur", () => done(inp.value));
  });
}

// === 主绘制：背景 + 遮罩 + 选区 + 标注 ===
function draw() {
  if (!ctx || !canvasRef.value || !imgEl.value) return;
  const W = canvasRef.value.width;
  const H = canvasRef.value.height;
  ctx.clearRect(0, 0, W, H);

  // 1. 画截图（保持原始比例，cover 整个屏幕）
  drawImageCover(ctx, imgEl.value, 0, 0, W, H);

  // 2. 半透黑色遮罩（45% 不透明度）
  if (hasSelection.value) {
    ctx.fillStyle = "rgba(0, 0, 0, 0.45)";
    ctx.fillRect(0, 0, W, H);

    // 3. 挖出选区
    if (selection.w > 0 && selection.h > 0) {
      ctx.save();
      ctx.beginPath();
      ctx.rect(selection.x, selection.y, selection.w, selection.h);
      ctx.clip();
      drawImageCover(ctx, imgEl.value, 0, 0, W, H);
      ctx.restore();

      // 4. 选区虚线边框
      ctx.strokeStyle = "#ffffff";
      ctx.lineWidth = 1;
      ctx.setLineDash([6, 4]);
      ctx.strokeRect(
        selection.x + 0.5,
        selection.y + 0.5,
        selection.w - 1,
        selection.h - 1,
      );
      ctx.setLineDash([]);

      // 5. 4px 圆角控制点（8 个，4 角 + 4 边中点）
      drawResizeHandles(ctx);
    }
  }

  // 6. 绘制所有标注
  for (const a of annotations.value) {
    drawAnnotation(ctx, a);
  }
  if (drawingAnnotation.value) {
    drawAnnotation(ctx, drawingAnnotation.value);
  }
}

function drawImageCover(
  ctx: CanvasRenderingContext2D,
  img: HTMLImageElement,
  x: number,
  y: number,
  w: number,
  h: number,
) {
  // cover 模式：填满 w×h，可能裁剪
  const iw = img.naturalWidth;
  const ih = img.naturalHeight;
  const scale = Math.max(w / iw, h / ih);
  const dw = iw * scale;
  const dh = ih * scale;
  const dx = x + (w - dw) / 2;
  const dy = y + (h - dh) / 2;
  ctx.drawImage(img, dx, dy, dw, dh);
}

function drawResizeHandles(ctx: CanvasRenderingContext2D) {
  const handles = getHandles();
  ctx.fillStyle = "#ffffff";
  ctx.strokeStyle = "#c8a2c8";
  ctx.lineWidth = 1;
  for (const h of handles) {
    ctx.beginPath();
    ctx.roundRect(h.x - 4, h.y - 4, 8, 8, 2);
    ctx.fill();
    ctx.stroke();
  }
}

function getHandles(): { x: number; y: number; key: string }[] {
  const { x, y, w, h } = selection;
  return [
    { x, y, key: "nw" },
    { x: x + w / 2, y, key: "n" },
    { x: x + w, y, key: "ne" },
    { x: x + w, y: y + h / 2, key: "e" },
    { x: x + w, y: y + h, key: "se" },
    { x: x + w / 2, y: y + h, key: "s" },
    { x, y: y + h, key: "sw" },
    { x, y: y + h / 2, key: "w" },
  ];
}

function drawAnnotation(ctx: CanvasRenderingContext2D, a: Annotation) {
  ctx.save();
  ctx.strokeStyle = a.color;
  ctx.fillStyle = a.color;
  ctx.lineWidth = a.size;
  ctx.lineCap = "round";
  ctx.lineJoin = "round";

  if (a.tool === "pen" && a.points && a.points.length > 0) {
    ctx.beginPath();
    ctx.moveTo(a.points[0].x, a.points[0].y);
    for (let i = 1; i < a.points.length; i++) {
      ctx.lineTo(a.points[i].x, a.points[i].y);
    }
    ctx.stroke();
  } else if (a.tool === "rect" && a.start && a.end) {
    const x = Math.min(a.start.x, a.end.x);
    const y = Math.min(a.start.y, a.end.y);
    const w = Math.abs(a.end.x - a.start.x);
    const h = Math.abs(a.end.y - a.start.y);
    ctx.strokeRect(x, y, w, h);
  } else if (a.tool === "circle" && a.start && a.end) {
    const cx = (a.start.x + a.end.x) / 2;
    const cy = (a.start.y + a.end.y) / 2;
    const rx = Math.abs(a.end.x - a.start.x) / 2;
    const ry = Math.abs(a.end.y - a.start.y) / 2;
    ctx.beginPath();
    ctx.ellipse(cx, cy, rx, ry, 0, 0, Math.PI * 2);
    ctx.stroke();
  } else if (a.tool === "arrow" && a.start && a.end) {
    drawArrow(ctx, a.start, a.end, a.size);
  } else if (a.tool === "text" && a.text && a.start) {
    ctx.font = `${Math.max(14, a.size * 4)}px "Segoe UI Variable", "Microsoft YaHei", sans-serif`;
    ctx.textBaseline = "top";
    ctx.fillText(a.text, a.start.x, a.start.y);
  } else if (a.tool === "mosaic" && a.start && a.end) {
    drawMosaic(ctx, a.start, a.end, a.mosaicMode || "block", a.mosaicSize || 8);
  }
  ctx.restore();
}

function drawArrow(
  ctx: CanvasRenderingContext2D,
  from: { x: number; y: number },
  to: { x: number; y: number },
  size: number,
) {
  const dx = to.x - from.x;
  const dy = to.y - from.y;
  const len = Math.sqrt(dx * dx + dy * dy);
  if (len < 1) return;
  const angle = Math.atan2(dy, dx);
  const headLen = Math.min(20, size * 3 + 6);

  // 主线
  ctx.beginPath();
  ctx.moveTo(from.x, from.y);
  ctx.lineTo(
    to.x - headLen * 0.6 * Math.cos(angle),
    to.y - headLen * 0.6 * Math.sin(angle),
  );
  ctx.stroke();

  // 箭头头
  ctx.beginPath();
  ctx.moveTo(to.x, to.y);
  ctx.lineTo(
    to.x - headLen * Math.cos(angle - Math.PI / 6),
    to.y - headLen * Math.sin(angle - Math.PI / 6),
  );
  ctx.lineTo(
    to.x - headLen * Math.cos(angle + Math.PI / 6),
    to.y - headLen * Math.sin(angle + Math.PI / 6),
  );
  ctx.closePath();
  ctx.fill();
}

function drawMosaic(
  ctx: CanvasRenderingContext2D,
  start: { x: number; y: number },
  end: { x: number; y: number },
  mode: "block" | "gaussian",
  blockSize: number,
) {
  if (!canvasRef.value || !imgEl.value) return;
  const x = Math.min(start.x, end.x);
  const y = Math.min(start.y, end.y);
  const w = Math.abs(end.x - start.x);
  const h = Math.abs(end.y - start.y);
  if (w < 1 || h < 1) return;

  // 把当前 canvas 区域作为源，做马赛克
  const sourceCanvas = document.createElement("canvas");
  sourceCanvas.width = canvasRef.value.width;
  sourceCanvas.height = canvasRef.value.height;
  const sctx = sourceCanvas.getContext("2d");
  if (!sctx) return;
  sctx.drawImage(canvasRef.value, 0, 0);

  if (mode === "block") {
    // 方块马赛克
    const bw = Math.max(2, blockSize);
    for (let by = 0; by < h; by += bw) {
      for (let bx = 0; bx < w; bx += bw) {
        const data = sctx.getImageData(x + bx, y + by, 1, 1).data;
        ctx.fillStyle = `rgba(${data[0]}, ${data[1]}, ${data[2]}, ${data[3] / 255})`;
        ctx.fillRect(x + bx, y + by, bw, bw);
      }
    }
  } else {
    // 高斯模糊（用 filter）
    ctx.save();
    ctx.filter = `blur(${blockSize}px)`;
    ctx.drawImage(
      sourceCanvas,
      x,
      y,
      w,
      h,
      x,
      y,
      w,
      h,
    );
    ctx.restore();
    ctx.filter = "none";
  }
}

// === 鼠标交互 ===
function onMouseDown(e: MouseEvent) {
  if (e.button !== 0) return;
  const x = e.clientX;
  const y = e.clientY;
  dragStart.x = x;
  dragStart.y = y;
  selStart.x = selection.x;
  selStart.y = selection.y;
  selStart.w = selection.w;
  selStart.h = selection.h;

  // 1. 标注工具优先（已有选区时）
  if (hasSelection.value && activeTool.value !== "select") {
    const inSel = isInSelection(x, y);
    if (!inSel) {
      // 在选区外，重新选区
      startSelection(x, y);
    } else {
      // 在选区内开始绘制
      startDrawing(x, y);
    }
    return;
  }

  // 2. select 工具
  if (activeTool.value === "select") {
    // 检测控制点
    const handle = getHandleAt(x, y);
    if (handle) {
      isResizing.value = true;
      resizeHandle.value = handle as "nw" | "ne" | "sw" | "se";
      return;
    }
    // 检测选区内拖动
    if (hasSelection.value && isInSelection(x, y)) {
      isMovingSelection.value = true;
      return;
    }
    // 重新选区
    startSelection(x, y);
    return;
  }

  // 3. 没有选区，开始选区
  startSelection(x, y);
}

function startSelection(x: number, y: number) {
  isSelecting.value = true;
  selection.x = x;
  selection.y = y;
  selection.w = 0;
  selection.h = 0;
  hasSelection.value = false;
}

function startDrawing(x: number, y: number) {
  const a: Annotation = {
    tool: activeTool.value,
    color: penColor.value,
    size: penSize.value,
  };
  if (activeTool.value === "pen") {
    a.points = [{ x, y }];
  } else {
    a.start = { x, y };
    a.end = { x, y };
  }
  if (activeTool.value === "mosaic") {
    a.mosaicMode = mosaicMode.value;
    a.mosaicSize = mosaicSize.value;
  }
  drawingAnnotation.value = a;
  isDrawing.value = true;
}

const isDrawing = ref(false);

function onMouseMove(e: MouseEvent) {
  const x = e.clientX;
  const y = e.clientY;

  if (isResizing.value) {
    resizeSelection(x, y);
    draw();
    return;
  }
  if (isMovingSelection.value) {
    const dx = x - dragStart.x;
    const dy = y - dragStart.y;
    selection.x = selStart.x + dx;
    selection.y = selStart.y + dy;
    draw();
    return;
  }
  if (isSelecting.value) {
    selection.x = Math.min(dragStart.x, x);
    selection.y = Math.min(dragStart.y, y);
    selection.w = Math.abs(x - dragStart.x);
    selection.h = Math.abs(y - dragStart.y);
    draw();
    return;
  }
  if (isDrawing.value && drawingAnnotation.value) {
    const a = drawingAnnotation.value;
    if (a.tool === "pen" && a.points) {
      a.points.push({ x, y });
    } else {
      a.end = { x, y };
    }
    draw();
    return;
  }
  // 仅更新鼠标光标
  updateCursor(x, y);
}

async function onMouseUp(e: MouseEvent) {
  if (e.button !== 0) return;

  if (isResizing.value) {
    isResizing.value = false;
    resizeHandle.value = "";
    return;
  }
  if (isMovingSelection.value) {
    isMovingSelection.value = false;
    return;
  }
  if (isSelecting.value) {
    isSelecting.value = false;
    if (selection.w > 5 && selection.h > 5) {
      hasSelection.value = true;
    } else {
      hasSelection.value = false;
      selection.w = 0;
      selection.h = 0;
    }
    draw();
    return;
  }
  if (isDrawing.value) {
    isDrawing.value = false;
    const a = drawingAnnotation.value;
    drawingAnnotation.value = null;
    if (!a) return;

    // text 工具：弹出输入框
    if (a.tool === "text" && a.start && a.end) {
      const text = await inputText();
      if (text) {
        a.text = text;
        annotations.value.push(a);
        redoStack.value = [];
        draw();
      }
      return;
    }
    // pen / rect / circle / arrow / mosaic
    annotations.value.push(a);
    redoStack.value = [];
    draw();
  }
}

function resizeSelection(x: number, y: number) {
  const h = resizeHandle.value;
  const r = { ...selStart };
  if (h.includes("w")) {
    const newX = Math.min(x, r.x + r.w);
    selection.x = newX;
    selection.w = r.x + r.w - newX;
  }
  if (h.includes("n")) {
    const newY = Math.min(y, r.y + r.h);
    selection.y = newY;
    selection.h = r.y + r.h - newY;
  }
  if (h.includes("e")) {
    selection.w = Math.max(0, x - r.x);
  }
  if (h.includes("s")) {
    selection.h = Math.max(0, y - r.y);
  }
}

function getHandleAt(x: number, y: number): string | "" {
  for (const h of getHandles()) {
    if (Math.abs(h.x - x) <= 6 && Math.abs(h.y - y) <= 6) {
      return h.key;
    }
  }
  return "";
}

function isInSelection(x: number, y: number): boolean {
  return (
    x >= selection.x &&
    x <= selection.x + selection.w &&
    y >= selection.y &&
    y <= selection.y + selection.h
  );
}

function updateCursor(x: number, y: number) {
  if (!canvasRef.value) return;
  if (activeTool.value === "select") {
    if (getHandleAt(x, y)) {
      canvasRef.value.style.cursor = "nwse-resize";
    } else if (hasSelection.value && isInSelection(x, y)) {
      canvasRef.value.style.cursor = "move";
    } else {
      canvasRef.value.style.cursor = "crosshair";
    }
  } else {
    canvasRef.value.style.cursor = "crosshair";
  }
}

// === 工具栏操作 ===
function selectTool(t: Tool) {
  activeTool.value = t;
}

function undo() {
  if (annotations.value.length === 0) return;
  const a = annotations.value.pop();
  if (a) redoStack.value.push(a);
  draw();
}

function redo() {
  if (redoStack.value.length === 0) return;
  const a = redoStack.value.pop();
  if (a) annotations.value.push(a);
  draw();
}

function cancel() {
  invoke("close_annotation_window").catch((e) =>
    console.error("close failed", e),
  );
}

async function saveOrCopy(action: "save" | "copy" | "pin") {
  if (!hasSelection.value || selection.w < 2 || selection.h < 2) return;

  // 裁剪选区到新 canvas
  const out = document.createElement("canvas");
  out.width = Math.round(selection.w);
  out.height = Math.round(selection.h);
  const octx = out.getContext("2d");
  if (!octx || !canvasRef.value) return;

  // 从主 canvas 拷贝选区
  octx.drawImage(
    canvasRef.value,
    selection.x,
    selection.y,
    selection.w,
    selection.h,
    0,
    0,
    out.width,
    out.height,
  );

  // 导出 PNG
  const dataUrl = out.toDataURL("image/png");
  const base64 = dataUrl.split(",")[1] || "";

  if (action === "pin") {
    // 保存到临时文件 → 打开贴图窗口
    await pinSelection(out);
    return;
  }

  try {
    await invoke<string>("commit_annotation", {
      pngData: base64,
      autoSave: action === "save",
      copyToClipboard: action === "copy",
    });
  } catch (e) {
    console.error("commit failed", e);
  }
}

async function pinSelection(canvas: HTMLCanvasElement) {
  // 把 canvas 转成临时文件，然后打开贴图窗口
  const { writeFile, BaseDirectory } = await import("@tauri-apps/plugin-fs");
  const tempDir = "jietu-hdr";
  const fileName = `pin_${Date.now()}.png`;
  try {
    const dataUrl = canvas.toDataURL("image/png");
    const base64 = dataUrl.split(",")[1] || "";
    const bytes = Uint8Array.from(atob(base64), (c) => c.charCodeAt(0));
    await writeFile(`${tempDir}/${fileName}`, bytes, {
      baseDir: BaseDirectory.Temp,
    });
    // 获取临时目录完整路径
    const tempPath = `${await getTempDir()}\\jietu-hdr\\${fileName}`;
    await invoke("open_pin_window", {
      imagePath: tempPath,
      x: selection.x,
      y: selection.y,
      w: selection.w,
      h: selection.h,
    });
    // 关闭标注窗口
    await invoke("close_annotation_window");
  } catch (e) {
    console.error("pin failed", e);
  }
}

async function getTempDir(): Promise<string> {
  // 简单方案：用 Rust 命令读取（这里先返回硬编码）
  return "C:\\Users\\Administrator\\AppData\\Local\\Temp";
}

// === 键盘快捷键 ===
function onKeyDown(e: KeyboardEvent) {
  if (e.key === "Escape") {
    if (drawingAnnotation.value) {
      drawingAnnotation.value = null;
      isDrawing.value = false;
      draw();
    } else {
      cancel();
    }
  } else if ((e.ctrlKey || e.metaKey) && e.key === "z") {
    e.preventDefault();
    if (e.shiftKey) redo();
    else undo();
  } else if ((e.ctrlKey || e.metaKey) && e.key === "y") {
    e.preventDefault();
    redo();
  } else if ((e.ctrlKey || e.metaKey) && e.key === "s") {
    e.preventDefault();
    saveOrCopy("save");
  } else if ((e.ctrlKey || e.metaKey) && e.key === "c") {
    e.preventDefault();
    saveOrCopy("copy");
  } else if (e.key === "Enter" && hasSelection.value) {
    saveOrCopy("save");
  }
}

onMounted(() => {
  window.addEventListener("keydown", onKeyDown);
  window.addEventListener("resize", onResize);
});
onUnmounted(() => {
  window.removeEventListener("keydown", onKeyDown);
  window.removeEventListener("resize", onResize);
});
function onResize() {
  if (canvasRef.value) {
    canvasRef.value.width = window.innerWidth;
    canvasRef.value.height = window.innerHeight;
    draw();
  }
}

// 工具栏位置（跟随选区底部）
const toolbarPos = computed(() => {
  if (hasSelection.value && selection.h > 0) {
    const top = selection.y + selection.h + 12;
    // 选区右边对齐
    const left = Math.max(
      12,
      Math.min(window.innerWidth - 580, selection.x + selection.w - 560),
    );
    return { top: top + 8, left };
  }
  return { top: window.innerHeight - 80, left: window.innerWidth / 2 - 280 };
});
</script>

<template>
  <div class="annotation-root">
    <!-- 加载/错误状态 -->
    <div v-if="imgError" class="loading-screen">
      <span>{{ imgError }}</span>
      <button @click="cancel">关闭</button>
    </div>
    <div v-else-if="!imgLoaded" class="loading-screen">
      <span>正在准备截图...</span>
    </div>

    <!-- 主 canvas（截图 + 遮罩 + 标注） -->
    <canvas
      v-show="imgLoaded"
      ref="canvasRef"
      class="annotation-canvas"
      @mousedown="onMouseDown"
      @mousemove="onMouseMove"
      @mouseup="onMouseUp"
    />

    <!-- 顶部小提示 -->
    <div v-if="imgLoaded" class="hint-bar">
      <span v-if="!hasSelection">拖拽鼠标选择截图区域</span>
      <span v-else>选区内可标注 · 工具栏在选区下方</span>
      <span class="hint-key">ESC 取消 · Ctrl+Z 撤销 · Ctrl+S 保存</span>
    </div>

    <!-- 标注工具栏 -->
    <div
      v-if="imgLoaded && hasSelection"
      class="toolbar"
      :style="{ top: toolbarPos.top + 'px', left: toolbarPos.left + 'px' }"
    >
      <!-- 工具按钮组 -->
      <div class="tool-group">
        <button
          class="tool-btn"
          :class="{ active: activeTool === 'select' }"
          title="选区"
          @click="selectTool('select')"
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M5 3L5 17L9 13L12 20L14 19L11 12L17 12L5 3Z" />
          </svg>
        </button>
        <button
          class="tool-btn"
          :class="{ active: activeTool === 'pen' }"
          title="画笔"
          @click="selectTool('pen')"
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M12 19l7-7 3 3-7 7-3-3z" />
            <path d="M18 13l-1.5-7.5L2 2l3.5 14.5L13 18l5-5z" />
            <path d="M2 2l7.586 7.586" />
            <circle cx="11" cy="11" r="2" />
          </svg>
        </button>
        <button
          class="tool-btn"
          :class="{ active: activeTool === 'text' }"
          title="文字"
          @click="selectTool('text')"
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <polyline points="4 7 4 4 20 4 20 7" />
            <line x1="9" y1="20" x2="15" y2="20" />
            <line x1="12" y1="4" x2="12" y2="20" />
          </svg>
        </button>
        <button
          class="tool-btn"
          :class="{ active: activeTool === 'arrow' }"
          title="箭头"
          @click="selectTool('arrow')"
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <line x1="5" y1="19" x2="19" y2="5" />
            <polyline points="9 5 19 5 19 15" />
          </svg>
        </button>
        <button
          class="tool-btn"
          :class="{ active: activeTool === 'rect' }"
          title="矩形"
          @click="selectTool('rect')"
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <rect x="4" y="6" width="16" height="12" rx="1" />
          </svg>
        </button>
        <button
          class="tool-btn"
          :class="{ active: activeTool === 'circle' }"
          title="圆圈"
          @click="selectTool('circle')"
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <circle cx="12" cy="12" r="8" />
          </svg>
        </button>
        <button
          class="tool-btn"
          :class="{ active: activeTool === 'mosaic' }"
          title="马赛克"
          @click="selectTool('mosaic')"
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <rect x="3" y="3" width="7" height="7" />
            <rect x="14" y="3" width="7" height="7" />
            <rect x="3" y="14" width="7" height="7" />
            <rect x="14" y="14" width="7" height="7" />
          </svg>
        </button>
      </div>

      <div class="divider" />

      <!-- 调色板 -->
      <div class="palette-group">
        <button
          v-for="c in palette"
          :key="c.color"
          class="palette-btn"
          :class="{ active: penColor === c.color }"
          :style="{ background: c.color }"
          :title="c.name"
          @click="penColor = c.color"
        />
      </div>

      <div class="divider" />

      <!-- 粗细滑块 -->
      <div class="size-group">
        <input
          type="range"
          min="1"
          max="20"
          v-model.number="penSize"
          class="size-slider"
        />
        <div class="size-preview" :style="{
          width: penSize + 'px',
          height: penSize + 'px',
          background: penColor,
        }" />
      </div>

      <div class="divider" />

      <!-- 操作按钮 -->
      <div class="tool-group">
        <button class="tool-btn" title="撤销" @click="undo">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <polyline points="1 4 1 10 7 10" />
            <path d="M3.51 15a9 9 0 1 0 2.13-9.36L1 10" />
          </svg>
        </button>
        <button class="tool-btn" title="复制" @click="saveOrCopy('copy')">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <rect x="9" y="9" width="13" height="13" rx="2" />
            <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" />
          </svg>
        </button>
        <button class="tool-btn primary" title="保存" @click="saveOrCopy('save')">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M19 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11l5 5v11a2 2 0 0 1-2 2z" />
            <polyline points="17 21 17 13 7 13 7 21" />
            <polyline points="7 3 7 8 15 8" />
          </svg>
        </button>
        <button class="tool-btn" title="贴图" @click="saveOrCopy('pin')">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M12 17v5" />
            <path d="M9 10.76V6a2 2 0 0 1 4 0v4.76a3 3 0 1 1-4 0z" />
          </svg>
        </button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.annotation-root {
  position: fixed;
  inset: 0;
  user-select: none;
  overflow: hidden;
  /* 不透明背景：避免 transparent 窗口穿透鼠标到桌面 */
  background: #1c1c20;
}

.annotation-canvas {
  position: absolute;
  inset: 0;
  cursor: crosshair;
}

.loading-screen {
  position: absolute;
  inset: 0;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 12px;
  color: #fff;
  background: rgba(0, 0, 0, 0.6);
  font-size: 14px;
}

.hint-bar {
  position: fixed;
  top: 16px;
  left: 50%;
  transform: translateX(-50%);
  display: flex;
  align-items: center;
  gap: 14px;
  padding: 6px 14px;
  background: rgba(40, 36, 46, 0.78);
  backdrop-filter: blur(20px) saturate(1.4);
  border-radius: 20px;
  color: #fff;
  font-size: 12px;
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.2);
}
.hint-key {
  opacity: 0.65;
  font-size: 11px;
}

/* === 标注工具栏（横向长条） === */
.toolbar {
  position: fixed;
  display: flex;
  align-items: center;
  gap: 4px;
  height: 44px;
  padding: 0 8px;
  background: rgba(245, 240, 246, 0.82);
  backdrop-filter: blur(16px) saturate(1.6);
  /* Acrylic 模拟：backdrop + blur + 半透 */
  border-radius: 12px;
  border: 1px solid rgba(200, 162, 200, 0.25);
  box-shadow: 0 6px 24px rgba(0, 0, 0, 0.18);
  z-index: 100;
  /* 入场淡入 */
  animation: toolbar-in 220ms ease-out;
}

@keyframes toolbar-in {
  from {
    opacity: 0;
    transform: translateY(8px);
  }
  to {
    opacity: 1;
    transform: translateY(0);
  }
}

html.dark .toolbar {
  background: rgba(46, 38, 52, 0.82);
  border-color: rgba(180, 162, 184, 0.18);
}

.tool-group {
  display: flex;
  align-items: center;
  gap: 2px;
}

.tool-btn {
  width: 28px;
  height: 28px;
  display: flex;
  align-items: center;
  justify-content: center;
  border: none;
  background: transparent;
  color: var(--jb-text, #2c1f2a);
  cursor: pointer;
  border-radius: 6px;
  transition: background-color 0.15s;
  padding: 0;
}
.tool-btn:hover {
  background: var(--jb-titlebar-btn-hover, rgba(180, 162, 184, 0.18));
}
.tool-btn.active {
  /* 选中工具：主题色 25% 透明度（跟随主题） */
  background: color-mix(in srgb, var(--jb-primary, #c8a2c8) 25%, transparent);
}
.tool-btn.primary {
  color: var(--jb-primary, #c8a2c8);
}

.divider {
  width: 1px;
  height: 22px;
  background: rgba(180, 162, 184, 0.25);
  margin: 0 4px;
}

.palette-group {
  display: flex;
  align-items: center;
  gap: 4px;
}
.palette-btn {
  width: 18px;
  height: 18px;
  border-radius: 50%;
  border: 2px solid rgba(255, 255, 255, 0.4);
  cursor: pointer;
  padding: 0;
  transition: transform 0.15s;
}
.palette-btn:hover {
  transform: scale(1.15);
}
.palette-btn.active {
  border-color: var(--jb-primary, #c8a2c8);
  box-shadow: 0 0 0 2px color-mix(in srgb, var(--jb-primary, #c8a2c8) 30%, transparent);
}

.size-group {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 0 4px;
}
.size-slider {
  width: 70px;
  height: 4px;
  background: rgba(180, 162, 184, 0.3);
  border-radius: 2px;
  appearance: none;
  outline: none;
  cursor: pointer;
}
.size-slider::-webkit-slider-thumb {
  appearance: none;
  width: 14px;
  height: 14px;
  border-radius: 50%;
  background: #fff;
  border: 2px solid var(--jb-primary, #c8a2c8);
  cursor: pointer;
}
.size-preview {
  border-radius: 50%;
  min-width: 4px;
  min-height: 4px;
  max-width: 20px;
  max-height: 20px;
}
</style>
