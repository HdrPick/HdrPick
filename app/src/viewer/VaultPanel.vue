<script setup lang="ts">
// 隐私相册页（设计稿 六·隐私相册）：在 LibraryView 内容区原位渲染，无 props，根节点 height:100%
// 状态机（vault_status）：未初始化 → 创建向导；已锁定 → 解锁卡片；已解锁 → 工具条 + 加密缩略图墙
// 数据流：vault_setup/unlock/lock/change_password 管理密码态；
//   vault_add 加密移入 → vault_list 列条目（thumb 为 base64 PNG，可能为空串）；
//   vault_open(id) 解出临时文件路径 → 派发 window 事件 "jietu:vault-open"（父级接线打开看图，组件只派发）
// 自动锁定：解锁态监听 window pointerdown/keydown（8s 节流）重置 5 分钟计时器，到期 vault_lock
// 后端命令未就绪时 try/catch + useMessage/useDialog 容错；全中文 UI
import { ref, watch, onMounted, onUnmounted } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
import {
  useMessage, useDialog, NButton, NIcon, NInput, NModal, NSpin, NEmpty,
} from "naive-ui";
import {
  Lock, LockOpen, Key, Photo, Rotate, Trash, ShieldLock,
} from "@vicons/tabler";

const message = useMessage();
const dialog = useDialog();

// === 后端数据结构（与 Rust 端 serde 命名对齐） ===
interface VaultStatus {
  initialized: boolean;
  unlocked: boolean;
  count: number;
}
interface VaultItem {
  id: number;
  name: string;
  width: number;
  height: number;
  added_ts: number;
  /** base64 PNG 缩略图，可能为空串（占位块显示扩展名） */
  thumb: string;
}

// === 状态 ===
const status = ref<VaultStatus | null>(null);
const items = ref<VaultItem[]>([]);
const loading = ref(false);

// 添加图片对话框过滤（与 ViewerWindow 同清单）
const IMAGE_FILTERS = [
  {
    name: "图片",
    extensions: [
      "png", "jpg", "jpeg", "gif", "bmp", "webp", "avif",
      "tif", "tiff", "ico", "psd", "jxl",
    ],
  },
];

// ==================== 状态刷新（所有状态切换后都走这里） ====================
async function refreshStatus() {
  loading.value = true;
  try {
    status.value = await invoke<VaultStatus>("vault_status");
  } catch (e) {
    status.value = null;
    message.error(`读取隐私相册状态失败: ${e}`, { duration: 2200 });
  } finally {
    loading.value = false;
  }
  if (status.value?.unlocked) refreshList();
  else items.value = [];
}

async function refreshList() {
  try {
    items.value = await invoke<VaultItem[]>("vault_list");
  } catch (e) {
    items.value = [];
    message.error(`读取加密图片列表失败: ${e}`, { duration: 2200 });
  }
}

onMounted(refreshStatus);

// ==================== ① 未初始化：创建隐私相册 ====================
const setupPw = ref("");
const setupPw2 = ref("");
const setting = ref(false);

function submitSetup() {
  const pw = setupPw.value;
  if (!pw) {
    message.info("请输入密码", { duration: 1800 });
    return;
  }
  if (pw !== setupPw2.value) {
    message.error("两次输入的密码不一致", { duration: 1800 });
    return;
  }
  dialog.warning({
    title: "创建隐私相册",
    content: "密码丢失将无法恢复，确定使用该密码创建吗？",
    positiveText: "创建",
    negativeText: "取消",
    onPositiveClick: async () => {
      setting.value = true;
      try {
        await invoke("vault_setup", { password: pw });
        message.success("隐私相册已创建", { duration: 2200 });
        setupPw.value = "";
        setupPw2.value = "";
        await refreshStatus();
      } catch (e) {
        message.error(`创建失败: ${e}`, { duration: 2200 });
      } finally {
        setting.value = false;
      }
    },
  });
}

// ==================== ② 已锁定：解锁 ====================
const unlockPw = ref("");
const unlocking = ref(false);

async function submitUnlock() {
  if (!unlockPw.value || unlocking.value) return;
  unlocking.value = true;
  try {
    await invoke("vault_unlock", { password: unlockPw.value });
    unlockPw.value = "";
    await refreshStatus(); // 成功：刷新状态并拉取列表
  } catch {
    message.error("密码错误", { duration: 2200 });
  } finally {
    unlocking.value = false;
  }
}

// ==================== ③ 已解锁：工具条操作 ====================

/** 添加图片：plugin-dialog 多选 → vault_add 加密移入 */
async function addImages() {
  try {
    const sel = await openFileDialog({ multiple: true, filters: IMAGE_FILTERS });
    const list = Array.isArray(sel) ? sel : typeof sel === "string" ? [sel] : [];
    if (list.length === 0) return;
    const n = await invoke<number>("vault_add", { paths: list });
    message.success(`已加密移入 ${n} 张`, { duration: 2200 });
    await refreshStatus();
  } catch (e) {
    message.error(`加入隐私相册失败: ${e}`, { duration: 2200 });
  }
}

/** 手动锁定（清理解锁临时文件由后端负责） */
async function lockNow() {
  try {
    await invoke("vault_lock");
    message.info("已锁定", { duration: 1800 });
  } catch (e) {
    message.error(`锁定失败: ${e}`, { duration: 2200 });
  }
  await refreshStatus();
}

/** 修改密码小浮层 */
const showChange = ref(false);
const chOld = ref("");
const chNew = ref("");
const chNew2 = ref("");
const changing = ref(false);

async function submitChange() {
  if (!chOld.value || !chNew.value) {
    message.info("请填写完整", { duration: 1800 });
    return;
  }
  if (chNew.value !== chNew2.value) {
    message.error("两次输入的新密码不一致", { duration: 1800 });
    return;
  }
  changing.value = true;
  try {
    await invoke("vault_change_password", {
      oldPw: chOld.value,
      newPw: chNew.value,
    });
    message.success("密码已修改", { duration: 2200 });
    showChange.value = false;
    chOld.value = "";
    chNew.value = "";
    chNew2.value = "";
  } catch (e) {
    message.error(`修改失败: ${e}`, { duration: 2200 });
  } finally {
    changing.value = false;
  }
}

// ==================== 缩略图墙操作 ====================

/** 双击查看：vault_open 解出临时路径 → 派发事件（父级监听接线打开看图，组件只派发） */
async function openVault(it: VaultItem) {
  try {
    const tmp = await invoke<string>("vault_open", { id: it.id });
    if (tmp) {
      window.dispatchEvent(new CustomEvent("jietu:vault-open", { detail: tmp }));
    }
  } catch (e) {
    message.error(`打开失败: ${e}`, { duration: 2200 });
  }
}

/** 恢复到原位置 */
async function restoreOne(it: VaultItem) {
  try {
    await invoke("vault_restore", { id: it.id });
    message.success(`已恢复原位置：${it.name}`, { duration: 2200 });
    await refreshStatus();
  } catch (e) {
    message.error(`恢复失败: ${e}`, { duration: 2200 });
  }
}

/** 移除（危险操作，二次确认后永久删除） */
function removeOne(it: VaultItem) {
  dialog.warning({
    title: "移除图片",
    content: `「${it.name}」将从隐私相册永久删除，无法恢复。`,
    positiveText: "永久移除",
    negativeText: "取消",
    onPositiveClick: async () => {
      try {
        await invoke("vault_remove", { id: it.id });
        message.success("已移除", { duration: 2200 });
        await refreshStatus();
      } catch (e) {
        message.error(`移除失败: ${e}`, { duration: 2200 });
      }
    },
  });
}

// ==================== 工具 ====================

/** thumb base64 → data URL（兼容裸 base64 / 完整 data URL；空串返回 ""） */
function thumbUrl(it: VaultItem): string {
  const t = it.thumb;
  if (!t) return "";
  return t.startsWith("data:") ? t : `data:image/png;base64,${t}`;
}

/** 扩展名大写标签（占位块用） */
function extOf(name: string): string {
  return (name.split(".").pop() || "?").toUpperCase().slice(0, 5);
}

/** 秒/毫秒自适应时间格式化 */
function fmtTime(ts: number): string {
  const d = new Date(ts > 1e12 ? ts : ts * 1000);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

// ==================== 自动锁定（5 分钟无操作） ====================
const AUTO_LOCK_MS = 5 * 60 * 1000;
/** 活动节流间隔：8 秒内重复触发不重置计时器（避免高频 pointerdown 反复 clearTimeout） */
const THROTTLE_MS = 8000;
let lockTimer: number | null = null;
let lastActivity = 0;

function armAutoLock() {
  if (lockTimer) window.clearTimeout(lockTimer);
  lockTimer = window.setTimeout(doAutoLock, AUTO_LOCK_MS);
}

function onUserActivity() {
  const now = Date.now();
  if (now - lastActivity < THROTTLE_MS) return; // 节流
  lastActivity = now;
  armAutoLock();
}

async function doAutoLock() {
  if (!status.value?.unlocked) return;
  try {
    await invoke("vault_lock");
    message.info("长时间无操作，隐私相册已自动锁定", { duration: 2200 });
  } catch {
    // 后端未就绪容错：忽略，下次状态刷新自然回到锁定 UI
  }
  await refreshStatus(); // 回到锁定 UI
}

function stopAutoLock() {
  window.removeEventListener("pointerdown", onUserActivity, true);
  window.removeEventListener("keydown", onUserActivity, true);
  if (lockTimer) window.clearTimeout(lockTimer);
  lockTimer = null;
}

// 解锁态装卸监听 + 起停计时器
watch(
  () => status.value?.unlocked ?? false,
  (unlocked) => {
    if (unlocked) {
      lastActivity = Date.now();
      window.addEventListener("pointerdown", onUserActivity, true);
      window.addEventListener("keydown", onUserActivity, true);
      armAutoLock();
    } else {
      stopAutoLock();
    }
  },
);

onUnmounted(stopAutoLock);
</script>

<template>
  <div class="vault-root">
    <!-- 读取中 -->
    <div v-if="loading && !status" class="vault-center">
      <n-spin :size="18" />
      <span class="center-tip">正在读取…</span>
    </div>

    <!-- ① 未初始化：创建隐私相册 -->
    <div v-else-if="status && !status.initialized" class="vault-center">
      <div class="vault-card">
        <div class="vault-badge">
          <n-icon :component="ShieldLock" :size="22" />
        </div>
        <span class="vault-title">创建隐私相册</span>
        <span class="vault-sub">
          设置一个密码，之后加入的图片将以 AES-256-GCM 加密存储于本机
        </span>
        <n-input
          v-model:value="setupPw"
          type="password"
          show-password-on="click"
          placeholder="密码"
          @keydown.enter="submitSetup"
        />
        <n-input
          v-model:value="setupPw2"
          type="password"
          show-password-on="click"
          placeholder="确认密码"
          @keydown.enter="submitSetup"
        />
        <div class="vault-warn">密码丢失将无法恢复，请务必牢记</div>
        <n-button
          type="primary"
          block
          :loading="setting"
          @click="submitSetup"
        >
          <template #icon>
            <n-icon :component="ShieldLock" size="15" />
          </template>
          创建隐私相册
        </n-button>
      </div>
    </div>

    <!-- ② 已锁定：解锁 -->
    <div v-else-if="status && !status.unlocked" class="vault-center">
      <div class="vault-card">
        <div class="vault-badge">
          <n-icon :component="Lock" :size="22" />
        </div>
        <span class="vault-title">解锁隐私相册</span>
        <span class="vault-sub">输入密码查看加密存储的图片</span>
        <n-input
          v-model:value="unlockPw"
          type="password"
          show-password-on="click"
          placeholder="密码"
          @keydown.enter="submitUnlock"
        />
        <n-button
          type="primary"
          block
          :loading="unlocking"
          @click="submitUnlock"
        >
          <template #icon>
            <n-icon :component="LockOpen" size="15" />
          </template>
          解锁
        </n-button>
      </div>
    </div>

    <!-- ③ 已解锁：工具条 + 缩略图墙 + 底部提示条 -->
    <template v-else>
      <header class="vault-toolbar">
        <n-icon :component="ShieldLock" :size="15" class="vt-icon" />
        <span class="vt-title">隐私相册</span>
        <span class="vt-count">{{ items.length }} 张</span>
        <div class="vt-spring" />
        <button class="vbtn primary" title="选择图片加密移入" @click="addImages">
          <n-icon :component="Photo" size="14" />
          <span>添加图片</span>
        </button>
        <button class="vbtn" title="修改相册密码" @click="showChange = true">
          <n-icon :component="Key" size="14" />
          <span>修改密码</span>
        </button>
        <button class="vbtn" title="立即锁定并清理解锁临时文件" @click="lockNow">
          <n-icon :component="Lock" size="14" />
          <span>锁定</span>
        </button>
      </header>

      <div class="vault-grid">
        <n-empty
          v-if="items.length === 0"
          class="vault-empty"
          size="large"
          description="还没有图片，点右上角「添加图片」加密移入"
        />
        <div
          v-for="it in items"
          :key="it.id"
          class="vcard"
          :title="`${it.name}（双击查看）`"
          @dblclick="openVault(it)"
        >
          <div class="vcard-thumb">
            <img v-if="thumbUrl(it)" :src="thumbUrl(it)" draggable="false" />
            <span v-else class="ph">{{ extOf(it.name) }}</span>
          </div>
          <div class="vcard-info">
            <span class="vcard-name ellip">{{ it.name }}</span>
            <span class="vcard-meta">
              {{ it.width }}×{{ it.height }} · {{ fmtTime(it.added_ts) }}
            </span>
          </div>
          <div class="vcard-ops">
            <button
              class="op"
              title="恢复到原位置"
              @click.stop="restoreOne(it)"
            >
              <n-icon :component="Rotate" size="13" />
              <span>恢复</span>
            </button>
            <button
              class="op danger"
              title="从隐私相册永久移除"
              @click.stop="removeOne(it)"
            >
              <n-icon :component="Trash" size="13" />
              <span>移除</span>
            </button>
          </div>
        </div>
      </div>

      <footer class="vault-foot">
        <n-icon :component="ShieldLock" :size="13" class="vf-icon" />
        <span>
          图片经 AES-256-GCM 加密存储于本机，解锁查看的临时文件将在锁定时清除；5
          分钟无操作自动锁定
        </span>
      </footer>
    </template>

    <!-- 修改密码小浮层 -->
    <n-modal
      v-model:show="showChange"
      preset="card"
      title="修改密码"
      :bordered="false"
      style="width: 380px; max-width: 92vw"
    >
      <div class="ch-form">
        <n-input
          v-model:value="chOld"
          type="password"
          show-password-on="click"
          placeholder="旧密码"
        />
        <n-input
          v-model:value="chNew"
          type="password"
          show-password-on="click"
          placeholder="新密码"
        />
        <n-input
          v-model:value="chNew2"
          type="password"
          show-password-on="click"
          placeholder="确认新密码"
          @keydown.enter="submitChange"
        />
        <n-button
          type="primary"
          block
          :loading="changing"
          @click="submitChange"
        >
          确认修改
        </n-button>
      </div>
    </n-modal>
  </div>
</template>

<style scoped>
/* ==================== 根布局：工具条 + 滚动缩略图墙 + 底部提示条 ==================== */
.vault-root {
  height: 100%;
  display: flex;
  flex-direction: column;
  min-height: 0;
  overflow: hidden;
  background: var(--jb-bg);
  color: var(--jb-text);
  font-size: 12px;
}
.ellip {
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

/* ==================== 居中态（创建 / 解锁 / 读取中） ==================== */
.vault-center {
  flex: 1;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 10px;
  user-select: none;
}
.center-tip {
  font-size: 12px;
  color: var(--jb-text-mute);
}
.vault-card {
  width: 380px;
  max-width: calc(100% - 32px);
  padding: 28px 26px;
  display: flex;
  flex-direction: column;
  gap: 12px;
  border: 1px solid var(--jb-border);
  border-radius: 12px;
  background: var(--jb-bg-card);
  box-shadow: var(--jb-shadow);
}
.vault-badge {
  width: 48px;
  height: 48px;
  border-radius: 50%;
  display: flex;
  align-items: center;
  justify-content: center;
  align-self: center;
  color: var(--jb-primary);
  background: color-mix(in srgb, var(--jb-primary) 12%, transparent);
  border: 1px solid color-mix(in srgb, var(--jb-primary) 28%, transparent);
}
.vault-title {
  text-align: center;
  font-size: 14px;
  font-weight: 600;
  color: var(--jb-text);
  letter-spacing: 1px;
}
.vault-sub {
  text-align: center;
  font-size: 11px;
  line-height: 1.7;
  color: var(--jb-text-mute);
}
.vault-warn {
  padding: 8px 12px;
  border-radius: 8px;
  font-size: 11px;
  line-height: 1.6;
  color: var(--jb-red);
  background: color-mix(in srgb, var(--jb-red) 8%, transparent);
  border: 1px solid color-mix(in srgb, var(--jb-red) 24%, transparent);
  text-align: center;
}

/* ==================== 工具条 ==================== */
.vault-toolbar {
  flex-shrink: 0;
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 10px 14px;
  border-bottom: 1px solid var(--jb-border);
  background: color-mix(in srgb, var(--jb-bg-card) 88%, transparent);
}
.vt-icon {
  color: var(--jb-primary);
  flex-shrink: 0;
}
.vt-title {
  font-size: 13px;
  font-weight: 600;
  color: var(--jb-text);
}
.vt-count {
  font-size: 11px;
  color: var(--jb-text-mute);
}
.vt-spring {
  flex: 1;
}
.vbtn {
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
.vbtn:hover:not(:disabled) {
  color: var(--jb-text);
  border-color: var(--jb-primary);
  transform: scale(1.03);
}
.vbtn:disabled {
  opacity: 0.5;
  cursor: default;
}
.vbtn.primary {
  color: var(--jb-primary);
  border-color: color-mix(in srgb, var(--jb-primary) 45%, transparent);
}

/* ==================== 缩略图墙 ==================== */
.vault-grid {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: 14px 16px 20px;
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(150px, 1fr));
  gap: 14px;
  align-content: start;
  scrollbar-width: thin;
  scrollbar-color: var(--jb-scrollbar) transparent;
}
.vault-grid::-webkit-scrollbar {
  width: 6px;
}
.vault-grid::-webkit-scrollbar-thumb {
  background: var(--jb-scrollbar);
  border-radius: 3px;
}
.vault-empty {
  grid-column: 1 / -1;
  margin-top: 56px;
}
.vcard {
  display: flex;
  flex-direction: column;
  border: 1px solid var(--jb-border);
  border-radius: 12px;
  background: var(--jb-bg-card);
  overflow: hidden;
  cursor: pointer;
  user-select: none;
  transition: transform 200ms ease, box-shadow 200ms ease,
    border-color 200ms ease;
}
.vcard:hover {
  transform: scale(1.03); /* hover 轻微放大（不超过 1.03） */
  box-shadow: var(--jb-shadow);
  z-index: 1;
}
.vcard-thumb {
  aspect-ratio: 4 / 3;
  display: flex;
  align-items: center;
  justify-content: center;
  background: var(--jb-bg);
  border-bottom: 1px solid var(--jb-divider);
  overflow: hidden;
}
.vcard-thumb img {
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
.vcard-info {
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding: 8px 10px 4px;
  min-width: 0;
}
.vcard-name {
  font-size: 11.5px;
  font-weight: 500;
  color: var(--jb-text);
}
.vcard-meta {
  font-size: 10px;
  color: var(--jb-text-mute);
}
.vcard-ops {
  display: flex;
  gap: 6px;
  padding: 6px 10px 10px;
}
.op {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 3px 10px;
  border: 1px solid var(--jb-border);
  border-radius: 999px;
  background: transparent;
  color: var(--jb-text-soft);
  font-size: 10.5px;
  cursor: pointer;
  transition: color 200ms ease, border-color 200ms ease;
}
.op:hover {
  color: var(--jb-primary);
  border-color: var(--jb-primary);
}
.op.danger:hover {
  color: var(--jb-red);
  border-color: var(--jb-red);
}

/* ==================== 底部提示条 ==================== */
.vault-foot {
  flex-shrink: 0;
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 14px;
  border-top: 1px solid var(--jb-border);
  background: var(--jb-bg-card);
  font-size: 11px;
  color: var(--jb-text-mute);
  user-select: none;
}
.vf-icon {
  flex-shrink: 0;
  color: var(--jb-primary);
}

/* ==================== 修改密码小浮层 ==================== */
.ch-form {
  display: flex;
  flex-direction: column;
  gap: 12px;
}
</style>
