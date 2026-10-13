/**
 * Tauri frontendDist 资产预备（单进程架构）：
 *   workbench-ui/dist → apps/desktop/ui/（MPA：主窗工作台 index.html + 宠物窗 pet/index.html）
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
if (!existsSync(join(workbenchDist, 'pet', 'index.html'))) {
  console.error('缺少 packages/workbench-ui/dist/pet/index.html（MPA 桌宠入口），请先执行: npm run build:ui')
  process.exit(1)
}

rmSync(uiDir, { recursive: true, force: true })
mkdirSync(uiDir, { recursive: true })
cpSync(workbenchDist, uiDir, { recursive: true })
console.log(`frontendDist 资产就绪: ${uiDir}（workbench + pet）`)
