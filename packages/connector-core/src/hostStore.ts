import { randomUUID } from 'node:crypto'
import { mkdir, readFile, rename, writeFile } from 'node:fs/promises'
import { dirname } from 'node:path'
import type { HostInput, HostRecord } from '@bgshell/protocol'

/**
 * 本地主机档案（hosts.json，设计稿 §5.4 离线模式的档案来源）。
 * M1：Agent 唯一档案源；M2 起与云端档案按 updatedAt 合并同步。
 */
export class HostStore {
  private records: HostRecord[] = []

  constructor(private readonly file: string) {}

  async load(): Promise<void> {
    try {
      this.records = JSON.parse(await readFile(this.file, 'utf8')) as HostRecord[]
    } catch {
      this.records = []
    }
  }

  list(): HostRecord[] {
    return this.records
  }

  get(id: string): HostRecord | undefined {
    return this.records.find((h) => h.id === id)
  }

  async create(input: HostInput): Promise<HostRecord> {
    const now = new Date().toISOString()
    const record: HostRecord = { ...input, id: randomUUID(), createdAt: now, updatedAt: now }
    this.records.push(record)
    await this.persist()
    return record
  }

  async update(id: string, input: HostInput): Promise<HostRecord | undefined> {
    const record = this.get(id)
    if (!record) return undefined
    Object.assign(record, input, { updatedAt: new Date().toISOString() })
    await this.persist()
    return record
  }

  async delete(id: string): Promise<boolean> {
    const before = this.records.length
    this.records = this.records.filter((h) => h.id !== id)
    if (this.records.length === before) return false
    await this.persist()
    return true
  }

  /** 原子写：临时文件 + rename，避免半写损坏档案 */
  private async persist(): Promise<void> {
    await mkdir(dirname(this.file), { recursive: true })
    const tmp = `${this.file}.tmp`
    await writeFile(tmp, JSON.stringify(this.records, null, 2), 'utf8')
    await rename(tmp, this.file)
  }
}
