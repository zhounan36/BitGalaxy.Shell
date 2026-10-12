import { DatabaseSync } from 'node:sqlite'
import { existsSync, renameSync, readFileSync } from 'node:fs'
import { join } from 'node:path'

/** 迁移用的主机记录结构（与 protocol HostRecord 一致） */
interface HostRow {
  id: string
  name: string
  host: string
  port: number
  username: string
  auth: { kind: string; password?: string; privateKeyPath?: string; passphrase?: string }
  tags?: string[]
  createdAt: string
  updatedAt: string
}

/** 迁移用的已知主机结构（与 knownHosts.ts KnownHostEntry 一致） */
interface KnownHostRow {
  host: string
  port: number
  keyType: string
  fingerprint: string
}

/**
 * SQLite 数据库（node:sqlite DatabaseSync 封装）。
 * 统一存储 hosts / known_hosts / settings / audit_log，替代散落的 JSON 文件。
 * 首次打开时自动从 JSON 迁移历史数据。
 */
export class Db {
  private readonly db: DatabaseSync

  constructor(file: string) {
    this.db = new DatabaseSync(file)
    this.db.exec('PRAGMA journal_mode = WAL')
    this.db.exec('PRAGMA journal_size_limit = 67108864')
    this.initSchema()
  }

  private initSchema(): void {
    this.db.exec(`
      CREATE TABLE IF NOT EXISTS hosts (
        id               TEXT PRIMARY KEY,
        name             TEXT NOT NULL,
        host             TEXT NOT NULL,
        port             INTEGER NOT NULL DEFAULT 22,
        username         TEXT NOT NULL,
        auth_kind        TEXT NOT NULL,
        password         TEXT,
        private_key_path TEXT,
        passphrase       TEXT,
        tags             TEXT,
        created_at       TEXT NOT NULL,
        updated_at       TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS known_hosts (
        host        TEXT NOT NULL,
        port        INTEGER NOT NULL,
        key_type    TEXT NOT NULL,
        fingerprint TEXT NOT NULL,
        PRIMARY KEY (host, port)
      );
      CREATE TABLE IF NOT EXISTS settings (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS audit_log (
        id   INTEGER PRIMARY KEY AUTOINCREMENT,
        ts   TEXT NOT NULL,
        type TEXT NOT NULL,
        meta TEXT NOT NULL
      );
    `)
  }

  prepare(sql: string) {
    return this.db.prepare(sql)
  }

  exec(sql: string) {
    this.db.exec(sql)
  }

  close(): void {
    this.db.close()
  }

  /**
   * 首次打开时从旧 JSON 文件迁移数据。
   * 仅当目标表为空且对应 JSON 文件存在时执行；迁移后 JSON 重命名为 .bak。
   */
  migrateFromJson(dataDir: string): void {
    this.migrateHosts(dataDir)
    this.migrateKnownHosts(dataDir)
    this.migrateSettings(dataDir)
    this.migrateAudit(dataDir)
    this.migrateAi(dataDir)
  }

  private migrateHosts(dataDir: string): void {
    const file = join(dataDir, 'hosts.json')
    if (!existsSync(file)) return
    const count = this.prepare('SELECT COUNT(*) AS c FROM hosts').get() as { c: number }
    if (count.c > 0) return
    try {
      const records = JSON.parse(readFileSync(file, 'utf8')) as HostRow[]
      const stmt = this.prepare(
        'INSERT INTO hosts (id, name, host, port, username, auth_kind, password, private_key_path, passphrase, tags, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)',
      )
      for (const h of records) {
        stmt.run(
          h.id,
          h.name,
          h.host,
          h.port,
          h.username,
          h.auth.kind,
          h.auth.password ?? null,
          h.auth.privateKeyPath ?? null,
          h.auth.passphrase ?? null,
          h.tags ? JSON.stringify(h.tags) : null,
          h.createdAt,
          h.updatedAt,
        )
      }
    } catch {
      // JSON 损坏 → 跳过迁移
    }
    renameSync(file, `${file}.bak`)
  }

  private migrateKnownHosts(dataDir: string): void {
    const file = join(dataDir, 'known-hosts.json')
    if (!existsSync(file)) return
    const count = this.prepare('SELECT COUNT(*) AS c FROM known_hosts').get() as { c: number }
    if (count.c > 0) return
    try {
      const entries = JSON.parse(readFileSync(file, 'utf8')) as KnownHostRow[]
      const stmt = this.prepare(
        'INSERT OR REPLACE INTO known_hosts (host, port, key_type, fingerprint) VALUES (?, ?, ?, ?)',
      )
      for (const e of entries) {
        stmt.run(e.host, e.port, e.keyType, e.fingerprint)
      }
    } catch {
      // pass
    }
    renameSync(file, `${file}.bak`)
  }

  private migrateSettings(dataDir: string): void {
    const file = join(dataDir, 'settings.json')
    if (!existsSync(file)) return
    const count = this.prepare("SELECT COUNT(*) AS c FROM settings WHERE key LIKE 'app.%'").get() as { c: number }
    if (count.c > 0) return
    try {
      const settings = JSON.parse(readFileSync(file, 'utf8')) as Record<string, unknown>
      const stmt = this.prepare('INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)')
      for (const [k, v] of Object.entries(settings)) {
        stmt.run(`app.${k}`, JSON.stringify(v))
      }
    } catch {
      // pass
    }
    renameSync(file, `${file}.bak`)
  }

  private migrateAudit(dataDir: string): void {
    const file = join(dataDir, 'audit-queue.jsonl')
    if (!existsSync(file)) return
    const count = this.prepare('SELECT COUNT(*) AS c FROM audit_log').get() as { c: number }
    if (count.c > 0) return
    try {
      const lines = readFileSync(file, 'utf8').split('\n').filter(Boolean)
      const stmt = this.prepare('INSERT INTO audit_log (ts, type, meta) VALUES (?, ?, ?)')
      for (const line of lines) {
        const ev = JSON.parse(line) as { type: string; ts: string }
        const { type, ts, ...meta } = ev
        stmt.run(ts, type, JSON.stringify(meta))
      }
    } catch {
      // pass
    }
    renameSync(file, `${file}.bak`)
  }

  /** ai.json → settings(ai.apiKey / ai.baseUrl / ai.model) */
  private migrateAi(dataDir: string): void {
    const file = join(dataDir, 'ai.json')
    if (!existsSync(file)) return
    const count = this.prepare("SELECT COUNT(*) AS c FROM settings WHERE key LIKE 'ai.%'").get() as { c: number }
    if (count.c > 0) return
    try {
      const raw = JSON.parse(readFileSync(file, 'utf8')) as {
        apiKey?: string
        baseUrl?: string
        model?: string
      }
      const stmt = this.prepare('INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)')
      for (const [k, v] of Object.entries(raw)) {
        if (v != null) stmt.run(`ai.${k}`, JSON.stringify(v))
      }
    } catch {
      // pass
    }
    renameSync(file, `${file}.bak`)
  }
}
