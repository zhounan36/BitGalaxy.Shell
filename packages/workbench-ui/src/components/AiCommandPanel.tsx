import { Tag, message } from 'antd'
import { ClearOutlined, CloseOutlined, ThunderboltOutlined } from '@ant-design/icons'
import type { AiCommand } from '@bgshell/protocol'
import { writeClipboardText } from '../api/clipboard'
import { pasteIntoTerminal } from './TerminalPane'

interface AiCommandPanelProps {
  commands: AiCommand[]
  /** 打开中的终端会话（key = sessionId） */
  terminals: { key: string; hostName: string }[]
  activeKey?: string
  onClose: () => void
  onClear: () => void
}

/** AI 候选命令面板：propose_command 推来的命令列表，固定在工作台右下角。
 *  AI 没有执行权限，命令由用户自己执行——点击列表项粘贴到对应 SSH 终端
 *  （尊重括号粘贴模式，不自动执行），target=local 的复制到剪贴板。 */
export function AiCommandPanel({ commands, terminals, activeKey, onClose, onClear }: AiCommandPanelProps) {
  if (commands.length === 0) return null

  const applyCommand = (cmd: AiCommand) => {
    if (cmd.target === 'local') {
      // 本机命令不自动进终端：复制到剪贴板，用户自己开 PowerShell 执行
      void writeClipboardText(cmd.command).then(
        () => message.success('已复制到剪贴板：粘贴到 PowerShell 执行'),
        () => message.error('复制失败，请手动复制命令'),
      )
      return
    }
    // host 优先精确匹配已打开终端的主机名；无匹配落到当前活动终端
    const target =
      (cmd.host ? terminals.find((t) => t.hostName === cmd.host) : undefined) ??
      terminals.find((t) => t.key === activeKey) ??
      terminals[0]
    if (!target) {
      message.warning('没有打开的 SSH 终端，请先连接主机')
      return
    }
    if (cmd.host && cmd.host !== target.hostName) {
      message.warning(`命令目标主机是「${cmd.host}」，将粘贴到「${target.hostName}」终端，请核对`)
    }
    if (pasteIntoTerminal(target.key, cmd.command)) {
      message.info(`已粘贴到「${target.hostName}」终端，按回车执行`)
    } else {
      message.error('粘贴失败：该终端会话可能已断开')
    }
  }

  return (
    <div className="ai-cmd-panel">
      <div className="ai-cmd-head">
        <ThunderboltOutlined style={{ color: 'var(--accent)' }} />
        <span className="ai-cmd-title">AI 候选命令</span>
        <span className="ai-cmd-hint">点击粘贴 · 回车执行</span>
        <button type="button" className="ai-cmd-btn" title="清空列表" onClick={onClear}>
          <ClearOutlined />
        </button>
        <button type="button" className="ai-cmd-btn" title="收起（新命令会重新弹出）" onClick={onClose}>
          <CloseOutlined />
        </button>
      </div>
      <div className="ai-cmd-list">
        {commands.map((c) => (
          <button key={c.id} type="button" className="ai-cmd-item" onClick={() => applyCommand(c)}>
            <code className="ai-cmd-text" title={c.purpose ?? c.command}>
              {c.command}
            </code>
            <span className="ai-cmd-meta">
              {c.target === 'local' ? (
                <Tag color="gold" style={{ marginInlineEnd: 0 }}>本机</Tag>
              ) : (
                <Tag color="blue" style={{ marginInlineEnd: 0 }}>{c.host ?? 'SSH'}</Tag>
              )}
              {c.purpose && <span className="ai-cmd-purpose">{c.purpose}</span>}
            </span>
          </button>
        ))}
      </div>
    </div>
  )
}
