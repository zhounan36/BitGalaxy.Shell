/**
 * Tauri frontendDist 资产预备（单进程架构）：
 *   1. workbench-ui/dist → apps/desktop/ui/（主窗工作台 SPA，index.html 即入口）
 *   2. pet-ui/dist       → apps/desktop/ui/pet/（宠物窗，挂载 /pet/）
 * 替代原 build-exe（SEA）+ prep-sidecar + prep-pet 三条链路。
 */
import { cpSync, rmSync, existsSync, mkdirSync } from 'node:fs'
import { join, resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const uiDir = join(root, 'apps', 'desktop', 'ui')
const workbenchDist = join(root, 'packages', 'workbench-ui', 'dist')
const petDist = join(root, 'packages', 'pet-ui', 'dist')

if (!existsSync(join(workbenchDist, 'index.html'))) {
  console.error('缺少 packages/workbench-ui/dist，请先执行: npm run build:ui')
  process.exit(1)
}
if (!existsSync(join(petDist, 'index.html'))) {
  console.error('缺少 packages/pet-ui/dist，请先执行: npm run build:pet')
  process.exit(1)
}

rmSync(uiDir, { recursive: true, force: true })
mkdirSync(uiDir, { recursive: true })
cpSync(workbenchDist, uiDir, { recursive: true })
cpSync(petDist, join(uiDir, 'pet'), { recursive: true })
console.log(`frontendDist 资产就绪: ${uiDir}（workbench + pet）`)
