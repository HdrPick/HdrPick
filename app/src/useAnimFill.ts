// JXL 动图 GPU 常驻填充进度 composable（后端 Tauri 事件 anim://fill）
// 事件契约（勿改）：{ filled_frames, filled_ms, total_frames(null=未知), state: filling|full_resident|degraded, backend: native|hybrid }
// 模块级单例监听：多个组件复用同一个 listen（重复挂载不重复监听），引用计数归零自动 unlisten。
// 主线程友好：纯事件驱动（无轮询）；事件处理器逐字段变化检测，值未变不触发响应式更新。
import { ref, onScopeDispose, getCurrentScope } from "vue";

export type AnimFillPhase = "idle" | "filling" | "full_resident" | "degraded";
export type AnimBackend = "native" | "hybrid";

/** 后端 anim://fill 负载（与 Rust serde 命名对齐） */
export interface AnimFillPayload {
  filled_frames: number;
  filled_ms: number;
  total_frames: number | null;
  state: "filling" | "full_resident" | "degraded";
  backend: AnimBackend;
}

/** filling 中超过该时长无新事件且播放窗口未反馈 → 界面提示"解码较慢"（防假死观感） */
export const ANIM_FILL_STALL_MS = 5000;

const filledFrames = ref(0);
const filledMs = ref(0);
const totalFrames = ref<number | null>(null);
const phase = ref<AnimFillPhase>("idle");
const backend = ref<AnimBackend | null>(null);
const isStalled = ref(false);

let unlistenFn: (() => void) | null = null;
let starting: Promise<void> | null = null;
let refCount = 0;
let stallTimer = 0;

/** 事件负载 → 响应式状态（逐字段变化检测：值未变不写 ref，即高频 state 变化时的渲染去重） */
export function applyAnimFillPayload(p: Partial<AnimFillPayload> | null | undefined): void {
  if (!p || typeof p !== "object") return;
  // state 是负载的语义锚点：缺失/未知 → 视为畸形负载整体忽略（防前端凭空触发 UI）
  const st: AnimFillPhase | null =
    p.state === "filling" ? "filling" : p.state === "full_resident" ? "full_resident" : p.state === "degraded" ? "degraded" : null;
  if (st === null) return;
  if (typeof p.filled_frames === "number" && p.filled_frames !== filledFrames.value)
    filledFrames.value = p.filled_frames;
  if (typeof p.filled_ms === "number" && p.filled_ms !== filledMs.value) filledMs.value = p.filled_ms;
  // total_frames 仅在首次已知时写入；已知后不回退为未知（防异常负载闪跳）
  if (typeof p.total_frames === "number" && p.total_frames !== totalFrames.value)
    totalFrames.value = p.total_frames;
  if ((p.backend === "native" || p.backend === "hybrid") && p.backend !== backend.value)
    backend.value = p.backend;
  if (st !== phase.value) phase.value = st;
  armStallWatch();
}

/** 停滞看门狗：filling 态下每次事件重置 5s 定时器（事件驱动，非后端轮询）；终态自动解除 */
function armStallWatch(): void {
  if (stallTimer) {
    clearTimeout(stallTimer);
    stallTimer = 0;
  }
  if (phase.value === "filling") {
    isStalled.value = false;
    stallTimer = setTimeout(() => {
      stallTimer = 0;
      isStalled.value = true;
    }, ANIM_FILL_STALL_MS);
  } else {
    isStalled.value = false;
  }
}

/** 清空会话状态（换图 / 新一轮填充前调用） */
function resetAnimFillState(): void {
  if (stallTimer) {
    clearTimeout(stallTimer);
    stallTimer = 0;
  }
  filledFrames.value = 0;
  filledMs.value = 0;
  totalFrames.value = null;
  phase.value = "idle";
  backend.value = null;
  isStalled.value = false;
}

async function ensureListener(): Promise<void> {
  if (unlistenFn) return;
  if (starting) return starting;
  starting = (async () => {
    try {
      const { listen } = await import("@tauri-apps/api/event");
      unlistenFn = await listen<AnimFillPayload>("anim://fill", (ev) => {
        applyAnimFillPayload(ev.payload);
      });
    } catch {
      // 非 Tauri 环境（纯浏览器 / 单测）：静默降级，仅响应 applyAnimFillPayload 直灌
      unlistenFn = null;
    } finally {
      starting = null;
    }
  })();
  return starting;
}

function release(): void {
  refCount -= 1;
  if (refCount > 0) return;
  if (unlistenFn) {
    try {
      unlistenFn();
    } catch {
      // 重复 unlisten / 窗口已销毁：忽略
    }
    unlistenFn = null;
  }
  resetAnimFillState();
}

/**
 * JXL 动图填充进度共享状态。
 * 返回响应式 { filledFrames, filledMs, totalFrames, state, backend, isStalled } 与 reset()；
 * 组件卸载自动 unlisten（引用计数归零时）。
 */
export function useAnimFill() {
  refCount += 1;
  void ensureListener();
  if (getCurrentScope()) onScopeDispose(release);
  return {
    filledFrames,
    filledMs,
    totalFrames,
    state: phase,
    backend,
    isStalled,
    reset: resetAnimFillState,
  };
}
