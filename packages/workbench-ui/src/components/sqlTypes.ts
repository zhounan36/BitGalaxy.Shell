import type { SqlColumn, SqlErrorKind, SqlResult } from '@bgshell/protocol'

// ── SQL 面板共享类型 ───────────────────────────────────────────────────

/** 结果网格视图：列 + 行数据（保存最近一次结果供 CSV 导出） */
export interface SqlResultView {
  columns: SqlColumn[]
  rows: unknown[][]
  rowCount: number
  truncated?: boolean
  durationMs?: number
}

/** 执行历史条目（localStorage 持久化，最近 50 条） */
export interface SqlHistoryItem {
  sql: string
  ts: number
  durationMs?: number
  ok: boolean
}

// ── 错误文案映射（仿 describeTransferError：标题 + 建议动作）──────────────

export interface SqlErrorText {
  title: string
  hint?: string
}

const SQL_ERROR_KIND_TEXT: Record<string, SqlErrorText> = {
  permission_denied: { title: '权限不足或密码错误', hint: '请检查用户名/密码与账号权限后重试' },
  network: { title: '网络连接中断，请检查 SSH 连接', hint: '确认 SSH 会话存活后重新连接数据库' },
  timeout: { title: '查询超时', hint: '请缩小查询范围或加 WHERE/LIMIT 后重试' },
  driver_crashed: { title: '数据库驱动已崩溃，请重连', hint: '断开当前连接后重新建立连接' },
  driver_unavailable: {
    title: '驱动不可用',
    hint: '驱动未随安装包提供或已损坏，请重新安装应用',
  },
  tunnel_failed: { title: 'SSH 隧道建立失败', hint: '请检查 SSH 会话与远端数据库端口可达性' },
  too_large: { title: '结果集过大', hint: '请加 WHERE/LIMIT 缩小结果集后重试' },
  unknown: { title: '未知错误', hint: '请重试，若持续失败请查看 Agent 日志' },
}

/** SQL 失败分类 → 中文标题 + 建议动作（sql_error 直接显示原始 message） */
export function describeSqlError(
  kind: SqlErrorKind | string | undefined,
  message?: string,
): SqlErrorText {
  if (kind === 'sql_error' || !kind) {
    return { title: message ?? 'SQL 执行失败' }
  }
  const t = SQL_ERROR_KIND_TEXT[kind]
  if (!t) return { title: message ?? '操作失败' }
  return { title: t.title, hint: message ? `${message}\n建议：${t.hint}` : t.hint }
}

// ── 标识符引用与限定表名（mysql/mariadb 反引号，postgres/sqlite 双引号）──

/** 引号引用标识符（内嵌引号按 SQL 方言转义防注入） */
export function quoteIdent(name: string, dbType: string): string {
  if (dbType === 'mysql' || dbType === 'mariadb') {
    return `\`${name.replace(/`/g, '``')}\``
  }
  return `"${name.replace(/"/g, '""')}"`
}

/** 带库/模式限定的表引用：mysql `db`.`tbl` / pg "schema"."tbl" / sqlite "tbl"（忽略 schema） */
export function qualifiedTable(schema: string | undefined, table: string, dbType: string): string {
  const t = quoteIdent(table, dbType)
  if (!schema || dbType === 'sqlite') return t
  return `${quoteIdent(schema, dbType)}.${t}`
}

/** SQL 单引号字符串字面量（' → '' 转义） */
export function sqlStringLit(v: string): string {
  return `'${v.replace(/'/g, "''")}'`
}

// ── CSV 导出（纯前端转换 + Blob 下载）──────────────────────────────────

function csvCell(v: unknown): string {
  if (v === null || v === undefined) return ''
  const s = typeof v === 'string' ? v : JSON.stringify(v)
  return /[",\r\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s
}

export function resultToCsv(view: SqlResultView): string {
  const lines = [view.columns.map((c) => csvCell(c.name)).join(',')]
  for (const row of view.rows) lines.push(row.map(csvCell).join(','))
  return lines.join('\r\n')
}

export function downloadCsv(filename: string, csv: string): void {
  // BOM 前缀保证 Excel 正确识别 UTF-8 中文
  const bom = String.fromCharCode(0xfeff)
  const blob = new Blob([bom, csv], { type: 'text/csv;charset=utf-8' })
  const url = URL.createObjectURL(blob)
  const a = document.createElement('a')
  a.href = url
  a.download = filename
  a.click()
  URL.revokeObjectURL(url)
}

// ── 执行历史（localStorage，最近 50 条）────────────────────────────────

const HISTORY_LIMIT = 50

export function historyKey(sessionId: string): string {
  return `sql-history-${sessionId}`
}

export function loadHistory(sessionId: string): SqlHistoryItem[] {
  try {
    const raw = localStorage.getItem(historyKey(sessionId))
    if (!raw) return []
    const arr = JSON.parse(raw) as unknown
    return Array.isArray(arr) ? (arr as SqlHistoryItem[]).slice(0, HISTORY_LIMIT) : []
  } catch {
    return []
  }
}

export function pushHistory(sessionId: string, item: SqlHistoryItem): SqlHistoryItem[] {
  const next = [item, ...loadHistory(sessionId)].slice(0, HISTORY_LIMIT)
  try {
    localStorage.setItem(historyKey(sessionId), JSON.stringify(next))
  } catch {
    // localStorage 满/禁用时静默放弃持久化
  }
  return next
}

// ── 结果单元格渲染（NULL / 对象 / 数组）────────────────────────────────

export function cellText(v: unknown): string {
  if (v === null || v === undefined) return ''
  if (typeof v === 'string') return v
  if (typeof v === 'object') return JSON.stringify(v)
  return String(v)
}

/** 由 SqlResult 提取结果视图（query/query-more 返回列与行时） */
export function resultToView(r: SqlResult): SqlResultView | undefined {
  if (!r.columns) return undefined
  return {
    columns: r.columns,
    rows: r.rows ?? [],
    rowCount: r.rowCount ?? (r.rows?.length ?? 0),
    truncated: r.truncated,
    durationMs: r.durationMs,
  }
}
