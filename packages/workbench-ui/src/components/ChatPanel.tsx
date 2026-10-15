import { useEffect, useRef, useState, type PointerEvent as ReactPointerEvent } from 'react'
import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { agentSocket } from '../api/ws'
import { writeClipboardText } from '../api/clipboard'
import type { AiAskPayload } from '../api/aiBridge'
import type { AiChatSummary, AiHistoryMessage } from '@bgshell/protocol'

interface ChatMsg {
  id: string
  role: 'user' | 'assistant'
  text: string
  thinking?: string
  error?: boolean
  /** AI 工具执行卡片（Trae 风格：可折叠，running 带单条终止按钮） */
  tool?: {
    id: string
    name: string
    detail?: string
    stage: 'running' | 'done' | 'failed' | 'cancelled'
    summary?: string
    output?: string
  }
}

interface Approval {
  reqId: string
  host: string
  command: string
}

/** 敏感信息输入请求（sudo 密码等） */
interface SecretReq {
  reqId: string
  title: string
  prompt: string
  secret: boolean
}

/** 每次「新对话」生成独立 chatId；Agent 侧按 chatId 记忆上下文并把消息落库 SQLite */
function newChatId(): string {
  return `pet-${crypto.randomUUID()}`
}

/** 对话面板尺寸（逻辑像素）；双宿主共用，拖拽调整后持久化 */
export interface ChatPanelSize {
  w: number
  h: number
}

/** 面板默认尺寸（未拖拽过时的 CSS 尺寸；桌宠宿主推导窗口尺寸也用它） */
export const CHAT_PANEL_DEFAULT_SIZE: ChatPanelSize = { w: 360, h: 460 }

const CHAT_SIZE_KEY = 'bgshell.chat-panel-size'
/** 最小 300×340（保证头/列表/输入行可用）；绝对上限 960×840 */
const CHAT_SIZE_MIN = { w: 300, h: 340 }
const CHAT_SIZE_MAX = { w: 960, h: 840 }

/** 按宿主可用范围夹取面板尺寸：max 缺省按所在窗口视口（面板是浮层，不得超出可视区） */
function clampChatSize(w: number, h: number, max?: ChatPanelSize): ChatPanelSize {
  const vw = typeof window === 'undefined' ? CHAT_SIZE_MAX.w : window.innerWidth
  const vh = typeof window === 'undefined' ? CHAT_SIZE_MAX.h : window.innerHeight
  const mw = Math.min(CHAT_SIZE_MAX.w, max?.w ?? vw - 24)
  const mh = Math.min(CHAT_SIZE_MAX.h, max?.h ?? vh - 24)
  return {
    w: Math.round(Math.min(Math.max(w, CHAT_SIZE_MIN.w), Math.max(CHAT_SIZE_MIN.w, mw))),
    h: Math.round(Math.min(Math.max(h, CHAT_SIZE_MIN.h), Math.max(CHAT_SIZE_MIN.h, mh))),
  }
}

/** 读取持久化的面板尺寸（桌宠宿主用它推导初始窗口尺寸；max 缺省按所在窗口视口夹取） */
export function loadChatSize(max?: ChatPanelSize): ChatPanelSize | undefined {
  try {
    const raw = localStorage.getItem(CHAT_SIZE_KEY)
    if (!raw) return undefined
    const v = JSON.parse(raw) as Partial<ChatPanelSize>
    if (Number.isFinite(v.w) && Number.isFinite(v.h)) return clampChatSize(v.w as number, v.h as number, max)
  } catch {
    /* 存储损坏按未设置处理 */
  }
  return undefined
}

function saveChatSize(s: ChatPanelSize): void {
  try {
    localStorage.setItem(CHAT_SIZE_KEY, JSON.stringify(s))
  } catch {
    /* 存储不可用时尺寸只在本次生效 */
  }
}

/** 拖拽调整尺寸的轴向（面板贴右下角，只能向左/上生长） */
type ResizeAxis = 'w' | 'h' | 'wh'

/** RFC3339 UTC → 本地 "MM-DD HH:mm"（历史列表展示） */
function fmtTime(iso: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  const p = (n: number) => String(n).padStart(2, '0')
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`
}

/** 持久化记录 → 界面消息（工具记录还原为终态卡片；瞬态「已终止」提示不还原） */
function historyToMsg(r: AiHistoryMessage): ChatMsg | null {
  if (r.error && (r.content === '已终止' || !r.content)) return null
  return {
    id: `h-${r.id}`,
    role: r.role === 'user' ? 'user' : 'assistant',
    text: r.content ?? '',
    thinking: r.thinking || undefined,
    error: r.error || undefined,
    tool: r.tool ?? undefined,
  }
}

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

/** 终端「问AI」组合成一条 user 消息文本（主机名 + 问题 + 选区代码块） */
function composeAskText(p: AiAskPayload): string {
  const head = p.hostName ? `【来自终端「${p.hostName}」的提问】` : '【来自终端的提问】'
  if (!p.selection.trim()) return `${head}\n${p.question}`
  return `${head}\n${p.question}\n\n终端选区内容：\n\`\`\`\n${p.selection}\n\`\`\``
}

/** AI 对话面板（双宿主）：宠物窗与工作台主窗共用（SSH 精简版的唯一 AI 入口）；与鲸鱼娘对话驱动 SSH 任务，危险命令弹批准卡片 */
export function ChatPanel({
  open,
  onClose,
  incomingAsk,
  onPanelResize,
  sizeMax,
}: {
  open: boolean
  onClose: () => void
  /** 终端「问AI」投递（pet/App 经 Tauri 事件转发；按 uid 去重） */
  incomingAsk?: AiAskPayload | null
  /** 拖拽调整尺寸时上报（桌宠宿主据此联动 Tauri 窗口尺寸；浏览器/工作台宿主可不传） */
  onPanelResize?: (size: ChatPanelSize) => void
  /** 面板尺寸上限（缺省按所在窗口视口；桌宠宿主按屏幕减去宠物区） */
  sizeMax?: ChatPanelSize
}) {
  const [msgs, setMsgs] = useState<ChatMsg[]>([])
  const [input, setInput] = useState('')
  const [busy, setBusy] = useState<string>()
  const [approval, setApproval] = useState<Approval | null>(null)
  const [secretReq, setSecretReq] = useState<SecretReq | null>(null)
  const [secretInput, setSecretInput] = useState('')
  const [copiedId, setCopiedId] = useState<string>()
  const listRef = useRef<HTMLDivElement>(null)
  const streamIdRef = useRef<string | undefined>(undefined)
  // 当前会话 id：ref 供回调/发送读取，「新对话」时整体替换
  const chatIdRef = useRef<string>(newChatId())
  // 流式消息 id 的渲染镜像：ref 供回调读取，state 驱动 ThinkBlock 展开状态
  const [streamingId, setStreamingId] = useState<string>()
  const [expandedTools, setExpandedTools] = useState<Set<string>>(new Set())
  // 历史会话管理抽屉：列表数据 + 待二次确认删除的 chatId
  const [histOpen, setHistOpen] = useState(false)
  const [chatList, setChatList] = useState<AiChatSummary[]>([])
  const [confirmDel, setConfirmDel] = useState<string>()
  // SSH 控制台实时输出（toolCallId → 累积文本）与介入输入框内容
  const [consoleOut, setConsoleOut] = useState<Record<string, string>>({})
  const [consoleInput, setConsoleInput] = useState('')
  // ── 拖拽调整尺寸：面板锚右下角，拖左边/上边/左上角把手改尺寸（松手落盘 + 上报宿主）──
  // 拖拽中直接改面板 DOM 样式（不走 state，避免整面板每帧重渲染）；宿主联动窗口尺寸需节流上报
  const [size, setSize] = useState<ChatPanelSize | undefined>(() => loadChatSize(sizeMax))
  const panelRef = useRef<HTMLDivElement>(null)
  const resizeRef = useRef<{ axis: ResizeAxis; x: number; y: number; w: number; h: number } | null>(null)
  const pendingSizeRef = useRef<ChatPanelSize | null>(null)
  const lastNotifyRef = useRef(0)

  /** 把手按下：记录起点尺寸并捕获指针（后续 move 都定向到该元素） */
  const startResize = (axis: ResizeAxis) => (e: ReactPointerEvent<HTMLDivElement>) => {
    e.preventDefault()
    e.stopPropagation()
    const rect = panelRef.current?.getBoundingClientRect()
    resizeRef.current = { axis, x: e.clientX, y: e.clientY, w: rect?.width ?? 0, h: rect?.height ?? 0 }
    e.currentTarget.setPointerCapture(e.pointerId)
  }

  /** 拖拽中：dx/dy 为负 = 向左/上拖 = 变大（面板锚右下）；节流上报宿主让窗口实时跟手 */
  const moveResize = (e: ReactPointerEvent<HTMLDivElement>) => {
    const s = resizeRef.current
    if (!s) return
    const dw = s.axis === 'h' ? 0 : s.x - e.clientX
    const dh = s.axis === 'w' ? 0 : s.y - e.clientY
    const next = clampChatSize(s.w + dw, s.h + dh, sizeMax)
    pendingSizeRef.current = next
    const el = panelRef.current
    if (el) {
      el.style.width = `${next.w}px`
      el.style.height = `${next.h}px`
    }
    const now = Date.now()
    if (now - lastNotifyRef.current >= 40) {
      lastNotifyRef.current = now
      onPanelResize?.(next)
    }
  }

  /** 松手：提交最终尺寸（state + localStorage + 宿主），结束拖拽 */
  const endResize = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (!resizeRef.current) return
    resizeRef.current = null
    if (e.currentTarget.hasPointerCapture(e.pointerId)) e.currentTarget.releasePointerCapture(e.pointerId)
    const next = pendingSizeRef.current
    pendingSizeRef.current = null
    if (!next) return
    setSize(next)
    saveChatSize(next)
    onPanelResize?.(next)
  }

  // ── 终端「问AI」投递：组合消息直接发送；busy/审批中排队，ai-result 后补发 ──
  const pendingAskRef = useRef<string | null>(null)
  const askUidRef = useRef<string | undefined>(undefined)
  // busy/approval 的 ref 镜像：incomingAsk effect 只依赖 payload，读 ref 避免重触发
  const busyRef = useRef<string | undefined>(undefined)
  const approvalRef = useRef<Approval | null>(null)
  busyRef.current = busy
  approvalRef.current = approval
  // 发送函数的 ref 镜像：onControl 闭包（挂载期注册）通过它调用最新实现
  const sendTextRef = useRef<(text: string) => void>(() => {})

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
      if (frame.t === 'ai-history') {
        // 恢复上次的对话（Agent 重启后记录仍在）：接管服务端返回的 chatId 继续聊
        chatIdRef.current = frame.m.chatId
        streamIdRef.current = undefined
        setStreamingId(undefined)
        setConsoleOut({})
        setExpandedTools(new Set())
        setMsgs(frame.m.messages.map(historyToMsg).filter((m): m is ChatMsg => m !== null))
        return
      }
      if (frame.t === 'ai-chats') {
        // 历史会话列表（打开抽屉 / 删除后刷新）
        setChatList(frame.m.chats)
        return
      }
      if (frame.t === 'ai-status') {
        setBusy(frame.m.stage === 'tool' ? (frame.m.detail ?? '执行工具中…') : '思考中…')
        return
      }
      if (frame.t === 'ai-approval') {
        setBusy(undefined)
        setApproval({ reqId: frame.m.reqId, host: frame.m.host, command: frame.m.command })
        return
      }
      if (frame.t === 'ai-input') {
        // sudo 密码等敏感输入：工具在等这份输入，卡片置顶弹出
        setBusy(undefined)
        setSecretInput('')
        setSecretReq({ reqId: frame.m.reqId, title: frame.m.title, prompt: frame.m.prompt, secret: frame.m.secret })
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
      if (frame.t === 'ai-console') {
        // SSH 控制台实时输出：按 toolCallId 累积（终态帧会带完整 output 并清掉这里）
        const { toolCallId, delta } = frame.m
        setConsoleOut((prev) => ({ ...prev, [toolCallId]: (prev[toolCallId] ?? '') + delta }))
        return
      }
      if (frame.t === 'ai-tool') {
        const ev = frame.m
        // 工具卡片独立成条：先终止当前流式消息，保证「思考 → 工具 → 思考」时序清晰
        streamIdRef.current = undefined
        setStreamingId(undefined)
        if (ev.stage === 'running') {
          // 幂等：同一 toolCallId 的 running 帧只建一张卡片（防御重复帧）
          setMsgs((prev) =>
            prev.some((m) => m.tool?.id === ev.toolCallId)
              ? prev
              : [
                  ...prev,
                  {
                    id: `tool-${ev.toolCallId}`,
                    role: 'assistant',
                    text: '',
                    tool: { id: ev.toolCallId, name: ev.name, detail: ev.detail, stage: 'running' },
                  },
                ],
          )
        } else {
          // 终态：按 toolCallId 更新同一张卡片
          setMsgs((prev) =>
            prev.map((m) =>
              m.tool && m.tool.id === ev.toolCallId
                ? { ...m, tool: { ...m.tool, stage: ev.stage, summary: ev.summary, output: ev.output } }
                : m,
            ),
          )
          // 控制台卡片：终态帧带完整 output 时以服务端为准清掉实时累积；
          // 取消/超时（output 只是一句提示）保留已滚出的实时记录，便于回看
          setConsoleOut((prev) => {
            const live = prev[ev.toolCallId]
            if (live === undefined) return prev
            const final = ev.output ?? ''
            if (final.length >= live.length) {
              const next = { ...prev }
              delete next[ev.toolCallId]
              return next
            }
            return prev
          })
          // 命令被终止后，等待中的审批卡片随之失效
          if (ev.stage === 'cancelled') setApproval(null)
        }
        return
      }
      if (frame.t === 'ai-result') {
        // 先记录流式状态再清（下方判断要用）：本轮文本若已通过流式气泡输出，
        // 结果帧就不重复追加
        const hadStream = streamIdRef.current !== undefined
        streamIdRef.current = undefined
        setStreamingId(undefined)
        setBusy(undefined)
        // 本轮对话已结束：审批/输入超时被服务端自动取消时也要撤下卡片，
        // 否则输入框被 approval/secretReq 永久禁用，表现为「卡死」
        setApproval(null)
        setSecretReq(null)
        setMsgs((prev) => {
          // 兜底：终态帧丢失/整轮被 abort 时，running 卡片不能永远转圈
          const patched = prev.map((m) =>
            m.tool && m.tool.stage === 'running'
              ? { ...m, tool: { ...m.tool, stage: 'done' as const, summary: m.tool.summary ?? '已结束' } }
              : m,
          )
          const tail = patched[patched.length - 1]
          let next = patched
          if (frame.m.text && !hadStream && tail?.role !== 'assistant') {
            next = [...next, { id: crypto.randomUUID(), role: 'assistant' as const, text: frame.m.text }]
          }
          if (!frame.m.ok && frame.m.message) {
            // 查重：同一结果帧被重复分发（多连接/重连竞态）时只保留一条
            const dup = tail?.role === 'assistant' && tail.error && tail.text === frame.m.message
            if (!dup) {
              next = [...next, { id: crypto.randomUUID(), role: 'assistant' as const, text: frame.m.message, error: true }]
            }
          }
          return next
        })
        // 本轮结束后补发排队的「问AI」消息（发起时对话正忙）
        const queued = pendingAskRef.current
        if (queued) {
          pendingAskRef.current = null
          window.setTimeout(() => sendTextRef.current(queued), 50)
        }
      }
    })
    return off
  }, [])

  // 挂载即请求历史（面板常驻挂载、关闭只是不渲染）：恢复上次的对话记录
  useEffect(() => {
    agentSocket
      .connect()
      .then(() => agentSocket.sendControl({ t: 'ai', m: { op: 'history' } }))
      .catch(() => {})
  }, [])

  // 消息变化滚到底部（控制台实时输出也跟随）
  useEffect(() => {
    const el = listRef.current
    if (el) el.scrollTop = el.scrollHeight
  }, [msgs, busy, approval, secretReq, consoleOut])

  // 终端「问AI」：uid 去重（主窗 1.2s 补发会重复到达）；对话忙时排队，空闲直接发送。
  // pet/App 收到事件的同时已 setChatOpen(true)，本 effect 跑在面板渲染之后，滚动手势已就绪
  useEffect(() => {
    if (!incomingAsk?.uid || incomingAsk.uid === askUidRef.current) return
    askUidRef.current = incomingAsk.uid
    const text = composeAskText(incomingAsk)
    if (busyRef.current || approvalRef.current) pendingAskRef.current = text
    else sendTextRef.current(text)
  }, [incomingAsk])

  if (!open) return null

  /** 发送一段已定稿的文本（输入框 send 与终端「问AI」共用） */
  const sendText = (text: string) => {
    if (!text) return
    setMsgs((prev) => [...prev, { id: crypto.randomUUID(), role: 'user', text }])
    setBusy('思考中…')
    agentSocket
      .connect()
      .then(() => agentSocket.sendControl({ t: 'ai', m: { op: 'chat', chatId: chatIdRef.current, text } }))
      .catch((err: Error) => {
        setBusy(undefined)
        setMsgs((prev) => [...prev, { id: crypto.randomUUID(), role: 'assistant', text: `连接 Agent 失败：${err.message}`, error: true }])
      })
  }
  sendTextRef.current = sendText

  const send = () => {
    const text = input.trim()
    if (!text || busy || approval) return
    setInput('')
    sendText(text)
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
    const snapshot = approval
    setMsgs((prev) => [
      ...prev,
      {
        id: crypto.randomUUID(),
        role: 'user',
        text: approved ? `（批准执行：${snapshot.command}）` : '（拒绝执行该命令）',
      },
    ])
    setApproval(null)
    if (approved) setBusy('执行中…')
    void agentSocket
      .connect()
      .then(() =>
        agentSocket.sendControl({ t: 'ai-approve', m: { chatId: chatIdRef.current, reqId: snapshot.reqId, approved } }),
      )
      .catch((err: Error) => {
        // 发送失败（Agent 不可达/断线重连中）：恢复卡片让用户稍后重点，
        // 否则审批结果丢失会被 120s 超时误判为「用户拒绝」
        setApproval(snapshot)
        setBusy(undefined)
        setMsgs((prev) => [
          ...prev,
          { id: crypto.randomUUID(), role: 'assistant', text: `审批结果发送失败：${err.message}（连接恢复后请再点一次）`, error: true },
        ])
      })
  }

  /** 回复敏感输入请求（sudo 密码）：确认发送 / 取消（value 缺省） */
  const answerSecret = (send: boolean) => {
    if (!secretReq) return
    const snapshot = secretReq
    const value = send ? secretInput : undefined
    setSecretReq(null)
    setSecretInput('')
    if (send) {
      setMsgs((prev) => [...prev, { id: crypto.randomUUID(), role: 'user', text: `（已提供 ${snapshot.title}所需的输入）` }])
      setBusy('执行中…')
    } else {
      setMsgs((prev) => [...prev, { id: crypto.randomUUID(), role: 'user', text: `（取消了 ${snapshot.title}）` }])
    }
    void agentSocket
      .connect()
      .then(() =>
        agentSocket.sendControl({
          t: 'ai',
          m: send
            ? { op: 'input-reply', chatId: chatIdRef.current, reqId: snapshot.reqId, value }
            : { op: 'input-reply', chatId: chatIdRef.current, reqId: snapshot.reqId },
        }),
      )
      .catch((err: Error) => {
        // 发送失败：恢复卡片让用户稍后重试（连接恢复前工具会一直等待）
        setSecretReq(snapshot)
        setBusy(undefined)
        setMsgs((prev) => [
          ...prev,
          { id: crypto.randomUUID(), role: 'assistant', text: `输入发送失败：${err.message}（连接恢复后请再提交一次）`, error: true },
        ])
      })
  }

  /** 终止本轮对话/执行：后端 abort 工具循环、撤下审批卡片并回「已终止」 */
  const stopTurn = () => {
    setBusy('正在停止…')
    setApproval(null)
    setSecretReq(null)
    void agentSocket
      .connect()
      .then(() => agentSocket.sendControl({ t: 'ai', m: { op: 'cancel', chatId: chatIdRef.current } }))
      .catch((err: Error) => {
        setBusy(undefined)
        setMsgs((prev) => [
          ...prev,
          { id: crypto.randomUUID(), role: 'assistant', text: `终止请求发送失败：${err.message}`, error: true },
        ])
      })
  }

  /** 只终止这一条执行中的命令：对话继续，AI 会看到「用户终止了该命令」 */
  const cancelTool = (toolCallId: string) => {
    void agentSocket
      .connect()
      .then(() =>
        agentSocket.sendControl({ t: 'ai', m: { op: 'cancel-tool', chatId: chatIdRef.current, toolCallId } }),
      )
      .catch(() => {
        // 连接断开时后端整轮也会因超时收尾；卡片由 ai-result 兜底清理
      })
  }

  /** 介入 SSH 控制台（console_command 运行中）：把文本写进 PTY stdin（"\x03" = Ctrl+C） */
  const sendConsole = (toolCallId: string, text: string) => {
    void agentSocket
      .connect()
      .then(() =>
        agentSocket.sendControl({ t: 'ai', m: { op: 'console-input', chatId: chatIdRef.current, toolCallId, text } }),
      )
      .catch(() => {})
  }

  /** 打开历史会话抽屉：执行中禁止切换/删除（旧轮次的流式帧会串进新会话视图） */
  const openHistory = () => {
    if (busy || approval || secretReq) return
    setConfirmDel(undefined)
    setHistOpen(true)
    void agentSocket
      .connect()
      .then(() => agentSocket.sendControl({ t: 'ai', m: { op: 'history-list' } }))
      .catch(() => {})
  }

  /** 复原一段历史对话：接管服务端返回的 chatId，继续在此会话上聊（上下文由 Agent 从库恢复） */
  const restoreChat = (id: string) => {
    setHistOpen(false)
    void agentSocket
      .connect()
      .then(() => agentSocket.sendControl({ t: 'ai', m: { op: 'history', chatId: id } }))
      .catch(() => {})
  }

  /** 删除历史会话：二次确认防误删；删到当前打开的会话时本地换新 chatId 并清空界面 */
  const deleteChat = (id: string) => {
    if (confirmDel !== id) {
      setConfirmDel(id)
      // 2.5s 未再点击自动撤销确认态
      window.setTimeout(() => setConfirmDel((c) => (c === id ? undefined : c)), 2500)
      return
    }
    setConfirmDel(undefined)
    if (chatIdRef.current === id) {
      chatIdRef.current = newChatId()
      streamIdRef.current = undefined
      setStreamingId(undefined)
      setMsgs([])
      setExpandedTools(new Set())
      setConsoleOut({})
    }
    void agentSocket
      .connect()
      .then(() => agentSocket.sendControl({ t: 'ai', m: { op: 'history-delete', chatId: id } }))
      .catch(() => setHistOpen(false))
  }

  return (
    <div ref={panelRef} className="chat-panel" style={size ? { width: size.w, height: size.h } : undefined}>
      {/* 拖拽调整尺寸把手：面板锚右下角，拖左边/上边/左上角调整大小（最小 300×340） */}
      <div
        className="chat-rz chat-rz-w"
        title="拖动调整宽度"
        onPointerDown={startResize('w')}
        onPointerMove={moveResize}
        onPointerUp={endResize}
        onPointerCancel={endResize}
      />
      <div
        className="chat-rz chat-rz-h"
        title="拖动调整高度"
        onPointerDown={startResize('h')}
        onPointerMove={moveResize}
        onPointerUp={endResize}
        onPointerCancel={endResize}
      />
      <div
        className="chat-rz chat-rz-wh"
        title="拖动调整大小"
        onPointerDown={startResize('wh')}
        onPointerMove={moveResize}
        onPointerUp={endResize}
        onPointerCancel={endResize}
      />
      <div className="chat-head">
        <span>鲸鱼娘 · AI 助手</span>
        <button type="button" className="chat-close" title="开启新对话（历史记录保留在数据库中）" onClick={() => {
          const old = chatIdRef.current
          // 结束旧会话：终止进行中的任务并清掉 Agent 侧记忆（记录本身保留，可随时查阅）
          void agentSocket
            .connect()
            .then(() => agentSocket.sendControl({ t: 'ai', m: { op: 'reset', chatId: old } }))
            .catch(() => {})
          chatIdRef.current = newChatId()
          streamIdRef.current = undefined
          setStreamingId(undefined)
          setMsgs([])
          setBusy(undefined)
          setApproval(null)
          setExpandedTools(new Set())
          setConsoleOut({})
        }}>
          新对话
        </button>
        <button type="button" className="chat-close" title="查看与管理历史对话（复原 / 删除）" onClick={openHistory} disabled={!!busy || !!approval || !!secretReq}>
          历史
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
            {m.tool ? (
              m.tool.name === 'console_command' ? (
                /* SSH 控制台卡片：终端样式（与 MCP 工具卡片区分），实时输出 + 用户介入输入 */
                <div className={`chat-tool chat-tool-console chat-tool-console-${m.tool.stage}`}>
                  <button type="button" className="chat-tool-bar">
                    {m.tool.stage === 'running' ? (
                      <span className="chat-tool-spinner" aria-label="执行中" />
                    ) : (
                      <span className={`chat-tool-dot chat-tool-dot-${m.tool.stage}`} aria-hidden />
                    )}
                    <span className="chat-tool-name">SSH 控制台</span>
                    {m.tool.detail && <code className="chat-tool-detail">{m.tool.detail}</code>}
                    <span className={`chat-tool-summary chat-tool-summary-${m.tool.stage}`}>
                      {m.tool.stage === 'running' ? '运行中…' : (m.tool.summary ?? '')}
                    </span>
                  </button>
                  {m.tool.stage === 'running' && (
                    <button type="button" className="chat-tool-stop" title="关闭这个控制台会话（对话继续）" onClick={() => cancelTool(m.tool!.id)}>
                      终止
                    </button>
                  )}
                  <pre className="chat-console-out">{consoleOut[m.tool.id] ?? m.tool.output ?? ''}</pre>
                  {m.tool.stage === 'running' && (
                    <div className="chat-console-input">
                      <input
                        className="chat-console-field"
                        value={consoleInput}
                        placeholder="介入控制台：输入后回车发送（与 AI 共用同一会话）"
                        onChange={(e) => setConsoleInput(e.target.value)}
                        onKeyDown={(e) => {
                          if (e.key === 'Enter' && consoleInput) {
                            sendConsole(m.tool!.id, `${consoleInput}\n`)
                            setConsoleInput('')
                          }
                        }}
                      />
                      <button
                        type="button"
                        className="chat-console-ctrl"
                        title="发送 Ctrl+C（中断当前命令）"
                        onClick={() => sendConsole(m.tool!.id, '\x03')}
                      >
                        ^C
                      </button>
                    </div>
                  )}
                </div>
              ) : (
                <div
                  className={`chat-tool chat-tool-${m.tool.stage}${m.tool.stage === 'failed' || m.tool.stage === 'cancelled' ? ' chat-tool-fail' : ''}`}
                >
                  <button
                    type="button"
                    className="chat-tool-bar"
                    title={m.tool.output ? '展开/收起输出' : undefined}
                    onClick={() =>
                      m.tool?.output &&
                      setExpandedTools((prev) => {
                        const next = new Set(prev)
                        if (next.has(m.tool!.id)) next.delete(m.tool!.id)
                        else next.add(m.tool!.id)
                        return next
                      })
                    }
                  >
                    {m.tool.stage === 'running' ? (
                      <span className="chat-tool-spinner" aria-label="执行中" />
                    ) : (
                      <span className={`chat-tool-dot chat-tool-dot-${m.tool.stage}`} aria-hidden />
                    )}
                    <span className="chat-tool-name">{m.tool.name}</span>
                    {m.tool.detail && <code className="chat-tool-detail">{m.tool.detail}</code>}
                    <span className={`chat-tool-summary chat-tool-summary-${m.tool.stage}`}>
                      {m.tool.stage === 'running' ? '执行中…' : (m.tool.summary ?? '')}
                    </span>
                    {m.tool.output && (
                      <span className={`chat-tool-caret${expandedTools.has(m.tool.id) ? ' chat-tool-caret-open' : ''}`} aria-hidden>
                        ▸
                      </span>
                    )}
                  </button>
                  {m.tool.stage === 'running' && (
                    <button type="button" className="chat-tool-stop" title="终止这条命令（对话继续）" onClick={() => cancelTool(m.tool!.id)}>
                      终止
                    </button>
                  )}
                  {m.tool.output && expandedTools.has(m.tool.id) && (
                    <pre className="chat-tool-output">{m.tool.output}</pre>
                  )}
                </div>
              )
            ) : m.error || m.role === 'user' ? (
              // 错误详情/用户输入保持纯文本（错误串里的路径下划线等不该被 Md 误格式化）
              m.text
            ) : (
              <div className="chat-md">
                <ReactMarkdown remarkPlugins={[remarkGfm]}>{m.text}</ReactMarkdown>
              </div>
            )}
            {!m.tool && (
              <button
                type="button"
                className="chat-copy"
                title="复制这条消息"
                onClick={() => copyMsg(m)}
              >
                {copiedId === m.id ? '已复制' : '复制'}
              </button>
            )}
          </div>
        ))}
        {busy && (
          <div className="chat-busy">
            {busy}
            {/* 「正在停止…」阶段隐藏按钮：终止在后端是幂等的，但连点会在界面上
                产生多条「已终止」气泡，等 ai-result 清掉 busy 后自然恢复 */}
            {busy !== '正在停止…' && (
              <button type="button" className="chat-stop" title="终止本轮对话与执行" onClick={stopTurn}>
                停止
              </button>
            )}
          </div>
        )}
      </div>
      {histOpen && (
        <div className="chat-hist">
          <div className="chat-head">
            <span>历史对话</span>
            <button type="button" className="chat-close" onClick={() => setHistOpen(false)}>
              ×
            </button>
          </div>
          <div className="chat-hist-list">
            {chatList.length === 0 && <div className="chat-empty">还没有历史对话</div>}
            {chatList.map((c) => (
              <div
                key={c.chatId}
                className={`chat-hist-item${confirmDel === c.chatId ? ' chat-hist-item-danger' : ''}`}
              >
                <button
                  type="button"
                  className="chat-hist-main"
                  title="复原这段对话（在原会话上继续聊）"
                  onClick={() => restoreChat(c.chatId)}
                >
                  <span className="chat-hist-preview">{c.preview || '（无文字内容）'}</span>
                  <span className="chat-hist-meta">
                    {fmtTime(c.lastAt)} · {c.msgCount} 条消息
                    {c.chatId === chatIdRef.current ? ' · 当前会话' : ''}
                  </span>
                </button>
                <button
                  type="button"
                  className={`chat-hist-del${confirmDel === c.chatId ? ' chat-hist-del-confirm' : ''}`}
                  title={confirmDel === c.chatId ? '再次点击确认删除（不可恢复）' : '删除这段对话'}
                  onClick={() => deleteChat(c.chatId)}
                >
                  {confirmDel === c.chatId ? '确认' : '删除'}
                </button>
              </div>
            ))}
          </div>
        </div>
      )}
      {approval && (
        <div className="chat-approval">
          <div className="chat-approval-title">AI 请求执行命令（{approval.host}）</div>
          <pre className="chat-approval-cmd">{approval.command}</pre>
          <div className="chat-approval-hint">等待你的批准，2 分钟内未处理将自动拒绝</div>
          <div className="chat-approval-btns">
            <button type="button" className="chat-kill" title="终止本轮对话（撤下审批并停止执行）" onClick={stopTurn}>
              终止本轮
            </button>
            <button type="button" className="chat-no" onClick={() => answerApproval(false)}>
              拒绝
            </button>
            <button type="button" className="chat-ok" onClick={() => answerApproval(true)}>
              批准执行
            </button>
          </div>
        </div>
      )}
      {secretReq && (
        <div className="chat-approval">
          <div className="chat-approval-title">{secretReq.title}（{secretReq.secret ? '密码输入' : '信息输入'}）</div>
          <div className="chat-approval-hint">{secretReq.prompt}</div>
          <input
            className="chat-input"
            type={secretReq.secret ? 'password' : 'text'}
            value={secretInput}
            autoFocus
            placeholder={secretReq.secret ? '输入密码…' : '输入内容…'}
            onChange={(e) => setSecretInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && secretInput) answerSecret(true)
            }}
          />
          <div className="chat-approval-hint">等待你的输入，5 分钟内未处理将自动取消（密码仅用于本次 sudo，不会展示或存盘）</div>
          <div className="chat-approval-btns">
            <button type="button" className="chat-kill" title="终止本轮对话（撤销输入请求）" onClick={stopTurn}>
              终止本轮
            </button>
            <button type="button" className="chat-no" onClick={() => answerSecret(false)}>
              取消
            </button>
            <button
              type="button"
              className="chat-ok"
              disabled={!secretInput}
              onClick={() => answerSecret(true)}
            >
              提交
            </button>
          </div>
        </div>
      )}
      <div className="chat-input-row">
        <input
          className="chat-input"
          value={input}
          placeholder={busy ? '鲸鱼娘正在忙…' : '对鲸鱼娘说点什么…'}
          disabled={!!busy || !!approval || !!secretReq}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter') send()
          }}
        />
        <button type="button" className="chat-send" disabled={!!busy || !!approval || !!secretReq || !input.trim()} onClick={send}>
          发送
        </button>
      </div>
    </div>
  )
}
