/**
 * Tauri sidecar 预备：把 Node SEA 产物 dist-app/bgshell-agent.exe 复制为
 * src-tauri/binaries/bgshell-agent-<target-triple>.exe（Tauri externalBin 命名约定）。
 * target triple 优先取 rustc -vV 的 host，失败时回退 Windows x64 默认值。
 */
import { copyFileSync, mkdirSync, existsSync } from 'node:fs'
import { execFileSync } from 'node:child_process'
import { join, resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const src = join(root, 'dist-app', 'bgshell-agent.exe')
const binariesDir = join(root, 'apps', 'desktop', 'src-tauri', 'binaries')

if (!existsSync(src)) {
  console.error('缺少 dist-app/bgshell-agent.exe，请先执行: npm run build:exe')
  process.exit(1)
}

function detectTriple() {
  try {
    const out = execFileSync('rustc', ['-vV'], { encoding: 'utf8', windowsHide: true })
    const m = out.match(/host:\s*(\S+)/)
    if (m) return m[1]
  } catch {
    // rustc 未就绪
  }
  return 'x86_64-pc-windows-msvc'
}

const triple = detectTriple()
mkdirSync(binariesDir, { recursive: true })
const dest = join(binariesDir, `bgshell-agent-${triple}.exe`)
copyFileSync(src, dest)
console.log(`sidecar 就绪: ${dest} (triple: ${triple})`)
