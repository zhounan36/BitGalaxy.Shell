/**
 * Tauri frontendDist 资产预备（单进程架构）：
 *   workbench-ui/dist → apps/desktop/ui/（完整版 MPA：工作台 index.html + 宠物窗 pet/index.html）
 *   SSH 精简版（vite 构建时 BGSH_FLAVOR=ssh）dist 无 pet/，仅打包工作台
 * 替代原 build-exe（SEA）+ prep-sidecar + prep-pet 三条链路。
 */
import { cpSync, rmSync, existsSync, mkdirSync } from 'node:fs'
import { join, resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const uiDir = join(root, 'apps', 'desktop', 'ui')
const workbenchDist = join(root, 'packages', 'workbench-ui', 'dist')

if (!existsSync(join(workbenchDist, 'index.html'))) {
  console.error('缺少 packages/workbench-ui/dist，请先执行: npm run build:ui')
  process.exit(1)
}
// 桌宠入口按 vite 构建形态判断：存在即完整版，不存在即 SSH 精简版
const hasPet = existsSync(join(workbenchDist, 'pet', 'index.html'))
if (!hasPet) {
  console.log('SSH 精简版（BGSH_FLAVOR=ssh）：未检测到桌宠入口，仅打包工作台')
}

// 整目录重建：切换完整版/精简版时自动清掉上一版的 pet/ 残留
rmSync(uiDir, { recursive: true, force: true })
mkdirSync(uiDir, { recursive: true })
cpSync(workbenchDist, uiDir, { recursive: true })
console.log(`frontendDist 资产就绪: ${uiDir}（workbench${hasPet ? ' + pet' : '，精简版'}）`)
