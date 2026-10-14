import ReactDOM from 'react-dom/client'
import { ConfigProvider, theme } from 'antd'
import zhCN from 'antd/locale/zh_CN'
import '@xterm/xterm/css/xterm.css'
import './index.css'
// AI 对话面板样式（共享）：SSH 精简版（无桌宠）工作台内嵌 ChatPanel 需要
import './styles/chat.css'
import { App } from './App'

// 深海蓝暗色令牌：与 src/pet/index.css 保持一致，统一两套 UI 的设计语言
const tokens = {
  colorPrimary: '#3ba6d8',
  colorInfo: '#3ba6d8',
  colorBgBase: '#0d1a2b',
  colorBgContainer: '#12233a',
  colorBgElevated: '#182f4d',
  colorBgLayout: '#0a121f',
  colorTextBase: '#e4eefb',
  colorText: '#e4eefb',
  colorTextSecondary: '#9fb6cf',
  colorTextTertiary: '#677f9c',
  colorBorder: 'rgba(130, 170, 210, 0.20)',
  colorBorderSecondary: 'rgba(130, 170, 210, 0.10)',
  colorLink: '#3ba6d8',
  colorError: '#e05769',
  colorSuccess: '#45c08a',
  colorWarning: '#dd9b34',
  borderRadius: 8,
  fontFamily:
    "'Segoe UI Variable Text', 'Segoe UI', 'PingFang SC', 'Microsoft YaHei', system-ui, -apple-system, sans-serif",
}

ReactDOM.createRoot(document.getElementById('root')!).render(
  <ConfigProvider locale={zhCN} theme={{ algorithm: theme.darkAlgorithm, token: tokens }}>
    <App />
  </ConfigProvider>,
)
