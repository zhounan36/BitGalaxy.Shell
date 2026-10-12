import { spawn, spawnSync } from 'node:child_process'
import { appendFileSync, existsSync, mkdirSync } from 'node:fs'
import { dirname, join } from 'node:path'
import type { SettingsStore } from '@bgshell/connector-core'

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

export function loadSettings(store: SettingsStore): Settings {
  return { openOnStart: store.get<boolean>('app.openOnStart') ?? true }
}

export function saveSettings(store: SettingsStore, settings: Settings): void {
  store.set('app.openOnStart', settings.openOnStart)
}

/**
 * 打开工作台：优先 Chromium 系 --app 模式（独立窗口，无标签栏/地址栏，接近桌面应用观感），
 * 找不到 Edge/Chrome 再回退系统默认浏览器。
 */
export function openBrowser(url: string): void {
  try {
    if (process.platform === 'win32') {
      const pf86 = process.env['ProgramFiles(x86)'] ?? ''
      const pf = process.env.ProgramFiles ?? ''
      const la = process.env.LOCALAPPDATA ?? ''
      const candidates = [
        join(pf86, 'Microsoft/Edge/Application/msedge.exe'),
        join(pf, 'Microsoft/Edge/Application/msedge.exe'),
        join(pf, 'Google/Chrome/Application/chrome.exe'),
        join(la, 'Google/Chrome/Application/chrome.exe'),
      ].filter((p) => p.length > 2 && existsSync(p))
      for (const exe of candidates) {
        const child = spawn(exe, [`--app=${url}`, '--window-size=1360,860'], {
          detached: true,
          stdio: 'ignore',
          windowsHide: true,
        })
        child.on('error', () => undefined) // 启动失败尝试下一个/回退
        child.unref()
        return
      }
    }
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
