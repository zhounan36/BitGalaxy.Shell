import { useEffect, useRef, useState } from 'react'
import { agentSocket } from '../api/ws'

interface ChatMsg {
  id: string
  role: 'user' | 'assistant'
  text: string
  error?: boolean
}

interface Approval {
  reqId: string
  host: string
  command: string
}

/** 单会话（Agent 侧按 chatId 记忆上下文；重置即清空记忆） */
const CHAT_ID = 'pet-main'

/** 桌宠 AI 对话面板：与鲸鱼娘对话驱动 SSH 任务；危险命令弹批准卡片 */
export function ChatPanel({ open, onClose }: { open: boolean; onClose: () => void }) {
  const [msgs, setMsgs] = useState<ChatMsg[]>([])
  const [input, setInput] = useState('')
  const [busy, setBusy] = useState<string>()
  const [approval, setApproval] = useState<Approval | null>(null)
  const listRef = useRef<HTMLDivElement>(null)
  const streamIdRef = useRef<string | undefined>(undefined)

  useEffect(() => {
    const off = agentSocket.onControl((frame) => {
      if (frame.t === 'ai-status') {
        setBusy(frame.m.stage === 'tool' ? (frame.m.detail ?? '执行工具中…') : '思考中…')
        return
      }
      if (frame.t === 'ai-approval') {
        setBusy(undefined)
        setApproval({ reqId: frame.m.reqId, host: frame.m.host, command: frame.m.command })
        return
      }
      if (frame.t === 'ai-delta') {
        setMsgs((prev) => {
          const id = streamIdRef.current
          if (!id) {
            const nid = crypto.randomUUID()
            streamIdRef.current = nid
            return [...prev, { id: nid, role: 'assistant', text: frame.m.delta }]
          }
          return prev.map((m) => (m.id === id ? { ...m, text: m.text + frame.m.delta } : m))
        })
        return
      }
      if (frame.t === 'ai-result') {
        streamIdRef.current = undefined
        setBusy(undefined)
        setMsgs((prev) => {
          const tail = prev[prev.length - 1]
          if (frame.m.text && streamIdRef.current === undefined && tail?.role !== 'assistant') {
            return [...prev, { id: crypto.randomUUID(), role: 'assistant', text: frame.m.text }]
          }
          if (!frame.m.ok && frame.m.message) {
            return [...prev, { id: crypto.randomUUID(), role: 'assistant', text: frame.m.message, error: true }]
          }
          return prev
        })
      }
    })
    return off
  }, [])

  // 消息变化滚到底部
  useEffect(() => {
    const el = listRef.current
    if (el) el.scrollTop = el.scrollHeight
  }, [msgs, busy, approval])

  if (!open) return null

  const send = () => {
    const text = input.trim()
    if (!text || busy || approval) return
    setInput('')
    setMsgs((prev) => [...prev, { id: crypto.randomUUID(), role: 'user', text }])
    setBusy('思考中…')
    agentSocket
      .connect()
      .then(() => agentSocket.sendControl({ t: 'ai', m: { op: 'chat', chatId: CHAT_ID, text } }))
      .catch((err: Error) => {
        setBusy(undefined)
        setMsgs((prev) => [...prev, { id: crypto.randomUUID(), role: 'assistant', text: `连接 Agent 失败：${err.message}`, error: true }])
      })
  }

  const answerApproval = (approved: boolean) => {
    if (!approval) return
    void agentSocket
      .connect()
      .then(() =>
        agentSocket.sendControl({ t: 'ai-approve', m: { chatId: CHAT_ID, reqId: approval.reqId, approved } }),
      )
      .catch(() => {})
    setMsgs((prev) => [
      ...prev,
      {
        id: crypto.randomUUID(),
        role: 'user',
        text: approved ? `（批准执行：${approval.command}）` : '（拒绝执行该命令）',
      },
    ])
    setApproval(null)
    if (approved) setBusy('执行中…')
  }

  return (
    <div className="chat-panel">
      <div className="chat-head">
        <span>鲸鱼娘 · AI 助手</span>
        <button type="button" className="chat-close" title="清空对话记忆" onClick={() => {
          void agentSocket
            .connect()
            .then(() => agentSocket.sendControl({ t: 'ai', m: { op: 'reset', chatId: CHAT_ID } }))
            .catch(() => {})
          streamIdRef.current = undefined
          setMsgs([])
          setBusy(undefined)
          setApproval(null)
        }}>
          新对话
        </button>
        <button type="button" className="chat-close" onClick={onClose}>
          ×
        </button>
      </div>
      <div className="chat-list" ref={listRef}>
        {msgs.length === 0 && (
          <div className="chat-empty">
            试试：看看有哪些主机 / 在 web 服务器上看下磁盘占用 / 列出 /var/log 下的文件
          </div>
        )}
        {msgs.map((m) => (
          <div key={m.id} className={`chat-msg chat-${m.role}${m.error ? ' chat-err' : ''}`}>
            {m.text}
          </div>
        ))}
        {busy && <div className="chat-busy">{busy}</div>}
      </div>
      {approval && (
        <div className="chat-approval">
          <div className="chat-approval-title">AI 请求执行命令（{approval.host}）</div>
          <pre className="chat-approval-cmd">{approval.command}</pre>
          <div className="chat-approval-btns">
            <button type="button" className="chat-ok" onClick={() => answerApproval(true)}>
              批准执行
            </button>
            <button type="button" className="chat-no" onClick={() => answerApproval(false)}>
              拒绝
            </button>
          </div>
        </div>
      )}
      <div className="chat-input-row">
        <input
          className="chat-input"
          value={input}
          placeholder={busy ? '鲸鱼娘正在忙…' : '对鲸鱼娘说点什么…'}
          disabled={!!busy || !!approval}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter') send()
          }}
        />
        <button type="button" className="chat-send" disabled={!!busy || !!approval || !input.trim()} onClick={send}>
          发送
        </button>
      </div>
    </div>
  )
}
