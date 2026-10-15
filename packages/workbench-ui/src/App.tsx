import { useCallback } from 'react'
import { Button, Space } from 'antd'
import { DatabaseOutlined, UsbOutlined } from '@ant-design/icons'
import { launchTool } from './pet/api/tauri'

/** AI 基座兜底页（P6）：基座是后台服务（托盘 + 桌宠），正常情况下没有窗口加载本页。
 *  仅当异常直开 / 调试时可见：提示基座状态并提供三工具拉起入口（与托盘菜单同款）。 */
export function App() {
  const launch = useCallback((tool: 'ssh' | 'sql' | 'serial') => {
    void launchTool(tool)
  }, [])

  return (
    <div
      style={{
        height: '100vh',
        display: 'flex',
        flexDirection: 'column',
        alignItems: 'center',
        justifyContent: 'center',
        gap: 16,
      }}
    >
      <div style={{ fontSize: 20, fontWeight: 600 }}>BitGalaxy Shell · AI 基座运行中</div>
      <div style={{ color: 'var(--text-muted, #888)', textAlign: 'center', lineHeight: 1.8 }}>
        基座为后台服务：对话请用右下角桌宠或各工具内的 AI 侧栏；
        <br />
        退出请用系统托盘菜单。
      </div>
      <Space>
        <Button type="primary" onClick={() => launch('ssh')}>
          SSH 终端
        </Button>
        <Button icon={<DatabaseOutlined />} onClick={() => launch('sql')}>
          SQL 客户端
        </Button>
        <Button icon={<UsbOutlined />} onClick={() => launch('serial')}>
          串口工具
        </Button>
      </Space>
    </div>
  )
}
