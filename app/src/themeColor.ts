// 主题色管理：预设 / 自定义 / 方案保存 / HSL 派生 / CSS 变量注入
import { ref, computed } from "vue";

// ====== 预设主题色（女生向低饱和甜系） ======
export interface ThemePreset {
  id: string;
  name: string;
  primary: string;
}

export const THEME_PRESETS: ThemePreset[] = [
  { id: "taro", name: "香芋紫", primary: "#C8A2C8" },
  { id: "sakura", name: "樱花粉", primary: "#E8A8C0" },
  { id: "mint", name: "薄荷绿", primary: "#98C9B8" },
  { id: "sky", name: "天空蓝", primary: "#9EC1E8" },
  { id: "milk", name: "奶茶橘", primary: "#E8C49E" },
  { id: "rose", name: "豆沙红", primary: "#D89898" },
  // 设计稿三套主题（取深色值系作主色，保证浅色模式按钮白字对比度）
  { id: "mist-pink", name: "柔雾粉", primary: "#D89AAF" },
  { id: "taro-purple", name: "芋泥紫", primary: "#B8A4C9" },
  { id: "cream", name: "奶白治愈", primary: "#D4B8A8" },
];

// ====== 自定义方案（名称 → #RRGGBB），由配置加载 ======
export const customSchemes = ref<Record<string, string>>({});

// ====== 当前主题色标识（预设 id 或 #RRGGBB） ======
const themeColorId = ref<string>("taro");

// ====== 颜色工具：hex ⇄ HSL ======
function hexToRgb(hex: string): [number, number, number] {
  const m = /^#?([0-9a-f]{6})$/i.exec(hex.trim());
  if (!m) return [200, 162, 200];
  const n = parseInt(m[1], 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

export function hexToHsl(hex: string): [number, number, number] {
  const [r0, g0, b0] = hexToRgb(hex);
  const r = r0 / 255, g = g0 / 255, b = b0 / 255;
  const max = Math.max(r, g, b), min = Math.min(r, g, b);
  const l = (max + min) / 2;
  if (max === min) return [0, 0, l]; // 灰色
  const d = max - min;
  const s = l > 0.5 ? d / (2 - max - min) : d / (max + min);
  let h: number;
  if (max === r) h = ((g - b) / d + (g < b ? 6 : 0)) / 6;
  else if (max === g) h = ((b - r) / d + 2) / 6;
  else h = ((r - g) / d + 4) / 6;
  return [h, s, l];
}

function hslToHex(h: number, s: number, l: number): string {
  const hue2rgb = (p: number, q: number, t: number) => {
    if (t < 0) t += 1;
    if (t > 1) t -= 1;
    if (t < 1 / 6) return p + (q - p) * 6 * t;
    if (t < 1 / 2) return q;
    if (t < 2 / 3) return p + (q - p) * (2 / 3 - t) * 6;
    return p;
  };
  l = Math.min(1, Math.max(0, l));
  if (s === 0) {
    const v = Math.round(l * 255);
    return `#${v.toString(16).padStart(2, "0").repeat(3)}`;
  }
  const q = l < 0.5 ? l * (1 + s) : l + s - l * s;
  const p = 2 * l - q;
  const r = Math.round(hue2rgb(p, q, h + 1 / 3) * 255);
  const g = Math.round(hue2rgb(p, q, h) * 255);
  const b = Math.round(hue2rgb(p, q, h - 1 / 3) * 255);
  return `#${[r, g, b].map((v) => v.toString(16).padStart(2, "0")).join("")}`;
}

/** hover 态：亮度 -8% */
export function deriveHover(primary: string): string {
  const [h, s, l] = hexToHsl(primary);
  return hslToHex(h, s, l - 0.08);
}

/** pressed 态：亮度 -16% */
export function derivePressed(primary: string): string {
  const [h, s, l] = hexToHsl(primary);
  return hslToHex(h, s, l - 0.16);
}

/** 半透明（供 canvas 绘制遮罩等） */
export function deriveAlpha(primary: string, alpha: number): string {
  const [r, g, b] = hexToRgb(primary);
  return `rgba(${r}, ${g}, ${b}, ${alpha})`;
}

// ====== 当前生效颜色（computed，随 themeColorId 变化） ======
export const primaryColor = computed<string>(() => {
  const id = themeColorId.value;
  const preset = THEME_PRESETS.find((p) => p.id === id);
  if (preset) return preset.primary;
  if (/^#[0-9a-fA-F]{6}$/.test(id)) return id.toUpperCase();
  return "#C8A2C8";
});

export const primaryHover = computed(() => deriveHover(primaryColor.value));
export const primaryPressed = computed(() => derivePressed(primaryColor.value));

// ====== CSS 变量注入（全站 --jb-primary 系列即时生效） ======
function applyCssVars(primary: string) {
  const root = document.documentElement.style;
  root.setProperty("--jb-primary", primary);
  root.setProperty("--jb-primary-hover", deriveHover(primary));
  root.setProperty("--jb-primary-pressed", derivePressed(primary));
}

// ====== 对外 API ======
export function useThemeColor() {
  return {
    /** 当前主题色标识（预设 id 或 #RRGGBB） */
    themeColorId,
    /** 当前主色 hex */
    primaryColor,
    primaryHover,
    primaryPressed,
    /** 预设列表 */
    presets: THEME_PRESETS,
    /** 自定义方案 */
    customSchemes,
    /** 从配置加载（get_config 返回后调用） */
    loadFromConfig(colorId: string, schemes: Record<string, { primary: string }>) {
      const map: Record<string, string> = {};
      for (const [name, s] of Object.entries(schemes || {})) {
        if (s?.primary && /^#[0-9a-fA-F]{6}$/.test(s.primary)) {
          map[name] = s.primary.toUpperCase();
        }
      }
      customSchemes.value = map;
      setThemeColor(colorId || "taro");
    },
  };
}

/** 设置主题色（立即生效：更新标识 + CSS 变量） */
export function setThemeColor(id: string) {
  themeColorId.value = id;
  applyCssVars(primaryColor.value);
}

/** 保存自定义方案（前端先记录，持久化由调用方 save_config 完成） */
export function saveScheme(name: string, primary: string) {
  const n = name.trim();
  if (!n || !/^#[0-9a-fA-F]{6}$/.test(primary)) return;
  customSchemes.value = { ...customSchemes.value, [n]: primary.toUpperCase() };
}

/** 删除自定义方案 */
export function removeScheme(name: string) {
  const next = { ...customSchemes.value };
  delete next[name];
  customSchemes.value = next;
}
