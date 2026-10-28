import { defineConfig } from "vite";
import { resolve } from "path";

export default defineConfig({
  clearScreen: false,
  // release 版 Tauri 以 tauri://localhost 加载内嵌资源，绝对路径 /assets 会 404（界面空白）
  base: "./",
  server: {
    host: "127.0.0.1",
    port: 5177,
    strictPort: true,
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    target: "chrome105",
    minify: "esbuild",
    sourcemap: false,
    rollupOptions: {
      input: {
        main: resolve(__dirname, "index.html"),
      },
    },
  },
});
