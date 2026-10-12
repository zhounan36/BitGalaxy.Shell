import { useEffect, useRef } from 'react'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import { DataFrameType } from '@bgshell/protocol'
import { agentSocket } from '../api/ws'

/** 单个终端面板：xterm ↔ Agent（stdin/stdout 二进制帧，按 sessionId 复用同一条 WS） */
export function TerminalPane({ sessionId }: { sessionId: string }) {
  const holderRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    const term = new Terminal({
      fontSize: 13,
      cursorBlink: true,
      fontFamily: 'Consolas, "Courier New", monospace',
      theme: { background: '#1e1e1e' },
    })
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
      fit.fit()
      agentSocket.sendControl({ t: 'resize', s: sessionId, m: { cols: term.cols, rows: term.rows } })
    }
    const ro = new ResizeObserver(resizeTo)
    ro.observe(holderRef.current!)

    return () => {
      offData()
      dataSub.dispose()
      ro.disconnect()
      term.dispose()
    }
  }, [sessionId])

  return <div ref={holderRef} style={{ height: '100%', padding: 8 }} />
}
