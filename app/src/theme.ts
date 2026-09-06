import type { GlobalThemeOverrides } from "naive-ui";
import { deriveHover, derivePressed } from "./themeColor";

// 主题覆盖：主色由 themeColor store 动态传入（预设或自定义），
// hover/pressed 由 HSL 派生（-8% / -16% 亮度），其余甜系配色保持一致。

function buildOverrides(dark: boolean, primary: string): GlobalThemeOverrides {
  const hover = deriveHover(primary);
  const pressed = derivePressed(primary);
  return {
    common: {
      // 主色（随主题色方案）
      primaryColor: primary,
      primaryColorHover: hover,
      primaryColorPressed: pressed,
      primaryColorSuppl: primary,

      // 成功 薄荷绿
      successColor: "#98C9B8",
      successColorHover: "#88BBA8",
      successColorPressed: "#7AAAA0",
      successColorSuppl: "#98C9B8",

      // 警告 奶茶橘
      warningColor: "#E8C49E",
      warningColorHover: "#DCB58A",
      warningColorPressed: "#D0A576",
      warningColorSuppl: "#E8C49E",

      // 危险 豆沙红
      errorColor: "#D89898",
      errorColorHover: "#C88888",
      errorColorPressed: "#B87878",
      errorColorSuppl: "#D89898",

      // 圆角统一 8px
      borderRadius: "8px",
      borderRadiusSmall: "4px",

      // 字体
      fontFamily:
        '"Segoe UI Variable", "Segoe UI", "Microsoft YaHei", system-ui, sans-serif',
      fontSize: "13px",
    },
    Button: {
      // 按钮圆角 8px
      borderRadiusMedium: "8px",
      borderRadiusSmall: "6px",
      // 主按钮高度 36px
      heightMedium: "36px",
      heightSmall: "28px",
    },
    Card: {
      // 卡片圆角 10px
      borderRadius: "10px",
      paddingMedium: "16px",
    },
    Input: {
      borderRadius: "8px",
      // 4.7 聚焦外发光（主色 15% 柔光，随主题色）
      boxShadowFocus: `0 0 0 2px ${hexAlpha(primary, 0.15)}`,
    },
    Select: {
      peers: {
        InternalSelection: {
          borderRadius: "8px",
        },
      },
    },
    Slider: {
      // 4.6 滑块 4px 轨道 / 14px 圆点
      railHeight: "4px",
      handleSize: "14px",
      fillColor: primary,
      fillColorHover: hover,
      handleColor: "#FFFFFF",
    },
    Switch: {
      railColorActive: primary,
      loadingColor: primary,
    },
    Checkbox: {
      colorChecked: primary,
      borderChecked: `1px solid ${primary}`,
      borderFocus: `1px solid ${primary}`,
    },
    Dialog: {
      // 4.8 弹窗圆角 10px
      borderRadius: "10px",
    },
    Modal: {
      borderRadius: "10px",
    },
    Message: {
      // 4.9 toast 圆角 12px + 4.12 降饱和度柔和底色
      borderRadius: "12px",
      color: dark ? "rgba(56, 52, 64, 0.96)" : "rgba(250, 248, 250, 0.96)",
      textColor: dark ? "#EDEBF2" : "#2C2C34",
      iconColorInfo: primary,
      iconColorSuccess: "#98C9B8",
      iconColorWarning: "#E8C49E",
      iconColorError: "#D89898",
      iconColorLoading: primary,
    },
  };
}

function hexAlpha(hex: string, alpha: number): string {
  const m = /^#?([0-9a-f]{6})$/i.exec(hex.trim());
  if (!m) return `rgba(200, 162, 200, ${alpha})`;
  const n = parseInt(m[1], 16);
  return `rgba(${(n >> 16) & 255}, ${(n >> 8) & 255}, ${n & 255}, ${alpha})`;
}

export { buildOverrides };

// 兼容旧引用（默认香芋紫）
export const lightThemeOverrides = buildOverrides(false, "#C8A2C8");
export const darkThemeOverrides = buildOverrides(true, "#C8A2C8");
export const themeOverrides = lightThemeOverrides;
