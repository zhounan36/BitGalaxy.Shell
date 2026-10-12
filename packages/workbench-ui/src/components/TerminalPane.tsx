import { useEffect, useRef, useState } from 'react'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import { DataFrameType } from '@bgshell/protocol'
import { agentSocket } from '../api/ws'
import { readClipboardText, writeClipboardText } from '../api/clipboard'

/** 右键菜单状态（clientX/Y + 当前是否有选区） */
interface TermMenuState {
  x: number
  y: number
  hasSelection: boolean
}

/** 单个终端面板：xterm ↔ Agent（stdin/stdout 二进制帧，按 sessionId 复用同一条 WS）
 *  右键呼出自定义菜单：复制 / 粘贴 / 全选 / 清屏；另支持终端惯例快捷键。 */
export function TerminalPane({ sessionId }: { sessionId: string }) {
  const holderRef = useRef<HTMLDivElement>(null)
  const termRef = useRef<Terminal | null>(null)
  const [menu, setMenu] = useState<TermMenuState | null>(null)

  // ── 剪贴板动作（菜单与快捷键共用）─────────────────────────────────────
  const copySelection = () => {
    const term = termRef.current
    if (!term) return
    const sel = term.getSelection()
    if (!sel) return
    void writeClipboardText(sel).catch(() => {})
    term.clearSelection()
  }
  const pasteFromClipboard = async () => {
    const term = termRef.current
    if (!term) return
    // term.paste 会按括号粘贴模式处理换行（\n → \r），无需手工转换
    try {
      const text = await readClipboardText()
      if (text) term.paste(text)
    } catch {
      /* 剪贴板不可用（权限拒绝等）：静默忽略 */
    }
    term.focus()
  }

  useEffect(() => {
    const term = new Terminal({
      fontSize: 13,
      cursorBlink: true,
      fontFamily: '"Cascadia Mono", "JetBrains Mono", "SF Mono", Consolas, "Courier New", monospace',
      theme: { background: '#081018' },
    })
    termRef.current = term
    const fit = new FitAddon()
    term.loadAddon(fit)
    term.open(holderRef.current!)
    fit.fit()
    term.focus()

    // 打开后立即上报实际尺寸（open 帧先用占位 80x24）
    agentSocket.sendControl({ t: 'resize', s: sessionId, m: { cols: term.cols, rows: term.rows } })

    const offData = agentSocket.onData((sid, type, payload) => {
      if (sid !== sessionId || type !== DataFrameType.Stdout) return
      term.write(payload)
    })
    const dataSub = term.onData((d) => {
      agentSocket.sendInput(sessionId, new TextEncoder().encode(d))
    })
    const resizeTo = () => {
      const before = `${term.cols}x${term.rows}`
      fit.fit()
      // 尺寸未变则跳过，防止 ResizeObserver ↔ fit 反馈循环（以及无谓的 WS resize 帧）
      if (`${term.cols}x${term.rows}` === before) return
      agentSocket.sendControl({ t: 'resize', s: sessionId, m: { cols: term.cols, rows: term.rows } })
    }
    const ro = new ResizeObserver(resizeTo)
    ro.observe(holderRef.current!)

    // ── 快捷键：Ctrl+Shift+C / Ctrl+Shift+V / Ctrl+Insert / Shift+Insert ──
    term.attachCustomKeyEventHandler((ev) => {
      if (ev.type !== 'keydown') return true
      const ctrl = ev.ctrlKey || ev.metaKey
      const wantCopy = (ctrl && ev.shiftKey && ev.key.toLowerCase() === 'c') || (ctrl && ev.key === 'Insert')
      const wantPaste = (ctrl && ev.shiftKey && ev.key.toLowerCase() === 'v') || (ev.shiftKey && ev.key === 'Insert')
      if (wantCopy && term.hasSelection()) {
        copySelection()
        return false
      }
      if (wantPaste) {
        void pasteFromClipboard()
        return false
      }
      return true
    })

    // ── 右键菜单：在光标处弹出自定义菜单（替换 WebView 默认菜单）──────────
    const onContextMenu = (e: MouseEvent) => {
      e.preventDefault()
      setMenu({ x: e.clientX, y: e.clientY, hasSelection: term.hasSelection() })
    }
    const holder = holderRef.current!
    holder.addEventListener('contextmenu', onContextMenu)

    return () => {
      holder.removeEventListener('contextmenu', onContextMenu)
      offData()
      dataSub.dispose()
      ro.disconnect()
      term.dispose()
      termRef.current = null
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId])

  // 菜单打开期间：点击菜单外 / Esc / 终端滚动 / 窗口失焦或缩放 → 关闭
  useEffect(() => {
    if (!menu) return
    const close = () => setMenu(null)
    const onMouseDown = (e: MouseEvent) => {
      if (!(e.target as HTMLElement | null)?.closest('.term-menu')) close()
    }
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') close()
    }
    window.addEventListener('mousedown', onMouseDown, true)
    window.addEventListener('keydown', onKeyDown, true)
    window.addEventListener('wheel', close, { capture: true, passive: true })
    window.addEventListener('blur', close)
    window.addEventListener('resize', close)
    return () => {
      window.removeEventListener('mousedown', onMouseDown, true)
      window.removeEventListener('keydown', onKeyDown, true)
      window.removeEventListener('wheel', close, true)
      window.removeEventListener('blur', close)
      window.removeEventListener('resize', close)
    }
  }, [menu])

  const menuStyle: React.CSSProperties | undefined = menu
    ? {
        left: Math.max(8, Math.min(menu.x, window.innerWidth - 200)),
        top: Math.max(8, Math.min(menu.y, window.innerHeight - 170)),
      }
    : undefined

  return (
    <div ref={holderRef} className="terminal-holder">
      {menu && (
        <div className="term-menu" style={menuStyle} onContextMenu={(e) => e.preventDefault()}>
          <button
            type="button"
            className="term-menu-item"
            disabled={!menu.hasSelection}
            onClick={() => {
              copySelection()
              setMenu(null)
            }}
          >
            <span>复制</span>
            <span className="term-menu-key">Ctrl+Shift+C</span>
          </button>
          <button
            type="button"
            className="term-menu-item"
            onClick={() => {
              setMenu(null)
              void pasteFromClipboard()
            }}
          >
            <span>粘贴</span>
            <span className="term-menu-key">Ctrl+Shift+V</span>
          </button>
          <div className="term-menu-sep" />
          <button
            type="button"
            className="term-menu-item"
            onClick={() => {
              termRef.current?.selectAll()
              setMenu({ ...menu, hasSelection: true })
            }}
          >
            <span>全选</span>
          </button>
          <button
            type="button"
            className="term-menu-item"
            onClick={() => {
              termRef.current?.clear()
              termRef.current?.focus()
              setMenu(null)
            }}
          >
            <span>清屏</span>
          </button>
        </div>
      )}
    </div>
  )
}
