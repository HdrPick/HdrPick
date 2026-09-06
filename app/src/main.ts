import { createApp } from "vue";
import { invoke } from "@tauri-apps/api/core";
import App from "./App.vue";

createApp(App).mount("#app");

// 窗口不可见 → 冻结全部 CSS 动画（规则见 App.vue 全局样式的 .anim-paused）。
// Chromium 对隐藏窗口仍会合成 infinite 动画（aurora-breathe / mem-blink 等），
// 实测应用静默在后台时 msedgewebview2.exe 持续吃 ~6% GPU 即此根因。
// 恢复可见 → 解冻：animation-play-state: paused 从冻结点续播，状态不跳变。
document.addEventListener("visibilitychange", () => {
  document.documentElement.classList.toggle("anim-paused", document.hidden);
});

// 主窗口延迟显示（tauri.conf.json visible:false）：load 事件 = 静态启动页已渲染完毕，
// 此刻才让原生窗口出现，消除启动期"透明内容 + 边框线"的空窗阶段。
// 看图/贴图/区域选择等 hash 路由窗口自行管理显隐，不参与此流程。
(function signalMainReady() {
  const h = window.location.hash || "";
  if (h !== "" && h !== "#" && h !== "#/") return;
  const ready = () => {
    invoke("frontend_main_ready").catch(() => {
      // 非 Tauri 环境忽略
    });
  };
  if (document.readyState === "complete") ready();
  else window.addEventListener("load", ready, { once: true });
})();
