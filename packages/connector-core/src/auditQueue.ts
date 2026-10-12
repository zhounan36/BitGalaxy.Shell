import type { Db } from './db.js'

export interface AuditEvent {
  type: string
  ts: string
  [key: string]: unknown
}

/** 审计队列（SQLite 持久化，替代 jsonl 追加） */
export class AuditQueue {
  private readonly stInsert: ReturnType<Db['prepare']>

  constructor(private readonly db: Db) {
    this.stInsert = db.prepare('INSERT INTO audit_log (ts, type, meta) VALUES (?, ?, ?)')
  }

  async append(type: string, meta: Record<string, unknown> = {}): Promise<void> {
    this.stInsert.run(new Date().toISOString(), type, JSON.stringify(meta))
  }
}
