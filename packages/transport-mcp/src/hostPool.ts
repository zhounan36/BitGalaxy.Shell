import type { SFTPWrapper } from 'ssh2'
import {
  SftpController,
  SshSession,
  type AuditQueue,
  type HostStore,
  type KnownHosts,
} from '@bgshell/connector-core'
import { ToolError, ToolErrorCode } from './errors.js'

interface PoolEntry {
  ssh: SshSession
  /** 底层 sftp 子系统会话（控制器与 read/write 工具共享同一通道） */
  sftp: SFTPWrapper | undefined
  sftpCtrl: SftpController | undefined
  idleTimer: NodeJS.Timeout
}

/**
 * 按 hostId 复用的 SSH 连接池（docs/ai-design.md §5）：
 * 并发调用合并等待同一次建连；空闲 TTL 自动断开；出错即逐出。
 * TOFU fail-closed：MCP 通道不做首连指纹确认（DA4），未知指纹直接拒绝。
 */
export class McpHostPool {
  private readonly entries = new Map<string, PoolEntry>()
  private readonly opening = new Map<string, Promise<SshSession>>()
  private closed = false

  constructor(
    private readonly hostStore: HostStore,
    private readonly knownHosts: KnownHosts,
    private readonly audit: AuditQueue,
    private readonly idleCloseSec: number,
  ) {}

  async acquire(hostId: string): Promise<SshSession> {
    let opening = this.opening.get(hostId)
    if (!opening) {
      const existing = this.entries.get(hostId)
      if (existing) {
        this.touchIdle(hostId)
        return existing.ssh
      }
      opening = this.connect(hostId).finally(() => this.opening.delete(hostId))
      this.opening.set(hostId, opening)
    }
    const ssh = await opening
    this.touchIdle(hostId)
    return ssh
  }

  /** 底层 sftp 子系统会话（惰性建立，缓存于连接池条目） */
  async acquireSftpRaw(hostId: string): Promise<SFTPWrapper> {
    const entry = this.entryOf(hostId)
    if (!entry.sftp) {
      try {
        entry.sftp = await entry.ssh.sftp()
      } catch (err) {
        throw new ToolError(ToolErrorCode.SftpFailed, `SFTP 通道打开失败: ${(err as Error).message}`)
      }
    }
    return entry.sftp
  }

  /** SftpController（目录/删除/改名等操作，复用同一 sftp 会话） */
  async acquireSftp(hostId: string): Promise<SftpController> {
    const entry = this.entryOf(hostId)
    if (!entry.sftpCtrl) {
      entry.sftpCtrl = new SftpController(await this.acquireSftpRaw(hostId), {
        onChunk: () => {},
        onProgress: () => {},
        onDone: () => {},
      })
    }
    return entry.sftpCtrl
  }

  evict(hostId: string, reason = 'evicted'): void {
    const entry = this.entries.get(hostId)
    if (!entry) return
    this.entries.delete(hostId)
    clearTimeout(entry.idleTimer)
    try {
      entry.sftpCtrl?.close() // 同时 end 底层 sftp 会话
    } catch {
      // 已死连接上的清理失败可忽略
    }
    if (!entry.sftpCtrl && entry.sftp) {
      try {
        entry.sftp.end()
      } catch {
        // 同上
      }
    }
    entry.ssh.close(reason) // SshSession.close 幂等（closed 标志）
  }

  closeAll(): void {
    this.closed = true
    for (const hostId of [...this.entries.keys()]) this.evict(hostId, 'server-stopped')
  }

  private entryOf(hostId: string): PoolEntry {
    const entry = this.entries.get(hostId)
    if (!entry) throw new ToolError(ToolErrorCode.ConnectFailed, '连接已失效，请重试')
    this.touchIdle(hostId)
    return entry
  }

  private touchIdle(hostId: string): void {
    const entry = this.entries.get(hostId)
    if (!entry) return
    clearTimeout(entry.idleTimer)
    entry.idleTimer = setTimeout(() => this.evict(hostId, 'idle-timeout'), this.idleCloseSec * 1000)
    entry.idleTimer.unref?.()
  }

  private async connect(hostId: string): Promise<SshSession> {
    if (this.closed) throw new ToolError(ToolErrorCode.ConnectFailed, 'server 已停止')
    const host = this.hostStore.get(hostId)
    if (!host) throw new ToolError(ToolErrorCode.HostNotFound, `主机档案不存在: ${hostId}`)

    let hostKeyUnknown = false
    let hostKeyMismatch = false

    const ssh = new SshSession(this.knownHosts, {
      onData: () => {}, // exec/sftp 通道不经 PTY，无流式输出
      onClose: () => this.evict(hostId, 'connection-closed'),
      onError: (err) => {
        if (err.message.includes('指纹不匹配')) hostKeyMismatch = true
      },
      onHostKeyUnknown: async (info) => {
        hostKeyUnknown = true
        await this.audit.append('ai.hostkey-unknown', {
          hostId,
          host: info.host,
          port: info.port,
          fingerprint: info.fingerprint,
        })
        return false // fail-closed：无人值守通道不做 TOFU 确认（DA4）
      },
    })

    try {
      const ok = await ssh.open(host, { kind: 'exec' })
      if (!ok) throw new ToolError(ToolErrorCode.HostKeyMismatch, '主机指纹校验未通过')
      this.entries.set(hostId, {
        ssh,
        sftp: undefined,
        sftpCtrl: undefined,
        idleTimer: setTimeout(() => this.evict(hostId, 'idle-timeout'), this.idleCloseSec * 1000),
      })
      void this.audit.append('ai.connection-opened', {
        hostId,
        target: `${host.username}@${host.host}:${host.port}`,
      })
      return ssh
    } catch (err) {
      ssh.close('connect-failed')
      if (err instanceof ToolError) throw err
      if (hostKeyUnknown) {
        throw new ToolError(
          ToolErrorCode.HostKeyUnknown,
          `主机 ${host.name}（${host.host}:${host.port}）指纹未确认：请先在 BitGalaxy Shell 工作台人工连接一次（TOFU 确认）后再交给 AI 使用`,
        )
      }
      if (hostKeyMismatch) {
        throw new ToolError(
          ToolErrorCode.HostKeyMismatch,
          `主机 ${host.host}:${host.port} 指纹不匹配（疑似中间人攻击），已拒绝连接`,
        )
      }
      const msg = (err as Error).message
      if (/auth/i.test(msg)) {
        throw new ToolError(ToolErrorCode.AuthFailed, `认证失败（${host.username}@${host.host}）: ${msg}`)
      }
      if (/timed?\s?out|ECONNREFUSED|ENOTFOUND|EHOSTUNREACH|ETIMEDOUT|ECONNRESET/i.test(msg)) {
        throw new ToolError(ToolErrorCode.ConnectFailed, `无法连接 ${host.host}:${host.port}: ${msg}`)
      }
      throw new ToolError(ToolErrorCode.ConnectFailed, msg)
    }
  }
}
