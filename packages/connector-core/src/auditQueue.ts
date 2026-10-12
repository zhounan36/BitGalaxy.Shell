import { appendFile, mkdir } from 'node:fs/promises'
import { dirname } from 'node:path'

export interface AuditEvent {
  type: string
  ts: string
  [key: string]: unknown
}

/**
 * 审计队列（设计稿 §5.4 / §6.7）：M1 本地 jsonl 暂存（离线队列），
 * M2 起恢复联网后携带原始时间戳补报云端。
 */
export class AuditQueue {
  constructor(private readonly file: string) {}

  async append(type: string, meta: Record<string, unknown> = {}): Promise<void> {
    const event: AuditEvent = { type, ts: new Date().toISOString(), ...meta }
    await mkdir(dirname(this.file), { recursive: true })
    await appendFile(this.file, `${JSON.stringify(event)}\n`, 'utf8')
  }
}
