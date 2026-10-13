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
  private readonly controlHandlers = new Set<ControlHandler>()
  private readonly dataHandlers = new Set<DataHandler>()
  private reconnectTimer: number | undefined

  get ready(): boolean {
    return this.ws?.readyState === WebSocket.OPEN
  }

  connect(): Promise<void> {
    if (this.ready) return Promise.resolve()
    window.clearTimeout(this.reconnectTimer)
    return new Promise((resolve, reject) => {
      // Tauri 壳宿主（tauri.localhost）与 Agent 不同源：直连本机 Agent 端口；
      // dev（vite）与浏览器宿主保持原样（同源 / 经代理）。
      const wsUrl =
        location.hostname === 'tauri.localhost' || location.protocol === 'tauri:'
          ? 'ws://127.0.0.1:7777/ws'
          : `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws`
      const ws = new WebSocket(wsUrl)
      ws.binaryType = 'arraybuffer'
      ws.onopen = () => {
        this.ws = ws
        resolve()
      }
      ws.onerror = () => reject(new Error('无法连接本机 Agent（请确认 agent 已启动）'))
      ws.onclose = () => {
        this.ws = null
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
