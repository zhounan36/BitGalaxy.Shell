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
      fontFamily: '"Cascadia Mono", "JetBrains Mono", "SF Mono", Consolas, "Courier New", monospace',
      theme: { background: '#081018' },
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
      const before = `${term.cols}x${term.rows}`
      fit.fit()
      // 尺寸未变则跳过，防止 ResizeObserver ↔ fit 反馈循环（以及无谓的 WS resize 帧）
      if (`${term.cols}x${term.rows}` === before) return
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

  return <div ref={holderRef} className="terminal-holder" />
}
