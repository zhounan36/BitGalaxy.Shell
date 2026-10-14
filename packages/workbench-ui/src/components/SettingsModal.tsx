import { useEffect, useState } from 'react'
import { Button, Input, Modal, Switch, Tabs, message } from 'antd'
import { CopyOutlined, InfoCircleOutlined, MailOutlined, RobotOutlined, SettingOutlined } from '@ant-design/icons'
import type { AgentResult } from '@bgshell/protocol'
import { agentSocket } from '../api/ws'
import { writeClipboardText } from '../api/clipboard'

const CONTACT_EMAIL = 'zhounan36@163.com'

type SettingsItems = Record<string, unknown>

/** 配置弹窗：通用设置 / AI 助手 / 关于 */
export function SettingsModal({ open, onClose }: { open: boolean; onClose: () => void }) {
  const [items, setItems] = useState<SettingsItems>({})
  const [agentInfo, setAgentInfo] = useState<AgentResult>()
  // AI 配置本地草稿（保存时才提交）
  const [aiKey, setAiKey] = useState('')
  const [aiBaseUrl, setAiBaseUrl] = useState('')
  const [aiModel, setAiModel] = useState('')
  // AI 配置内联校验错误
  const [aiKeyErr, setAiKeyErr] = useState<string>()
  const [aiBaseUrlErr, setAiBaseUrlErr] = useState<string>()
  const [aiModelErr, setAiModelErr] = useState<string>()

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

  // AI 配置保存：先做内联校验，通过才提交
  const saveAi = () => {
    const key = aiKey.trim()
    const base = aiBaseUrl.trim()
    const model = aiModel.trim()

    let ok = true
    if (key && /\s/.test(key)) {
      setAiKeyErr('API Key 不应包含空格')
      ok = false
    } else {
      setAiKeyErr(undefined)
    }
    if (base && !/^https?:\/\/\S+$/i.test(base)) {
      setAiBaseUrlErr('请输入合法 URL，如 https://api.deepseek.com')
      ok = false
    } else {
      setAiBaseUrlErr(undefined)
    }
    if (model && /\s/.test(model)) {
      setAiModelErr('模型名不应包含空格')
      ok = false
    } else {
      setAiModelErr(undefined)
    }
    if (!ok) return

    agentSocket.sendControl({ t: 'settings', m: { op: 'set', key: 'ai.apiKey', value: key } })
    agentSocket.sendControl({ t: 'settings', m: { op: 'set', key: 'ai.baseUrl', value: base } })
    agentSocket.sendControl({ t: 'settings', m: { op: 'set', key: 'ai.model', value: model } })
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
                  <div style={{ marginBottom: 4, fontSize: 13, color: 'var(--text-secondary)' }}>API Key</div>
                  <Input.Password
                    value={aiKey}
                    onChange={(e) => {
                      setAiKey(e.target.value)
                      setAiKeyErr(undefined)
                    }}
                    status={aiKeyErr ? 'error' : undefined}
                    placeholder="sk-..."
                  />
                  {aiKeyErr && <div className="field-error">{aiKeyErr}</div>}
                </div>
                <div>
                  <div style={{ marginBottom: 4, fontSize: 13, color: 'var(--text-secondary)' }}>
                    Base URL（默认 https://api.deepseek.com）
                  </div>
                  <Input
                    value={aiBaseUrl}
                    onChange={(e) => {
                      setAiBaseUrl(e.target.value)
                      setAiBaseUrlErr(undefined)
                    }}
                    status={aiBaseUrlErr ? 'error' : undefined}
                    placeholder="https://api.deepseek.com"
                  />
                  {aiBaseUrlErr && <div className="field-error">{aiBaseUrlErr}</div>}
                </div>
                <div>
                  <div style={{ marginBottom: 4, fontSize: 13, color: 'var(--text-secondary)' }}>
                    模型（默认 deepseek-chat）
                  </div>
                  <Input
                    value={aiModel}
                    onChange={(e) => {
                      setAiModel(e.target.value)
                      setAiModelErr(undefined)
                    }}
                    status={aiModelErr ? 'error' : undefined}
                    placeholder="deepseek-chat"
                  />
                  {aiModelErr && <div className="field-error">{aiModelErr}</div>}
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
                  <span style={{ color: 'var(--text-muted)' }}>
                    Agent v{agentInfo?.version ?? '—'}
                  </span>
                </div>
                <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
                  <span>作者 / Bug 反馈</span>
                  <span style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
                    <span style={{ color: 'var(--text-muted)' }}>{CONTACT_EMAIL}</span>
                    <Button
                      size="small"
                      icon={<CopyOutlined />}
                      onClick={() => {
                        void writeClipboardText(CONTACT_EMAIL)
                          .then(() => message.success('邮箱已复制'))
                          .catch(() => message.error('复制失败，请手动复制'))
                      }}
                    >
                      复制
                    </Button>
                  </span>
                </div>
                <Button
                  block
                  icon={<MailOutlined />}
                  onClick={() => {
                    window.location.href = `mailto:${CONTACT_EMAIL}?subject=${encodeURIComponent(
                      '[BitGalaxy Shell] Bug 反馈',
                    )}&body=${encodeURIComponent(
                      `软件版本：Agent v${agentInfo?.version ?? '—'}\n问题描述：\n复现步骤：\n预期行为：\n`,
                    )}`
                  }}
                >
                  发送 Bug 反馈邮件
                </Button>
                <div style={{ display: 'flex', justifyContent: 'space-between' }}>
                  <span>存储</span>
                  <span style={{ color: 'var(--text-muted)' }}>SQLite（bitgalaxy.db）</span>
                </div>
                <div style={{ fontSize: 12, color: 'var(--text-muted)', marginTop: 8 }}>
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
