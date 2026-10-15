import ReactDOM from 'react-dom/client'
import { ConfigProvider, theme } from 'antd'
import zhCN from 'antd/locale/zh_CN'
import '@xterm/xterm/css/xterm.css'
import '../index.css'
import '../styles/chat.css'
import { themeTokens } from '../theme'
import { SshApp } from './SshApp'

ReactDOM.createRoot(document.getElementById('root')!).render(
  <ConfigProvider locale={zhCN} theme={{ algorithm: theme.darkAlgorithm, token: themeTokens }}>
    <SshApp />
  </ConfigProvider>,
)
