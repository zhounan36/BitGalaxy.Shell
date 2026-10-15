import ReactDOM from 'react-dom/client'
import { ConfigProvider, theme } from 'antd'
import zhCN from 'antd/locale/zh_CN'
import './index.css'
import { App } from './App'
import { themeTokens } from './theme'

ReactDOM.createRoot(document.getElementById('root')!).render(
  <ConfigProvider locale={zhCN} theme={{ algorithm: theme.darkAlgorithm, token: themeTokens }}>
    <App />
  </ConfigProvider>,
)
