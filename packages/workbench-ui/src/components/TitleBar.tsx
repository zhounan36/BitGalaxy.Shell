import { useEffect, useState } from 'react'
import type { Window } from '@tauri-apps/api/window'

/** 仅 Tauri 壳内存在；浏览器宿主（vite dev / 云端）无 __TAURI_INTERNALS__ */
const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window

/**
 * 自定义标题栏：替代主窗口原生标题栏（decorations=false）。
 * 左侧标题区可拖动窗口，右侧自绘最小化 / 最大化（还原）/ 关闭。
 */
export function TitleBar() {
  const [win, setWin] = useState<Window | null>(null)
  const [maximized, setMaximized] = useState(false)

  useEffect(() => {
    if (!isTauri) return
    let disposed = false
    let unlisten: (() => void) | undefined
    void (async () => {
      const { getCurrentWindow } = await import('@tauri-apps/api/window')
      if (disposed) return
      const w = getCurrentWindow()
      setWin(w)
      setMaximized(await w.isMaximized())
      unlisten = await w.onResized(() => {
        void w.isMaximized().then((m) => {
          if (!disposed) setMaximized(m)
        })
      })
    })()
    return () => {
      disposed = true
      unlisten?.()
    }
  }, [])

  if (!isTauri) return null

  return (
    <div className="titlebar">
      <div className="titlebar-drag" data-tauri-drag-region>
        <span className="titlebar-logo" aria-hidden />
        <span className="titlebar-title">BitGalaxy Shell</span>
      </div>
      <div className="titlebar-controls">
        <button
          type="button"
          className="tb-btn"
          onClick={() => void win?.minimize()}
          aria-label="最小化"
          title="最小化"
        >
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <line x1="0" y1="5" x2="10" y2="5" stroke="currentColor" strokeWidth="1" />
          </svg>
        </button>
        <button
          type="button"
          className="tb-btn"
          onClick={() => void win?.toggleMaximize()}
          aria-label={maximized ? '还原' : '最大化'}
          title={maximized ? '还原' : '最大化'}
        >
          {maximized ? (
            <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true" fill="none" stroke="currentColor" strokeWidth="1">
              <rect x="0.5" y="2.5" width="7" height="7" />
              <path d="M2.5 2.5V0.5H9.5V7.5H7.5" />
            </svg>
          ) : (
            <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true" fill="none" stroke="currentColor" strokeWidth="1">
              <rect x="0.5" y="0.5" width="9" height="9" />
            </svg>
          )}
        </button>
        <button
          type="button"
          className="tb-btn tb-close"
          onClick={() => void win?.close()}
          aria-label="关闭"
          title="关闭"
        >
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true" stroke="currentColor" strokeWidth="1">
            <path d="M0.5 0.5L9.5 9.5M9.5 0.5L0.5 9.5" />
          </svg>
        </button>
      </div>
    </div>
  )
}
