import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { Alert, Button, Empty, Input, InputNumber, Select, Space, Spin, Table, Tag, Typography, message } from 'antd'
import type { TableProps } from 'antd'
import {
  CopyOutlined,
  DownloadOutlined,
  LeftOutlined,
  ReloadOutlined,
  RightOutlined,
  SearchOutlined,
} from '@ant-design/icons'
import type { SqlRequest, SqlResult } from '@bgshell/protocol'
import {
  cellText,
  describeSqlError,
  downloadCsv,
  qualifiedTable,
  quoteIdent,
  resultToCsv,
  resultToView,
} from './sqlTypes'
import type { SqlResultView } from './sqlTypes'

const PAGE_SIZE_OPTIONS = [50, 100, 200, 500]
/** 数据页查询超时（后端预算 60s，前端兜底 70s） */
const QUERY_TIMEOUT_MS = 60000
/** COUNT(*) 大表可能较慢（后端 120s 预算，前端 130s 兜底） */
const COUNT_TIMEOUT_MS = 120000

interface ErrorState {
  title: string
  hint?: string
}

/** 表数据浏览页签（主流 SQL 工具「打开表」）：服务端分页（LIMIT/OFFSET）+
 *  点击列头远程排序 + WHERE 筛选 + 行数统计 + 复制 SQL / 导出 CSV。 */
export function SqlTableDataTab({
  connId,
  dbType,
  schema,
  table,
  viewKind,
  request,
}: {
  connId: string
  dbType: string
  /** 表所属库/模式（sqlite 忽略） */
  schema?: string
  table: string
  /** table | view（视图打开数据同样适用） */
  viewKind?: string
  request: (op: SqlRequest, timeoutMs?: number) => Promise<SqlResult>
}) {
  const [view, setView] = useState<SqlResultView>()
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<ErrorState>()
  const [where, setWhere] = useState('')
  const [appliedWhere, setAppliedWhere] = useState('')
  const [page, setPage] = useState(0)
  const [pageSize, setPageSize] = useState(100)
  const [orderBy, setOrderBy] = useState<{ col: string; dir: 'asc' | 'desc' }>()
  const [total, setTotal] = useState<number>()
  const [counting, setCounting] = useState(false)
  const [refreshTick, setRefreshTick] = useState(0)
  const seqRef = useRef(0)

  const from = useMemo(() => qualifiedTable(schema, table, dbType), [schema, table, dbType])

  const buildSql = useCallback(
    (opts: { where: string; orderBy?: { col: string; dir: 'asc' | 'desc' }; limit: number; offset: number }) => {
      let sql = `SELECT * FROM ${from}`
      if (opts.where.trim()) sql += ` WHERE ${opts.where.trim()}`
      if (opts.orderBy) sql += ` ORDER BY ${quoteIdent(opts.orderBy.col, dbType)} ${opts.orderBy.dir.toUpperCase()}`
      sql += ` LIMIT ${opts.limit} OFFSET ${opts.offset}`
      return sql
    },
    [from, dbType],
  )

  /** 当前页 SQL（复制到剪贴板用，与实际执行的完全一致） */
  const currentSql = useMemo(
    () => buildSql({ where: appliedWhere, orderBy, limit: pageSize, offset: page * pageSize }),
    [buildSql, appliedWhere, orderBy, pageSize, page],
  )

  // 加载当前页（页码/每页行数/筛选/排序/手动刷新变化时重查）
  useEffect(() => {
    if (!connId) return
    const seq = ++seqRef.current
    setLoading(true)
    setError(undefined)
    const sql = buildSql({ where: appliedWhere, orderBy, limit: pageSize, offset: page * pageSize })
    void (async () => {
      const r = await request(
        { op: 'query', connId, sql, maxRows: pageSize, timeoutMs: QUERY_TIMEOUT_MS },
        QUERY_TIMEOUT_MS + 10000,
      )
      if (seq !== seqRef.current) return
      setLoading(false)
      if (r.ok && r.columns) {
        setView(resultToView(r))
      } else {
        setView(undefined)
        setError(describeSqlError(r.errorKind, r.message))
      }
    })()
  }, [connId, appliedWhere, orderBy, page, pageSize, buildSql, request, refreshTick])

  // 行数统计（筛选条件变化时重新计数；翻页/排序不重数）
  useEffect(() => {
    if (!connId) return
    let cancelled = false
    setCounting(true)
    const w = appliedWhere.trim()
    const sql = `SELECT COUNT(*) FROM ${from}${w ? ` WHERE ${w}` : ''}`
    void (async () => {
      const r = await request(
        { op: 'query', connId, sql, maxRows: 1, timeoutMs: COUNT_TIMEOUT_MS },
        COUNT_TIMEOUT_MS + 10000,
      )
      if (cancelled) return
      setCounting(false)
      if (r.ok && r.rows?.[0]) {
        const n = Number(r.rows[0][0])
        setTotal(Number.isFinite(n) ? n : undefined)
      } else {
        setTotal(undefined)
      }
    })()
    return () => {
      cancelled = true
    }
  }, [connId, appliedWhere, from, request, refreshTick])

  // 点击列头远程排序（列名来自结果集列，白名单无注入风险）
  const onTableChange: TableProps<Record<string, unknown>>['onChange'] = (_pg, _filters, sorter) => {
    const s = (Array.isArray(sorter) ? sorter[0] : sorter) as { field?: unknown; order?: string } | undefined
    const field = typeof s?.field === 'string' ? s.field : undefined
    if (!field || !s?.order || !view) {
      setOrderBy(undefined)
      return
    }
    const idx = Number(field.slice(1))
    const col = view.columns[idx]?.name
    if (!col) return
    setOrderBy({ col, dir: s.order === 'ascend' ? 'asc' : 'desc' })
    setPage(0)
  }

  const gridColumns = useMemo(
    () =>
      view?.columns.map((c, i) => ({
        title: (
          <span>
            {c.name}
            {c.dataType && <Tag style={{ marginLeft: 6, fontSize: 10, lineHeight: '14px' }}>{c.dataType}</Tag>}
          </span>
        ),
        dataIndex: `c${i}`,
        key: `c${i}`,
        ellipsis: true,
        sorter: true,
        sortOrder: orderBy?.col === c.name ? (orderBy.dir === 'asc' ? ('ascend' as const) : ('descend' as const)) : undefined,
        render: (v: unknown) => {
          if (v === null || v === undefined) {
            return <span style={{ color: 'var(--text-muted)', fontStyle: 'italic' }}>NULL</span>
          }
          return <span>{cellText(v)}</span>
        },
      })) ?? [],
    [view, orderBy],
  )

  const gridRows = useMemo(
    () =>
      view?.rows.map((row, i) => {
        const rec: Record<string, unknown> = { __key: i }
        row.forEach((v, j) => {
          rec[`c${j}`] = v
        })
        return rec
      }) ?? [],
    [view],
  )

  const applyWhere = () => {
    setAppliedWhere(where.trim())
    setPage(0)
  }

  const copySql = async () => {
    try {
      await navigator.clipboard.writeText(currentSql)
      message.success('SQL 已复制')
    } catch {
      message.error('复制失败')
    }
  }

  const exportCsv = () => {
    if (!view) return
    downloadCsv(`${table}-${Date.now()}.csv`, resultToCsv(view))
    message.success('CSV 已导出')
  }

  const maxPage = total !== undefined ? Math.max(1, Math.ceil(total / pageSize)) : undefined
  const canPrev = page > 0
  const canNext = !!view && view.rows.length >= pageSize

  return (
    <div style={{ height: '100%', display: 'flex', flexDirection: 'column', background: 'var(--bg-deep)' }}>
      {/* 工具栏 */}
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: 8,
          padding: '6px 12px',
          borderBottom: '1px solid var(--border-subtle)',
          background: 'var(--bg-base)',
          flexWrap: 'wrap',
        }}
      >
        <Space size={6}>
          <Typography.Text strong style={{ color: 'var(--text-primary)' }}>
            {schema ? `${schema}.` : ''}
            {table}
          </Typography.Text>
          {viewKind === 'view' && <Tag style={{ fontSize: 11 }}>视图</Tag>}
        </Space>
        <Button
          size="small"
          icon={<ReloadOutlined />}
          onClick={() => setRefreshTick((t) => t + 1)}
          loading={loading}
        >
          刷新
        </Button>
        <Input
          size="small"
          allowClear
          placeholder="WHERE 条件，如 id > 100 AND name LIKE '%a%'"
          style={{ width: 280 }}
          value={where}
          onChange={(e) => setWhere(e.target.value)}
          onPressEnter={applyWhere}
        />
        <Button size="small" icon={<SearchOutlined />} onClick={applyWhere}>
          筛选
        </Button>
        {appliedWhere && (
          <Button
            size="small"
            type="text"
            onClick={() => {
              setWhere('')
              setAppliedWhere('')
              setPage(0)
            }}
          >
            清除筛选
          </Button>
        )}
        <div style={{ flex: 1 }} />
        <Space size={4}>
          <Button
            size="small"
            type="text"
            icon={<LeftOutlined />}
            disabled={!canPrev}
            onClick={() => setPage((p) => Math.max(0, p - 1))}
          />
          <span style={{ fontSize: 12, color: 'var(--text-secondary)' }}>
            第 {page + 1}{maxPage !== undefined ? ` / ${maxPage}` : ''} 页
          </span>
          <Button size="small" type="text" icon={<RightOutlined />} disabled={!canNext} onClick={() => setPage((p) => p + 1)} />
          {maxPage !== undefined && maxPage > 1 && (
            <InputNumber
              size="small"
              min={1}
              max={maxPage}
              style={{ width: 64 }}
              placeholder="跳页"
              onPressEnter={(e) => {
                const v = Number((e.target as HTMLInputElement).value)
                if (Number.isFinite(v)) setPage(Math.min(maxPage, Math.max(1, v)) - 1)
              }}
            />
          )}
        </Space>
        <Typography.Text type="secondary" style={{ fontSize: 12 }}>
          每页
        </Typography.Text>
        <Select
          size="small"
          value={pageSize}
          style={{ width: 76 }}
          options={PAGE_SIZE_OPTIONS.map((n) => ({ value: n, label: String(n) }))}
          onChange={(v) => {
            setPageSize(v)
            setPage(0)
          }}
        />
        <Space size={4}>
          {counting ? (
            <Spin size="small" />
          ) : (
            <Typography.Text type="secondary" style={{ fontSize: 12 }}>
              {total !== undefined ? `共 ${total.toLocaleString()} 行` : '行数未知'}
            </Typography.Text>
          )}
        </Space>
        <Button size="small" icon={<CopyOutlined />} onClick={() => void copySql()}>
          复制 SQL
        </Button>
        <Button size="small" icon={<DownloadOutlined />} disabled={!view} onClick={exportCsv}>
          CSV
        </Button>
      </div>

      {/* 数据网格 */}
      <div style={{ flex: 1, minHeight: 0, overflow: 'auto', padding: 8 }}>
        {error ? (
          <Alert
            type="error"
            showIcon
            message={error.title}
            description={error.hint && <span style={{ whiteSpace: 'pre-wrap' }}>{error.hint}</span>}
          />
        ) : view ? (
          <>
            <Table
              size="small"
              rowKey="__key"
              columns={gridColumns}
              dataSource={gridRows}
              pagination={false}
              scroll={{ x: 'max-content' }}
              onChange={onTableChange}
              loading={loading}
            />
            <div style={{ display: 'flex', gap: 16, alignItems: 'center', color: 'var(--text-muted)', fontSize: 12, marginTop: 8 }}>
              <span>本页 {view.rows.length} 行</span>
              {view.durationMs !== undefined && <span>耗时 {view.durationMs} ms</span>}
              {view.truncated && <Tag color="orange">结果已截断</Tag>}
              <span style={{ color: 'var(--text-muted)' }}>点击列头排序</span>
            </div>
          </>
        ) : loading ? (
          <div style={{ textAlign: 'center', marginTop: 48, color: 'var(--text-muted)' }}>
            <Spin /> <span style={{ marginLeft: 8 }}>加载中…</span>
          </div>
        ) : (
          <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="无数据" style={{ marginTop: 48 }} />
        )}
      </div>
    </div>
  )
}
