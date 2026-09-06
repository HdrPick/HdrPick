<script setup lang="ts">
import { ref, reactive, computed, onMounted, onUnmounted } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { primaryColor, deriveAlpha } from "../themeColor";

// 当前主题色（选框 / 放大镜 / 控制点随主题色方案）
const themePrimary = computed(() => primaryColor.value);

// === 前端日志转发到 Rust 后端（写入 jietu-hdr.log）===
async function flog(msg: string) {
  try {
    await invoke("frontend_log", { msg });
  } catch {
    // 忽略
  }
}

// === 模式：selecting（透明拖选）→ annotating（标注模式）===
type Mode = "selecting" | "loading" | "annotating";
const mode = ref<Mode>("selecting");

// === 截图行为配置（主面板开关） ===
// show_toolbar=false → 不弹标注工具栏（Enter 确认保存仍可用）
// enable_pin=false → 工具栏隐藏贴图按钮
const cfgShowToolbar = ref(true);
const cfgEnablePin = ref(true);

// === QQ 式放大镜（选择模式：指针处像素放大 + 十字线 + 坐标） ===
const mouseIn = ref(false);
const mousePos = reactive({ x: -1, y: -1 });
const MAG_SIZE = 136; // 放大镜边长（CSS px，方形）
const MAG_ZOOM = 6; // 放大倍数

// === 选区状态 ===
interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}
const selection = reactive<Rect>({ x: 0, y: 0, w: 0, h: 0 });
const isSelecting = ref(false);
const dragStart = reactive({ x: 0, y: 0 });

// === 自动窗口识别（微信/QQ 式）===
// 悬停高亮窗口，单击 = 截取整个窗口，拖动 = 自由区域选择
interface WindowRect {
  x: number;
  y: number;
  w: number;
  h: number;
  title: string;
}
let screenWindows: WindowRect[] = []; // CSS 像素坐标（相对覆盖层视口）
const hoveredWindow = ref<WindowRect | null>(null);
// 窗口识别就绪标志：进入选择模式后的首次 mousemove 是系统补发事件，
// 不触发窗口高亮（避免进入瞬间虚线框闪烁）
let windowRecognitionArmed = false;
const mouseDown = ref(false);
let downPos = { x: 0, y: 0 };
let pendingWindow: WindowRect | null = null;
const DRAG_THRESHOLD = 4; // 超过该位移才算拖选，否则视为单击

// 加载窗口列表并转换为 CSS 坐标
// 后端已通过 MapWindowPoints 把窗口矩形换算为覆盖层客户区物理坐标，
// 前端只需除以 dpr（不再依赖 outerPosition——最大化窗口的 GetWindowRect
// 在 Windows 上可能带 -8px 边框溢出，会导致吸附偏移）
async function loadWindows() {
  try {
    const dpr = window.devicePixelRatio || 1;
    const vw = window.innerWidth;
    const vh = window.innerHeight;
    const wins = await invoke<
      { left: number; top: number; width: number; height: number; title: string }[]
    >("list_windows");
    screenWindows = [];
    for (const w of wins) {
      // 覆盖层客户区物理坐标 → CSS 坐标，并夹紧到视口内
      const x1 = Math.max(w.left / dpr, 0);
      const y1 = Math.max(w.top / dpr, 0);
      const x2 = Math.min((w.left + w.width) / dpr, vw);
      const y2 = Math.min((w.top + w.height) / dpr, vh);
      // 完全不在视口内或太小的窗口跳过
      if (x2 - x1 < 10 || y2 - y1 < 10) continue;
      screenWindows.push({ x: x1, y: y1, w: x2 - x1, h: y2 - y1, title: w.title });
    }
    flog(`窗口识别: ${screenWindows.length} 个可用窗口`);
  } catch (e) {
    flog(`加载窗口列表失败: ${e}`);
    screenWindows = [];
  }
}

// 找到坐标所在的最上层窗口（列表按 z 序排列，顶层在前）
function windowAt(x: number, y: number): WindowRect | null {
  for (const w of screenWindows) {
    if (x >= w.x && x <= w.x + w.w && y >= w.y && y <= w.y + w.h) {
      return w;
    }
  }
  return null;
}

// 标注模式下选区可移动/调整
const isMovingSelection = ref(false);
const isResizing = ref(false);
const resizeHandle = ref<"" | "nw" | "n" | "ne" | "e" | "se" | "s" | "sw" | "w">("");
const selStart = reactive<Rect>({ x: 0, y: 0, w: 0, h: 0 });

// === Canvas ===
const canvasRef = ref<HTMLCanvasElement | null>(null);
const canvasReady = ref(false); // canvas 是否已准备好（清空旧内容）
let ctx: CanvasRenderingContext2D | null = null;

// === 截图图像（标注模式）===
let capturedImg: HTMLImageElement | null = null;
const imgLoaded = ref(false);
// 截图临时文件路径（OCR 复用）
let capturedImgPath = "";

// === 预截图冻结背景（QQ 式）===
// 覆盖层立即显示（透明看实时屏幕），后端后台预截图 → bg-ready 事件
// 选区确认时若预截图已就绪则本地裁剪（零 IPC/零 hide/show）
let bgImg: HTMLImageElement | null = null;
let bgImgLoaded = false;
let bgLoadPromise: Promise<boolean> | null = null;

function startBgLoad(path: string) {
  bgImg = null;
  bgImgLoaded = false;
  if (!path) {
    bgLoadPromise = Promise.resolve(false);
    return;
  }
  bgLoadPromise = (async () => {
    try {
      const { readFile } = await import("@tauri-apps/plugin-fs");
      const bytes = await readFile(path);
      const blob = new Blob([bytes], { type: "image/png" });
      const dataUrl = await blobToDataURL(blob);
      const loaded = await new Promise<HTMLImageElement>((res, rej) => {
        const img = new Image();
        img.onload = () => res(img);
        img.onerror = () => rej(new Error("bg decode fail"));
        img.src = dataUrl;
      });
      bgImg = loaded;
      bgImgLoaded = true;
      flog(`预截图背景已加载: ${loaded.naturalWidth}x${loaded.naturalHeight}`);
      draw();
      return true;
    } catch (e) {
      flog(`加载预截图失败: ${e}`);
      return false;
    }
  })();
}

// 等待后台预截图就绪（选区确认时调用；超时则回退旧路径）
async function waitForBg(timeoutMs: number): Promise<boolean> {
  if (bgImgLoaded) return true;
  if (!bgLoadPromise) return false;
  try {
    await Promise.race([
      bgLoadPromise,
      new Promise<boolean>((_, rej) => setTimeout(() => rej(new Error("timeout")), timeoutMs)),
    ]);
  } catch {
    // 超时
  }
  return bgImgLoaded;
}

// === OCR 文字识别 ===
interface OcrLineUi {
  text: string;
  x: number;
  y: number;
  w: number;
  h: number;
}
const ocrActive = ref(false);
const ocrLoading = ref(false);
const ocrLines = ref<OcrLineUi[]>([]);

function initCanvas() {
  if (!canvasRef.value) return;
  // 高 DPI：canvas 分辨率 = 物理像素，绘制坐标仍用 CSS 像素（ctx 按 dpr 缩放）
  const dpr = window.devicePixelRatio || 1;
  canvasRef.value.width = Math.round(window.innerWidth * dpr);
  canvasRef.value.height = Math.round(window.innerHeight * dpr);
  ctx = canvasRef.value.getContext("2d");
  // 设置 width/height 会重置变换矩阵，必须重新应用 dpr 缩放
  ctx?.setTransform(dpr, 0, 0, dpr, 0, 0);
  draw();
  canvasReady.value = true;
}

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
const penColor = ref("#D89898");
const penSize = ref(4);
// 文字工具字体设置
const fontSize = ref(18);
const fontFamily = ref('"Segoe UI Variable", "Segoe UI", "Microsoft YaHei", sans-serif');
const fontSizeOptions = [12, 14, 16, 18, 20, 24, 28, 32, 40, 48];
const fontFamilyOptions = [
  { label: "Segoe UI", value: '"Segoe UI Variable", "Segoe UI", sans-serif' },
  { label: "微软雅黑", value: '"Microsoft YaHei", sans-serif' },
  { label: "宋体", value: '"SimSun", serif' },
  { label: "黑体", value: '"SimHei", sans-serif' },
  { label: "楷体", value: '"KaiTi", serif' },
  { label: "等线", value: '"DengXian", sans-serif' },
];

const palette = [
  { name: "豆沙红", color: "#D89898" },
  { name: "香芋紫", color: "#C8A2C8" },
  { name: "薄荷绿", color: "#98C9B8" },
  { name: "雾霾蓝", color: "#9DB4C0" },
  { name: "奶茶棕", color: "#C9A87C" },
  { name: "浅灰", color: "#B8B8C0" },
];

// === 标注数据 ===
interface Annotation {
  tool: Tool;
  color: string;
  size: number;
  points?: { x: number; y: number }[];
  start?: { x: number; y: number };
  end?: { x: number; y: number };
  text?: string;
  fontSize?: number;
  fontFamily?: string;
  gaussian?: boolean; // 马赛克高斯模糊模式（false/缺省 = 方块模式）
}
const annotations = ref<Annotation[]>([]);
const redoStack = ref<Annotation[]>([]);
const drawingAnnotation = ref<Annotation | null>(null);
const isDrawing = ref(false);

// 马赛克模式：方块 / 高斯模糊（5.12 双模式）
const mosaicMode = ref<"block" | "gaussian">("block");

// === 文字输入 ===
// 在鼠标点击位置弹出输入框，应用当前字体设置
async function inputText(x: number, y: number): Promise<string> {
  return new Promise((resolve) => {
    const inp = document.createElement("input");
    inp.type = "text";
    inp.placeholder = "输入文字";
    inp.style.position = "fixed";
    // 点击位置弹出（限制在屏幕内）
    inp.style.left = `${Math.min(x, window.innerWidth - 220)}px`;
    inp.style.top = `${Math.min(y, window.innerHeight - 40)}px`;
    inp.style.padding = "6px 10px";
    inp.style.borderRadius = "8px";
    inp.style.border = `1px solid ${themePrimary.value}`;
    inp.style.fontFamily = fontFamily.value;
    inp.style.fontSize = `${fontSize.value}px`;
    inp.style.zIndex = "9999";
    inp.style.background = "#fff";
    inp.style.color = "#2c2c34";
    inp.style.boxShadow = "0 2px 8px rgba(0,0,0,0.2)";
    document.body.appendChild(inp);
    inp.focus();
    let finished = false;
    const done = (v: string) => {
      if (finished) return;
      finished = true;
      inp.remove();
      resolve(v);
    };
    inp.addEventListener("keydown", (e) => {
      e.stopPropagation(); // 防止 ESC/Enter 冒泡到全局快捷键处理
      if (e.key === "Enter") done(inp.value);
      if (e.key === "Escape") done("");
    });
    inp.addEventListener("blur", () => done(inp.value));
  });
}

// === 主绘制 ===
// 点击吸附路径的加载期是否留白（不绘制加载虚线框，避免 canvas 状态
// 在覆盖层隐藏/重现间被重置导致的异常缩小虚线框）
let loadingBlank = false;
function draw() {
  if (!ctx || !canvasRef.value) return;
  // 防御性重设 DPR 变换：覆盖层 hide/show 期间 canvas 可能被重置为恒等变换，
  // 不重设会绘制出按物理像素（缩小约 1/dpr）的错误虚线框
  const dprNow = window.devicePixelRatio || 1;
  ctx.setTransform(dprNow, 0, 0, dprNow, 0, 0);
  const W = canvasRef.value.width;
  const H = canvasRef.value.height;
  ctx.clearRect(0, 0, W, H);

  if (mode.value === "selecting" || mode.value === "loading") {
    // 吸附路径 loading 期：不绘制预截图冻结帧（热键时刻旧帧，会造成
    // "实时 → 旧帧 → 实时截图"回闪），保持透明持续透出实时画面，
    // 仅画选区蚂蚁线作为处理反馈；实时截图就绪后直接进入标注（一步到位）
    const snapLoading = mode.value === "loading" && loadingBlank;
    if (!snapLoading && bgImgLoaded && bgImg) {
      // QQ 式冻结画面：全屏绘制（覆盖层已全屏盖住任务栏，
      // 冻结背景的任务栏即唯一所见，无 Mica 半透明叠加问题）
      ctx.drawImage(bgImg, 0, 0, window.innerWidth, window.innerHeight);
    }
    if (loadingBlank) {
      // 吸附处理中：透明 + 选区蚂蚁线（无遮罩/无冻结背景/无放大镜）
      if (selection.w > 0 && selection.h > 0) {
        ctx.save();
        ctx.strokeStyle = themePrimary.value;
        ctx.lineWidth = 2;
        ctx.setLineDash([8, 4]);
        ctx.lineDashOffset = -Date.now() / 50;
        ctx.strokeRect(
          selection.x + 0.5,
          selection.y + 0.5,
          selection.w - 1,
          selection.h - 1,
        );
        ctx.setLineDash([]);
        ctx.restore();
      }
      return;
    }
    // 透明拖选模式 / 加载中（保持选区虚线框可见，避免消失闪烁）
    if (isSelecting.value || (selection.w > 0 && selection.h > 0)) {
      drawMaskAndSelection(W, H);
      if (selection.w > 5 && selection.h > 5) {
        drawSizeLabel();
      }
    } else if (hoveredWindow.value) {
      // 悬停窗口高亮（微信式：全屏遮罩 + 窗口区域挖空 + 蚂蚁线边框 + 尺寸）
      drawWindowHighlight(W, H);
    }
    // QQ 式放大镜（悬停 + 拖选中均跟随指针）
    if (!ocrActive.value && mouseIn.value) {
      drawMagnifier();
    }
  } else if (mode.value === "annotating" && capturedImg) {
    // 标注模式：半透遮罩 + 选区位置绘制截图 + 标注
    // 1. 半透遮罩铺满全屏
    ctx.fillStyle = "rgba(0, 0, 0, 0.55)";
    ctx.fillRect(0, 0, W, H);

    // 2. 在原选区位置绘制截取的图像（保持原尺寸，不放大）
    if (selection.w > 0 && selection.h > 0) {
      // 将截取的图像绘制到选区位置（图像本身是选区大小，直接绘制）
      ctx.drawImage(
        capturedImg,
        selection.x,
        selection.y,
        selection.w,
        selection.h,
      );

      // 选区边框
      ctx.strokeStyle = themePrimary.value;
      ctx.lineWidth = 2;
      ctx.setLineDash([8, 4]);
      ctx.lineDashOffset = -Date.now() / 50;
      ctx.strokeRect(
        selection.x + 0.5,
        selection.y + 0.5,
        selection.w - 1,
        selection.h - 1,
      );
      ctx.setLineDash([]);

      // 控制点（仅在 select 工具下显示）
      if (activeTool.value === "select") {
        drawResizeHandles(ctx);
      }
    }

    // 3. 绘制标注（标注坐标基于全屏 canvas，选区内绘制）
    for (const a of annotations.value) {
      drawAnnotation(ctx, a);
    }
    if (drawingAnnotation.value) {
      drawAnnotation(ctx, drawingAnnotation.value);
    }

    // 4. OCR 识别区域高亮（半透明主题色框，文字层在 DOM 中可选中复制）
    if (ocrActive.value) {
      ctx.save();
      for (const l of ocrLines.value) {
        ctx.fillStyle = deriveAlpha(themePrimary.value, 0.14);
        ctx.fillRect(l.x, l.y, l.w, l.h);
        ctx.strokeStyle = deriveAlpha(themePrimary.value, 0.5);
        ctx.lineWidth = 1;
        ctx.strokeRect(l.x + 0.5, l.y + 0.5, l.w - 1, l.h - 1);
      }
      ctx.restore();
    }
  }
}

function drawMaskAndSelection(W: number, H: number) {
  if (!ctx) return;
  ctx.fillStyle = "rgba(0, 0, 0, 0.3)";
  ctx.fillRect(0, 0, W, H);
  if (selection.w > 0 && selection.h > 0) {
    ctx.save();
    ctx.globalCompositeOperation = "destination-out";
    ctx.fillStyle = "rgba(0, 0, 0, 1)";
    ctx.fillRect(selection.x, selection.y, selection.w, selection.h);
    ctx.restore();

    ctx.save();
    ctx.strokeStyle = themePrimary.value;
    ctx.lineWidth = 2;
    ctx.setLineDash([8, 4]);
    ctx.lineDashOffset = -Date.now() / 50;
    ctx.strokeRect(
      selection.x + 0.5,
      selection.y + 0.5,
      selection.w - 1,
      selection.h - 1,
    );
    ctx.setLineDash([]);
    ctx.restore();
  }
}

function drawSizeLabel() {
  if (selection.w < 1 || selection.h < 1) return;
  drawRectSizeLabel(selection.x, selection.y, selection.w, selection.h);
}

// 通用尺寸标签（选区 / 悬停窗口共用）：W × H + 近似整数比值（如 16:9）
function drawRectSizeLabel(x: number, y: number, w: number, h: number) {
  if (!ctx || w < 1 || h < 1) return;
  const ratio = aspectRatioText(w, h);
  const label = ratio
    ? `${Math.round(w)} × ${Math.round(h)} (${ratio})`
    : `${Math.round(w)} × ${Math.round(h)}`;
  ctx.save();
  ctx.font = '12px "Segoe UI Variable", "Microsoft YaHei", sans-serif';
  const metrics = ctx.measureText(label);
  const padX = 8;
  const boxW = metrics.width + padX * 2;
  const boxH = 20;
  let bx = x + w - boxW - 4;
  let by = y + h - boxH - 4;
  if (w < boxW + 8 || h < boxH + 8) {
    bx = x;
    by = y + h + 4;
    if (by + boxH > window.innerHeight) {
      by = y - boxH - 4;
    }
  }
  ctx.fillStyle = "rgba(40, 36, 46, 0.85)";
  ctx.beginPath();
  ctx.roundRect(bx, by, boxW, boxH, 4);
  ctx.fill();
  ctx.fillStyle = "#f0e6f2";
  ctx.textBaseline = "middle";
  ctx.fillText(label, bx + padX, by + boxH / 2);
  ctx.restore();
}

// 最大公约数
function gcd(a: number, b: number): number {
  return b === 0 ? a : gcd(b, a % b);
}

// 近似整数宽高比（QQ 式）：先约分；分子/分母过大时在 1..32 内找最接近的小整数比
function aspectRatioText(w: number, h: number): string {
  const W = Math.round(w);
  const H = Math.round(h);
  if (W < 8 || H < 8) return "";
  const g = gcd(W, H);
  const rw = W / g;
  const rh = H / g;
  if (rw <= 32 && rh <= 32) return `${rw}:${rh}`;
  // 暴力搜索：与实际比值相对误差最小的小整数比（同误差取更简单者）
  const target = W / H;
  let bestA = 16;
  let bestB = 9;
  let bestErr = Infinity;
  for (let a = 1; a <= 32; a++) {
    for (let b = 1; b <= 32; b++) {
      const err = Math.abs(a / b - target) / target;
      if (
        err < bestErr - 1e-9 ||
        (Math.abs(err - bestErr) < 1e-9 && a + b < bestA + bestB)
      ) {
        bestErr = err;
        bestA = a;
        bestB = b;
      }
    }
  }
  return `${bestA}:${bestB}`;
}

// QQ 式放大镜：指针周围像素放大 + 中心十字线 + 物理像素坐标
function drawMagnifier() {
  if (!ctx) return;
  const x = mousePos.x;
  const y = mousePos.y;
  if (x < 0 || y < 0) return;

  const dpr = window.devicePixelRatio || 1;
  const box = MAG_SIZE;
  const zoom = MAG_ZOOM;
  const pad = 4;
  const inner = box - pad * 2;

  // 位置：指针右下偏移 20px，屏幕边缘自动翻转
  let bx = x + 20;
  let by = y + 20;
  if (bx + box > window.innerWidth) bx = x - box - 20;
  if (by + box > window.innerHeight) by = y - box - 20;

  ctx.save();
  // 底板 + 主题色描边
  ctx.fillStyle = "rgba(28, 22, 32, 0.92)";
  ctx.strokeStyle = deriveAlpha(themePrimary.value, 0.9);
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.roundRect(bx, by, box, box, 8);
  ctx.fill();
  ctx.stroke();

  // 放大内容：从冻结背景图按物理像素采样（CSS→自然像素换算）
  if (bgImgLoaded && bgImg) {
    const kx = bgImg.naturalWidth / window.innerWidth;
    const ky = bgImg.naturalHeight / window.innerHeight;
    // 方形源区域：边长 = inner/zoom 个 CSS 像素 → 自然像素
    const srcSideCss = inner / zoom;
    const srcSide = srcSideCss * Math.min(kx, ky);
    // 源中心 = 指针位置（越界时 clamp，保证中心尽量贴近指针）
    let scx = x * kx;
    let scy = y * ky;
    scx = Math.max(srcSide / 2, Math.min(scx, bgImg.naturalWidth - srcSide / 2));
    scy = Math.max(srcSide / 2, Math.min(scy, bgImg.naturalHeight - srcSide / 2));

    ctx.save();
    ctx.beginPath();
    ctx.roundRect(bx + pad, by + pad, inner, inner, 5);
    ctx.clip();
    ctx.drawImage(
      bgImg,
      scx - srcSide / 2,
      scy - srcSide / 2,
      srcSide,
      srcSide,
      bx + pad,
      by + pad,
      inner,
      inner,
    );
    // 中心十字线（对准指针像素）
    const cx = bx + pad + inner / 2;
    const cy = by + pad + inner / 2;
    ctx.strokeStyle = "rgba(216, 152, 152, 0.95)";
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(cx, by + pad);
    ctx.lineTo(cx, by + pad + inner);
    ctx.moveTo(bx + pad, cy);
    ctx.lineTo(bx + pad + inner, cy);
    ctx.stroke();
    ctx.restore();
  }

  // 坐标条（物理像素，QQ 式），放不下时移到放大镜上方
  const label = `${Math.round(x * dpr)}, ${Math.round(y * dpr)}`;
  ctx.font = '12px "Segoe UI Variable", "Microsoft YaHei", sans-serif';
  const tw = ctx.measureText(label).width;
  const labelH = 22;
  const labelW = tw + 16;
  const lx = bx + (box - labelW) / 2;
  let ly = by + box + 6;
  if (ly + labelH > window.innerHeight) {
    ly = by - labelH - 6;
  }
  ctx.fillStyle = "rgba(28, 22, 32, 0.92)";
  ctx.beginPath();
  ctx.roundRect(lx, ly, labelW, labelH, 4);
  ctx.fill();
  ctx.strokeStyle = deriveAlpha(themePrimary.value, 0.5);
  ctx.stroke();
  ctx.fillStyle = "#f0e6f2";
  ctx.textBaseline = "middle";
  ctx.fillText(label, lx + 8, ly + labelH / 2);
  ctx.restore();
}

// 悬停窗口高亮：全屏遮罩 + 窗口区域挖空 + 蚂蚁线边框 + 尺寸标签
function drawWindowHighlight(W: number, H: number) {
  if (!ctx || !hoveredWindow.value) return;
  const win = hoveredWindow.value;
  ctx.fillStyle = "rgba(0, 0, 0, 0.3)";
  ctx.fillRect(0, 0, W, H);
  ctx.save();
  ctx.globalCompositeOperation = "destination-out";
  ctx.fillStyle = "rgba(0, 0, 0, 1)";
  ctx.fillRect(win.x, win.y, win.w, win.h);
  ctx.restore();

  ctx.save();
  ctx.strokeStyle = themePrimary.value;
  ctx.lineWidth = 2;
  ctx.setLineDash([8, 4]);
  ctx.lineDashOffset = -Date.now() / 50;
  ctx.strokeRect(win.x + 0.5, win.y + 0.5, win.w - 1, win.h - 1);
  ctx.setLineDash([]);
  ctx.restore();

  drawRectSizeLabel(win.x, win.y, win.w, win.h);
}

function drawResizeHandles(ctx: CanvasRenderingContext2D) {
  const handles = getHandles();
  ctx.fillStyle = "#ffffff";
  ctx.strokeStyle = themePrimary.value;
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
    ctx.font = `${a.fontSize || 18}px ${a.fontFamily || '"Segoe UI Variable", "Segoe UI", "Microsoft YaHei", sans-serif'}`;
    ctx.textBaseline = "top";
    ctx.fillText(a.text, a.start.x, a.start.y);
  } else if (a.tool === "mosaic" && a.points && a.points.length > 0) {
    // 马赛克：块大小/模糊强度跟随画笔粗细滑块（交互一致）
    drawMosaic(ctx, a.points, a.size || 8, a.gaussian === true);
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
  ctx.beginPath();
  ctx.moveTo(from.x, from.y);
  ctx.lineTo(
    to.x - headLen * 0.6 * Math.cos(angle),
    to.y - headLen * 0.6 * Math.sin(angle),
  );
  ctx.stroke();
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
  points: { x: number; y: number }[],
  blockSize: number,
  gaussian = false,
) {
  if (!capturedImg || !canvasRef.value || points.length === 0) return;

  // 从原始截图采样（不读当前 canvas——上面有遮罩+已绘内容，会导致半透明叠加污染）
  const imgW = capturedImg.naturalWidth;
  const imgH = capturedImg.naturalHeight;
  const off = document.createElement("canvas");
  off.width = imgW;
  off.height = imgH;
  const octx = off.getContext("2d", { willReadFrequently: true });
  if (!octx) return;
  octx.drawImage(capturedImg, 0, 0);

  const sel = selection;

  if (gaussian) {
    // 高斯模糊模式：预生成整张模糊源图，沿笔迹以圆形裁剪贴回
    const blurPx = Math.max(2, blockSize);
    const blurCanvas = document.createElement("canvas");
    blurCanvas.width = imgW;
    blurCanvas.height = imgH;
    const bctx = blurCanvas.getContext("2d");
    if (!bctx) return;
    bctx.filter = `blur(${blurPx}px)`;
    bctx.drawImage(off, 0, 0);
    bctx.filter = "none";

    // 屏幕坐标 → 截图像素坐标（选区等比映射）
    const toImg = (p: { x: number; y: number }) => ({
      x: Math.min(imgW - 1, Math.max(0, Math.round(((p.x - sel.x) / sel.w) * imgW))),
      y: Math.min(imgH - 1, Math.max(0, Math.round(((p.y - sel.y) / sel.h) * imgH))),
    });
    const r = Math.max(6, blockSize * 1.5); // 屏幕半径
    // 去重（按块间距）避免重复贴图
    const seen = new Set<string>();
    const step = Math.max(4, Math.floor(blockSize / 2));
    ctx.save();
    for (const p of points) {
      const kx = Math.floor(p.x / step);
      const ky = Math.floor(p.y / step);
      const key = `${kx},${ky}`;
      if (seen.has(key)) continue;
      seen.add(key);
      const ip = toImg(p);
      // 裁剪半径（图像像素，等比）
      const irx = Math.max(1, Math.round((r / sel.w) * imgW));
      const iry = Math.max(1, Math.round((r / sel.h) * imgH));
      const sx = Math.max(0, ip.x - irx);
      const sy = Math.max(0, ip.y - iry);
      const sw = Math.min(imgW - sx, irx * 2);
      const sh = Math.min(imgH - sy, iry * 2);
      ctx.save();
      ctx.beginPath();
      ctx.arc(p.x, p.y, r, 0, Math.PI * 2);
      ctx.clip();
      ctx.drawImage(blurCanvas, sx, sy, sw, sh, p.x - r, p.y - r, r * 2, r * 2);
      ctx.restore();
    }
    ctx.restore();
    return;
  }

  // 方块模式：马赛克块大小（CSS 像素，随画笔粗细变化）
  const bw = Math.max(3, blockSize);
  // 一次性读取整个位图数据（避免每个点一次 getImageData）
  const src = octx.getImageData(0, 0, imgW, imgH).data;
  // 收集需要绘制的区块（去重）
  const seen = new Set<string>();
  const blocks: { px: number; py: number; color: string }[] = [];
  for (const p of points) {
    const px = Math.floor(p.x / bw) * bw;
    const py = Math.floor(p.y / bw) * bw;
    const key = `${px},${py}`;
    if (seen.has(key)) continue;
    seen.add(key);
    // 换算到截图位图像素（CSS 坐标 → 相对选区 → 按比例映射）
    const relX = p.x - sel.x;
    const relY = p.y - sel.y;
    const sx = Math.min(imgW - 1, Math.max(0, Math.round((relX / sel.w) * imgW)));
    const sy = Math.min(imgH - 1, Math.max(0, Math.round((relY / sel.h) * imgH)));
    const idx = (sy * imgW + sx) * 4;
    blocks.push({
      px,
      py,
      color: `rgb(${src[idx]}, ${src[idx + 1]}, ${src[idx + 2]})`,
    });
  }
  // 不透明填充
  ctx.globalAlpha = 1;
  for (const b of blocks) {
    ctx.fillStyle = b.color;
    ctx.fillRect(b.px, b.py, bw, bw);
  }
}

// === 鼠标交互 ===
function onMouseDown(e: MouseEvent) {
  // 右键 = 退出截图（QQ 式），不恢复主窗口
  if (e.button === 2) {
    if (ocrActive.value) {
      closeOcr();
      return;
    }
    if (mode.value === "annotating") {
      // 标注模式：右键先回到选区模式
      mode.value = "selecting";
      capturedImg = null;
      imgLoaded.value = false;
      annotations.value = [];
      resetSelection();
      return;
    }
    cancelRegionSelect(false);
    return;
  }
  if (e.button !== 0) return;
  // 整屏快照直通等待期：屏蔽误拖选（selection 尚未就绪）
  if (mode.value === "loading" && loadingBlank && instantPending) return;
  if (ocrActive.value) return; // OCR 文字选择模式下不处理 canvas 事件
  const x = e.clientX;
  const y = e.clientY;
  dragStart.x = x;
  dragStart.y = y;
  selStart.x = selection.x;
  selStart.y = selection.y;
  selStart.w = selection.w;
  selStart.h = selection.h;

  if (mode.value === "selecting") {
    // 微信式：按下时仅记录位置；移动超过阈值才进入拖选，
    // 未拖动直接松开 = 单击识别窗口
    mouseDown.value = true;
    downPos = { x, y };
    pendingWindow = hoveredWindow.value;
    return;
  }

  // 标注模式
  if (activeTool.value !== "select") {
    // 标注工具：在选区内绘制
    if (isInSelection(x, y)) {
      startDrawing(x, y);
    } else {
      // 选区外点击 → 重新选区
      mode.value = "selecting";
      annotations.value = [];
      redoStack.value = [];
      capturedImg = null;
      imgLoaded.value = false;
      startSelection(x, y);
    }
    return;
  }

  // select 工具：调整选区
  const handle = getHandleAt(x, y);
  if (handle) {
    isResizing.value = true;
    resizeHandle.value = handle as any;
    return;
  }
  if (isInSelection(x, y)) {
    isMovingSelection.value = true;
    return;
  }
  // 选区外 → 重新选区
  mode.value = "selecting";
  annotations.value = [];
  redoStack.value = [];
  capturedImg = null;
  imgLoaded.value = false;
  startSelection(x, y);
}

function startSelection(x: number, y: number) {
  isSelecting.value = true;
  selection.x = x;
  selection.y = y;
  selection.w = 0;
  selection.h = 0;
  draw();
}

function startDrawing(x: number, y: number) {
  const a: Annotation = {
    tool: activeTool.value,
    color: penColor.value,
    size: penSize.value,
  };
  if (activeTool.value === "text") {
    // 文字标注记录当前字体设置
    a.fontSize = fontSize.value;
    a.fontFamily = fontFamily.value;
  }
  if (activeTool.value === "pen" || activeTool.value === "mosaic") {
    a.points = [{ x, y }];
    if (activeTool.value === "mosaic" && mosaicMode.value === "gaussian") {
      a.gaussian = true;
    }
  } else {
    a.start = { x, y };
    a.end = { x, y };
  }
  drawingAnnotation.value = a;
  isDrawing.value = true;
}

function onMouseMove(e: MouseEvent) {
  const x = e.clientX;
  const y = e.clientY;
  // 放大镜跟踪（选择模式下持续重绘由 animate 循环承担）
  mouseIn.value = true;
  mousePos.x = x;
  mousePos.y = y;

  if (mode.value === "selecting" && !ocrActive.value) {
    if (mouseDown.value && !isSelecting.value) {
      // 按住未动 → 判断是否超过拖动阈值，是则进入自由拖选
      if (
        Math.abs(x - downPos.x) > DRAG_THRESHOLD ||
        Math.abs(y - downPos.y) > DRAG_THRESHOLD
      ) {
        hoveredWindow.value = null;
        dragStart.x = downPos.x;
        dragStart.y = downPos.y;
        startSelection(downPos.x, downPos.y);
      }
    } else if (!isSelecting.value) {
      // 未按下 → 更新悬停窗口高亮
      // 首次 mousemove（进入模式时系统补发）只作标记不识别，防止虚线框闪烁
      if (!windowRecognitionArmed) {
        windowRecognitionArmed = true;
        return;
      }
      const hit = windowAt(x, y);
      if (hit !== hoveredWindow.value) {
        hoveredWindow.value = hit;
      }
      return;
    }
  }

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
    if ((a.tool === "pen" || a.tool === "mosaic") && a.points) {
      a.points.push({ x, y });
    } else {
      a.end = { x, y };
    }
    draw();
    return;
  }
  updateCursor(x, y);
}

async function onMouseUp(e: MouseEvent) {
  if (e.button !== 0) return;
  if (ocrActive.value) return; // OCR 文字选择模式下不处理 canvas 事件

  if (mode.value === "selecting" && mouseDown.value) {
    mouseDown.value = false;
    if (isSelecting.value) {
      // 拖选结束
      isSelecting.value = false;
      if (selection.w > 5 && selection.h > 5) {
        // 选区确定 → 截取图像并进入标注模式
        await enterAnnotationMode();
      } else {
        resetSelection();
      }
      return;
    }
    // 未拖动 = 单击 → 识别并截取整个窗口
    if (pendingWindow && pendingWindow.w > 5 && pendingWindow.h > 5) {
      selection.x = pendingWindow.x;
      selection.y = pendingWindow.y;
      selection.w = pendingWindow.w;
      selection.h = pendingWindow.h;
      hoveredWindow.value = null;
      pendingWindow = null;
      // 点击吸附：加载期留白，避免异常缩小的虚线框，直接过渡到标注
      await enterAnnotationMode(true);
    } else {
      pendingWindow = null;
    }
    return;
  }

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
      // 选区确定 → 截取图像并进入标注模式
      await enterAnnotationMode();
    } else {
      resetSelection();
    }
    return;
  }
  if (isDrawing.value) {
    isDrawing.value = false;
    const a = drawingAnnotation.value;
    drawingAnnotation.value = null;
    if (!a) return;

    if (a.tool === "text" && a.start && a.end) {
      // 在鼠标点击位置弹出文字输入框（应用当前字体/字号设置）
      const text = await inputText(a.start.x, a.start.y);
      if (text) {
        a.text = text;
        annotations.value.push(a);
        redoStack.value = [];
        draw();
      }
      return;
    }
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
  if (mode.value === "selecting") {
    canvasRef.value.style.cursor = "crosshair";
  } else if (activeTool.value === "select") {
    if (getHandleAt(x, y)) {
      canvasRef.value.style.cursor = "nwse-resize";
    } else if (isInSelection(x, y)) {
      canvasRef.value.style.cursor = "move";
    } else {
      canvasRef.value.style.cursor = "crosshair";
    }
  } else {
    canvasRef.value.style.cursor = "crosshair";
  }
}

function resetSelection() {
  selection.x = 0;
  selection.y = 0;
  selection.w = 0;
  selection.h = 0;
  isSelecting.value = false;
  mouseDown.value = false;
  pendingWindow = null;
  draw();
}

// === 整屏快照直通标注（annotation_capture_mode="fullscreen"） ===
// show(instant) → loading → instant-annotate(base64) → annotating，跳过拖选
let instantPending = false;
let instantTimer: number | null = null;
function clearInstantPending() {
  instantPending = false;
  if (instantTimer !== null) {
    clearTimeout(instantTimer);
    instantTimer = null;
  }
}

// 进入标注模式（截图底图就绪后统一入口；区域/整屏两路径共用）
function startAnnotate(img: HTMLImageElement, path: string) {
  capturedImg = img;
  capturedImgPath = path;
  imgLoaded.value = true;
  mode.value = "annotating";
  loadingBlank = false;
  activeTool.value = "pen";
  annotations.value = [];
  redoStack.value = [];
  draw();
}

// 彻底清空所有状态和 canvas（退出时调用，避免下次显示闪烁旧内容）
function clearAll() {
  // 先隐藏 canvas，避免显示旧内容
  canvasReady.value = false;
  selection.x = 0;
  selection.y = 0;
  selection.w = 0;
  selection.h = 0;
  isSelecting.value = false;
  isDrawing.value = false;
  isMovingSelection.value = false;
  isResizing.value = false;
  mode.value = "selecting";
  activeTool.value = "pen";
  annotations.value = [];
  redoStack.value = [];
  drawingAnnotation.value = null;
  capturedImg = null;
  imgLoaded.value = false;
  capturedImgPath = "";
  // 预截图背景（下次进入时重新加载）
  bgImg = null;
  bgImgLoaded = false;
  bgLoadPromise = null;
  loadingBlank = false;
  // 整屏直通等待态复位
  clearInstantPending();
  // 窗口识别状态
  hoveredWindow.value = null;
  mouseDown.value = false;
  pendingWindow = null;
  // OCR 状态
  ocrActive.value = false;
  ocrLines.value = [];
  ocrLoading.value = false;
  // 工具栏状态（拖动位置 / 收起）复位
  toolbarDragPos.value = null;
  toolbarCollapsed.value = false;
  // 立即清空 canvas
  if (ctx && canvasRef.value) {
    ctx.clearRect(0, 0, canvasRef.value.width, canvasRef.value.height);
  }
  // 下一帧再显示 canvas，确保旧内容已清除
  requestAnimationFrame(() => {
    canvasReady.value = true;
  });
}

// === 进入标注模式：截取选区图像 ===
// blankLoading=true（点击吸附窗口）：加载期不绘制中间虚线框，直接过渡到标注
//
// 截图来源：**确认瞬间实时截取**（后端 DXGI 拿当前帧）。
// 覆盖层已 WDA_EXCLUDEFROMCAPTURE 不入镜，游戏等动态内容与所见一致；
// 预截图仅用于选择期的视觉冻结与放大镜，实时截取失败时才回退本地裁剪。
async function enterAnnotationMode(blankLoading = false) {
  mode.value = "loading";
  loadingBlank = blankLoading;
  draw();

  const dpr = window.devicePixelRatio || 1;
  const physX = Math.round(selection.x * dpr);
  const physY = Math.round(selection.y * dpr);
  const physW = Math.round(selection.w * dpr);
  const physH = Math.round(selection.h * dpr);

  // 1) 实时截取（确认瞬间画面）：后端 base64 直传，零文件 IO
  try {
    const b64 = await invoke<string>("capture_region_for_annotation", {
      x: physX,
      y: physY,
      w: physW,
      h: physH,
    });
    const dataUrl = `data:image/png;base64,${b64}`;
    flog(`实时截图 base64 直传: ${Math.round(b64.length / 1024)}KB`);

    const img = await new Promise<HTMLImageElement>((res, rej) => {
      const im = new Image();
      im.onload = () => res(im);
      im.onerror = () => rej(new Error("image decode fail"));
      im.src = dataUrl;
    });
    // OCR 走后端异步落盘的临时文件（就绪时间略晚于标注显示，属预期）
    startAnnotate(img, "");
    flog("实时截图完成，进入标注模式（内存直传）");
    return;
  } catch (e) {
    flog(`实时截图失败，回退预截图本地裁剪: ${e}`);
  }

  // 2) 兜底：从预截图本地裁剪（热键瞬间画面）
  if (!bgImgLoaded && bgLoadPromise) {
    const ok = await waitForBg(1000);
    if (!ok) flog("预截图等待超时");
  }
  if (bgImgLoaded && bgImg) {
    try {
      // 离屏裁剪（物理像素 1:1）
      const off = document.createElement("canvas");
      off.width = physW;
      off.height = physH;
      const octx = off.getContext("2d");
      if (!octx) throw new Error("无法创建离屏 canvas");
      octx.drawImage(bgImg, physX, physY, physW, physH, 0, 0, physW, physH);
      const dataUrl = off.toDataURL("image/png");
      const base64 = dataUrl.split(",")[1] || "";

      const img = await new Promise<HTMLImageElement>((res, rej) => {
        const im = new Image();
        im.onload = () => res(im);
        im.onerror = () => rej(new Error("image decode fail"));
        im.src = dataUrl;
      });
      startAnnotate(img, "");
      flog("本地裁剪完成，进入标注模式（兜底）");

      // 异步写临时文件供 OCR 使用（不阻塞标注显示）
      (async () => {
        try {
          const { writeFile } = await import("@tauri-apps/plugin-fs");
          const b = atob(base64);
          const arr = new Uint8Array(b.length);
          for (let i = 0; i < b.length; i++) arr[i] = b.charCodeAt(i);
          const tmp = await invoke<string>("get_temp_dir");
          const p = `${tmp}\\jietu-hdr\\region_annotation.png`;
          await writeFile(p, arr);
          capturedImgPath = p;
        } catch (e) {
          flog(`写入标注临时文件失败（OCR 可能不可用）: ${e}`);
        }
      })();
      return;
    } catch (e) {
      flog(`本地裁剪失败: ${e}`);
    }
  }

  // 3) 全部失败：返回选择模式
  flog("截图全部失败，返回选择模式");
  mode.value = "selecting";
  loadingBlank = false;
  resetSelection();
}

// Blob 转 data URL
function blobToDataURL(blob: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result as string);
    reader.onerror = reject;
    reader.readAsDataURL(blob);
  });
}

// === OCR 文字识别 ===
// 识别截图中的文字，映射到选区位置显示为可选中复制的文字层
// 语言跟随配置（空 = 自动选择）
let ocrLang = "";

// 实时截图 base64 直传后，OCR 临时文件由后端异步落盘；
// 触发 OCR 时若路径未就绪则轮询等待（盘写通常几十 ms 完成）
async function ensureOcrPath(): Promise<string> {
  if (capturedImgPath) return capturedImgPath;
  const { exists } = await import("@tauri-apps/plugin-fs");
  const tmp = await invoke<string>("get_temp_dir");
  const p = `${tmp}\\jietu-hdr\\region_annotation.png`;
  for (let i = 0; i < 20; i++) {
    if (await exists(p)) {
      capturedImgPath = p;
      return p;
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  return "";
}

async function runOcr() {
  const path = await ensureOcrPath();
  if (!path || ocrLoading.value) return;
  ocrLoading.value = true;
  flog(`OCR 开始: ${path}`);
  try {
    // 读取配置语言（缓存；设置在主面板保存后下次截图生效）
    try {
      const cfg = await invoke<{ ocr_language: string }>("get_config");
      ocrLang = cfg.ocr_language || "";
    } catch {
      // 保持默认
    }
    const lines = await invoke<
      { text: string; x: number; y: number; w: number; h: number }[]
    >("ocr_image", { pngPath: path, language: ocrLang || null });
    // 图像像素坐标 → CSS 像素（映射到选区位置）
    const imgW = capturedImg?.naturalWidth || 1;
    const imgH = capturedImg?.naturalHeight || 1;
    ocrLines.value = lines
      .filter((l) => l.text.trim() && l.w > 0 && l.h > 0)
      .map((l) => ({
        text: l.text,
        x: selection.x + (l.x / imgW) * selection.w,
        y: selection.y + (l.y / imgH) * selection.h,
        w: (l.w / imgW) * selection.w,
        h: (l.h / imgH) * selection.h,
      }));
    ocrActive.value = true;
    flog(`OCR 完成: ${ocrLines.value.length} 行`);
    draw();
  } catch (e) {
    flog(`OCR 失败: ${e}`);
  } finally {
    ocrLoading.value = false;
  }
}

// 退出文字识别模式
function closeOcr() {
  ocrActive.value = false;
  ocrLines.value = [];
  window.getSelection()?.removeAllRanges();
  draw();
}

// 复制 OCR 文字
// all=true 复制全部；否则优先复制鼠标选中的文字（无选中时复制全部）
async function copyOcrText(all: boolean) {
  let text = "";
  if (all) {
    text = ocrLines.value.map((l) => l.text).join("\n");
  } else {
    text = window.getSelection()?.toString() || "";
    if (!text) {
      text = ocrLines.value.map((l) => l.text).join("\n");
    }
  }
  if (!text.trim()) {
    flog("OCR 复制: 无文字");
    return;
  }
  try {
    const { writeText } = await import("@tauri-apps/plugin-clipboard-manager");
    await writeText(text);
    flog(`OCR 文字已复制 (${text.length} 字符)`);
    await cancelRegionSelect();
  } catch (e) {
    flog(`复制文字失败: ${e}`);
  }
}

// === 工具栏操作 ===
function selectTool(t: Tool) {
  activeTool.value = t;
  draw();
}

function undo() {
  flog(`undo 被调用, annotations 数量=${annotations.value.length}`);
  if (annotations.value.length === 0) return;
  const a = annotations.value.pop();
  if (a) redoStack.value.push(a);
  flog(`撤销后 annotations 数量=${annotations.value.length}`);
  draw();
}

function redo() {
  if (redoStack.value.length === 0) return;
  const a = redoStack.value.pop();
  if (a) annotations.value.push(a);
  draw();
}

// === 导出选区（原始截图 + 仅标注，不含 UI 装饰）===
// 注意：不能直接裁剪主 canvas——其中还画了选区虚线框、控制点、
// 半透遮罩等 UI 装饰，裁剪会把虚线框带进最终图像。
// 正确做法：新画布上重画原始截图，再叠加标注（坐标平移到选区原点）。
function exportSelectionCanvas(): HTMLCanvasElement | null {
  flog(`exportSelectionCanvas: sel=${JSON.stringify({ ...selection })} img=${!!capturedImg}`);
  if (selection.w < 2 || selection.h < 2 || !capturedImg) {
    flog("导出失败: 选区太小或截图未加载");
    return null;
  }
  const out = document.createElement("canvas");
  // 以物理像素分辨率导出（高 DPI 下保持原生清晰度，贴图/保存均 1:1 无重采样）
  const dpr = window.devicePixelRatio || 1;
  out.width = Math.round(selection.w * dpr);
  out.height = Math.round(selection.h * dpr);
  const octx = out.getContext("2d");
  if (!octx) return null;
  // 之后按 CSS 坐标绘制，物理分辨率由 scale 保证
  octx.scale(dpr, dpr);
  // 1. 原始截图铺满（截图即选区大小，1:1）
  octx.drawImage(capturedImg, 0, 0, selection.w, selection.h);
  // 2. 仅叠加标注（标注坐标是全屏 CSS 坐标 → 平移到选区原点）
  octx.translate(-selection.x, -selection.y);
  for (const a of annotations.value) {
    drawAnnotation(octx, a);
  }
  flog(`导出 canvas 尺寸(物理): ${out.width}x${out.height} dpr=${dpr} 标注数=${annotations.value.length}`);
  return out;
}

// === 复制到剪贴板 ===
async function copyToClipboard() {
  flog("copyToClipboard 被调用");
  const out = exportSelectionCanvas();
  if (!out) return;
  const dataUrl = out.toDataURL("image/png");
  const base64 = dataUrl.split(",")[1] || "";
  flog(`base64 长度: ${base64.length}`);
  flog("准备调用 commit_annotation...");
  try {
    const result = await invoke("commit_annotation", {
      pngData: base64,
      autoSave: false,
      copyToClipboard: true,
      hasAnnotations: annotations.value.length > 0,
    });
    flog(`复制成功 result=${result}，取消区域选择`);
    await cancelRegionSelect();
  } catch (e) {
    flog(`复制失败: ${e}`);
  }
}

// === 保存（自动保存到配置目录）===
async function quickSave() {
  flog("quickSave 被调用");
  const out = exportSelectionCanvas();
  if (!out) return;
  const dataUrl = out.toDataURL("image/png");
  const base64 = dataUrl.split(",")[1] || "";
  flog("准备调用 commit_annotation (save)...");
  try {
    const result = await invoke("commit_annotation", {
      pngData: base64,
      autoSave: true,
      copyToClipboard: false,
      hasAnnotations: annotations.value.length > 0,
    });
    flog(`保存成功 result=${result}`);
    await cancelRegionSelect();
  } catch (e) {
    flog(`保存失败: ${e}`);
  }
}

// === 另存为（弹出文件选择对话框，格式跟随配置）===
async function saveAs() {
  flog("saveAs 被调用");
  const out = exportSelectionCanvas();
  if (!out) return;
  const dataUrl = out.toDataURL("image/png");
  const base64 = dataUrl.split(",")[1] || "";

  // 读取配置的输出格式（与全屏截图一致）
  let ext = "png";
  let filterName = "PNG 图像";
  try {
    const cfg = await invoke<{ output_format: string }>("get_config");
    const fmt = cfg.output_format;
    if (fmt === "Exr") {
      ext = "exr";
      filterName = "OpenEXR 图像";
    } else if (fmt === "PngHdr") {
      ext = "png";
      filterName = "HDR PNG 图像（BT.2020/PQ）";
    } else if (fmt === "Jxl") {
      ext = "jxl";
      filterName = "HDR JPEG XL 图像（BT.2020/PQ）";
    } else if (fmt === "Jxr") {
      ext = "jxr";
      filterName = "HDR JPEG XR 图像（scRGB）";
    } else if (fmt === "Avif") {
      ext = "avif";
      filterName = "AVIF 图像";
    }
  } catch (e) {
    flog(`读取配置失败（使用 PNG）: ${e}`);
  }

  // 弹出文件保存对话框
  const { save } = await import("@tauri-apps/plugin-dialog");
  const filePath = await save({
    title: "另存为",
    defaultPath: `jietu_${Date.now()}.${ext}`,
    filters: [{ name: filterName, extensions: [ext] }],
  });

  flog(`用户选择的路径: ${filePath}`);
  if (!filePath) {
    flog("用户取消了保存");
    return;
  }

  // 交给后端按配置格式转换并写入（EXR/HDR PNG 在后端编码）
  try {
    const saved = await invoke<string>("save_annotation_as", {
      pngData: base64,
      path: filePath,
      hasAnnotations: annotations.value.length > 0,
    });
    flog(`保存成功: ${saved}`);
    await cancelRegionSelect();
  } catch (e) {
    flog(`保存失败: ${e}`);
  }
}

// === 贴图 ===
async function pinToDesktop() {
  flog("pinToDesktop 被调用");
  const out = exportSelectionCanvas();
  if (!out) return;
  const dataUrl = out.toDataURL("image/png");
  const base64 = dataUrl.split(",")[1] || "";
  flog(`贴图 base64 长度: ${base64.length}`);
  try {
    const { writeFile, BaseDirectory } = await import("@tauri-apps/plugin-fs");
    const fileName = `pin_${Date.now()}.png`;
    const bytes = Uint8Array.from(atob(base64), (c) => c.charCodeAt(0));
    await writeFile(`jietu-hdr/${fileName}`, bytes, {
      baseDir: BaseDirectory.Temp,
    });

    // 获取真实临时目录路径（后端已去除尾部反斜杠）
    const tempPath = await invoke<string>("get_temp_dir");
    const fullPath = `${tempPath}\\jietu-hdr\\${fileName}`;
    flog(`贴图临时文件: ${fullPath}`);

    // 复制选区参数（下面 clearAll 会清空 selection）
    const pinX = selection.x;
    const pinY = selection.y;
    const pinW = selection.w;
    const pinH = selection.h;
    const pinPath = fullPath;

    // 先清空前端状态（不清空也行，但避免下次显示闪烁旧内容）
    clearAll();

    // 调用后端组合命令：一次性完成"隐藏 region-select + 创建贴图窗口 + 显示主窗口"
    // 避免前端多次异步 invoke 导致的窗口 z-order 卡死
    flog("贴图：调用 pin_screenshot 组合命令");
    await invoke("pin_screenshot", {
      imagePath: pinPath,
      x: pinX,
      y: pinY,
      w: pinW,
      h: pinH,
    });
    flog("贴图窗口已打开");
  } catch (e) {
    flog(`贴图失败: ${e}`);
    // 失败时也要尝试退出区域选择模式，避免卡死
    try {
      await invoke("cancel_region_select", { showMain: true });
    } catch {}
  }
}

// === 键盘快捷键 ===
function onKeyDown(e: KeyboardEvent) {
  if (e.key === "Escape") {
    if (ocrActive.value) {
      // 先退出文字识别模式
      closeOcr();
      return;
    }
    if (isDrawing.value && drawingAnnotation.value) {
      drawingAnnotation.value = null;
      isDrawing.value = false;
      draw();
    } else if (mode.value === "annotating") {
      // 回到选区模式
      mode.value = "selecting";
      capturedImg = null;
      imgLoaded.value = false;
      annotations.value = [];
      resetSelection();
    } else if (isSelecting.value) {
      resetSelection();
    } else {
      // ESC 取消截图：不恢复主窗口
      cancelRegionSelect(false);
    }
  } else if ((e.ctrlKey || e.metaKey) && e.key === "c" && mode.value === "annotating") {
    e.preventDefault();
    // OCR 模式下复制选中的文字，否则复制图像
    if (ocrActive.value) {
      copyOcrText(false);
    } else {
      copyToClipboard();
    }
  } else if (e.key === "Enter" && mode.value === "annotating") {
    if (ocrActive.value) {
      copyOcrText(false);
    } else {
      copyToClipboard();
    }
  } else if (ocrActive.value) {
    // OCR 模式下放行文字编辑相关按键（如 Ctrl+A 全选），屏蔽截图快捷键
    if ((e.ctrlKey || e.metaKey) && ["z", "s"].includes(e.key.toLowerCase())) {
      e.preventDefault();
    }
    return;
  } else if ((e.ctrlKey || e.metaKey) && e.key === "z") {
    e.preventDefault();
    if (e.shiftKey) redo();
    else undo();
  } else if ((e.ctrlKey || e.metaKey) && e.key === "s") {
    e.preventDefault();
    quickSave();
  }
}

async function cancelRegionSelect(showMain = true) {
  // 先清空所有状态和 canvas，避免下次显示时闪烁旧内容
  clearAll();
  try {
    await invoke("cancel_region_select", { showMain });
  } catch (e) {
    console.error("取消失败:", e);
    await getCurrentWindow().hide();
  }
}

// === 工具栏位置 ===
// 工具栏宽度估算（文字工具多字体控件、OCR 模式多操作按钮时更宽）
const toolbarWidth = computed(() => {
  let w = 580;
  if (activeTool.value === "text") w += 170;
  if (ocrActive.value) w += 200;
  return w;
});

// === 工具栏拖动 / 收起（5.7 / 5.9） ===
// toolbarDragPos：用户拖动后的固定位置（优先于自动计算）
const toolbarDragPos = ref<{ top: number; left: number } | null>(null);
const toolbarCollapsed = ref(false);
const collapsedPos = ref({ top: 0, left: 0 });

function onToolbarMouseDown(e: MouseEvent) {
  // 按钮等控件上的按下不触发拖动
  const t = e.target as HTMLElement;
  if (t.closest("button, input, select")) return;
  const base = { ...toolbarPos.value };
  const mx = e.clientX;
  const my = e.clientY;
  const onMove = (ev: MouseEvent) => {
    toolbarDragPos.value = {
      top: Math.max(
        0,
        Math.min(window.innerHeight - 48, base.top + (ev.clientY - my)),
      ),
      left: Math.max(
        0,
        Math.min(window.innerWidth - 120, base.left + (ev.clientX - mx)),
      ),
    };
  };
  const onUp = () => {
    window.removeEventListener("mousemove", onMove);
    window.removeEventListener("mouseup", onUp);
  };
  window.addEventListener("mousemove", onMove);
  window.addEventListener("mouseup", onUp);
}

function collapseToolbar() {
  collapsedPos.value = { ...toolbarPos.value };
  toolbarCollapsed.value = true;
}

function expandToolbar() {
  toolbarCollapsed.value = false;
}

const toolbarPos = computed(() => {
  // 用户拖动后的位置优先
  if (toolbarDragPos.value) return toolbarDragPos.value;
  if (mode.value !== "annotating" || selection.h <= 0) {
    return {
      top: window.innerHeight - 80,
      left: Math.max(8, window.innerWidth / 2 - toolbarWidth.value / 2),
    };
  }
  const top = selection.y + selection.h + 8;
  const left = Math.max(
    8,
    Math.min(
      window.innerWidth - toolbarWidth.value - 8,
      selection.x + selection.w - toolbarWidth.value + 10,
    ),
  );
  // 下方放不下（全屏/贴底选区）→ 屏幕下方居中偏上
  // （距底 ~96px：避开屏幕最底边，视觉上更舒适）
  if (top + 44 > window.innerHeight - 8) {
    return {
      top: window.innerHeight - 96,
      left: Math.max(8, window.innerWidth / 2 - toolbarWidth.value / 2),
    };
  }
  return { top, left };
});

// === 动画循环 ===
let animFrame = 0;
// 心跳：渲染循环活着才上报（Rust 看门狗发现心跳停止 8s 会强制收起覆盖层，
// 防止渲染进程崩溃后全屏冻结画面吞掉系统键盘/鼠标）
let lastHeartbeat = 0;
function sendHeartbeat() {
  const now = performance.now();
  if (now - lastHeartbeat >= 1000) {
    lastHeartbeat = now;
    invoke("region_heartbeat").catch(() => {});
  }
}

function animate() {
  sendHeartbeat();
  // loading 模式也持续重绘，保持虚线框蚂蚁线动画不中断
  if (mode.value === "selecting" || mode.value === "loading") {
    if (
      isSelecting.value ||
      selection.w > 0 ||
      hoveredWindow.value ||
      mouseIn.value
    ) {
      draw();
    }
  } else if (mode.value === "annotating") {
    draw();
  }
  animFrame = requestAnimationFrame(animate);
}

// === 生命周期 ===
let unlistenShow: (() => void) | null = null;
let unlistenBg: (() => void) | null = null;
let unlistenInstant: (() => void) | null = null;

onMounted(async () => {
  initCanvas();
  window.addEventListener("keydown", onKeyDown);
  window.addEventListener("resize", onResize);
  animate();

  // 读取截图行为开关（弹工具栏 / 贴图）
  try {
    const cfg = await invoke<{ show_toolbar: boolean; enable_pin: boolean }>(
      "get_config",
    );
    cfgShowToolbar.value = cfg.show_toolbar !== false;
    cfgEnablePin.value = cfg.enable_pin !== false;
  } catch {
    // 读取失败保持默认
  }

  try {
    unlistenShow = await listen("region-select://show", (ev) => {
      // 整屏快照直通标注：show 事件 payload 携带 { instant } 标记
      const instant =
        (ev.payload as { instant?: boolean } | null)?.instant === true;
      // 确保键盘焦点在覆盖层（Esc 取消依赖 keydown；焦点丢失会导致无法退出）
      window.focus();
      // 彻底清空所有状态（避免闪烁上次截图）
      clearAll();
      initCanvas();
      // 重置窗口识别：首次 mousemove 只作标记不高亮
      windowRecognitionArmed = false;
      hoveredWindow.value = null;
      if (instant) {
        // 整屏快照：直接进 loading（透明无 UI），跳过拖选；跳过 loadWindows 省一次 IPC
        mode.value = "loading";
        loadingBlank = true;
        instantPending = true;
        // 兜底：事件丢失（如首次 webview 未就绪）4s 后回落选择模式
        instantTimer = window.setTimeout(() => {
          if (instantPending) {
            flog("整屏快照等待超时，回退选择模式");
            clearInstantPending();
            loadingBlank = false;
            mode.value = "selecting";
          }
        }, 4000);
        return;
      }
      // 加载窗口列表用于自动窗口识别（微信/QQ 式）
      loadWindows();
    });
    // 后台预截图就绪 → 加载冻结背景（选择中途画面"冻结"，视觉无缝）
    unlistenBg = await listen<string>("region-select://bg-ready", (ev) => {
      startBgLoad(ev.payload || "");
    });
    // 整屏快照直通 → 底图就绪（base64 直传；空 image = 后端捕获失败）
    unlistenInstant = await listen<{ image: string }>(
      "region-select://instant-annotate",
      async (ev) => {
        if (!instantPending) return; // 已取消/超时，丢弃迟到帧
        clearInstantPending();
        const b64 = ev.payload?.image || "";
        if (!b64) {
          // 后端捕获失败 → 回落选择模式（透明实时层可正常拖选）
          flog("整屏快照失败，回退选择模式");
          loadingBlank = false;
          mode.value = "selecting";
          return;
        }
        try {
          const img = await new Promise<HTMLImageElement>((res, rej) => {
            const im = new Image();
            im.onload = () => res(im);
            im.onerror = () => rej(new Error("image decode fail"));
            im.src = `data:image/png;base64,${b64}`;
          });
          // 选区 = 整屏（CSS 坐标）：导出/工具栏/OCR 坐标映射全部基于 selection，自动适配
          selection.x = 0;
          selection.y = 0;
          selection.w = window.innerWidth;
          selection.h = window.innerHeight;
          startAnnotate(img, "");
          flog("整屏快照就绪，进入标注模式（内存直传）");
        } catch (e) {
          flog(`整屏快照解码失败，回退选择模式: ${e}`);
          loadingBlank = false;
          mode.value = "selecting";
        }
      },
    );
  } catch (e) {
    console.warn("无法监听显示事件:", e);
  }
});

onUnmounted(() => {
  window.removeEventListener("keydown", onKeyDown);
  window.removeEventListener("resize", onResize);
  cancelAnimationFrame(animFrame);
  unlistenShow?.();
  unlistenBg?.();
  unlistenInstant?.();
  clearInstantPending();
});

function onResize() {
  if (canvasRef.value && ctx) {
    const dpr = window.devicePixelRatio || 1;
    canvasRef.value.width = Math.round(window.innerWidth * dpr);
    canvasRef.value.height = Math.round(window.innerHeight * dpr);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    draw();
  }
}
</script>

<template>
  <div class="region-select-root" :class="{ 'annotating': mode === 'annotating' }" @contextmenu.prevent>
    <canvas
      ref="canvasRef"
      class="select-canvas"
      v-show="canvasReady"
      @mousedown="onMouseDown"
      @mousemove="onMouseMove"
      @mouseup="onMouseUp"
      @mouseleave="mouseIn = false"
    />

    <!-- 顶部提示 -->
    <div v-if="mode === 'selecting'" class="hint-bar">
      <span>点击识别窗口 · 拖拽选择区域</span>
      <span class="hint-key">ESC 取消</span>
    </div>
    <div v-else-if="mode === 'loading'" class="hint-bar">
      <span>正在截取选区...</span>
    </div>
    <div v-else class="hint-bar">
      <span>{{ ocrActive ? '拖动鼠标选择要复制的文字' : '选区内标注 · 工具栏在选区下方' }}</span>
      <span class="hint-key">{{ ocrActive ? 'Ctrl+C 复制选中 · ESC 退出识别' : 'ESC 退出 · Ctrl+Z 撤销 · Enter 复制' }}</span>
    </div>

    <!-- 完整标注工具栏（可拖动 / 可收起为小圆点） -->
    <div
      v-if="mode === 'annotating' && cfgShowToolbar && !toolbarCollapsed"
      class="toolbar"
      :style="{ top: toolbarPos.top + 'px', left: toolbarPos.left + 'px' }"
      @mousedown.stop="onToolbarMouseDown"
      @mouseup.stop
    >
      <!-- 工具按钮组 -->
      <div class="tool-group">
        <button class="tool-btn" :class="{ active: activeTool === 'select' }" title="选区" @click="selectTool('select')">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M5 3L5 17L9 13L12 20L14 19L11 12L17 12L5 3Z" />
          </svg>
        </button>
        <button class="tool-btn" :class="{ active: activeTool === 'pen' }" title="画笔" @click="selectTool('pen')">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M12 19l7-7 3 3-7 7-3-3z" />
            <path d="M18 13l-1.5-7.5L2 2l3.5 14.5L13 18l5-5z" />
            <path d="M2 2l7.586 7.586" />
            <circle cx="11" cy="11" r="2" />
          </svg>
        </button>
        <button class="tool-btn" :class="{ active: activeTool === 'text' }" title="文字" @click="selectTool('text')">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <polyline points="4 7 4 4 20 4 20 7" />
            <line x1="9" y1="20" x2="15" y2="20" />
            <line x1="12" y1="4" x2="12" y2="20" />
          </svg>
        </button>
        <button class="tool-btn" :class="{ active: activeTool === 'arrow' }" title="箭头" @click="selectTool('arrow')">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <line x1="5" y1="19" x2="19" y2="5" />
            <polyline points="9 5 19 5 19 15" />
          </svg>
        </button>
        <button class="tool-btn" :class="{ active: activeTool === 'rect' }" title="矩形" @click="selectTool('rect')">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <rect x="4" y="6" width="16" height="12" rx="1" />
          </svg>
        </button>
        <button class="tool-btn" :class="{ active: activeTool === 'circle' }" title="圆圈" @click="selectTool('circle')">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <circle cx="12" cy="12" r="8" />
          </svg>
        </button>
        <button class="tool-btn" :class="{ active: activeTool === 'mosaic' }" title="马赛克" @click="selectTool('mosaic')">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <rect x="3" y="3" width="7" height="7" />
            <rect x="14" y="3" width="7" height="7" />
            <rect x="3" y="14" width="7" height="7" />
            <rect x="14" y="14" width="7" height="7" />
          </svg>
        </button>
        <!-- 马赛克双模式：方块 / 高斯模糊（5.12） -->
        <template v-if="activeTool === 'mosaic'">
          <button
            class="tool-btn"
            :class="{ active: mosaicMode === 'block' }"
            title="方块马赛克"
            @click="mosaicMode = 'block'"
          >
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <rect x="4" y="4" width="7" height="7" rx="1" />
              <rect x="13" y="13" width="7" height="7" rx="1" />
              <rect x="13" y="4" width="7" height="7" rx="1" fill="currentColor" stroke="none" />
            </svg>
          </button>
          <button
            class="tool-btn"
            :class="{ active: mosaicMode === 'gaussian' }"
            title="高斯模糊"
            @click="mosaicMode = 'gaussian'"
          >
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M12 3s6 6.5 6 11a6 6 0 0 1-12 0c0-4.5 6-11 6-11z" />
              <path d="M9.5 14a2.5 2.5 0 0 0 2.5 2.5" />
            </svg>
          </button>
        </template>
      </div>

      <div class="divider" />

      <!-- 文字工具：字体和字号选择 -->
      <template v-if="activeTool === 'text'">
        <div class="font-group">
          <select class="font-select" v-model="fontFamily" title="字体">
            <option v-for="f in fontFamilyOptions" :key="f.label" :value="f.value">{{ f.label }}</option>
          </select>
          <select class="font-select font-size" v-model.number="fontSize" title="字号">
            <option v-for="s in fontSizeOptions" :key="s" :value="s">{{ s }}px</option>
          </select>
        </div>
        <div class="divider" />
      </template>

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
        <input type="range" min="1" max="20" v-model.number="penSize" class="size-slider" />
        <div class="size-preview" :style="{ width: penSize + 'px', height: penSize + 'px', background: penColor }" />
      </div>

      <div class="divider" />

      <!-- 操作按钮 -->
      <div class="tool-group">
        <button class="tool-btn" title="撤销 (Ctrl+Z)" @click="undo">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <polyline points="1 4 1 10 7 10" />
            <path d="M3.51 15a9 9 0 1 0 2.13-9.36L1 10" />
          </svg>
        </button>
        <button class="tool-btn" title="复制到剪贴板 (Enter)" @click="copyToClipboard">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <rect x="9" y="9" width="13" height="13" rx="2" />
            <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" />
          </svg>
        </button>
        <button class="tool-btn" title="保存 (Ctrl+S)" @click="quickSave">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M19 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11l5 5v11a2 2 0 0 1-2 2z" />
            <polyline points="17 21 17 13 7 13 7 21" />
            <polyline points="7 3 7 8 15 8" />
          </svg>
        </button>
        <button class="tool-btn" title="另存为..." @click="saveAs">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" />
            <polyline points="14 2 14 8 20 8" />
            <line x1="12" y1="13" x2="12" y2="19" />
            <line x1="9" y1="16" x2="15" y2="16" />
          </svg>
        </button>
        <button v-if="cfgEnablePin" class="tool-btn" title="贴图" @click="pinToDesktop">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M12 17v5" />
            <path d="M9 10.76V6a2 2 0 0 1 4 0v4.76a3 3 0 1 1-4 0z" />
          </svg>
        </button>
        <button
          v-if="!ocrActive"
          class="tool-btn"
          :class="{ active: ocrLoading }"
          :title="ocrLoading ? '识别中...' : '文字识别（OCR）：识别图中的文字并选择复制'"
          @click="runOcr"
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M3 7V5a2 2 0 0 1 2-2h2" />
            <path d="M17 3h2a2 2 0 0 1 2 2v2" />
            <path d="M21 17v2a2 2 0 0 1-2 2h-2" />
            <path d="M7 21H5a2 2 0 0 1-2-2v-2" />
            <path d="M8 8h8" />
            <path d="M12 8v8" />
          </svg>
        </button>
        <!-- OCR 模式：文字操作 -->
        <template v-if="ocrActive">
          <div class="divider" />
          <div class="tool-group">
            <button class="tool-btn text-btn" title="复制鼠标选中的文字（无选中则复制全部）" @click="copyOcrText(false)">复制选中</button>
            <button class="tool-btn text-btn" title="复制全部识别文字" @click="copyOcrText(true)">复制全部</button>
            <button class="tool-btn" title="退出文字识别 (ESC)" @click="closeOcr">
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                <line x1="18" y1="6" x2="6" y2="18" />
                <line x1="6" y1="6" x2="18" y2="18" />
              </svg>
            </button>
          </div>
        </template>

        <!-- 收起工具栏（5.9：收为小圆点） -->
        <div class="divider" />
        <button class="tool-btn" title="收起工具栏" @click="collapseToolbar">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <polyline points="4 14 10 14 10 20" />
            <polyline points="20 10 14 10 14 4" />
            <line x1="14" y1="10" x2="21" y2="3" />
            <line x1="3" y1="21" x2="10" y2="14" />
          </svg>
        </button>
      </div>
    </div>

    <!-- 工具栏收起后的小圆点（点击展开） -->
    <div
      v-if="mode === 'annotating' && cfgShowToolbar && toolbarCollapsed"
      class="toolbar-dot"
      :style="{ top: collapsedPos.top + 'px', left: collapsedPos.left + 'px' }"
      title="展开工具栏"
      @click="expandToolbar"
    >
      <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <path d="M12 19l7-7 3 3-7 7-3-3z" />
        <path d="M18 13l-1.5-7.5L2 2l3.5 14.5L13 18l5-5z" />
        <path d="M2 2l7.586 7.586" />
        <circle cx="11" cy="11" r="2" />
      </svg>
    </div>

    <!-- OCR 文字选择层：覆盖在截图上方，文字透明但可选中复制 -->
    <div
      v-if="ocrActive && mode === 'annotating'"
      class="ocr-layer"
      :style="{
        left: selection.x + 'px',
        top: selection.y + 'px',
        width: selection.w + 'px',
        height: selection.h + 'px',
      }"
    >
      <div
        v-for="(l, i) in ocrLines"
        :key="i"
        class="ocr-line"
        :style="{
          left: l.x - selection.x + 'px',
          top: l.y - selection.y + 'px',
          width: l.w + 'px',
          height: l.h + 'px',
          fontSize: Math.max(10, l.h * 0.72) + 'px',
          lineHeight: l.h + 'px',
        }"
        :title="l.text"
      >{{ l.text }}</div>
    </div>
  </div>
</template>

<style scoped>
.region-select-root {
  position: fixed;
  inset: 0;
  user-select: none;
  overflow: hidden;
  background: rgba(0, 0, 0, 0.01);
}
.region-select-root.annotating {
  background: #1c1c20;
}

.select-canvas {
  position: absolute;
  inset: 0;
  /* canvas 是替换元素，inset 不拉伸；显式 100% 铺满视口（CSS 尺寸），
     位图为物理分辨率，浏览器按 dpr 渲染 → 1:1 原生清晰度 */
  width: 100%;
  height: 100%;
  cursor: crosshair;
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
  color: #f0e6f2;
  font-size: 12px;
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.2);
  z-index: 10;
  pointer-events: none;
}
.hint-key {
  opacity: 0.65;
  font-size: 11px;
}

/* 标注工具栏 */
.toolbar {
  position: fixed;
  display: flex;
  align-items: center;
  gap: 4px;
  height: 44px;
  padding: 0 8px;
  background: rgba(245, 240, 246, 0.82);
  backdrop-filter: blur(16px) saturate(1.6);
  border-radius: 12px;
  border: 1px solid rgba(200, 162, 200, 0.25);
  box-shadow: 0 6px 24px rgba(0, 0, 0, 0.18);
  z-index: 100;
  animation: toolbar-in 220ms ease-out;
  cursor: grab;
}
.toolbar:active {
  cursor: grabbing;
}
@keyframes toolbar-in {
  from { opacity: 0; transform: translateY(8px); }
  to { opacity: 1; transform: translateY(0); }
}

/* 工具栏收起后的小圆点 */
.toolbar-dot {
  position: fixed;
  width: 36px;
  height: 36px;
  display: flex;
  align-items: center;
  justify-content: center;
  border-radius: 50%;
  background: rgba(245, 240, 246, 0.86);
  backdrop-filter: blur(16px) saturate(1.6);
  border: 1px solid rgba(200, 162, 200, 0.35);
  box-shadow: 0 4px 16px rgba(0, 0, 0, 0.2);
  color: #6e6e7c;
  cursor: pointer;
  z-index: 100;
  animation: toolbar-in 220ms ease-out;
  transition: background-color 0.15s, color 0.15s;
}
.toolbar-dot:hover {
  background: rgba(200, 162, 200, 0.35);
  color: #2c2c34;
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
  color: #2c1f2a;
  cursor: pointer;
  border-radius: 6px;
  transition: background-color 0.15s;
  padding: 0;
}
.tool-btn:hover {
  background: rgba(180, 162, 184, 0.18);
}
.tool-btn.active {
  background: rgba(200, 162, 200, 0.25);
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
  border-color: var(--jb-primary);
  box-shadow: 0 0 0 2px color-mix(in srgb, var(--jb-primary) 30%, transparent);
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
  border: 2px solid var(--jb-primary);
  cursor: pointer;
}
.size-preview {
  border-radius: 50%;
  min-width: 4px;
  min-height: 4px;
  max-width: 20px;
  max-height: 20px;
}

/* 文字工具：字体/字号选择 */
.font-group {
  display: flex;
  align-items: center;
  gap: 4px;
  padding: 0 2px;
}
.font-select {
  height: 26px;
  border: 1px solid rgba(180, 162, 184, 0.35);
  border-radius: 6px;
  background: rgba(255, 255, 255, 0.65);
  color: #2c1f2a;
  font-size: 12px;
  padding: 0 4px;
  outline: none;
  cursor: pointer;
  max-width: 92px;
}
.font-select.font-size {
  max-width: 62px;
}

/* OCR 文字选择层 */
.ocr-layer {
  position: absolute;
  z-index: 50;
  cursor: text;
  /* 覆盖层仅覆盖选区：拦截鼠标做文字选择，不再触发 canvas 标注 */
}
.ocr-line {
  position: absolute;
  white-space: nowrap;
  overflow: visible;
  color: transparent;
  user-select: text;
  font-family: "Segoe UI Variable", "Segoe UI", "Microsoft YaHei", sans-serif;
}
.ocr-line::selection {
  background: rgba(200, 162, 200, 0.5);
  color: transparent;
}

/* OCR 模式文字按钮 */
.text-btn {
  width: auto;
  min-width: 52px;
  height: 26px;
  padding: 0 8px;
  font-size: 12px;
  white-space: nowrap;
}
</style>
