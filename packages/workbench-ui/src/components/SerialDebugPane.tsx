import { useCallback, useEffect, useMemo, useRef, useState, type CSSProperties } from 'react'
import { Badge, Button, Collapse, Input, InputNumber, Menu, Modal, Select, Space, Switch, Tag, Tooltip, message } from 'antd'
import type { MenuProps } from 'antd'
import {
  CaretRightOutlined,
  ClearOutlined,
  CopyOutlined,
  DeleteOutlined,
  DownloadOutlined,
  EditOutlined,
  PauseCircleOutlined,
  PlayCircleOutlined,
  ReloadOutlined,
  RightOutlined,
  RobotOutlined,
} from '@ant-design/icons'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import {
  DataFrameType,
  type SerialCtlOp,
  type SerialCtlResult,
  type SerialSignals,
} from '@bgshell/protocol'
import { agentSocket } from '../api/ws'
import { askAi } from '../api/askAi'
import { requestAiAsk } from '../api/aiBridge'
import { readClipboardText, writeClipboardText } from '../api/clipboard'
import { SERIAL_AI_SYSTEM, SerialAiModal } from './SerialAiModal'

const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window

/** Hex 监视条目（时间戳 + 方向 + 原始字节，Hex/文本渲染在展示时合成） */
interface MonitorEntry {
  id: number
  ts: number
  dir: 'rx' | 'tx'
  data: Uint8Array
}

/** 快捷发送预设（localStorage 按端口名分组持久化） */
interface SendPreset {
  id: string
  label: string
  content: string
  hex: boolean
  /** 行尾追加（串口设备指令多需 CR 结尾） */
  ending: 'none' | 'cr' | 'lf' | 'crlf'
}

const MONITOR_MAX = 2000
const PRESET_KEY = (port: string) => `bgshell.serial.presets.${port}`
const PANEL_KEY = 'bgshell.serial.panelOpen'

function fmtTime(ts: number): string {
  const d = new Date(ts)
  const p = (n: number, w = 2) => String(n).padStart(w, '0')
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`
}

function toHex(data: Uint8Array, limit = 256): string {
  return [...data.slice(0, limit)].map((b) => b.toString(16).padStart(2, '0')).join(' ')
}

function toAscii(data: Uint8Array, limit = 256): string {
  return [...data.slice(0, limit)]
    .map((b) => (b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : b === 0x0d ? '\\r' : b === 0x0a ? '\\n' : '·'))
    .join('')
}

/** 文本 → 字节（Hex 模式解析 "a1 b2" / "a1b2"；文本模式 UTF-8 + 行尾） */
function encodePayload(content: string, hex: boolean, ending: SendPreset['ending']): Uint8Array | null {
  const tail = ending === 'cr' ? [0x0d] : ending === 'lf' ? [0x0a] : ending === 'crlf' ? [0x0d, 0x0a] : []
  if (hex) {
    const clean = content.replace(/\s+/g, '')
    if (clean.length === 0 || clean.length % 2 !== 0 || !/^[0-9a-fA-F]+$/.test(clean)) return null
    const bytes = new Uint8Array(clean.length / 2 + tail.length)
    for (let i = 0; i < clean.length; i += 2) bytes[i / 2] = parseInt(clean.slice(i, i + 2), 16)
    bytes.set(tail, clean.length / 2)
    return bytes
  }
  const body = new TextEncoder().encode(content)
  const bytes = new Uint8Array(body.length + tail.length)
  bytes.set(body, 0)
  bytes.set(tail, body.length)
  return bytes
}

export interface SerialDebugPaneProps {
  sessionId: string
  /** 端口名（预设按端口分组持久化 + 状态条显示） */
  portName: string
  baudRate: number
  /** 打开时 DTR/RTS 预设电平（session-opened 回传） */
  initDtr: boolean
  initRts: boolean
}

/** 串口调试工作台（双栏）：左侧控制台 xterm + 右侧折叠调试面板
 *  （Hex 监视 / 快捷发送 / 信号控制 / 统计）。收发路径与 shell 会话同构：
 *  终端输入与快捷发送共用 DF_STDIN，设备数据经 DF_STDOUT 同时喂控制台与 Hex 监视。 */
export function SerialDebugPane({ sessionId, portName, baudRate, initDtr, initRts }: SerialDebugPaneProps) {
  const hostRef = useRef<HTMLDivElement>(null)
  const termRef = useRef<Terminal | null>(null)
  const entryIdRef = useRef(0)
  const monitorRef = useRef<HTMLDivElement>(null)
  // pushEntry 回调里的暂停/记录开关经 ref 读取（回调不随开关变化重建）
  const pausedRef = useRef(false)
  const recordingRef = useRef(false)
  const recordRef = useRef<MonitorEntry[]>([])

  // ── 会话状态 ────────────────────────────────────────────────────────────
  const [connected, setConnected] = useState(false)
  const [closedReason, setClosedReason] = useState<string>()
  const [baud, setBaud] = useState(baudRate)
  // ── 调试面板 ────────────────────────────────────────────────────────────
  const [panelOpen, setPanelOpen] = useState(() => localStorage.getItem(PANEL_KEY) !== '0')
  const [entries, setEntries] = useState<MonitorEntry[]>([])
  const [paused, setPaused] = useState(false)
  const [dirFilter, setDirFilter] = useState<'all' | 'rx' | 'tx'>('all')
  const [localEcho, setLocalEcho] = useState(true)
  pausedRef.current = paused
  // ── 发送 ────────────────────────────────────────────────────────────────
  const [presets, setPresets] = useState<SendPreset[]>([])
  const [presetLabel, setPresetLabel] = useState('')
  const [draft, setDraft] = useState('')
  const [draftHex, setDraftHex] = useState(false)
  const [draftEnding, setDraftEnding] = useState<SendPreset['ending']>('cr')
  const [repeatOn, setRepeatOn] = useState(false)
  const [repeatMs, setRepeatMs] = useState(1000)
  // ── 信号控制 ────────────────────────────────────────────────────────────
  const [dtr, setDtr] = useState(initDtr)
  const [rts, setRts] = useState(initRts)
  const [breakMs, setBreakMs] = useState(250)
  const [signals, setSignals] = useState<SerialSignals>()
  // ── 统计（速率 = 近 1s 内字节数）─────────────────────────────────────────
  const [stat, setStat] = useState({ rx: 0, tx: 0, rxRate: 0, txRate: 0 })
  const statRef = useRef({
    rx: 0,
    tx: 0,
    rxWin: [] as Array<[number, number]>, // [ts, len]
    txWin: [] as Array<[number, number]>,
  })
  // ── 控制台右键菜单（term-menu，快照选区打开时定格）─────────────────────────
  const [termMenu, setTermMenu] = useState<{
    x: number
    y: number
    hasSelection: boolean
    selText: string | null
  } | null>(null)
  // 问 AI 卡片（term-ask-card 浮层）：选区快照 + 用户问题
  const [ask, setAsk] = useState<{ selection: string; question: string } | null>(null)
  // ── Hex 条目 / 预设条目右键（antd Menu + 共享遮罩）─────────────────────────
  const [entryMenu, setEntryMenu] = useState<{ x: number; y: number; entry: MonitorEntry }>()
  const [presetMenu, setPresetMenu] = useState<{ x: number; y: number; preset: SendPreset }>()
  // ── AI 结果弹窗（parse/generate 两动作共用）+ 序号防串台 ───────────────────
  const [ai, setAi] = useState<{
    action: 'parse' | 'generate'
    loading: boolean
    text?: string
    error?: string
    /** 生成动作发起时用户选的行尾（回填发送框时用） */
    genEnding?: SendPreset['ending']
  }>()
  const aiSeqRef = useRef(0)
  // AI 生成指令输入弹窗
  const [genOpen, setGenOpen] = useState(false)
  const [genDesc, setGenDesc] = useState('')
  const [genHex, setGenHex] = useState(false)
  const [genEnding, setGenEnding] = useState<SendPreset['ending']>('cr')
  const [genWithContext, setGenWithContext] = useState(true)
  // 预设编辑弹窗（编辑期间持有拷贝，保存时按 id 替换）
  const [editPreset, setEditPreset] = useState<SendPreset | null>(null)

  /** 单条收发记录的日志行格式（「复制整行」与记录导出共用） */
  const formatLogLine = (e: MonitorEntry): string =>
    `${fmtTime(e.ts)} ${e.dir.toUpperCase()}  ${toHex(e.data, 4096)}  ${toAscii(e.data, 4096)}`

  const pushEntry = useCallback((dir: 'rx' | 'tx', data: Uint8Array) => {
    if (pausedRef.current && dir === 'rx') return
    const entry: MonitorEntry = { id: entryIdRef.current++, ts: Date.now(), dir, data: data.slice() }
    if (recordingRef.current) recordRef.current.push(entry)
    setEntries((prev) => {
      const next = prev.length >= MONITOR_MAX ? prev.slice(prev.length - MONITOR_MAX + 1) : prev.slice()
      next.push(entry)
      return next
    })
    const s = statRef.current
    const len = data.byteLength
    if (dir === 'rx') {
      s.rx += len
      s.rxWin.push([entry.ts, len])
    } else {
      s.tx += len
      s.txWin.push([entry.ts, len])
    }
  }, [])

  // pushEntry / localEcho 经 ref 供 xterm 回调读取：开关变化不重建终端（保留屏幕内容）
  const pushRef = useRef(pushEntry)
  pushRef.current = pushEntry
  const echoRef = useRef(localEcho)
  echoRef.current = localEcho

  // ── 控制台（xterm）────────────────────────────────────────────────────────
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
    term.open(hostRef.current!)
    fit.fit()
    term.focus()

    const offData = agentSocket.onData((sid, type, payload) => {
      if (sid !== sessionId || type !== DataFrameType.Stdout) return
      term.write(payload)
      pushRef.current('rx', payload)
    })
    // 本地回显：串口设备通常不 echo，开启时键盘输入与快捷发送在控制台本地显示
    const dataSub = term.onData((d) => {
      const bytes = new TextEncoder().encode(d)
      agentSocket.sendInput(sessionId, bytes)
      pushRef.current('tx', bytes)
      if (echoRef.current) term.write(d)
    })
    const resizeTo = () => fit.fit()
    const ro = new ResizeObserver(resizeTo)
    ro.observe(hostRef.current!)
    return () => {
      offData()
      dataSub.dispose()
      ro.disconnect()
      term.dispose()
      termRef.current = null
    }
  }, [sessionId])

  // ── 会话生命周期 / 控制回包 ──────────────────────────────────────────────
  useEffect(() => {
    const off = agentSocket.onControl((frame) => {
      if (!('s' in frame) || frame.s !== sessionId) return
      if (frame.t === 'session-opened') {
        setConnected(true)
        setDtr(frame.m.dtr ?? true)
        setRts(frame.m.rts ?? true)
        agentSocket.sendControl({ t: 'serial-ctl', s: sessionId, m: { op: 'signals' } })
      } else if (frame.t === 'serial-ctl-result') {
        const m: SerialCtlResult = frame.m
        if (m.op === 'signals' && m.signals) setSignals(m.signals)
        if (!m.ok && m.message) message.warning(m.message)
      } else if (frame.t === 'session-closed') {
        // open-failed 也走此帧（App 会移除页签，此处兜底记录原因 + 停定时重发）
        setConnected(false)
        setClosedReason(frame.m.reason)
        setRepeatOn(false)
      }
    })
    return () => off()
  }, [sessionId])

  // ── 统计速率（1s 滑动窗口）───────────────────────────────────────────────
  useEffect(() => {
    const timer = window.setInterval(() => {
      const now = Date.now()
      const s = statRef.current
      s.rxWin = s.rxWin.filter(([ts]) => ts > now - 1000)
      s.txWin = s.txWin.filter(([ts]) => ts > now - 1000)
      setStat({
        rx: s.rx,
        tx: s.tx,
        rxRate: s.rxWin.reduce((a, [, len]) => a + len, 0),
        txRate: s.txWin.reduce((a, [, len]) => a + len, 0),
      })
    }, 1000)
    return () => window.clearInterval(timer)
  }, [])

  // ── 预设持久化 ──────────────────────────────────────────────────────────
  useEffect(() => {
    try {
      const raw = localStorage.getItem(PRESET_KEY(portName))
      if (raw) setPresets(JSON.parse(raw) as SendPreset[])
    } catch {
      /* 损坏的本地数据：忽略并用空预设 */
    }
  }, [portName])
  const savePresets = useCallback(
    (next: SendPreset[]) => {
      setPresets(next)
      localStorage.setItem(PRESET_KEY(portName), JSON.stringify(next))
    },
    [portName],
  )

  // ── 发送（控制台键盘 / 快捷发送 / 定时重发共用同一条 stdin 通道）──────────
  const send = useCallback(
    (bytes: Uint8Array) => {
      agentSocket.sendInput(sessionId, bytes)
      pushEntry('tx', bytes)
      if (localEcho) termRef.current?.write(bytes)
    },
    [sessionId, pushEntry, localEcho],
  )

  const sendPreset = useCallback(
    (p: SendPreset) => {
      const bytes = encodePayload(p.content, p.hex, p.ending)
      if (!bytes) {
        message.warning('Hex 内容格式不正确（需偶数个十六进制字符）')
        return
      }
      send(bytes)
    },
    [send],
  )

  /** 把当前输入框内容存为预设（标签缺省按序号） */
  const savePreset = () => {
    const label = presetLabel.trim() || `预设 ${presets.length + 1}`
    savePresets([...presets, { id: crypto.randomUUID(), label, content: draft, hex: draftHex, ending: draftEnding }])
    setPresetLabel('')
  }

  // 定时重发：当前输入框内容按其格式/行尾重复发送（心跳/轮询调试）
  const repeatRef = useRef<number | undefined>(undefined)
  const draftRef = useRef({ content: '', hex: false, ending: 'cr' as SendPreset['ending'] })
  draftRef.current = { content: draft, hex: draftHex, ending: draftEnding }
  useEffect(() => {
    if (!repeatOn) {
      if (repeatRef.current) window.clearInterval(repeatRef.current)
      repeatRef.current = undefined
      return
    }
    repeatRef.current = window.setInterval(() => {
      const d = draftRef.current
      const bytes = encodePayload(d.content, d.hex, d.ending)
      if (bytes) send(bytes)
    }, repeatMs)
    return () => {
      if (repeatRef.current) window.clearInterval(repeatRef.current)
    }
  }, [repeatOn, repeatMs, send])

  // ── 信号控制 ────────────────────────────────────────────────────────────
  const ctl = useCallback(
    (op: SerialCtlOp) => agentSocket.sendControl({ t: 'serial-ctl', s: sessionId, m: op }),
    [sessionId],
  )
  const toggleDtr = (level: boolean) => {
    setDtr(level)
    ctl({ op: 'dtr', level })
  }
  const toggleRts = (level: boolean) => {
    setRts(level)
    ctl({ op: 'rts', level })
  }
  /** Arduino 自动复位：DTR 脉冲（拉低→恢复），开发板收到复位进 bootloader */
  const pulseDtr = () => {
    ctl({ op: 'dtr', level: false })
    window.setTimeout(() => {
      ctl({ op: 'dtr', level: true })
      setDtr(true)
    }, 100)
  }
  /** ESP32 进下载模式：IO0 拉低（RTS）+ 复位（DTR），随后释放 RTS 保持 IO0 低 */
  const esp32Download = () => {
    ctl({ op: 'rts', level: true })
    ctl({ op: 'dtr', level: false })
    window.setTimeout(() => {
      ctl({ op: 'dtr', level: true })
      setDtr(true)
    }, 100)
  }

  // ── 记录（收集帧日志，停止时选择保存位置写盘）────────────────────────────
  const [recording, setRecording] = useState(false)
  recordingRef.current = recording
  const toggleRecord = () => {
    if (!recording) {
      recordRef.current = []
      setRecording(true)
      message.success('开始记录收发数据，停止时选择保存位置')
      return
    }
    setRecording(false)
    const log = recordRef.current
    if (log.length === 0) {
      message.info('本次记录没有捕获到数据')
      return
    }
    const text = log.map(formatLogLine).join('\r\n')
    void (async () => {
      if (isTauri) {
        const { save } = await import('@tauri-apps/plugin-dialog')
        const { invoke } = await import('@tauri-apps/api/core')
        const path = await save({ defaultPath: `${portName}-log.txt`, filters: [{ name: '文本', extensions: ['txt'] }] })
        if (!path) return
        try {
          await invoke('write_text_file', { path, contents: text })
          message.success(`记录已保存到 ${path}`)
        } catch (e) {
          message.error(`保存失败：${String(e)}`)
        }
        return
      }
      const a = document.createElement('a')
      a.href = URL.createObjectURL(new Blob([text], { type: 'text/plain' }))
      a.download = `${portName}-log.txt`
      a.click()
      URL.revokeObjectURL(a.href)
    })()
  }

  // 自动滚动到底部（暂停时不跟随）
  useEffect(() => {
    if (paused || !monitorRef.current) return
    monitorRef.current.scrollTop = monitorRef.current.scrollHeight
  }, [entries, paused])

  // ── 控制台右键菜单 ─────────────────────────────────────────────────────
  // 菜单打开期间：点击菜单外 / Esc / 滚动 / 窗口失焦或缩放 → 关闭（仿 TerminalPane）
  useEffect(() => {
    if (!termMenu) return
    const close = () => setTermMenu(null)
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
  }, [termMenu])

  const termMenuStyle: CSSProperties | undefined = termMenu
    ? {
        left: Math.max(8, Math.min(termMenu.x, window.innerWidth - 200)),
        top: Math.max(8, Math.min(termMenu.y, window.innerHeight - 300)),
      }
    : undefined

  /** 复制选区（右键菜单）：写剪贴板后清选区回焦点 */
  const copySelection = () => {
    const sel = termRef.current?.getSelection()
    if (sel) void writeClipboardText(sel).then(() => message.success('已复制'))
    termRef.current?.clearSelection()
    termRef.current?.focus()
    setTermMenu(null)
  }

  /** 粘贴即发送：term.paste 触发 onData → sendInput，tx 记录/统计/本地回显走全链路 */
  const pasteFromClipboard = async () => {
    try {
      const text = await readClipboardText()
      if (text) termRef.current?.paste(text)
    } catch {
      /* 剪贴板权限异常：静默 */
    }
    termRef.current?.focus()
    setTermMenu(null)
  }

  /** 问 AI 提交（控制台选区 + 问题 → AiSidebar 对话面板） */
  const submitAsk = () => {
    if (!ask || !ask.question.trim()) return
    void requestAiAsk({
      hostName: `${portName}@${baud}`,
      selection: ask.selection,
      question: ask.question.trim(),
    })
    setAsk(null)
    message.success('已发给 AI，回答在右侧 AI 助手查看')
  }

  // ── AI 动作（askAi 一次性问答，序号防串台：旧请求回包丢弃）──────────────
  const startAiParse = (entry: MonitorEntry) => {
    const seq = ++aiSeqRef.current
    setAi({ action: 'parse', loading: true })
    const dir = entry.dir === 'rx' ? '接收（RX，设备 → 主机）' : '发送（TX，主机 → 设备）'
    const prompt =
      `请分析下面这条串口数据：逐字节标注偏移与含义，判断协议类型与帧结构` +
      `（帧头/长度域/命令字/数据域/校验），若存在校验字节请指出疑似校验算法（如异或/累加和/CRC8/CRC16-Modbus）与计算范围；` +
      `若是 ASCII 行协议、AT 指令或 NMEA 语句请说明语义。\n\n` +
      `上下文：${dir}，时间 ${fmtTime(entry.ts)}，共 ${entry.data.length} 字节${entry.data.length > 1024 ? '（超长已截断）' : ''}。\n` +
      `HEX：\n${toHex(entry.data, 1024)}\n\n` +
      `ASCII：\n${toAscii(entry.data, 1024)}`
    void askAi(prompt, SERIAL_AI_SYSTEM).then((r) => {
      if (seq !== aiSeqRef.current) return
      setAi((prev) =>
        prev
          ? {
              ...prev,
              loading: false,
              text: r.ok ? r.text : undefined,
              error: r.ok ? undefined : r.message ?? 'AI 请求失败，请重试',
            }
          : prev,
      )
    })
  }

  /** AI 生成指令（输入弹窗收集需求 → 生成 → SerialAiModal 展示 → 填入发送框） */
  const startAiGenerate = () => {
    const desc = genDesc.trim()
    if (!desc) return
    setGenOpen(false)
    const seq = ++aiSeqRef.current
    const ending = genEnding
    setAi({ action: 'generate', loading: true, genEnding: ending })
    let context = ''
    if (genWithContext && entries.length > 0) {
      const lines = entries.slice(-6).map((e) => {
        const hex = toHex(e.data, 64)
        return `[${fmtTime(e.ts)}] ${e.dir.toUpperCase()} ${hex}${e.data.length > 64 ? '…' : ''}`
      })
      context = `\n\n最近收发数据（供参考的设备行为，判断指令格式与响应习惯用）：\n${lines.join('\n').slice(0, 2000)}`
    }
    const prompt =
      `请生成一条可直接通过串口发送的指令。\n\n` +
      `需求：${desc}\n` +
      `输出格式：${genHex ? '十六进制字节串，字节间用空格分隔（如 a1 b2 0d）' : '纯文本'}。\n` +
      `注意：不要包含行尾字符（CR/LF），发送时会按行尾设置自动追加。${context}\n\n` +
      `只输出指令本身：${genHex ? '务必用 ```hex 代码块' : '用 ```text 代码块'}给出，不要任何解释。`
    void askAi(prompt, SERIAL_AI_SYSTEM).then((r) => {
      if (seq !== aiSeqRef.current) return
      setAi((prev) =>
        prev
          ? {
              ...prev,
              loading: false,
              text: r.ok ? r.text : undefined,
              error: r.ok ? undefined : r.message ?? 'AI 请求失败，请重试',
            }
          : prev,
      )
    })
  }

  /** AI 生成结果填入发送框（内容 + hex 开关 + 行尾三态联动） */
  const insertGenerated = (b: { content: string; hex: boolean }) => {
    setDraft(b.content)
    setDraftHex(b.hex)
    setDraftEnding(ai?.genEnding ?? 'cr')
    setAi(undefined)
    message.success('已填入发送框')
  }

  // ── Hex 条目 / 预设条目右键（antd Menu + 共享遮罩）──────────────────────
  const closeMenus = () => {
    setEntryMenu(undefined)
    setPresetMenu(undefined)
  }

  const entryMenuItems: MenuProps['items'] = useMemo(() => {
    if (!entryMenu) return []
    return [
      { key: 'hex', icon: <CopyOutlined />, label: '复制 Hex' },
      { key: 'ascii', icon: <CopyOutlined />, label: '复制 ASCII' },
      { key: 'line', icon: <CopyOutlined />, label: '复制整行日志' },
      { key: 'ts', icon: <CopyOutlined />, label: '复制时间戳' },
      { type: 'divider' as const },
      { key: 'ai', icon: <RobotOutlined />, label: 'AI 解析此数据' },
    ]
  }, [entryMenu])

  const onEntryMenuClick: MenuProps['onClick'] = ({ key }) => {
    if (!entryMenu) return
    const e = entryMenu.entry
    let text = ''
    if (key === 'hex') text = toHex(e.data, 4096)
    else if (key === 'ascii') text = toAscii(e.data, 4096)
    else if (key === 'line') text = formatLogLine(e)
    else if (key === 'ts') text = fmtTime(e.ts)
    else if (key === 'ai') {
      closeMenus()
      startAiParse(e)
      return
    }
    closeMenus()
    if (text) void writeClipboardText(text).then(() => message.success('已复制'))
  }

  const presetMenuItems: MenuProps['items'] = useMemo(() => {
    if (!presetMenu) return []
    return [
      { key: 'send', icon: <CaretRightOutlined />, label: '立即发送' },
      { key: 'edit', icon: <EditOutlined />, label: '编辑预设' },
      { key: 'copy', icon: <CopyOutlined />, label: '复制内容' },
      { key: 'ai', icon: <RobotOutlined />, label: 'AI 生成指令' },
      { type: 'divider' as const },
      { key: 'del', icon: <DeleteOutlined />, label: '删除', danger: true },
    ]
  }, [presetMenu])

  const onPresetMenuClick: MenuProps['onClick'] = ({ key }) => {
    if (!presetMenu) return
    const p = presetMenu.preset
    closeMenus()
    if (key === 'send') sendPreset(p)
    else if (key === 'edit') setEditPreset({ ...p })
    else if (key === 'copy') void writeClipboardText(p.content).then(() => message.success('已复制'))
    else if (key === 'ai') setGenOpen(true)
    else if (key === 'del') {
      savePresets(presets.filter((x) => x.id !== p.id))
      message.success(`已删除预设「${p.label}」`)
    }
  }

  /** 保存预设编辑（hex 内容先经 encodePayload 试算校验） */
  const saveEditPreset = () => {
    if (!editPreset) return
    if (editPreset.hex && !encodePayload(editPreset.content, true, 'none')) {
      message.warning('Hex 内容格式不正确（需偶数个十六进制字符）')
      return
    }
    savePresets(presets.map((x) => (x.id === editPreset.id ? editPreset : x)))
    setEditPreset(null)
  }

  const filtered = useMemo(
    () => (dirFilter === 'all' ? entries : entries.filter((e) => e.dir === dirFilter)),
    [entries, dirFilter],
  )

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%', minHeight: 0 }}>
      {/* 状态条：连接状态 · 参数 · 信号线 · 速率 · 调试开关 */}
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: 12,
          padding: '6px 12px',
          background: 'var(--bg-base)',
          borderBottom: '1px solid var(--border-subtle)',
          fontSize: 12,
          flexWrap: 'wrap',
        }}
      >
        <Badge status={connected ? 'success' : 'error'} text={connected ? `${portName} 已连接` : `${portName} ${closedReason ? `已断开（${closedReason}）` : '未连接'}`} />
        <Tag color="blue">{baud} 8N1</Tag>
        <Space size={4}>
          <Tag color={dtr ? 'green' : 'default'}>DTR{dtr ? ' ●' : ' ○'}</Tag>
          <Tag color={rts ? 'green' : 'default'}>RTS{rts ? ' ●' : ' ○'}</Tag>
        </Space>
        <span style={{ color: 'var(--text-secondary)' }}>
          ↓{stat.rxRate}B/s ↑{stat.txRate}B/s
        </span>
        <div style={{ flex: 1 }} />
        <Space size={4}>
          <Button size="small" icon={paused ? <PlayCircleOutlined /> : <PauseCircleOutlined />} onClick={() => setPaused((p) => !p)}>
            {paused ? '恢复' : '暂停'}
          </Button>
          <Button size="small" icon={<ClearOutlined />} onClick={() => setEntries([])}>
            清空
          </Button>
          <Button size="small" icon={<DownloadOutlined />} danger={recording} onClick={toggleRecord}>
            {recording ? '停止记录' : '记录'}
          </Button>
          <Button size="small" onClick={() => { setPanelOpen(!panelOpen); localStorage.setItem(PANEL_KEY, panelOpen ? '0' : '1') }}>
            {panelOpen ? '收起面板' : '调试面板'}
          </Button>
        </Space>
      </div>

      {/* 双栏：控制台 + 折叠调试面板（relative 作问 AI 卡片的 absolute 锚点） */}
      <div style={{ display: 'flex', flex: 1, minHeight: 0, position: 'relative' }}>
        <div
          ref={hostRef}
          style={{ flex: 1, minWidth: 0, background: '#081018' }}
          onContextMenu={(e) => {
            // 右键菜单：快照当前选区（打开期间选区可能消失），替换 WebView 默认菜单
            e.preventDefault()
            const sel = termRef.current?.getSelection()
            setTermMenu({
              x: e.clientX,
              y: e.clientY,
              hasSelection: !!sel,
              // 选区快照截断到 8000 字符（问 AI 附带日志足够；太长会撑爆对话上下文）
              selText: sel ? sel.replace(/\s+$/, '').slice(0, 8000) : null,
            })
          }}
        />
        {termMenu && (
          <div className="term-menu" style={termMenuStyle} onContextMenu={(e) => e.preventDefault()}>
            <button
              type="button"
              className="term-menu-item"
              disabled={!termMenu.hasSelection}
              onClick={copySelection}
            >
              <span>复制</span>
              <span className="term-menu-key">Ctrl+Shift+C</span>
            </button>
            <button
              type="button"
              className="term-menu-item"
              disabled={!termMenu.hasSelection}
              title={termMenu.hasSelection ? '把所选数据/日志连同问题发给 AI 助手' : '先框选一段数据/日志'}
              onClick={() => {
                const sel = termMenu.selText
                setTermMenu(null)
                if (sel) setAsk({ selection: sel, question: '' })
              }}
            >
              <span>问 AI</span>
              <span className="term-menu-key">选区</span>
            </button>
            <button type="button" className="term-menu-item" onClick={() => void pasteFromClipboard()}>
              <span>粘贴并发送</span>
              <span className="term-menu-key">Ctrl+Shift+V</span>
            </button>
            <div className="term-menu-sep" />
            <button
              type="button"
              className="term-menu-item"
              onClick={() => {
                termRef.current?.selectAll()
                setTermMenu({ ...termMenu, hasSelection: true })
              }}
            >
              <span>全选</span>
              <span className="term-menu-key">Ctrl+Shift+A</span>
            </button>
            <button
              type="button"
              className="term-menu-item"
              onClick={() => {
                termRef.current?.clear()
                termRef.current?.focus()
                setTermMenu(null)
              }}
            >
              <span>清屏</span>
              <span className="term-menu-key">只清控制台，Hex 监视保留</span>
            </button>
          </div>
        )}
        {ask && (
          <div className="term-ask-card">
            <div className="term-drop-card-head">
              <span className="term-drop-card-title">问 AI（附上控制台选区）</span>
              <button type="button" className="term-drop-card-close" aria-label="取消" onClick={() => setAsk(null)}>
                ×
              </button>
            </div>
            <div className="term-ask-sel" title={ask.selection}>
              {ask.selection.length > 400 ? `${ask.selection.slice(0, 400)}…` : ask.selection}
            </div>
            <textarea
              className="term-ask-input"
              autoFocus
              rows={2}
              placeholder="输入你的问题：回车发送，Shift+回车换行，Esc 取消"
              value={ask.question}
              onChange={(e) => setAsk({ ...ask, question: e.target.value })}
              onKeyDown={(e) => {
                if (e.key === 'Enter' && !e.shiftKey) {
                  e.preventDefault()
                  submitAsk()
                } else if (e.key === 'Escape') {
                  setAsk(null)
                }
              }}
            />
            <button type="button" className="term-ask-send" disabled={!ask.question.trim()} onClick={submitAsk}>
              发送给 AI
            </button>
          </div>
        )}
        {panelOpen && (
          <div
            style={{
              width: 420,
              borderLeft: '1px solid var(--border-subtle)',
              background: 'var(--bg-base)',
              display: 'flex',
              flexDirection: 'column',
              minHeight: 0,
            }}
          >
            <Collapse
              ghost
              defaultActiveKey={['hex', 'send', 'signal']}
              style={{ flex: 1, overflow: 'auto', minHeight: 0 }}
              items={[
                {
                  key: 'hex',
                  label: <span style={{ color: 'var(--text-primary)', fontWeight: 600 }}>Hex 监视</span>,
                  children: (
                    <>
                      <Space style={{ marginBottom: 8 }} size={4}>
                        <Select
                          size="small"
                          value={dirFilter}
                          onChange={setDirFilter}
                          style={{ width: 88 }}
                          options={[
                            { value: 'all', label: '收发' },
                            { value: 'rx', label: '仅 RX' },
                            { value: 'tx', label: '仅 TX' },
                          ]}
                        />
                        <Switch size="small" checked={localEcho} onChange={setLocalEcho} />
                        <span style={{ color: 'var(--text-muted)', fontSize: 12 }}>本地回显</span>
                      </Space>
                      <div
                        ref={monitorRef}
                        style={{
                          height: 280,
                          overflow: 'auto',
                          background: 'var(--bg-inset)',
                          border: '1px solid var(--border-subtle)',
                          borderRadius: 4,
                          fontFamily: 'Consolas, monospace',
                          fontSize: 11,
                        }}
                      >
                        {filtered.length === 0 && (
                          <div style={{ color: 'var(--text-muted)', padding: 12 }}>暂无数据</div>
                        )}
                        {filtered.map((e) => (
                          <div
                            key={e.id}
                            style={{
                              padding: '3px 8px',
                              borderBottom: '1px solid var(--border-subtle)',
                              borderLeft: `3px solid ${e.dir === 'rx' ? 'var(--accent)' : 'var(--warning)'}`,
                            }}
                            onContextMenu={(ev) => {
                              ev.preventDefault()
                              setEntryMenu({ x: ev.clientX, y: ev.clientY, entry: e })
                            }}
                          >
                            <span style={{ color: 'var(--text-muted)' }}>{fmtTime(e.ts)}</span>{' '}
                            <Tag color={e.dir === 'rx' ? 'cyan' : 'orange'} style={{ marginInlineEnd: 4 }}>
                              {e.dir.toUpperCase()}
                            </Tag>
                            <div style={{ color: 'var(--text-primary)', wordBreak: 'break-all' }}>{toHex(e.data)}</div>
                            <div style={{ color: 'var(--text-secondary)', wordBreak: 'break-all' }}>{toAscii(e.data)}</div>
                          </div>
                        ))}
                      </div>
                    </>
                  ),
                },
                {
                  key: 'send',
                  label: <span style={{ color: 'var(--text-primary)', fontWeight: 600 }}>快捷发送</span>,
                  children: (
                    <>
                      <Input.TextArea
                        rows={2}
                        value={draft}
                        onChange={(e) => setDraft(e.target.value)}
                        placeholder={draftHex ? '十六进制字节，如 a1 b2 0d' : '要发送的文本'}
                        style={{ fontFamily: 'Consolas, monospace' }}
                      />
                      <Space style={{ margin: '8px 0' }} size={4} wrap>
                        <Switch size="small" checked={draftHex} onChange={setDraftHex} />
                        <span style={{ color: 'var(--text-muted)', fontSize: 12 }}>Hex</span>
                        <Select
                          size="small"
                          value={draftEnding}
                          onChange={setDraftEnding}
                          style={{ width: 96 }}
                          options={[
                            { value: 'none', label: '无行尾' },
                            { value: 'cr', label: 'CR' },
                            { value: 'lf', label: 'LF' },
                            { value: 'crlf', label: 'CRLF' },
                          ]}
                        />
                        <Button size="small" type="primary" icon={<CaretRightOutlined />} onClick={() => {
                          const bytes = encodePayload(draft, draftHex, draftEnding)
                          if (!bytes) return message.warning('Hex 内容格式不正确（需偶数个十六进制字符）')
                          send(bytes)
                        }}>
                          发送
                        </Button>
                        <Tooltip title="按当前输入内容与行尾定时重复发送（心跳/轮询调试）">
                          <Button size="small" danger={repeatOn} icon={repeatOn ? <PauseCircleOutlined /> : <PlayCircleOutlined />} onClick={() => setRepeatOn((r) => !r)}>
                            {repeatOn ? '停止重发' : '定时重发'}
                          </Button>
                        </Tooltip>
                        <InputNumber size="small" min={100} max={60000} step={100} value={repeatMs} onChange={(v) => setRepeatMs(v ?? 1000)} style={{ width: 92 }} addonAfter="ms" />
                      </Space>
                      <div style={{ display: 'flex', gap: 6, marginBottom: 8 }}>
                        <Input
                          size="small"
                          placeholder="预设标签"
                          value={presetLabel}
                          onChange={(e) => setPresetLabel(e.target.value)}
                          onPressEnter={() => {
                            savePreset()
                          }}
                        />
                        <Button size="small" onClick={savePreset}>
                          存为预设
                        </Button>
                        <Tooltip title="描述需求，AI 生成可发送的指令（hex 或文本）并填入发送框">
                          <Button size="small" icon={<RobotOutlined />} onClick={() => setGenOpen(true)}>
                            AI 生成
                          </Button>
                        </Tooltip>
                      </div>
                      <Space direction="vertical" style={{ width: '100%' }} size={4}>
                        {presets.map((p) => (
                          <div
                            key={p.id}
                            style={{
                              display: 'flex',
                              alignItems: 'center',
                              gap: 8,
                              padding: '4px 8px',
                              background: 'var(--bg-raised)',
                              borderRadius: 4,
                            }}
                            onContextMenu={(ev) => {
                              ev.preventDefault()
                              setPresetMenu({ x: ev.clientX, y: ev.clientY, preset: p })
                            }}
                          >
                            <Tag color={p.hex ? 'purple' : 'blue'} style={{ marginInlineEnd: 0 }}>
                              {p.hex ? 'HEX' : 'TXT'}
                            </Tag>
                            <span style={{ flex: 1, fontSize: 12, color: 'var(--text-primary)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                              {p.label}
                            </span>
                            <Button size="small" type="text" icon={<CaretRightOutlined />} onClick={() => sendPreset(p)} />
                            <Button size="small" type="text" onClick={() => savePresets(presets.filter((x) => x.id !== p.id))}>
                              ✕
                            </Button>
                          </div>
                        ))}
                        {presets.length === 0 && <div style={{ color: 'var(--text-muted)', fontSize: 12 }}>暂无预设：填好内容后存为预设，下次打开可直接点击发送</div>}
                      </Space>
                    </>
                  ),
                },
                {
                  key: 'signal',
                  label: <span style={{ color: 'var(--text-primary)', fontWeight: 600 }}>信号控制</span>,
                  children: (
                    <>
                      <Space direction="vertical" style={{ width: '100%' }} size={8}>
                        <Space>
                          <span style={{ color: 'var(--text-secondary)', fontSize: 12, width: 32 }}>DTR</span>
                          <Switch size="small" checked={dtr} onChange={toggleDtr} />
                          <span style={{ color: 'var(--text-secondary)', fontSize: 12, width: 32 }}>RTS</span>
                          <Switch size="small" checked={rts} onChange={toggleRts} />
                          <Button size="small" icon={<ReloadOutlined />} onClick={() => ctl({ op: 'signals' })}>
                            刷新信号
                          </Button>
                        </Space>
                        {signals && (
                          <Space size={4}>
                            {(['cd', 'cts', 'dsr', 'ri'] as const).map((k) => (
                              <Tag key={k} color={signals[k] ? 'green' : 'default'}>
                                {k.toUpperCase()}
                                {signals[k] ? ' ●' : ' ○'}
                              </Tag>
                            ))}
                          </Space>
                        )}
                        <Space wrap>
                          <Select size="small" value={breakMs} onChange={setBreakMs} style={{ width: 88 }} options={[100, 250, 500].map((v) => ({ value: v, label: `${v}ms` }))} />
                          <Button size="small" onClick={() => ctl({ op: 'break', ms: breakMs })}>
                            发送 Break
                          </Button>
                          <Tooltip title="DTR 脉冲复位（Arduino 自动复位进 bootloader）">
                            <Button size="small" onClick={pulseDtr}>Arduino 复位</Button>
                          </Tooltip>
                          <Tooltip title="RTS(IO0)+DTR 复位时序，ESP32 进入下载模式">
                            <Button size="small" onClick={esp32Download}>ESP32 下载</Button>
                          </Tooltip>
                        </Space>
                        <Space wrap>
                          <span style={{ color: 'var(--text-secondary)', fontSize: 12 }}>波特率</span>
                          <Select
                            size="small"
                            value={baud}
                            onChange={(v) => {
                              setBaud(v)
                              ctl({ op: 'set-baud', baudRate: v })
                            }}
                            style={{ width: 110 }}
                            options={[9600, 19200, 38400, 57600, 115200, 230400, 460800, 921600].map((v) => ({ value: v, label: String(v) }))}
                            showSearch
                          />
                          <Tooltip title="对未知设备扫波特率：切换后观察 Hex 监视数据是否变为可读文本">
                            <RightOutlined style={{ color: 'var(--text-muted)' }} />
                          </Tooltip>
                        </Space>
                      </Space>
                    </>
                  ),
                },
              ]}
            />
          </div>
        )}
      </div>

      {/* ── Hex 条目 / 预设条目右键（antd Menu + 共享遮罩）───────────────── */}
      {(entryMenu || presetMenu) && (
        <>
          <div
            style={{ position: 'fixed', inset: 0, zIndex: 1049 }}
            onClick={closeMenus}
            onContextMenu={(e) => {
              e.preventDefault()
              closeMenus()
            }}
          />
          {entryMenu && (
            <Menu
              items={entryMenuItems}
              onClick={onEntryMenuClick}
              style={{
                position: 'fixed',
                // 面板在窗口右侧 420px：钳制防溢出屏幕
                left: Math.max(8, Math.min(entryMenu.x, window.innerWidth - 220)),
                top: Math.max(8, Math.min(entryMenu.y, window.innerHeight - 260)),
                zIndex: 1050,
                border: '1px solid var(--border)',
              }}
            />
          )}
          {presetMenu && (
            <Menu
              items={presetMenuItems}
              onClick={onPresetMenuClick}
              style={{
                position: 'fixed',
                left: Math.max(8, Math.min(presetMenu.x, window.innerWidth - 220)),
                top: Math.max(8, Math.min(presetMenu.y, window.innerHeight - 260)),
                zIndex: 1050,
                border: '1px solid var(--border)',
              }}
            />
          )}
        </>
      )}

      {/* ── 预设编辑弹窗 ─────────────────────────────────────────────────── */}
      <Modal
        title="编辑预设"
        open={!!editPreset}
        onCancel={() => setEditPreset(null)}
        onOk={saveEditPreset}
        okText="保存"
        cancelText="取消"
        width={520}
      >
        {editPreset && (
          <Space direction="vertical" style={{ width: '100%' }} size={8}>
            <Input
              placeholder="预设标签"
              value={editPreset.label}
              onChange={(e) => setEditPreset({ ...editPreset, label: e.target.value })}
            />
            <Input.TextArea
              rows={2}
              value={editPreset.content}
              onChange={(e) => setEditPreset({ ...editPreset, content: e.target.value })}
              placeholder={editPreset.hex ? '十六进制字节，如 a1 b2 0d' : '要发送的文本'}
              style={{ fontFamily: 'Consolas, monospace' }}
            />
            <Space size={4} wrap>
              <Switch size="small" checked={editPreset.hex} onChange={(v) => setEditPreset({ ...editPreset, hex: v })} />
              <span style={{ color: 'var(--text-muted)', fontSize: 12 }}>Hex</span>
              <Select
                size="small"
                value={editPreset.ending}
                onChange={(v) => setEditPreset({ ...editPreset, ending: v })}
                style={{ width: 96 }}
                options={[
                  { value: 'none', label: '无行尾' },
                  { value: 'cr', label: 'CR' },
                  { value: 'lf', label: 'LF' },
                  { value: 'crlf', label: 'CRLF' },
                ]}
              />
            </Space>
          </Space>
        )}
      </Modal>

      {/* ── AI 生成指令输入弹窗 ───────────────────────────────────────────── */}
      <Modal
        title="AI 生成发送指令"
        open={genOpen}
        onCancel={() => setGenOpen(false)}
        onOk={startAiGenerate}
        okText="生成"
        cancelText="取消"
        okButtonProps={{ disabled: !genDesc.trim() }}
        width={560}
      >
        <Space direction="vertical" style={{ width: '100%' }} size={8}>
          <Input.TextArea
            autoFocus
            rows={3}
            value={genDesc}
            onChange={(e) => setGenDesc(e.target.value)}
            placeholder="例如：Modbus RTU 读保持寄存器：从站 1，起始 0x0000，读 2 个；或：发送 AT+RST 重启设备"
          />
          <Space size={4} wrap>
            <Select
              size="small"
              value={genHex ? 'hex' : 'text'}
              onChange={(v) => setGenHex(v === 'hex')}
              style={{ width: 100 }}
              options={[
                { value: 'hex', label: 'HEX 字节' },
                { value: 'text', label: '文本' },
              ]}
            />
            <Select
              size="small"
              value={genEnding}
              onChange={setGenEnding}
              style={{ width: 96 }}
              options={[
                { value: 'none', label: '无行尾' },
                { value: 'cr', label: 'CR' },
                { value: 'lf', label: 'LF' },
                { value: 'crlf', label: 'CRLF' },
              ]}
            />
            <Switch size="small" checked={genWithContext} onChange={setGenWithContext} />
            <span style={{ color: 'var(--text-muted)', fontSize: 12 }}>附最近收发数据（帮助 AI 判断指令格式）</span>
          </Space>
        </Space>
      </Modal>

      {/* ── AI 结果弹窗（解析数据帧 / 生成指令共用）────────────────────────── */}
      <SerialAiModal
        open={!!ai}
        title={ai?.action === 'parse' ? 'AI 解析数据帧' : 'AI 生成指令'}
        loading={ai?.loading}
        error={ai?.error}
        text={ai?.text}
        autoCopy={ai?.action === 'generate'}
        onInsert={ai?.action === 'generate' ? insertGenerated : undefined}
        onClose={() => setAi(undefined)}
      />
    </div>
  )
}
