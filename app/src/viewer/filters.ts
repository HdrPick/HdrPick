// 共享滤镜预设表：编辑页（EditorView）与看图窗口共用
// css 字符串可直接用于 <img> 的 style.filter 与 Canvas 的 ctx.filter（两者同语法，WebView2 支持良好）
// 组合仅用 brightness / contrast / saturate / sepia / hue-rotate / blur（黑白两款另加 grayscale），
// 保证「预览所见」与「导出所得」完全一致

/** 滤镜预设项 */
export interface FilterPreset {
  /** 唯一标识（稳定，供选中态比对） */
  id: string;
  /** 中文显示名 */
  name: string;
  /** CSS filter 字符串（可直接赋给 style.filter / ctx.filter） */
  css: string;
}

/** 12 款女生向滤镜预设（柔肤 / 冷调 / 胶片 / 氛围感路线） */
export const FILTER_PRESETS: FilterPreset[] = [
  {
    id: "cream-skin",
    name: "奶油肌",
    css: "brightness(1.06) contrast(0.94) saturate(1.05) sepia(0.1)",
  },
  {
    id: "cool-white",
    name: "清透冷白",
    css: "brightness(1.12) contrast(0.96) saturate(0.88) hue-rotate(-10deg)",
  },
  {
    id: "film-soft",
    name: "胶片柔焦",
    css: "sepia(0.18) brightness(1.02) contrast(0.92) saturate(0.9) blur(0.4px)",
  },
  {
    id: "milk-apricot",
    name: "奶杏氛围感",
    css: "brightness(1.05) contrast(0.93) saturate(1.08) sepia(0.22)",
  },
  {
    id: "peach-oolong",
    name: "蜜桃乌龙",
    css: "brightness(1.04) contrast(0.96) saturate(1.12) sepia(0.12) hue-rotate(-6deg)",
  },
  {
    id: "mist-blue",
    name: "雾蓝调",
    css: "brightness(1.03) contrast(0.95) saturate(0.8) hue-rotate(14deg)",
  },
  {
    id: "cream-latte",
    name: "奶油拿铁",
    css: "brightness(1.03) contrast(0.95) saturate(0.95) sepia(0.3)",
  },
  {
    id: "first-love",
    name: "初恋粉",
    css: "brightness(1.07) contrast(0.94) saturate(1.15) sepia(0.15) hue-rotate(-10deg)",
  },
  {
    id: "sunset-orange",
    name: "日落暖橘",
    css: "brightness(1.05) contrast(1.02) saturate(1.2) sepia(0.28) hue-rotate(-6deg)",
  },
  {
    id: "mint-ice",
    name: "薄荷冰",
    css: "brightness(1.09) contrast(0.97) saturate(0.85) hue-rotate(30deg)",
  },
  {
    id: "bw-film",
    name: "黑白胶片",
    css: "grayscale(1) brightness(1.02) contrast(1.12)",
  },
  {
    id: "gray-chic",
    name: "高级灰",
    css: "grayscale(0.35) brightness(1.03) contrast(0.92) saturate(0.75)",
  },
];

/**
 * 调节参数 → CSS filter 字符串（与预设叠加使用，声明顺序：预设在前、调节在后）
 * 入参范围：brightness / contrast 为 50-150（100 = 中性），saturation 为 0-200（100 = 中性）
 * 归一规则：数值 ÷ 100 即 CSS 倍率；全中性时返回 ""（无调节，不进合成串）
 */
export function adjustToCss(
  brightness: number,
  contrast: number,
  saturation: number,
): string {
  const b = Math.min(150, Math.max(50, brightness)) / 100;
  const c = Math.min(150, Math.max(50, contrast)) / 100;
  const s = Math.min(200, Math.max(0, saturation)) / 100;
  if (b === 1 && c === 1 && s === 1) return "";
  // 两位小数截断，避免浮点尾巴进串
  const f = (v: number) => String(Math.round(v * 100) / 100);
  return `brightness(${f(b)}) contrast(${f(c)}) saturate(${f(s)})`;
}
