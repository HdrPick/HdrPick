<script setup lang="ts">
// 调参历史 + 双列卡片 + 四宫格对比（TonePanel「历史」按钮弹出）
//
// 数据流：
// - 列表 = 后端 sidecar（.jietu-hdr/<stem>.json，参数快照跟随图片）
// - 每条历史自动后台 retonemap 出预览缩略图（卡片内直接可见效果）
// - 点击卡片 → emit('apply', entry) → ViewerWindow 重渲（交叉淡入）
// - 「重生图」→ history_export_png 按该参数输出 PNG 到原图同目录
// - 勾选 ≤4 条 → 「对比」→ 四宫格放大对比，Esc 逐层退出
import { ref, computed, watch } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { NIcon, NButton, NEmpty, NTag, useMessage } from "naive-ui";
import { X, Trash, LayoutGrid, Download } from "@vicons/tabler";
import type { ToneMapOverrides } from "../configStore";

interface HistoryEntry {
  ts: number;
  preset: string;
  overrides: ToneMapOverrides;
  note?: string | null;
}
interface HistoryFile {
  entries: HistoryEntry[];
  applied: number;
}

const props = defineProps<{
  /** HDR 源图路径（sidecar 定位） */
  path: string;
}>();

const emit = defineEmits<{
  /** 应用某条历史（重渲主图） */
  (e: "apply", entry: HistoryEntry): void;
  (e: "close"): void;
}>();

const message = useMessage();

const history = ref<HistoryFile>({ entries: [], applied: 0 });
const selected = ref<number[]>([]);
/** 历史预览缩略图（ts → URL；加载完成后渐次填充） */
const previewUrls = ref<Record<number, string>>({});
/** 重生图进行中（ts 集合） */
const exporting = ref<Set<number>>(new Set());
/** 四宫格渲染结果（ts → URL） */
const compareUrls = ref<Record<number, string>>({});
const comparing = ref(false);
const zoomed = ref<number | null>(null);

async function load() {
  try {
    history.value = await invoke<HistoryFile>("history_list", { path: props.path });
  } catch {
    history.value = { entries: [], applied: 0 };
  }
  previewUrls.value = {};
  // 双列卡片自动预览：逐条后台 retonemap（HDR 源已缓存，毫秒级/张），
  // 限制并发 3 防止瞬时 CPU 峰值；完成一张显示一张
  const queue = [...history.value.entries];
  const worker = async () => {
    while (queue.length > 0) {
      const e = queue.shift();
      if (!e) break;
      try {
        const info = await invoke<{ url: string }>("retonemap_image", {
          path: props.path,
          preset: e.preset,
          overrides: e.overrides,
        });
        previewUrls.value = { ...previewUrls.value, [e.ts]: info.url };
      } catch {
        /* 单张失败跳过（卡片显示参数无预览） */
      }
    }
  };
  void Promise.all([worker(), worker(), worker()]);
}
watch(() => props.path, load, { immediate: true });

function toggle(ts: number) {
  const i = selected.value.indexOf(ts);
  if (i >= 0) {
    selected.value.splice(i, 1);
  } else if (selected.value.length < 4) {
    selected.value.push(ts);
  }
}

/** 参数差异摘要（卡片/角标） */
function summarize(e: HistoryEntry): string {
  const parts: string[] = [e.preset.replace("builtin:", "")];
  const ev = e.overrides.exposure_ev;
  if (ev !== null && ev !== undefined) parts.push(`${ev > 0 ? "+" : ""}${ev.toFixed(1)}EV`);
  const dw = e.overrides.output_diffuse_white;
  if (dw !== null && dw !== undefined) parts.push(`W${dw.toFixed(2)}`);
  const pk = e.overrides.source_peak_nits;
  if (pk !== null && pk !== undefined) parts.push(`${pk}n`);
  const st = e.overrides.saturation;
  if (st !== null && st !== undefined) parts.push(`S${st.toFixed(1)}`);
  return parts.join(" · ");
}

function fmtTime(ts: number): string {
  const d = new Date(ts);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** 重生图：按该条历史参数输出 SDR PNG 到原图同目录 */
async function exportPng(e: HistoryEntry) {
  if (exporting.value.has(e.ts)) return;
  exporting.value = new Set(exporting.value).add(e.ts);
  try {
    const out = await invoke<string>("history_export_png", {
      path: props.path,
      ts: e.ts,
    });
    message.success(`已输出 PNG：${out.split(/[\\/]/).pop()}`, { duration: 3200 });
  } catch (err) {
    message.error(`重生图失败: ${err}`, { duration: 2600 });
  } finally {
    const s = new Set(exporting.value);
    s.delete(e.ts);
    exporting.value = s;
  }
}

/** 进入四宫格：逐条后台 retonemap（复用预览缓存 + 补齐未完成的） */
async function startCompare() {
  if (selected.value.length < 2) return;
  comparing.value = true;
  compareUrls.value = { ...previewUrls.value };
  await Promise.all(
    selected.value.map(async (ts) => {
      if (compareUrls.value[ts]) return;
      const e = history.value.entries.find((x) => x.ts === ts);
      if (!e) return;
      try {
        const info = await invoke<{ url: string }>("retonemap_image", {
          path: props.path,
          preset: e.preset,
          overrides: e.overrides,
        });
        compareUrls.value = { ...compareUrls.value, [ts]: info.url };
      } catch {
        /* 单张失败跳过 */
      }
    }),
  );
}

async function removeEntry(ts: number) {
  try {
    await invoke("history_delete", { path: props.path, ts });
    selected.value = selected.value.filter((t) => t !== ts);
    await load();
  } catch { /* 忽略 */ }
}

/** Esc：退出放大 / 退出对比 / 关闭面板 */
function onKey(e: KeyboardEvent) {
  if (e.key === "Escape") {
    if (zoomed.value !== null) zoomed.value = null;
    else if (comparing.value) { comparing.value = false; selected.value = []; }
    else emit("close");
  }
}

const comparedList = computed(() =>
  selected.value
    .map((ts) => history.value.entries.find((x) => x.ts === ts))
    .filter((x): x is HistoryEntry => !!x),
);
</script>

<template>
  <div class="hc-wrap" tabindex="0" @keydown="onKey">
    <!-- 四宫格对比视图 -->
    <div v-if="comparing" class="hc-compare">
      <div class="hc-compare-head">
        <span class="hc-title">参数对比（{{ comparedList.length }}/4）</span>
        <n-button size="tiny" quaternary @click="comparing = false; selected = []">
          退出对比
        </n-button>
      </div>
      <div class="hc-grid" :class="{ [`n${comparedList.length}`]: true }">
        <div
          v-for="e in comparedList"
          :key="e.ts"
          class="hc-cell"
          @click="zoomed = e.ts"
        >
          <img v-if="compareUrls[e.ts]" :src="compareUrls[e.ts]" draggable="false" />
          <div v-else class="hc-cell-loading">渲染中…</div>
          <span class="hc-cell-tag">{{ summarize(e) }}</span>
        </div>
      </div>
      <!-- 放大单张（点击宫格） -->
      <div v-if="zoomed !== null" class="hc-zoom" @click="zoomed = null">
        <img
          v-if="compareUrls[zoomed]"
          :src="compareUrls[zoomed]"
          draggable="false"
        />
        <span v-if="zoomed !== null" class="hc-zoom-tag">
          {{ summarize(comparedList.find((x) => x.ts === zoomed)!) }}
        </span>
      </div>
    </div>

    <!-- 历史卡片视图（双列 + 自动预览图） -->
    <div v-else class="hc-list">
      <div class="hc-head">
        <span class="hc-title">调参历史（{{ history.entries.length }}）</span>
        <span class="hc-hint">点击应用 · 勾选对比</span>
        <button class="hc-close" title="关闭" @click="emit('close')">
          <n-icon :component="X" size="14" />
        </button>
      </div>
      <n-empty v-if="history.entries.length === 0" size="small" description="暂无调整记录（调节滑杆后自动保存）" class="hc-empty" />
      <div v-else class="hc-cards">
        <div
          v-for="e in history.entries"
          :key="e.ts"
          class="hc-card"
          :class="{
            active: selected.includes(e.ts),
            applied: e.ts === history.applied,
          }"
          @click="emit('apply', e)"
        >
          <!-- 预览图（后台自动渲染；占位渐显） -->
          <div class="hc-card-preview">
            <img
              v-if="previewUrls[e.ts]"
              :src="previewUrls[e.ts]"
              draggable="false"
            />
            <div v-else class="hc-card-ph">渲染中…</div>
            <n-tag v-if="e.ts === history.applied" size="tiny" type="success" :bordered="false" class="hc-cur">
              当前
            </n-tag>
            <input
              type="checkbox"
              class="hc-check"
              title="勾选加入对比"
              :checked="selected.includes(e.ts)"
              @click.stop
              @change="toggle(e.ts)"
            />
          </div>
          <!-- 参数 + 操作 -->
          <div class="hc-card-info">
            <div class="hc-card-params" :title="summarize(e)">{{ summarize(e) }}</div>
            <div class="hc-card-time">{{ fmtTime(e.ts) }}</div>
          </div>
          <div class="hc-card-actions">
            <button
              class="hc-act"
              title="按此参数输出 PNG 到图片所在目录"
              :disabled="exporting.has(e.ts)"
              @click.stop="exportPng(e)"
            >
              <n-icon :component="Download" size="13" />
              <span>{{ exporting.has(e.ts) ? "输出中…" : "重生图" }}</span>
            </button>
            <button class="hc-del" title="删除这条记录" @click.stop="removeEntry(e.ts)">
              <n-icon :component="Trash" size="12" />
            </button>
          </div>
        </div>
      </div>
      <div class="hc-foot">
        <n-button
          size="small"
          :disabled="selected.length < 2"
          @click="startCompare"
        >
          <template #icon>
            <n-icon :component="LayoutGrid" size="14" />
          </template>
          对比（{{ selected.length }}）
        </n-button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.hc-wrap {
  width: 340px;
  max-height: 74vh;
  display: flex;
  flex-direction: column;
  background: rgba(255, 255, 255, 0.92);
  backdrop-filter: blur(16px);
  border: 1px solid rgba(0, 0, 0, 0.08);
  border-radius: 10px;
  box-shadow: 0 8px 28px rgba(0, 0, 0, 0.16);
  overflow: hidden;
}
:global(html.dark) .hc-wrap {
  background: rgba(30, 32, 38, 0.94);
  border-color: rgba(255, 255, 255, 0.08);
}
.hc-head,
.hc-compare-head {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 10px 12px 8px;
  flex-shrink: 0;
}
.hc-title {
  font-size: 13px;
  font-weight: 600;
}
.hc-hint {
  font-size: 11px;
  opacity: 0.55;
}
.hc-close {
  margin-left: auto;
  border: none;
  background: none;
  cursor: pointer;
  padding: 4px;
  border-radius: 6px;
  color: inherit;
  opacity: 0.65;
}
.hc-close:hover {
  opacity: 1;
  background: rgba(0, 0, 0, 0.06);
}

.hc-empty {
  padding: 24px 0;
}

/* 双列卡片 */
.hc-cards {
  overflow-y: auto;
  padding: 0 8px;
  flex: 1;
  min-height: 0;
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 8px;
  align-content: start;
}
.hc-card {
  display: flex;
  flex-direction: column;
  border-radius: 10px;
  border: 1.5px solid rgba(0, 0, 0, 0.07);
  background: rgba(255, 255, 255, 0.55);
  cursor: pointer;
  overflow: hidden;
  transition: border-color 120ms ease, background 120ms ease, transform 120ms ease;
}
.hc-card:hover {
  transform: translateY(-1px);
  background: rgba(255, 255, 255, 0.85);
}
:global(html.dark) .hc-card {
  border-color: rgba(255, 255, 255, 0.07);
  background: rgba(255, 255, 255, 0.03);
}
:global(html.dark) .hc-card:hover {
  background: rgba(255, 255, 255, 0.06);
}
.hc-card.active {
  border-color: var(--jb-primary, #4080ff);
  background: rgba(64, 128, 255, 0.08);
}
.hc-card.applied:not(.active) {
  border-color: rgba(64, 178, 107, 0.5);
}
.hc-card-preview {
  position: relative;
  aspect-ratio: 16 / 10;
  background: #000;
  display: flex;
  align-items: center;
  justify-content: center;
}
.hc-card-preview img {
  width: 100%;
  height: 100%;
  object-fit: contain;
}
.hc-card-ph {
  color: rgba(255, 255, 255, 0.45);
  font-size: 11px;
}
.hc-cur {
  position: absolute;
  top: 5px;
  left: 5px;
}
.hc-check {
  position: absolute;
  top: 5px;
  right: 5px;
  accent-color: var(--jb-primary, #4080ff);
}
.hc-card-info {
  padding: 6px 8px 2px;
  min-width: 0;
}
.hc-card-params {
  font-size: 11.5px;
  font-weight: 550;
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.hc-card-time {
  font-size: 10px;
  opacity: 0.5;
  font-variant-numeric: tabular-nums;
}
.hc-card-actions {
  display: flex;
  align-items: center;
  gap: 4px;
  padding: 4px 8px 7px;
}
.hc-act {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  border: 1px solid rgba(0, 0, 0, 0.1);
  background: rgba(255, 255, 255, 0.7);
  border-radius: 999px;
  padding: 2px 8px;
  font-size: 10.5px;
  cursor: pointer;
  color: inherit;
  transition: border-color 0.15s, color 0.15s;
}
.hc-act:hover:not(:disabled) {
  border-color: var(--jb-primary, #4080ff);
  color: var(--jb-primary, #4080ff);
}
.hc-act:disabled {
  opacity: 0.55;
  cursor: default;
}
:global(html.dark) .hc-act {
  border-color: rgba(255, 255, 255, 0.12);
  background: rgba(255, 255, 255, 0.05);
}
.hc-del {
  margin-left: auto;
  border: none;
  background: none;
  padding: 4px;
  border-radius: 6px;
  cursor: pointer;
  color: inherit;
  opacity: 0;
  flex-shrink: 0;
}
.hc-card:hover .hc-del {
  opacity: 0.55;
}
.hc-del:hover {
  opacity: 1;
  background: rgba(220, 50, 50, 0.12);
}
.hc-foot {
  padding: 8px 12px;
  display: flex;
  justify-content: flex-end;
  flex-shrink: 0;
}

/* 四宫格（覆盖式大面板） */
.hc-compare {
  position: fixed;
  inset: 0;
  z-index: 200;
  background: rgba(0, 0, 0, 0.72);
  backdrop-filter: blur(8px);
  display: flex;
  flex-direction: column;
}
.hc-compare-head {
  color: #fff;
}
.hc-compare-head .hc-title {
  color: #fff;
}
.hc-grid {
  flex: 1;
  min-height: 0;
  display: grid;
  gap: 10px;
  padding: 0 24px 24px;
}
.hc-grid.n2 { grid-template-columns: 1fr 1fr; }
.hc-grid.n3,
.hc-grid.n4 { grid-template-columns: 1fr 1fr; }
.hc-cell {
  position: relative;
  background: #000;
  border-radius: 8px;
  overflow: hidden;
  cursor: zoom-in;
  display: flex;
  align-items: center;
  justify-content: center;
}
.hc-cell img {
  max-width: 100%;
  max-height: 100%;
  object-fit: contain;
}
.hc-cell-loading {
  color: rgba(255, 255, 255, 0.5);
  font-size: 13px;
}
.hc-cell-tag {
  position: absolute;
  left: 8px;
  bottom: 8px;
  padding: 3px 9px;
  border-radius: 999px;
  font-size: 11px;
  font-variant-numeric: tabular-nums;
  color: #fff;
  background: rgba(0, 0, 0, 0.62);
  backdrop-filter: blur(6px);
}
/* 放大单张 */
.hc-zoom {
  position: fixed;
  inset: 0;
  z-index: 201;
  background: rgba(0, 0, 0, 0.88);
  display: flex;
  align-items: center;
  justify-content: center;
  cursor: zoom-out;
}
.hc-zoom img {
  max-width: 92vw;
  max-height: 92vh;
  object-fit: contain;
}
.hc-zoom-tag {
  position: absolute;
  left: 50%;
  transform: translateX(-50%);
  bottom: 22px;
  padding: 4px 12px;
  border-radius: 999px;
  font-size: 12px;
  color: #fff;
  background: rgba(0, 0, 0, 0.62);
  backdrop-filter: blur(6px);
}
</style>
