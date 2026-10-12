import { randomUUID } from 'node:crypto'
import type { SFTPWrapper } from 'ssh2'
import type { SftpEntry } from '@bgshell/protocol'

type StatResult = Parameters<Parameters<SFTPWrapper['stat']>[1]>[1]
type FileList = Parameters<Parameters<SFTPWrapper['readdir']>[1]>[1]
type WriteStreamT = ReturnType<SFTPWrapper['createWriteStream']>
type ReadStreamT = ReturnType<SFTPWrapper['createReadStream']>

export interface SftpHooks {
  /** 下载流：文件块回送（二进制 0x11，按 opId 组装） */
  onChunk(opId: string, chunk: Buffer): void
  /** 下载进度（transferred === total 即最后一个块） */
  onProgress(opId: string, transferred: number, total: number): void
  /** 下载流结束 */
  onDone(opId: string): void
}

interface UploadTransfer {
  stream: WriteStreamT
  error: Error | undefined
  closed: Promise<void>
}

/** SFTP 文件管理（kind=sftp 会话）：目录操作 + 流式上传/下载（64KB 块） */
export class SftpController {
  private readonly uploads = new Map<string, UploadTransfer>()
  private readonly downloads = new Map<string, ReadStreamT>()

  constructor(
    private readonly sftp: SFTPWrapper,
    private readonly hooks: SftpHooks,
  ) {}

  async realpath(path: string): Promise<string> {
    return await new Promise((resolve, reject) =>
      this.sftp.realpath(path, (err, abs) => (err ? reject(err) : resolve(abs))),
    )
  }

  async ls(path: string): Promise<{ resolvedPath: string; entries: SftpEntry[] }> {
    const resolvedPath = await this.realpath(path)
    const list: FileList = await new Promise((resolve, reject) =>
      this.sftp.readdir(resolvedPath, (err, l) => (err ? reject(err) : resolve(l))),
    )
    const entries: SftpEntry[] = list
      .map((e) => ({
        name: e.filename,
        type: longnameToType(e.longname?.[0] ?? ''),
        sizeB: e.attrs?.size ?? 0,
        mtimeMs: (e.attrs?.mtime ?? 0) * 1000,
      }))
      .sort((a, b) => (a.type === b.type ? a.name.localeCompare(b.name) : a.type === 'dir' ? -1 : 1))
    return { resolvedPath, entries }
  }

  async mkdir(path: string): Promise<void> {
    await new Promise<void>((resolve, reject) =>
      this.sftp.mkdir(path, (err) => (err ? reject(err) : resolve())),
    )
  }

  async rename(from: string, to: string): Promise<void> {
    await new Promise<void>((resolve, reject) =>
      this.sftp.rename(from, to, (err) => (err ? reject(err) : resolve())),
    )
  }

  async rm(path: string, recursive: boolean): Promise<void> {
    if (!recursive) {
      await new Promise<void>((resolve, reject) =>
        this.sftp.unlink(path, (err) => (err ? reject(err) : resolve())),
      )
      return
    }
    const stat: StatResult = await new Promise((resolve, reject) =>
      this.sftp.stat(path, (err, s) => (err ? reject(err) : resolve(s))),
    )
    if (!stat.isDirectory()) {
      await this.rm(path, false)
      return
    }
    const list: FileList = await new Promise((resolve, reject) =>
      this.sftp.readdir(path, (err, l) => (err ? reject(err) : resolve(l))),
    )
    for (const e of list) {
      if (e.filename === '.' || e.filename === '..') continue
      await this.rm(joinRemote(path, e.filename), true)
    }
    await new Promise<void>((resolve, reject) =>
      this.sftp.rmdir(path, (err) => (err ? reject(err) : resolve())),
    )
  }

  /** 打开上传流，返回 opId；块经 uploadChunk 写入，uploadClose 收尾 */
  uploadOpen(path: string): string {
    const opId = randomUUID()
    const stream = this.sftp.createWriteStream(path)
    const transfer: UploadTransfer = {
      stream,
      error: undefined,
      closed: new Promise<void>((resolve) => stream.on('close', () => resolve())),
    }
    stream.on('error', (err: Error) => {
      transfer.error = err
    })
    this.uploads.set(opId, transfer)
    return opId
  }

  uploadChunk(opId: string, chunk: Buffer): boolean {
    const t = this.uploads.get(opId)
    if (!t || t.error) return false
    return t.stream.write(chunk)
  }

  async uploadClose(opId: string): Promise<void> {
    const t = this.uploads.get(opId)
    if (!t) return
    t.stream.end()
    await t.closed
    this.uploads.delete(opId)
    if (t.error) throw t.error
  }

  async downloadOpen(path: string): Promise<{ opId: string; sizeB: number }> {
    const stat: StatResult = await new Promise((resolve, reject) =>
      this.sftp.stat(path, (err, s) => (err ? reject(err) : resolve(s))),
    )
    const sizeB = stat.size
    const opId = randomUUID()
    const stream = this.sftp.createReadStream(path, { highWaterMark: 64 * 1024 })
    let transferred = 0
    stream.on('data', (chunk: Buffer) => {
      transferred += chunk.length
      this.hooks.onChunk(opId, chunk)
      this.hooks.onProgress(opId, transferred, sizeB)
    })
    stream.on('close', () => this.hooks.onDone(opId))
    this.downloads.set(opId, stream)
    return { opId, sizeB }
  }

  downloadClose(opId: string): void {
    this.downloads.get(opId)?.destroy()
    this.downloads.delete(opId)
  }

  close(): void {
    for (const t of this.uploads.values()) t.stream.destroy()
    for (const s of this.downloads.values()) s.destroy()
    this.uploads.clear()
    this.downloads.clear()
    this.sftp.end()
  }
}

function longnameToType(ch: string): SftpEntry['type'] {
  if (ch === 'd') return 'dir'
  if (ch === 'l') return 'symlink'
  if (ch === '-') return 'file'
  return 'other'
}

/** POSIX 远程路径拼接（SFTP 目标多为 Linux） */
export function joinRemote(dir: string, name: string): string {
  if (dir.endsWith('/')) return `${dir}${name}`
  return `${dir}/${name}`
}

/** 远程父目录（根目录返回 '/'） */
export function parentRemote(path: string): string {
  const parts = path.split('/').filter(Boolean)
  if (parts.length <= 1) return '/'
  return `/${parts.slice(0, -1).join('/')}`
}
