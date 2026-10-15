import { useEffect, useState } from 'react'
import { Button } from 'antd'
import { aiSocket } from '../api/ws'
import { onAiAsk, type AiAskPayload } from '../api/aiBridge'
import { ChatPanel } from './ChatPanel'

/** AI 基座（7777）连接态 */
type BaseState = 'connecting' | 'down' | 'up'

const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window

/** 三工具壳（SSH/SQL/串口）共享的 AI 侧栏（四 EXE 架构 P5）：
 *  - 经 aiSocket 跨进程连 AI 基座（主程序 7777），复用 ChatPanel 全部对话能力
 *    （流式/审批/敏感输入/工具卡片/历史；广播帧多端同收，桌宠与本侧栏看到同一轮对话）
 *  - 基座未运行 → 闸门卡片 + 「拉起」按钮（IPC spawn_ai_base：静默 spawn
 *    bgshell-desktop.exe 并等端口就绪）
 *  - 自带右下角 FAB 开关与「问AI」监听（终端选区提问自动唤起本侧栏）
 */
export function AiSidebar() {
  const [open, setOpen] = useState(false)
  const [ask, setAsk] = useState<AiAskPayload | null>(null)
  const [base, setBase] = useState<BaseState>('connecting')
  const [launching, setLaunching] = useState(false)
  const [launchErr, setLaunchErr] = useState<string>()

  // 打开侧栏时探测基座；订阅断连/重连伪帧维护状态（ChatPanel 收发都在 aiSocket 上）
  useEffect(() => {
    if (!open) return
    let disposed = false
    void aiSocket
      .connect(2)
      .then(() => !disposed && setBase('up'))
      .catch(() => !disposed && setBase('down'))
    const off = aiSocket.onControl((frame) => {
      if (frame.t !== 'error') return
      if (frame.m.code === 'disconnected') setBase('down')
      else if (frame.m.code === 'reconnected') setBase('up')
    })
    return () => {
      disposed = true
      off()
    }
  }, [open])

  // 终端「问AI」（本窗事件）：唤起侧栏并投递给 ChatPanel（uid 去重在其内部）
  useEffect(() => {
    let disposed = false
    let off: (() => void) | undefined
    void onAiAsk((p) => {
      if (disposed || !p?.uid) return
      setOpen(true)
      setAsk(p)
    }).then((u) => {
      if (disposed) u()
      else off = u
    })
    return () => {
      disposed = true
      off?.()
    }
  }, [])

  const launch = async () => {
    if (!isTauri) {
      setLaunchErr('浏览器开发模式无法自动拉起，请手动启动主程序（npm run desktop:dev）')
      return
    }
    setLaunching(true)
    setLaunchErr(undefined)
    try {
      const { invoke } = await import('@tauri-apps/api/core')
      await invoke('spawn_ai_base')
      await aiSocket.connect(6)
      setBase('up')
    } catch (e) {
      setLaunchErr(e instanceof Error ? e.message : String(e))
    } finally {
      setLaunching(false)
    }
  }

  if (!open) {
    return (
      <button type="button" className="ai-fab" title="打开 AI 助手（连主程序 AI 基座）" onClick={() => setOpen(true)}>
        AI
      </button>
    )
  }
  // .wb-chat-wrap：右下角浮层定位（与工作台 ChatPanel 宿主同款，各壳 index.css 均含该规则）
  return (
    <div className="wb-chat-wrap">
      {base !== 'up' ? (
        <div className="chat-panel ai-base-gate">
          <div className="chat-head">
            <span>鲸鱼娘 · AI 助手</span>
            <button type="button" className="chat-close" onClick={() => setOpen(false)}>
              ×
            </button>
          </div>
          <div className="ai-base-gate-body">
            <div className="ai-base-gate-title">
              {base === 'connecting' ? '正在连接 AI 基座…' : 'AI 基座未运行'}
            </div>
            <div className="ai-base-gate-hint">
              AI 能力由主程序 BitGalaxy Shell（后台服务）提供；拉起后即可在本侧栏对话，
              与桌宠看到同一轮回答。
            </div>
            {launchErr && <div className="ai-base-gate-err">{launchErr}</div>}
            <Button type="primary" loading={launching} disabled={base === 'connecting'} onClick={() => void launch()}>
              拉起 AI 基座
            </Button>
          </div>
        </div>
      ) : (
        <ChatPanel open onClose={() => setOpen(false)} socket={aiSocket} incomingAsk={ask} />
      )}
    </div>
  )
}
