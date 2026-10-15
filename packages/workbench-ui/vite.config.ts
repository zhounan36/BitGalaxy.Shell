import { resolve } from 'node:path'
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
  server: {
    port: 17177,
    // 固定 IPv4 回环：Tauri 壳探测 dev server（127.0.0.1），避免 Node 把 localhost 解析到 ::1
    host: '127.0.0.1',
    // 开发模式：/ws 代理到本机 Agent（transport-ws 已放行 17177 来源）
    // 端口选冷门段 17177：避开 vite 默认 5173 段，防与其他项目 dev server 冲突
    proxy: {
      '/ws': { target: 'http://127.0.0.1:7777', ws: true },
    },
  },
  // MPA 多入口（四 EXE 架构）：`/` 基座兜底页 + `/pet/` 桌宠（基座 AI 形象）
  // + `/ssh/` `/sql/` `/serial/` 三个工具应用（各入口 HTML 内嵌 window.__BGSH_WS__ 指向后端端口）
  build: {
    rollupOptions: {
      input: {
        index: resolve(__dirname, 'index.html'),
        pet: resolve(__dirname, 'pet/index.html'),
        ssh: resolve(__dirname, 'ssh/index.html'),
        sql: resolve(__dirname, 'sql/index.html'),
        serial: resolve(__dirname, 'serial/index.html'),
      },
    },
  },
  // @bgshell/protocol 为 workspace 链接的 TS 源码包（browser 条件直指 src），不走预构建
  optimizeDeps: { exclude: ['@bgshell/protocol'] },
})
