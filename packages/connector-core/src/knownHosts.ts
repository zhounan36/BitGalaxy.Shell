import { mkdir, readFile, rename, writeFile } from 'node:fs/promises'
import { dirname } from 'node:path'

export interface KnownHostEntry {
  host: string
  port: number
  keyType: string
  fingerprint: string
}

/**
 * known_hosts（TOFU，设计稿 §5.3）：首连确认指纹后落盘，后续连接比对；
 * 指纹不匹配（疑似中间人）直接拒绝。
 */
export class KnownHosts {
  private entries: KnownHostEntry[] = []

  constructor(private readonly file: string) {}

  async load(): Promise<void> {
    try {
      this.entries = JSON.parse(await readFile(this.file, 'utf8')) as KnownHostEntry[]
    } catch {
      this.entries = []
    }
  }

  find(host: string, port: number): KnownHostEntry | undefined {
    return this.entries.find((e) => e.host === host && e.port === port)
  }

  async save(entry: KnownHostEntry): Promise<void> {
    const idx = this.entries.findIndex((e) => e.host === entry.host && e.port === entry.port)
    if (idx >= 0) this.entries[idx] = entry
    else this.entries.push(entry)
    await mkdir(dirname(this.file), { recursive: true })
    const tmp = `${this.file}.tmp`
    await writeFile(tmp, JSON.stringify(this.entries, null, 2), 'utf8')
    await rename(tmp, this.file)
  }
}
