import { useCallback, useEffect, useState } from 'react'
import { Button, Empty, Tabs, Typography, message } from 'antd'
import { PlusOutlined, ReloadOutlined, UsbOutlined } from '@ant-design/icons'
import type { SerialOptions, SerialPortInfo } from '@bgshell/protocol'
import { agentSocket } from '../api/ws'
import { SerialConnectModal } from '../components/SerialConnectModal'
import { SerialDebugPane } from '../components/SerialDebugPane'

interface SerialTab {
  key: string
  opts: SerialOptions
}

/** 串口独立应用入口：多页签串口调试（每个 COM 口一个页签）。
 *  后端为独立 WS 服务（7773，serial 入口 HTML 内嵌 __BGSH_WS__ 注入）。 */
export function SerialApp() {
  const [tabs, setTabs] = useState<SerialTab[]>([])
  const [activeKey, setActiveKey] = useState<string>()
  const [connOpen, setConnOpen] = useState(false)
  const [ports, setPorts] = useState<SerialPortInfo[]>([])
  const [portsError, setPortsError] = useState<string>()

  useEffect(() => {
    void agentSocket.connect().then(() => {
      agentSocket.sendControl({ t: 'serial-list' })
    })
    const off = agentSocket.onControl((frame) => {
      switch (frame.t) {
        case 'serial-ports':
          setPorts(frame.m.ports ?? [])
          setPortsError(frame.m.ok ? undefined : frame.m.message)
          break
        case 'session-closed':
          setTabs((prev) => prev.filter((t) => t.key !== frame.s))
          message.info(`串口已关闭（${frame.m.reason}）`)
          break
      }
    })
    // 无页签时自动弹连接窗（独立应用直达串口选择）
    setConnOpen(true)
    return off
  }, [])

  const refreshPorts = useCallback(() => {
    agentSocket.sendControl({ t: 'serial-list' })
  }, [])

  const openSession = useCallback((opts: SerialOptions) => {
    const sessionId = crypto.randomUUID()
    setTabs((prev) => [...prev, { key: sessionId, opts }])
    setActiveKey(sessionId)
    setConnOpen(false)
    agentSocket.sendControl({
      t: 'open',
      s: sessionId,
      m: { hostId: 'serial', kind: 'serial', serial: opts },
    })
  }, [])

  const closeTab = useCallback((key: string) => {
    agentSocket.sendControl({ t: 'close', s: key, m: { reason: 'user' } })
  }, [])

  return (
    <div className="app-shell" style={{ display: 'flex', flexDirection: 'column', height: '100vh' }}>
      {/* 顶栏：标题 + 新建/刷新 */}
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: 8,
          padding: '8px 12px',
          borderBottom: '1px solid var(--border-subtle)',
          background: 'var(--bg-base)',
        }}
      >
        <UsbOutlined style={{ color: 'var(--accent)' }} />
        <Typography.Text strong>BitGalaxy Serial</Typography.Text>
        <div style={{ flex: 1 }} />
        <Button size="small" icon={<ReloadOutlined />} onClick={refreshPorts}>
          刷新端口
        </Button>
        <Button size="small" type="primary" icon={<PlusOutlined />} onClick={() => setConnOpen(true)}>
          打开串口
        </Button>
      </div>

      {/* 页签主体：每个 COM 口一个调试页签 */}
      <div style={{ flex: 1, overflow: 'hidden', minHeight: 0 }}>
        {tabs.length === 0 ? (
          <Empty
            style={{ marginTop: 80 }}
            description="尚未打开串口"
            image={Empty.PRESENTED_IMAGE_SIMPLE}
          >
            <Button type="primary" icon={<PlusOutlined />} onClick={() => setConnOpen(true)}>
              打开串口
            </Button>
          </Empty>
        ) : (
          <Tabs
            type="editable-card"
            hideAdd
            activeKey={activeKey}
            onChange={setActiveKey}
            onEdit={(key, action) => {
              if (action === 'remove' && typeof key === 'string') closeTab(key)
            }}
            items={tabs.map((t) => ({
              key: t.key,
              label: `${t.opts.port} @ ${t.opts.baudRate}`,
              children: (
                <SerialDebugPane
                  sessionId={t.key}
                  portName={t.opts.port}
                  baudRate={t.opts.baudRate}
                  initDtr={t.opts.dtr ?? true}
                  initRts={t.opts.rts ?? true}
                />
              ),
            }))}
            style={{ height: '100%', paddingInline: 8 }}
          />
        )}
      </div>

      <SerialConnectModal
        open={connOpen}
        ports={ports}
        error={portsError}
        onClose={() => setConnOpen(false)}
        onRefresh={refreshPorts}
        onConnect={openSession}
      />
    </div>
  )
}
