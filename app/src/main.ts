import { createApp } from "vue";
import { createPinia } from "pinia";
import App from "./App.vue";
import "./style.css";

// 桌面应用不该出现 WebView2 自带的右键菜单（刷新/另存为/检查…）。
// 输入框/文本域里保留右键，方便粘贴。
window.addEventListener("contextmenu", (e) => {
  const t = e.target as HTMLElement | null;
  const tag = t?.tagName;
  if (tag === "INPUT" || tag === "TEXTAREA" || t?.isContentEditable) return;
  e.preventDefault();
});

createApp(App).use(createPinia()).mount("#app");
