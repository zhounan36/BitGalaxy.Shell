import { useEffect, useRef, useState } from 'react'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import { Modal, Progress, Typography, message } from 'antd'
import { CloseOutlined, CompressOutlined, DownloadOutlined, ExpandOutlined } from '@ant-design/icons'
import { DataFrameType } from '@bgshell/protocol'
import { agentSocket } from '../api/ws'
import { readClipboardText, writeClipboardText } from '../api/clipboard'
import { requestAiAsk } from '../api/aiBridge'
import {
  cancelTermDownload,
  cleanSelectedName,
  clearSessionCwd,
  deleteRemoteTarget,
  downloadSelectionFromSession,
  parseOsc7,
  reportOsc7,
  resolveSelectionTarget,
  setSessionAltScreen,
  type TermDownloadState,
} from '../api/termUpload'
import { formatBytes } from '../utils/format'

/** 右键菜单状态（clientX/Y + 当前选区与清洗出的待下载名） */
interface TermMenuState {
  x: number
  y: number
  hasSelection: boolean
  /** 选区清洗出的文件/文件夹名（无有效选区为 null） */
  selName: string | null
  /** 选区原文（截断到 8000 字符，供「问AI」附进对话；无选区为 null） */
  selText: string | null
}

/** 候选命令粘贴注册表：sessionId → 粘贴函数（候选命令面板点击时调用）。
 *  用 term.paste（尊重括号粘贴模式）粘贴但不自动执行，用户敲回车才执行。 */
const pasteRegistry = new Map<string, (text: string) => void>()

/** 把命令文本粘贴到指定终端；返回 false 表示该会话没有可粘贴的终端 */
export function pasteIntoTerminal(sessionId: string, text: string): boolean {
  const fn = pasteRegistry.get(sessionId)
  if (!fn) return false
  fn(text)
  return true
}

/** 当前注册了粘贴入口的会话 id 列表 */
export function pasteTargets(): string[] {
  return [...pasteRegistry.keys()]
}

/** 「问AI」输入态：携带发起时的选区快照 + 用户输入的问题 */
interface AskDraft {
  selection: string
  question: string
}

interface TerminalPaneProps {
  sessionId: string
  /** 该会话对应的主机名（「问AI」时随问题带给 AI 定位上下文） */
  hostName?: string
  /** HTML5 拖放兜底（浏览器 dev 宿主；Tauri 壳内原生拖放接管、不会触发） */
  onFilesDrop?: (files: File[]) => void
  /** 专注模式（App 层隐藏侧栏/标签栏，把宽度让给终端增加列数） */
  focusMode?: boolean
  onFocusToggle?: () => void
}

/** 单个终端面板：xterm ↔ Agent（stdin/stdout 二进制帧，按 sessionId 复用同一条 WS）
 *  右键呼出自定义菜单：复制 / 下载所选（SFTP）/ 删除所选（SFTP，删除前确认）/ 粘贴 / 全选 / 清屏 / 专注模式；
 *  另支持终端惯例快捷键。
 *  OSC 7（shell 集成上报当前目录）→ termUpload 目录缓存，供拖拽上传与选区下载定位远端路径。
 *  折行视觉层：被列宽折断的续行标 ↳ + 折行块左侧竖线 + 块结束分隔线（docker ps 等长输出更易读）。 */
export function TerminalPane({ sessionId, hostName, onFilesDrop, focusMode, onFocusToggle }: TerminalPaneProps) {
  const holderRef = useRef<HTMLDivElement>(null)
  const hostRef = useRef<HTMLDivElement>(null)
  const layerRef = useRef<HTMLDivElement>(null)
  const termRef = useRef<Terminal | null>(null)
  const [menu, setMenu] = useState<TermMenuState | null>(null)
  const [dl, setDl] = useState<TermDownloadState | null>(null)
  /** 「问AI」输入态（非 null 时浮层显示选区 + 问题输入框） */
  const [ask, setAsk] = useState<AskDraft | null>(null)
  // 快捷键回调存 ref：避免 effect 闭包捕获旧的 onFocusToggle
  const focusToggleRef = useRef(onFocusToggle)
  focusToggleRef.current = onFocusToggle

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
    term.open(hostRef.current!)
    fit.fit()
    term.focus()

    // OSC 7：shell 集成上报当前目录（file://host/path）→ 缓存并做主机校验
    // （嵌套 SSH 内层 shell 的上报会被拒绝）；全屏程序（top/vim 等交替屏幕）
    // 运行期间无提示符 → 无新上报，缓存自然停留在进入前的目录
    term.parser.registerOscHandler(7, (data: string) => {
      const r = parseOsc7(data)
      if (r) reportOsc7(sessionId, r.host, r.dir)
      return false
    })
    // 全屏程序检测：xterm 自己维护交替屏幕状态，拖拽时按需读取
    setSessionAltScreen(sessionId, () => term.buffer.active.type === 'alternate')

    // 打开后立即上报实际尺寸（open 帧先用占位 80x24）
    agentSocket.sendControl({ t: 'resize', s: sessionId, m: { cols: term.cols, rows: term.rows } })
    // 兜底：open 建连完成（session-opened）后再补发一次当前尺寸，
    // 确保远端 PTY 与 xterm 实际尺寸一致（不一致时 readline 折行/光标错位，
    // 方向键调历史会覆盖遮挡旧内容；后端 open 期间也会缓存 resize，双保险）
    const offOpened = agentSocket.onControl((frame) => {
      if (frame.t === 'session-opened' && frame.s === sessionId) {
        agentSocket.sendControl({ t: 'resize', s: sessionId, m: { cols: term.cols, rows: term.rows } })
      }
    })

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

    // ── 折行视觉层：被列宽折断的续行标 ↳、折行块左侧竖线、块结束分隔线 ──
    // 只读 isWrapped 重绘覆盖层，不动 buffer（复制文本不受影响）。
    // 全屏程序（交替屏幕：vim/top 等）不标注；用 rAF 合并高频渲染帧。
    const layer = layerRef.current!
    let wrapRaf = 0
    const paintWrapLayer = () => {
      wrapRaf = 0
      const screenEl = term.element?.querySelector('.xterm-screen')
      if (!screenEl) return
      const buf = term.buffer.active
      const layerRect = layer.getBoundingClientRect()
      const screenRect = screenEl.getBoundingClientRect()
      if (buf.type === 'alternate' || layerRect.width === 0 || screenRect.height === 0) {
        layer.innerHTML = ''
        return
      }
      const cellH = screenRect.height / term.rows
      const ox = screenRect.left - layerRect.left
      const oy = screenRect.top - layerRect.top
      // 左侧留出的行号槽位（.term-gutter 宽度），标记画在这里不遮内容
      const gx = ox - 22
      const parts: string[] = []
      let i = 0
      while (i < term.rows) {
        const line = buf.getLine(buf.viewportY + i)
        if (!line) {
          i++
          continue
        }
        // 折行块 = 一整条逻辑行（被列宽折成的连续行）
        let s = i
        while (s > 0 && buf.getLine(buf.viewportY + s)?.isWrapped) s--
        let e = i
        while (e + 1 < term.rows && buf.getLine(buf.viewportY + e + 1)?.isWrapped) e++
        if (e > s) {
          const top = oy + s * cellH
          const bottom = oy + (e + 1) * cellH
          parts.push(
            `<div class="wrap-bar" style="top:${top + 1}px;height:${bottom - top - 2}px;left:${ox - 6}px"></div>`,
          )
          for (let y = s + 1; y <= e; y++) {
            parts.push(
              `<div class="wrap-arrow" style="top:${oy + y * cellH}px;height:${cellH}px;left:${gx}px">↳</div>`,
            )
          }
          // 块结束分隔线：与下一排拉开视觉距离（最后一行下方是屏幕外，不必画）
          if (e < term.rows - 1) {
            parts.push(
              `<div class="wrap-sep" style="top:${bottom - 1}px;left:${gx}px;width:${layerRect.width - gx}px"></div>`,
            )
          }
        }
        i = e + 1
      }
      layer.innerHTML = parts.join('')
    }
    const scheduleWrapPaint = () => {
      if (!wrapRaf) wrapRaf = requestAnimationFrame(paintWrapLayer)
    }
    const offRender = term.onRender(scheduleWrapPaint)
    const offScroll = term.onScroll(scheduleWrapPaint)
    const offResize = term.onResize(scheduleWrapPaint)
    scheduleWrapPaint()

    // ── 快捷键：Ctrl+Shift+C/V/A / Ctrl+Insert / Shift+Insert ──
    term.attachCustomKeyEventHandler((ev) => {
      if (ev.type !== 'keydown') return true
      const ctrl = ev.ctrlKey || ev.metaKey
      const wantCopy = (ctrl && ev.shiftKey && ev.key.toLowerCase() === 'c') || (ctrl && ev.key === 'Insert')
      const wantPaste = (ctrl && ev.shiftKey && ev.key.toLowerCase() === 'v') || (ev.shiftKey && ev.key === 'Insert')
      const wantSelectAll = ctrl && ev.shiftKey && ev.key.toLowerCase() === 'a'
      const wantFocus = ctrl && ev.shiftKey && ev.key.toLowerCase() === 'f'
      // 必须显式 preventDefault：返回 false 只是不让 xterm 处理该键，
      // 浏览器默认动作仍会触发（Ctrl+Shift+V / Shift+Insert = 原生粘贴 →
      // xterm textarea 的 paste 事件再粘贴一次，表现为粘贴两遍）
      if (wantCopy && term.hasSelection()) {
        ev.preventDefault()
        copySelection()
        return false
      }
      if (wantPaste) {
        ev.preventDefault()
        void pasteFromClipboard()
        return false
      }
      if (wantSelectAll) {
        ev.preventDefault()
        term.selectAll()
        return false
      }
      if (wantFocus) {
        ev.preventDefault()
        focusToggleRef.current?.()
        return false
      }
      return true
    })

    // ── 右键菜单：在光标处弹出自定义菜单（替换 WebView 默认菜单）──────────
    const onContextMenu = (e: MouseEvent) => {
      e.preventDefault()
      const sel = term.getSelection()
      setMenu({
        x: e.clientX,
        y: e.clientY,
        hasSelection: !!sel,
        selName: sel ? cleanSelectedName(sel) : null,
        // 选区快照截断到 8000 字符（问AI 附带日志足够；太长会撑爆对话上下文）
        selText: sel ? sel.replace(/\s+$/, '').slice(0, 8000) : null,
      })
    }
    const holder = holderRef.current!
    holder.addEventListener('contextmenu', onContextMenu)
    // 候选命令粘贴入口：AiCommandPanel 点击列表项时按 sessionId 找到这里
    pasteRegistry.set(sessionId, (text) => {
      term.paste(text)
      term.focus()
    })

    return () => {
      holder.removeEventListener('contextmenu', onContextMenu)
      pasteRegistry.delete(sessionId)
      clearSessionCwd(sessionId)
      offOpened()
      offData()
      dataSub.dispose()
      ro.disconnect()
      offRender.dispose()
      offScroll.dispose()
      offResize.dispose()
      if (wrapRaf) cancelAnimationFrame(wrapRaf)
      term.dispose()
      termRef.current = null
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId])

  // ── 选区下载（右键菜单触发）：状态经 onState 推给右下角浮层 ─────────────
  const startDownload = (name: string) => {
    setDl(null)
    void downloadSelectionFromSession(sessionId, name, (st) => setDl(st)).then((fin) => {
      // 成功 → 5s 后自动收起；失败/取消保留供查看
      if (fin && fin.status === 'done') {
        window.setTimeout(() => {
          setDl((prev) => (prev && prev.status === 'done' && prev.name === fin.name ? null : prev))
        }, 5000)
      }
    })
  }

  // ── 选区删除（右键菜单触发）：ls 自动识别文件/文件夹 → 确认后 rm（目录递归）──
  const startDelete = (name: string) => {
    void (async () => {
      const target = await resolveSelectionTarget(sessionId, name)
      if ('error' in target) {
        message.error(target.error)
        return
      }
      Modal.confirm({
        title: target.isDir ? `删除文件夹「${target.name}」？` : `删除文件「${target.name}」？`,
        content: target.isDir
          ? `将递归删除远端目录 ${target.remotePath} 及其全部内容，操作不可恢复。`
          : `将删除远端文件 ${target.remotePath}，操作不可恢复。`,
        okText: '删除',
        okButtonProps: { danger: true },
        cancelText: '取消',
        onOk: async () => {
          const r = await deleteRemoteTarget(sessionId, target)
          if (r.ok) message.success(`已删除 ${target.name}`)
          else message.error(r.message ?? '删除失败')
        },
      })
    })()
  }

  // ── 问AI（右键菜单触发）：选区快照 + 问题投递给桌宠对话面板 ─────────────
  const submitAsk = () => {
    if (!ask || !ask.question.trim()) return
    void requestAiAsk({ hostName: hostName ?? '', selection: ask.selection, question: ask.question.trim() })
    setAsk(null)
    message.success('已发给 AI，回答在桌宠对话面板查看')
  }

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
        top: Math.max(8, Math.min(menu.y, window.innerHeight - 352)),
      }
    : undefined

  return (
    <div
      ref={holderRef}
      className="terminal-holder"
      data-term-session={sessionId}
      onDragOver={(e) => {
        // Tauri 壳内原生拖放接管，此处仅浏览器 dev 宿主生效
        e.preventDefault()
        holderRef.current?.classList.add('drag-over')
      }}
      onDragLeave={() => holderRef.current?.classList.remove('drag-over')}
      onDrop={(e) => {
        e.preventDefault()
        holderRef.current?.classList.remove('drag-over')
        const files = [...e.dataTransfer.files]
        if (files.length > 0) onFilesDrop?.(files)
      }}
    >
      <div ref={hostRef} className="term-host" />
      <div ref={layerRef} className="term-wrap-layer" aria-hidden="true" />
      <button
        type="button"
        className="term-focus-btn"
        title={focusMode ? '退出专注模式（Ctrl+Shift+F）' : '专注模式：隐藏侧栏，加宽终端（Ctrl+Shift+F）'}
        onClick={() => onFocusToggle?.()}
      >
        {focusMode ? <CompressOutlined /> : <ExpandOutlined />}
      </button>
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
            disabled={!menu.hasSelection}
            title={menu.hasSelection ? '把所选内容连同问题发给桌宠 AI' : '先框选一段日志/输出'}
            onClick={() => {
              const sel = menu.selText
              setMenu(null)
              if (sel) setAsk({ selection: sel, question: '' })
            }}
          >
            <span>问 AI</span>
            <span className="term-menu-key">选区</span>
          </button>
          <button
            type="button"
            className="term-menu-item"
            disabled={!menu.selName}
            title={menu.selName ?? undefined}
            onClick={() => {
              const name = menu.selName
              setMenu(null)
              if (name) startDownload(name)
            }}
          >
            <span>
              {menu.selName
                ? `下载「${menu.selName.length > 12 ? `${menu.selName.slice(0, 11)}…` : menu.selName}」`
                : '下载所选'}
            </span>
            <span className="term-menu-key">SFTP</span>
          </button>
          <button
            type="button"
            className="term-menu-item term-menu-item-danger"
            disabled={!menu.selName}
            title={menu.selName ?? undefined}
            onClick={() => {
              const name = menu.selName
              setMenu(null)
              if (name) startDelete(name)
            }}
          >
            <span>
              {menu.selName
                ? `删除「${menu.selName.length > 12 ? `${menu.selName.slice(0, 11)}…` : menu.selName}」`
                : '删除所选'}
            </span>
            <span className="term-menu-key">SFTP</span>
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
            <span className="term-menu-key">Ctrl+Shift+A</span>
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
          <div className="term-menu-sep" />
          <button
            type="button"
            className="term-menu-item"
            onClick={() => {
              setMenu(null)
              onFocusToggle?.()
            }}
          >
            <span>{focusMode ? '退出专注模式' : '专注模式'}</span>
            <span className="term-menu-key">Ctrl+Shift+F</span>
          </button>
        </div>
      )}
      {dl && (
        <div className="term-dl-card">
          <div className="term-drop-card-head">
            <DownloadOutlined style={{ color: 'var(--accent)' }} />
            <span className="term-drop-card-title" title={dl.name}>
              {dl.isDir ? '下载文件夹' : '下载'}：{dl.name}
            </span>
            {dl.status === 'downloading' && dl.opId ? (
              <button
                type="button"
                className="term-drop-card-close"
                title="取消下载（保留断点，重新下载可续传）"
                onClick={() => {
                  cancelTermDownload(sessionId, dl.opId!)
                  setDl({ ...dl, status: 'error', message: '已取消（断点已保留，重新下载自动续传）' })
                }}
              >
                <CloseOutlined />
              </button>
            ) : (
              <button
                type="button"
                className="term-drop-card-close"
                aria-label="关闭"
                onClick={() => setDl(null)}
              >
                <CloseOutlined />
              </button>
            )}
          </div>
          <Progress
            size="small"
            style={{ marginBottom: 0 }}
            status={dl.status === 'error' ? 'exception' : dl.status === 'done' ? 'success' : 'active'}
            percent={dl.total > 0 ? Math.floor((dl.transferred / dl.total) * 100) : 0}
            format={() =>
              dl.status === 'resolving'
                ? '定位远端文件…'
                : dl.status === 'error'
                  ? '失败'
                  : dl.total > 0
                    ? `${formatBytes(dl.transferred)} / ${formatBytes(dl.total)}${dl.speedBps ? ` · ${formatBytes(dl.speedBps)}/s` : ''}`
                    : '准备中…'
            }
          />
          {dl.status === 'downloading' && dl.current && (
            <div className="term-dl-card-sub" title={dl.current}>
              {dl.fileTotal ? `${dl.fileDone ?? 0}/${dl.fileTotal} · ` : ''}
              {dl.current}
            </div>
          )}
          {dl.status === 'done' && (
            <div className="term-dl-card-sub">
              已完成{dl.total > 0 ? `（${formatBytes(dl.transferred)}）` : ''}
            </div>
          )}
          {dl.message && (
            <Typography.Text type="danger" style={{ fontSize: 12 }}>
              {dl.message}
            </Typography.Text>
          )}
        </div>
      )}
      {ask && (
        <div className="term-ask-card">
          <div className="term-drop-card-head">
            <span className="term-drop-card-title">问 AI（附上终端选区）</span>
            <button type="button" className="term-drop-card-close" aria-label="取消" onClick={() => setAsk(null)}>
              <CloseOutlined />
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
    </div>
  )
}
