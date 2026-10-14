/** Tauri 宿主桥：浏览器宿主下全部 no-op/降级，保证桌宠页双宿主可运行 */

export const isTauri =
  typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window

/** 原生窗口拖拽（桌宠拖动 = 移动整个窗口；浏览器宿主由 App 走 DOM 拖拽） */
export async function startWindowDrag(): Promise<void> {
  if (!isTauri) return
  const { getCurrentWindow } = await import('@tauri-apps/api/window')
  await getCurrentWindow().startDragging()
}

/** 写系统剪贴板（Tauri 壳走原生插件 IPC；浏览器宿主回退 Web Clipboard API） */
export async function writeClipboardText(text: string): Promise<void> {
  if (isTauri) {
    const { invoke } = await import('@tauri-apps/api/core')
    await invoke('plugin:clipboard-manager|write_text', { text })
    return
  }
  await navigator.clipboard.writeText(text)
}

/** ── 桌宠缩放 ─────────────────────────────────────────────────────────────
 * 精灵尺寸持久化在 localStorage（默认 256，步进 16，范围 160..480）；
 * 窗口尺寸由「精灵尺寸 + 对话面板开合」动态推导，宠物本体始终贴右下角。
 */
export const PET_SIZE_KEY = 'bgshell.pet-size'
export const PET_SIZE_DEFAULT = 256
export const PET_SIZE_MIN = 160
export const PET_SIZE_MAX = 480
export const PET_SIZE_STEP = 16

/** 收敛到步进倍数并夹到 [MIN, MAX] */
export function clampPetSize(v: number): number {
  const stepped = Math.round(v / PET_SIZE_STEP) * PET_SIZE_STEP
  return Math.min(PET_SIZE_MAX, Math.max(PET_SIZE_MIN, stepped))
}

export function loadPetSize(): number {
  const raw = Number(localStorage.getItem(PET_SIZE_KEY))
  return Number.isFinite(raw) && raw > 0 ? clampPetSize(raw) : PET_SIZE_DEFAULT
}

export function savePetSize(v: number): void {
  localStorage.setItem(PET_SIZE_KEY, String(clampPetSize(v)))
}

/**
 * 由精灵尺寸 + 对话面板开合 + 右键菜单/动作包面板推导窗口尺寸（逻辑像素）：
 * - rest：气泡区（最高约 164px）+ 精灵盒；宽兜底 340（气泡 max-width）
 * - chat：再容纳右侧 (petSize-46) 偏移 + 360px 宽的对话面板，高度按 rest+80 收敛
 * - menu：右键菜单打开且主机列表非空时，窗口加高以容纳完整菜单
 *   （菜单 bottom 偏移 petSize-40，固定项约 160px，每台主机行约 34px；超出屏幕由
 *   applyPetWindowSize 的屏幕收敛 + CSS 滚动兜底处理）
 * - pack：动作包管理面板（360 宽覆盖式面板，与 chat 同布局规则，高度更矮）
 * 与旧常量在 petSize=256 时完全一致（340×420 / 596×500）。
 */
export function petWindowSize(petSize: number, chat: boolean, menuHosts = 0, pack = false): { w: number; h: number } {
  const restW = Math.max(340, petSize + 84)
  const restH = petSize + 164
  if (chat) return { w: Math.max(596, restW + 256), h: Math.max(500, restH + 80) }
  if (pack) return { w: Math.max(560, restW + 220), h: Math.max(520, restH + 60) }
  if (menuHosts > 0) {
    const menuH = 160 + 34 * menuHosts
    return { w: restW, h: Math.max(restH, petSize - 40 + menuH + 12) }
  }
  return { w: restW, h: restH }
}

/**
 * 把宠物窗调整为目标尺寸：右下角锚定（扩窗向左上生长、收窗向右下缩回，宠物本体不动）。
 * 仅扩窗（对话面板 / 右键菜单 / 动作包面板）时做屏幕收敛，避免被顶出屏幕。浏览器宿主 no-op。
 */
export async function applyPetWindowSize(petSize: number, chat: boolean, menuHosts = 0, pack = false): Promise<void> {
  if (!isTauri) return
  const { getCurrentWindow, currentMonitor } = await import('@tauri-apps/api/window')
  const { LogicalSize, LogicalPosition } = await import('@tauri-apps/api/dpi')
  const win = getCurrentWindow()
  const scale = await win.scaleFactor()
  const toLogical = (v: number) => v / scale
  const [pos, size, monitor] = await Promise.all([win.outerPosition(), win.outerSize(), currentMonitor()])
  const { w, h } = petWindowSize(petSize, chat, menuHosts, pack)
  // 右下角锚定：统一按逻辑坐标计算
  let nx = toLogical(pos.x) + toLogical(size.width) - w
  let ny = toLogical(pos.y) + toLogical(size.height) - h
  // 屏幕收敛（仅扩窗）：宠物被拖到屏幕边缘时，扩窗不得把面板/菜单顶出屏幕外
  if ((chat || pack || menuHosts > 0) && monitor) {
    const ms = monitor.scaleFactor
    const mx = monitor.position.x / ms
    const my = monitor.position.y / ms
    const mw = monitor.size.width / ms
    const mh = monitor.size.height / ms
    nx = Math.min(Math.max(nx, mx), mx + mw - w)
    ny = Math.min(Math.max(ny, my), my + mh - h)
  }
  await win.setPosition(new LogicalPosition(nx, ny))
  await win.setSize(new LogicalSize(w, h))
}

/** 双击宠物：Tauri 唤起工作台主窗；浏览器开 Agent 内嵌工作台 */
export async function openWorkbench(): Promise<void> {
  if (isTauri) {
    const { invoke } = await import('@tauri-apps/api/core')
    await invoke('show_main_window')
  } else {
    window.open('http://127.0.0.1:7777', '_blank')
  }
}

/** 隐藏桌宠（托盘"显示桌宠"或再次启动应用唤回；浏览器宿主 no-op） */
export async function hidePet(): Promise<void> {
  if (!isTauri) return
  const { invoke } = await import('@tauri-apps/api/core')
  await invoke('set_pet_visible', { visible: false })
}

/** 退出整个应用（桌宠右键菜单「退出」，与托盘「退出」同路径；浏览器宿主 no-op） */
export async function quitApp(): Promise<void> {
  if (!isTauri) return
  const { invoke } = await import('@tauri-apps/api/core')
  await invoke('quit_app')
}

type SessionKindName = 'shell' | 'sftp' | 'metrics'

/** 向主窗（工作台）发事件。主窗可能还停在启动 loading 壳页（监听未挂载），1.2s 后补发一次 */
async function emitToMainWithRetry(event: string, payload: Record<string, unknown>): Promise<void> {
  const { emitTo } = await import('@tauri-apps/api/event')
  await emitTo('main', event, payload)
  window.setTimeout(() => {
    void emitTo('main', event, payload)
  }, 1200)
}

/** 右键菜单呼出会话：唤起主窗并转发主机/会话类型（浏览器宿主退化为打开工作台） */
export async function openHostSession(hostId: string, kind: SessionKindName): Promise<void> {
  if (!isTauri) {
    await openWorkbench()
    return
  }
  const { invoke } = await import('@tauri-apps/api/core')
  await invoke('show_main_window')
  await emitToMainWithRetry('pet://open-session', { hostId, kind, uid: crypto.randomUUID() })
}

/** 把拖入宠物的文件路径转发给工作台，打开上传面板 */
export async function forwardDropUpload(paths: string[]): Promise<void> {
  if (!isTauri || paths.length === 0) return
  const { invoke } = await import('@tauri-apps/api/core')
  await invoke('show_main_window')
  await emitToMainWithRetry('pet://drop-upload', { paths, uid: crypto.randomUUID() })
}

/** 宠物窗文件拖放信号：悬停（展示提示）/ 离开 / 落下（带本地路径） */
export type FileDropSignal =
  | { type: 'hover' }
  | { type: 'leave' }
  | { type: 'drop'; paths: string[] }

/** 订阅宠物窗的 OS 级文件拖放（Tauri 原生拖放事件；浏览器宿主 no-op） */
export async function onFileDrop(cb: (s: FileDropSignal) => void): Promise<() => void> {
  if (!isTauri) return () => {}
  const { getCurrentWebview } = await import('@tauri-apps/api/webview')
  return getCurrentWebview().onDragDropEvent((event) => {
    if (event.payload.type === 'drop') {
      if (event.payload.paths.length > 0) cb({ type: 'drop', paths: event.payload.paths })
    } else if (event.payload.type === 'leave') {
      cb({ type: 'leave' })
    } else {
      cb({ type: 'hover' })
    }
  })
}
