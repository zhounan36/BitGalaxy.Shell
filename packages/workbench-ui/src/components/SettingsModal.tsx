import { useEffect, useState } from 'react'
import { Button, Input, Modal, Switch, Tabs, message } from 'antd'
import { InfoCircleOutlined, RobotOutlined, SettingOutlined } from '@ant-design/icons'
import type { AgentResult } from '@bgshell/protocol'
import { agentSocket } from '../api/ws'

type SettingsItems = Record<string, unknown>

/** 配置弹窗：通用设置 / AI 助手 / 关于 */
export function SettingsModal({ open, onClose }: { open: boolean; onClose: () => void }) {
  const [items, setItems] = useState<SettingsItems>({})
  const [agentInfo, setAgentInfo] = useState<AgentResult>()
  // AI 配置本地草稿（保存时才提交）
  const [aiKey, setAiKey] = useState('')
  const [aiBaseUrl, setAiBaseUrl] = useState('')
  const [aiModel, setAiModel] = useState('')

  // 打开时加载全部设置 + Agent 信息
  useEffect(() => {
    if (!open) return
    agentSocket.sendControl({ t: 'settings', m: { op: 'list' } })
    agentSocket.sendControl({ t: 'agent', m: { op: 'get-info' } })
  }, [open])

  useEffect(() => {
    const off = agentSocket.onControl((frame) => {
      if (frame.t === 'settings-result' && frame.m.ok && frame.m.items) {
        const data = frame.m.items
        setItems(data)
        // 填充 AI 配置草稿
        setAiKey((data['ai.apiKey'] as string) ?? '')
        setAiBaseUrl((data['ai.baseUrl'] as string) ?? '')
        setAiModel((data['ai.model'] as string) ?? '')
      } else if (frame.t === 'agent-result') {
        setAgentInfo(frame.m)
      }
    })
    return off
  }, [])

  // 通用设置：openOnStart
  const setOpenOnStart = (enabled: boolean) => {
    agentSocket.sendControl({ t: 'settings', m: { op: 'set', key: 'app.openOnStart', value: enabled } })
    agentSocket.sendControl({ t: 'agent', m: { op: 'set-open-on-start', enabled } })
    setItems((prev) => ({ ...prev, 'app.openOnStart': enabled }))
  }

  // AI 配置保存
  const saveAi = () => {
    agentSocket.sendControl({ t: 'settings', m: { op: 'set', key: 'ai.apiKey', value: aiKey } })
    agentSocket.sendControl({ t: 'settings', m: { op: 'set', key: 'ai.baseUrl', value: aiBaseUrl } })
    agentSocket.sendControl({ t: 'settings', m: { op: 'set', key: 'ai.model', value: aiModel } })
    message.success('AI 配置已保存')
  }

  return (
    <Modal
      title="设置"
      open={open}
      onCancel={onClose}
      footer={null}
      width={520}
    >
      <Tabs
        items={[
          {
            key: 'general',
            label: (
              <span>
                <SettingOutlined /> 通用
              </span>
            ),
            children: (
              <div style={{ display: 'flex', flexDirection: 'column', gap: 20, padding: '8px 0' }}>
                <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
                  <span>开机自启</span>
                  <Switch
                    checked={!!agentInfo?.autostart}
                    onChange={(enabled) =>
                      agentSocket.sendControl({ t: 'agent', m: { op: 'set-autostart', enabled } })
                    }
                  />
                </div>
                <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
                  <span>启动时打开工作台</span>
                  <Switch
                    checked={items['app.openOnStart'] !== false}
                    onChange={setOpenOnStart}
                  />
                </div>
                <Button
                  danger
                  block
                  onClick={() => {
                    Modal.confirm({
                      title: '退出本机 Agent？',
                      content: '已打开的终端 / 文件 / 状态会话将全部断开。',
                      okText: '退出',
                      cancelText: '取消',
                      onOk: () => agentSocket.sendControl({ t: 'agent', m: { op: 'shutdown' } }),
                    })
                  }}
                >
                  退出 Agent
                </Button>
              </div>
            ),
          },
          {
            key: 'ai',
            label: (
              <span>
                <RobotOutlined /> AI 助手
              </span>
            ),
            children: (
              <div style={{ display: 'flex', flexDirection: 'column', gap: 16, padding: '8px 0' }}>
                <div>
                  <div style={{ marginBottom: 4, fontSize: 13, color: '#666' }}>API Key</div>
                  <Input.Password
                    value={aiKey}
                    onChange={(e) => setAiKey(e.target.value)}
                    placeholder="sk-..."
                  />
                </div>
                <div>
                  <div style={{ marginBottom: 4, fontSize: 13, color: '#666' }}>
                    Base URL（默认 https://api.deepseek.com）
                  </div>
                  <Input
                    value={aiBaseUrl}
                    onChange={(e) => setAiBaseUrl(e.target.value)}
                    placeholder="https://api.deepseek.com"
                  />
                </div>
                <div>
                  <div style={{ marginBottom: 4, fontSize: 13, color: '#666' }}>
                    模型（默认 deepseek-chat）
                  </div>
                  <Input
                    value={aiModel}
                    onChange={(e) => setAiModel(e.target.value)}
                    placeholder="deepseek-chat"
                  />
                </div>
                <Button type="primary" onClick={saveAi}>
                  保存 AI 配置
                </Button>
              </div>
            ),
          },
          {
            key: 'about',
            label: (
              <span>
                <InfoCircleOutlined /> 关于
              </span>
            ),
            children: (
              <div style={{ display: 'flex', flexDirection: 'column', gap: 12, padding: '8px 0' }}>
                <div style={{ display: 'flex', justifyContent: 'space-between' }}>
                  <span>版本</span>
                  <span style={{ color: '#999' }}>
                    Agent v{agentInfo?.version ?? '—'}
                  </span>
                </div>
                <div style={{ display: 'flex', justifyContent: 'space-between' }}>
                  <span>存储</span>
                  <span style={{ color: '#999' }}>SQLite（bitgalaxy.db）</span>
                </div>
                <div style={{ fontSize: 12, color: '#bbb', marginTop: 8 }}>
                  全部配置（主机档案 / 已知指纹 / 应用设置 / 审计日志）统一保存在 SQLite 数据库中。
                  首次启动时自动从旧 JSON 文件迁移数据。
                </div>
              </div>
            ),
          },
        ]}
      />
    </Modal>
  )
}
