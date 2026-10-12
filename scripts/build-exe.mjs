/**
 * 打包桌面单文件程序（Node SEA）：
 *   1. esbuild 把 agent（含 ws/ssh2/核心包 TS 源）打成单 CJS bundle
 *   2. workbench-ui/dist 收集为 SEA assets（内嵌工作台，单文件分发）
 *   3. node --experimental-sea-config 生成 blob → postject 注入 node.exe 副本
 *   4. PE Subsystem 改为 GUI（双击启动无控制台黑窗，后台运行）
 * 产物：dist-app/bgshell-agent.exe（双击启动并自动打开工作台；--silent 静默启动）
 */
import { execFileSync } from 'node:child_process'
import { copyFileSync, existsSync, mkdirSync, readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs'
import { join, relative, resolve } from 'node:path'
import { createRequire } from 'node:module'
import * as esbuild from 'esbuild'

const require_ = createRequire(import.meta.url)
const postject = require_('postject')

const root = resolve(import.meta.dirname, '..')
const outDir = join(root, 'dist-app')
const uiDist = join(root, 'packages', 'workbench-ui', 'dist')
const exePath = join(outDir, 'bgshell-agent.exe')
const sentinelFuse = 'NODE_SEA_FUSE_fce680ab2cc467b6e072b8b5df1996b2'

if (!existsSync(uiDist)) {
  console.error('缺少 workbench-ui 构建产物，请先执行: npm run build:ui')
  process.exit(1)
}
mkdirSync(outDir, { recursive: true })

// 1) agent 单文件 bundle（CJS，SEA 入口）
console.log('[1/5] esbuild 打包 agent ...')
await esbuild.build({
  entryPoints: [join(root, 'apps/agent/src/main.ts')],
  bundle: true,
  platform: 'node',
  target: 'node20',
  format: 'cjs',
  outfile: join(outDir, 'bundle.cjs'),
  external: ['cpu-features'], // ssh2 可选原生依赖，缺失时自动回退纯 JS
})

// 2) 工作台资源 → SEA assets（'ui/<相对路径>' 为 key）
console.log('[2/5] 收集工作台静态资源 ...')
const files = []
function walk(dir) {
  for (const name of readdirSync(dir)) {
    const p = join(dir, name)
    if (statSync(p).isDirectory()) walk(p)
    else files.push(p)
  }
}
walk(uiDist)
const assetKeys = files.map((f) => `ui/${relative(uiDist, f).split('\\').join('/')}`)
writeFileSync(join(outDir, 'ui-manifest.json'), JSON.stringify({ files: assetKeys }))
const assets = {}
files.forEach((f, i) => (assets[assetKeys[i]] = f))
assets['ui-manifest.json'] = join(outDir, 'ui-manifest.json')

// 3) SEA blob
console.log('[3/5] 生成 SEA blob ...')
writeFileSync(
  join(outDir, 'sea-config.json'),
  JSON.stringify(
    { main: 'bundle.cjs', output: 'sea.blob', disableExperimentalSEAWarning: true, assets },
    null,
    2,
  ),
)
execFileSync(process.execPath, ['--experimental-sea-config', 'sea-config.json'], {
  cwd: outDir,
  stdio: 'inherit',
})

// 4) 注入 node.exe 副本
console.log('[4/5] postject 注入单文件 exe ...')
copyFileSync(process.execPath, exePath)
await postject.inject(exePath, 'NODE_SEA_BLOB', readFileSync(join(outDir, 'sea.blob')), {
  sentinelFuse,
})

// 5) PE Subsystem: CONSOLE(3) → GUI(2)，双击运行无黑窗
console.log('[5/5] 切换 PE 子系统为 GUI ...')
patchSubsystemGui(exePath)

console.log(`完成: ${exePath}`)
console.log('  双击启动（自动打开工作台）；开机自启项以 --silent 静默启动')

function patchSubsystemGui(exe) {
  const buf = readFileSync(exe)
  const peOff = buf.readUInt32LE(0x3c)
  const optOff = peOff + 4 + 20 // PE 签名(4) + COFF 头(20)
  const magic = buf.readUInt16LE(optOff)
  // PE32+ (0x20b) 与 PE32 (0x10b) 的 Subsystem 字段都在 OptionalHeader+68
  if (magic !== 0x20b && magic !== 0x10b) throw new Error(`未知 PE OptionalHeader magic: 0x${magic.toString(16)}`)
  const subsystemOff = optOff + 68
  const current = buf.readUInt16LE(subsystemOff)
  if (current !== 2 && current !== 3) throw new Error(`意外的 Subsystem 值: ${current}`)
  buf.writeUInt16LE(2, subsystemOff) // IMAGE_SUBSYSTEM_WINDOWS_GUI
  writeFileSync(exe, buf)
}
