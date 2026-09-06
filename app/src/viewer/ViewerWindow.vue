<script setup lang="ts">
// 看图窗口根编排：TitleBar（复用）+ Toolbar 顶部浮动条 + ImageViewer 主区 + ThumbnailBar 底部栏 + ExifPanel 右侧抽屉
// 数据流：打开图片 → open_image → 并行拉同目录列表 + EXIF（缩略图由 ThumbnailBar 懒加载）
// 后端命令未就绪时 try/catch + toast 容错
import { ref, computed, watch, nextTick, onMounted, onUnmounted } from "vue";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { open as openFileDialog, save as saveFileDialog } from "@tauri-apps/plugin-dialog";
import { useMessage, useDialog, NModal, NButton, NIcon, NDropdown, NProgress } from "naive-ui";
import type { DropdownOption } from "naive-ui";
import { useTheme } from "../useTheme";
import { useAnimFill } from "../useAnimFill";
import TitleBar from "../TitleBar.vue";
import Toolbar from "./Toolbar.vue";
import ImageViewer from "./ImageViewer.vue";
import ThumbnailBar from "./ThumbnailBar.vue";
import ExifPanel from "./ExifPanel.vue";
import LibraryView from "./LibraryView.vue";
import EditorView from "./EditorView.vue";
import CollageView from "./CollageView.vue";
import ViewerTabs from "./ViewerTabs.vue";
import type { ViewerTabItem } from "./ViewerTabs.vue";
import PrintPanel from "./PrintPanel.vue";
import TonePanel from "./TonePanel.vue";
import HistoryCompare from "./HistoryCompare.vue";
import UpscalePanel from "./UpscalePanel.vue";
import { Trash, Rotate } from "@vicons/tabler";

const { mode, isDark, setMode } = useTheme();
const message = useMessage();
const dialog = useDialog();

// === 后端数据结构（与 Rust 端 serde 命名对齐） ===
interface ImageInfo {
  url: string;
  width: number;
  height: number;
  format: string;
  frame_count: number;
  page_count: number;
  has_exif: boolean;
}
interface ImageEntry {
  path: string;
  name: string;
  width: number;
  height: number;
}
interface ExifData {
  groups: Array<{ title: string; items: Array<{ label: string; value: string }> }>;
}

// === 多标签状态（文件夹级：一个文件夹一个标签，浏览器式） ===
/** 单个看图标签：绑定一个目录，独立持有当前图/目录列表/EXIF/抽屉态/已打开记录 */
interface ViewerTab {
  /** 标签绑定的目录（标签身份） */
  dir: string;
  /** 当前查看的图片完整路径 */
  currentPath: string;
  imageInfo: ImageInfo | null;
  imageList: ImageEntry[];
  exifData: ExifData | null;
  showExif: boolean;
  /** 本标签内已打开过的图片路径（缩略图"已看"角标，浏览器已访问链接思路） */
  visited: string[];
  /** PDF 当前页码（1 基；非 PDF 恒为 1） */
  pdfPage: number;
}

const tabs = ref<ViewerTab[]>([]);
const activeIdx = ref(0);
const activeTab = computed(() => tabs.value[activeIdx.value]);

// 兼容层：既有模板/逻辑统一走 activeTab 代理（最小化改动面）
const imageInfo = computed(() => activeTab.value?.imageInfo ?? null);
const currentPath = computed(() => activeTab.value?.currentPath ?? "");
const imageList = computed(() => activeTab.value?.imageList ?? []);
const exifData = computed(() => activeTab.value?.exifData ?? null);
const showExif = computed({
  get: () => activeTab.value?.showExif ?? false,
  set: (v: boolean) => {
    if (activeTab.value) activeTab.value.showExif = v;
  },
});

const scale = ref(1);
const fullscreen = ref(false);
/** 图片切换进行中（防连点） */
const switching = ref(false);

const viewerRef = ref<InstanceType<typeof ImageViewer> | null>(null);

// ==================== 模式路由（设计稿 六：相册首页 ↔ 大图浏览 ↔ 编辑/拼图） ====================
type ViewMode = "library" | "viewer" | "editor" | "collage";
/** 当前页面模式：默认相册首页（无图进入时）；打开任一图片切到单图浏览 */
const viewMode = ref<ViewMode>("library");

// 标签态上报 Rust（决定标题栏 X 的行为：有标签 = 关当前标签回相册页；无标签 = 隐藏窗口）
watch(
  () => tabs.value.length > 0 && viewMode.value === "viewer",
  (open) => invoke("set_viewer_tabs_open", { open }).catch(() => {}),
  { immediate: true },
);
/** 滤镜编辑的目标图路径 */
const editorPath = ref("");
/** 最近一次打开图片是否来自相册内（决定关最后一个标签时回相册页还是隐藏窗口） */
const lastOpenedFromLibrary = ref(false);
/** 拼图素材（1-9 张） */
const collagePaths = ref<string[]>([]);
/** 打印面板（预置当前图；队列在面板会话内保留，可继续追加） */
const showPrint = ref(false);
const printSeedPaths = ref<string[]>([]);
function openPrint() {
  printSeedPaths.value = currentPath.value ? [currentPath.value] : [];
  showPrint.value = true;
}
/** 画册多选 → 打印：预置全部选中路径（≥2 个时面板自动勾选「合并打印」） */
function openPrintPaths(paths: string[]) {
  if (!paths || paths.length === 0) {
    message.info("请先选择要打印的图片或 PDF", { duration: 2000 });
    return;
  }
  printSeedPaths.value = paths;
  showPrint.value = true;
}
/** 进入编辑/拼图前的模式（关闭后返回） */
const returnMode = ref<ViewMode>("library");

function openEditor(path: string) {
  if (!path) return;
  editorPath.value = path;
  returnMode.value = viewMode.value === "viewer" ? "viewer" : "library";
  viewMode.value = "editor";
}
function openCollage(paths: string[]) {
  if (!paths || paths.length === 0) {
    message.info("请先选择 1-9 张图片", { duration: 2200 });
    return;
  }
  collagePaths.value = paths.slice(0, 9);
  returnMode.value = viewMode.value === "viewer" ? "viewer" : "library";
  viewMode.value = "collage";
}
/** 关闭编辑/拼图子页，返回上一层 */
function closeSubPage() {
  viewMode.value = returnMode.value;
}
/** 子页保存成功（子页内部已 toast，这里负责返回） */
function onSubSaved() {
  closeSubPage();
}
/** 相册内打开图片（loadPath 包装：标记来源为相册，关标签时回相册页） */
function openFromLibrary(path: string) {
  lastOpenedFromLibrary.value = true;
  loadPath(path);
}

/** 隐私相册内双击查看（VaultPanel 派发 CustomEvent，路径为解密临时文件） */
function onVaultOpenImage(ev: Event) {
  const p = (ev as CustomEvent<string>).detail;
  if (typeof p === "string" && p) loadPath(p);
}

// 打开对话框过滤：浏览器原生格式 + 后端可解码格式
const IMAGE_FILTERS = [
  {
    name: "图片",
    extensions: [
      "png", "jpg", "jpeg", "gif", "bmp", "webp", "avif",
      "tif", "tiff", "ico", "psd", "jxl", "jxr", "wdp", "exr",
    ],
  },
  { name: "PDF 文档", extensions: ["pdf"] },
];

/** 后端返回的图片地址 → 可加载的 src
 *  Tier1 原生格式与 Tier2/3 临时 PNG 均为原始 Windows 路径，
 *  必须经 convertFileSrc 转为 asset: 协议（tauri.conf assetProtocol scope **），
 *  否则 <img> 将其当相对 URL 解析而 404（与 App.vue toAssetUrl 同规则） */
function toAssetUrl(u: string): string {
  if (!u) return "";
  if (/^(https?:|asset:|data:|blob:)/.test(u)) return u;
  if (/^[A-Za-z]:[\\/]/.test(u)) return convertFileSrc(u);
  return u;
}

const imageUrl = computed(() => toAssetUrl(imageInfo.value?.url ?? ""));
const currentIndex = computed(() =>
  imageList.value.findIndex((it) => it.path === currentPath.value),
);
const canPrev = computed(() => currentIndex.value > 0 || pdfPage.value > 1);
const canNext = computed(
  () =>
    pdfPage.value < pdfPageCount.value ||
    (currentIndex.value >= 0 && currentIndex.value < imageList.value.length - 1),
);

// === PDF 多页（2345 看图王同类行为：翻页浏览，边界继续切图） ===
const pdfPageCount = computed(() => imageInfo.value?.page_count ?? 1);
const pdfPage = computed(() => activeTab.value?.pdfPage ?? 1);
/** 当前是多页 PDF（显示翻页浮条/键盘 ←→ 先翻页） */
const isMultiPagePdf = computed(
  () => (imageInfo.value?.format ?? "").toLowerCase() === "pdf" && pdfPageCount.value > 1,
);

/** PDF 翻页：渲染指定页（1 基）并更新当前 tab */
async function pdfGoto(page: number) {
  const tab = activeTab.value;
  if (!tab || page < 1 || page > pdfPageCount.value || page === tab.pdfPage) return;
  if (switching.value) return;
  switching.value = true;
  try {
    const info = await invoke<ImageInfo>("pdf_render_page", {
      path: tab.currentPath,
      page,
    });
    tab.pdfPage = page;
    tab.imageInfo = info; // url 变化 → ImageViewer watch 重置视图并加载新页
  } catch (e) {
    message.error(`PDF 翻页失败: ${e}`, { duration: 2200 });
  } finally {
    switching.value = false;
  }
}
/** 标签栏展示数据：标题=文件夹名，提示=目录 + 已打开/总数（常驻显示，仅点 × 关闭） */
const tabItems = computed<ViewerTabItem[]>(() =>
  tabs.value.map((t) => ({
    key: t.dir,
    title: t.dir.split(/[\\/]/).pop() || t.dir,
    tip: `${t.dir}（已打开 ${t.visited.length}/${t.imageList.length} 张）`,
  })),
);
/** 当前标签已打开的图片路径（传给底部缩略图条打"已看"角标） */
const visitedPaths = computed<string[]>(() => activeTab.value?.visited ?? []);

// === 打开图片（内嵌入口：工具栏「打开」按钮；记忆上次目录，设计稿 5.5） ===
async function handleOpen() {
  try {
    const lastDir = localStorage.getItem("jietu-viewer:last-dir") || undefined;
    const sel = await openFileDialog({
      multiple: false,
      filters: IMAGE_FILTERS,
      defaultPath: lastDir,
    });
    if (typeof sel === "string" && sel) {
      await loadPath(sel);
    }
  } catch (e) {
    message.error(`打开对话框失败: ${e}`, { duration: 2200 });
  }
}

// === 打开相册（工具栏按钮：选文件夹 → 挂载相册源 → 切相册页网格浏览） ===
async function handleOpenFolder() {
  try {
    const sel = await openFileDialog({ directory: true, multiple: false });
    if (typeof sel === "string" && sel) {
      await invoke("add_library_root", { path: sel });
      viewMode.value = "library";
      // 通知 LibraryView 重扫列表并定位到全部图片
      window.dispatchEvent(new CustomEvent("jietu:library-refresh"));
      message.success(`已打开相册：${sel}`, { duration: 2200 });
    }
  } catch (e) {
    message.error(`打开相册失败: ${e}`, { duration: 2200 });
  }
}

/** 从完整路径取目录（兼容 / 与 \） */
function dirOf(p: string): string {
  const i = Math.max(p.lastIndexOf("\\"), p.lastIndexOf("/"));
  return i > 0 ? p.slice(0, i) : "";
}

/** 加载图片数据到指定 tab：open_image → 并行拉同目录列表 + EXIF（失败容错不阻塞） */
async function openInto(tab: ViewerTab, path: string) {
  const info = await invoke<ImageInfo>("open_image", { path });
  tab.currentPath = path;
  tab.imageInfo = info;
  tab.pdfPage = 1; // 换图重置 PDF 页码
  // 记录"已打开"（缩略图角标用；重复打开不累计）
  if (!tab.visited.includes(path)) tab.visited.push(path);
  const dir = dirOf(path);
  const tasks: Promise<void>[] = [
    invoke<ExifData>("get_exif", { path })
      .then((d) => {
        tab.exifData = d;
      })
      .catch(() => {
        tab.exifData = null; // 后端未就绪容错
      }),
  ];
  // 目录列表仅在换目录时重拉（同目录翻页沿用缓存）
  if (dir !== tab.dir || tab.imageList.length === 0) {
    tab.dir = dir;
    tasks.push(
      invoke<ImageEntry[]>("list_directory_images", { dir })
        .then((list) => {
          tab.imageList = list;
          // 同步轮询签名（本次即为最新，避免下轮重复拉取）
          dirListSig = `${list.length}:${list[list.length - 1]?.path ?? ""}`;
        })
        .catch(() => {
          tab.imageList = []; // 后端未就绪容错
        }),
    );
  }
  await Promise.all(tasks);
  // 记忆上次打开目录（下次打开对话框默认定位，设计稿 5.5）
  if (dir) localStorage.setItem("jietu-viewer:last-dir", dir);
}

/** 视频扩展（拖入看图窗口的视频转独立播放器窗口打开，不进图片管线） */
const VIDEO_EXTS = ["mp4", "mkv", "mov", "avi", "webm", "flv", "ts", "m4v"];

/** 打开图片（文件夹级 tab：所有外部入口统一走这）
 *  图所在目录已有标签 → 切到该标签并显示该图（同目录不重复建标签）；
 *  否则新建标签并激活；底部缩略图条随标签展示该目录全部图片。
 *  视频文件 → 独立原生播放器窗口（PotPlayer 形态，不与看图共用窗口） */
async function loadPath(path: string) {
  if (switching.value) return;
  const ext = (path.split(".").pop() ?? "").toLowerCase();
  if (VIDEO_EXTS.includes(ext)) {
    invoke("video_play_open", { paths: [path], opts: {} }).catch((e: unknown) => {
      message.error(`打开视频失败: ${e}`, { duration: 3000 });
    });
    return;
  }
  const dir = dirOf(path);
  const exist = tabs.value.findIndex((t) => t.dir === dir);
  if (exist >= 0) {
    activeIdx.value = exist;
    viewMode.value = "viewer";
    if (tabs.value[exist].currentPath !== path) await switchInTab(path);
    return;
  }
  switching.value = true;
  const raw: ViewerTab = { dir, currentPath: path, imageInfo: null, imageList: [], exifData: null, showExif: false, visited: [], pdfPage: 1 };
  try {
    tabs.value.push(raw);
    activeIdx.value = tabs.value.length - 1;
    viewMode.value = "viewer"; // 任一来源的打开 → 切到单图浏览
    // 必须取数组内的代理对象写入：直接改 raw 原始对象不触发响应式（Vue3 代理陷阱）
    await openInto(tabs.value[tabs.value.length - 1], path);
  } catch (e) {
    // 失败回滚：移除刚建的空 tab
    const i = tabs.value.indexOf(raw);
    if (i >= 0) {
      tabs.value.splice(i, 1);
      activeIdx.value = Math.min(activeIdx.value, Math.max(tabs.value.length - 1, 0));
    }
    if (tabs.value.length === 0) viewMode.value = "library";
    message.error(`打开图片失败: ${e}`, { duration: 2200 });
  } finally {
    switching.value = false;
  }
}

/** 当前 tab 内切换图片（翻页/缩略图点击/幻灯片：不新建 tab） */
async function switchInTab(path: string) {
  const tab = tabs.value[activeIdx.value];
  if (!tab || switching.value || path === tab.currentPath) return;
  switching.value = true;
  try {
    await openInto(tab, path);
  } catch (e) {
    message.error(`打开图片失败: ${e}`, { duration: 2200 });
  } finally {
    switching.value = false;
  }
}

/** 关闭标签：关的是当前 → 激活相邻；关前面的 → 索引左移；
 *  全关 → 相册内打开的回相册页；外部来源（双击文件/主面板选图）隐藏窗口 */
function closeTab(idx: number) {
  if (idx < 0 || idx >= tabs.value.length) return;
  tabs.value.splice(idx, 1);
  if (tabs.value.length === 0) {
    activeIdx.value = 0;
    if (lastOpenedFromLibrary.value) {
      viewMode.value = "library";
    } else {
      // 外部来源且未用过相册：隐藏窗口（下次双击图再唤起）
      viewMode.value = "library";
      invoke("hide_viewer_window").catch(() => {});
    }
  } else if (activeIdx.value >= tabs.value.length) {
    activeIdx.value = tabs.value.length - 1;
  } else if (idx < activeIdx.value) {
    activeIdx.value -= 1;
  }
}

// === 上一张 / 下一张（当前 tab 内切换；多页 PDF 先翻页，到页边界继续切图） ===
function goPrev() {
  if (isMultiPagePdf.value && pdfPage.value > 1) {
    pdfGoto(pdfPage.value - 1);
    return;
  }
  const i = currentIndex.value;
  if (i > 0) switchInTab(imageList.value[i - 1].path);
}
function goNext() {
  if (isMultiPagePdf.value && pdfPage.value < pdfPageCount.value) {
    pdfGoto(pdfPage.value + 1);
    return;
  }
  const i = currentIndex.value;
  if (i >= 0 && i < imageList.value.length - 1) {
    switchInTab(imageList.value[i + 1].path);
  }
}

// 缩略图点击切换（当前 tab 内）
function handleSelect(path: string) {
  if (path !== currentPath.value) switchInTab(path);
}

// === 键盘快捷键（设计稿 5.2：←/→ 切换、空格全屏、Delete 回收站、F5 放映） ===
function onKeydown(e: KeyboardEvent) {
  // 输入控件聚焦时不拦截
  const tag = (e.target as HTMLElement)?.tagName;
  if (tag === "INPUT" || tag === "TEXTAREA") return;
  // 相册/编辑/拼图模式：仅 Esc 关闭编辑/拼图子页，其余交给对应组件自身处理
  if (viewMode.value !== "viewer") {
    if (
      e.key === "Escape" &&
      (viewMode.value === "editor" || viewMode.value === "collage")
    ) {
      closeSubPage();
    }
    return;
  }
  if (e.key === "ArrowLeft") goPrev();
  else if (e.key === "ArrowRight") goNext();
  else if ((e.ctrlKey || e.metaKey) && (e.key === "c" || e.key === "C")) {
    e.preventDefault(); // 系统热键：复制当前图到剪贴板（与看图工具栏「复制」一致）
    handleCopy();
  } else if (e.key === " ") {
    e.preventDefault(); // 防止页面滚动
    viewerRef.value?.toggleFullscreen();
  } else if (e.key === "Delete") {
    handleDelete();
  } else if (e.key === "F5") {
    e.preventDefault(); // 拦截浏览器刷新
    toggleSlideshow();
  } else if (e.key === "Escape" && hdrEmbedded.value) {
    e.preventDefault(); // 真 HDR 嵌入：Esc 退出回 SDR 预览（画布原生侧同样支持）
    invoke("close_hdr_viewer").catch(() => {});
  } else if (e.key === "Escape" && slideshow.value) {
    toggleSlideshow(); // Esc 停止放映
  } else if (e.ctrlKey && (e.key === "w" || e.key === "W")) {
    e.preventDefault(); // Ctrl+W 关闭当前标签（浏览器习惯）
    closeTab(activeIdx.value);
  }
}

// 外部打开事件反注册句柄
let unlistenOpen: (() => void) | null = null;
// 拖拽打开反注册句柄
let unlistenDrop: (() => void) | null = null;
// 隐私相册热键事件反注册句柄
let unlistenVault: (() => void) | null = null;
// 右键文件夹浏览事件反注册句柄
let unlistenLibrary: (() => void) | null = null;
// 右键「添加到隐私相册」事件反注册句柄
let unlistenVaultAdd: (() => void) | null = null;
// 主面板「打开相册」事件反注册句柄
let unlistenAlbum: (() => void) | null = null;
// 标题栏 X（有标签时）关当前标签事件反注册句柄
let unlistenCloseTab: (() => void) | null = null;
// 真 HDR 嵌入视图销毁通知反注册句柄
let unlistenHdrClosed: (() => void) | null = null;
// 真 HDR 嵌入窗口就绪通知反注册句柄
let unlistenHdrReady: (() => void) | null = null;

// === 目录实时刷新（自动保存截图等新图即时出现在缩略图条） ===
/** 上次拉取的目录列表签名（长度 + 末尾路径），变化才更新，避免无谓重渲染 */
let dirListSig = "";
/** 刷新进行中标记（防重入） */
let dirRefreshing = false;
async function refreshActiveDirList() {
  // GPU 空转治理：窗口隐藏（document.hidden）时直接跳过——定时器唤醒本身开销
  // 极低，贵的是 invoke 的目录扫描；不用 clearInterval/visibilitychange 恢复方案
  // （清/建定时器逻辑分散易漏），恢复可见后下一轮 2s 内自动续上
  if (document.hidden) return;
  const tab = activeTab.value;
  if (
    dirRefreshing ||
    switching.value ||
    viewMode.value !== "viewer" ||
    !tab ||
    !tab.dir ||
    tab.imageList.length === 0
  ) {
    return;
  }
  dirRefreshing = true;
  try {
    const list = await invoke<ImageEntry[]>("list_directory_images", { dir: tab.dir });
    const sig = `${list.length}:${list[list.length - 1]?.path ?? ""}`;
    if (sig !== dirListSig) {
      dirListSig = sig;
      tab.imageList = list; // currentPath 不动，缩略图条响应式追加新图
    }
  } catch {
    // 目录暂时不可读（截图写入中）等场景静默跳过，下轮重试
  } finally {
    dirRefreshing = false;
  }
}
/** 轻量轮询：2s 一次（打开图片/切目录时 openInto 内同步重置签名） */
let dirWatchTimer = 0;

onMounted(async () => {
  window.addEventListener("keydown", onKeydown);
  // 隐私相册内双击查看（VaultPanel 派发 CustomEvent）
  window.addEventListener("jietu:vault-open", onVaultOpenImage);
  // 缩略图条目录轮询（仅在 viewer 模式有标签时拉取，开销极低）
  dirWatchTimer = window.setInterval(refreshActiveDirList, 2000);
  // 监听外部打开请求（主面板「打开图片」按钮 / 托盘菜单 / 双击图片文件）
  try {
    const { listen } = await import("@tauri-apps/api/event");
    // payload 兼容两种：string（旧）| { path, external }（新；external=相册外来源）
    unlistenOpen = await listen<string | { path: string; external?: boolean }>("viewer://open", (ev) => {
      const raw = ev.payload;
      const p = typeof raw === "string" ? raw : raw?.path;
      if (!p) return;
      // 外部来源（双击文件/主面板选图）：关最后一个标签时隐藏窗口而非回相册页
      if (typeof raw === "object" && raw.external === true) lastOpenedFromLibrary.value = false;
      loadPath(p);
    });
    // 全局热键 Ctrl+Shift+L → 相册页并切到隐私相册导航
    unlistenVault = await listen("viewer://vault", () => {
      viewMode.value = "library";
      window.dispatchEvent(new CustomEvent("jietu:vault-nav"));
    });
    // 标题栏 X（有标签打开时）：关当前标签回相册页，而不是关整个窗口
    unlistenCloseTab = await listen("viewer://close-current-tab", () => {
      if (viewMode.value === "viewer" && tabs.value.length > 0) {
        closeTab(activeIdx.value);
      }
    });
    // 主面板「打开相册」→ 直接切相册页浏览（无 payload，不挂载新源）
    unlistenAlbum = await listen("viewer://album", () => {
      invoke("trace_log", { msg: `ViewerWindow 收到 viewer://album（当前模式 ${viewMode.value}）` }).catch(() => {});
      viewMode.value = "library";
      // 首次进入时 LibraryView watch immediate 已自动加载；再次进入刷新一次列表
      window.dispatchEvent(new CustomEvent("jietu:library-refresh"));
    });
    // 资源管理器右键文件夹「用 jietu-hdr 浏览」→ 挂载为相册源并打开相册页
    unlistenLibrary = await listen<string>("viewer://library", async (ev) => {
      const dir = ev.payload;
      if (typeof dir !== "string" || !dir) return;
      invoke("trace_log", { msg: `ViewerWindow 收到 viewer://library: ${dir}（当前模式 ${viewMode.value}）` }).catch(() => {});
      viewMode.value = "library";
      try {
        await invoke("add_library_root", { path: dir });
        message.success(`已挂载相册源并开始浏览：${dir}`, { duration: 2600 });
        // 触发 LibraryView 刷新（其内部 watch visible 不触发，需显式通知）
        window.dispatchEvent(new CustomEvent("jietu:library-refresh"));
      } catch (e) {
        message.error(`挂载相册源失败: ${e}`, { duration: 2200 });
      }
    });
    // 资源管理器右键「添加到隐私相册」：解锁态直接加密移入，否则引导去解锁
    unlistenVaultAdd = await listen<string[]>("viewer://vault-add", async (ev) => {
      const paths = Array.isArray(ev.payload) ? ev.payload : [];
      if (paths.length === 0) return;
      try {
        const st = await invoke<{ initialized: boolean; unlocked: boolean }>("vault_status");
        if (st.initialized && st.unlocked) {
          const n = await invoke<number>("vault_add", { paths });
          message.success(`已加密移入隐私相册（${n} 张）`, { duration: 2200 });
        } else {
          // 未初始化/未锁定：切到隐私相册页引导操作（文件尚未移动，安全）
          viewMode.value = "library";
          window.dispatchEvent(new CustomEvent("jietu:vault-nav"));
          message.warning(
            st.initialized ? "隐私相册未解锁，请先解锁后再从右键添加" : "请先创建隐私相册并解锁，再从右键添加",
            { duration: 3600 },
          );
        }
      } catch (e) {
        message.error(`添加到隐私相册失败: ${e}`, { duration: 2200 });
      }
    });
  } catch {
    // 非 Tauri 环境忽略
  }
  // 拖拽图片到窗口直接打开（设计稿 5.5）
  try {
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    unlistenDrop = await getCurrentWindow().onDragDropEvent((ev) => {
      const p = ev.payload;
      if (p.type === "drop" && p.paths.length > 0) loadPath(p.paths[0]);
    });
    // 主窗口缩放/全屏 → 真 HDR 嵌入容器矩形重同步
    getCurrentWindow().onResized(() => { scheduleHdrRect(); }).catch(() => {});
  } catch {
    // 非 Tauri 环境忽略
  }
  // 真 HDR 嵌入视图销毁/就绪通知（Esc 关闭 / D3D 就绪补发矩形）
  try {
    const { listen } = await import("@tauri-apps/api/event");
    unlistenHdrClosed = await listen("hdr-view://closed", () => {
      onHdrViewClosed();
    });
    unlistenHdrReady = await listen("hdr-view://ready", () => {
      onHdrViewReady();
    });
  } catch {
    // 非 Tauri 环境忽略
  }
  window.addEventListener("resize", scheduleHdrRect);
});
onUnmounted(() => {
  window.removeEventListener("keydown", onKeydown);
  window.removeEventListener("jietu:vault-open", onVaultOpenImage);
  window.removeEventListener("resize", scheduleHdrRect);
  unlistenOpen?.();
  unlistenVault?.();
  unlistenLibrary?.();
  unlistenVaultAdd?.();
  unlistenAlbum?.();
  unlistenCloseTab?.();
  unlistenDrop?.();
  unlistenHdrClosed?.();
  unlistenHdrReady?.();
  if (hdrRectRaf) window.cancelAnimationFrame(hdrRectRaf);
  if (slideTimer) window.clearInterval(slideTimer);
  if (toolbarHideTimer) window.clearTimeout(toolbarHideTimer);
  if (dirWatchTimer) window.clearInterval(dirWatchTimer);
  if (residentTimer) window.clearTimeout(residentTimer); // full_resident 横幅淡出定时器
});

// === 另存为（转格式 PNG/JPEG/WebP） ===
const SAVE_FILTERS = [
  { name: "PNG 图片", extensions: ["png"] },
  { name: "JPEG 图片", extensions: ["jpg", "jpeg"] },
  { name: "WebP 图片", extensions: ["webp"] },
];

// HDR 源格式（另存为时走当前激活预设重新色调映射输出 SDR）
const HDR_EXTS = ["exr", "jxl", "jxr", "wdp"];

async function handleSave() {
  if (!currentPath.value) {
    message.info("请先打开一张图片", { duration: 2200 });
    return;
  }
  try {
    const name = currentPath.value.split(/[\\/]/).pop() ?? "image.png";
    // HDR 源默认加 _sdr 后缀（原文件不动，输出为新图）
    const dot = name.lastIndexOf(".");
    const isHdr = HDR_EXTS.includes(
      (dot > 0 ? name.slice(dot + 1) : "").toLowerCase(),
    ) || imageInfo.value?.format === "png-hdr";
    const defaultName = isHdr
      ? `${dot > 0 ? name.slice(0, dot) : name}_sdr.png`
      : name;
    const dst = await saveFileDialog({
      filters: SAVE_FILTERS,
      defaultPath: defaultName,
    });
    if (!dst) return;
    // 按保存路径扩展名决定目标格式
    const ext = (dst.split(".").pop() ?? "").toLowerCase();
    const format = ext === "jpg" || ext === "jpeg" ? "jpeg" : ext === "webp" ? "webp" : "png";
    await invoke("convert_image", {
      src: currentPath.value,
      dst,
      format,
      quality: 90,
    });
    message.success(
      isHdr ? "已按当前预设重新输出 SDR 图像" : "已保存",
      { duration: 2400 },
    );
  } catch (e) {
    message.error(`保存失败: ${e}`, { duration: 2200 });
  }
}

// === HDR 亮度调节面板（即时重渲） ===
const tonePanelVisible = ref(false);

// === AI 放大面板（waifu2x 超分） ===
const upscalePanelVisible = ref(false);
// HDR 源判定（与后端 HdrSource 缓存口径一致）
const isHdrSource = computed(() => {
  const f = imageInfo.value?.format;
  if (!f || !currentPath.value) return false;
  return f === "png-hdr" || ["exr", "jxl", "jxr", "wdp"].includes(
    (currentPath.value.split(".").pop() ?? "").toLowerCase(),
  );
});

// === 真 HDR 查看（D3D11 scRGB 子窗口嵌入本窗口，绕过 WebView2 的 SDR 限制） ===
interface HdrDisplayInfo {
  any_hdr: boolean;
  max_luminance: number;
  sdr_white_nits: number;
  /** 面板原生色域分类（null = 未识别） */
  gamut_name: string | null;
  p3_coverage: number | null;
  bt2020_coverage: number | null;
}
const hdrInfo = ref<HdrDisplayInfo | null>(null);
/** 真 HDR 按钮：HDR 源 + 系统存在已开启 HDR 的显示器（非 HDR 机器不显示） */
const hdrAvailable = computed(
  () => isHdrSource.value && (hdrInfo.value?.any_hdr ?? false),
);
/** 系统存在已开启 HDR 的显示器（SDR→HDR AI 反转按钮显示条件，与当前图无关） */
const hdrDisplayOn = computed(() => hdrInfo.value?.any_hdr ?? false);
async function loadHdrInfo() {
  try {
    hdrInfo.value = await invoke<HdrDisplayInfo>("hdr_display_info");
  } catch {
    hdrInfo.value = null;
  }
}
watch(currentPath, loadHdrInfo, { immediate: true });

// === JXL 动图探测（后端 BasicInfo.have_animation） ===
const isAnimation = ref(false);
watch(
  currentPath,
  async (p) => {
    isAnimation.value = false;
    if (!p || !p.toLowerCase().endsWith(".jxl")) return;
    try {
      isAnimation.value = await invoke<boolean>("probe_animation", { path: p });
    } catch {
      isAnimation.value = false;
    }
  },
  { immediate: true },
);

/** 播放动图：独立原生窗口流式回放（真 HDR 交换链，无限循环） */
async function openAnimationPlayer() {
  if (!currentPath.value) return;
  animOpening.value = true;
  try {
    await invoke("open_animation_player", { path: currentPath.value });
    // 成功返回 = 播放窗口已弹出 → 进度态收敛为 HUD 小条（前端面板仅剩反馈职责）
    animWindowOpened.value = true;
  } catch (e) {
    message.error(String(e), { duration: 3600 });
  } finally {
    animOpening.value = false;
  }
}

// === JXL 动图 GPU 常驻填充进度（anim://fill 事件驱动，见 useAnimFill.ts） ===
const {
  filledFrames: animFilledFrames,
  filledMs: animFilledMs,
  totalFrames: animTotalFrames,
  state: animState,
  backend: animBackend,
  isStalled: animStalled,
  reset: resetAnimFill,
} = useAnimFill();
/** open_animation_player 进行中（点击播放 → 窗口弹出前的过渡态） */
const animOpening = ref(false);
/** 播放窗口已弹出 → 进度条收敛为 HUD 模式（纯文字小条） */
const animWindowOpened = ref(false);
/** full_resident 就绪横幅 1.5s 后淡出标记 */
const residentFadeDone = ref(false);
let residentTimer = 0;

watch(animState, (s) => {
  if (residentTimer) {
    window.clearTimeout(residentTimer);
    residentTimer = 0;
  }
  if (s === "full_resident") {
    residentFadeDone.value = false;
    residentTimer = window.setTimeout(() => {
      residentTimer = 0;
      residentFadeDone.value = true;
    }, 1500);
  } else {
    residentFadeDone.value = false;
  }
});

// 换图：清空上一张动图的填充会话（播放窗口标志一并复位，等新一轮 anim://fill 事件）
watch(currentPath, () => {
  resetAnimFill();
  animWindowOpened.value = false;
  animOpening.value = false;
});

/** 填充百分比：total 已知 → 实际比例；未知 → 以已缓冲时长近似（封顶 95% 保持"不定"观感） */
const animFillPct = computed(() => {
  const t = animTotalFrames.value;
  if (t != null && t > 0) return Math.min(100, Math.round((animFilledFrames.value / t) * 100));
  return Math.min(95, Math.floor(animFilledMs.value / 200));
});
const animFillSeconds = computed(() => (animFilledMs.value / 1000).toFixed(1));

/**
 * 小条显示矩阵（O=animOpening 过渡态 × W=animWindowOpened 播放窗口已开 × S=animState；T=animStalled，F=residentFadeDone）。
 * 文案分支自上而下短路命中（优先级递减），可见性由 animFillVisible 独立计算，二者无互相覆盖：
 * - S=degraded（O/W 任意）：橙色常驻警告（最高优先级，压过 HUD 徽标文案，不并显后端徽标）
 * - S=full_resident（O/W 任意）：主题色横幅「播放中 · {backend} 后端 · 已常驻显存」（total 未知时省略尾段），
 *   1.5s 后 F=true 整体淡出；此后若 S 回落 filling，watch 复位 F 且 filling 恒可见 → 缓冲进度自动恢复显示
 * - S=filling & W=否：T → 「解码较慢…」；否则过渡文案 + 进度条（total 未知 → 「正在缓冲 N 帧 / S 秒」）
 * - S=filling & W=是：HUD 模式纯文字「播放中 · {backend} 后端 · 缓冲 N 帧」（total 已知附 total 与百分比，无进度条）
 * - S=idle & W=否：O → 「正在打开动图播放器…」；未点播放 → 可见性为隐藏不打扰
 * - S=idle & W=是：可见性为隐藏（reset 与 W 复位在同一 watch 同步发生，正常流程不可渲染，仅文案分支兜底）
 */
const animFillText = computed(() => {
  if (animState.value === "degraded") return "显存/解码异常，已缓冲部分仍可播放";
  if (animState.value === "full_resident") {
    const badge = `播放中 · ${animBackend.value ?? "native"} 后端`;
    return animTotalFrames.value != null ? `${badge} · 已常驻显存` : badge;
  }
  if (animStalled.value && !animWindowOpened.value) return "解码较慢，仍在后台准备…";
  if (animOpening.value && animState.value === "idle" && !animWindowOpened.value)
    return "正在打开动图播放器…";
  if (animWindowOpened.value) {
    // HUD 模式（播放窗口已开、原生侧渲染）：面板侧只留后端徽标 + 填充态
    const badge = `播放中 · ${animBackend.value ?? "native"} 后端`;
    if (animState.value === "filling") {
      return animTotalFrames.value != null
        ? `${badge} · 缓冲 ${animFilledFrames.value} / ${animTotalFrames.value} 帧 · ${animFillPct.value}%`
        : `${badge} · 缓冲 ${animFilledFrames.value} 帧`;
    }
    return badge; // S=idle 兜底（正常流程在可见性层已隐藏）
  }
  return animTotalFrames.value != null
    ? `${animFilledFrames.value} / ${animTotalFrames.value} 帧 · ${animFillPct.value}%`
    : `正在缓冲 ${animFilledFrames.value} 帧 / ${animFillSeconds.value} 秒`;
});

/** 显示条件：无事件不打扰；degraded 常驻警告；full_resident 横幅 1.5s 后淡出 */
const animFillVisible = computed(() => {
  if (!isAnimation.value || hdrEmbedded.value) return false;
  if (animOpening.value) return true;
  if (animState.value === "idle") return false;
  if (animState.value === "full_resident") return !residentFadeDone.value;
  return true;
});
/** 进度条仅过渡态（窗口未弹出且仍在填充）显示；HUD / 就绪 / 警告为纯文字 */
const animFillShowBar = computed(
  () => animState.value === "filling" && !animWindowOpened.value && !animStalled.value,
);

/** 嵌入模式激活：图片主区由原生 scRGB 子窗口接管（本组件渲染容器边框+工具栏+状态栏） */
const hdrEmbedded = ref(false);
/** HDR 容器插槽（原生子窗口精确覆盖此元素的屏幕矩形） */
const hdrSlotRef = ref<HTMLDivElement | null>(null);
/** 换图重开窗口期间忽略旧视图的 closed 事件（避免布局误退出） */
let hdrSwitching = false;

/** 容器矩形同步：CSS px × devicePixelRatio → 物理客户区坐标下发后端
 *  画布插槽从工具栏下方 56px 起（工具栏留给 WebView2，可见可点）；
 *  不做 SetWindowRgn 挖洞（与 flip 交换链不兼容，曾致卡白膜） */
async function syncHdrRect() {
  if (!hdrEmbedded.value) return;
  const el = hdrSlotRef.value;
  if (!el) return;
  const r = el.getBoundingClientRect();
  const dpr = window.devicePixelRatio || 1;
  // 容器诊断日志（分析 SDR/HDR 画布范围一致性）
  invoke("trace_log", {
    msg: `[画布对齐] HDR插槽 CSS(${Math.round(r.left)},${Math.round(r.top)} ${Math.round(r.width)}x${Math.round(r.height)}) dpr=${dpr} 物理(${Math.round(r.left * dpr)},${Math.round(r.top * dpr)} ${Math.round(r.width * dpr)}x${Math.round(r.height * dpr)})`,
  }).catch(() => {});
  await invoke("set_hdr_view_rect", {
    x: Math.round(r.left * dpr),
    y: Math.round(r.top * dpr),
    w: Math.round(r.width * dpr),
    h: Math.round(r.height * dpr),
  }).catch(() => {});
}

/** Bolt 按钮开关：开 → 创建子窗口 + 布局切换 + 首个矩形；关 → 关闭回 SDR 预览 */
async function openHdrView() {
  if (hdrEmbedded.value) {
    await invoke("close_hdr_viewer").catch(() => {});
    return;
  }
  if (!currentPath.value) return;
  hdrSwitching = true;
  // 面板与抽屉会压在原生子窗口下面（WebView2 层级更低），进入前收起
  tonePanelVisible.value = false;
  showExif.value = false;
  try {
    await invoke("open_hdr_viewer", { path: currentPath.value });
    hdrEmbedded.value = true;
    // SDR 预览 fit 基准切换到画布同范围（56px 下方），内容同位置点亮零跳变
    viewerRef.value?.fitView();
    await nextTick();
    await syncHdrRect();
    // 兜底重发：后端排队消费 + ready 事件双保险之外的第三重（事件偶发丢失场景）
    window.setTimeout(() => { if (hdrEmbedded.value) syncHdrRect(); }, 900);
  } catch (e) {
    message.error(String(e), { duration: 3600 });
  } finally {
    window.setTimeout(() => { hdrSwitching = false; }, 400);
  }
}

/** 原生子窗口销毁通知（Esc / 后端关闭）：退出 HDR 布局回 SDR 预览 */
async function onHdrViewClosed() {
  if (hdrSwitching) return; // 换图重开时旧视图的销毁，忽略
  hdrEmbedded.value = false;
  itmEmbedded.value = false;
  // SDR 预览恢复全舞台 fit 基准（退出画布同范围模式）
  viewerRef.value?.fitView();
}

// === SDR→HDR（AI 反转，HDRTVNet 逆色调映射） ===
/** 推理进行中（首次含模型加载；1080p CPU 数秒，按钮转圈禁用） */
const itmBusy = ref(false);
/** 当前嵌入画布为 ITM 合成源（工具栏显示 AI 强度滑杆） */
const itmEmbedded = ref(false);
/** AI 反转强度（0-1；与后端默认 0.7 一致） */
const itmStrength = ref(0.7);
let itmDebounceTimer = 0;

interface ItmInfo {
  itmPath: string;
  width: number;
  height: number;
  elapsedMs: number;
}

/** ITM 推理 → 以合成键打开/换源真 HDR 嵌入（共用流程） */
async function runItmToViewer() {
  const info = await invoke<ItmInfo>("itm_to_hdr", {
    path: currentPath.value,
    strength: itmStrength.value,
  });
  hdrSwitching = true;
  await invoke("open_hdr_viewer", { path: info.itmPath });
  itmEmbedded.value = true;
  hdrEmbedded.value = true;
  viewerRef.value?.fitView();
  await nextTick();
  await syncHdrRect();
  // 兜底重发（与 openHdrView 同款三重保险）
  window.setTimeout(() => { if (hdrEmbedded.value) syncHdrRect(); }, 900);
}

/** Sparkles 按钮：SDR 图 AI 反转 → 自动进入真 HDR 嵌入；再次点击退出 */
async function openItmView() {
  if (!currentPath.value || itmBusy.value) return;
  if (hdrEmbedded.value && itmEmbedded.value) {
    await invoke("close_hdr_viewer").catch(() => {});
    return;
  }
  itmBusy.value = true;
  // 面板与抽屉会压在原生子窗口下面（WebView2 层级更低），进入前收起
  tonePanelVisible.value = false;
  showExif.value = false;
  try {
    await runItmToViewer();
  } catch (e) {
    message.error(String(e), { duration: 4200 });
  } finally {
    itmBusy.value = false;
    window.setTimeout(() => { hdrSwitching = false; }, 400);
  }
}

/** AI 强度滑杆：防抖 600ms 重跑推理（解码缓存命中，仅模型 + 混合重算）原位换源 */
function onItmStrength(v: number) {
  itmStrength.value = v;
  if (itmDebounceTimer) window.clearTimeout(itmDebounceTimer);
  itmDebounceTimer = window.setTimeout(() => {
    itmDebounceTimer = 0;
    if (!hdrEmbedded.value || !itmEmbedded.value || !currentPath.value) return;
    itmBusy.value = true;
    runItmToViewer()
      .catch((e) => message.error(String(e), { duration: 4200 }))
      .finally(() => {
        itmBusy.value = false;
        window.setTimeout(() => { hdrSwitching = false; }, 400);
      });
  }, 600);
}

/** 窗口就绪通知（D3D 初始化完成但矩形尚未到达）：补发矩形 */
async function onHdrViewReady() {
  if (!hdrEmbedded.value) return;
  await syncHdrRect();
}

/** 主窗口尺寸变化（含全屏切换动画）：重新测量插槽并同步（rAF 防抖） */
let hdrRectRaf = 0;
function scheduleHdrRect() {
  if (!hdrEmbedded.value) return;
  if (hdrRectRaf) return;
  hdrRectRaf = window.requestAnimationFrame(() => {
    hdrRectRaf = 0;
    window.setTimeout(syncHdrRect, 30); // 布局稳定后再测（全屏过渡动画）
  });
}

/** 切换图片：同为 HDR 源 → 原位换图；否则退出嵌入模式 */
watch(currentPath, (p) => {
  if (!hdrEmbedded.value || !p) return;
  const ext = (p.split(".").pop() ?? "").toLowerCase();
  if (HDR_EXTS.includes(ext)) {
    hdrSwitching = true;
    invoke("open_hdr_viewer", { path: p })
      .then(async () => { await syncHdrRect(); })
      .catch(() => { hdrEmbedded.value = false; })
      .finally(() => window.setTimeout(() => { hdrSwitching = false; }, 400));
  } else {
    itmEmbedded.value = false;
    invoke("close_hdr_viewer").catch(() => {});
  }
});

/** 离开看图模式（编辑/拼图/相册页）→ 关闭嵌子窗口（否则会浮在子页上） */
watch(viewMode, (m) => {
  if (m !== "viewer" && hdrEmbedded.value) {
    invoke("close_hdr_viewer").catch(() => {});
  }
});

/** 面板回调：用新预设/覆盖参数重渲 → 交叉淡入替换 URL → 写调参历史 */
async function onToneRerender(preset: string, overrides: object) {
  if (!currentPath.value || !activeTab.value) return;
  try {
    const info = await invoke<ImageInfo>("retonemap_image", {
      path: currentPath.value,
      preset,
      overrides,
    });
    activeTab.value.imageInfo = { ...activeTab.value.imageInfo!, url: info.url };
    // 参数快照入 sidecar（去重 60s；跟随图片移动）
    invoke("history_add", { path: currentPath.value, preset, overrides }).catch(() => {});
  } catch (e) {
    message.error(`重渲失败: ${e}`, { duration: 2200 });
  }
}

// === 调参历史面板（TonePanel「历史」按钮） ===
const historyVisible = ref(false);
/** 重渲淡入模式：调参/历史条目应用时启用（跳过模糊重置，180ms 交叉淡入） */
const crossfade = ref(false);
function openHistory() {
  historyVisible.value = !historyVisible.value;
}
/** 历史条目应用：重渲（淡入）+ 标记 applied */
async function onHistoryApply(entry: { ts: number; preset: string; overrides: object }) {
  if (!currentPath.value) return;
  crossfade.value = true;
  await onToneRerender(entry.preset, entry.overrides);
  invoke("history_apply", { path: currentPath.value, ts: entry.ts }).catch(() => {});
  window.setTimeout(() => { crossfade.value = false; }, 400);
}

// === HDR 文件夹预解码：打开 HDR 图时后台预载同目录（切换瞬时） ===
watch(
  () => [currentPath.value, isHdrSource.value] as const,
  ([p, hdr]) => {
    if (!p || !hdr) return;
    const dir = p.slice(0, p.lastIndexOf("\\"));
    if (dir) invoke("prewarm_folder", { dir }).catch(() => {});
  },
);

// === 复制到剪贴板 ===
async function handleCopy() {
  if (!currentPath.value) {
    message.info("请先打开一张图片", { duration: 2200 });
    return;
  }
  try {
    await invoke("copy_image_to_clipboard", { path: currentPath.value });
    message.success("已复制到剪贴板", { duration: 2200 });
  } catch (e) {
    message.error(`复制失败: ${e}`, { duration: 2200 });
  }
}

// === ImageViewer 状态回传（工具栏显示） ===
function onScaleChange(v: number) {
  scale.value = v;
}
function onFullscreenChange(v: boolean) {
  fullscreen.value = v;
  scheduleHdrRect(); // 全屏切换动画完成后重测 HDR 容器矩形
}

// === ImageViewer 多操作方式回传（设计稿 5.1：侧键/轻扫/双指横滑 → 切换） ===
function onNav(dir: -1 | 1) {
  if (dir === -1) goPrev();
  else goNext();
}

// === 右键 / 触屏长按快捷菜单（设计稿 5.1；长按由浏览器转 contextmenu 同路径触发） ===
const ctxMenu = ref({ show: false, x: 0, y: 0 });
function openCtxMenu(x: number, y: number) {
  ctxMenu.value = { show: true, x, y };
}
const ctxOptions: DropdownOption[] = [
  { label: "上一张", key: "prev" },
  { label: "下一张", key: "next" },
  { type: "divider", key: "d1" },
  { label: "向左旋转 90°", key: "rotl" },
  { label: "向右旋转 90°", key: "rotr" },
  { label: "自适应窗口", key: "fit" },
  { type: "divider", key: "d2" },
  { label: "复制到剪贴板", key: "copy" },
  { label: "另存为…", key: "save" },
  { label: "设为桌面壁纸", key: "wallpaper" },
  { label: "EXIF 信息", key: "exif" },
  { type: "divider", key: "d3" },
  { label: "移入回收站", key: "delete" },
];
function onCtxSelect(key: string | number) {
  ctxMenu.value.show = false;
  switch (key) {
    case "prev":
      goPrev();
      break;
    case "next":
      goNext();
      break;
    case "rotl":
      viewerRef.value?.rotateLeft();
      break;
    case "rotr":
      viewerRef.value?.rotateRight();
      break;
    case "fit":
      viewerRef.value?.fitView();
      break;
    case "copy":
      handleCopy();
      break;
    case "save":
      handleSave();
      break;
    case "wallpaper":
      handleWallpaper();
      break;
    case "exif":
      showExif.value = !showExif.value;
      break;
    case "delete":
      handleDelete();
      break;
  }
}

// ==================== 设计稿融合功能 ====================

// === 设为桌面壁纸（设计稿 3.2 壁纸一键设置） ===
async function handleWallpaper() {
  if (!currentPath.value) {
    message.info("请先打开一张图片", { duration: 2200 });
    return;
  }
  try {
    await invoke("set_wallpaper", { path: currentPath.value });
    message.success("已设为桌面壁纸", { duration: 2200 });
  } catch (e) {
    message.error(`设置壁纸失败: ${e}`, { duration: 2200 });
  }
}

// === 幻灯片放映（设计稿 3.1：自动切换 + 全屏沉浸） ===
const slideshow = ref(false);
let slideTimer: number | null = null;
const SLIDE_INTERVAL = 3000; // 3 秒/张（设计稿：自定义间隔；简化为固定档）

function toggleSlideshow() {
  slideshow.value = !slideshow.value;
  if (slideshow.value) {
    // 开启时进入全屏并循环切换
    if (!fullscreen.value) viewerRef.value?.toggleFullscreen();
    slideTimer = window.setInterval(() => {
      const i = currentIndex.value;
      if (imageList.value.length === 0) return;
      if (i >= 0 && i < imageList.value.length - 1) {
        switchInTab(imageList.value[i + 1].path);
      } else {
        switchInTab(imageList.value[0].path); // 循环
      }
    }, SLIDE_INTERVAL);
    message.info("开始放映 · F5 或 Esc 停止", { duration: 2200 });
  } else {
    if (slideTimer) window.clearInterval(slideTimer);
    slideTimer = null;
  }
}

// === 删除 → 软件回收站（设计稿 3.3：7 天可恢复，柔和确认） ===
async function handleDelete() {
  if (!currentPath.value) return;
  const name = currentPath.value.split(/[\\/]/).pop() ?? "该图片";
  dialog.warning({
    title: "移入回收站",
    content: `「${name}」将移入软件回收站，7 天内可恢复。`,
    positiveText: "移入回收站",
    negativeText: "取消",
    onPositiveClick: async () => {
      try {
        const tab = activeTab.value;
        const i = currentIndex.value;
        const deleted = currentPath.value;
        await invoke("recycle_image", { path: deleted });
        message.success(`已移入回收站：${name}`, { duration: 2200 });
        if (!tab) return;
        // 刷新列表并切换到相邻图片（用户已切走 tab 则只清列表）
        const list = tab.imageList.filter((it) => it.path !== deleted);
        tab.imageList = list;
        if (list.length === 0) {
          closeTab(tabs.value.indexOf(tab)); // 列表空 → 关闭该标签
        } else if (tab === activeTab.value) {
          const next = list[Math.min(i, list.length - 1)];
          switchInTab(next.path);
        }
      } catch (e) {
        message.error(`删除失败: ${e}`, { duration: 2200 });
      }
    },
  });
}

// === 回收站弹窗（设计稿 3.3：列表/恢复/彻底删除/清空） ===
interface RecycleEntry {
  original_path: string;
  recycled_name: string;
  deleted_ts: number;
  display_name: string;
}
const showRecycle = ref(false);
const recycleList = ref<RecycleEntry[]>([]);

async function openRecycle() {
  showRecycle.value = true;
  await refreshRecycle();
}
async function refreshRecycle() {
  try {
    recycleList.value = await invoke<RecycleEntry[]>("list_recycle");
  } catch (e) {
    recycleList.value = [];
    message.error(`读取回收站失败: ${e}`, { duration: 2200 });
  }
}
function fmtTime(ts: number): string {
  // 秒/毫秒自适应
  const ms = ts > 1e12 ? ts : ts * 1000;
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}
async function restoreOne(e: RecycleEntry) {
  try {
    await invoke("restore_recycle", { recycledName: e.recycled_name });
    message.success(`已恢复：${e.display_name}`, { duration: 2200 });
    refreshRecycle();
  } catch (err) {
    message.error(`恢复失败: ${err}`, { duration: 2200 });
    refreshRecycle();
  }
}
async function purgeOne(e: RecycleEntry) {
  try {
    await invoke("purge_recycle", { recycledName: e.recycled_name });
    refreshRecycle();
  } catch (err) {
    message.error(`删除失败: ${err}`, { duration: 2200 });
  }
}
/** 彻底删除单条（危险操作二次确认，设计稿 5.5） */
function purgeOneConfirm(e: RecycleEntry) {
  dialog.warning({
    title: "彻底删除",
    content: `「${e.display_name}」将被永久删除，无法恢复。`,
    positiveText: "彻底删除",
    negativeText: "取消",
    onPositiveClick: () => purgeOne(e),
  });
}
function purgeAllConfirm() {
  dialog.warning({
    title: "清空回收站",
    content: `将彻底删除全部 ${recycleList.value.length} 项，无法恢复。`,
    positiveText: "清空",
    negativeText: "取消",
    onPositiveClick: async () => {
      try {
        const n = await invoke<number>("purge_all_recycle");
        message.success(`已清空回收站（${n} 项）`, { duration: 2200 });
        refreshRecycle();
      } catch (err) {
        message.error(`清空失败: ${err}`, { duration: 2200 });
      }
    },
  });
}

// === 沉浸模式（设计稿 5.5：工具栏自动隐藏，鼠标移到顶部唤起） ===
const toolbarVisible = ref(true);
let toolbarHideTimer: number | null = null;
/** 唤起工具栏并延迟自动隐藏（3s 无操作淡出） */
function revealToolbar() {
  toolbarVisible.value = true;
  if (toolbarHideTimer) window.clearTimeout(toolbarHideTimer);
  toolbarHideTimer = window.setTimeout(() => {
    toolbarVisible.value = false;
  }, 3000);
}

// === 已看图片面板（画布右侧按钮，点开在按钮下方显示本目录看过的缩略图） ===
const showVisited = ref(false);
/** 已看图片列表（按打开顺序，当前图排最后） */
const visitedItems = computed(() => {
  const tab = activeTab.value;
  if (!tab) return [];
  const byPath = new Map(tab.imageList.map((it) => [it.path, it]));
  return tab.visited
    .map((p) => byPath.get(p))
    .filter((it): it is ImageEntry => !!it);
});
/** 已看缩略图缓存（path → data URL；与 ThumbnailBar 同后端命令） */
const visitedThumbs = ref<Record<string, string>>({});
watch(
  visitedItems,
  (list) => {
    for (const it of list) {
      if (visitedThumbs.value[it.path]) continue;
      invoke<string>("get_thumbnail", { path: it.path })
        .then((b64) => {
          const url = b64.startsWith("data:") ? b64 : `data:image/png;base64,${b64}`;
          visitedThumbs.value = { ...visitedThumbs.value, [it.path]: url };
        })
        .catch(() => {});
    }
  },
  { immediate: true },
);
function toggleVisitedPanel() {
  showVisited.value = !showVisited.value;
}
/** 清除单张已看记录（不移除图片本身，仅去掉"已看"标记） */
function clearVisitedOne(path: string) {
  const tab = activeTab.value;
  if (!tab) return;
  tab.visited = tab.visited.filter((p) => p !== path);
}
/** 清空本标签全部已看记录（当前图重新计入：正在查看即"已看"） */
function clearVisitedAll() {
  const tab = activeTab.value;
  if (!tab) return;
  tab.visited = tab.currentPath ? [tab.currentPath] : [];
}
</script>

<template>
  <div class="viewer-root">
    <!-- 标题栏复用（置顶/主题/Win11 按钮全套） -->
    <TitleBar
      title="jietu-hdr · 看图"
      :mode="mode"
      :is-dark="isDark"
      :set-mode="setMode"
    />
    <!-- 文件夹级标签栏（浏览器式：一文件夹一标签，常驻显示，仅点 × 关闭） -->
    <ViewerTabs
      v-if="viewMode === 'viewer' && tabs.length > 0"
      :tabs="tabItems"
      :active="activeIdx"
      @select="activeIdx = $event"
      @close="closeTab"
    />
    <!-- 相册首页（设计稿 六·主界面：导航+网格+详情+状态栏） -->
    <div v-show="viewMode === 'library'" class="sub-page">
      <LibraryView
        :visible="viewMode === 'library'"
        @open-image="openFromLibrary"
        @edit-image="openEditor"
        @collage="openCollage"
        @print="openPrintPaths"
      />
    </div>
    <!-- 单图浏览 -->
    <div v-show="viewMode === 'viewer'" class="viewer-body">
      <div class="viewer-main">
        <!-- 图片主区 + 顶部浮动工具栏（沉浸模式：3s 无操作淡出，鼠标顶部唤起） -->
        <!-- 真 HDR 嵌入：原生 scRGB 画布作为透明覆盖层叠加在 SDR <img> 之上——
             进入时画布盖住 img（同位置内容"点亮"为 HDR），退出时销毁后 img 直接
             露出：SDR ↔ HDR 全程零黑块、零布局替换（沉浸式切换） -->
        <div class="viewer-stage" @mousemove="revealToolbar">
          <ImageViewer
            ref="viewerRef"
            :src="imageUrl"
            :crossfade="crossfade"
            :fit-inset-top="56"
            @scale-change="onScaleChange"
            @fullscreen-change="onFullscreenChange"
            @nav="onNav"
            @contextmenu="openCtxMenu"
          />
          <!-- HDR 画布层：透明覆盖层（无背景色，SDR img 常驻其下零黑块）；
               画布矩形从工具栏下方 56px 起（工具栏留在 WebView2 可见可点） -->
          <div v-if="hdrEmbedded" class="hdr-canvas-layer">
            <div ref="hdrSlotRef" class="hdr-slot" />
          </div>
          <div class="toolbar-float" :class="{ hidden: !toolbarVisible && !hdrEmbedded }">
            <Toolbar
              :scale="scale"
              :can-prev="canPrev"
              :can-next="canNext"
              :fullscreen="fullscreen"
              :exif-active="showExif"
              :slideshow="slideshow"
              :tone-active="tonePanelVisible"
              :upscale-active="upscalePanelVisible"
              :is-hdr="isHdrSource"
              :is-animation="isAnimation"
              :hdr-available="hdrAvailable"
              :hdr-embedded="hdrEmbedded"
              :hdr-any="hdrDisplayOn"
              :itm-busy="itmBusy"
              :itm-embedded="itmEmbedded"
              :itm-strength="itmStrength"
              :hdr-nits="Math.round(hdrInfo?.max_luminance ?? 0)"
              :hdr-gamut="hdrInfo?.gamut_name ?? ''"
              :hdr-p3-coverage="hdrInfo?.p3_coverage ?? 0"
              @open="handleOpen"
              @open-folder="handleOpenFolder"
              @prev="goPrev"
              @next="goNext"
              @zoom-in="viewerRef?.zoomIn()"
              @zoom-out="viewerRef?.zoomOut()"
              @fit="viewerRef?.fitView()"
              @actual-size="viewerRef?.actualView()"
              @rotate-left="viewerRef?.rotateLeft()"
              @rotate-right="viewerRef?.rotateRight()"
              @edit="openEditor(currentPath)"
              @tone="tonePanelVisible = !tonePanelVisible"
              @upscale="upscalePanelVisible = !upscalePanelVisible"
              @hdr-view="openHdrView"
              @hdr-itm="openItmView"
              @itm-strength="onItmStrength"
              @play-animation="openAnimationPlayer"
              @fullscreen="viewerRef?.toggleFullscreen()"
              @toggle-exif="showExif = !showExif"
              @save="handleSave"
              @copy="handleCopy"
              @print="openPrint"
              @wallpaper="handleWallpaper"
              @slideshow="toggleSlideshow"
              @recycle="openRecycle"
            />
          </div>
          <!-- HDR 亮度调节面板（右侧浮动；仅 HDR 源显示） -->
          <TonePanel
            v-if="tonePanelVisible && isHdrSource"
            class="tone-panel-float"
            @rerender="onToneRerender"
            @history="openHistory"
            @close="tonePanelVisible = false"
          />
          <!-- AI 放大面板（右侧浮动；任意图片可用） -->
          <UpscalePanel
            v-if="upscalePanelVisible && currentPath"
            class="tone-panel-float"
            :path="currentPath"
            :width="imageInfo?.width ?? 0"
            :height="imageInfo?.height ?? 0"
            @close="upscalePanelVisible = false"
          />
          <!-- 调参历史面板（TonePanel「历史」弹出；参数 sidecar 跟随图片） -->
          <HistoryCompare
            v-if="historyVisible && isHdrSource && currentPath"
            class="tone-panel-float history-panel-float"
            :path="currentPath"
            @apply="onHistoryApply"
            @close="historyVisible = false"
          />
          <!-- 底部状态浮签（HDR 嵌入时被原生画布盖住 → 隐藏，信息并入工具栏徽标） -->
          <div
            v-if="imageInfo && !hdrEmbedded"
            class="status-chip"
            :class="{ hidden: !toolbarVisible }"
          >
            <template v-if="isMultiPagePdf">第 {{ pdfPage }} / {{ pdfPageCount }} 页 · </template>
            {{ Math.max(currentIndex + 1, 1) }} / {{ imageList.length || 1 }} ·
            {{ imageInfo.width }}×{{ imageInfo.height }} ·
            {{ imageInfo.format.toUpperCase() }}
          </div>
          <!-- JXL 动图 GPU 常驻填充进度（anim://fill 事件驱动；播放窗口弹出前的过渡态，弹出后收敛为 HUD 小条） -->
          <Transition name="anim-fill">
            <div
              v-if="animFillVisible"
              class="anim-fill-bar"
              :class="{ degraded: animState === 'degraded', resident: animState === 'full_resident' }"
            >
              <span class="anim-fill-text">{{ animFillText }}</span>
              <n-progress
                v-if="animFillShowBar"
                class="anim-fill-progress"
                type="line"
                :percentage="animFillPct"
                :height="4"
                :show-indicator="false"
                :processing="animTotalFrames == null"
              />
            </div>
          </Transition>
          <!-- PDF 翻页浮条（多页 PDF 专用：上一页/页码/下一页，2345 看图王同类交互） -->
          <div
            v-if="isMultiPagePdf && !hdrEmbedded"
            class="pdf-pager"
            :class="{ hidden: !toolbarVisible }"
          >
            <button class="pg-btn" :disabled="pdfPage <= 1" title="上一页（←）" @click="pdfGoto(pdfPage - 1)">
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round">
                <path d="M15 18l-6-6 6-6" />
              </svg>
            </button>
            <span class="pg-num">{{ pdfPage }} / {{ pdfPageCount }}</span>
            <button class="pg-btn" :disabled="pdfPage >= pdfPageCount" title="下一页（→）" @click="pdfGoto(pdfPage + 1)">
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round">
                <path d="M9 6l6 6-6 6" />
              </svg>
            </button>
          </div>
          <!-- 图片两侧上一张/下一张按钮（HDR 嵌入时位于原生画布区域被盖住 → 隐藏，键盘 ←/→ 仍可切换） -->
          <button
            v-if="!hdrEmbedded"
            class="nav-arrow prev"
            :class="{ hidden: !toolbarVisible }"
            :disabled="!canPrev"
            title="上一张（←）"
            @click="goPrev"
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M15 18l-6-6 6-6" />
            </svg>
          </button>
          <button
            v-if="!hdrEmbedded"
            class="nav-arrow next"
            :class="{ hidden: !toolbarVisible }"
            :disabled="!canNext"
            title="下一张（→）"
            @click="goNext"
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M9 6l6 6-6 6" />
            </svg>
          </button>
          <!-- 右侧「已看图片」按钮 + 展开面板（HDR 嵌入时被盖住 → 隐藏） -->
          <div v-if="!hdrEmbedded" class="visited-wrap">
            <button
              class="visited-btn"
              :class="{ active: showVisited }"
              :title="`已看图片（${visitedItems.length} 张）`"
              @click="toggleVisitedPanel"
            >
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
                <path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7-10-7-10-7z" />
                <circle cx="12" cy="12" r="3" />
              </svg>
              <span class="visited-count">{{ visitedItems.length }}</span>
            </button>
            <transition name="visited-pop">
              <div v-if="showVisited" class="visited-panel">
                <div class="visited-header">
                  <span>已看 {{ visitedItems.length }} 张</span>
                  <button
                    v-if="visitedItems.length > 0"
                    class="visited-clear-all"
                    title="清空全部已看记录"
                    @click="clearVisitedAll"
                  >
                    清空
                  </button>
                </div>
                <div v-if="visitedItems.length === 0" class="visited-empty">
                  还没有看过其他图片
                </div>
                <div v-else class="visited-grid">
                  <div
                    v-for="it in visitedItems"
                    :key="it.path"
                    class="visited-item"
                    :class="{ current: it.path === currentPath }"
                    :title="it.name"
                    @click="handleSelect(it.path)"
                  >
                    <span
                      class="visited-remove"
                      title="清除这条已看记录"
                      @click.stop="clearVisitedOne(it.path)"
                    >
                      ×
                    </span>
                    <img
                      v-if="visitedThumbs[it.path]"
                      :src="visitedThumbs[it.path]"
                      draggable="false"
                    />
                    <span v-else class="visited-ph">…</span>
                    <span class="visited-name">{{ it.name }}</span>
                  </div>
                </div>
              </div>
            </transition>
          </div>
        </div>
        <!-- 底部缩略图栏（当前目录全部图片；已打开的带主题色角标） -->
        <div class="viewer-thumbs">
          <ThumbnailBar
            :items="imageList"
            :current="currentPath"
            :visited="visitedPaths"
            @select="handleSelect"
          />
        </div>
      </div>
      <!-- 右侧 EXIF 抽屉（覆盖式滑出） -->
      <ExifPanel :exif="exifData" :visible="showExif" @close="showExif = false" />
    </div>
    <!-- 滤镜编辑页（设计稿 六·图片编辑页） -->
    <div v-if="viewMode === 'editor'" class="sub-page">
      <EditorView :path="editorPath" @close="closeSubPage" @saved="onSubSaved" />
    </div>
    <!-- 拼图编辑页（设计稿 六·拼图编辑器页） -->
    <div v-if="viewMode === 'collage'" class="sub-page">
      <CollageView :paths="collagePaths" @close="closeSubPage" @saved="onSubSaved" />
    </div>

    <!-- 回收站弹窗（设计稿 3.3：7 天可恢复） -->
    <n-modal
      v-model:show="showRecycle"
      preset="card"
      title="回收站"
      :bordered="false"
      style="max-width: 560px"
    >
      <div class="recycle-list">
        <div v-if="recycleList.length === 0" class="recycle-empty">
          回收站是空的，删掉的图片会先在这里放 7 天
        </div>
        <div v-for="e in recycleList" :key="e.recycled_name" class="recycle-item">
          <n-icon :component="Trash" size="15" class="ri-icon" />
          <div class="ri-info">
            <span class="ri-name" :title="e.original_path">{{ e.display_name }}</span>
            <span class="ri-meta" :title="e.original_path">
              {{ fmtTime(e.deleted_ts) }} · {{ e.original_path }}
            </span>
          </div>
          <button class="ri-btn restore" title="恢复到原位置" @click="restoreOne(e)">
            <n-icon :component="Rotate" size="14" />
            恢复
          </button>
          <button class="ri-btn purge" title="彻底删除" @click="purgeOneConfirm(e)">
            删除
          </button>
        </div>
      </div>
      <template #footer>
        <div class="recycle-footer">
          <span class="recycle-hint">共 {{ recycleList.length }} 项 · 7 天后自动清理</span>
          <n-button size="small" quaternary type="error" :disabled="recycleList.length === 0" @click="purgeAllConfirm">
            清空回收站
          </n-button>
        </div>
      </template>
    </n-modal>

    <!-- 右键 / 长按快捷菜单（设计稿 5.1） -->
    <n-dropdown
      placement="bottom-start"
      trigger="manual"
      :show="ctxMenu.show"
      :x="ctxMenu.x"
      :y="ctxMenu.y"
      :options="ctxOptions"
      @select="onCtxSelect"
      @clickoutside="ctxMenu.show = false"
    />

    <!-- 打印面板（图片 / PDF，批量队列） -->
    <PrintPanel :visible="showPrint" :initial-paths="printSeedPaths" @close="showPrint = false" />
  </div>
</template>

<style scoped>
.viewer-root {
  display: flex;
  flex-direction: column;
  height: 100vh;
  overflow: hidden;
  background: var(--jb-bg);
  /* GPU 空转治理：全窗口 blur 40→16（合成成本约降 60%，视觉近似） */
  backdrop-filter: blur(16px) saturate(1.2);
  position: relative;
}

/* === 极淡主题色极光晕染（柔光滤镜，不抢内容，与主面板一致） === */
.viewer-root::before {
  content: "";
  position: absolute;
  inset: 0;
  pointer-events: none;
  z-index: 0;
  background:
    radial-gradient(
      46% 38% at 14% 6%,
      color-mix(in srgb, var(--jb-primary) 11%, transparent),
      transparent 72%
    ),
    radial-gradient(
      40% 34% at 90% 98%,
      color-mix(in srgb, var(--jb-primary) 9%, transparent),
      transparent 72%
    );
  /* GPU 空转治理：停用 aurora-breathe 呼吸动画——可见窗口的 infinite 动画在
     集显办公机上持续产生合成成本；静态渐变保留（Mica 质感不依赖动画） */
}

/* === 主区：左主列 + 右侧抽屉 === */
.viewer-body {
  flex: 1;
  display: flex;
  min-height: 0;
  position: relative;
  z-index: 1;
}
/* 相册/编辑/拼图子页容器（与 viewer-body 同级占满剩余空间） */
.sub-page {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  position: relative;
  z-index: 1;
}
.viewer-main {
  flex: 1;
  display: flex;
  flex-direction: column;
  min-width: 0;
  min-height: 0;
}
/* 图片显示主区（flex:1，工具栏浮动其上） */
.viewer-stage {
  flex: 1;
  position: relative;
  min-height: 0;
}

/* === 真 HDR 沉浸覆盖层 === */
/* 透明层：SDR <img> 常驻渲染不销毁，原生画布叠其上"点亮"。
   绝不能设背景色（白膜/黑块根源：WebView2 在原生 owned 窗口之下，
   若原生画布不呈现，背景色就成了盖在 SDR 图上的半透明白膜）。 */
.hdr-canvas-layer {
  position: absolute;
  top: 0;
  left: 0;
  right: 0;
  bottom: 0;
  z-index: 1; /* 高于 img（z 0），低于工具栏/浮动面板（z 9-11） */
  pointer-events: none; /* 指针事件穿透：原生窗口自身拦截画布区输入 */
}
/* 画布插槽：从工具栏下方起（8 边距 + 44 工具栏 + 4 缝），
   该区域必须留给 WebView2 工具栏（原生窗口矩形内 WebView2 完全不可见） */
.hdr-slot {
  position: absolute;
  top: 56px;
  left: 0;
  right: 0;
  bottom: 0;
}
.toolbar-float {
  position: absolute;
  top: 8px;
  left: 50%;
  transform: translateX(-50%);
  z-index: 10;
}
/* HDR 亮度调节面板：右侧浮动（对齐 EXIF 抽屉风格） */
.tone-panel-float {
  position: absolute;
  top: 64px;
  right: 12px;
  width: 272px;
  z-index: 11;
}
/* 调参历史面板：偏右下错位（TonePanel 同位弹出避免遮挡） */
.history-panel-float {
  top: 120px;
  right: 24px;
  width: 300px;
}
/* 底部缩略图栏 80px */
.viewer-thumbs {
  height: 80px;
  flex-shrink: 0;
}

/* === 沉浸模式：工具栏 3s 无操作淡出（设计稿 5.5） === */
.toolbar-float {
  transition: opacity 220ms ease, transform 220ms ease;
}
.toolbar-float.hidden {
  opacity: 0;
  transform: translateY(-6px);
  pointer-events: none;
}

/* === 底部状态浮签（第X/N张 · 尺寸 · 格式，随沉浸模式淡出） === */
.status-chip {
  position: absolute;
  left: 14px;
  bottom: 12px;
  z-index: 9;
  padding: 5px 12px;
  border-radius: 999px;
  font-size: 11px;
  letter-spacing: 0.5px;
  color: var(--jb-text-soft);
  background: color-mix(in srgb, var(--jb-bg-card) 78%, transparent);
  backdrop-filter: blur(16px) saturate(1.3);
  border: 1px solid var(--jb-border);
  user-select: none;
  pointer-events: none;
  transition: opacity 220ms ease;
}
.status-chip.hidden {
  opacity: 0;
}

/* === JXL 动图 GPU 常驻填充进度小条（底部中央，事件驱动；复用 status-chip 视觉 token） === */
.anim-fill-bar {
  position: absolute;
  left: 50%;
  bottom: 12px;
  transform: translateX(-50%);
  z-index: 9;
  display: flex;
  flex-direction: column;
  gap: 4px;
  max-width: min(76%, 420px);
  padding: 6px 14px;
  border-radius: 999px;
  background: color-mix(in srgb, var(--jb-bg-card) 78%, transparent);
  backdrop-filter: blur(16px) saturate(1.3);
  border: 1px solid var(--jb-border);
  user-select: none;
  pointer-events: none;
}
.anim-fill-bar.degraded {
  border-color: color-mix(in srgb, #f0a020 45%, transparent);
  background: color-mix(in srgb, #f0a020 14%, var(--jb-bg-card));
}
.anim-fill-bar.resident {
  border-color: color-mix(in srgb, var(--jb-primary) 45%, transparent);
}
.anim-fill-text {
  font-size: 11px;
  letter-spacing: 0.5px;
  text-align: center;
  color: var(--jb-text);
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.anim-fill-bar.degraded .anim-fill-text {
  color: #f0a020;
}
.anim-fill-progress {
  width: 180px;
}
.anim-fill-enter-active,
.anim-fill-leave-active {
  transition:
    opacity 220ms ease,
    transform 220ms ease;
}
.anim-fill-enter-from,
.anim-fill-leave-to {
  opacity: 0;
  transform: translate(-50%, 8px);
}

/* === PDF 翻页浮条（底部中央，多页 PDF 专用） === */
.pdf-pager {
  position: absolute;
  left: 50%;
  bottom: 12px;
  transform: translateX(-50%);
  z-index: 9;
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 5px 10px;
  border-radius: 999px;
  background: color-mix(in srgb, var(--jb-bg-card) 78%, transparent);
  backdrop-filter: blur(16px) saturate(1.3);
  border: 1px solid var(--jb-border);
  user-select: none;
  transition: opacity 220ms ease;
}
.pdf-pager.hidden {
  opacity: 0;
  pointer-events: none;
}
.pg-btn {
  width: 26px;
  height: 26px;
  border: none;
  border-radius: 50%;
  background: transparent;
  color: var(--jb-text-soft);
  cursor: pointer;
  display: flex;
  align-items: center;
  justify-content: center;
  transition: color 200ms ease, background-color 200ms ease;
}
.pg-btn svg {
  width: 15px;
  height: 15px;
}
.pg-btn:hover:not(:disabled) {
  color: var(--jb-primary);
  background: color-mix(in srgb, var(--jb-primary) 12%, transparent);
}
.pg-btn:disabled {
  opacity: 0.35;
  cursor: default;
}
.pg-num {
  min-width: 56px;
  text-align: center;
  font-size: 11.5px;
  font-variant-numeric: tabular-nums;
  letter-spacing: 0.5px;
  color: var(--jb-text);
}

/* === 图片两侧上一张/下一张按钮（悬浮箭头，随沉浸模式淡出） === */
.nav-arrow {
  position: absolute;
  top: 50%;
  transform: translateY(-50%);
  width: 38px;
  height: 38px;
  display: flex;
  align-items: center;
  justify-content: center;
  border: 1px solid var(--jb-border);
  border-radius: 50%;
  background: color-mix(in srgb, var(--jb-bg-card) 82%, transparent);
  backdrop-filter: blur(16px) saturate(1.3);
  color: var(--jb-text-soft);
  cursor: pointer;
  z-index: 9;
  transition: opacity 220ms ease, color 0.15s, border-color 0.15s,
    background-color 0.15s, transform 220ms ease;
}
.nav-arrow svg {
  width: 20px;
  height: 20px;
}
.nav-arrow.prev {
  left: 14px;
}
.nav-arrow.next {
  right: 14px;
}
.nav-arrow:hover:not(:disabled) {
  color: var(--jb-primary);
  border-color: var(--jb-primary);
  background: color-mix(in srgb, var(--jb-bg-card-hover) 90%, transparent);
}
.nav-arrow:disabled {
  opacity: 0.32;
  cursor: default;
}
.nav-arrow.hidden {
  opacity: 0;
  pointer-events: none;
}
.nav-arrow.prev.hidden {
  transform: translateY(-50%) translateX(-8px);
}
.nav-arrow.next.hidden {
  transform: translateY(-50%) translateX(8px);
}

/* === 右侧「已看图片」按钮 + 展开面板 === */
/* 右侧垂直 66% 处（与 50% 处的翻页箭头错开） */
.visited-wrap {
  position: absolute;
  top: 66%;
  right: 14px;
  transform: translateY(-50%);
  z-index: 9;
  display: flex;
  flex-direction: column;
  align-items: flex-end;
}
.visited-btn {
  position: relative;
  width: 38px;
  height: 38px;
  display: flex;
  align-items: center;
  justify-content: center;
  border: 1px solid var(--jb-border);
  border-radius: 50%;
  background: color-mix(in srgb, var(--jb-bg-card) 82%, transparent);
  backdrop-filter: blur(16px) saturate(1.3);
  color: var(--jb-text-soft);
  cursor: pointer;
  transition: color 0.15s, border-color 0.15s, background-color 0.15s;
}
.visited-btn svg {
  width: 19px;
  height: 19px;
}
.visited-btn:hover,
.visited-btn.active {
  color: var(--jb-primary);
  border-color: var(--jb-primary);
}
.visited-count {
  position: absolute;
  top: -5px;
  right: -5px;
  min-width: 16px;
  height: 16px;
  padding: 0 4px;
  border-radius: 8px;
  background: var(--jb-primary);
  color: #fff;
  font-size: 10px;
  line-height: 16px;
  text-align: center;
  pointer-events: none;
}
/* 展开面板：按钮下方弹出，头部信息行 + 网格缩略图 */
.visited-panel {
  position: absolute;
  top: 46px;
  right: 0;
  width: 224px;
  max-height: 320px;
  display: flex;
  flex-direction: column;
  gap: 6px;
  padding: 10px;
  background: color-mix(in srgb, var(--jb-bg-card) 92%, transparent);
  backdrop-filter: blur(22px) saturate(1.3);
  border: 1px solid var(--jb-border);
  border-radius: 12px;
  box-shadow: var(--jb-shadow);
}
.visited-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 0 2px;
  font-size: 11px;
  color: var(--jb-text-soft);
  flex-shrink: 0;
}
.visited-clear-all {
  padding: 2px 8px;
  border: none;
  border-radius: 999px;
  background: transparent;
  color: var(--jb-text-mute);
  font-size: 11px;
  cursor: pointer;
  transition: color 0.15s, background-color 0.15s;
}
.visited-clear-all:hover {
  color: var(--jb-red);
  background: color-mix(in srgb, var(--jb-red) 10%, transparent);
}
/* 网格主体（可滚动） */
.visited-grid {
  overflow-y: auto;
  display: grid;
  grid-template-columns: repeat(3, 1fr);
  gap: 8px;
}
.visited-grid::-webkit-scrollbar {
  width: 5px;
}
.visited-grid::-webkit-scrollbar-thumb {
  background: var(--jb-scrollbar);
  border-radius: 3px;
}
.visited-empty {
  grid-column: 1 / -1;
  padding: 18px 6px;
  text-align: center;
  font-size: 11px;
  color: var(--jb-text-mute);
}
.visited-item {
  position: relative;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 4px;
  padding: 4px;
  border: 1.5px solid transparent;
  border-radius: 8px;
  cursor: pointer;
  transition: border-color 0.15s, background-color 0.15s;
}
/* 单张清除按钮：hover 时右上角浮现 */
.visited-remove {
  position: absolute;
  top: 2px;
  right: 2px;
  width: 16px;
  height: 16px;
  display: flex;
  align-items: center;
  justify-content: center;
  border-radius: 50%;
  background: color-mix(in srgb, var(--jb-text) 72%, transparent);
  color: #fff;
  font-size: 12px;
  line-height: 1;
  opacity: 0;
  pointer-events: auto;
  transition: opacity 0.15s, background-color 0.15s;
  z-index: 1;
}
.visited-item:hover .visited-remove {
  opacity: 1;
}
.visited-remove:hover {
  background: var(--jb-red);
}
.visited-item:hover {
  background: var(--jb-bg-card-hover);
}
.visited-item.current {
  border-color: var(--jb-primary);
}
.visited-item img {
  width: 56px;
  height: 56px;
  object-fit: contain;
  border-radius: 5px;
}
.visited-ph {
  width: 56px;
  height: 56px;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 11px;
  color: var(--jb-text-mute);
  background: var(--jb-bg);
  border-radius: 5px;
}
.visited-name {
  max-width: 60px;
  font-size: 10px;
  color: var(--jb-text-mute);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
/* 面板弹出过渡（220ms 淡入上浮，与全局节奏一致） */
.visited-pop-enter-active,
.visited-pop-leave-active {
  transition: opacity 220ms ease, transform 220ms ease;
}
.visited-pop-enter-from,
.visited-pop-leave-to {
  opacity: 0;
  transform: translateY(-6px);
}

/* === 回收站弹窗（设计稿 3.3） === */
.recycle-list {
  display: flex;
  flex-direction: column;
  gap: 6px;
  max-height: 46vh;
  overflow-y: auto;
  min-width: 440px;
}
.recycle-list::-webkit-scrollbar {
  width: 6px;
}
.recycle-list::-webkit-scrollbar-thumb {
  background: var(--jb-scrollbar);
  border-radius: 3px;
}
.recycle-empty {
  padding: 28px 10px;
  text-align: center;
  font-size: 12px;
  color: var(--jb-text-mute);
}
.recycle-item {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 8px 10px;
  border: 1px solid var(--jb-border);
  border-radius: 10px;
  background: var(--jb-bg-card);
}
.ri-icon {
  color: var(--jb-text-mute);
  flex-shrink: 0;
}
.ri-info {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.ri-name {
  font-size: 12px;
  color: var(--jb-text);
  font-weight: 500;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.ri-meta {
  font-size: 10px;
  color: var(--jb-text-mute);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.ri-btn {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 4px 10px;
  border: 1px solid var(--jb-border);
  border-radius: 999px;
  background: transparent;
  color: var(--jb-text-soft);
  font-size: 11px;
  cursor: pointer;
  flex-shrink: 0;
  transition: color 0.15s, border-color 0.15s;
}
.ri-btn.restore:hover {
  color: var(--jb-primary);
  border-color: var(--jb-primary);
}
.ri-btn.purge:hover {
  color: var(--jb-red);
  border-color: var(--jb-red);
}
.recycle-footer {
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.recycle-hint {
  font-size: 11px;
  color: var(--jb-text-mute);
}
</style>
