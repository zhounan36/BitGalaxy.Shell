/** Tauri 宿主桥：浏览器宿主下全部 no-op/降级，保证 pet-ui 双宿主可运行 */

export const isTauri =
  typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window

/** 原生窗口拖拽（桌宠拖动 = 移动整个窗口；浏览器宿主由 App 走 DOM 拖拽） */
export async function startWindowDrag(): Promise<void> {
  if (!isTauri) return
  const { getCurrentWindow } = await import('@tauri-apps/api/window')
  await getCurrentWindow().startDragging()
}

/** 对话面板展开后的宠物窗尺寸（逻辑像素）：容纳 right:210 / width:360 的面板与余量 */
const CHAT_OPEN_W = 596.0
const CHAT_OPEN_H = 500.0
/** 收起时回到常驻尺寸（与 Rust 侧 PET_W / PET_H 一致） */
const PET_REST_W = 340.0
const PET_REST_H = 420.0

/**
 * 对话面板开合 → 动态调整宠物窗大小。
 * 宠物本体贴窗口右下角，因此扩窗时向左上方生长（右下角锚定不动），关面板时收回常驻尺寸。
 * 浏览器宿主 no-op（窗口即整个浏览器视口）。
 */
export async function setPetWindowChat(open: boolean): Promise<void> {
  if (!isTauri) return
  const { getCurrentWindow } = await import('@tauri-apps/api/window')
  const { LogicalSize, LogicalPosition } = await import('@tauri-apps/api/dpi')
  const win = getCurrentWindow()
  const scale = await win.scaleFactor()
  const toLogical = (v: number) => v / scale
  const [pos, size] = await Promise.all([win.outerPosition(), win.outerSize()])
  const w = toLogical(size.width)
  const h = toLogical(size.height)
  const nw = open ? CHAT_OPEN_W : PET_REST_W
  const nh = open ? CHAT_OPEN_H : PET_REST_H
  // 右下角锚定：扩窗向左上生长，收窗向右下缩回（统一按逻辑坐标计算）
  await win.setPosition(new LogicalPosition(toLogical(pos.x) + w - nw, toLogical(pos.y) + h - nh))
  await win.setSize(new LogicalSize(nw, nh))
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
