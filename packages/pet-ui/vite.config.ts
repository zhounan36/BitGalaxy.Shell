import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
  // 随 Tauri frontendDist 根分发（apps/desktop/ui/pet/），页面挂载在 /pet/ 下：
  // 构建出的 JS/CSS 与 CSS 内 public 素材 URL 均以此为前缀，避免按根路径解析 404
  base: '/pet/',
  server: {
    port: 5178,
    // 开发模式：/ws 代理到本机 Agent（transport-ws 已放行 5178 来源）
    proxy: {
      '/ws': { target: 'http://127.0.0.1:7777', ws: true },
    },
  },
  // @bgshell/protocol 为 workspace 链接的 TS 源码包（browser 条件直指 src），不走预构建
  optimizeDeps: { exclude: ['@bgshell/protocol'] },
  build: { minify: 'esbuild' },
})
