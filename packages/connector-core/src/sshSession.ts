import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { Client, type ClientChannel, type ConnectConfig, type SFTPWrapper } from 'ssh2'
import type { HostRecord } from '@bgshell/protocol'
import type { KnownHosts } from './knownHosts.js'

export interface HostKeyQuestion {
  host: string
  port: number
  keyType: string
  fingerprint: string
}

export interface SshCallbacks {
  onData(chunk: Buffer): void
  onClose(reason: string): void
  onError(err: Error): void
  /** TOFU 首连确认：返回 true = 用户接受该指纹 */
  onHostKeyUnknown(info: HostKeyQuestion): Promise<boolean>
}

/** OpenSSH 风格指纹（SHA256 base64） */
export function hostKeyFingerprint(key: Buffer): string {
  return `SHA256:${createHash('sha256').update(key).digest('base64')}`
}

export type SshOpenKind = 'shell' | 'sftp' | 'metrics' | 'exec'

/**
 * 单条 SSH 会话：建连（含 hostVerifier TOFU/防中间人）。
 * kind=shell → 打开 PTY shell（onData 双向流）；
 * kind=sftp/metrics → ready 即返回，后续经 sftp()/exec() 使用同一连接。
 * 私钥/密码仅在建连握手期间使用（M1 从 hosts.json 内联读取；M2 起改由密库解密供给）。
 */
export class SshSession {
  private client: Client | null = null
  private stream: ClientChannel | null = null
  private closed = false

  constructor(
    private readonly knownHosts: KnownHosts,
    private readonly cb: SshCallbacks,
  ) {}

  /** 建连；返回 false = 指纹未通过（拒绝连接） */
  async open(
    host: HostRecord,
    opts: { kind: SshOpenKind; cols?: number; rows?: number; term?: string },
  ): Promise<boolean> {
    const client = new Client()
    this.client = client

    const auth: ConnectConfig = {}
    if (host.auth.kind === 'password') {
      auth.password = host.auth.password
    } else {
      if (!host.auth.privateKeyPath) throw new Error('私钥认证缺少 privateKeyPath')
      auth.privateKey = readFileSync(host.auth.privateKeyPath)
      if (host.auth.passphrase) auth.passphrase = host.auth.passphrase
    }

    return await new Promise<boolean>((resolve, reject) => {
      let settled = false
      client
        .on('ready', () => {
          if (opts.kind !== 'shell') {
            // sftp/metrics：连接就绪即完成，通道按需开启
            settled = true
            resolve(true)
            return
          }
          client.shell(
            { term: opts.term ?? 'xterm-256color', cols: opts.cols ?? 80, rows: opts.rows ?? 24 },
            (err, stream) => {
              if (err) {
                settled = true
                reject(err)
                return
              }
              this.stream = stream
              stream.on('data', (d: Buffer) => this.cb.onData(d))
              stream.on('close', () => this.close('shell-closed'))
              stream.stderr?.on('data', (d: Buffer) => this.cb.onData(d))
              settled = true
              resolve(true)
            },
          )
        })
        .on('error', (err: Error) => {
          if (!settled) {
            settled = true
            reject(err)
          }
          this.close(`error: ${err.message}`)
        })
        .on('close', () => this.close('client-closed'))
        .connect({
          host: host.host,
          port: host.port,
          username: host.username,
          readyTimeout: 15_000,
          keepaliveInterval: 15_000,
          hostVerifier: (key: Buffer) => this.verifyHostKey(host, key),
          ...auth,
        })
    })
  }

  /** 打开 SFTP 子系统（kind=sftp 会话用） */
  sftp(): Promise<SFTPWrapper> {
    if (!this.client) throw new Error('SSH 连接未建立')
    return new Promise((resolve, reject) => {
      this.client!.sftp((err, sftp) => (err ? reject(err) : resolve(sftp)))
    })
  }

  /**
   * 单次远程命令（metrics 采集 / MCP exec 工具共用）。
   * 超时不抛错：携带已收输出与 timedOut 标志返回（调用方自行判定失败语义）。
   * maxBytes：stdout/stderr 各自的采集硬顶，超限丢弃后续内容并置 truncated。
   */
  exec(
    cmd: string,
    timeoutMs = 10_000,
    maxBytes = 2 * 1024 * 1024,
  ): Promise<{ stdout: string; stderr: string; code: number | null; truncated: boolean; timedOut: boolean }> {
    if (!this.client) return Promise.reject(new Error('SSH 连接未建立'))
    return new Promise((resolve, reject) => {
      this.client!.exec(cmd, (err, stream) => {
        if (err) return reject(err) // 通道打开失败（非超时语义）
        let truncated = false
        const collect = (cur: { v: string }, d: Buffer): void => {
          if (cur.v.length >= maxBytes) {
            truncated = true
            return // 丢弃超出部分（继续消费防背压，但不累积）
          }
          cur.v += d.toString('utf8')
          if (cur.v.length > maxBytes) {
            cur.v = cur.v.slice(0, maxBytes)
            truncated = true
          }
        }
        const out = { v: '' }
        const errOut = { v: '' }
        const timer = setTimeout(() => {
          stream.close()
          resolve({ stdout: out.v, stderr: errOut.v, code: null, truncated, timedOut: true })
        }, timeoutMs)
        stream.on('data', (d: Buffer) => collect(out, d))
        stream.stderr?.on('data', (d: Buffer) => collect(errOut, d))
        stream.on('close', (code: number | null) => {
          clearTimeout(timer)
          resolve({ stdout: out.v, stderr: errOut.v, code, truncated, timedOut: false })
        })
      })
    })
  }

  write(data: Uint8Array): void {
    this.stream?.write(data)
  }

  resize(cols: number, rows: number): void {
    this.stream?.setWindow(rows, cols, 480, 240)
  }

  close(reason: string): void {
    if (this.closed) return
    this.closed = true
    this.stream?.close()
    this.client?.end()
    this.cb.onClose(reason)
  }

  /** 已知指纹 → 比对；未知 → TOFU 询问；不匹配 → 拒绝（防中间人） */
  private async verifyHostKey(host: HostRecord, key: Buffer): Promise<boolean> {
    const fingerprint = hostKeyFingerprint(key)
    const keyType = keyAlgorithm(key)
    const known = this.knownHosts.find(host.host, host.port)
    if (known) {
      if (known.fingerprint === fingerprint) return true
      this.cb.onError(new Error(`主机指纹不匹配（疑似中间人攻击）：${host.host}:${host.port}`))
      return false
    }
    const accepted = await this.cb.onHostKeyUnknown({ host: host.host, port: host.port, keyType, fingerprint })
    if (accepted) {
      await this.knownHosts.save({ host: host.host, port: host.port, keyType, fingerprint })
    }
    return accepted
  }
}

/** SSH wire 格式主机公钥 blob：[len:4B][algo][...]，取首段算法名 */
function keyAlgorithm(key: Buffer): string {
  if (key.length < 4) return 'unknown'
  const len = key.readUInt32BE(0)
  if (len === 0 || 4 + len > key.length) return 'unknown'
  return key.subarray(4, 4 + len).toString('utf8')
}
