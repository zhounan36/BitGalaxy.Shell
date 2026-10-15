import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  Alert,
  Badge,
  Button,
  Drawer,
  InputNumber,
  Modal,
  Space,
  Spin,
  Table,
  Tabs,
  Tag,
  Typography,
  message,
} from 'antd'
import type { TabsProps } from 'antd'
import {
  AppstoreOutlined,
  ClearOutlined,
  CloseOutlined,
  ClockCircleOutlined,
  DatabaseOutlined,
  DownloadOutlined,
  LinkOutlined,
  PlayCircleOutlined,
  StopOutlined,
  TableOutlined,
  ThunderboltOutlined,
} from '@ant-design/icons'
import type {
  SqlConnSpec,
  SqlConnRecord,
  SqlDbType,
  SqlOp,
  SqlRequest,
  SqlResult,
} from '@bgshell/protocol'
import { agentSocket } from '../api/ws'
import { SqlEditor } from './SqlEditor'
import { SqlConnModal } from './SqlConnModal'
import type { SqlConnFormValues } from './SqlConnModal'
import { SqlSchemaTree } from './SqlSchemaTree'
import { SqlTableDataTab } from './SqlTableDataTab'
import { SqlTableStructTab } from './SqlTableStructTab'
import {
  cellText,
  describeSqlError,
  downloadCsv,
  loadHistory,
  pushHistory,
  resultToCsv,
  resultToView,
} from './sqlTypes'
import type { SqlHistoryItem, SqlResultView } from './sqlTypes'

/** 查询编辑器页签 */
interface QueryTab {
  key: string
  kind: 'query'
  title: string
  sql: string
}

/** 表数据 / 表结构页签（结构树双击或右键打开） */
interface TableTab {
  key: string
  kind: 'data' | 'struct'
  title: string
  schema?: string
  table: string
  viewKind?: string
}

type PaneTab = QueryTab | TableTab

interface ConnState {
  connId: string
  name: string
  type: SqlDbType
  serverVersion?: string
  spec: SqlConnSpec
}

interface ErrorState {
  title: string
  hint?: string
}

/** SQL 数据库管理面板（Navicat 精简版）：连接管理 + 结构树 + 多标签编辑器 +
 *  结果网格 + 表数据/表结构页签 + 执行历史。协议走 agentSocket 控制帧
 *  （t='sql' / 'sql-result' / 'sql-progress'）。 */
export function SqlPane({ sessionId }: { sessionId: string }) {
  const [conn, setConn] = useState<ConnState>()
  const [connModalOpen, setConnModalOpen] = useState(false)
  const [lastForm, setLastForm] = useState<Partial<SqlConnFormValues>>()

  const [tabs, setTabs] = useState<PaneTab[]>([{ key: 'sql-1', kind: 'query', title: '查询 1', sql: '' }])
  const [activeTab, setActiveTab] = useState('sql-1')
  const tabSeqRef = useRef(1)

  const [running, setRunning] = useState(false)
  const [runningKind, setRunningKind] = useState<'query' | 'execute'>('query')
  const [resultView, setResultView] = useState<SqlResultView>()
  const [execInfo, setExecInfo] = useState<{ affected: number; durationMs?: number }>()
  const [queryError, setQueryError] = useState<ErrorState>()
  const [maxRows, setMaxRows] = useState(500)

  const [history, setHistory] = useState<SqlHistoryItem[]>([])
  const [historyOpen, setHistoryOpen] = useState(false)
  const [treeTick, setTreeTick] = useState(0)

  const pendingRef = useRef(new Map<string, { resolve: (r: SqlResult) => void; timer: number }>())
  const runningQueryIdRef = useRef<string | undefined>(undefined)
  const activeTabRef = useRef(activeTab)
  useEffect(() => {
    activeTabRef.current = activeTab
  }, [activeTab])

  const activeTabEntry = tabs.find((t) => t.key === activeTab)
  const activeSql = activeTabEntry?.kind === 'query' ? activeTabEntry.sql : ''

  const setActiveSql = useCallback((sql: string) => {
    setTabs((prev) =>
      prev.map((t) => (t.kind === 'query' && t.key === activeTabRef.current ? { ...t, sql } : t)),
    )
  }, [])

  // 执行历史按会话加载
  useEffect(() => {
    setHistory(loadHistory(sessionId))
  }, [sessionId])

  /** 请求/响应关联（rid）+ 超时兜底（仿 SftpPane request()） */
  const request = useCallback(
    async (op: SqlRequest, timeoutMs = 20000, onRid?: (rid: string) => void): Promise<SqlResult> => {
      const rid = crypto.randomUUID()
      onRid?.(rid)
      return await new Promise<SqlResult>((resolve) => {
        const timer = window.setTimeout(() => {
          if (pendingRef.current.delete(rid)) resolve({ op: op.op, rid, ok: false, message: '请求超时', errorKind: 'timeout' })
        }, timeoutMs)
        pendingRef.current.set(rid, { resolve, timer })
        agentSocket.sendControl({ t: 'sql', s: sessionId, m: { ...op, rid } as SqlOp })
      })
    },
    [sessionId],
  )

  // 订阅 SQL 回包与长任务进度
  useEffect(() => {
    const off = agentSocket.onControl((frame) => {
      if (frame.t === 'sql-result' && frame.s === sessionId) {
        const m = frame.m
        if (m.rid) {
          const pending = pendingRef.current.get(m.rid)
          if (pending) {
            clearTimeout(pending.timer)
            pendingRef.current.delete(m.rid)
            pending.resolve(m)
          }
        }
      }
    })
    return off
  }, [sessionId])

  // 面板打开即未连接：自动弹出连接窗（所有 SQL 入口直达数据库连接，无需再点「新建连接」）
  useEffect(() => {
    setConnModalOpen(true)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  /** 弹窗完成 connect 后登记连接状态 */
  const adoptConn = useCallback((r: SqlResult, spec: SqlConnSpec) => {
    if (!r.connId) return
    setConn({
      connId: r.connId,
      name: spec.name ?? '数据库连接',
      type: spec.type,
      serverVersion: r.serverVersion,
      spec,
    })
    setTreeTick((t) => t + 1)
  }, [])

  // 已保存连接（sql-connections.db）：挂载拉列表 + 订阅回包（新建/删除后服务端总回最新列表）
  const [savedConns, setSavedConns] = useState<SqlConnRecord[]>([])
  useEffect(() => {
    agentSocket.sendControl({ t: 'sql-conns', m: { op: 'list' } })
    const off = agentSocket.onControl((frame) => {
      if (frame.t === 'sql-conns-result' && frame.m.conns) setSavedConns(frame.m.conns)
    })
    return off
  }, [])

  /** 已存连接一键直连 */
  const connectFromSaved = useCallback(
    async (rec: SqlConnRecord) => {
      const spec: SqlConnSpec = {
        type: rec.type,
        host: rec.host || undefined,
        port: rec.port || undefined,
        user: rec.user || undefined,
        password: rec.password || undefined,
        database: rec.database || undefined,
        sqlitePath: rec.type === 'sqlite' ? rec.sqlitePath || undefined : undefined,
        name: rec.name,
      }
      const r = await request({ op: 'connect', conn: spec }, 180000)
      if (r.ok) {
        message.success(`连接成功${r.serverVersion ? `（${r.serverVersion}）` : ''}`)
        adoptConn(r, spec)
      } else {
        message.error(describeSqlError(r.errorKind, r.message).title)
      }
    },
    [request, adoptConn],
  )

  const deleteSavedConn = useCallback((id: string) => {
    agentSocket.sendControl({ t: 'sql-conns', m: { op: 'delete', id } })
  }, [])

  const doDisconnect = useCallback(async () => {
    if (!conn) return
    const r = await request({ op: 'disconnect', connId: conn.connId })
    if (!r.ok) message.warning(describeSqlError(r.errorKind, r.message).title)
    setConn(undefined)
    setResultView(undefined)
    setExecInfo(undefined)
    // 表数据/结构页签依附连接：断开即关闭，回到单个查询页签
    setTabs([{ key: 'sql-1', kind: 'query', title: '查询 1', sql: '' }])
    setActiveTab('sql-1')
  }, [conn, request])

  /** 结构树双击/右键打开表页签（同表复用已有页签）：data = 数据网格 / struct = 结构详情 */
  const openTableTab = useCallback(
    (schema: string | undefined, table: string, viewKind: string | undefined, target: 'data' | 'struct') => {
      const key = `${target}:${schema ?? ''}::${table}`
      setTabs((prev) =>
        prev.some((t) => t.key === key)
          ? prev
          : [...prev, { key, kind: target, title: table, schema, table, viewKind }],
      )
      setActiveTab(key)
    },
    [],
  )

  const cancelRun = useCallback(async () => {
    const qid = runningQueryIdRef.current
    if (qid) {
      void request({ op: 'cancel', queryId: qid })
      // 就地 resolve 在途查询，UI 立即回到可操作状态
      const pending = pendingRef.current.get(qid)
      if (pending) {
        clearTimeout(pending.timer)
        pendingRef.current.delete(qid)
        pending.resolve({ op: 'query', rid: qid, ok: false, message: '查询已取消' })
        return
      }
    }
    setRunning(false)
    runningQueryIdRef.current = undefined
  }, [request])

  const recordHistory = useCallback(
    (sql: string, ok: boolean, durationMs?: number) => {
      setHistory(pushHistory(sessionId, { sql, ts: Date.now(), ok, durationMs }))
    },
    [sessionId],
  )

  /** 查询（结果进网格） */
  const runQuery = useCallback(
    async (sql: string) => {
      if (!conn || running || !sql.trim()) return
      setRunning(true)
      setRunningKind('query')
      setQueryError(undefined)
      setExecInfo(undefined)
      setResultView(undefined)
      try {
        // 查询允许更长超时（10 分钟兜底）；rid 即取消句柄（cancel 用）
        const r = await request(
          { op: 'query', connId: conn.connId, sql, maxRows },
          600000,
          (rid) => {
            runningQueryIdRef.current = rid
          },
        )
        runningQueryIdRef.current = undefined
        if (r.ok && r.columns) {
          setResultView(resultToView(r))
          recordHistory(sql, true, r.durationMs)
        } else {
          setQueryError(describeSqlError(r.errorKind, r.message))
          if (r.message !== '查询已取消') recordHistory(sql, false, r.durationMs)
        }
      } finally {
        setRunning(false)
      }
    },
    [conn, maxRows, recordHistory, request, running],
  )

  /** 执行（DDL/DML，无结果集）：二次确认后发送 */
  const confirmExecute = useCallback(
    (sql: string) => {
      if (!conn || running || !sql.trim()) return
      Modal.confirm({
        title: '确认执行 SQL？',
        content: (
          <Typography.Text
            code
            style={{ whiteSpace: 'pre-wrap', maxHeight: 120, overflow: 'auto', display: 'block' }}
          >
            {sql.length > 400 ? `${sql.slice(0, 400)}…` : sql}
          </Typography.Text>
        ),
        okText: '执行',
        cancelText: '取消',
        onOk: async () => {
          setRunning(true)
          setRunningKind('execute')
          setQueryError(undefined)
          setResultView(undefined)
          setExecInfo(undefined)
          try {
            const r = await request(
              { op: 'execute', connId: conn.connId, sql },
              600000,
              (rid) => {
                runningQueryIdRef.current = rid
              },
            )
            runningQueryIdRef.current = undefined
            if (r.ok) {
              setExecInfo({ affected: r.affected ?? 0, durationMs: r.durationMs })
              message.success(`执行成功（影响 ${r.affected ?? 0} 行）`)
              recordHistory(sql, true, r.durationMs)
            } else {
              setQueryError(describeSqlError(r.errorKind, r.message))
              recordHistory(sql, false, r.durationMs)
            }
          } finally {
            setRunning(false)
          }
        },
      })
    },
    [conn, recordHistory, request, running],
  )

  /** Ctrl+Enter 智能执行：SELECT 类走查询进结果网格，DDL/DML 走 execute（带二次确认） */
  const handleRun = useCallback(
    (sql: string) => {
      if (!conn) {
        message.warning('请先新建数据库连接')
        return
      }
      const head = sql.replace(/^\s*--[^\n]*\n/, '').trim().slice(0, 12).toLowerCase()
      if (/^(select|show|explain|describe|desc|with|pragma|values)\b/.test(head)) {
        void runQuery(sql)
      } else {
        confirmExecute(sql)
      }
    },
    [conn, runQuery, confirmExecute],
  )

  /** 结构树「生成 SELECT」：当前不在查询页签时先切回查询页签再执行（结果区在查询布局内） */
  const runSqlFromTree = useCallback(
    (sql: string) => {
      const active = tabs.find((t) => t.key === activeTabRef.current)
      if (!active || active.kind !== 'query') {
        const lastQuery = [...tabs].reverse().find((t) => t.kind === 'query')
        if (lastQuery) setActiveTab(lastQuery.key)
      }
      handleRun(sql)
    },
    [tabs, handleRun],
  )

  /** 结构树库节点「新建查询」：新建查询页签并聚焦 */
  const newQueryTab = useCallback(() => {
    tabSeqRef.current += 1
    const key = `sql-${tabSeqRef.current}`
    setTabs((prev) => [...prev, { key, kind: 'query' as const, title: `查询 ${tabSeqRef.current}`, sql: '' }])
    setActiveTab(key)
  }, [])

  // 编辑器面板（每个标签一个实例，切换标签即时保存草稿）
  const editorPane = useMemo(
    () => (
      <SqlEditor
        value={activeSql}
        onChange={setActiveSql}
        onRun={(text) => handleRun(text)}
        dbType={conn?.type}
      />
    ),
    [activeSql, setActiveSql, handleRun, conn],
  )

  /** 页签 label：查询=纯文本；表数据/结构=图标 + 表名（悬浮完整限定名） */
  const tabLabel = (t: PaneTab) => {
    if (t.kind === 'query') return t.title
    return (
      <span
        title={`${t.schema ? `${t.schema}.` : ''}${t.table}${t.kind === 'data' ? '（数据）' : '（结构）'}`}
        style={{ display: 'inline-flex', alignItems: 'center', gap: 4, maxWidth: 150 }}
      >
        {t.kind === 'data' ? <TableOutlined /> : <AppstoreOutlined />}
        <span style={{ overflow: 'hidden', textOverflow: 'ellipsis' }}>{t.table}</span>
      </span>
    )
  }

  const tabItems: TabsProps['items'] = tabs.map((t) => ({
    key: t.key,
    label: tabLabel(t),
    closable: tabs.length > 1,
    // 内容渲染在 Tabs 下方的兄弟层（display 切换保持各页签状态）
    children: null,
  }))

  const onTabsEdit: TabsProps['onEdit'] = (target, action) => {
    if (action === 'add') {
      tabSeqRef.current += 1
      const key = `sql-${tabSeqRef.current}`
      setTabs((prev) => [...prev, { key, kind: 'query' as const, title: `查询 ${tabSeqRef.current}`, sql: '' }])
      setActiveTab(key)
    } else if (action === 'remove') {
      const key = String(target)
      setTabs((prev) => {
        const next = prev.filter((t) => t.key !== key)
        if (next.length === 0) {
          tabSeqRef.current += 1
          const fresh: QueryTab = {
            key: `sql-${tabSeqRef.current}`,
            kind: 'query',
            title: `查询 ${tabSeqRef.current}`,
            sql: '',
          }
          setActiveTab(fresh.key)
          return [fresh]
        }
        if (key === activeTabRef.current) setActiveTab(next[next.length - 1].key)
        return next
      })
    }
  }

  // 结果网格列（动态列头：列名 + 类型小标签）
  const gridColumns = useMemo(
    () =>
      resultView?.columns.map((c, i) => ({
        title: (
          <span>
            {c.name}
            {c.dataType && (
              <Tag style={{ marginLeft: 6, fontSize: 10, lineHeight: '14px' }}>{c.dataType}</Tag>
            )}
          </span>
        ),
        dataIndex: `c${i}`,
        key: `c${i}`,
        render: (v: unknown) => {
          if (v === null || v === undefined) {
            return <span style={{ color: 'var(--text-muted)', fontStyle: 'italic' }}>NULL</span>
          }
          return <span>{cellText(v)}</span>
        },
      })) ?? [],
    [resultView],
  )

  const gridRows = useMemo(
    () =>
      resultView?.rows.map((row, i) => {
        const rec: Record<string, unknown> = { __key: i }
        row.forEach((v, j) => {
          rec[`c${j}`] = v
        })
        return rec
      }) ?? [],
    [resultView],
  )

  const exportCsv = () => {
    if (!resultView) return
    downloadCsv(`sql-result-${Date.now()}.csv`, resultToCsv(resultView))
    message.success('CSV 已导出')
  }

  const connected = !!conn

  return (
    <div style={{ height: '100%', display: 'flex', flexDirection: 'column', background: 'var(--bg-deep)' }}>
      {/* ── 连接栏 ── */}
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: 12,
          padding: '6px 12px',
          borderBottom: '1px solid var(--border-subtle)',
          background: 'var(--bg-base)',
          flexWrap: 'wrap',
        }}
      >
        <Space size={8}>
          <DatabaseOutlined style={{ color: 'var(--accent)' }} />
          <span style={{ color: 'var(--text-primary)', fontWeight: 600 }}>SQL</span>
        </Space>
        <Space size={8}>
          {conn ? (
            <>
              <Badge status="success" />
              <Typography.Text style={{ color: 'var(--text-primary)' }}>
                {conn.name}
              </Typography.Text>
              {conn.serverVersion && (
                <Tag color="green" style={{ fontSize: 11 }}>
                  {conn.serverVersion}
                </Tag>
              )}
            </>
          ) : (
            <>
              <Badge status="default" />
              <Typography.Text type="secondary">未连接</Typography.Text>
            </>
          )}
        </Space>
        <Space size={8}>
          <Button
            type="primary"
            size="small"
            icon={<LinkOutlined />}
            onClick={() => {
              setConnModalOpen(true)
            }}
          >
            新建连接
          </Button>
          {connected && (
            <Button size="small" danger onClick={() => void doDisconnect()}>
              断开
            </Button>
          )}
        </Space>
        <div style={{ flex: 1 }} />
        <Button
          size="small"
          icon={<ClockCircleOutlined />}
          onClick={() => setHistoryOpen(true)}
        >
          历史
        </Button>
      </div>

      {/* ── 已保存连接（未连接时展示；点击直连，关闭按钮删除档案） ── */}
      {!conn && savedConns.length > 0 && (
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: 6,
            padding: '4px 12px',
            borderBottom: '1px solid var(--border-subtle)',
            background: 'var(--bg-base)',
            flexWrap: 'wrap',
          }}
        >
          <Typography.Text type="secondary" style={{ fontSize: 12 }}>
            已保存连接：
          </Typography.Text>
          {savedConns.map((c) => (
            <Tag
              key={c.id}
              color="blue"
              style={{ cursor: 'pointer', marginInlineEnd: 0 }}
              closable
              onClose={(e) => {
                e.preventDefault()
                deleteSavedConn(c.id)
              }}
              onClick={() => void connectFromSaved(c)}
            >
              {c.name}
            </Tag>
          ))}
        </div>
      )}

      {/* ── 主体：左结构树 / 右编辑器 + 结果 ── */}
      <div style={{ flex: 1, display: 'flex', minHeight: 0 }}>
        <div
          style={{
            width: 260,
            minWidth: 260,
            borderRight: '1px solid var(--border-subtle)',
            background: 'var(--bg-base)',
            display: 'flex',
            flexDirection: 'column',
            overflow: 'hidden',
          }}
        >
          <SqlSchemaTree
            connId={conn?.connId}
            connLabel={conn ? `${conn.name}${conn.serverVersion ? `（${conn.serverVersion}）` : ''}` : '未连接'}
            dbType={conn?.type ?? 'mysql'}
            request={request}
            onRunSql={runSqlFromTree}
            onOpenTable={openTableTab}
            onNewQuery={newQueryTab}
            onError={(title, hint) => {
              setQueryError({ title, hint })
              message.error(title)
            }}
            refreshTick={treeTick}
          />
        </div>

        <div style={{ flex: 1, display: 'flex', flexDirection: 'column', minWidth: 0 }}>
          {/* 编辑器标签区 */}
          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              borderBottom: '1px solid var(--border-subtle)',
              background: 'var(--bg-base)',
            }}
          >
            <Tabs
              type="editable-card"
              size="small"
              items={tabItems}
              activeKey={activeTab}
              onChange={setActiveTab}
              onEdit={onTabsEdit}
              style={{ flex: 1, minWidth: 0 }}
            />
          </div>

          {/* 页签内容：查询 = 工具栏 + 编辑器 + 结果区；表数据/结构 = 全高组件。
              兄弟层 display 切换（非销毁），切页签不丢数据页的页码/筛选/排序状态 */}
          <div style={{ flex: 1, minHeight: 0 }}>
            {tabs.map((t) => (
              <div
                key={t.key}
                style={{
                  height: '100%',
                  display: t.key === activeTab ? 'flex' : 'none',
                  flexDirection: 'column',
                }}
              >
                {t.kind === 'data' && conn ? (
                  <SqlTableDataTab
                    connId={conn.connId}
                    dbType={conn.type}
                    schema={t.schema}
                    table={t.table}
                    viewKind={t.viewKind}
                    request={request}
                  />
                ) : t.kind === 'struct' && conn ? (
                  <SqlTableStructTab
                    connId={conn.connId}
                    dbType={conn.type}
                    schema={t.schema}
                    table={t.table}
                    viewKind={t.viewKind}
                    request={request}
                  />
                ) : t.kind === 'query' && t.key === activeTab ? (
                  <>
          {/* 工具栏 */}
          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 8,
              padding: '6px 12px',
              borderBottom: '1px solid var(--border-subtle)',
              flexWrap: 'wrap',
            }}
          >
            <Button
              type="primary"
              size="small"
              icon={<ThunderboltOutlined />}
              disabled={!connected || running}
              onClick={() => handleRun(activeSql)}
            >
              查询
            </Button>
            <Button
              size="small"
              icon={<PlayCircleOutlined />}
              disabled={!connected || running}
              onClick={() => confirmExecute(activeSql)}
            >
              执行
            </Button>
            {running ? (
              <Button size="small" danger icon={<StopOutlined />} onClick={() => void cancelRun()}>
                取消
              </Button>
            ) : null}
            {running && (
              <span style={{ display: 'inline-flex', alignItems: 'center', gap: 4 }}>
                <Spin size="small" />
                <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                  {runningKind === 'query' ? '查询中…' : '执行中…'}
                </Typography.Text>
              </span>
            )}
            <Typography.Text type="secondary" style={{ fontSize: 12 }}>
              每页行数
            </Typography.Text>
            <InputNumber
              size="small"
              value={maxRows}
              min={1}
              max={2000}
              style={{ width: 90 }}
              onChange={(v) => setMaxRows(v ?? 500)}
            />
            <Typography.Text type="secondary" style={{ fontSize: 12 }}>
              Ctrl+Enter 执行（有选区则执行选区）
            </Typography.Text>
            <div style={{ flex: 1 }} />
            <Button
              size="small"
              icon={<DownloadOutlined />}
              disabled={!resultView}
              onClick={exportCsv}
            >
              导出 CSV
            </Button>
            <Button
              size="small"
              icon={<ClearOutlined />}
              disabled={!resultView && !execInfo && !queryError}
              onClick={() => {
                setResultView(undefined)
                setExecInfo(undefined)
                setQueryError(undefined)
              }}
            >
              清空结果
            </Button>
          </div>

          {/* 编辑器 */}
          <div style={{ height: 220, borderBottom: '1px solid var(--border-subtle)', background: 'var(--bg-inset)' }}>
            {editorPane}
          </div>

          {/* 结果区 */}
          <div style={{ flex: 1, overflow: 'auto', padding: 8, minHeight: 0 }}>
            {queryError && (
              <Alert
                type="error"
                showIcon
                style={{ marginBottom: 8 }}
                message={queryError.title}
                description={
                  queryError.hint && <span style={{ whiteSpace: 'pre-wrap' }}>{queryError.hint}</span>
                }
              />
            )}
            {execInfo && (
              <Alert
                type="success"
                showIcon
                style={{ marginBottom: 8 }}
                message={`执行成功，影响 ${execInfo.affected} 行${
                  execInfo.durationMs !== undefined ? `（${execInfo.durationMs} ms）` : ''
                }`}
              />
            )}
            {resultView && (
              <>
                <Table
                  size="small"
                  rowKey="__key"
                  columns={gridColumns}
                  dataSource={gridRows}
                  pagination={{
                    defaultPageSize: 50,
                    showSizeChanger: true,
                    pageSizeOptions: [20, 50, 100, 200],
                    showTotal: (total) => `共 ${total} 行`,
                  }}
                  scroll={{ x: 'max-content' }}
                />
                <div
                  style={{
                    display: 'flex',
                    gap: 16,
                    alignItems: 'center',
                    color: 'var(--text-muted)',
                    fontSize: 12,
                    marginTop: 8,
                  }}
                >
                  <span>返回 {resultView.rowCount} 行</span>
                  {resultView.durationMs !== undefined && <span>耗时 {resultView.durationMs} ms</span>}
                  {resultView.truncated && (
                    <Tag color="orange">结果已截断（请加 WHERE/LIMIT 或调大每页行数）</Tag>
                  )}
                </div>
              </>
            )}
            {!queryError && !execInfo && !resultView && (
              <div
                style={{
                  color: 'var(--text-muted)',
                  textAlign: 'center',
                  marginTop: 48,
                  fontSize: 13,
                }}
              >
                连接数据库后，在编辑器中输入 SQL 并按 Ctrl+Enter 执行
              </div>
            )}
          </div>
                  </>
                ) : null}
              </div>
            ))}
          </div>
        </div>
      </div>

      {/* ── 执行历史抽屉 ── */}
      <Drawer
        title="执行历史（最近 50 条）"
        placement="right"
        width={420}
        open={historyOpen}
        onClose={() => setHistoryOpen(false)}
        extra={
          <Button
            size="small"
            icon={<CloseOutlined />}
            onClick={() => {
              try {
                localStorage.removeItem(`sql-history-${sessionId}`)
              } catch {
                // localStorage 不可用时忽略
              }
              setHistory([])
            }}
          >
            清空
          </Button>
        }
      >
        {history.length === 0 && (
          <div style={{ color: 'var(--text-muted)', textAlign: 'center', marginTop: 24 }}>暂无历史</div>
        )}
        {history.map((h, i) => (
          <div
            key={`${h.ts}-${i}`}
            onClick={() => {
              setActiveSql(h.sql)
              setHistoryOpen(false)
              message.success('已回填到编辑器')
            }}
            style={{
              padding: '8px 10px',
              marginBottom: 8,
              border: '1px solid var(--border-subtle)',
              borderRadius: 6,
              cursor: 'pointer',
              background: 'var(--bg-base)',
            }}
          >
            <div
              style={{
                display: 'flex',
                justifyContent: 'space-between',
                marginBottom: 4,
                fontSize: 12,
                color: 'var(--text-muted)',
              }}
            >
              <span>{new Date(h.ts).toLocaleString()}</span>
              <Space size={8}>
                {h.durationMs !== undefined && <span>{h.durationMs} ms</span>}
                <Tag color={h.ok ? 'green' : 'red'} style={{ fontSize: 11, lineHeight: '16px' }}>
                  {h.ok ? '成功' : '失败'}
                </Tag>
              </Space>
            </div>
            <div
              style={{
                fontFamily: 'Consolas, Monaco, monospace',
                fontSize: 12,
                color: 'var(--text-secondary)',
                whiteSpace: 'pre-wrap',
                maxHeight: 60,
                overflow: 'hidden',
              }}
            >
              {h.sql}
            </div>
          </div>
        ))}
      </Drawer>

      {/* ── 新建连接弹窗 ── */}
      <SqlConnModal
        sessionId={sessionId}
        open={connModalOpen}
        request={request}
        initialValues={lastForm}
        onClose={() => setConnModalOpen(false)}
        onConnected={(r, spec) => {
          // 记住表单值（内存；密码仅本组件状态，不进 localStorage）
          setLastForm({
            type: spec.type,
            host: spec.host ?? '127.0.0.1',
            port: spec.port ?? (spec.type === 'postgres' ? 5432 : 3306),
            user: spec.user ?? '',
            password: spec.password ?? '',
            database: spec.database ?? '',
            sqlitePath: spec.sqlitePath ?? '',
            name: spec.name ?? '',
          })
          adoptConn(r, spec)
        }}
      />
    </div>
  )
}
