import ReactDOM from 'react-dom/client'
import { ConfigProvider, theme } from 'antd'
import zhCN from 'antd/locale/zh_CN'
import '../index.css'
import '../styles/chat.css'
import { themeTokens } from '../theme'
import { SqlApp } from './SqlApp'

ReactDOM.createRoot(document.getElementById('root')!).render(
  <ConfigProvider locale={zhCN} theme={{ algorithm: theme.darkAlgorithm, token: themeTokens }}>
    <SqlApp />
  </ConfigProvider>,
)
