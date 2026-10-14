import { Button } from 'antd'
import { RobotOutlined, SettingOutlined } from '@ant-design/icons'

/** 侧边栏底部：设置入口 + AI 助手入口（onOpenChat 仅 SSH 精简版传入：完整版 AI 对话在宠物窗） */
export function AgentControls({
  onOpenSettings,
  onOpenChat,
}: {
  onOpenSettings: () => void
  onOpenChat?: () => void
}) {
  return (
    <div className="agent-controls">
      {onOpenChat && (
        <Button size="small" block icon={<RobotOutlined />} onClick={onOpenChat}>
          AI 助手
        </Button>
      )}
      <Button size="small" block icon={<SettingOutlined />} onClick={onOpenSettings}>
        设置
      </Button>
    </div>
  )
}
