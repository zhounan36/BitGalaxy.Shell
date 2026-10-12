import type { Db } from './db.js'

export interface KnownHostEntry {
  host: string
  port: number
  keyType: string
  fingerprint: string
}

/** known_hosts（TOFU，SQLite 持久化） */
export class KnownHosts {
  private readonly stFind: ReturnType<Db['prepare']>
  private readonly stUpsert: ReturnType<Db['prepare']>

  constructor(private readonly db: Db) {
    this.stFind = db.prepare('SELECT * FROM known_hosts WHERE host = ? AND port = ?')
    this.stUpsert = db.prepare(
      'INSERT OR REPLACE INTO known_hosts (host, port, key_type, fingerprint) VALUES (?, ?, ?, ?)',
    )
  }

  /** 兼容旧 API：SQLite 同步无需加载 */
  async load(): Promise<void> {}

  find(host: string, port: number): KnownHostEntry | undefined {
    const row = this.stFind.get(host, port) as
      | { host: string; port: number; key_type: string; fingerprint: string }
      | undefined
    return row
      ? { host: row.host, port: row.port, keyType: row.key_type, fingerprint: row.fingerprint }
      : undefined
  }

  async save(entry: KnownHostEntry): Promise<void> {
    this.stUpsert.run(entry.host, entry.port, entry.keyType, entry.fingerprint)
  }
}
