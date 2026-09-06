<script setup lang="ts">
// 底部缩略图栏：横向滚动 + IntersectionObserver 懒加载（进入可视区才请求）+ Map 缓存（LRU 上限 100）
// 后端 get_thumbnail 返回 base64 PNG 字符串 → 拼 data URL 展示
import { ref, computed, watch, nextTick, onMounted, onUnmounted } from "vue";
import { invoke } from "@tauri-apps/api/core";

interface ImageEntry {
  path: string;
  name: string;
  width: number;
  height: number;
}

const props = defineProps<{
  /** 同目录图片列表（list_directory_images 返回） */
  items: ImageEntry[];
  /** 当前显示图片路径（高亮 + 滚动定位） */
  current: string;
  /** 本标签已打开过的图片路径（角标标记，浏览器已访问链接思路） */
  visited?: string[];
}>();

const emit = defineEmits<{
  (e: "select", path: string): void;
}>();

const scrollRef = ref<HTMLElement | null>(null);
/** path → data URL（展示层，与 cache 同步淘汰） */
const thumbs = ref<Record<string, string>>({});
/** LRU 缓存：最近使用移到尾部，超上限淘汰头部 */
const cache = new Map<string, string>();
// 与后端 LRU 上限一致（400px 高清缩略图）
const LRU_MAX = 600;
/** 进行中的请求去重 */
const pending = new Set<string>();

let observer: IntersectionObserver | null = null;

/** 写入缓存（LRU 淘汰时同步清掉展示层引用） */
function putCache(path: string, url: string) {
  if (cache.has(path)) cache.delete(path);
  cache.set(path, url);
  while (cache.size > LRU_MAX) {
    const oldest = cache.keys().next().value;
    if (oldest === undefined) break;
    cache.delete(oldest);
    delete thumbs.value[oldest];
  }
}

function requestThumb(path: string) {
  if (cache.has(path) || pending.has(path)) return;
  pending.add(path);
  invoke<string>("get_thumbnail", { path })
    .then((b64) => {
      // 兼容后端返回裸 base64 或完整 data URL 两种形态
      const url = b64.startsWith("data:") ? b64 : `data:image/png;base64,${b64}`;
      putCache(path, url);
      thumbs.value = { ...thumbs.value, [path]: url };
    })
    .catch(() => {
      // 后端命令未就绪时静默容错：保持占位块
    })
    .finally(() => pending.delete(path));
}

/** 对所有缩略图项挂 IntersectionObserver（重复 observe 同一元素为幂等操作） */
function observeAll() {
  const root = scrollRef.value;
  if (!root) return;
  if (!observer) {
    observer = new IntersectionObserver(
      (entries) => {
        for (const en of entries) {
          if (!en.isIntersecting) continue;
          const path = (en.target as HTMLElement).dataset.path;
          if (path) requestThumb(path);
          observer?.unobserve(en.target); // 首次进入可视区即请求，后续命中缓存
        }
      },
      { root, rootMargin: "120px" },
    );
  }
  root.querySelectorAll<HTMLElement>(".thumb-item").forEach((el) => {
    observer?.observe(el);
  });
}

/** 当前项滚动到可视区中央
 *  不用 scrollIntoView：它会连带滚动 overflow:hidden 的祖先（大图舞台），
 *  导致主图偏移出中心、右侧露背景（表现为"图片不居中 + 遮挡层"）。
 *  手动计算 scrollLeft 只滚缩略图容器本身。 */
function scrollActiveIntoView() {
  stopMomentum(); // 程序定位（键盘翻图等）打断用户惯性滑动
  const root = scrollRef.value;
  const el = root?.querySelector<HTMLElement>('.thumb-item[data-active="true"]');
  if (!root || !el) return;
  const target = el.offsetLeft + el.offsetWidth / 2 - root.clientWidth / 2;
  root.scrollTo({ left: target, behavior: "smooth" });
}

// 列表变化：清空时重置 observer（root 元素已卸载），重建后重新观察 + 定位当前项
watch(
  () => props.items,
  (list) => {
    if (list.length === 0) {
      observer?.disconnect();
      observer = null;
    }
    nextTick(() => {
      observeAll();
      scrollActiveIntoView();
    });
  },
);

// 当前项变化：平滑滚动定位
watch(
  () => props.current,
  () => nextTick(scrollActiveIntoView),
);

/** 占位块显示扩展名（如 PNG / JPG） */
function extOf(name: string): string {
  const i = name.lastIndexOf(".");
  return i > 0 ? name.slice(i + 1).toUpperCase() : "?";
}

/** 是否已打开过（角标判断；Set 每次重建成本可忽略，列表 ≤ 千级） */
const visitedSet = computed(() => new Set(props.visited ?? []));

onMounted(() => {
  nextTick(observeAll);
  // 滚轮垂直滚动 → 横向滚动（缩略图栏习惯：普通滚轮即可翻看，免找 Shift）
  scrollRef.value?.addEventListener("wheel", onWheel, { passive: false });
  // 按住拖动：down 在容器上，move/up 挂 window（拖出容器仍跟踪，且不劫持子项 click）
  scrollRef.value?.addEventListener("pointerdown", onPointerDown);
  window.addEventListener("pointermove", onPointerMove);
  window.addEventListener("pointerup", onPointerUp);
  window.addEventListener("pointercancel", onPointerUp);
  // 捕获阶段吞掉拖动后的 click（拖动不是选图）
  scrollRef.value?.addEventListener("click", onClickCapture, true);
});

onUnmounted(() => {
  observer?.disconnect();
  observer = null;
  stopMomentum();
  scrollRef.value?.removeEventListener("wheel", onWheel);
  scrollRef.value?.removeEventListener("pointerdown", onPointerDown);
  window.removeEventListener("pointermove", onPointerMove);
  window.removeEventListener("pointerup", onPointerUp);
  window.removeEventListener("pointercancel", onPointerUp);
  scrollRef.value?.removeEventListener("click", onClickCapture, true);
});

/** 垂直滚轮转横向滚动（ deltaX 手势原样保留）；滚轮打断惯性滑动 */
function onWheel(e: WheelEvent) {
  const el = scrollRef.value;
  if (!el) return;
  stopMomentum();
  if (Math.abs(e.deltaY) > Math.abs(e.deltaX)) {
    e.preventDefault();
    el.scrollLeft += e.deltaY;
  }
}

// ==================== 按住拖动 + 惯性滑动（速度感应） ====================
// 按住缩略条左右拖动跟手滚动；松手按最近拖动速度启动惯性，
// 速度随时间指数衰减（快甩远滑、慢拖轻移），撞到边界立即停止。
// 位移超过 DRAG_THRESHOLD 才算拖动，避免误吞单击选图。
const DRAG_THRESHOLD = 5; // px
/** 是否处于按住状态 */
let dragging = false;
/** 本次按住是否发生实际拖动（用于吞掉拖动后的 click） */
let dragMoved = false;
let dragStartX = 0;
let dragStartLeft = 0;
let lastX = 0;
let lastT = 0;
/** 平滑后的拖动速度（px/ms），松手时作为惯性初速 */
let dragVelocity = 0;
/** 惯性动画 rAF id（0 = 无惯性） */
let momentumId = 0;

function onPointerDown(e: PointerEvent) {
  if (e.button !== 0) return;
  const el = scrollRef.value;
  if (!el) return;
  stopMomentum();
  dragging = true;
  dragMoved = false;
  dragStartX = e.clientX;
  dragStartLeft = el.scrollLeft;
  lastX = e.clientX;
  lastT = performance.now();
  dragVelocity = 0;
  // 注意：不能用 setPointerCapture——它会把后续 click 重定向到容器，
  // 导致缩略图按钮的 @click 选图失效；move/up 挂 window 保证拖出容器仍跟踪
}

function onPointerMove(e: PointerEvent) {
  if (!dragging) return;
  const el = scrollRef.value;
  if (!el) return;
  const dx = e.clientX - dragStartX;
  if (!dragMoved) {
    if (Math.abs(dx) <= DRAG_THRESHOLD) return;
    dragMoved = true;
    el.classList.add("dragging");
  }
  // 采样瞬时速度并指数平滑（0.6 旧值 + 0.4 新值）：
  // 兼顾甩动末段的灵敏度与手抖造成的速度毛刺抑制
  const now = performance.now();
  const dt = now - lastT;
  if (dt > 0) {
    const v = (e.clientX - lastX) / dt;
    dragVelocity = dragVelocity * 0.6 + v * 0.4;
  }
  lastX = e.clientX;
  lastT = now;
  el.scrollLeft = dragStartLeft - dx;
}

function onPointerUp() {
  if (!dragging) return;
  dragging = false;
  scrollRef.value?.classList.remove("dragging");
  if (dragMoved) startMomentum();
}

/** 松手后的惯性滑动：初速 = 最近拖动速度，每毫秒衰减 0.2%（半衰期 ~346ms） */
function startMomentum() {
  const el = scrollRef.value;
  if (!el || Math.abs(dragVelocity) < 0.05) return; // 太慢不启动
  let v = dragVelocity; // px/ms
  let last = performance.now();
  const step = (now: number) => {
    const dt = Math.min(now - last, 40); // 防后台节流产生大 dt 跳变
    last = now;
    v *= Math.pow(0.998, dt);
    const intended = v * dt;
    const before = el.scrollLeft;
    el.scrollLeft -= intended;
    // 实际位移明显小于预期 = 已被边界夹住 → 停
    if (Math.abs(before - el.scrollLeft - intended) > 1) return;
    if (Math.abs(v) < 0.02) return;
    momentumId = requestAnimationFrame(step);
  };
  momentumId = requestAnimationFrame(step);
}

function stopMomentum() {
  if (momentumId) {
    cancelAnimationFrame(momentumId);
    momentumId = 0;
  }
}

/** 拖动后的 click 吞掉（拖动不是选图）；捕获阶段拦截先于子项 click */
function onClickCapture(e: MouseEvent) {
  if (dragMoved) {
    e.stopPropagation();
    e.preventDefault();
  }
}
</script>

<template>
  <div class="thumb-bar">
    <div v-if="items.length === 0" class="thumb-empty">
      打开图片后，同目录图片会显示在这里
    </div>
    <div v-else ref="scrollRef" class="thumb-scroll">
      <button
        v-for="item in items"
        :key="item.path"
        class="thumb-item"
        :data-path="item.path"
        :data-active="item.path === current"
        :title="`${item.name}（${item.width}×${item.height}${visitedSet.has(item.path) ? ' · 已打开' : ''}）`"
        @click="emit('select', item.path)"
      >
        <!-- 已打开角标（主题色小圆点，浏览器已访问链接思路） -->
        <span
          v-if="item.path !== current && visitedSet.has(item.path)"
          class="thumb-dot"
        />
        <img
          v-if="thumbs[item.path]"
          :src="thumbs[item.path]"
          class="thumb-img"
          draggable="false"
        />
        <span v-else class="thumb-placeholder">{{ extOf(item.name) }}</span>
      </button>
    </div>
  </div>
</template>

<style scoped>
.thumb-bar {
  height: 100%;
  display: flex;
  align-items: center;
  background: var(--jb-bg-card);
  border-top: 1px solid var(--jb-border);
  user-select: none;
}
.thumb-empty {
  width: 100%;
  text-align: center;
  font-size: 11px;
  color: var(--jb-text-mute);
}
.thumb-scroll {
  display: flex;
  gap: 8px;
  align-items: center;
  height: 100%;
  width: 100%;
  overflow-x: auto;
  overflow-y: hidden;
  /* 对称半屏留白：数量少时缩略图整体居中；超宽时首/尾项可滚动到正中央
     （60px = 缩略图宽的一半 + 边框；calc 基于容器可视宽） */
  padding: 0 max(calc(50% - 38px), 12px);
  scrollbar-width: thin;
  scrollbar-color: var(--jb-scrollbar) transparent;
  cursor: grab;
}
/* 拖动中：抓手光标 + 关闭卡片过渡（hover 放大动画在跟手滚动时会造成视觉抖动） */
.thumb-scroll.dragging {
  cursor: grabbing;
}
.thumb-scroll.dragging .thumb-item {
  transition: none;
}
/* 超宽滚动条（Chromium/WebView2）：细窄轻量，hover 加深 */
.thumb-scroll::-webkit-scrollbar {
  height: 6px;
}
.thumb-scroll::-webkit-scrollbar-track {
  background: transparent;
}
.thumb-scroll::-webkit-scrollbar-thumb {
  background: var(--jb-scrollbar);
  border-radius: 3px;
}
.thumb-scroll::-webkit-scrollbar-thumb:hover {
  background: color-mix(in srgb, var(--jb-primary) 45%, var(--jb-scrollbar));
}
.thumb-item {
  flex-shrink: 0;
  width: 60px;
  height: 60px;
  border: 2px solid transparent;
  border-radius: 8px;
  background: var(--jb-bg);
  display: flex;
  align-items: center;
  justify-content: center;
  cursor: pointer;
  padding: 0;
  overflow: hidden;
  transition: border-color 220ms ease, transform 220ms ease, box-shadow 220ms ease;
}
/* hover 轻微放大 + 阴影加深（设计稿 4.3/5.4：缩放不超过 1.03 倍） */
.thumb-item:hover {
  transform: translateY(-2px) scale(1.03);
  box-shadow: var(--jb-shadow);
  z-index: 1;
}
/* 当前项：主题色描边 + 柔光 */
.thumb-item[data-active="true"] {
  border-color: var(--jb-primary);
  box-shadow: 0 0 0 3px color-mix(in srgb, var(--jb-primary) 22%, transparent);
}
.thumb-img {
  max-width: 100%;
  max-height: 100%;
  object-fit: contain;
}
.thumb-placeholder {
  font-size: 10px;
  color: var(--jb-text-mute);
}
/* 已打开角标：右上角主题色圆点（当前项已有描边高亮，不重复标） */
.thumb-dot {
  position: absolute;
  top: 4px;
  right: 4px;
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: var(--jb-primary);
  box-shadow: 0 0 0 2px color-mix(in srgb, var(--jb-bg-card) 85%, transparent);
  pointer-events: none;
}
</style>
