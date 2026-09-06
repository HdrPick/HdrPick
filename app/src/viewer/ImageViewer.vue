<script setup lang="ts">
// 图片显示主区：wheel 缩放（0.1×-8×，以指针为中心）+ pointer 拖拽平移 + 左右旋转 90° + 自适应 contain + 全屏切换
// 多操作方式（设计稿 5.1）：
// - 鼠标：滚轮缩放 / 侧键（后退/前进）翻页 / 右键快捷菜单
// - 触控板：双指捏合（ctrl+wheel）缩放 / 双指横滑（wheel deltaX）切图
// - 触屏：双指捏合缩放 / 横向轻扫切换 / 长按呼出菜单（浏览器长按触发 contextmenu，与右键同路径）
// 缩放/平移/旋转统一走 CSS transform（will-change 提升合成层，GPU 合成零重排）
import { ref, computed, watch, onMounted, onUnmounted } from "vue";
import { NIcon, NSpin } from "naive-ui";
import { PhotoOff } from "@vicons/tabler";
import { getCurrentWindow } from "@tauri-apps/api/window";

const props = defineProps<{
  /** 图片地址（open_image 返回的 url：asset 协议或临时 PNG） */
  src: string;
  /** 轻渐变模式（调参重渲切 URL）：跳过模糊/状态重置，仅 180ms 交叉淡入 */
  crossfade?: boolean;
  /** 自适应顶部留空（px）：HDR 嵌入时传 56——SDR 预览 fit 基准与
   *  原生画布插槽范围一致（都从工具栏下方起），切换零跳变 */
  fitInsetTop?: number;
}>();

const emit = defineEmits<{
  (e: "scale-change", v: number): void;
  (e: "fullscreen-change", v: boolean): void;
  /** 侧键/轻扫/双指横滑请求切换：-1 上一张 / 1 下一张 */
  (e: "nav", dir: -1 | 1): void;
  /** 右键/长按快捷菜单（屏幕坐标，ViewerWindow 弹 n-dropdown） */
  (e: "contextmenu", x: number, y: number): void;
}>();

/** 缩放范围 0.1× - 8× */
const MIN_SCALE = 0.1;
const MAX_SCALE = 8;

const stageRef = ref<HTMLElement | null>(null);
const loading = ref(false);
const errorMsg = ref("");
const scale = ref(1);
const rotation = ref(0);
/** 平移偏移（px，相对容器中心） */
const tx = ref(0);
const ty = ref(0);
const imgW = ref(0);
const imgH = ref(0);
const stageW = ref(0);
const stageH = ref(0);
const maximized = ref(false);
/** 旋转动画期间启用 transform 过渡（缩放/拖拽不加过渡，保持跟手） */
const rotating = ref(false);
/** 图片已加载（触发模糊→清晰渐显，设计稿 5.4） */
const loaded = ref(false);
/** 用户手动操作过视图（缩放/平移/1:1）：容器 resize 不再强制重排视图 */
const userInteracted = ref(false);

const imgStyle = computed(() => ({
  width: `${imgW.value}px`,
  height: `${imgH.value}px`,
  // 先居中（-50%），再平移/缩放/旋转（缩放百分比 = 真实像素比：scale 1 = 100% 1:1）
  transform: `translate(-50%, -50%) translate(${tx.value}px, ${ty.value}px) scale(${scale.value}) rotate(${rotation.value}deg)`,
}));

// === 图片加载状态 ===
function onImgLoad(e: Event) {
  const img = e.target as HTMLImageElement;
  imgW.value = img.naturalWidth;
  imgH.value = img.naturalHeight;
  loading.value = false;
  errorMsg.value = "";
  loaded.value = true;
  fitView(); // 默认自适应：大图缩小完整显示（铺满），小图 1:1 原像素
}

function onImgError() {
  loading.value = false;
  loaded.value = false;
  errorMsg.value = "图片加载失败";
}

/** 实际像素视图：100% 1:1，居中显示 */
function actualView() {
  userInteracted.value = true;
  scale.value = 1;
  tx.value = 0;
  ty.value = 0;
  emit("scale-change", 1);
}

// 切换图片：重置视图状态（缩放/旋转/平移归位）+ 重置渐显
// crossfade 模式（调参重渲）：保留视图状态（用户正在看同构图，只换像素），
// 跳过 loading/模糊重置——180ms 交叉淡入抵消重渲卡顿感
watch(
  () => props.src,
  (src, old) => {
    if (props.crossfade && old) {
      // 旧 URL 暂存淡出层（同 transform，视觉无跳变）
      fadingSrc.value = old;
      window.setTimeout(() => { fadingSrc.value = ""; }, 240);
      loading.value = false;
      errorMsg.value = "";
      return;
    }
    loading.value = !!src;
    loaded.value = false;
    errorMsg.value = "";
    userInteracted.value = false;
    scale.value = 1;
    rotation.value = 0;
    tx.value = 0;
    ty.value = 0;
    emit("scale-change", 1);
  },
);

/** 交叉淡出层：调参重渲时的旧图 URL（240ms 后清除） */
const fadingSrc = ref("");

// === 容器尺寸跟踪（自适应计算用） ===
let resizeObserver: ResizeObserver | null = null;

onMounted(() => {
  // 同步初测：ResizeObserver 首次回调是异步的——图片 load 快于它时
  // stageW/H 仍为 0 → fitView 退化为 scale=1（小图不铺满的根因）。
  // mount 时布局已确定（absolute inset 容器尺寸即时可得），同步量一次。
  if (stageRef.value) {
    const r = stageRef.value.getBoundingClientRect();
    stageW.value = r.width;
    stageH.value = r.height;
  }
  resizeObserver = new ResizeObserver((entries) => {
    const rect = entries[0].contentRect;
    const changed = rect.width !== stageW.value || rect.height !== stageH.value;
    stageW.value = rect.width;
    stageH.value = rect.height;
    // 容器尺寸变化：处于自适应态（无手动缩放/平移/1:1）才重算铺满，
    // 用户手动操作过（zoom/拖拽）不打扰其视图
    if (changed && userInteracted.value === false && loaded.value && rect.width > 0 && rect.height > 0) {
      fitView();
    }
  });
  if (stageRef.value) resizeObserver.observe(stageRef.value);
  // wheel 需要非 passive 监听才能 preventDefault
  stageRef.value?.addEventListener("wheel", onWheel, { passive: false });
  try {
    getCurrentWindow()
      .isMaximized()
      .then((v) => {
        maximized.value = v;
      })
      .catch(() => {
        // 非 Tauri 环境忽略
      });
  } catch {
    // 非 Tauri 环境忽略
  }
});

onUnmounted(() => {
  resizeObserver?.disconnect();
  stageRef.value?.removeEventListener("wheel", onWheel);
});

// === wheel：纵向缩放（鼠标滚轮/触控板双指捏合 ctrl+wheel）/ 横向切图（触控板双指横滑） ===
/** 横滑切图节流时间戳（防一次手势连切多张） */
let wheelNavTs = 0;

function onWheel(e: WheelEvent) {
  if (!props.src) return;
  e.preventDefault();
  // 触控板双指横滑 → 切换图片（设计稿 5.1）
  if (Math.abs(e.deltaX) > Math.abs(e.deltaY)) {
    const now = performance.now();
    if (now - wheelNavTs < 280) return;
    wheelNavTs = now;
    emit("nav", e.deltaX > 0 ? 1 : -1);
    return;
  }
  const el = stageRef.value;
  if (!el) return;
  const rect = el.getBoundingClientRect();
  // 指针相对容器中心（transform-origin）的坐标
  const px = e.clientX - rect.left - rect.width / 2;
  const py = e.clientY - rect.top - rect.height / 2;
  // ctrl+wheel 为触控板捏合，手势已带平滑量，用小步长；纯滚轮用大步长
  const factor = e.ctrlKey ? 1.06 : 1.2;
  zoomAt(px, py, e.deltaY < 0 ? factor : 1 / factor);
}

/**
 * 以 (px,py) 为锚点缩放 factor 倍：保持锚点下方图像内容不动。
 * 推导：变换为 p = T(t)·S(s)·R(r)·q，缩放后要求同一 q 仍映射到 p，
 * 可得 t' = p - k·(p - t)（k = s'/s），结论与旋转角 r 无关。
 */
function zoomAt(px: number, py: number, factor: number) {
  const ns = Math.min(MAX_SCALE, Math.max(MIN_SCALE, scale.value * factor));
  const k = ns / scale.value;
  if (k === 1) return;
  userInteracted.value = true;
  tx.value = px - k * (px - tx.value);
  ty.value = py - k * (py - ty.value);
  scale.value = ns;
  emit("scale-change", ns);
}

/** 工具栏放大/缩小（以视图中心为锚点） */
function zoomIn() {
  zoomAt(0, 0, 1.25);
}
function zoomOut() {
  zoomAt(0, 0, 1 / 1.25);
}

/** 自适应 contain：大图缩小到完整可见（铺满容器）；小图 1:1 原像素显示（不放大）
 *  （scale 上限封顶 1；超出容器的图按 contain 缩到完整可见）
 *  fitInsetTop：与原生画布插槽同范围（顶部 56px 留白给悬浮工具栏），
 *  SDR/HDR 两模式图像区域恒一致 */
function fitView() {
  userInteracted.value = false;
  const insetTop = props.fitInsetTop ?? 0;
  const fitW = stageW.value;
  const fitH = stageH.value - insetTop;
  // fit 中心也随 inset 下移（与 HDR 画布图像中心重合）
  tx.value = 0;
  ty.value = insetTop > 0 ? insetTop / 2 : 0;
  if (imgW.value > 0 && imgH.value > 0 && fitW > 0 && fitH > 0) {
    // 旋转 90°/270° 时视觉宽高互换
    const w = rotation.value % 180 !== 0 ? imgH.value : imgW.value;
    const h = rotation.value % 180 !== 0 ? imgW.value : imgH.value;
    // min(..., 1)：小图（两向都放得下）→ 1 原像素；大图 → contain 缩小铺满
    scale.value = Math.min(fitW / w, fitH / h, 1);
  } else {
    scale.value = 1;
  }
  // 容器诊断日志（与 [画布对齐] HDR插槽日志对照：fit 基准 = (0,insetTop,w,h-insetTop)）
  logContainer(`SDR fit: 舞台${Math.round(stageW.value)}x${Math.round(stageH.value)} inset=${insetTop} 基准(0,${insetTop} ${Math.round(fitW)}x${Math.round(fitH)}) 图${imgW.value}x${imgH.value} scale=${scale.value.toFixed(3)}`);
  emit("scale-change", scale.value);
}

/** 容器诊断日志：低频（load/fit/resize），写后端 jietu-hdr.log 便于离线分析 */
let lastContainerLog = "";
function logContainer(msg: string) {
  if (msg === lastContainerLog) return; // 相同内容去重
  lastContainerLog = msg;
  console.info(`[画布对齐] ${msg}`);
  import("@tauri-apps/api/core")
    .then(({ invoke }) => invoke("trace_log", { msg: `[画布对齐] ${msg}` }))
    .catch(() => {});
}

// === 旋转 ===
function rotateLeft() {
  rotateBy(-90);
}
function rotateRight() {
  rotateBy(90);
}
function rotateBy(deg: number) {
  rotation.value = (rotation.value + deg + 360) % 360;
  rotating.value = true;
  window.setTimeout(() => {
    rotating.value = false;
  }, 240);
  // 保持当前像素缩放（scale 均匀作用，旋转不影响 1:1 语义），平移归位防出界
  tx.value = 0;
  ty.value = 0;
}

// === 全屏（窗口最大化）切换 ===
async function toggleFullscreen() {
  try {
    const win = getCurrentWindow();
    await win.toggleMaximize();
    maximized.value = await win.isMaximized();
    emit("fullscreen-change", maximized.value);
  } catch {
    // 非 Tauri 环境忽略
  }
}

// === pointer：拖拽平移 / 侧键翻页 / 触屏双指捏合 / 横向轻扫切换 ===
let dragging = false;
let dragStartX = 0;
let dragStartY = 0;
let dragBaseTx = 0;
let dragBaseTy = 0;
/** 本次拖拽累计位移与起始时刻（轻扫判定用） */
let dragDx = 0;
let dragDy = 0;
let dragTs = 0;
/** 活动触点表（pointerId → 屏幕坐标），双指捏合跟踪用 */
const pointers = new Map<number, { x: number; y: number }>();
/** 捏合基准：上一次两指距离（增量式缩放，避免累计误差） */
let pinchPrevDist = 0;

function onPointerDown(e: PointerEvent) {
  // 鼠标侧键翻页：3=后退(上一张) 4=前进(下一张)，设计稿 5.1
  if (e.button === 3 || e.button === 4) {
    e.preventDefault();
    emit("nav", e.button === 3 ? -1 : 1);
    return;
  }
  // 仅直接点击舞台（图片区域）时拖拽；target !== currentTarget 说明点在工具栏等浮层上
  if (e.button !== 0 || e.target !== e.currentTarget) return;
  pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
  // 第二指落下：进入捏合，终止单指拖拽
  if (pointers.size === 2) {
    dragging = false;
    const [a, b] = [...pointers.values()];
    pinchPrevDist = Math.hypot(a.x - b.x, a.y - b.y);
    return;
  }
  if (pointers.size > 2) return; // 三指及以上不处理
  dragging = true;
  dragStartX = e.clientX;
  dragStartY = e.clientY;
  dragBaseTx = tx.value;
  dragBaseTy = ty.value;
  dragDx = 0;
  dragDy = 0;
  dragTs = performance.now();
  (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
}

function onPointerMove(e: PointerEvent) {
  // 更新触点表（捏合中）
  if (pointers.has(e.pointerId)) {
    pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
  }
  // 双指捏合缩放：以两指中点为锚，增量式 zoomAt
  if (pointers.size === 2 && pinchPrevDist > 0) {
    const [a, b] = [...pointers.values()];
    const dist = Math.hypot(a.x - b.x, a.y - b.y);
    if (dist > 0) {
      const el = stageRef.value;
      if (el) {
        const rect = el.getBoundingClientRect();
        const px = (a.x + b.x) / 2 - rect.left - rect.width / 2;
        const py = (a.y + b.y) / 2 - rect.top - rect.height / 2;
        zoomAt(px, py, dist / pinchPrevDist);
      }
      pinchPrevDist = dist;
    }
    return;
  }
  if (!dragging) return;
  userInteracted.value = true;
  dragDx = e.clientX - dragStartX;
  dragDy = e.clientY - dragStartY;
  tx.value = dragBaseTx + dragDx;
  ty.value = dragBaseTy + dragDy;
}

function onPointerUp(e: PointerEvent) {
  pointers.delete(e.pointerId);
  if (pointers.size < 2) pinchPrevDist = 0;
  // 捏合后剩一指：以剩余触点重建拖拽基准，避免图片跳变
  if (pointers.size === 1) {
    const p = [...pointers.values()][0];
    dragging = true;
    dragStartX = p.x;
    dragStartY = p.y;
    dragBaseTx = tx.value;
    dragBaseTy = ty.value;
    dragDx = 0;
    dragDy = 0;
    dragTs = performance.now();
    return;
  }
  if (pointers.size > 0 || !dragging) return;
  dragging = false;
  // 触屏横向轻扫切换：快速（<320ms）、位移大（>60px）、水平占优（触屏滑动切换，设计稿 5.1）
  const dt = performance.now() - dragTs;
  if (dt < 320 && Math.abs(dragDx) > 60 && Math.abs(dragDx) > Math.abs(dragDy) * 1.5) {
    tx.value = dragBaseTx; // 回弹平移
    ty.value = dragBaseTy;
    emit("nav", dragDx < 0 ? 1 : -1); // 向左扫 → 下一张
  }
}

// === 右键 / 触屏长按快捷菜单（长按由浏览器转 contextmenu，同路径） ===
function onContextmenu(e: MouseEvent) {
  if (!props.src) return; // 空状态不弹自定义菜单
  e.preventDefault();
  emit("contextmenu", e.clientX, e.clientY);
}

// === 鸟瞰图导航器（大图超屏时快速查看图片区域，设计稿 5.4 快速定位） ===
/** 图片显示尺寸（缩放后，考虑 90° 旋转宽高互换） */
const dispW = computed(() =>
  rotation.value % 180 !== 0 ? imgH.value * scale.value : imgW.value * scale.value,
);
const dispH = computed(() =>
  rotation.value % 180 !== 0 ? imgW.value * scale.value : imgH.value * scale.value,
);
/** 仅当图片超出舞台可视区时显示鸟瞰图（+16px 容差防临界抖动） */
const navVisible = computed(
  () =>
    loaded.value &&
    !!props.src &&
    (dispW.value > stageW.value + 16 || dispH.value > stageH.value + 16),
);
/** 鸟瞰图容器固定尺寸 */
const NAV_W = 168;
const NAV_H = 126;
/** 鸟瞰图内整图缩放比（contain） */
const navScale = computed(() => {
  if (imgW.value <= 0 || imgH.value <= 0) return 1;
  return Math.min(NAV_W / imgW.value, NAV_H / imgH.value);
});
/** 鸟瞰图内整图的显示尺寸 */
const navImgW = computed(() => imgW.value * navScale.value);
const navImgH = computed(() => imgH.value * navScale.value);

/**
 * 可视框（舞台可见区域映射到图片域再缩放到鸟瞰图坐标）。
 * 变换链：图片本地 q（左上原点）→ 舞台 p = center + T + S·R·(q - imgCenter)，
 * 反解舞台可视区 [0..stageW]×[0..stageH] 左上角与尺寸（90° 倍数旋转下为正矩形）。
 */
const navRect = computed(() => {
  const s = scale.value;
  const r = ((rotation.value % 360) + 360) % 360;
  // 舞台左上角相对图片中心的向量（缩放域）
  const ax = (-stageW.value / 2 - tx.value) / s;
  const ay = (-stageH.value / 2 - ty.value) / s;
  // 旋转反变换 R(-r)：90° 倍数
  let qx: number, qy: number, rw: number, rh: number;
  if (r === 90) {
    qx = ay;
    qy = -ax;
    rw = stageH.value / s;
    rh = stageW.value / s;
  } else if (r === 180) {
    qx = -ax;
    qy = -ay;
    rw = stageW.value / s;
    rh = stageH.value / s;
  } else if (r === 270) {
    qx = -ay;
    qy = ax;
    rw = stageH.value / s;
    rh = stageW.value / s;
  } else {
    qx = ax;
    qy = ay;
    rw = stageW.value / s;
    rh = stageH.value / s;
  }
  // 图片域左上角（相对图片左上原点）
  const px = qx + imgW.value / 2;
  const py = qy + imgH.value / 2;
  return {
    x: px * navScale.value,
    y: py * navScale.value,
    w: rw * navScale.value,
    h: rh * navScale.value,
  };
});

/** 鸟瞰图交互：把鸟瞰图内某点（图片域坐标）定位到舞台中心 */
function centerImageAt(imgPx: number, imgPy: number) {
  const s = scale.value;
  const r = ((rotation.value % 360) + 360) % 360;
  // 期望：该点位于舞台中心 → T = -s·R(r)·(q - imgCenter)
  const dx = imgPx - imgW.value / 2;
  const dy = imgPy - imgH.value / 2;
  let vx: number, vy: number;
  if (r === 90) {
    vx = -dy;
    vy = dx;
  } else if (r === 180) {
    vx = -dx;
    vy = -dy;
  } else if (r === 270) {
    vx = dy;
    vy = -dx;
  } else {
    vx = dx;
    vy = dy;
  }
  tx.value = -s * vx;
  ty.value = -s * vy;
}

/** 鸟瞰图 pointer：点击/拖拽定位（框中心跟随指针） */
const navDragging = ref(false);
function onNavPointerDown(e: PointerEvent) {
  if (e.button !== 0) return;
  e.preventDefault();
  e.stopPropagation();
  navDragging.value = true;
  (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
  navLocate(e);
}
function onNavPointerMove(e: PointerEvent) {
  if (!navDragging.value) return;
  navLocate(e);
}
function onNavPointerUp() {
  navDragging.value = false;
}
function navLocate(e: PointerEvent) {
  const el = e.currentTarget as HTMLElement;
  const rect = el.getBoundingClientRect();
  // 鸟瞰图容器内坐标 → 整图 contain 区域（居中）内坐标 → 图片域坐标
  const ox = e.clientX - rect.left - (rect.width - navImgW.value) / 2;
  const oy = e.clientY - rect.top - (rect.height - navImgH.value) / 2;
  centerImageAt(ox / navScale.value, oy / navScale.value);
}

// 暴露给 ViewerWindow（工具栏按钮调用）
defineExpose({
  zoomIn,
  zoomOut,
  fitView,
  actualView,
  rotateLeft,
  rotateRight,
  toggleFullscreen,
});
</script>

<template>
  <div
    ref="stageRef"
    class="img-stage"
    :class="{ grabbable: !!src && !errorMsg }"
    @pointerdown="onPointerDown"
    @pointermove="onPointerMove"
    @pointerup="onPointerUp"
    @pointercancel="onPointerUp"
    @contextmenu="onContextmenu"
  >
    <!-- 交叉淡出层：调参重渲的旧图（同 transform 叠在新图下，180ms 淡出） -->
    <img
      v-if="fadingSrc"
      :src="fadingSrc"
      class="img-old leaving"
      :style="imgStyle"
      draggable="false"
    />
    <img
      v-if="src && !errorMsg"
      :src="src"
      class="img-main"
      :class="{ rotating, loaded, crossfade: crossfade && fadingSrc }"
      :style="imgStyle"
      draggable="false"
      @load="onImgLoad"
      @error="onImgError"
    />
    <!-- 状态浮层：加载中 / 错误 / 空引导 -->
    <div v-if="loading" class="img-state">
      <n-spin :size="18" />
      <span>正在解码…</span>
    </div>
    <div v-else-if="errorMsg" class="img-state">
      <n-icon :component="PhotoOff" :size="28" />
      <span>{{ errorMsg }}</span>
    </div>
    <!-- 空状态：柔和相纸插画（设计稿 4.6 拒绝生硬图标，温柔文案） -->
    <div v-else-if="!src" class="img-state empty">
      <div class="polaroid">
        <div class="polaroid-photo">
          <div class="pp-blob a" />
          <div class="pp-blob b" />
          <div class="pp-sun" />
        </div>
        <div class="polaroid-caption">温柔记录每一帧</div>
      </div>
      <span class="empty-hint">拖入图片，或点击工具栏「打开」挑选第一张</span>
    </div>
    <!-- 鸟瞰图导航器：图片超出可视区时显示，点击/拖拽快速定位 -->
    <div
      v-if="navVisible"
      class="img-navigator"
      :class="{ dragging: navDragging }"
      @pointerdown="onNavPointerDown"
      @pointermove="onNavPointerMove"
      @pointerup="onNavPointerUp"
      @pointercancel="onNavPointerUp"
    >
      <img
        :src="src"
        class="nav-thumb"
        :style="{
          width: `${navImgW}px`,
          height: `${navImgH}px`,
        }"
        draggable="false"
      />
      <div
        class="nav-rect"
        :style="{
          left: `${navRect.x}px`,
          top: `${navRect.y}px`,
          width: `${navRect.w}px`,
          height: `${navRect.h}px`,
        }"
      />
    </div>
  </div>
</template>

<style scoped>
.img-stage {
  position: absolute;
  inset: 0;
  overflow: hidden;
  touch-action: none;
}
.img-stage.grabbable {
  cursor: grab;
}
.img-stage.grabbable:active {
  cursor: grabbing;
}
.img-main {
  position: absolute;
  left: 50%;
  top: 50%;
  /* 尺寸由 imgStyle 按自然像素设置：scale 1 = 100% 实际像素（1:1） */
  transform-origin: center center;
  will-change: transform; /* GPU 合成层 */
  user-select: none;
  pointer-events: none; /* 拖拽/滚轮统一走舞台容器 */
  /* 加载渐显初始态：模糊+透明（设计稿 5.4） */
  opacity: 0;
  filter: blur(14px);
}
/* 加载完成：模糊→清晰 + 淡入（320ms 舒缓节奏） */
.img-main.loaded {
  opacity: 1;
  filter: blur(0);
  transition: opacity 320ms ease, filter 320ms ease;
}
/* 重渲（调参历史选条目）：轻渐变（180ms 交叉淡入，无模糊重置感） */
.img-main.crossfade {
  opacity: 1;
  filter: blur(0);
  transition: opacity 180ms ease, filter 180ms ease;
}
/* 交叉淡入用双 img 层：旧图淡出层 */
.img-old {
  position: absolute;
  left: 50%;
  top: 50%;
  transform-origin: center center;
  user-select: none;
  pointer-events: none;
  opacity: 0;
  transition: opacity 180ms ease;
}
.img-old.leaving {
  opacity: 0.6;
}
/* 旋转动画：transform 过渡（须列全 opacity/filter，避免覆盖渐显过渡） */
.img-main.rotating {
  transition: transform 220ms ease, opacity 320ms ease, filter 320ms ease;
}
.img-state {
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
  user-select: none;
}

/* === 鸟瞰图导航器（右下角浮层，图片超屏时显示） === */
.img-navigator {
  position: absolute;
  right: 16px;
  bottom: 16px;
  width: 168px;
  height: 126px;
  display: flex;
  align-items: center;
  justify-content: center;
  background: color-mix(in srgb, var(--jb-bg-card) 86%, transparent);
  backdrop-filter: blur(18px) saturate(1.3);
  border: 1px solid var(--jb-border);
  border-radius: 10px;
  box-shadow: var(--jb-shadow);
  cursor: pointer;
  user-select: none;
  touch-action: none;
  z-index: 5;
  overflow: hidden;
  transition: opacity 220ms ease, transform 220ms ease;
}
.img-navigator.dragging {
  border-color: color-mix(in srgb, var(--jb-primary) 55%, var(--jb-border));
}
.nav-thumb {
  object-fit: fill;
  pointer-events: none;
  opacity: 0.92;
}
/* 可视区域框：主题色描边 + 半透明填充，超出鸟瞰图区域被裁剪 */
.nav-rect {
  position: absolute;
  border: 1.5px solid var(--jb-primary);
  background: color-mix(in srgb, var(--jb-primary) 14%, transparent);
  box-shadow: 0 0 0 1px color-mix(in srgb, var(--jb-bg-card) 70%, transparent);
  pointer-events: none;
}

/* === 空状态：CSS 相纸插画（柔和手绘感，无位图资源） === */
.img-state.empty {
  gap: 22px;
}
.polaroid {
  width: 148px;
  padding: 10px 10px 14px;
  background: #fdfbfa;
  border-radius: 6px;
  box-shadow: 0 12px 32px rgba(0, 0, 0, 0.14), 0 2px 6px rgba(0, 0, 0, 0.08);
  transform: rotate(-4deg);
  animation: polaroid-float 5s ease-in-out infinite alternate;
}
@keyframes polaroid-float {
  from {
    transform: rotate(-4deg) translateY(0);
  }
  to {
    transform: rotate(-3deg) translateY(-6px);
  }
}
.polaroid-photo {
  position: relative;
  height: 128px;
  border-radius: 3px;
  overflow: hidden;
  background: linear-gradient(160deg, #fdf0f4 0%, #f3ecfa 55%, #fbeee6 100%);
}
/* 相纸「照片」内的柔色光斑（跟随主题色调配） */
.pp-blob {
  position: absolute;
  border-radius: 50%;
  filter: blur(2px);
}
.pp-blob.a {
  width: 74px;
  height: 74px;
  left: 12px;
  bottom: 14px;
  background: color-mix(in srgb, var(--jb-primary) 42%, #fff);
}
.pp-blob.b {
  width: 46px;
  height: 46px;
  right: 16px;
  top: 16px;
  background: color-mix(in srgb, var(--jb-primary) 26%, #fff);
}
.pp-sun {
  position: absolute;
  right: 22px;
  bottom: 18px;
  width: 22px;
  height: 22px;
  border-radius: 50%;
  background: #ffe9c9;
  box-shadow: 0 0 14px 6px rgba(255, 224, 178, 0.65);
}
.polaroid-caption {
  margin-top: 9px;
  text-align: center;
  font-size: 11px;
  letter-spacing: 2px;
  color: #a89ba4;
  font-family: "Segoe UI Variable", "微软雅黑", sans-serif;
}
.empty-hint {
  font-size: 12px;
  color: var(--jb-text-mute);
  letter-spacing: 0.5px;
}
</style>
