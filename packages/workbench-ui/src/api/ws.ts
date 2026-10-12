import {
  DataFrameType,
  decodeDataFrame,
  decodeOpPayload,
  encodeDataFrame,
  encodeOpPayload,
  type ClientFrame,
  type ServerFrame,
} from '@bgshell/protocol'

type ControlHandler = (frame: ServerFrame) => void
type DataHandler = (sid: string, type: number, payload: Uint8Array) => void
type ChunkHandler = (chunk: Uint8Array) => void

/** 与本机 Agent 的 WebSocket 通道（生产：Agent 同源 /ws；开发：经 vite 代理） */
export class AgentSocket {
  private ws: WebSocket | null = null
  private readonly controlHandlers = new Set<ControlHandler>()
  private readonly dataHandlers = new Set<DataHandler>()
  private readonly downloadHandlers = new Map<string, ChunkHandler>()

  get ready(): boolean {
    return this.ws?.readyState === WebSocket.OPEN
  }

  connect(): Promise<void> {
    if (this.ready) return Promise.resolve()
    return new Promise((resolve, reject) => {
      // Tauri 壳宿主（tauri.localhost）与 Agent 不同源：直连本机 Agent 端口；
      // dev（vite）与浏览器宿主（Agent 内嵌工作台）保持同源。
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
      }
      ws.onmessage = (ev) => {
        if (typeof ev.data === 'string') {
          const frame = JSON.parse(ev.data) as ServerFrame
          this.controlHandlers.forEach((h) => h(frame))
        } else {
          const { type, sid, payload } = decodeDataFrame(new Uint8Array(ev.data))
          if (type === DataFrameType.SftpDownload) {
            const { opId, chunk } = decodeOpPayload(payload)
            this.downloadHandlers.get(opId)?.(chunk)
          } else {
            this.dataHandlers.forEach((h) => h(sid, type, payload))
          }
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

  /** 注册下载块回调（按 opId），返回注销函数 */
  registerDownloadChunk(opId: string, cb: ChunkHandler): () => void {
    this.downloadHandlers.set(opId, cb)
    return () => this.downloadHandlers.delete(opId)
  }

  sendControl(frame: ClientFrame): void {
    this.ws?.send(JSON.stringify(frame))
  }

  sendInput(sid: string, data: Uint8Array): void {
    // encodeDataFrame/TextEncoder 产物独占整段 buffer，可直接发 buffer（DOM send 需要 ArrayBuffer）
    this.ws?.send(encodeDataFrame(DataFrameType.Stdin, sid, data).buffer as ArrayBuffer)
  }

  /** SFTP 上传块（含发送缓冲背压：超过 8MB 暂停再发，避免大文件占爆内存） */
  async sendSftpChunk(sid: string, opId: string, chunk: Uint8Array): Promise<void> {
    while (this.ws && this.ws.bufferedAmount > 8 * 1024 * 1024) {
      await new Promise((r) => setTimeout(r, 25))
    }
    const frame = encodeDataFrame(DataFrameType.SftpUpload, sid, encodeOpPayload(opId, chunk))
    this.ws?.send(frame.buffer as ArrayBuffer)
  }
}

export const agentSocket = new AgentSocket()
