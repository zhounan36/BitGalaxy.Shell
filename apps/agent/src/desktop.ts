import { spawn, spawnSync } from 'node:child_process'
import { appendFileSync, mkdirSync, readFileSync, renameSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'

export interface Settings {
  /** 手动启动（非 --silent）时是否自动打开工作台 */
  openOnStart: boolean
}

const AUTOSTART_RUN_KEY = 'HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run'
const AUTOSTART_VALUE_NAME = 'BitGalaxyShell'

/** 日志：控制台（GUI 子系统下可能无效，try 包裹）+ 数据目录文件（滚动追加） */
export function appendLog(dataDir: string, msg: string): void {
  const line = `${new Date().toISOString()} ${msg}`
  try {
    console.log(line)
  } catch {
    // GUI 子系统无控制台句柄，忽略
  }
  try {
    const file = join(dataDir, 'agent.log')
    mkdirSync(dirname(file), { recursive: true })
    appendFileSync(file, `${line}\n`, 'utf8')
  } catch {
    // 日志失败不阻断主流程
  }
}

export async function loadSettings(dataDir: string): Promise<Settings> {
  const file = join(dataDir, 'settings.json')
  try {
    return { openOnStart: true, ...(JSON.parse(readFileSync(file, 'utf8')) as Partial<Settings>) }
  } catch {
    return { openOnStart: true }
  }
}

export async function saveSettings(dataDir: string, settings: Settings): Promise<void> {
  const file = join(dataDir, 'settings.json')
  const tmp = `${file}.tmp`
  mkdirSync(dirname(file), { recursive: true })
  writeFileSync(tmp, JSON.stringify(settings, null, 2), 'utf8')
  renameSync(tmp, file)
}

/** 用系统默认浏览器打开工作台（Windows：cmd start） */
export function openBrowser(url: string): void {
  try {
    if (process.platform === 'win32') {
      spawn('cmd', ['/c', 'start', '', url], { detached: true, stdio: 'ignore', windowsHide: true }).unref()
    } else if (process.platform === 'darwin') {
      spawn('open', [url], { detached: true, stdio: 'ignore' }).unref()
    } else {
      spawn('xdg-open', [url], { detached: true, stdio: 'ignore' }).unref()
    }
  } catch {
    // 打开失败不阻断
  }
}

/** 是否配置了开机自启（注册表 HKCU Run 键） */
export function getAutoStart(): boolean {
  if (process.platform !== 'win32') return false
  const r = spawnSync('reg', ['query', AUTOSTART_RUN_KEY, '/v', AUTOSTART_VALUE_NAME], { windowsHide: true })
  return r.status === 0
}

/** 设置/取消开机自启；自启项以 --silent 启动（开机不弹浏览器） */
export function setAutoStart(enabled: boolean): { ok: boolean; message?: string } {
  if (process.platform !== 'win32') return { ok: false, message: '当前平台暂不支持开机自启' }
  const cmd = enabled
    ? spawnSync('reg', ['add', AUTOSTART_RUN_KEY, '/v', AUTOSTART_VALUE_NAME, '/t', 'REG_SZ', '/d', `"${process.execPath}" --silent`, '/f'], { windowsHide: true })
    : spawnSync('reg', ['delete', AUTOSTART_RUN_KEY, '/v', AUTOSTART_VALUE_NAME, '/f'], { windowsHide: true })
  if (cmd.status === 0) return { ok: true }
  return { ok: false, message: (cmd.stderr?.toString() ?? '').trim() || '注册表操作失败' }
}
