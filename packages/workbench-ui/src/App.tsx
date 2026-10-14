import { useCallback, useEffect, useRef, useState, lazy, Suspense } from 'react'
import { Input, Layout, Modal, Tabs, message } from 'antd'
import { CodeOutlined, DashboardOutlined, FolderOutlined } from '@ant-design/icons'
import type { AiCommand, HostRecord, SessionKind } from '@bgshell/protocol'
import { agentSocket } from './api/ws'
import { onAiAsk, type AiAskPayload } from './api/aiBridge'
import {
  clearSessionCwd,
  isSessionAltScreen,
  resolveRemoteCwd,
  uploadToSession,
  type UploadSource,
} from './api/termUpload'
import { AgentControls } from './components/AgentControls'
import { AiCommandPanel } from './components/AiCommandPanel'
import { DropUploadModal } from './components/DropUploadModal'
import { HostEditDrawer } from './components/HostEditDrawer'
import { HostList, type HostStatus } from './components/HostList'
import { MetricsPane } from './components/MetricsPane'
import { SettingsModal } from './components/SettingsModal'
import { SftpPane } from './components/SftpPane'
import { TermDropUploadCard, type TermDropUploadView } from './components/TermDropUploadCard'
import { TerminalPane } from './components/TerminalPane'
import { TitleBar } from './components/TitleBar'

interface SessionTab {
  key: string
  kind: SessionKind
  hostId: string
  hostName: string
}

const KIND_LABEL: Record<SessionKind, string> = { shell: '', sftp: ' · 文件', metrics: ' · 状态' }
const KIND_ICON: Record<SessionKind, React.ReactNode> = {
  shell: <CodeOutlined />,
  sftp: <FolderOutlined />,
  metrics: <DashboardOutlined />,
}

// AI 对话面板（仅 SSH 精简版加载）：lazy + 构建常量折叠——完整版的动态 import
// 会被 tree-shake，ChatPanel chunk 不进入工作台页面的预加载图（AI 对话宿主是宠物窗）
const ChatPanel = __BGSH_PET__
  ? null
  : lazy(() => import('./components/ChatPanel').then((m) => ({ default: m.ChatPanel })))

export function App() {
  const [connected, setConnected] = useState(false)
  const [hosts, setHosts] = useState<HostRecord[]>([])
  const [hostStatus, setHostStatus] = useState<Record<string, HostStatus | undefined>>({})
  const [tabs, setTabs] = useState<SessionTab[]>([])
  const [activeKey, setActiveKey] = useState<string>()
  const [editing, setEditing] = useState<{ open: boolean; host?: HostRecord }>({ open: false })
  const [settingsOpen, setSettingsOpen] = useState(false)
  // ── 终端专注模式：隐藏侧栏/标签栏，把宽度让给终端增加列数（docker ps 等宽表不折行）──
  const [termFocus, setTermFocus] = useState(false)
  // ── 密码认证失败重试弹窗（auth-retry 帧 → retry-auth 回包）─────────────
  const [authRetry, setAuthRetry] = useState<{
    sessionId: string
    username: string
    host: string
    message: string
    attempt: number
  } | null>(null)
  const [authPassword, setAuthPassword] = useState('')
  const [authWaiting, setAuthWaiting] = useState(false)
  // ── AI 候选命令（propose_command 推送）：累积显示在右下角面板，用户点击粘贴执行 ──
  const [aiCommands, setAiCommands] = useState<AiCommand[]>([])
  const [aiCmdOpen, setAiCmdOpen] = useState(true)
  // ── AI 对话面板（仅 SSH 精简版：完整版的 AI 对话宿主是宠物窗）─────────────
  const [chatOpen, setChatOpen] = useState(false)
  const [askReq, setAskReq] = useState<AiAskPayload | null>(null)

  useEffect(() => {
    if (__BGSH_PET__) return
    let disposed = false
    let off: (() => void) | undefined
    // 终端「问AI」（精简版 requestAiAsk 发给本窗）：打开面板并交给 ChatPanel 组合发送
    void onAiAsk((p) => {
      if (disposed || !p?.uid) return
      setChatOpen(true)
      setAskReq(p)
    }).then((u) => {
      if (disposed) u()
      else off = u
    })
    return () => {
      disposed = true
      off?.()
    }
  }, [])

  const refreshHosts = useCallback(() => agentSocket.sendControl({ t: 'hosts', m: { op: 'list' } }), [])

  // 专注模式只属于终端页签：切到文件/状态页签、关闭全部页签时自动退出，避免无处退出的死角
  useEffect(() => {
    const active = tabs.find((t) => t.key === activeKey)
    if (termFocus && active?.kind !== 'shell') setTermFocus(false)
  }, [termFocus, tabs, activeKey])

  useEffect(() => {
    let disposed = false
    agentSocket
      .connect()
      .then(() => {
        if (disposed) return
        setConnected(true)
        refreshHosts()
      })
      .catch((err: Error) => message.error(`${err.message}`))
    const off = agentSocket.onControl((frame) => {
      switch (frame.t) {
        case 'session-opened':
          // 重试密码后连接成功：收起重试弹窗
          setAuthRetry((prev) => (prev && prev.sessionId === frame.s ? null : prev))
          setAuthWaiting(false)
          break
        case 'hosts':
          if (frame.m.ok && frame.m.hosts) setHosts(frame.m.hosts)
          else if (!frame.m.ok && frame.m.message) message.error(frame.m.message)
          break
        case 'probe-result':
          setHostStatus((prev) => ({
            ...prev,
            [frame.m.hostId]: { ok: frame.m.ok, ms: frame.m.ms, message: frame.m.message },
          }))
          break
        case 'ai-commands':
          // AI 提交的候选命令：累积进右下角面板（收起状态也重新弹出）
          setAiCommands((prev) => [...prev, ...frame.m.commands].slice(-20))
          setAiCmdOpen(true)
          break
        case 'session-closed':
          clearSessionCwd(frame.s)
          setTabs((prev) => prev.filter((t) => t.key !== frame.s))
          setAuthRetry((prev) => (prev && prev.sessionId === frame.s ? null : prev))
          setAuthWaiting(false)
          message.info(`会话已关闭（${frame.m.reason}）`)
          break
        case 'auth-retry':
          // 密码认证被拒：弹窗重输（同一会话再次失败时更新提示并清空输入）
          setAuthWaiting(false)
          setAuthPassword('')
          setAuthRetry((prev) =>
            prev && prev.sessionId === frame.s
              ? { ...prev, message: frame.m.message, attempt: prev.attempt + 1 }
              : {
                  sessionId: frame.s,
                  username: frame.m.username,
                  host: frame.m.host,
                  message: frame.m.message,
                  attempt: 1,
                },
          )
          break
        case 'hostkey-unknown':
          Modal.confirm({
            title: `首次连接 ${frame.m.host}:${frame.m.port}`,
            content: (
              <div>
                <p style={{ wordBreak: 'break-all' }}>
                  <b>{frame.m.keyType}</b>
                </p>
                <p style={{ fontFamily: 'monospace', wordBreak: 'break-all' }}>{frame.m.fingerprint}</p>
                <p style={{ color: 'var(--text-muted)' }}>
                  请与服务器上 ssh-keygen -lf 输出的指纹比对，确认后信任（TOFU）。
                </p>
              </div>
            ),
            okText: '信任并连接',
            cancelText: '拒绝',
            onOk: () => agentSocket.sendControl({ t: 'confirm-hostkey', s: frame.s, m: { accepted: true } }),
            onCancel: () => agentSocket.sendControl({ t: 'confirm-hostkey', s: frame.s, m: { accepted: false } }),
          })
          break
        case 'error':
          if (frame.m.code === 'disconnected') {
            // 后端在 WS 断开时已 close_all 全部会话：清空页签避免僵尸终端；
            // socket 会自动重连，重连成功后走 reconnected 分支刷新
            setTabs([])
            setActiveKey(undefined)
            setAuthRetry(null)
            setAuthWaiting(false)
            message.error(frame.m.message)
            return
          }
          if (frame.m.code === 'reconnected') {
            refreshHosts()
            message.info(frame.m.message)
            return
          }
          // 建连失败关闭页签；运行期错误（metrics-failed 等）仅提示，由面板自行呈现
          message.error(frame.m.message)
          if (frame.s && frame.m.code === 'open-failed') {
            setTabs((prev) => prev.filter((t) => t.key !== frame.s))
            setAuthRetry((prev) => (prev && prev.sessionId === frame.s ? null : prev))
            setAuthWaiting(false)
          }
          break
      }
    })
    return () => {
      disposed = true
      off()
    }
  }, [refreshHosts])

  // 主机列表变化时自动探测全部主机
  useEffect(() => {
    if (!connected) return
    for (const h of hosts) {
      setHostStatus((prev) => (prev[h.id] ? prev : { ...prev, [h.id]: undefined }))
      agentSocket.sendControl({ t: 'probe', m: { hostId: h.id } })
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [connected, hosts.map((h) => h.id).join(',')])

  // ── 桌宠联动（仅 Tauri 壳内生效；浏览器宿主无 __TAURI_INTERNALS__）────────
  // 宠物窗右键呼出会话 / 拖文件到宠物打开上传面板，均经 Tauri 事件转发
  const [petLink, setPetLink] = useState<{ hostId: string; kind: SessionKind } | null>(null)
  const [dropUpload, setDropUpload] = useState<{ paths: string[]; uid: string } | null>(null)
  const lastPetUidRef = useRef<string | undefined>(undefined)
  useEffect(() => {
    if (!('__TAURI_INTERNALS__' in window)) return
    let disposed = false
    const offs: Array<() => void> = []
    void (async () => {
      const { listen } = await import('@tauri-apps/api/event')
      if (disposed) return
      offs.push(
        await listen<{ hostId: string; kind?: SessionKind; uid?: string }>('pet://open-session', (e) => {
          const p = e.payload
          if (!p?.hostId || (p.uid && p.uid === lastPetUidRef.current)) return
          lastPetUidRef.current = p.uid
          setPetLink({ hostId: p.hostId, kind: p.kind ?? 'shell' })
        }),
      )
      offs.push(
        await listen<{ paths?: string[]; uid?: string }>('pet://drop-upload', (e) => {
          const p = e.payload
          const paths = (p?.paths ?? []).filter(Boolean)
          if (paths.length === 0 || (p.uid && p.uid === lastPetUidRef.current)) return
          lastPetUidRef.current = p.uid
          setDropUpload({ paths, uid: p.uid ?? crypto.randomUUID() })
        }),
      )
    })()
    return () => {
      disposed = true
      offs.forEach((off) => off())
    }
  }, [])

  // 呼出的会话：等主机列表就绪后打开（主机不存在则忽略——未同步或已删除）
  useEffect(() => {
    if (!petLink || hosts.length === 0) return
    const host = hosts.find((h) => h.id === petLink.hostId)
    setPetLink(null)
    if (host) openSession(host, petLink.kind)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [petLink, hosts])

  // ── 终端拖拽上传：拖文件到终端面板 → 经该会话（同连接 SFTP）上传到当前目录 ──
  // 目录来源：OSC 7（shell 集成实时上报）优先，sftp pwd（登录目录）兜底
  const [termUpload, setTermUpload] = useState<TermDropUploadView | null>(null)
  const tabsRef = useRef<SessionTab[]>([])
  useEffect(() => {
    tabsRef.current = tabs
  }, [tabs])

  const startTermUpload = useCallback((sessionId: string, sources: UploadSource[]) => {
    const tab = tabsRef.current.find((t) => t.key === sessionId)
    setTermUpload({
      sessionId,
      hostName: tab?.hostName ?? '主机',
      dir: '',
      items: [],
      finished: false,
      altScreen: isSessionAltScreen(sessionId),
    })
    void (async () => {
      const cwd = await resolveRemoteCwd(sessionId)
      if ('error' in cwd) {
        setTermUpload((prev) => (prev?.sessionId === sessionId ? { ...prev, error: cwd.error, finished: true } : prev))
        return
      }
      setTermUpload((prev) => (prev?.sessionId === sessionId ? { ...prev, dir: cwd.dir, dirSource: cwd.source } : prev))
      const items = await uploadToSession(sessionId, cwd.dir, sources, (its) => {
        setTermUpload((prev) => (prev?.sessionId === sessionId ? { ...prev, items: its } : prev))
      })
      setTermUpload((prev) => (prev?.sessionId === sessionId ? { ...prev, items, finished: true } : prev))
      // 全部成功 → 5s 后自动收起；有失败则保留供查看
      if (items.every((i) => i.status === 'done')) {
        window.setTimeout(() => setTermUpload((prev) => (prev?.sessionId === sessionId ? null : prev)), 5000)
      }
    })()
  }, [])

  /** Tauri 原生拖放给出本地绝对路径：stat 后过滤目录再上传 */
  const startTermUploadFromPaths = useCallback(
    async (sessionId: string, paths: string[]) => {
      const { invoke } = await import('@tauri-apps/api/core')
      const sources: UploadSource[] = []
      const skipped: string[] = []
      for (const path of paths) {
        try {
          const s = await invoke<{ name: string; size: number; is_dir: boolean }>('stat_file', { path })
          if (s.is_dir) skipped.push(s.name)
          else sources.push({ name: s.name, sizeB: s.size, path })
        } catch {
          skipped.push(path.split(/[\\/]/).pop() ?? path)
        }
      }
      if (skipped.length > 0) message.warning(`已跳过（不支持目录/不可读）：${skipped.join('、')}`)
      if (sources.length > 0) startTermUpload(sessionId, sources)
    },
    [startTermUpload],
  )

  // Tauri 壳：OS 级拖放事件（物理坐标命中测试 → 终端面板高亮 / 落下上传）
  useEffect(() => {
    if (!('__TAURI_INTERNALS__' in window)) return
    let disposed = false
    let unlisten: (() => void) | undefined
    const clearHighlight = () =>
      document
        .querySelectorAll('.terminal-holder.drag-over')
        .forEach((el) => el.classList.remove('drag-over'))
    const termSessionAt = (xPhys: number, yPhys: number): string | null => {
      const scale = window.devicePixelRatio || 1
      const el = document.elementFromPoint(xPhys / scale, yPhys / scale)
      return (
        (el as HTMLElement | null)?.closest('[data-term-session]')?.getAttribute('data-term-session') ?? null
      )
    }
    void (async () => {
      const { getCurrentWebview } = await import('@tauri-apps/api/webview')
      if (disposed) return
      unlisten = await getCurrentWebview().onDragDropEvent((ev) => {
        const p = ev.payload
        if (p.type === 'drop') {
          clearHighlight()
          const sid = termSessionAt(p.position.x, p.position.y)
          if (!sid || p.paths.length === 0) return
          void startTermUploadFromPaths(sid, p.paths)
        } else if (p.type === 'enter' || p.type === 'over') {
          clearHighlight()
          const sid = termSessionAt(p.position.x, p.position.y)
          if (sid) document.querySelector(`[data-term-session="${sid}"]`)?.classList.add('drag-over')
        } else if (p.type === 'leave') {
          clearHighlight()
        }
      })
    })()
    return () => {
      disposed = true
      unlisten?.()
      clearHighlight()
    }
  }, [startTermUploadFromPaths])

  const openSession = (host: HostRecord, kind: SessionKind) => {
    const sessionId = crypto.randomUUID()
    setTabs((prev) => [...prev, { key: sessionId, kind, hostId: host.id, hostName: host.name }])
    setActiveKey(sessionId)
    agentSocket.sendControl({
      t: 'open',
      s: sessionId,
      m: { hostId: host.id, kind, cols: 80, rows: 24 },
    })
  }

  // 密码重试弹窗：提交新密码（等待后端结果），或取消（password 缺省 = 关闭会话）
  const submitAuthRetry = (password?: string) => {
    if (!authRetry) return
    if (password) {
      setAuthWaiting(true)
      agentSocket.sendControl({ t: 'retry-auth', s: authRetry.sessionId, m: { password } })
    } else {
      agentSocket.sendControl({ t: 'retry-auth', s: authRetry.sessionId, m: {} })
      setAuthRetry(null)
      setAuthWaiting(false)
    }
  }

  const onHostAction = (host: HostRecord, action: string) => {
    if (action === 'shell' || action === 'sftp' || action === 'metrics') openSession(host, action)
    else if (action === 'edit') setEditing({ open: true, host })
    else if (action === 'delete') {
      Modal.confirm({
        title: `删除主机档案「${host.name}」？`,
        onOk: () => agentSocket.sendControl({ t: 'hosts', m: { op: 'delete', id: host.id } }),
      })
    }
  }

  return (
    <div className="app-shell">
      <TitleBar />
      <Layout style={{ flex: 1, minHeight: 0 }}>
        {!termFocus && (
          <Layout.Sider width={280} className="bgshell-sider">
            <div style={{ display: 'flex', flexDirection: 'column', height: '100%' }}>
              <div style={{ flex: 1, minHeight: 0 }}>
                <HostList
                  hosts={hosts}
                  hostStatus={hostStatus}
                  onAction={onHostAction}
                  onAdd={() => setEditing({ open: true })}
                  onRefresh={refreshHosts}
                />
              </div>
              <AgentControls
                onOpenSettings={() => setSettingsOpen(true)}
                onOpenChat={__BGSH_PET__ ? undefined : () => setChatOpen(true)}
              />
            </div>
          </Layout.Sider>
        )}
      <Layout.Content className="bgshell-content">
        {tabs.length === 0 ? (
          <div className="bgshell-empty">
            <div className="bgshell-empty-icon">
              <CodeOutlined />
            </div>
            <div className="bgshell-empty-title">选择一个主机开始</div>
            <div className="bgshell-empty-hint">
              单击左侧主机打开终端；通过主机菜单可进入「文件管理」或「运行状态」。
            </div>
          </div>
        ) : (
          <Tabs
            className={termFocus ? 'bgshell-tabs bgshell-tabs-focus' : 'bgshell-tabs'}
            type="editable-card"
            hideAdd
            activeKey={activeKey}
            onChange={setActiveKey}
            onEdit={(targetKey, action) => {
              if (action === 'remove') {
                const sid = String(targetKey)
                agentSocket.sendControl({ t: 'close', s: sid, m: { reason: 'user' } })
                setTabs((prev) => prev.filter((t) => t.key !== sid))
              }
            }}
            items={tabs.map((t) => ({
              key: t.key,
              label: (
                <span>
                  {KIND_ICON[t.kind]} {t.hostName}
                  {KIND_LABEL[t.kind]}
                </span>
              ),
              children:
                t.kind === 'shell' ? (
                  <TerminalPane
                    sessionId={t.key}
                    hostName={t.hostName}
                    focusMode={termFocus}
                    onFocusToggle={() => setTermFocus((prev) => !prev)}
                    onFilesDrop={(files) =>
                      startTermUpload(
                        t.key,
                        files.map((f) => ({ name: f.name, sizeB: f.size, file: f })),
                      )
                    }
                  />
                ) : t.kind === 'sftp' ? (
                  <SftpPane sessionId={t.key} />
                ) : (
                  <MetricsPane sessionId={t.key} />
                ),
            }))}
          />
        )}
      </Layout.Content>
      <HostEditDrawer
        open={editing.open}
        host={editing.host}
        onClose={() => setEditing({ open: false })}
        onSaved={() => {
          setEditing({ open: false })
          refreshHosts()
        }}
      />
      <Modal
        open={!!authRetry}
        title={authRetry ? `SSH 认证失败：${authRetry.username}@${authRetry.host}` : ''}
        okText="重试连接"
        cancelText="取消"
        okButtonProps={{ disabled: !authPassword || authWaiting, loading: authWaiting }}
        cancelButtonProps={{ disabled: authWaiting }}
        maskClosable={false}
        keyboard={false}
        onOk={() => submitAuthRetry(authPassword)}
        onCancel={() => submitAuthRetry()}
      >
        <p style={{ color: 'var(--text-muted)' }}>
          {authRetry?.message}，请重新输入 {authRetry?.username} 的密码后重试。
        </p>
        <Input.Password
          key={authRetry?.attempt}
          autoFocus
          placeholder="密码"
          value={authPassword}
          onChange={(e) => setAuthPassword(e.target.value)}
          onPressEnter={() => authPassword && !authWaiting && submitAuthRetry(authPassword)}
        />
      </Modal>
      <SettingsModal open={settingsOpen} onClose={() => setSettingsOpen(false)} />
      {aiCmdOpen && aiCommands.length > 0 && (
        <AiCommandPanel
          commands={aiCommands}
          terminals={tabs.filter((t) => t.kind === 'shell').map((t) => ({ key: t.key, hostName: t.hostName }))}
          activeKey={activeKey}
          onClose={() => setAiCmdOpen(false)}
          onClear={() => setAiCommands([])}
        />
      )}
      {termUpload && <TermDropUploadCard view={termUpload} onClose={() => setTermUpload(null)} />}
      {/* SSH 精简版：AI 对话面板常驻挂载（关闭仅不渲染），审批卡片/敏感输入也在此处理 */}
      {ChatPanel && (
        <div className="wb-chat-wrap">
          <Suspense fallback={null}>
            <ChatPanel open={chatOpen} onClose={() => setChatOpen(false)} incomingAsk={askReq} />
          </Suspense>
        </div>
      )}
      <DropUploadModal
        open={!!dropUpload}
        paths={dropUpload?.paths ?? []}
        hosts={hosts}
        onClose={() => setDropUpload(null)}
        onOpenSftp={(hostId) => {
          const host = hosts.find((h) => h.id === hostId)
          if (host) openSession(host, 'sftp')
        }}
      />
      </Layout>
    </div>
  )
}
