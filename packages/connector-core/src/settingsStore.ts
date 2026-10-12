import type { Db } from './db.js'

/** settings 表行结构 */
interface SettingRow {
  key: string
  value: string
}

/**
 * 键值配置存储（SQLite 持久化）。
 * 替代 settings.json + ai.json；value 以 JSON 编码存储。
 */
export class SettingsStore {
  private readonly stGet: ReturnType<Db['prepare']>
  private readonly stAll: ReturnType<Db['prepare']>
  private readonly stUpsert: ReturnType<Db['prepare']>
  private readonly stDelete: ReturnType<Db['prepare']>

  constructor(private readonly db: Db) {
    this.stGet = db.prepare('SELECT value FROM settings WHERE key = ?')
    this.stAll = db.prepare('SELECT key, value FROM settings')
    this.stUpsert = db.prepare('INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)')
    this.stDelete = db.prepare('DELETE FROM settings WHERE key = ?')
  }

  get<T = unknown>(key: string): T | undefined {
    const row = this.stGet.get(key) as SettingRow | undefined
    if (!row) return undefined
    try {
      return JSON.parse(row.value) as T
    } catch {
      return undefined
    }
  }

  getAll(): Record<string, unknown> {
    const rows = this.stAll.all() as unknown as SettingRow[]
    const out: Record<string, unknown> = {}
    for (const r of rows) {
      try {
        out[r.key] = JSON.parse(r.value)
      } catch {
        // 跳过损坏值
      }
    }
    return out
  }

  set(key: string, value: unknown): void {
    this.stUpsert.run(key, JSON.stringify(value))
  }

  delete(key: string): boolean {
    const r = this.stDelete.run(key)
    return r.changes > 0
  }
}
