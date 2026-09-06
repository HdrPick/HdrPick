import { ref, computed, watch, onMounted, onUnmounted } from "vue";
import { darkTheme } from "naive-ui";
import type { GlobalTheme } from "naive-ui";
import { invoke } from "@tauri-apps/api/core";

export type ThemeMode = "follow" | "light" | "dark";

const STORAGE_KEY = "jietu-hdr:theme-mode";

const mode = ref<ThemeMode>(loadMode());
const systemDark = ref(false);

function loadMode(): ThemeMode {
  const v = localStorage.getItem(STORAGE_KEY);
  if (v === "light" || v === "dark" || v === "follow") return v;
  return "follow";
}

function persistMode() {
  localStorage.setItem(STORAGE_KEY, mode.value);
}

function detectSystem() {
  systemDark.value = window.matchMedia("(prefers-color-scheme: dark)").matches;
}

let mediaListener: (() => void) | null = null;

const isDark = computed(() => {
  if (mode.value === "dark") return true;
  if (mode.value === "light") return false;
  return systemDark.value;
});

const naiveTheme = computed<GlobalTheme | null>(() =>
  isDark.value ? darkTheme : null,
);

watch(mode, persistMode);
watch(
  isDark,
  (dark) => {
    document.documentElement.classList.toggle("dark", dark);
    document.documentElement.classList.toggle("light", !dark);
    // 通知后端切换窗口材质：浅色 Mica / 深色 Mica-Alt
    invoke("set_window_theme", { dark }).catch(() => {
      // 非 Tauri 环境（纯浏览器调试）忽略
    });
  },
  { immediate: true },
);

export function useTheme() {
  onMounted(() => {
    detectSystem();
    if (!mediaListener) {
      const mql = window.matchMedia("(prefers-color-scheme: dark)");
      const handler = () => detectSystem();
      mql.addEventListener("change", handler);
      mediaListener = () => mql.removeEventListener("change", handler);
    }
  });
  onUnmounted(() => {
    if (mediaListener) {
      mediaListener();
      mediaListener = null;
    }
  });

  return {
    mode,
    isDark,
    naiveTheme,
    setMode: (m: ThemeMode) => {
      mode.value = m;
    },
  };
}
