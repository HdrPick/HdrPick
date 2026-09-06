<script setup lang="ts">
// 相册首页（设计稿第六节主界面）：左侧导航 + 中间内容区 + 右侧详情栏 + 底部状态栏
// 数据流：library_state / scan_library 拉全量条目与元数据 → 前端过滤（导航分类 + 文件名搜索）
// 缩略图由 get_thumbnail 懒加载（IntersectionObserver + LRU 缓存，参考 ThumbnailBar）
// 后端命令未就绪时 try/catch + useMessage 容错；全中文 UI
import { ref, reactive, computed, watch, nextTick, onMounted, onUnmounted } from "vue";
import type { Component } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { open as openFileDialog, save as saveFileDialog } from "@tauri-apps/plugin-dialog";
import { useMessage, useDialog, NIcon, NInput, NEmpty, NSpin, NButton } from "naive-ui";
import {
  Photo, Star, Tag, Tags, Robot, Trash, Lock,
  ChevronsLeft, ChevronsRight, Search, LayoutGrid, LayoutList, CalendarEvent,
  Refresh, FolderPlus, Folder, Eye, Pencil, GridDots, Copy, X, Rotate, Printer,
  Plus, Upload, Download,
} from "@vicons/tabler";
// 依赖组件（由他人提供，模块不存在属预期）
import BatchPanel from "./BatchPanel.vue";
import VaultPanel from "./VaultPanel.vue";

const props = defineProps<{
  /** 相册页是否可见（父组件控制；首次可见时自动加载） */
  visible: boolean;
}>();

const emit = defineEmits<{
  (e: "open-image", path: string): void;
  (e: "edit-image", path: string): void;
  (e: "collage", paths: string[]): void;
  (e: "print", paths: string[]): void;
}>();

const message = useMessage();
const dialog = useDialog();

// === 后端数据结构（与 Rust 端 serde 命名对齐） ===
interface LibEntry {
  path: string;
  name: string;
  dir: string;
  width: number;
  height: number;
  format: string;
  mtime: number;
}
interface LibMeta {
  stars: number;
  tags: string[];
}
interface LibraryState {
  roots: string[];
  entries: LibEntry[];
  metas: Record<string, LibMeta>;
}
interface ExifData {
  groups: Array<{ title: string; items: Array<{ label: string; value: string }> }>;
}
interface RecycleEntry {
  original_path: string;
  recycled_name: string;
  deleted_ts: number;
  display_name: string;
}

// === 视图定义 ===
type ViewMode = "grid" | "list" | "timeline";
type NavKey = "all" | "favorites" | "smart" | "recycle" | "vault" | "tag" | "album";
const VIEW_DEFS: Array<{ key: ViewMode; label: string; icon: Component; hint: string }> = [
  { key: "grid", label: "网格", icon: LayoutGrid, hint: "网格视图（Ctrl+滚轮调整列数）" },
  { key: "list", label: "列表", icon: LayoutList, hint: "列表视图" },
  { key: "timeline", label: "时间线", icon: CalendarEvent, hint: "时间线视图" },
];

// === 状态 ===
const roots = ref<string[]>([]);
const entries = ref<LibEntry[]>([]);
const metas = ref<Record<string, LibMeta>>({});
/** 首次读取中（内容区显示骨架加载） */
const loading = ref(false);
/** 重新扫描中（工具栏按钮转圈） */
const scanning = ref(false);

const navKey = ref<NavKey>("all");
const activeTag = ref("");
/** 当前筛选的相册源（空 = 全部源混合显示；点左侧相册源切换） */
const activeRoot = ref("");
const navCollapsed = ref(localStorage.getItem("jietu-library:nav-collapsed") === "1");
const search = ref("");
const viewMode = ref<ViewMode>(
  (["grid", "list", "timeline"] as const).includes(
    localStorage.getItem("jietu-library:view") as ViewMode,
  )
    ? (localStorage.getItem("jietu-library:view") as ViewMode)
    : "grid",
);
/** 网格列数 2-8（Ctrl+滚轮调整，存 localStorage） */
const gridCols = ref(
  Math.min(8, Math.max(2, Number(localStorage.getItem("jietu-library:cols")) || 5)),
);

/** 选中路径集合（替换式更新保证响应） */
const selected = ref<Set<string>>(new Set());
/** Shift 范围选择锚点 */
const lastClickPath = ref("");

/** 右侧详情栏收起态 */
const detailOpen = ref(true);
const exifSummary = ref<Array<{ label: string; value: string }>>([]);
const detailTagInput = ref("");

/** 批量面板（BatchPanel）显示态 */
const showBatch = ref(false);
/** 批量打标签输入框展开态 */
const batchTagOpen = ref(false);
const batchTagInput = ref("");

/** 回收站列表 */
const recycleList = ref<RecycleEntry[]>([]);

// === 逻辑相册（虚拟文件夹树，不对应磁盘任何目录；条目只存实际 path） ===
interface AlbumItem {
  path: string;
  name: string;
  width: number;
  height: number;
  format: string;
  mtime: number;
  added: number;
}
interface AlbumFolder {
  id: string;
  name: string;
  children: AlbumFolder[];
  items: AlbumItem[];
}
interface AlbumFile {
  version: number;
  folders: AlbumFolder[];
}
const albums = ref<AlbumFile>({ version: 1, folders: [] });
/** 当前浏览的逻辑相册 id（空 = 未进入相册视图） */
const activeAlbumId = ref("");

interface AlbumRow {
  folder: AlbumFolder;
  depth: number;
  /** 含子孙的图片总数 */
  count: number;
}
/** 树 → 缩进行列表（左导航渲染用） */
const albumRows = computed<AlbumRow[]>(() => {
  const countOf = (f: AlbumFolder): number =>
    f.items.length + f.children.reduce((s, c) => s + countOf(c), 0);
  const out: AlbumRow[] = [];
  const walk = (fs: AlbumFolder[], depth: number) => {
    for (const f of fs) {
      out.push({ folder: f, depth, count: countOf(f) });
      walk(f.children, depth + 1);
    }
  };
  walk(albums.value.folders, 0);
  return out;
});

function albumFolderById(id: string): AlbumFolder | null {
  const walk = (fs: AlbumFolder[]): AlbumFolder | null => {
    for (const f of fs) {
      if (f.id === id) return f;
      const hit = walk(f.children);
      if (hit) return hit;
    }
    return null;
  };
  return walk(albums.value.folders);
}
const activeAlbumFolder = computed(() => albumFolderById(activeAlbumId.value));

/** 当前相册条目 → 展示用 LibEntry（伪条目：dir 留空，mtime 用收录时快照） */
const albumEntries = computed<LibEntry[]>(() =>
  (activeAlbumFolder.value?.items ?? []).map((it) => ({
    path: it.path,
    name: it.name,
    dir: "",
    width: it.width,
    height: it.height,
    format: it.format,
    mtime: it.mtime,
  })),
);

const contentRef = ref<HTMLElement | null>(null);

// === 元数据取值（缺省 0 星 / 无标签） ===
function metaOf(path: string): LibMeta {
  return metas.value[path] ?? { stars: 0, tags: [] };
}

// === 派生数据 ===
/** 全部标签（去重排序，左导航动态列出） */
const allTags = computed(() => {
  const set = new Set<string>();
  for (const m of Object.values(metas.value)) m.tags?.forEach((t) => set.add(t));
  return [...set].sort((a, b) => a.localeCompare(b, "zh"));
});
const starredCount = computed(
  () => entries.value.filter((e) => metaOf(e.path).stars > 0).length,
);

/** 导航分类 + 相册源 + 文件名搜索过滤后的条目 */
const filteredEntries = computed(() => {
  // 逻辑相册模式：直接展示当前相册条目（不参与相册源/收藏/标签筛选）
  if (navKey.value === "album") {
    let list = albumEntries.value;
    const kw = search.value.trim().toLowerCase();
    if (kw) list = list.filter((e) => e.name.toLowerCase().includes(kw));
    return list;
  }
  let list = entries.value;
  // 相册源筛选（忽略大小写 + 尾部 \ 兼容）
  if (activeRoot.value) {
    const p = activeRoot.value.toLowerCase().replace(/\\+$/, "") + "\\";
    list = list.filter((e) => e.path.toLowerCase().startsWith(p));
  }
  if (navKey.value === "favorites") list = list.filter((e) => metaOf(e.path).stars > 0);
  else if (navKey.value === "tag") {
    const t = activeTag.value;
    list = list.filter((e) => metaOf(e.path).tags?.includes(t));
  }
  const kw = search.value.trim().toLowerCase();
  if (kw) list = list.filter((e) => e.name.toLowerCase().includes(kw));
  return list;
});

/** 智能分类：按 format 分组 */
const formatGroups = computed(() => {
  const map = new Map<string, LibEntry[]>();
  for (const e of filteredEntries.value) {
    const f = (e.format || "?").toUpperCase();
    const arr = map.get(f);
    if (arr) arr.push(e);
    else map.set(f, [e]);
  }
  return [...map.entries()]
    .sort((a, b) => a[0].localeCompare(b[0]))
    .map(([f, items]) => ({ key: `fmt-${f}`, title: f, items }));
});

// === 时间工具（秒/毫秒自适应） ===
function dateOf(ts: number): Date {
  return new Date(ts > 1e12 ? ts : ts * 1000);
}
function fmtTime(ts: number): string {
  const d = dateOf(ts);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}
function dateKey(ts: number): string {
  const d = dateOf(ts);
  return `${d.getFullYear()}-${d.getMonth() + 1}-${d.getDate()}`;
}
function dateLabel(ts: number): string {
  const d = dateOf(ts);
  const w = "日一二三四五六"[d.getDay()];
  return `${d.getFullYear()}年${d.getMonth() + 1}月${d.getDate()}日 · 周${w}`;
}

/** 时间线：按修改日期分组（新→旧，黏性标题） */
const timelineGroups = computed(() => {
  const sorted = [...filteredEntries.value].sort((a, b) => b.mtime - a.mtime);
  const out: Array<{ key: string; title: string; items: LibEntry[] }> = [];
  for (const e of sorted) {
    const k = dateKey(e.mtime);
    const last = out[out.length - 1];
    if (last && last.key === k) last.items.push(e);
    else out.push({ key: `d-${k}`, title: dateLabel(e.mtime), items: [e] });
  }
  return out;
});

/** 实际渲染分组：智能分类按格式 / 时间线按日期 / 其余平铺单组 */
const renderGroups = computed(() => {
  if (navKey.value === "smart") return formatGroups.value;
  if (viewMode.value === "timeline") return timelineGroups.value;
  return [{ key: "__all__", title: "", items: filteredEntries.value }];
});

/** Shift 范围选择使用的展示顺序 */
const displayOrder = computed(() => renderGroups.value.flatMap((g) => g.items));

const selectedPaths = computed(() => [...selected.value]);
/** 恰好选中一张时的详情对象 */
const singleEntry = computed(() => {
  if (selected.value.size !== 1) return null;
  const p = [...selected.value][0];
  return entries.value.find((e) => e.path === p) ?? null;
});

const gridStyle = computed(() => ({
  gridTemplateColumns: `repeat(${gridCols.value}, minmax(0, 1fr))`,
}));

const statusText = computed(() => {
  const k = roots.value.length;
  if (navKey.value === "recycle") return `回收站 ${recycleList.value.length} 项 · 相册源 ${k} 个`;
  if (navKey.value === "vault") return `隐私相册 · 相册源 ${k} 个`;
  if (navKey.value === "album") {
    const f = activeAlbumFolder.value;
    return f
      ? `相册「${f.name}」${filteredEntries.value.length} 张 · 已选 ${selected.value.size}`
      : "我的相册";
  }
  return `共 ${filteredEntries.value.length} 张 · 已选 ${selected.value.size} · 相册源 ${k} 个`;
});

const emptyText = computed(() => {
  const kw = search.value.trim();
  if (kw) return `没有找到匹配「${kw}」的图片，换个关键词试试`;
  if (navKey.value === "album")
    return activeAlbumFolder.value
      ? "这个相册还是空的，点上方「添加图片」把散落各处的照片收进来"
      : "请选择左侧相册";
  if (navKey.value === "favorites") return "还没有星标图片，点亮卡片角上的小星星收藏喜欢的照片";
  if (navKey.value === "tag") return `标签「${activeTag.value}」下还没有图片`;
  if (entries.value.length === 0 && roots.value.length > 0)
    return "相册源里还没有发现图片，点右上角「刷新扫描」重新整理";
  return "这里还没有图片";
});

// === 缩略图：IntersectionObserver 懒加载 + LRU 缓存（同 ThumbnailBar） ===
const thumbs = ref<Record<string, string>>({});
const cache = new Map<string, string>();
// 与后端 LRU 上限一致（400px 高清缩略图，600 张 ≈ 24MB 内存）
const LRU_MAX = 600;
const pending = new Set<string>();
let observer: IntersectionObserver | null = null;

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

/** 观察内容区全部懒加载缩略图元素（重复 observe 幂等） */
function observeAll() {
  const root = contentRef.value;
  if (!root) return;
  if (!observer) {
    observer = new IntersectionObserver(
      (observed) => {
        for (const en of observed) {
          if (!en.isIntersecting) continue;
          const path = (en.target as HTMLElement).dataset.path;
          if (path) requestThumb(path);
          observer?.unobserve(en.target);
        }
      },
      { root, rootMargin: "160px" },
    );
  }
  root.querySelectorAll<HTMLElement>(".lazy-thumb").forEach((el) => observer?.observe(el));
}

// 渲染分组 / 列数变化后重新挂观察器
watch(
  [renderGroups, gridCols],
  () => nextTick(observeAll),
  { immediate: true },
);

// 过滤结果变化后修剪选中集（避免「已选」里混入不可见条目）
watch(filteredEntries, (list) => {
  const ok = new Set(list.map((e) => e.path));
  const keep = [...selected.value].filter((p) => ok.has(p));
  if (keep.length !== selected.value.size) selected.value = new Set(keep);
});

// === 数据加载 ===
let loaded = false;

function applyState(st: LibraryState) {
  roots.value = st.roots ?? [];
  entries.value = st.entries ?? [];
  metas.value = st.metas ?? {};
}

/** 读取相册（useScan=true 时走 scan_library 全量重扫）；返回是否成功 */
async function refresh(useScan = false): Promise<boolean> {
  if (useScan) scanning.value = true;
  loading.value = true;
  loadAlbums(); // 逻辑相册配置一并拉取（互不阻塞）
  try {
    const st = await invoke<LibraryState>(useScan ? "scan_library" : "library_state");
    applyState(st);
    return true;
  } catch (e) {
    message.error(`读取相册失败: ${e}`, { duration: 2200 });
    return false;
  } finally {
    loading.value = false;
    scanning.value = false;
  }
}

async function rescan() {
  if (await refresh(true)) message.success("扫描完成", { duration: 2200 });
}

// === 逻辑相册数据加载与操作 ===
async function loadAlbums() {
  try {
    albums.value = await invoke<AlbumFile>("albums_state");
  } catch {
    // 后端命令未就绪时容错：保持空相册
  }
}

/** 轻量输入弹窗（naive useDialog 无输入框，自实现遮罩 + NInput） */
const promptState = reactive<{
  show: boolean;
  title: string;
  text: string;
  onOk: null | ((v: string) => void);
}>({ show: false, title: "", text: "", onOk: null });

function askPrompt(title: string, def: string, onOk: (v: string) => void) {
  promptState.title = title;
  promptState.text = def;
  promptState.onOk = onOk;
  promptState.show = true;
}

function promptOk() {
  const v = promptState.text.trim();
  if (!v) {
    message.error("名称不能为空", { duration: 1600 });
    return;
  }
  promptState.show = false;
  promptState.onOk?.(v);
}

/** 统一调用相册变更命令并回填状态 */
async function applyAlbums(p: Promise<AlbumFile>, okMsg?: string) {
  try {
    albums.value = await p;
    if (okMsg) message.success(okMsg, { duration: 1800 });
  } catch (e) {
    message.error(`相册操作失败: ${e}`, { duration: 2200 });
  }
}

function switchAlbum(id: string) {
  if (navKey.value === "album" && activeAlbumId.value === id) return;
  activeAlbumId.value = id;
  navKey.value = "album";
  clearSelection();
}

function createAlbumFolder(parentId: string | null) {
  askPrompt(parentId ? "新建子相册" : "新建相册", "", (name) => {
    applyAlbums(
      invoke<AlbumFile>("album_create_folder", { parentId, name }),
      "已创建相册",
    );
  });
}

function renameAlbumFolder(id: string, cur: string) {
  askPrompt("重命名相册", cur, (name) => {
    applyAlbums(invoke<AlbumFile>("album_rename_folder", { id, name }));
  });
}

function deleteAlbumFolder(f: AlbumFolder) {
  dialog.warning({
    title: "删除相册",
    content: `「${f.name}」及其子相册将被删除。仅删除逻辑组织方式，磁盘上的图片文件不会被移动或删除。`,
    positiveText: "删除",
    negativeText: "取消",
    onPositiveClick: async () => {
      if (activeAlbumId.value === f.id) {
        activeAlbumId.value = "";
        navKey.value = "all";
      }
      await applyAlbums(invoke<AlbumFile>("album_delete_folder", { id: f.id }));
    },
  });
}

/** 收录图片：多选文件 → album_add_items（后端快照宽高/格式/mtime） */
async function addAlbumImages() {
  const fid = activeAlbumId.value;
  if (!fid) return;
  try {
    const sel = await openFileDialog({
      multiple: true,
      filters: [
        {
          name: "图片",
          extensions: ["png", "jpg", "jpeg", "gif", "bmp", "webp", "avif", "tif", "tiff", "ico", "psd", "jxl"],
        },
      ],
    });
    const paths = (Array.isArray(sel) ? sel : sel ? [sel] : []) as string[];
    if (paths.length === 0) return;
    const before = albumFolderById(fid)?.items.length ?? 0;
    await applyAlbums(invoke<AlbumFile>("album_add_items", { folderId: fid, paths }));
    const after = albumFolderById(fid)?.items.length ?? 0;
    const n = after - before;
    if (n > 0) message.success(`已收录 ${n} 张进相册`, { duration: 1800 });
    else message.info("没有新收录的图片（可能已收录过或格式不支持）", { duration: 2200 });
  } catch (e) {
    message.error(`添加失败: ${e}`, { duration: 2200 });
  }
}

/** 移出相册（仅移出引用，磁盘文件不动） */
async function removeAlbumItems() {
  const fid = activeAlbumId.value;
  if (!fid) return;
  const paths = selectedPaths.value;
  await applyAlbums(
    invoke<AlbumFile>("album_remove_items", { folderId: fid, paths }),
    `已移出 ${paths.length} 张`,
  );
  clearSelection();
}

/** 导入相册配置 JSON（替换 / 合并两种方式） */
async function importAlbums() {
  try {
    const sel = await openFileDialog({
      multiple: false,
      filters: [{ name: "相册配置", extensions: ["json"] }],
    });
    if (typeof sel !== "string" || !sel) return;
    dialog.warning({
      title: "导入相册配置",
      content: `将导入「${baseName(sel)}」。替换 = 覆盖当前全部相册组织（建议先导出备份）；合并 = 追加为新的顶层相册。`,
      positiveText: "替换导入",
      negativeText: "合并导入",
      onPositiveClick: () =>
        applyAlbums(invoke<AlbumFile>("albums_import", { path: sel, mode: "replace" }), "导入完成（替换）"),
      onNegativeClick: () =>
        applyAlbums(invoke<AlbumFile>("albums_import", { path: sel, mode: "merge" }), "导入完成（合并）"),
    });
  } catch (e) {
    message.error(`导入失败: ${e}`, { duration: 2200 });
  }
}

/** 导出当前相册配置 JSON */
async function exportAlbums() {
  try {
    const p = await saveFileDialog({
      defaultPath: "我的相册.json",
      filters: [{ name: "相册配置", extensions: ["json"] }],
    });
    if (typeof p === "string" && p) {
      await invoke("albums_export", { path: p });
      message.success("已导出相册配置", { duration: 1800 });
    }
  } catch (e) {
    message.error(`导出失败: ${e}`, { duration: 2200 });
  }
}

// 首次可见时自动加载
watch(
  () => props.visible,
  (v) => {
    if (v && !loaded) {
      loaded = true;
      refresh();
    }
  },
  { immediate: true },
);

// === 相册源管理 ===
async function addRoot() {
  try {
    const sel = await openFileDialog({ directory: true, multiple: false });
    if (typeof sel === "string" && sel) {
      await invoke("add_library_root", { path: sel });
      message.success("已打开相册，正在扫描…", { duration: 2200 });
      await refresh(true);
    }
  } catch (e) {
    message.error(`添加相册源失败: ${e}`, { duration: 2200 });
  }
}

function removeRoot(path: string) {
  dialog.warning({
    title: "移除相册源",
    content: `移除后「${baseName(path)}」下的图片将不再显示（文件不会被删除）。`,
    positiveText: "移除",
    negativeText: "取消",
    onPositiveClick: async () => {
      try {
        await invoke("remove_library_root", { path });
        // 移除的正是当前筛选源 → 清筛选回全部
        if (activeRoot.value === path) activeRoot.value = "";
        message.success("已移除相册源", { duration: 2200 });
        await refresh();
      } catch (e) {
        message.error(`移除相册源失败: ${e}`, { duration: 2200 });
      }
    },
  });
}

function baseName(p: string): string {
  return p.split(/[\\/]/).pop() ?? p;
}

// === 导航切换 ===
function switchNav(k: NavKey, tag?: string) {
  navKey.value = k;
  if (k === "tag" && tag) activeTag.value = tag;
  clearSelection();
  if (k === "recycle") refreshRecycle();
}

/** 切换相册源筛选（同源再点 = 取消筛选回全部） */
function switchRoot(path: string) {
  activeRoot.value = activeRoot.value === path ? "" : path;
  navKey.value = "all";
  clearSelection();
}

function toggleNavCollapsed() {
  navCollapsed.value = !navCollapsed.value;
  localStorage.setItem("jietu-library:nav-collapsed", navCollapsed.value ? "1" : "0");
}

function setView(v: ViewMode) {
  viewMode.value = v;
  localStorage.setItem("jietu-library:view", v);
}

// === 选中逻辑（单击 / Ctrl 多选 / Shift 范围 / Ctrl+A 全选） ===
function setSelection(paths: string[]) {
  selected.value = new Set(paths);
}
function clearSelection() {
  selected.value = new Set();
  batchTagOpen.value = false;
}

function onCardClick(entry: LibEntry, ev: MouseEvent) {
  if (ev.ctrlKey || ev.metaKey) {
    const s = new Set(selected.value);
    if (s.has(entry.path)) s.delete(entry.path);
    else s.add(entry.path);
    selected.value = s;
    lastClickPath.value = entry.path;
  } else if (ev.shiftKey) {
    const order = displayOrder.value;
    const anchor = order.findIndex((x) => x.path === lastClickPath.value);
    const idx = order.findIndex((x) => x.path === entry.path);
    if (anchor < 0 || idx < 0) {
      setSelection([entry.path]);
    } else {
      const [a, b] = anchor <= idx ? [anchor, idx] : [idx, anchor];
      const s = new Set(selected.value);
      for (let i = a; i <= b; i++) s.add(order[i].path);
      selected.value = s;
    }
    lastClickPath.value = entry.path;
  } else {
    setSelection([entry.path]);
    lastClickPath.value = entry.path;
  }
}

function selectAll() {
  selected.value = new Set(filteredEntries.value.map((e) => e.path));
}

function onKeydown(e: KeyboardEvent) {
  if (!props.visible) return;
  const tag = (e.target as HTMLElement)?.tagName;
  if (tag === "INPUT" || tag === "TEXTAREA") return;
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "a") {
    if (navKey.value === "recycle" || navKey.value === "vault") return;
    e.preventDefault();
    selectAll();
  } else if (e.key === "Escape") {
    clearSelection();
  }
}

// === 网格 Ctrl+滚轮调列数（2-8，存 localStorage） ===
function onContentWheel(e: WheelEvent) {
  if (!e.ctrlKey) return;
  e.preventDefault(); // 拦截浏览器页面缩放
  const next = Math.min(8, Math.max(2, gridCols.value + (e.deltaY < 0 ? 1 : -1)));
  if (next !== gridCols.value) {
    gridCols.value = next;
    localStorage.setItem("jietu-library:cols", String(next));
  }
}

// === 元数据写入（乐观更新，失败回滚） ===
async function pushMeta(path: string, stars: number, tags: string[]): Promise<boolean> {
  const prev = metas.value[path];
  metas.value = { ...metas.value, [path]: { stars, tags } };
  try {
    await invoke("set_meta", { path, stars, tags });
    return true;
  } catch (e) {
    const m = { ...metas.value };
    if (prev) m[path] = prev;
    else delete m[path];
    metas.value = m;
    message.error(`保存失败: ${e}`, { duration: 2200 });
    return false;
  }
}

/** 卡片角标星标切换（0 ↔ 1 星；详情栏可精确评 1-5 星） */
async function toggleStar(path: string) {
  const m = metaOf(path);
  await pushMeta(path, m.stars > 0 ? 0 : 1, m.tags ?? []);
}

/** 详情栏评分：点击当前星级归零，否则评 n 星 */
async function rate(n: number) {
  const p = singleEntry.value?.path;
  if (!p) return;
  const m = metaOf(p);
  await pushMeta(p, m.stars === n ? 0 : n, m.tags ?? []);
}

async function addTagDetail() {
  const p = singleEntry.value?.path;
  const t = detailTagInput.value.trim();
  if (!p || !t) return;
  const m = metaOf(p);
  if (m.tags?.includes(t)) {
    message.info("这个标签已经加过了", { duration: 1800 });
    detailTagInput.value = "";
    return;
  }
  detailTagInput.value = "";
  await pushMeta(p, m.stars, [...(m.tags ?? []), t]);
}

async function removeTagDetail(t: string) {
  const p = singleEntry.value?.path;
  if (!p) return;
  const m = metaOf(p);
  await pushMeta(p, m.stars, (m.tags ?? []).filter((x) => x !== t));
}

// === 详情栏：EXIF 摘要（前 8 条）+ 大缩略图 ===
watch(
  () => singleEntry.value?.path,
  (p) => {
    exifSummary.value = [];
    detailTagInput.value = "";
    if (!p) return;
    requestThumb(p); // 详情大图直接请求（不在滚动容器内）
    invoke<ExifData>("get_exif", { path: p })
      .then((d) => {
        exifSummary.value = d.groups.flatMap((g) => g.items).slice(0, 8);
      })
      .catch(() => {
        exifSummary.value = []; // 后端未就绪容错
      });
  },
);

async function copyPath() {
  const p = singleEntry.value?.path;
  if (!p) return;
  try {
    await navigator.clipboard.writeText(p);
    message.success("路径已复制", { duration: 1800 });
  } catch {
    message.error("复制失败", { duration: 1800 });
  }
}

// === 删除（单张 → 软件回收站，7 天可恢复） ===
function dropPaths(paths: string[]) {
  const set = new Set(paths);
  entries.value = entries.value.filter((e) => !set.has(e.path));
  const m = { ...metas.value };
  paths.forEach((p) => delete m[p]);
  metas.value = m;
  clearSelection();
}

function deleteOne(path: string) {
  const name = baseName(path);
  dialog.warning({
    title: "移入回收站",
    content: `「${name}」将移入软件回收站，7 天内可恢复。`,
    positiveText: "移入回收站",
    negativeText: "取消",
    onPositiveClick: async () => {
      try {
        await invoke("recycle_image", { path });
        message.success(`已移入回收站：${name}`, { duration: 2200 });
        dropPaths([path]);
      } catch (e) {
        message.error(`删除失败: ${e}`, { duration: 2200 });
      }
    },
  });
}

// === 批量操作（多选 ≥1 时底部浮出工具条） ===
async function batchStar() {
  const paths = selectedPaths.value;
  let ok = 0;
  for (const p of paths) {
    const m = metaOf(p);
    if (await pushMeta(p, m.stars > 0 ? m.stars : 1, m.tags ?? [])) ok++;
  }
  if (ok > 0) message.success(`已为 ${ok} 张图片加星`, { duration: 2200 });
}

async function batchAddTagConfirm() {
  const t = batchTagInput.value.trim();
  if (!t) return;
  const paths = selectedPaths.value;
  let ok = 0;
  for (const p of paths) {
    const m = metaOf(p);
    if (!m.tags?.includes(t) && (await pushMeta(p, m.stars, [...(m.tags ?? []), t]))) ok++;
  }
  batchTagInput.value = "";
  batchTagOpen.value = false;
  if (ok > 0) message.success(`已为 ${ok} 张图片添加标签「${t}」`, { duration: 2200 });
}

function batchRecycle() {
  const paths = selectedPaths.value;
  dialog.warning({
    title: "批量移入回收站",
    content: `选中的 ${paths.length} 张图片将移入软件回收站，7 天内可恢复。`,
    positiveText: "移入回收站",
    negativeText: "取消",
    onPositiveClick: async () => {
      const results = await Promise.allSettled(
        paths.map((p) => invoke("recycle_image", { path: p })),
      );
      const done = paths.filter((_, i) => results[i].status === "fulfilled");
      const failed = results.length - done.length;
      if (done.length > 0) dropPaths(done);
      if (done.length > 0) message.success(`已移入回收站 ${done.length} 张`, { duration: 2200 });
      if (failed > 0) message.error(`${failed} 张删除失败`, { duration: 2200 });
    },
  });
}

/** 加入隐私相册（后端 vault_add：加密移入，资源管理器不再可见原文件） */
async function batchVault() {
  try {
    await invoke("vault_add", { paths: selectedPaths.value });
    message.success(`已加入隐私相册（${selectedPaths.value.length} 张）`, { duration: 2200 });
  } catch (e) {
    message.error(`加入隐私相册失败: ${e}`, { duration: 2200 });
  }
}

// === 回收站 ===
async function refreshRecycle() {
  try {
    recycleList.value = await invoke<RecycleEntry[]>("list_recycle");
  } catch (e) {
    recycleList.value = [];
    message.error(`读取回收站失败: ${e}`, { duration: 2200 });
  }
}

async function restoreOne(e: RecycleEntry) {
  try {
    await invoke("restore_recycle", { recycledName: e.recycled_name });
    message.success(`已恢复：${e.display_name}`, { duration: 2200 });
    refreshRecycle();
    refresh();
  } catch (err) {
    message.error(`恢复失败: ${err}`, { duration: 2200 });
    refreshRecycle();
  }
}

function purgeOne(e: RecycleEntry) {
  dialog.warning({
    title: "彻底删除",
    content: `「${e.display_name}」将被永久删除，无法恢复。`,
    positiveText: "彻底删除",
    negativeText: "取消",
    onPositiveClick: async () => {
      try {
        await invoke("purge_recycle", { recycledName: e.recycled_name });
        refreshRecycle();
      } catch (err) {
        message.error(`删除失败: ${err}`, { duration: 2200 });
      }
    },
  });
}

// === 工具 ===
function extLabel(e: LibEntry): string {
  return (e.format || e.name.split(".").pop() || "?").toUpperCase().slice(0, 5);
}

/** 热键唤起：切换到隐私相册导航（Ctrl+Shift+L → ViewerWindow 转发） */
function onVaultNav() {
  navKey.value = "vault";
}

/** 挂载新相册源后刷新列表（ViewerWindow 转发：主面板/工具栏「打开相册」、右键浏览）
 *  add_library_root 只登记根目录不扫描，必须走 scan_library 全量重扫，否则新目录的图片不会出现 */
function onLibraryRefresh() {
  refresh(true);
  if (navKey.value === "recycle") refreshRecycle();
}

onMounted(() => {
  window.addEventListener("keydown", onKeydown);
  // wheel 需非 passive 监听才能 preventDefault（Ctrl+滚轮调列数）
  contentRef.value?.addEventListener("wheel", onContentWheel, { passive: false });
  nextTick(observeAll);
  // 全局热键 Ctrl+Shift+L 唤起隐私相册（ViewerWindow 转发 jietu:vault-nav）
  window.addEventListener("jietu:vault-nav", onVaultNav);
  // 右键文件夹挂载相册源后刷新（ViewerWindow 转发 jietu:library-refresh）
  window.addEventListener("jietu:library-refresh", onLibraryRefresh);
});

onUnmounted(() => {
  window.removeEventListener("keydown", onKeydown);
  window.removeEventListener("jietu:vault-nav", onVaultNav);
  window.removeEventListener("jietu:library-refresh", onLibraryRefresh);
  contentRef.value?.removeEventListener("wheel", onContentWheel);
  observer?.disconnect();
  observer = null;
});
</script>
<template>
  <div v-show="visible" class="lib-root">
    <div class="lib-body">
      <!-- === 左侧导航（200px 可收起为 56px 图标列） === -->
      <aside class="lib-nav" :class="{ collapsed: navCollapsed }">
        <div class="nav-scroll">
          <button
            class="nav-item"
            :class="{ active: navKey === 'all' }"
            title="全部图片"
            @click="switchNav('all')"
          >
            <n-icon :component="Photo" size="17" />
            <span class="nav-text nav-label">全部图片</span>
            <span class="nav-text nav-count">{{ entries.length }}</span>
          </button>
          <button
            class="nav-item"
            :class="{ active: navKey === 'favorites' }"
            title="收藏（星标）"
            @click="switchNav('favorites')"
          >
            <n-icon :component="Star" size="17" />
            <span class="nav-text nav-label">收藏</span>
            <span class="nav-text nav-count">{{ starredCount }}</span>
          </button>

          <!-- 标签：从元数据动态列出 -->
          <div v-if="!navCollapsed && allTags.length > 0" class="nav-sec nav-text">标签</div>
          <template v-if="!navCollapsed">
            <button
              v-for="t in allTags"
              :key="t"
              class="nav-item sub"
              :class="{ active: navKey === 'tag' && activeTag === t }"
              :title="'# ' + t"
              @click="switchNav('tag', t)"
            >
              <n-icon :component="Tag" size="15" />
              <span class="nav-text nav-label ellip"># {{ t }}</span>
            </button>
          </template>

          <button
            class="nav-item"
            :class="{ active: navKey === 'smart' }"
            title="智能分类（按格式分组）"
            @click="switchNav('smart')"
          >
            <n-icon :component="Robot" size="17" />
            <span class="nav-text nav-label">智能分类</span>
          </button>
          <button
            class="nav-item"
            :class="{ active: navKey === 'recycle' }"
            title="回收站"
            @click="switchNav('recycle')"
          >
            <n-icon :component="Trash" size="17" />
            <span class="nav-text nav-label">回收站</span>
          </button>
          <button
            class="nav-item"
            :class="{ active: navKey === 'vault' }"
            title="隐私相册"
            @click="switchNav('vault')"
          >
            <n-icon :component="Lock" size="17" />
            <span class="nav-text nav-label">隐私相册</span>
          </button>

          <!-- 我的相册（逻辑相册：虚拟文件夹树，不对应磁盘目录） -->
          <div v-if="!navCollapsed" class="nav-sec nav-album-head">
            <span>我的相册</span>
            <span class="nav-album-ops">
              <button class="op-btn" title="导入相册配置 JSON" @click="importAlbums">
                <n-icon :component="Download" size="13" />
              </button>
              <button class="op-btn" title="导出当前相册配置" @click="exportAlbums">
                <n-icon :component="Upload" size="13" />
              </button>
              <button class="op-btn" title="新建相册" @click="createAlbumFolder(null)">
                <n-icon :component="Plus" size="13" />
              </button>
            </span>
          </div>
          <div v-if="!navCollapsed" class="nav-albums nav-text">
            <div
              v-for="row in albumRows"
              :key="row.folder.id"
              class="album-row"
              :class="{ active: navKey === 'album' && activeAlbumId === row.folder.id }"
              :style="{ paddingLeft: 6 + row.depth * 13 + 'px' }"
              :title="row.folder.name + ' · ' + row.count + ' 张'"
              @click="switchAlbum(row.folder.id)"
            >
              <n-icon :component="Folder" size="14" class="root-ic" />
              <span class="root-name ellip">{{ row.folder.name }}</span>
              <span class="album-count">{{ row.count }}</span>
              <span class="album-ops" @click.stop>
                <button class="op-btn" title="新建子相册" @click="createAlbumFolder(row.folder.id)">
                  <n-icon :component="Plus" size="12" />
                </button>
                <button
                  class="op-btn"
                  title="重命名"
                  @click="renameAlbumFolder(row.folder.id, row.folder.name)"
                >
                  <n-icon :component="Pencil" size="12" />
                </button>
                <button class="op-btn danger" title="删除相册" @click="deleteAlbumFolder(row.folder)">
                  <n-icon :component="Trash" size="12" />
                </button>
              </span>
            </div>
            <div v-if="albums.folders.length === 0" class="root-empty">暂无相册，点右上 + 新建</div>
          </div>

          <!-- 相册源列表（点击切换筛选，同源再点回全部） -->
          <div v-if="!navCollapsed" class="nav-sec nav-text">相册源</div>
          <div v-if="!navCollapsed" class="nav-roots nav-text">
            <div
              v-for="r in roots"
              :key="r"
              class="root-row"
              :class="{ active: activeRoot === r }"
              :title="r"
              @click="switchRoot(r)"
            >
              <n-icon :component="Folder" size="15" class="root-ic" />
              <span class="root-name ellip">{{ baseName(r) }}</span>
              <button class="root-x" title="移除相册源" @click.stop="removeRoot(r)">
                <n-icon :component="X" size="11" />
              </button>
            </div>
            <div v-if="roots.length === 0" class="root-empty">暂无，点右上角添加</div>
          </div>
        </div>
        <button
          class="nav-collapse"
          :title="navCollapsed ? '展开导航' : '收起导航'"
          @click="toggleNavCollapsed"
        >
          <n-icon :component="navCollapsed ? ChevronsRight : ChevronsLeft" size="16" />
          <span class="nav-text">收起</span>
        </button>
      </aside>

      <!-- === 中间内容区 === -->
      <section class="lib-center">
        <!-- 顶部工具条：打开相册（主操作）+ 搜索 + 视图切换 + 刷新扫描 -->
        <header class="lib-toolbar">
          <button
            v-if="navKey === 'album'"
            class="lb-btn primary"
            title="把磁盘上任意位置的图片收录进当前相册（原文件不动）"
            @click="addAlbumImages"
          >
            <n-icon :component="Photo" size="15" />
            <span class="lb-btn-text">添加图片</span>
          </button>
          <button
            class="lb-btn"
            :class="{ primary: navKey !== 'album' }"
            title="选择一个文件夹作为相册打开（浏览其中全部图片）"
            @click="addRoot"
          >
            <n-icon :component="FolderPlus" size="15" />
            <span class="lb-btn-text">打开相册</span>
          </button>
          <n-input
            v-model:value="search"
            class="tb-search"
            size="small"
            round
            clearable
            placeholder="搜索文件名…"
          >
            <template #prefix>
              <n-icon :component="Search" size="14" />
            </template>
          </n-input>
          <div class="seg" role="group" aria-label="视图切换">
            <button
              v-for="v in VIEW_DEFS"
              :key="v.key"
              class="seg-btn"
              :class="{ active: viewMode === v.key }"
              :title="v.hint"
              @click="setView(v.key)"
            >
              <n-icon :component="v.icon" size="15" />
            </button>
          </div>
          <div class="tb-spring" />
          <span v-if="viewMode === 'grid' && navKey !== 'recycle' && navKey !== 'vault'" class="cols-hint">
            Ctrl+滚轮 · {{ gridCols }} 列
          </span>
          <button
            class="lb-btn"
            :disabled="scanning"
            title="重新扫描全部相册源"
            @click="rescan"
          >
            <n-icon :component="Refresh" size="15" :class="{ spin: scanning }" />
            <span class="lb-btn-text">刷新扫描</span>
          </button>
        </header>

        <!-- 内容滚动区 -->
        <div ref="contentRef" class="lib-content">
          <!-- 回收站视图 -->
          <template v-if="navKey === 'recycle'">
            <div v-if="recycleList.length === 0" class="soft-empty">
              回收站是空的，删除的图片会在这里保留 7 天
            </div>
            <div v-else class="recycle-rows">
              <div v-for="r in recycleList" :key="r.recycled_name" class="rc-row">
                <n-icon :component="Trash" size="15" class="rc-ic" />
                <div class="rc-info">
                  <span class="rc-name" :title="r.original_path">{{ r.display_name }}</span>
                  <span class="rc-meta" :title="r.original_path">
                    {{ fmtTime(r.deleted_ts) }} · {{ r.original_path }}
                  </span>
                </div>
                <button class="lb-btn" title="恢复到原位置" @click="restoreOne(r)">
                  <n-icon :component="Rotate" size="14" />
                  <span class="lb-btn-text">恢复</span>
                </button>
                <button class="lb-btn danger" title="彻底删除" @click="purgeOne(r)">
                  <span class="lb-btn-text">删除</span>
                </button>
              </div>
              <div class="rc-hint">共 {{ recycleList.length }} 项 · 7 天后自动清理</div>
            </div>
          </template>

          <!-- 隐私相册（VaultPanel 由他人提供） -->
          <VaultPanel v-else-if="navKey === 'vault'" />

          <template v-else>
            <!-- 首次读取中 -->
            <div v-if="loading && entries.length === 0 && roots.length === 0" class="lib-loading">
              <n-spin :size="18" />
              <span>正在读取相册…</span>
            </div>

            <!-- 无相册源引导：柔和相纸插画 + 添加按钮 -->
            <div v-else-if="roots.length === 0 && entries.length === 0" class="guide">
              <div class="polaroid">
                <div class="polaroid-photo">
                  <div class="pp-blob a" />
                  <div class="pp-blob b" />
                  <div class="pp-sun" />
                </div>
                <div class="polaroid-caption">把美好都收进来</div>
              </div>
              <span class="guide-title">欢迎来到相册</span>
              <span class="guide-sub">先打开一个文件夹作为相册，散落各处的照片会自动整理到这里</span>
              <n-button size="small" type="primary" secondary @click="addRoot">
                <template #icon>
                  <n-icon :component="FolderPlus" size="15" />
                </template>
                打开相册
              </n-button>
            </div>

            <!-- 过滤后为空 -->
            <n-empty
              v-else-if="filteredEntries.length === 0"
              class="lib-empty"
              size="large"
              :description="emptyText"
            />

            <!-- 列表视图（平铺表格） -->
            <div v-else-if="viewMode === 'list'" class="lib-table">
              <div class="lt-head">
                <span class="c-thumb-h" />
                <span>名称</span>
                <span>尺寸</span>
                <span>格式</span>
                <span>修改时间</span>
                <span>星标</span>
                <span>标签</span>
              </div>
              <div
                v-for="it in filteredEntries"
                :key="it.path"
                class="lt-row"
                :class="{ sel: selected.has(it.path) }"
                @click="onCardClick(it, $event)"
                @dblclick="emit('open-image', it.path)"
              >
                <div class="lt-thumb lazy-thumb" :data-path="it.path">
                  <img v-if="thumbs[it.path]" :src="thumbs[it.path]" draggable="false" />
                  <span v-else class="ph">{{ extLabel(it) }}</span>
                </div>
                <span class="c-name ellip" :title="it.path">{{ it.name }}</span>
                <span class="c-dim">{{ it.width }}×{{ it.height }}</span>
                <span class="c-fmt">{{ extLabel(it) }}</span>
                <span class="c-time">{{ fmtTime(it.mtime) }}</span>
                <button
                  class="star-btn"
                  :class="{ on: metaOf(it.path).stars > 0 }"
                  title="切换星标"
                  @click.stop="toggleStar(it.path)"
                >
                  <n-icon :component="Star" size="14" />
                </button>
                <span class="c-tags ellip">
                  <template v-if="metaOf(it.path).tags?.length">
                    {{ metaOf(it.path).tags.map((t) => "#" + t).join(" ") }}
                  </template>
                  <span v-else class="c-none">—</span>
                </span>
              </div>
            </div>

            <!-- 网格 / 时间线 / 智能分类（分组卡片） -->
            <template v-else>
              <section v-for="g in renderGroups" :key="g.key" class="group">
                <div v-if="g.title" class="group-head">
                  <span class="group-title">{{ g.title }}</span>
                  <span class="group-count">{{ g.items.length }} 张</span>
                </div>
                <div class="card-grid" :style="gridStyle">
                  <div
                    v-for="it in g.items"
                    :key="it.path"
                    class="card"
                    :class="{ sel: selected.has(it.path) }"
                    :title="it.path"
                    @click="onCardClick(it, $event)"
                    @dblclick="emit('open-image', it.path)"
                  >
                    <div class="card-thumb lazy-thumb" :data-path="it.path">
                      <img v-if="thumbs[it.path]" :src="thumbs[it.path]" draggable="false" />
                      <span v-else class="ph">{{ extLabel(it) }}</span>
                      <!-- 角标星标切换 -->
                      <button
                        class="card-star"
                        :class="{ on: metaOf(it.path).stars > 0 }"
                        title="切换星标"
                        @click.stop="toggleStar(it.path)"
                      >
                        <n-icon :component="Star" size="13" />
                      </button>
                      <!-- 缩略标签（最多 2 个） -->
                      <div v-if="metaOf(it.path).tags?.length" class="card-tags">
                        <span v-for="t in metaOf(it.path).tags.slice(0, 2)" :key="t" class="ellip"># {{ t }}</span>
                      </div>
                    </div>
                    <div class="card-name ellip">{{ it.name }}</div>
                  </div>
                </div>
              </section>
            </template>
          </template>
        </div>
      </section>

      <!-- === 右侧详情栏（280px 可收起；恰好选中一张时显示） === -->
      <aside v-if="singleEntry && detailOpen" class="lib-detail">
        <div class="dt-head">
          <span class="dt-title">图片详情</span>
          <button class="dt-close" title="收起详情栏" @click="detailOpen = false">
            <n-icon :component="X" size="14" />
          </button>
        </div>
        <div class="dt-body">
          <!-- 大缩略图 -->
          <div class="dt-thumb">
            <img v-if="thumbs[singleEntry.path]" :src="thumbs[singleEntry.path]" draggable="false" />
            <span v-else class="ph">{{ extLabel(singleEntry) }}</span>
          </div>
          <div class="dt-name" :title="singleEntry.path">{{ singleEntry.name }}</div>

          <!-- 基础信息 -->
          <div class="dt-sec">
            <div class="dt-sec-title">基础信息</div>
            <div class="dt-row">
              <span class="dt-k">尺寸</span>
              <span class="dt-v">{{ singleEntry.width }}×{{ singleEntry.height }}</span>
            </div>
            <div class="dt-row">
              <span class="dt-k">格式</span>
              <span class="dt-v">{{ extLabel(singleEntry) }}</span>
            </div>
            <div class="dt-row">
              <span class="dt-k">修改时间</span>
              <span class="dt-v">{{ fmtTime(singleEntry.mtime) }}</span>
            </div>
            <div class="dt-row">
              <span class="dt-k">所在目录</span>
              <span class="dt-v ellip" :title="singleEntry.dir">{{ singleEntry.dir || "—" }}</span>
            </div>
          </div>

          <!-- EXIF 摘要（前 8 条） -->
          <div class="dt-sec">
            <div class="dt-sec-title">EXIF 摘要</div>
            <template v-if="exifSummary.length > 0">
              <div v-for="(it, i) in exifSummary" :key="i" class="dt-row">
                <span class="dt-k">{{ it.label }}</span>
                <span class="dt-v">{{ it.value }}</span>
              </div>
            </template>
            <div v-else class="dt-none">暂无 EXIF 信息</div>
          </div>

          <!-- 星级（5 星可点，点当前星级归零） -->
          <div class="dt-sec">
            <div class="dt-sec-title">星级</div>
            <div class="dt-stars">
              <button
                v-for="i in 5"
                :key="i"
                class="rate-star"
                :class="{ on: i <= metaOf(singleEntry.path).stars }"
                :title="`${i} 星`"
                @click="rate(i)"
              >
                <n-icon :component="Star" size="17" />
              </button>
            </div>
          </div>

          <!-- 标签 chips（可增删） -->
          <div class="dt-sec">
            <div class="dt-sec-title">标签</div>
            <div class="dt-chips">
              <span v-for="t in metaOf(singleEntry.path).tags" :key="t" class="chip">
                # {{ t }}
                <button class="chip-x" title="移除标签" @click="removeTagDetail(t)">×</button>
              </span>
              <span v-if="!metaOf(singleEntry.path).tags?.length" class="dt-none">还没有标签</span>
            </div>
            <n-input
              v-model:value="detailTagInput"
              size="tiny"
              placeholder="添加标签，回车确认"
              @keydown.enter="addTagDetail"
            />
          </div>

          <!-- 路径复制 + 操作按钮 -->
          <button class="lb-btn dt-copy" title="复制完整路径" @click="copyPath">
            <n-icon :component="Copy" size="14" />
            <span class="lb-btn-text">复制路径</span>
          </button>
          <div class="dt-actions">
            <n-button size="tiny" type="primary" secondary title="在看图窗口打开" @click="emit('open-image', singleEntry.path)">
              <template #icon><n-icon :component="Eye" size="14" /></template>
              查看
            </n-button>
            <n-button size="tiny" secondary title="在编辑器中打开" @click="emit('edit-image', singleEntry.path)">
              <template #icon><n-icon :component="Pencil" size="14" /></template>
              编辑
            </n-button>
            <n-button size="tiny" secondary title="用选中图片发起拼图" @click="emit('collage', [singleEntry.path])">
              <template #icon><n-icon :component="GridDots" size="14" /></template>
              拼图
            </n-button>
            <n-button size="tiny" quaternary type="error" title="移入软件回收站" @click="deleteOne(singleEntry.path)">
              <template #icon><n-icon :component="Trash" size="14" /></template>
              删除
            </n-button>
          </div>
        </div>
      </aside>

      <!-- 详情栏收起后的重新展开小把手 -->
      <button
        v-else-if="singleEntry && !detailOpen"
        class="detail-reopen"
        title="展开详情栏"
        @click="detailOpen = true"
      >
        <n-icon :component="ChevronsLeft" size="15" />
      </button>
    </div>

    <!-- === 批量工具条（多选 ≥1 时底部浮出） === -->
    <div
      v-if="selected.size > 0 && navKey !== 'recycle' && navKey !== 'vault'"
      class="lib-batch"
    >
      <span class="batch-count">已选 {{ selected.size }} 项</span>
      <button
        v-if="navKey === 'album'"
        class="lb-btn"
        title="把选中图片移出当前逻辑相册（磁盘文件不动）"
        @click="removeAlbumItems"
      >
        <n-icon :component="X" size="14" />
        <span class="lb-btn-text">移出相册</span>
      </button>
      <button class="lb-btn" title="为选中图片加星" @click="batchStar">
        <n-icon :component="Star" size="14" />
        <span class="lb-btn-text">加星</span>
      </button>
      <button class="lb-btn" title="为选中图片统一打标签" @click="batchTagOpen = !batchTagOpen">
        <n-icon :component="Tags" size="14" />
        <span class="lb-btn-text">打标签</span>
      </button>
      <n-input
        v-if="batchTagOpen"
        v-model:value="batchTagInput"
        class="batch-tag-input"
        size="tiny"
        round
        placeholder="输入标签，回车确认"
        @keydown.enter="batchAddTagConfirm"
      />
      <button class="lb-btn" title="用选中图片发起拼图" @click="emit('collage', selectedPaths)">
        <n-icon :component="GridDots" size="14" />
        <span class="lb-btn-text">拼图</span>
      </button>
      <button class="lb-btn" title="把选中的图片 / PDF 送去打印（可配合一张多页拼版）" @click="emit('print', selectedPaths)">
        <n-icon :component="Printer" size="14" />
        <span class="lb-btn-text">打印</span>
      </button>
      <button class="lb-btn" title="将选中图片加入隐私相册" @click="batchVault">
        <n-icon :component="Lock" size="14" />
        <span class="lb-btn-text">加入隐私相册</span>
      </button>
      <button class="lb-btn" title="打开批量处理面板" @click="showBatch = true">
        <n-icon :component="GridDots" size="14" />
        <span class="lb-btn-text">批量处理</span>
      </button>
      <button class="lb-btn danger" title="批量移入软件回收站" @click="batchRecycle">
        <n-icon :component="Trash" size="14" />
        <span class="lb-btn-text">移入回收站</span>
      </button>
      <button class="lb-btn" title="取消选择（Esc）" @click="clearSelection">
        <n-icon :component="X" size="14" />
      </button>
    </div>

    <!-- === 底部状态栏 === -->
    <footer class="lib-status">
      <span>{{ statusText }}</span>
      <div class="tb-spring" />
      <span v-if="navKey !== 'recycle' && navKey !== 'vault'" class="status-hint">
        单击选中 · Ctrl/Shift 多选 · 双击打开看图
      </span>
    </footer>

    <!-- === 轻量输入弹窗（相册命名/重命名） === -->
    <div v-if="promptState.show" class="prompt-mask" @click.self="promptState.show = false">
      <div class="prompt-box">
        <span class="prompt-title">{{ promptState.title }}</span>
        <n-input
          v-model:value="promptState.text"
          size="small"
          placeholder="输入名称，回车确认"
          autofocus
          @keydown.enter="promptOk"
        />
        <div class="prompt-actions">
          <button class="lb-btn" @click="promptState.show = false">取消</button>
          <button class="lb-btn primary" @click="promptOk">确定</button>
        </div>
      </div>
    </div>

    <!-- 批量处理面板（BatchPanel 由他人提供；内部 showBatch 控制其显示） -->
    <BatchPanel :paths="selectedPaths" :visible="showBatch" @close="showBatch = false" />
  </div>
</template>
<style scoped>
/* ==================== 根布局：导航 + 内容 + 详情 + 状态栏 ==================== */
.lib-root {
  position: relative;
  display: flex;
  flex-direction: column;
  height: 100%;
  overflow: hidden;
  background: var(--jb-bg);
}
/* 极淡主题色极光晕染（与看图窗口一致，柔光不抢内容） */
.lib-root::before {
  content: "";
  position: absolute;
  inset: 0;
  pointer-events: none;
  z-index: 0;
  background:
    radial-gradient(
      46% 38% at 14% 6%,
      color-mix(in srgb, var(--jb-primary) 10%, transparent),
      transparent 72%
    ),
    radial-gradient(
      40% 34% at 90% 98%,
      color-mix(in srgb, var(--jb-primary) 8%, transparent),
      transparent 72%
    );
}
.lib-body {
  flex: 1;
  display: flex;
  min-height: 0;
  position: relative;
  z-index: 1;
}
.ellip {
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

/* ==================== 左侧导航 ==================== */
.lib-nav {
  width: 200px;
  flex-shrink: 0;
  display: flex;
  flex-direction: column;
  background: color-mix(in srgb, var(--jb-bg-card) 88%, transparent);
  border-right: 1px solid var(--jb-border);
  transition: width 200ms ease;
  overflow: hidden;
}
.lib-nav.collapsed {
  width: 56px;
}
.nav-scroll {
  flex: 1;
  overflow-y: auto;
  overflow-x: hidden;
  padding: 10px 8px 8px;
  display: flex;
  flex-direction: column;
  gap: 2px;
  scrollbar-width: thin;
  scrollbar-color: var(--jb-scrollbar) transparent;
}
.nav-item {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 8px 10px;
  border: none;
  border-radius: 10px;
  background: transparent;
  color: var(--jb-text-soft);
  font-size: 12.5px;
  cursor: pointer;
  flex-shrink: 0;
  transition: background-color 200ms ease, color 200ms ease;
}
.nav-item:hover {
  background: var(--jb-titlebar-btn-hover);
  color: var(--jb-text);
}
.nav-item.active {
  background: color-mix(in srgb, var(--jb-primary) 14%, transparent);
  color: var(--jb-primary);
  font-weight: 600;
}
.nav-item.sub {
  padding: 6px 10px 6px 14px;
  font-size: 12px;
}
.nav-label {
  flex: 1;
  min-width: 0;
  text-align: left;
}
.nav-count {
  font-size: 10.5px;
  color: var(--jb-text-mute);
}
/* 收起态：仅图标居中 */
.lib-nav.collapsed .nav-item {
  justify-content: center;
  padding: 9px 0;
}
.nav-sec {
  margin: 12px 10px 4px;
  font-size: 10.5px;
  letter-spacing: 2px;
  color: var(--jb-text-mute);
  user-select: none;
}
.nav-roots {
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding: 0 2px;
}
.root-row {
  display: flex;
  align-items: center;
  gap: 7px;
  padding: 5px 8px;
  border-radius: 8px;
  font-size: 11.5px;
  color: var(--jb-text-soft);
  cursor: pointer;
  transition: background-color 200ms ease;
}
.root-row:hover {
  background: var(--jb-titlebar-btn-hover);
}
/* 选中态：主题色左侧指示条 + 高亮文字 */
.root-row.active {
  background: color-mix(in srgb, var(--jb-primary) 12%, transparent);
  color: var(--jb-primary);
  font-weight: 500;
}
.root-row.active .root-ic {
  color: var(--jb-primary);
}
.root-ic {
  color: var(--jb-text-mute);
  flex-shrink: 0;
}
.root-name {
  flex: 1;
  min-width: 0;
}
.root-x {
  border: none;
  background: transparent;
  color: var(--jb-text-mute);
  cursor: pointer;
  display: flex;
  align-items: center;
  padding: 2px;
  border-radius: 5px;
  flex-shrink: 0;
  transition: color 200ms ease, background-color 200ms ease;
}
.root-x:hover {
  color: var(--jb-red);
  background: color-mix(in srgb, var(--jb-red) 12%, transparent);
}
.root-empty {
  padding: 4px 10px;
  font-size: 10.5px;
  color: var(--jb-text-mute);
}

/* ==================== 我的相册（逻辑相册树） ==================== */
.nav-album-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 4px;
}
.nav-album-ops {
  display: inline-flex;
  gap: 1px;
}
.op-btn {
  width: 20px;
  height: 20px;
  border: none;
  border-radius: 5px;
  background: transparent;
  color: var(--jb-text-mute);
  cursor: pointer;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  flex-shrink: 0;
  transition: color 200ms ease, background-color 200ms ease;
}
.op-btn:hover {
  color: var(--jb-primary);
  background: color-mix(in srgb, var(--jb-primary) 12%, transparent);
}
.op-btn.danger:hover {
  color: var(--jb-red);
  background: color-mix(in srgb, var(--jb-red) 12%, transparent);
}
.nav-albums {
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding: 0 2px;
}
.album-row {
  display: flex;
  align-items: center;
  gap: 7px;
  padding: 5px 8px;
  border-radius: 8px;
  font-size: 11.5px;
  color: var(--jb-text-soft);
  cursor: pointer;
  transition: background-color 200ms ease;
}
.album-row:hover {
  background: var(--jb-titlebar-btn-hover);
}
.album-row.active {
  background: color-mix(in srgb, var(--jb-primary) 12%, transparent);
  color: var(--jb-primary);
  font-weight: 500;
}
.album-row.active .root-ic {
  color: var(--jb-primary);
}
.album-count {
  font-size: 10px;
  color: var(--jb-text-mute);
  flex-shrink: 0;
}
.album-ops {
  display: none;
  gap: 1px;
  flex-shrink: 0;
}
.album-row:hover .album-ops {
  display: inline-flex;
}
/* hover 出现操作按钮时隐藏计数，避免行宽跳动 */
.album-row:hover .album-count {
  display: none;
}

/* ==================== 轻量输入弹窗 ==================== */
.prompt-mask {
  position: absolute;
  inset: 0;
  z-index: 100;
  background: rgba(0, 0, 0, 0.28);
  display: flex;
  align-items: center;
  justify-content: center;
}
.prompt-box {
  width: 300px;
  padding: 16px 16px 14px;
  border-radius: 14px;
  border: 1px solid var(--jb-border);
  background: var(--jb-bg-card);
  box-shadow: var(--jb-shadow);
  display: flex;
  flex-direction: column;
  gap: 12px;
}
.prompt-title {
  font-size: 13px;
  font-weight: 600;
  color: var(--jb-text);
}
.prompt-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}
.nav-collapse {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 6px;
  padding: 9px 0;
  border: none;
  border-top: 1px solid var(--jb-divider);
  background: transparent;
  color: var(--jb-text-mute);
  font-size: 11px;
  cursor: pointer;
  flex-shrink: 0;
  transition: color 200ms ease, background-color 200ms ease;
}
.nav-collapse:hover {
  color: var(--jb-text);
  background: var(--jb-titlebar-btn-hover);
}

/* ==================== 中间内容区 ==================== */
.lib-center {
  flex: 1;
  display: flex;
  flex-direction: column;
  min-width: 0;
  min-height: 0;
}
.lib-toolbar {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 10px 14px;
  border-bottom: 1px solid var(--jb-border);
  flex-shrink: 0;
}
.tb-search {
  width: 220px;
}
.tb-spring {
  flex: 1;
}
.seg {
  display: flex;
  gap: 2px;
  padding: 2px;
  border: 1px solid var(--jb-border);
  border-radius: 9px;
  background: var(--jb-bg-card);
}
.seg-btn {
  width: 30px;
  height: 26px;
  border: none;
  border-radius: 7px;
  background: transparent;
  color: var(--jb-text-mute);
  cursor: pointer;
  display: flex;
  align-items: center;
  justify-content: center;
  transition: color 200ms ease, background-color 200ms ease;
}
.seg-btn:hover {
  color: var(--jb-text);
}
.seg-btn.active {
  color: var(--jb-primary);
  background: color-mix(in srgb, var(--jb-primary) 14%, transparent);
}
.cols-hint {
  font-size: 10.5px;
  color: var(--jb-text-mute);
  user-select: none;
  white-space: nowrap;
}

/* 通用小按钮（工具条 / 批量条 / 详情栏共用） */
.lb-btn {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  padding: 5px 11px;
  border: 1px solid var(--jb-border);
  border-radius: 999px;
  background: var(--jb-bg-card);
  color: var(--jb-text-soft);
  font-size: 11.5px;
  cursor: pointer;
  white-space: nowrap;
  flex-shrink: 0;
  transition: color 200ms ease, border-color 200ms ease, transform 200ms ease;
}
.lb-btn:hover:not(:disabled) {
  color: var(--jb-text);
  border-color: var(--jb-primary);
  transform: scale(1.03);
}
.lb-btn:disabled {
  opacity: 0.5;
  cursor: default;
}
.lb-btn.primary {
  color: var(--jb-primary);
  border-color: color-mix(in srgb, var(--jb-primary) 45%, transparent);
}
.lb-btn.danger:hover:not(:disabled) {
  color: var(--jb-red);
  border-color: var(--jb-red);
}
/* 刷新扫描转圈 */
.spin {
  animation: jb-rotate 0.9s linear infinite;
}
@keyframes jb-rotate {
  to {
    transform: rotate(360deg);
  }
}

/* 内容滚动区 */
.lib-content {
  flex: 1;
  overflow-y: auto;
  overflow-x: hidden;
  padding: 14px 16px 24px;
  scrollbar-width: thin;
  scrollbar-color: var(--jb-scrollbar) transparent;
}
.lib-content::-webkit-scrollbar {
  width: 6px;
}
.lib-content::-webkit-scrollbar-thumb {
  background: var(--jb-scrollbar);
  border-radius: 3px;
}
.lib-loading {
  height: 100%;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 10px;
  color: var(--jb-text-mute);
  font-size: 12px;
}
.lib-empty {
  margin-top: 72px;
}
.soft-empty {
  padding: 48px 12px;
  text-align: center;
  font-size: 12px;
  color: var(--jb-text-mute);
}

/* ==================== 网格卡片 ==================== */
.group {
  margin-bottom: 18px;
}
.group-head {
  position: sticky;
  top: -14px; /* 抵消内容区 padding，贴住滚动容器顶 */
  z-index: 5;
  display: flex;
  align-items: baseline;
  gap: 8px;
  padding: 6px 4px 8px;
  margin-bottom: 8px;
  background: color-mix(in srgb, var(--jb-bg) 86%, transparent);
  backdrop-filter: blur(12px) saturate(1.2);
  border-bottom: 1px solid var(--jb-divider);
}
.group-title {
  font-size: 12.5px;
  font-weight: 600;
  color: var(--jb-text);
  letter-spacing: 0.5px;
}
.group-count {
  font-size: 10.5px;
  color: var(--jb-text-mute);
}
.card-grid {
  display: grid;
  gap: 14px;
}
.card {
  cursor: pointer;
  user-select: none;
  transition: transform 200ms ease;
}
.card:hover {
  transform: translateY(-2px) scale(1.03); /* hover 轻微放大（不超过 1.03） */
  z-index: 1;
}
.card-thumb {
  position: relative;
  aspect-ratio: 4 / 3;
  border-radius: 12px;
  overflow: hidden;
  background: var(--jb-bg-card);
  border: 1px solid var(--jb-border);
  display: flex;
  align-items: center;
  justify-content: center;
  transition: box-shadow 200ms ease, border-color 200ms ease;
}
.card:hover .card-thumb {
  box-shadow: var(--jb-shadow);
}
.card.sel .card-thumb {
  border-color: var(--jb-primary);
  box-shadow: 0 0 0 3px color-mix(in srgb, var(--jb-primary) 22%, transparent);
}
.card-thumb img {
  width: 100%;
  height: 100%;
  object-fit: cover;
}
.ph {
  font-size: 10.5px;
  color: var(--jb-text-mute);
  letter-spacing: 1px;
  user-select: none;
}
/* 角标星标 */
.card-star {
  position: absolute;
  top: 6px;
  right: 6px;
  width: 24px;
  height: 24px;
  border: none;
  border-radius: 50%;
  background: rgba(0, 0, 0, 0.38);
  backdrop-filter: blur(6px);
  color: rgba(255, 255, 255, 0.85);
  cursor: pointer;
  display: flex;
  align-items: center;
  justify-content: center;
  opacity: 0;
  transition: opacity 200ms ease, transform 200ms ease, color 200ms ease;
}
.card:hover .card-star,
.card-star.on {
  opacity: 1;
}
.card-star:hover {
  transform: scale(1.12);
}
.card-star.on {
  color: #f6b73c;
}
.card-star.on :deep(svg) {
  fill: currentColor;
}
/* 缩略标签（最多 2 个） */
.card-tags {
  position: absolute;
  left: 6px;
  bottom: 6px;
  display: flex;
  gap: 4px;
  max-width: calc(100% - 12px);
}
.card-tags span {
  max-width: 84px;
  font-size: 10px;
  line-height: 1;
  padding: 3px 7px;
  border-radius: 999px;
  background: rgba(0, 0, 0, 0.42);
  backdrop-filter: blur(6px);
  color: #fff;
}
.card-name {
  margin-top: 6px;
  padding: 0 2px;
  font-size: 11px;
  color: var(--jb-text-mute);
}

/* ==================== 列表视图（表格） ==================== */
.lib-table {
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.lt-head,
.lt-row {
  display: grid;
  grid-template-columns: 44px minmax(0, 1fr) 110px 64px 148px 56px minmax(0, 150px);
  gap: 10px;
  align-items: center;
  padding: 0 8px;
}
.lt-head {
  position: sticky;
  top: -14px;
  z-index: 5;
  height: 32px;
  font-size: 10.5px;
  letter-spacing: 1px;
  color: var(--jb-text-mute);
  background: color-mix(in srgb, var(--jb-bg) 86%, transparent);
  backdrop-filter: blur(12px) saturate(1.2);
  border-bottom: 1px solid var(--jb-divider);
  user-select: none;
}
.lt-row {
  height: 44px;
  border-radius: 10px;
  cursor: pointer;
  user-select: none;
  font-size: 12px;
  color: var(--jb-text);
  transition: background-color 200ms ease;
}
.lt-row:hover {
  background: var(--jb-bg-card);
}
.lt-row.sel {
  background: color-mix(in srgb, var(--jb-primary) 12%, transparent);
}
.lt-thumb {
  width: 40px;
  height: 34px;
  border-radius: 7px;
  overflow: hidden;
  background: var(--jb-bg-card);
  border: 1px solid var(--jb-border);
  display: flex;
  align-items: center;
  justify-content: center;
}
.lt-thumb img {
  width: 100%;
  height: 100%;
  object-fit: cover;
}
.c-name {
  min-width: 0;
}
.c-dim,
.c-fmt,
.c-time {
  font-size: 11px;
  color: var(--jb-text-mute);
}
.c-tags {
  font-size: 11px;
  color: var(--jb-text-soft);
  min-width: 0;
}
.c-none {
  color: var(--jb-text-mute);
}
.star-btn {
  border: none;
  background: transparent;
  color: var(--jb-text-mute);
  cursor: pointer;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 3px;
  border-radius: 6px;
  transition: color 200ms ease, transform 200ms ease;
}
.star-btn:hover {
  transform: scale(1.15);
}
.star-btn.on {
  color: #f6b73c;
}
.star-btn.on :deep(svg) {
  fill: currentColor;
}

/* ==================== 右侧详情栏（280px） ==================== */
.lib-detail {
  width: 280px;
  flex-shrink: 0;
  display: flex;
  flex-direction: column;
  background: color-mix(in srgb, var(--jb-bg-card) 92%, transparent);
  backdrop-filter: blur(16px) saturate(1.3);
  border-left: 1px solid var(--jb-border);
  min-height: 0;
}
.dt-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 12px 14px;
  border-bottom: 1px solid var(--jb-divider);
  flex-shrink: 0;
}
.dt-title {
  font-size: 13px;
  font-weight: 600;
  color: var(--jb-text);
}
.dt-close {
  width: 26px;
  height: 26px;
  border: none;
  border-radius: 6px;
  background: transparent;
  color: var(--jb-text-soft);
  cursor: pointer;
  display: flex;
  align-items: center;
  justify-content: center;
  transition: background-color 200ms ease;
}
.dt-close:hover {
  background: var(--jb-titlebar-btn-hover);
}
.dt-body {
  flex: 1;
  overflow-y: auto;
  padding: 14px;
  display: flex;
  flex-direction: column;
  gap: 16px;
  scrollbar-width: thin;
  scrollbar-color: var(--jb-scrollbar) transparent;
}
.dt-thumb {
  height: 180px;
  border-radius: 12px;
  overflow: hidden;
  background: var(--jb-bg);
  border: 1px solid var(--jb-border);
  display: flex;
  align-items: center;
  justify-content: center;
  flex-shrink: 0;
}
.dt-thumb img {
  max-width: 100%;
  max-height: 100%;
  object-fit: contain;
}
.dt-name {
  font-size: 12.5px;
  font-weight: 600;
  color: var(--jb-text);
}
.dt-sec {
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.dt-sec-title {
  font-size: 11px;
  font-weight: 600;
  letter-spacing: 1px;
  color: var(--jb-primary);
}
.dt-row {
  display: flex;
  justify-content: space-between;
  gap: 12px;
  font-size: 12px;
  line-height: 1.6;
}
.dt-k {
  color: var(--jb-text-mute);
  flex-shrink: 0;
}
.dt-v {
  color: var(--jb-text);
  text-align: right;
  word-break: break-all;
  min-width: 0;
}
.dt-none {
  font-size: 11px;
  color: var(--jb-text-mute);
}
.dt-stars {
  display: flex;
  gap: 4px;
}
.rate-star {
  border: none;
  background: transparent;
  color: var(--jb-text-mute);
  cursor: pointer;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 3px;
  border-radius: 6px;
  transition: color 200ms ease, transform 200ms ease;
}
.rate-star:hover {
  transform: scale(1.15);
}
.rate-star.on {
  color: #f6b73c;
}
.rate-star.on :deep(svg) {
  fill: currentColor;
}
.dt-chips {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}
.chip {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 2px 9px;
  border-radius: 999px;
  font-size: 11px;
  color: var(--jb-primary);
  background: color-mix(in srgb, var(--jb-primary) 12%, transparent);
  border: 1px solid color-mix(in srgb, var(--jb-primary) 30%, transparent);
}
.chip-x {
  border: none;
  background: transparent;
  color: inherit;
  cursor: pointer;
  font-size: 12px;
  line-height: 1;
  padding: 0 1px;
  opacity: 0.7;
  transition: opacity 200ms ease;
}
.chip-x:hover {
  opacity: 1;
}
.dt-copy {
  align-self: flex-start;
}
.dt-actions {
  display: flex;
  gap: 6px;
  flex-wrap: wrap;
}
/* 详情栏收起后的展开把手 */
.detail-reopen {
  position: absolute;
  right: 8px;
  top: 60px;
  z-index: 20;
  width: 26px;
  height: 44px;
  border: 1px solid var(--jb-border);
  border-radius: 8px;
  background: color-mix(in srgb, var(--jb-bg-card) 88%, transparent);
  backdrop-filter: blur(12px);
  color: var(--jb-text-soft);
  cursor: pointer;
  display: flex;
  align-items: center;
  justify-content: center;
  box-shadow: var(--jb-shadow);
  transition: color 200ms ease, border-color 200ms ease;
}
.detail-reopen:hover {
  color: var(--jb-primary);
  border-color: var(--jb-primary);
}

/* ==================== 回收站视图 ==================== */
.recycle-rows {
  display: flex;
  flex-direction: column;
  gap: 6px;
  max-width: 760px;
}
.rc-row {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 9px 12px;
  border: 1px solid var(--jb-border);
  border-radius: 12px;
  background: var(--jb-bg-card);
  transition: border-color 200ms ease, box-shadow 200ms ease;
}
.rc-row:hover {
  box-shadow: var(--jb-shadow);
}
.rc-ic {
  color: var(--jb-text-mute);
  flex-shrink: 0;
}
.rc-info {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.rc-name {
  font-size: 12px;
  color: var(--jb-text);
  font-weight: 500;
}
.rc-meta {
  font-size: 10px;
  color: var(--jb-text-mute);
}
.rc-hint {
  padding: 8px 4px;
  font-size: 11px;
  color: var(--jb-text-mute);
}

/* ==================== 批量工具条（底部浮出） ==================== */
.lib-batch {
  position: absolute;
  left: 50%;
  bottom: 42px;
  transform: translateX(-50%);
  z-index: 40;
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 12px;
  border: 1px solid var(--jb-border);
  border-radius: 14px;
  background: color-mix(in srgb, var(--jb-bg-card) 88%, transparent);
  backdrop-filter: blur(16px) saturate(1.3);
  box-shadow: var(--jb-shadow);
  animation: batch-rise 200ms ease;
  max-width: calc(100% - 32px);
  flex-wrap: wrap;
  justify-content: center;
}
@keyframes batch-rise {
  from {
    opacity: 0;
    transform: translate(-50%, 8px);
  }
  to {
    opacity: 1;
    transform: translate(-50%, 0);
  }
}
.batch-count {
  font-size: 11px;
  color: var(--jb-primary);
  font-weight: 600;
  white-space: nowrap;
}
.batch-tag-input {
  width: 170px;
}

/* ==================== 底部状态栏 ==================== */
.lib-status {
  height: 30px;
  flex-shrink: 0;
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 0 14px;
  border-top: 1px solid var(--jb-border);
  background: var(--jb-bg-card);
  font-size: 11px;
  color: var(--jb-text-mute);
  user-select: none;
  position: relative;
  z-index: 1;
}
.status-hint {
  letter-spacing: 0.3px;
}

/* ==================== 空状态引导：纯 CSS 相纸插画（参考 ImageViewer） ==================== */
.guide {
  height: 100%;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 14px;
  user-select: none;
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
.guide-title {
  margin-top: 6px;
  font-size: 14px;
  font-weight: 600;
  color: var(--jb-text);
  letter-spacing: 1px;
}
.guide-sub {
  font-size: 12px;
  color: var(--jb-text-mute);
  letter-spacing: 0.5px;
}
</style>
