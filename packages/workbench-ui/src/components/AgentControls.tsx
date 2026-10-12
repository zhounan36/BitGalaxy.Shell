import { Button } from 'antd'
import { SettingOutlined } from '@ant-design/icons'

/** 侧边栏底部：设置入口（打开 SettingsModal，替代原散落的开关组） */
export function AgentControls({ onOpenSettings }: { onOpenSettings: () => void }) {
  return (
    <div className="agent-controls">
      <Button size="small" block icon={<SettingOutlined />} onClick={onOpenSettings}>
        设置
      </Button>
    </div>
  )
}
