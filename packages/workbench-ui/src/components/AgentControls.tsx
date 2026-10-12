import { useEffect, useState } from 'react'
import { Button, Modal, Switch, message } from 'antd'
import type { AgentResult } from '@bgshell/protocol'
import { agentSocket } from '../api/ws'

/** Agent 自身控制：开机自启 / 启动时打开工作台 / 退出 Agent（桌面形态由工作台代替托盘入口） */
export function AgentControls({ connected }: { connected: boolean }) {
  const [info, setInfo] = useState<AgentResult>()

  // 连接建立后查询 Agent 状态
  useEffect(() => {
    if (connected) agentSocket.sendControl({ t: 'agent', m: { op: 'get-info' } })
  }, [connected])

  useEffect(() => {
    const off = agentSocket.onControl((frame) => {
      if (frame.t !== 'agent-result') return
      const m = frame.m
      setInfo(m)
      if (!m.ok && m.message) message.error(m.message)
      // 设置类操作后回查，保证开关状态与实际一致
      if (m.op === 'set-autostart' || m.op === 'set-open-on-start') {
        agentSocket.sendControl({ t: 'agent', m: { op: 'get-info' } })
      }
    })
    return off
  }, [])

  return (
    <div style={{ borderTop: '1px solid #f0f0f0', padding: '10px 12px', display: 'flex', flexDirection: 'column', gap: 10 }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
        <span style={{ fontSize: 13 }}>开机自启</span>
        <Switch
          size="small"
          checked={!!info?.autostart}
          disabled={!connected}
          onChange={(enabled) =>
            agentSocket.sendControl({ t: 'agent', m: { op: 'set-autostart', enabled } })
          }
        />
      </div>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
        <span style={{ fontSize: 13 }}>启动时打开工作台</span>
        <Switch
          size="small"
          checked={info?.openOnStart ?? true}
          disabled={!connected}
          onChange={(enabled) =>
            agentSocket.sendControl({ t: 'agent', m: { op: 'set-open-on-start', enabled } })
          }
        />
      </div>
      {info?.version && (
        <div style={{ fontSize: 12, color: '#999' }}>Agent v{info.version}</div>
      )}
      <Button
        size="small"
        danger
        block
        disabled={!connected}
        onClick={() =>
          Modal.confirm({
            title: '退出本机 Agent？',
            content: '已打开的终端 / 文件 / 状态会话将全部断开。',
            okText: '退出',
            cancelText: '取消',
            onOk: () => agentSocket.sendControl({ t: 'agent', m: { op: 'shutdown' } }),
          })
        }
      >
        退出 Agent
      </Button>
    </div>
  )
}
