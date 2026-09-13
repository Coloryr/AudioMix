import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";

// Tauri 需要固定端口
export default defineConfig({
  plugins: [vue()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    target: "chrome105",
    minify: "esbuild",
    sourcemap: false,
  },
});
