import { createServer, type IncomingMessage, type Server, type ServerResponse } from 'node:http'
import { connect as tcpConnect, type Socket } from 'node:net'
import { join, resolve } from 'node:path'
import { WebSocketServer, type WebSocket } from 'ws'
import {
  DataFrameType,
  decodeDataFrame,
  decodeOpPayload,
  encodeDataFrame,
  type AgentOp,
  type AgentResult,
  type ClientFrame,
  type HostRecord,
  type ServerFrame,
} from '@bgshell/protocol'
import { AuditQueue, HostStore, KnownHosts, SessionManager } from '@bgshell/connector-core'
import { serveStatic } from './static.js'

export interface AgentServerOptions {
  port?: number
  /** 监听地址，默认 127.0.0.1（仅本机；不对局域网开放） */
  host?: string
  dataDir: string
  /** workbench-ui 静态构建目录；不存在时 / 返回构建提示 */
  uiDist: string
  /** SEA 单文件 exe 模式：内嵌工作台资源（key=URL 路径），优先于 uiDist 文件系统读取 */
  staticAssets?: Map<string, Buffer>
  /** 追加允许的页面来源（M2 引入云端签发一次性令牌后弱化为云端域名白名单） */
  allowedOrigins?: string[]
  /** Agent 自身控制（自启/退出等，桌面形态由入口注入；未注入时回复不支持） */
  agentControl?: (op: AgentOp) => Promise<AgentResult>
}

export interface AgentServerHandle {
  server: Server
  close: () => void
}

/** vite dev server（workbench-ui 开发模式代理 /ws） */
const VITE_DEV_ORIGINS = ['http://localhost:5177', 'http://127.0.0.1:5177']

/**
 * Agent 接入面①：单端口同时提供内嵌工作台（HTTP 静态）与帧通道（WS /ws）。
 * 每条 WS 连接绑定一个 SessionManager（连接断开即关闭其全部会话）。
 */
export async function startAgentServer(opts: AgentServerOptions): Promise<AgentServerHandle> {
  const port = opts.port ?? 7777
  const listenHost = opts.host ?? '127.0.0.1'
  const allowed = new Set<string>([
    `http://127.0.0.1:${port}`,
    `http://localhost:${port}`,
    ...VITE_DEV_ORIGINS,
    ...(opts.allowedOrigins ?? []),
  ])

  const hostStore = new HostStore(join(opts.dataDir, 'hosts.json'))
  const knownHosts = new KnownHosts(join(opts.dataDir, 'known-hosts.json'))
  const audit = new AuditQueue(join(opts.dataDir, 'audit-queue.jsonl'))
  await hostStore.load()
  await knownHosts.load()

  const uiRoot = resolve(opts.uiDist)
  const server = createServer((req: IncomingMessage, res: ServerResponse) =>
    serveStatic(uiRoot, req, res, opts.staticAssets),
  )
  const wss = new WebSocketServer({ noServer: true })

  server.on('upgrade', (req, socket, head) => {
    const url = new URL(req.url ?? '/', 'http://127.0.0.1')
    if (url.pathname !== '/ws') {
      socket.destroy()
      return
    }
    // Origin 白名单：仅允许本机工作台与开发代理页面驱动 Agent（设计稿 §8）
    const origin = req.headers.origin ?? 'null'
    if (!allowed.has(origin)) {
      socket.write('HTTP/1.1 403 Forbidden\r\n\r\n')
      socket.destroy()
      return
    }
    wss.handleUpgrade(req, socket, head, (ws) => wss.emit('connection', ws, req))
  })

  wss.on('connection', (ws: WebSocket) => {
    const send = (frame: ServerFrame) => ws.send(JSON.stringify(frame))
    const manager = new SessionManager(hostStore, knownHosts, audit, {
      sendControl: send,
      sendData: (sid, type, chunk) => ws.send(encodeDataFrame(type, sid, chunk) as Buffer),
      onSessionClosed: () => {},
    })

    ws.on('message', (data, isBinary) => {
      if (isBinary) {
        const df = decodeDataFrame(data as Buffer)
        if (df.type === DataFrameType.Stdin) {
          manager.input(df.sid, df.payload)
        } else if (df.type === DataFrameType.SftpUpload) {
          const { opId, chunk } = decodeOpPayload(df.payload)
          manager.uploadChunk(df.sid, opId, chunk)
        }
        return
      }
      let frame: ClientFrame
      try {
        frame = JSON.parse(data.toString('utf8')) as ClientFrame
      } catch {
        return
      }
      void handleControlFrame(frame, manager, hostStore, send, opts.agentControl)
    })

    ws.on('close', () => manager.closeAll('socket-closed'))
    ws.on('error', () => manager.closeAll('socket-error'))
  })

  return await new Promise<AgentServerHandle>((resolveReady, rejectReady) => {
    server.once('error', rejectReady) // 端口占用等启动错误上抛给入口
    server.listen(port, listenHost, () => {
      server.off('error', rejectReady)
      resolveReady({ server, close: () => { wss.close(); server.close() } })
    })
  })
}

async function handleControlFrame(
  frame: ClientFrame,
  manager: SessionManager,
  hostStore: HostStore,
  send: (frame: ServerFrame) => void,
  agentControl?: (op: AgentOp) => Promise<AgentResult>,
): Promise<void> {
  switch (frame.t) {
    case 'open':
      manager.open(frame.s, frame.m.hostId, {
        kind: frame.m.kind,
        cols: frame.m.cols,
        rows: frame.m.rows,
        term: frame.m.term,
      })
      return
    case 'resize':
      manager.resize(frame.s, frame.m.cols, frame.m.rows)
      return
    case 'close':
      manager.close(frame.s, frame.m.reason ?? 'user')
      return
    case 'confirm-hostkey':
      manager.resolveHostKey(frame.s, frame.m.accepted)
      return
    case 'sftp':
      manager.handleSftp(frame.s, frame.m)
      return
    case 'metrics':
      manager.setMetricsInterval(frame.s, frame.m.intervalSec)
      return
    case 'agent': {
      if (!agentControl) {
        send({ t: 'agent-result', m: { op: frame.m.op, ok: false, message: '当前运行形态不支持 Agent 控制' } })
        return
      }
      try {
        send({ t: 'agent-result', m: await agentControl(frame.m) })
      } catch (err) {
        send({ t: 'agent-result', m: { op: frame.m.op, ok: false, message: (err as Error).message } })
      }
      return
    }
    case 'probe': {
      const host = hostStore.get(frame.m.hostId)
      if (!host) {
        send({ t: 'probe-result', m: { hostId: frame.m.hostId, ok: false, message: '主机档案不存在' } })
        return
      }
      const r = await probeHost(host)
      send({ t: 'probe-result', m: { hostId: frame.m.hostId, ...r } })
      return
    }
    case 'hosts': {
      const { op } = frame.m
      try {
        if (op === 'list') {
          send({ t: 'hosts', m: { op, ok: true, hosts: hostStore.list() } })
        } else if (op === 'create') {
          const host = await hostStore.create(frame.m.host)
          send({ t: 'hosts', m: { op, ok: true, host } })
        } else if (op === 'update') {
          const host = await hostStore.update(frame.m.host.id, frame.m.host)
          if (!host) throw new Error('主机档案不存在')
          send({ t: 'hosts', m: { op, ok: true, host } })
        } else if (op === 'delete') {
          const ok = await hostStore.delete(frame.m.id)
          if (!ok) throw new Error('主机档案不存在')
          send({ t: 'hosts', m: { op, ok: true } })
        }
      } catch (err) {
        send({ t: 'hosts', m: { op: String(op), ok: false, message: (err as Error).message } })
      }
      return
    }
  }
}

/** TCP 连通性探测（默认 2.5s 超时；不做 SSH 握手，避免凭据参与） */
function probeHost(host: HostRecord): Promise<{ ok: boolean; ms?: number; message?: string }> {
  return new Promise((resolve) => {
    const start = Date.now()
    const socket: Socket = tcpConnect({ host: host.host, port: host.port })
    socket.setTimeout(2500)
    socket.on('connect', () => {
      const ms = Date.now() - start
      socket.destroy()
      resolve({ ok: true, ms })
    })
    socket.on('timeout', () => {
      socket.destroy()
      resolve({ ok: false, message: '连接超时' })
    })
    socket.on('error', (err: Error) => {
      socket.destroy()
      resolve({ ok: false, message: err.message })
    })
  })
}
