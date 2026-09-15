import { defineConfig } from 'vite';

// 前端是纯静态资源，由 wasm 后端在编译期嵌入，所以构建配置只做三件事：
//   1. 产物放到 dist/（后端 build.rs 会嵌入这个目录）
//   2. 文件名带 hash，好让后端放心给长缓存
//   3. dev 时把 /api 代理到本地跑的 wasm 后端（wasmtime serve 默认 8080）
export default defineConfig({
  base: './',
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    target: 'es2020',
    sourcemap: false,
    cssMinify: true,
    assetsInlineLimit: 4096,
    rollupOptions: {
      output: {
        entryFileNames: 'assets/[name].[hash].js',
        chunkFileNames: 'assets/[name].[hash].js',
        assetFileNames: 'assets/[name].[hash][extname]',
      },
    },
  },
  server: {
    port: 5173,
    proxy: {
      // 本地开发：npm run dev 的同时用
      //   wasmtime serve -S http=y -S inherit-network=y -S inherit-env=y \
      //     -S listenfd=n target/wasm32-wasip2/release/k3s_wasm_ui.wasm
      // 起后端，前端就能热更新地连真实数据。
      '/api': {
        target: 'http://127.0.0.1:8080',
        changeOrigin: true,
      },
    },
  },
});
