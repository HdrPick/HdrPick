<script setup lang="ts">
// 通用标题栏：置顶 / HDR 显示器面板 / 主题切换 / Win11 窗口按钮
// 主面板与设置中心共用（沉浸式切换时标题栏保持稳定）
import { ref } from "vue";
import {
  NIcon,
  NPopover,
  NPopselect,
  NSwitch,
  NSlider,
  NTag,
  useMessage,
} from "naive-ui";
import { Camera, Contrast, Moon, Sun, Pin, ChevronLeft, Flare } from "@vicons/tabler";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import type { ThemeMode } from "./useTheme";

const props = defineProps<{
  mode: ThemeMode;
  isDark: boolean;
  setMode: (m: ThemeMode) => void;
  /** 左侧返回按钮（设置中心用） */
  showBack?: boolean;
  /** 标题文字（默认 jietu-hdr） */
  title?: string;
}>();

const emit = defineEmits<{
  (e: "back"): void;
}>();

const message = useMessage();

// ====== 窗口控制（Win11 原生按钮） ======
async function minimize() {
  await getCurrentWindow().minimize();
}
async function toggleMaximize() {
  await getCurrentWindow().toggleMaximize();
}
async function close() {
  // minimize_to_tray=true 时 Rust 端会拦截 close → hide
  await getCurrentWindow().close();
}

// ====== 窗口置顶 ======
const pinned = ref(false);
async function togglePin() {
  pinned.value = !pinned.value;
  try {
    await getCurrentWindow().setAlwaysOnTop(pinned.value);
  } catch (e) {
    pinned.value = !pinned.value;
    message.error("置顶切换失败: " + e, { duration: 2200 });
  }
}

// ====== 显示器 HDR 开关 + 屏幕亮度 ======
interface HdrMonitorState {
  device_name: string;
  friendly_name: string;
  hdr_supported: boolean;
  hdr_enabled: boolean;
  sdr_white_nits: number;
}
interface MonitorBrightness {
  device_name: string;
  is_primary: boolean;
  current: number;
  max: number;
  supported: boolean;
}
interface MonitorPanelItem {
  device_name: string;
  friendly_name: string;
  is_primary: boolean;
  hdr_supported: boolean;
  hdr_enabled: boolean;
  brightness_supported: boolean;
  brightness: number;
  /** HDR 内容亮度（SDR 白电平 nits；HDR 关闭时为 0 = 不显示滑块） */
  sdrWhiteNits: number;
}
interface PanelGamut {
  r: [number, number];
  g: [number, number];
  b: [number, number];
  white: [number, number];
  max_luminance: number;
  name: string;
  p3_coverage: number;
  bt2020_coverage: number;
}
const monitorPanel = ref<MonitorPanelItem[]>([]);
const panelGamut = ref<PanelGamut | null>(null);
const hdrSwitching = ref(false);
const showMonitorPanel = ref(false);

async function loadMonitorPanel() {
  try {
    const [hdr, bri, gamut] = await Promise.all([
      invoke<HdrMonitorState[]>("get_hdr_status"),
      invoke<MonitorBrightness[]>("get_brightness_status"),
      invoke<PanelGamut | null>("panel_gamut"),
    ]);
    panelGamut.value = gamut;
    monitorPanel.value = hdr.map((h) => {
      const b = bri.find((x) => x.device_name === h.device_name);
      return {
        device_name: h.device_name,
        friendly_name: h.friendly_name,
        is_primary: b?.is_primary ?? false,
        hdr_supported: h.hdr_supported,
        hdr_enabled: h.hdr_enabled,
        brightness_supported: b?.supported ?? false,
        brightness: b?.current ?? 0,
        sdrWhiteNits: Math.round((h.sdr_white_nits ?? 0) / 4) * 4,
      };
    });
  } catch (e) {
    console.warn("查询显示器状态失败:", e);
  }
}

function onMonitorPanelShow(show: boolean) {
  showMonitorPanel.value = show;
  if (show) loadMonitorPanel();
}

async function toggleHdr(m: MonitorPanelItem, enable: boolean) {
  if (hdrSwitching.value) return;
  hdrSwitching.value = true;
  try {
    await invoke("set_hdr_state", { deviceName: m.device_name, enable });
    message.success(
      `${m.friendly_name} HDR 已${enable ? "开启" : "关闭"}（屏幕短暂闪烁属正常）`,
      { duration: 2200 },
    );
    setTimeout(loadMonitorPanel, 1500);
  } catch (e) {
    message.error(`HDR 切换失败: ${e}`, { duration: 2200 });
    loadMonitorPanel();
  } finally {
    hdrSwitching.value = false;
  }
}

// 亮度调节（防抖：停止拖动后发送）
let brightnessTimer: number | null = null;
async function onBrightnessChange(m: MonitorPanelItem, v: number) {
  m.brightness = v;
  if (brightnessTimer) window.clearTimeout(brightnessTimer);
  brightnessTimer = window.setTimeout(async () => {
    try {
      await invoke("set_brightness", { deviceName: m.device_name, level: v });
    } catch (e) {
      message.error(`亮度调节失败: ${e}`, { duration: 2200 });
      loadMonitorPanel();
    }
  }, 250);
}

// HDR 内容亮度（SDR 白电平，防抖：停止拖动后发送）
let sdrWhiteTimer: number | null = null;
async function onSdrWhiteChange(m: MonitorPanelItem, v: number) {
  m.sdrWhiteNits = v;
  if (sdrWhiteTimer) window.clearTimeout(sdrWhiteTimer);
  sdrWhiteTimer = window.setTimeout(async () => {
    try {
      await invoke("set_sdr_white_level", {
        deviceName: m.device_name,
        nits: v,
      });
    } catch (e) {
      message.error(`HDR 内容亮度调节失败: ${e}`, { duration: 2200 });
      loadMonitorPanel();
    }
  }, 250);
}

// ====== 主题切换 ======
const themeOptions = [
  { label: "跟随系统", value: "follow" },
  { label: "浅色", value: "light" },
  { label: "深色", value: "dark" },
];
</script>

<template>
  <div class="titlebar" data-tauri-drag-region>
    <div class="title-left" data-tauri-drag-region>
      <button
        v-if="showBack"
        class="win-btn icon-btn back-btn"
        title="返回主面板"
        @click="emit('back')"
      >
        <n-icon :component="ChevronLeft" size="16" />
      </button>
      <n-icon :component="Camera" size="14" color="var(--jb-primary)" />
      <span class="title-text">{{ title ?? "jietu-hdr" }}</span>
    </div>
    <div class="title-actions">
      <!-- 窗口置顶 -->
      <button
        class="win-btn icon-btn"
        :class="{ active: pinned }"
        :title="pinned ? '取消置顶' : '窗口置顶'"
        @click="togglePin"
      >
        <n-icon :component="Pin" size="14" />
      </button>
      <!-- 显示器 HDR / 亮度 -->
      <n-popover
        trigger="click"
        placement="bottom-end"
        :show-arrow="false"
        style="padding: 12px"
        @update:show="onMonitorPanelShow"
      >
        <template #trigger>
          <button class="win-btn icon-btn" title="HDR 与屏幕亮度">
            <n-icon
              :component="Contrast"
              size="14"
              :color="monitorPanel.some((m) => m.hdr_enabled) ? 'var(--jb-primary)' : undefined"
            />
          </button>
        </template>
        <div class="monitor-panel">
          <div v-for="m in monitorPanel" :key="m.device_name" class="monitor-item">
            <div class="monitor-head">
              <span class="monitor-name">
                {{ m.friendly_name }}
                <n-tag v-if="m.is_primary" size="tiny" :bordered="false" type="primary">
                  主屏
                </n-tag>
              </span>
              <div class="hdr-switch">
                <span class="hdr-label">HDR</span>
                <n-switch
                  size="small"
                  :value="m.hdr_enabled"
                  :disabled="!m.hdr_supported || hdrSwitching"
                  @update:value="(v: boolean) => toggleHdr(m, v)"
                />
              </div>
            </div>
            <!-- HDR 内容亮度（Windows 11「SDR 内容亮度」同款，仅 HDR 开启时显示） -->
            <div v-if="m.hdr_enabled" class="monitor-bri-row" title="HDR 开启时 SDR 内容（桌面应用）的显示亮度">
              <n-icon :component="Flare" size="13" class="bri-icon" />
              <n-slider
                :value="m.sdrWhiteNits"
                :min="80"
                :max="480"
                :step="4"
                class="bri-slider"
                @update:value="(v: number) => onSdrWhiteChange(m, v)"
              />
              <span class="bri-value">{{ m.sdrWhiteNits }} nits</span>
            </div>
            <div class="monitor-bri-row">
              <n-icon :component="Sun" size="13" class="bri-icon" />
              <n-slider
                :value="m.brightness"
                :min="0"
                :max="100"
                :step="1"
                :disabled="!m.brightness_supported"
                class="bri-slider"
                @update:value="(v: number) => onBrightnessChange(m, v)"
              />
              <span class="bri-value">{{ m.brightness }}</span>
            </div>
            <div v-if="!m.hdr_supported && !m.brightness_supported" class="bri-unsupported">
              此显示器不支持 HDR 与亮度控制
            </div>
          </div>
          <div v-if="monitorPanel.length === 0" class="bri-unsupported">
            未检测到显示器
          </div>
          <!-- 面板原生色域（WinRT AdvancedColorInfo 实测；真 HDR 画布经 DWM 映射到该色域） -->
          <div v-if="panelGamut" class="gamut-row" title="面板实测原生色域（CIE xy 原色）；真 HDR 画布输出 scRGB，由 Windows 按此色域自动映射">
            <span class="gamut-name">{{ panelGamut.name }}</span>
            <span class="gamut-cov">P3 {{ Math.round(panelGamut.p3_coverage * 100) }}%</span>
            <span class="gamut-cov">2020 {{ Math.round(panelGamut.bt2020_coverage * 100) }}%</span>
            <span v-if="panelGamut.max_luminance > 0" class="gamut-cov">
              峰值 {{ Math.round(panelGamut.max_luminance) }} nits
            </span>
          </div>
        </div>
      </n-popover>
      <!-- 主题切换 -->
      <n-popselect
        :value="props.mode"
        :options="themeOptions"
        trigger="click"
        size="small"
        @update:value="(v: string) => props.setMode(v as ThemeMode)"
      >
        <button class="win-btn icon-btn" title="主题">
          <n-icon :component="props.isDark ? Moon : Sun" size="14" />
        </button>
      </n-popselect>
      <!-- Win11 原生按钮：Segoe Fluent Icons -->
      <button class="win-btn caption-btn" @click="minimize" title="最小化">
        <span class="fluent-icon">&#xE949;</span>
      </button>
      <button class="win-btn caption-btn" @click="toggleMaximize" title="最大化/还原">
        <span class="fluent-icon">&#xE922;</span>
      </button>
      <button class="win-btn caption-btn close" @click="close" title="关闭">
        <span class="fluent-icon">&#xE8BB;</span>
      </button>
    </div>
  </div>
</template>

<style scoped>
.titlebar {
  height: var(--jb-titlebar-h);
  flex-shrink: 0;
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 0 0 0 14px;
  user-select: none;
  background: var(--jb-titlebar);
  /* 提升到预览弹窗遮罩层之上：预览打开时仍可拖动窗口 */
  position: relative;
  z-index: 3000;
}
.title-left {
  display: flex;
  align-items: center;
  gap: 6px;
}
.title-text {
  font-size: 12px;
  color: var(--jb-text-soft);
  font-weight: 500;
}
.title-actions {
  display: flex;
  gap: 0;
  align-items: center;
}
.win-btn {
  border: none;
  background: transparent;
  cursor: pointer;
  color: var(--jb-text);
  border-radius: 4px;
  display: flex;
  align-items: center;
  justify-content: center;
  transition: background-color 0.15s;
}
.icon-btn {
  width: 32px;
  height: 28px;
  margin-right: 4px;
}
.icon-btn:hover {
  background: var(--jb-titlebar-btn-hover);
}
/* 置顶等激活态：主题色高亮 */
.icon-btn.active {
  color: var(--jb-primary);
}
.icon-btn.active .n-icon {
  color: var(--jb-primary);
}
.back-btn {
  margin-left: -8px;
}
.caption-btn {
  width: 36px;
  height: 32px;
  font-size: 11px;
}
.caption-btn:hover {
  background: var(--jb-titlebar-btn-hover);
}
.caption-btn.close:hover {
  background: var(--jb-close-hover);
  color: #fff;
}
.fluent-icon {
  font-family: "Segoe Fluent Icons", "Segoe MDL2 Assets";
  font-size: 10px;
  line-height: 1;
}

/* === 显示器弹出面板 === */
.monitor-panel {
  display: flex;
  flex-direction: column;
  gap: 12px;
  min-width: 240px;
  max-width: 280px;
}
.monitor-item {
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.monitor-item + .monitor-item {
  padding-top: 12px;
  border-top: 1px solid var(--jb-border);
}
.monitor-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
}
.monitor-name {
  display: flex;
  align-items: center;
  gap: 6px;
  min-width: 0;
  font-size: 13px;
  color: var(--jb-text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.hdr-switch {
  display: flex;
  align-items: center;
  gap: 6px;
  flex-shrink: 0;
}
.hdr-label {
  font-size: 11px;
  font-weight: 600;
  color: var(--jb-text-mute);
}
.monitor-bri-row {
  display: flex;
  align-items: center;
  gap: 8px;
}
.bri-icon {
  color: var(--jb-text-mute);
  flex-shrink: 0;
}
.bri-slider {
  flex: 1;
}
.bri-value {
  font-size: 11px;
  color: var(--jb-text-mute);
  /* 固定宽度：兼容最宽的「480 nits」，两行滑块等长 */
  width: 52px;
  flex-shrink: 0;
  text-align: right;
}
.bri-unsupported {
  font-size: 11px;
  color: var(--jb-text-mute);
}
/* 面板原生色域行：主题色小胶囊组（分类名 + 覆盖率 + 峰值） */
.gamut-row {
  display: flex;
  align-items: center;
  gap: 6px;
  margin-top: 8px;
  padding-top: 8px;
  border-top: 1px solid var(--jb-divider, rgba(0, 0, 0, 0.08));
  flex-wrap: wrap;
}
.gamut-name {
  font-size: 11px;
  font-weight: 600;
  color: var(--jb-primary, #4080ff);
}
.gamut-cov {
  font-size: 10.5px;
  color: var(--jb-text-soft, #888);
  font-variant-numeric: tabular-nums;
}
</style>
