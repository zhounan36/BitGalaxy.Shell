import {
  DataFrameType,
  encodeOpPayload,
  type ServerFrame,
  type SessionKind,
  type SftpOp,
  type SftpResult,
} from '@bgshell/protocol'
import { AuditQueue } from './auditQueue.js'
import { HostStore } from './hostStore.js'
import { KnownHosts } from './knownHosts.js'
import { buildMetricsCommand, parseMetrics } from './metrics.js'
import { SftpController } from './sftp.js'
import { SshSession } from './sshSession.js'

export interface ManagerHooks {
  sendControl(frame: ServerFrame): void
  sendData(sessionId: string, type: number, chunk: Uint8Array): void
  onSessionClosed(sessionId: string): void
}

interface SessionEntry {
  hostId: string
  kind: SessionKind
  ssh: SshSession | undefined
  sftp: SftpController | undefined
  metricsTimer: NodeJS.Timeout | undefined
  /** 建连期间到达的 metrics 订阅，就绪后自动启动 */
  metricsPendingSec: number | undefined
  resolveHostKey: ((accepted: boolean) => void) | undefined
}

/**
 * 会话管理器：一个传输连接（WS/NM/中继）对应一个实例。
 * 三种会话 kind 共享 SSH 建连/TOFU/审计路径；sftp/metrics 通道按 kind 挂载。
 */
export class SessionManager {
  private readonly sessions = new Map<string, SessionEntry>()

  constructor(
    private readonly hostStore: HostStore,
    private readonly knownHosts: KnownHosts,
    private readonly audit: AuditQueue,
    private readonly hooks: ManagerHooks,
  ) {}

  open(
    sessionId: string,
    hostId: string,
    opts: { kind?: SessionKind; cols?: number; rows?: number; term?: string },
  ): void {
    if (this.sessions.has(sessionId)) return
    const kind = opts.kind ?? 'shell'
    const entry: SessionEntry = {
      hostId,
      kind,
      ssh: undefined,
      sftp: undefined,
      metricsTimer: undefined,
      metricsPendingSec: undefined,
      resolveHostKey: undefined,
    }
    this.sessions.set(sessionId, entry)

    // 异步建连：结果经控制帧回送，不阻塞传输层消息循环
    void (async () => {
      try {
        const host = this.hostStore.get(hostId)
        if (!host) throw new Error('主机档案不存在')

        const ssh = new SshSession(this.knownHosts, {
          onData: (chunk) => this.hooks.sendData(sessionId, DataFrameType.Stdout, chunk),
          onClose: (reason) => this.handleClosed(sessionId, reason),
          onError: (err) =>
            this.hooks.sendControl({ t: 'error', s: sessionId, m: { code: 'ssh-error', message: err.message } }),
          onHostKeyUnknown: async (info) => {
            this.hooks.sendControl({ t: 'hostkey-unknown', s: sessionId, m: info })
            return await new Promise<boolean>((resolve) => {
              entry.resolveHostKey = resolve
            })
          },
        })
        entry.ssh = ssh

        const ok = await ssh.open(host, { kind, cols: opts.cols, rows: opts.rows, term: opts.term })
        if (!ok) throw new Error('主机指纹未通过校验')

        if (kind === 'sftp') {
          entry.sftp = new SftpController(await ssh.sftp(), {
            onChunk: (opId, chunk) =>
              this.hooks.sendData(sessionId, DataFrameType.SftpDownload, encodeOpPayload(opId, chunk)),
            onProgress: (opId, transferred, total) =>
              this.hooks.sendControl({ t: 'sftp-progress', s: sessionId, m: { opId, transferred, total } }),
            onDone: (opId) =>
              this.hooks.sendControl({
                t: 'sftp-result',
                s: sessionId,
                m: { op: 'download-done', ok: true, opId },
              }),
          })
        }

        this.hooks.sendControl({ t: 'session-opened', s: sessionId, m: { hostId, kind } })
        void this.audit.append('session.opened', {
          sessionId,
          hostId,
          kind,
          target: `${host.username}@${host.host}:${host.port}`,
        })

        if (kind === 'metrics' && entry.metricsPendingSec) {
          const sec = entry.metricsPendingSec
          entry.metricsPendingSec = undefined
          this.startMetrics(entry, sessionId, sec)
        }
      } catch (err) {
        this.hooks.sendControl({
          t: 'error',
          s: sessionId,
          m: { code: 'open-failed', message: (err as Error).message },
        })
        this.handleClosed(sessionId, `open-failed: ${(err as Error).message}`)
      }
    })()
  }

  // ── terminal ───────────────────────────────────────────────────────────

  input(sessionId: string, data: Uint8Array): void {
    this.sessions.get(sessionId)?.ssh?.write(data)
  }

  resize(sessionId: string, cols: number, rows: number): void {
    this.sessions.get(sessionId)?.ssh?.resize(cols, rows)
  }

  // ── sftp ───────────────────────────────────────────────────────────────

  handleSftp(sessionId: string, op: SftpOp): void {
    const reply = (m: SftpResult) => this.hooks.sendControl({ t: 'sftp-result', s: sessionId, m })
    const entry = this.sessions.get(sessionId)
    if (!entry?.sftp) {
      reply({ op: op.op, rid: op.rid, ok: false, message: 'SFTP 会话未就绪' })
      return
    }
    const sftp = entry.sftp
    void (async () => {
      try {
        switch (op.op) {
          case 'ls': {
            const r = await sftp.ls(op.path)
            reply({ op: 'ls', rid: op.rid, ok: true, resolvedPath: r.resolvedPath, entries: r.entries })
            return
          }
          case 'mkdir':
            await sftp.mkdir(op.path)
            reply({ op: 'mkdir', rid: op.rid, ok: true })
            return
          case 'rm':
            await sftp.rm(op.path, op.recursive === true)
            reply({ op: 'rm', rid: op.rid, ok: true })
            return
          case 'rename':
            await sftp.rename(op.from, op.to)
            reply({ op: 'rename', rid: op.rid, ok: true })
            return
          case 'upload-open': {
            const opId = sftp.uploadOpen(op.path)
            reply({ op: 'upload-open', rid: op.rid, ok: true, opId })
            return
          }
          case 'upload-close':
            await sftp.uploadClose(op.opId)
            reply({ op: 'upload-close', rid: op.rid, ok: true })
            return
          case 'download-open': {
            const r = await sftp.downloadOpen(op.path)
            reply({ op: 'download-open', rid: op.rid, ok: true, opId: r.opId, sizeB: r.sizeB })
            return
          }
          case 'download-close':
            sftp.downloadClose(op.opId)
            reply({ op: 'download-close', rid: op.rid, ok: true })
            return
        }
      } catch (err) {
        reply({ op: op.op, rid: op.rid, ok: false, message: (err as Error).message })
      }
    })()
  }

  /** 二进制 0x10 上传块（payload 已解出 opId/chunk） */
  uploadChunk(sessionId: string, opId: string, chunk: Uint8Array): void {
    this.sessions.get(sessionId)?.sftp?.uploadChunk(opId, chunk as Buffer)
  }

  // ── metrics ────────────────────────────────────────────────────────────

  setMetricsInterval(sessionId: string, intervalSec: number): void {
    const entry = this.sessions.get(sessionId)
    if (!entry || entry.kind !== 'metrics') return
    if (entry.metricsTimer) {
      clearInterval(entry.metricsTimer)
      entry.metricsTimer = undefined
    }
    if (intervalSec <= 0) return
    if (!entry.ssh) {
      entry.metricsPendingSec = intervalSec // 建连中，就绪后自动启动
      return
    }
    this.startMetrics(entry, sessionId, intervalSec)
  }

  private startMetrics(entry: SessionEntry, sessionId: string, intervalSec: number): void {
    const tick = async () => {
      try {
        const r = await entry.ssh!.exec(buildMetricsCommand(), 10_000)
        if (r.timedOut) throw new Error('远程命令执行超时')
        this.hooks.sendControl({ t: 'metrics-data', s: sessionId, m: parseMetrics(r.stdout) })
      } catch (err) {
        // 采集失败不终止会话（可能为目标临时不可用），下发错误帧由 UI 呈现
        this.hooks.sendControl({
          t: 'error',
          s: sessionId,
          m: { code: 'metrics-failed', message: (err as Error).message },
        })
      }
    }
    void tick()
    entry.metricsTimer = setInterval(() => void tick(), intervalSec * 1000)
  }

  // ── 生命周期 ───────────────────────────────────────────────────────────

  resolveHostKey(sessionId: string, accepted: boolean): void {
    this.sessions.get(sessionId)?.resolveHostKey?.(accepted)
  }

  close(sessionId: string, reason = 'user'): void {
    const entry = this.sessions.get(sessionId)
    if (!entry) return
    entry.resolveHostKey?.(false)
    if (entry.metricsTimer) clearInterval(entry.metricsTimer)
    entry.sftp?.close()
    entry.ssh?.close(reason)
    this.handleClosed(sessionId, reason)
  }

  closeAll(reason = 'socket-closed'): void {
    for (const sid of [...this.sessions.keys()]) this.close(sid, reason)
  }

  private handleClosed(sessionId: string, reason: string): void {
    if (!this.sessions.delete(sessionId)) return
    this.hooks.sendControl({ t: 'session-closed', s: sessionId, m: { reason } })
    this.hooks.onSessionClosed(sessionId)
    void this.audit.append('session.closed', { sessionId, reason })
  }
}
