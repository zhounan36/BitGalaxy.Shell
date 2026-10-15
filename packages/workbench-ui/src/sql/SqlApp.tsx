import { useEffect, useRef } from 'react'
import { agentSocket } from '../api/ws'
import { SqlPane } from '../components/SqlPane'

/** SQL 独立应用入口：全屏 SqlPane（连接管理 + 结构树 + 多标签编辑器）。
 *  后端为独立 WS 服务（7772，sql 入口 HTML 内嵌 __BGSH_WS__ 注入）；
 *  会话 id 进程内固定（窗口关闭 = WS 断开 = 后端 close_all 兜底清理）。 */
export function SqlApp() {
  const sidRef = useRef(`sql-app-${crypto.randomUUID()}`)
  useEffect(() => {
    void agentSocket.connect()
  }, [])
  return (
    <div style={{ height: '100vh', overflow: 'hidden' }}>
      <SqlPane sessionId={sidRef.current} />
    </div>
  )
}
