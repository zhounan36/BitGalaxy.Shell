import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { App } from './App'
import { isTauri } from './api/tauri'
import './index.css'

// Tauri 透明窗宿主：标记 + 移除预览网格底
if (isTauri) document.documentElement.classList.add('in-tauri')

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
