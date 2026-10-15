/**
 * Tauri frontendDist 资产预备（四 EXE 架构）：
 *   workbench-ui/dist → apps/desktop/ui/（主程序：工作台 index.html + 宠物窗 pet/，
 *                       多入口构建时剔除其他应用的入口目录）
 *   dist/sql          → apps/sql/ui/（SQL 独立应用）
 *   dist/serial       → apps/serial/ui/（串口工具）
 *   dist/ssh          → apps/ssh/ui/（SSH 客户端）
 * 入口 HTML 引用绝对路径 /assets/**（共享 chunk），因此每个壳都拷贝 assets/。
 * 替代原 build-exe（SEA）+ prep-sidecar + prep-pet 三条链路。
 */
import { cpSync, rmSync, existsSync, mkdirSync } from 'node:fs'
import { join, resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const dist = join(root, 'packages', 'workbench-ui', 'dist')
const desktopUi = join(root, 'apps', 'desktop', 'ui')

// 子应用入口目录名 → 壳 ui 目标目录（desktop 主程序剔除全部子应用入口）
const APP_SHELLS = [
  { entry: 'ssh', ui: join(root, 'apps', 'ssh', 'ui') },
  { entry: 'sql', ui: join(root, 'apps', 'sql', 'ui') },
  { entry: 'serial', ui: join(root, 'apps', 'serial', 'ui') },
]

if (!existsSync(join(dist, 'index.html'))) {
  console.error('缺少 packages/workbench-ui/dist，请先执行: npm run build:ui')
  process.exit(1)
}

// ── 主程序壳：整 dist 拷贝后剔除其他应用入口目录（desktop 只需工作台 + pet）──
rmSync(desktopUi, { recursive: true, force: true })
mkdirSync(desktopUi, { recursive: true })
cpSync(dist, desktopUi, { recursive: true })
const removed = APP_SHELLS.filter(({ entry }) => {
  rmSync(join(desktopUi, entry), { recursive: true, force: true })
  return existsSync(join(dist, entry, 'index.html'))
}).map(({ entry }) => entry)
const hasPet = existsSync(join(desktopUi, 'pet', 'index.html'))
console.log(`frontendDist 资产就绪: ${desktopUi}（workbench${hasPet ? ' + pet' : '，精简版'}${removed.length ? `，剔除 ${removed.join('/')}` : ''}）`)

// ── 子应用壳：各自入口目录 + 共享 assets ──
for (const { entry, ui } of APP_SHELLS) {
  if (existsSync(join(dist, entry, 'index.html'))) {
    rmSync(ui, { recursive: true, force: true })
    mkdirSync(ui, { recursive: true })
    cpSync(join(dist, entry), ui, { recursive: true })
    cpSync(join(dist, 'assets'), join(ui, 'assets'), { recursive: true })
    console.log(`壳资产就绪: ${ui}`)
  } else {
    console.log(`未检测到 ${entry} 入口，跳过该壳分发`)
  }
}
