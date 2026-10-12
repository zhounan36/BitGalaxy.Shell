import { randomUUID } from 'node:crypto'
import type { HostInput, HostRecord } from '@bgshell/protocol'
import type { Db } from './db.js'

/** hosts 表行结构 */
interface HostRow {
  id: string
  name: string
  host: string
  port: number
  username: string
  auth_kind: string
  password: string | null
  private_key_path: string | null
  passphrase: string | null
  tags: string | null
  created_at: string
  updated_at: string
}

function rowToRecord(row: HostRow): HostRecord {
  return {
    id: row.id,
    name: row.name,
    host: row.host,
    port: row.port,
    username: row.username,
    auth: {
      kind: row.auth_kind as 'password' | 'privateKey',
      ...(row.password != null ? { password: row.password } : {}),
      ...(row.private_key_path != null ? { privateKeyPath: row.private_key_path } : {}),
      ...(row.passphrase != null ? { passphrase: row.passphrase } : {}),
    },
    ...(row.tags ? { tags: JSON.parse(row.tags) as string[] } : {}),
    createdAt: row.created_at,
    updatedAt: row.updated_at,
  }
}

/**
 * 本地主机档案（SQLite 持久化，设计稿 §5.4）。
 * 公开 API 保持 async 签名以兼容调用方 `await`。
 */
export class HostStore {
  private readonly stList: ReturnType<Db['prepare']>
  private readonly stGet: ReturnType<Db['prepare']>
  private readonly stInsert: ReturnType<Db['prepare']>
  private readonly stUpdate: ReturnType<Db['prepare']>
  private readonly stDelete: ReturnType<Db['prepare']>

  constructor(private readonly db: Db) {
    this.stList = db.prepare('SELECT * FROM hosts ORDER BY created_at')
    this.stGet = db.prepare('SELECT * FROM hosts WHERE id = ?')
    this.stInsert = db.prepare(
      'INSERT INTO hosts (id, name, host, port, username, auth_kind, password, private_key_path, passphrase, tags, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)',
    )
    this.stUpdate = db.prepare(
      'UPDATE hosts SET name = ?, host = ?, port = ?, username = ?, auth_kind = ?, password = ?, private_key_path = ?, passphrase = ?, tags = ?, updated_at = ? WHERE id = ?',
    )
    this.stDelete = db.prepare('DELETE FROM hosts WHERE id = ?')
  }

  /** 兼容旧 API：SQLite 同步无需加载 */
  async load(): Promise<void> {}

  list(): HostRecord[] {
    return (this.stList.all() as unknown as HostRow[]).map(rowToRecord)
  }

  get(id: string): HostRecord | undefined {
    const row = this.stGet.get(id) as HostRow | undefined
    return row ? rowToRecord(row) : undefined
  }

  async create(input: HostInput): Promise<HostRecord> {
    const now = new Date().toISOString()
    const record: HostRecord = { ...input, id: randomUUID(), createdAt: now, updatedAt: now }
    this.stInsert.run(
      record.id,
      record.name,
      record.host,
      record.port,
      record.username,
      record.auth.kind,
      record.auth.password ?? null,
      record.auth.privateKeyPath ?? null,
      record.auth.passphrase ?? null,
      record.tags ? JSON.stringify(record.tags) : null,
      record.createdAt,
      record.updatedAt,
    )
    return record
  }

  async update(id: string, input: HostInput): Promise<HostRecord | undefined> {
    const now = new Date().toISOString()
    this.stUpdate.run(
      input.name,
      input.host,
      input.port,
      input.username,
      input.auth.kind,
      input.auth.password ?? null,
      input.auth.privateKeyPath ?? null,
      input.auth.passphrase ?? null,
      input.tags ? JSON.stringify(input.tags) : null,
      now,
      id,
    )
    return this.get(id)
  }

  async delete(id: string): Promise<boolean> {
    const r = this.stDelete.run(id)
    return r.changes > 0
  }
}
