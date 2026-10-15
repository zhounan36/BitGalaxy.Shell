import { resolve } from 'node:path'
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// SSH 精简版（BGSH_FLAVOR=ssh）：只构建工作台单入口，跳过桌宠页 /pet/ 与
// public 动作包素材（pet-packs/** 约 89MB + 规范文档/模板），配合 Tauri
// --no-default-features（不含 pet feature）实现独立 SSH 客户端安装包
const sshOnly = process.env.BGSH_FLAVOR === 'ssh'

export default defineConfig({
  plugins: [react()],
  // 精简版不打 public 目录（全部为桌宠资源）
  publicDir: sshOnly ? false : 'public',
  // 构建形态常量：前端按完整版/精简版分流（如终端「问AI」的目标窗口、工作台是否内嵌 ChatPanel）
  define: { __BGSH_PET__: JSON.stringify(!sshOnly) },
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
  // MPA 多入口：`/` 工作台（index.html）+ `/pet/` 桌宠 + `/ssh/` SSH 客户端 + `/sql/` SQL 客户端 + `/serial/` 串口工具
  // （四 EXE 架构：各入口 HTML 内嵌 window.__BGSH_WS__ 指向后端端口）；精简版仅工作台入口
  build: {
    rollupOptions: {
      input: sshOnly
        ? { index: resolve(__dirname, 'index.html') }
        : {
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
