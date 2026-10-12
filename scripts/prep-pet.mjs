/**
 * 宠物窗资源预备：把 pet-ui 构建产物（含 public/pet 素材）复制到
 * apps/desktop/ui/pet/，随 Tauri frontendDist（../ui）一起分发。
 * 桌宠窗 release 形态加载 /pet/index.html。
 */
import { cpSync, rmSync, existsSync, mkdirSync } from 'node:fs'
import { join, resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const src = join(root, 'packages', 'pet-ui', 'dist')
const dest = join(root, 'apps', 'desktop', 'ui', 'pet')

if (!existsSync(join(src, 'index.html'))) {
  console.error('缺少 packages/pet-ui/dist，请先执行: npm run build:pet')
  process.exit(1)
}

rmSync(dest, { recursive: true, force: true })
mkdirSync(dest, { recursive: true })
cpSync(src, dest, { recursive: true })
console.log(`pet 窗资源就绪: ${dest}`)
