import { useCallback, useEffect, useRef, useState } from 'react'
import { Layout, Modal, Tabs, message } from 'antd'
import { CodeOutlined, DashboardOutlined, FolderOutlined } from '@ant-design/icons'
import type { HostRecord, SessionKind } from '@bgshell/protocol'
import { agentSocket } from './api/ws'
import { AgentControls } from './components/AgentControls'
import { DropUploadModal } from './components/DropUploadModal'
import { HostEditDrawer } from './components/HostEditDrawer'
import { HostList, type HostStatus } from './components/HostList'
import { MetricsPane } from './components/MetricsPane'
import { SettingsModal } from './components/SettingsModal'
import { SftpPane } from './components/SftpPane'
import { TerminalPane } from './components/TerminalPane'

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

export function App() {
  const [connected, setConnected] = useState(false)
  const [hosts, setHosts] = useState<HostRecord[]>([])
  const [hostStatus, setHostStatus] = useState<Record<string, HostStatus | undefined>>({})
  const [tabs, setTabs] = useState<SessionTab[]>([])
  const [activeKey, setActiveKey] = useState<string>()
  const [editing, setEditing] = useState<{ open: boolean; host?: HostRecord }>({ open: false })
  const [settingsOpen, setSettingsOpen] = useState(false)

  const refreshHosts = useCallback(() => agentSocket.sendControl({ t: 'hosts', m: { op: 'list' } }), [])

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
        case 'session-closed':
          setTabs((prev) => prev.filter((t) => t.key !== frame.s))
          message.info(`会话已关闭（${frame.m.reason}）`)
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
                <p style={{ color: '#888' }}>
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
          // 建连失败关闭页签；运行期错误（metrics-failed 等）仅提示，由面板自行呈现
          message.error(frame.m.message)
          if (frame.s && frame.m.code === 'open-failed') {
            setTabs((prev) => prev.filter((t) => t.key !== frame.s))
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
    <Layout style={{ height: '100%' }}>
      <Layout.Sider width={280} theme="light" style={{ borderRight: '1px solid #f0f0f0' }}>
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
          <AgentControls onOpenSettings={() => setSettingsOpen(true)} />
        </div>
      </Layout.Sider>
      <Layout.Content style={{ background: '#1e1e1e' }}>
        {tabs.length === 0 ? (
          <div
            style={{ color: '#888', height: '100%', display: 'flex', alignItems: 'center', justifyContent: 'center' }}
          >
            在左侧选择主机：单击开终端，菜单可打开 文件管理 / 运行状态
          </div>
        ) : (
          <Tabs
            className="bgshell-tabs"
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
                  <TerminalPane sessionId={t.key} />
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
      <SettingsModal open={settingsOpen} onClose={() => setSettingsOpen(false)} />
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
  )
}
