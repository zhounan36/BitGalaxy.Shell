import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5177,
    // 固定 IPv4 回环：Tauri 壳探测 dev server（127.0.0.1），避免 Node 把 localhost 解析到 ::1
    host: '127.0.0.1',
    // 开发模式：/ws 代理到本机 Agent（transport-ws 已放行 5177 来源）
    proxy: {
      '/ws': { target: 'http://127.0.0.1:7777', ws: true },
    },
  },
  // @bgshell/protocol 为 workspace 链接的 TS 源码包（browser 条件直指 src），不走预构建
  optimizeDeps: { exclude: ['@bgshell/protocol'] },
})
