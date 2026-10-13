import { useEffect, useRef, useState } from 'react'
import { agentSocket } from '../api/ws'
import { writeClipboardText } from '../api/tauri'

interface ChatMsg {
  id: string
  role: 'user' | 'assistant'
  text: string
  thinking?: string
  error?: boolean
}

interface Approval {
  reqId: string
  host: string
  command: string
}

/** 单会话（Agent 侧按 chatId 记忆上下文；重置即清空记忆） */
const CHAT_ID = 'pet-main'

/** 可折叠思考块：流式期间自动展开，回答结束后自动折叠，点击可切换（与主流对话框一致） */
function ThinkBlock({ text, streaming }: { text: string; streaming: boolean }) {
  const [open, setOpen] = useState(streaming)
  const [secs, setSecs] = useState<number>()
  const startRef = useRef(Date.now())

  // 回答结束：记录用时并自动折叠（用户随后仍可点击展开）
  useEffect(() => {
    if (streaming) return
    setSecs(Math.max(1, Math.round((Date.now() - startRef.current) / 1000)))
    setOpen(false)
  }, [streaming])

  return (
    <div className={`chat-think${streaming ? ' chat-think-live' : ''}`}>
      <button
        type="button"
        className="chat-think-toggle"
        title={open ? '收起思考过程' : '展开思考过程'}
        onClick={() => setOpen((v) => !v)}
      >
        <span className={`chat-think-arrow${open ? ' open' : ''}`}>▸</span>
        {streaming ? '思考中…' : secs !== undefined ? `已思考 ${secs} 秒` : '思考过程'}
      </button>
      {open && <div className="chat-think-body">{text}</div>}
    </div>
  )
}

/** 桌宠 AI 对话面板：与鲸鱼娘对话驱动 SSH 任务；危险命令弹批准卡片 */
export function ChatPanel({ open, onClose }: { open: boolean; onClose: () => void }) {
  const [msgs, setMsgs] = useState<ChatMsg[]>([])
  const [input, setInput] = useState('')
  const [busy, setBusy] = useState<string>()
  const [approval, setApproval] = useState<Approval | null>(null)
  const [copiedId, setCopiedId] = useState<string>()
  const listRef = useRef<HTMLDivElement>(null)
  const streamIdRef = useRef<string | undefined>(undefined)
  // 流式消息 id 的渲染镜像：ref 供回调读取，state 驱动 ThinkBlock 展开状态
  const [streamingId, setStreamingId] = useState<string>()

  // 创建/复用当前流式消息，写入正文或思考内容（多轮工具循环的思考以空行分隔）
  const streamInto = (delta: string, thinking: boolean) => {
    let id = streamIdRef.current
    if (!id) {
      id = crypto.randomUUID()
      streamIdRef.current = id
      setStreamingId(id)
      setMsgs((prev) => [
        ...prev,
        { id: id!, role: 'assistant', text: thinking ? '' : delta, thinking: thinking ? delta : undefined },
      ])
      return
    }
    const pid = id
    setMsgs((prev) =>
      prev.map((m) => {
        if (m.id !== pid) return m
        if (thinking) return { ...m, thinking: m.thinking ? `${m.thinking}\n\n${delta}` : delta }
        return { ...m, text: m.text + delta }
      }),
    )
  }

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
      if (frame.t === 'ai-thinking') {
        streamInto(frame.m.delta, true)
        return
      }
      if (frame.t === 'ai-delta') {
        streamInto(frame.m.delta, false)
        return
      }
      if (frame.t === 'ai-result') {
        streamIdRef.current = undefined
        setStreamingId(undefined)
        setBusy(undefined)
        // 本轮对话已结束：审批超时被服务端自动拒绝时也要撤下卡片，
        // 否则输入框被 approval 永久禁用，表现为「卡死」
        setApproval(null)
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

  const copyMsg = (m: ChatMsg) => {
    void writeClipboardText(m.text)
      .then(() => {
        setCopiedId(m.id)
        window.setTimeout(() => setCopiedId(undefined), 1500)
      })
      .catch(() => {})
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
          setStreamingId(undefined)
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
            {m.thinking && <ThinkBlock text={m.thinking} streaming={m.id === streamingId} />}
            {m.text}
            <button
              type="button"
              className="chat-copy"
              title="复制这条消息"
              onClick={() => copyMsg(m)}
            >
              {copiedId === m.id ? '已复制' : '复制'}
            </button>
          </div>
        ))}
        {busy && <div className="chat-busy">{busy}</div>}
      </div>
      {approval && (
        <div className="chat-approval">
          <div className="chat-approval-title">AI 请求执行命令（{approval.host}）</div>
          <pre className="chat-approval-cmd">{approval.command}</pre>
          <div className="chat-approval-hint">等待你的批准，2 分钟内未处理将自动拒绝</div>
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
