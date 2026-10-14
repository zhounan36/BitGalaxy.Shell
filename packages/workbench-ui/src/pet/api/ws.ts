import {
  DataFrameType,
  decodeDataFrame,
  type ClientFrame,
  type ServerFrame,
} from '@bgshell/protocol'

type ControlHandler = (frame: ServerFrame) => void
type DataHandler = (sid: string, type: number, payload: Uint8Array) => void

/**
 * 与本机 Agent 的 WebSocket 通道。
 * 与 workbench-ui/src/api/ws.ts 同协议的轻量实现（宠物宿主只消费事件流，
 * 不需要 SFTP 下载分流与背压发送，故独立成份）。
 * 生产（Tauri 壳）：与 Agent 同源 /ws；开发：经 vite 代理到 127.0.0.1:7777。
 */
export class AgentSocket {
  private ws: WebSocket | null = null
  /** 在建连接的 promise 单例：并发 connect 复用同一次建连 */
  private connectPromise: Promise<void> | null = null
  private readonly controlHandlers = new Set<ControlHandler>()
  private readonly dataHandlers = new Set<DataHandler>()
  private reconnectTimer: number | undefined

  get ready(): boolean {
    return this.ws?.readyState === WebSocket.OPEN
  }

  connect(): Promise<void> {
    if (this.ready) return Promise.resolve()
    // 建连去重：并发 connect（StrictMode 双挂载 / 多组件同时 connect / 断线重连排程
    // 与用户操作撞车）必须复用在建的那一条。否则会繁殖多条 WebSocket，各自绑定
    // onmessage 向同一组 handlers 分发 → 每帧事件重复处理 N 次（表现为
    // 工具卡片成对重复、单次停止弹多条「已终止」）。
    if (this.connectPromise) return this.connectPromise
    // 旧连接（OPEN/CONNECTING/半开）：摘掉回调并关闭。
    if (this.ws) {
      const stale = this.ws
      stale.onopen = null
      stale.onmessage = null
      stale.onerror = null
      stale.onclose = null
      try {
        stale.close()
      } catch {
        // 忽略关闭异常
      }
      this.ws = null
    }
    window.clearTimeout(this.reconnectTimer)
    const p = new Promise<void>((resolve, reject) => {
      // Tauri 壳宿主（tauri.localhost）与 Agent 不同源：直连本机 Agent 端口；
      // dev（vite）与浏览器宿主保持原样（同源 / 经代理）。
      const wsUrl =
        location.hostname === 'tauri.localhost' || location.protocol === 'tauri:'
          ? 'ws://127.0.0.1:7777/ws'
          : `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws`
      const ws = new WebSocket(wsUrl)
      ws.binaryType = 'arraybuffer'
      // 创建即占位（不等 onopen）：CONNECTING 窗口内进来的并发 connect 才能看见它
      this.ws = ws
      ws.onopen = () => {
        resolve()
      }
      ws.onerror = () => reject(new Error('无法连接本机 Agent（请确认 agent 已启动）'))
      ws.onclose = () => {
        // 只清理自己：避免迟到 close 误清新连接的引用
        if (this.ws === ws) {
          this.ws = null
        }
        this.controlHandlers.forEach((h) =>
          h({ t: 'error', m: { code: 'disconnected', message: '与 Agent 的连接已断开' } }),
        )
        // 桌宠长驻：断线后自动重连（5s 退避）
        this.reconnectTimer = window.setTimeout(() => {
          this.connect().catch(() => {})
        }, 5000)
      }
      ws.onmessage = (ev) => {
        if (typeof ev.data === 'string') {
          const frame = JSON.parse(ev.data) as ServerFrame
          this.controlHandlers.forEach((h) => h(frame))
        } else {
          const { type, sid, payload } = decodeDataFrame(new Uint8Array(ev.data))
          this.dataHandlers.forEach((h) => h(sid, type, payload))
        }
      }
    })
    this.connectPromise = p
    // settle 后释放单例（吞掉 rejection 避免未处理异常），下次 connect 重新发起
    p.catch(() => {}).then(() => {
      if (this.connectPromise === p) {
        this.connectPromise = null
      }
    })
    return p
  }

  onControl(h: ControlHandler): () => void {
    this.controlHandlers.add(h)
    return () => this.controlHandlers.delete(h)
  }

  onData(h: DataHandler): () => void {
    this.dataHandlers.add(h)
    return () => this.dataHandlers.delete(h)
  }

  sendControl(frame: ClientFrame): void {
    this.ws?.send(JSON.stringify(frame))
  }
}

export const agentSocket = new AgentSocket()
