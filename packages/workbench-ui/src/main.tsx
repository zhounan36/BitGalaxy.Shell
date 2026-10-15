import ReactDOM from 'react-dom/client'
import { ConfigProvider, theme } from 'antd'
import zhCN from 'antd/locale/zh_CN'
import '@xterm/xterm/css/xterm.css'
import './index.css'
// AI 对话面板样式（共享）：SSH 精简版（无桌宠）工作台内嵌 ChatPanel 需要
import './styles/chat.css'
import { App } from './App'
import { themeTokens } from './theme'

ReactDOM.createRoot(document.getElementById('root')!).render(
  <ConfigProvider locale={zhCN} theme={{ algorithm: theme.darkAlgorithm, token: themeTokens }}>
    <App />
  </ConfigProvider>,
)
