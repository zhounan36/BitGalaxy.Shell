import { existsSync } from 'node:fs'
import { mkdir } from 'node:fs/promises'
import { createRequire } from 'node:module'
import { homedir } from 'node:os'
import { join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import type { AgentOp, AgentResult } from '@bgshell/protocol'
import { startAgentServer } from '@bgshell/transport-ws'
import {
  appendLog,
  getAutoStart,
  loadSettings,
  openBrowser,
  saveSettings,
  setAutoStart,
  type Settings,
} from './desktop.js'

const VERSION = '0.1.0'
/** 静默模式：开机自启使用，不弹浏览器 */
const isSilent = process.argv.includes('--silent') || process.env.BGSH_SILENT === '1'

/** 数据目录（设计稿 §6.1）：Windows %APPDATA%\bitgalaxy-shell；其余平台 ~/.local/state/bitgalaxy-shell */
function resolveDataDir(): string {
  if (process.env.APPDATA) return join(process.env.APPDATA, 'bitgalaxy-shell')
  return join(homedir(), '.local', 'state', 'bitgalaxy-shell')
}

/** SEA 单文件 exe：把内嵌的工作台资源装进内存 Map（key=URL 路径）；普通 node 运行返回 undefined */
function loadSeaStaticAssets(): Map<string, Buffer> | undefined {
  let sea: { isSea: boolean; getAsset(key: string): ArrayBuffer } | undefined
  try {
    // Node ≥20.12；低版本无此模块，普通运行形态
    sea = createRequire(import.meta.url)('node:sea') as typeof sea
  } catch {
    return undefined
  }
  if (!sea?.isSea) return undefined
  const manifest = JSON.parse(Buffer.from(sea.getAsset('ui-manifest.json')).toString('utf8')) as {
    files: string[]
  }
  const assets = new Map<string, Buffer>()
  for (const key of manifest.files) {
    // 'ui/assets/index-x.js' → '/assets/index-x.js'
    assets.set(`/${key.slice('ui/'.length)}`, Buffer.from(sea.getAsset(key)))
  }
  return assets
}

async function handleAgentControl(
  op: AgentOp,
  ctx: { dataDir: string; settings: Settings },
): Promise<AgentResult> {
  switch (op.op) {
    case 'get-info':
      return {
        op: 'get-info',
        ok: true,
        autostart: getAutoStart(),
        openOnStart: ctx.settings.openOnStart,
        version: VERSION,
      }
    case 'set-autostart': {
      const r = setAutoStart(op.enabled)
      return { op: 'set-autostart', ok: r.ok, message: r.message, autostart: getAutoStart() }
    }
    case 'set-open-on-start': {
      ctx.settings.openOnStart = op.enabled
      await saveSettings(ctx.dataDir, ctx.settings)
      return { op: 'set-open-on-start', ok: true, openOnStart: op.enabled }
    }
    case 'shutdown': {
      appendLog(ctx.dataDir, '[bgshell-agent] 收到工作台退出指令，进程退出')
      // 先让回包发出，再退出
      setTimeout(() => process.exit(0), 200)
      return { op: 'shutdown', ok: true }
    }
  }
}

async function main(): Promise<void> {
  // 仓库根 = 本文件向上 4 级（dist/main.js 与 src/main.ts 皆成立）；
  // SEA bundle 中 import.meta 不可用 → 兜底 cwd（静态资源已内嵌，仅作 uiDist 提示用）
  let repoRoot: string
  try {
    repoRoot = resolve(fileURLToPath(import.meta.url), '..', '..', '..', '..')
  } catch {
    repoRoot = process.cwd()
  }
  const port = Number(process.env.BGSH_PORT ?? 7777)
  const dataDir = process.env.BGSH_DATA_DIR ?? resolveDataDir()
  const uiDist = process.env.BGSH_UI_DIST ?? join(repoRoot, 'packages', 'workbench-ui', 'dist')

  await mkdir(dataDir, { recursive: true })
  const settings = await loadSettings(dataDir)
  const staticAssets = loadSeaStaticAssets()

  try {
    await startAgentServer({
      port,
      dataDir,
      uiDist,
      staticAssets,
      agentControl: (op) => handleAgentControl(op, { dataDir, settings }),
    })
  } catch (err) {
    const code = (err as NodeJS.ErrnoException)?.code
    if (code === 'EADDRINUSE') {
      // 已有实例在运行：打开工作台后本进程退出（单实例语义）
      appendLog(dataDir, `[bgshell-agent] 端口 ${port} 已被占用（已有实例），直接引导到现有工作台。`)
      if (!isSilent) openBrowser(`http://127.0.0.1:${port}/`)
      process.exit(0)
    }
    throw err
  }

  appendLog(dataDir, `[bgshell-agent] v${VERSION} 工作台: http://127.0.0.1:${port}/`)
  if (!staticAssets && !existsSync(uiDist)) {
    appendLog(dataDir, `[bgshell-agent] 未找到工作台静态资源（${uiDist}），请先 npm run build:ui`)
  }
  if (!isSilent && settings.openOnStart) {
    openBrowser(`http://127.0.0.1:${port}/`)
  }
}

void main().catch((err) => {
  console.error(err)
  process.exit(1)
})
