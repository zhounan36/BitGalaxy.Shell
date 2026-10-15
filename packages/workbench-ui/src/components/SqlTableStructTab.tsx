import { useCallback, useEffect, useMemo, useState } from 'react'
import { Alert, Button, Space, Spin, Table, Tabs, Tag, Typography, message } from 'antd'
import { CopyOutlined, ReloadOutlined } from '@ant-design/icons'
import type { SqlDbType, SqlColumnInfo, SqlIndexInfo, SqlRequest, SqlResult } from '@bgshell/protocol'
import { describeSqlError, qualifiedTable, sqlStringLit } from './sqlTypes'

interface ErrorState {
  title: string
  hint?: string
}

/** 表结构页签：列（名称/类型/PK/可空/默认值）+ 索引（名称/唯一/列）+ DDL（mysql/sqlite）。
 *  数据经 introspect columns/indexes 获取；DDL 懒加载（切到 DDL 标签才查询）。 */
export function SqlTableStructTab({
  connId,
  dbType,
  schema,
  table,
  viewKind,
  request,
}: {
  connId: string
  dbType: SqlDbType | string
  schema?: string
  table: string
  viewKind?: string
  request: (op: SqlRequest, timeoutMs?: number) => Promise<SqlResult>
}) {
  const [fields, setFields] = useState<SqlColumnInfo[]>()
  const [indexes, setIndexes] = useState<SqlIndexInfo[]>()
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<ErrorState>()
  const [indexError, setIndexError] = useState<string>()
  const [structTab, setStructTab] = useState('columns')
  const [ddl, setDdl] = useState<string>()
  const [ddlState, setDdlState] = useState<'idle' | 'loading' | 'error'>('idle')
  const [ddlError, setDdlError] = useState<string>()

  const from = useMemo(() => qualifiedTable(schema, table, dbType), [schema, table, dbType])
  const ddlSupported = dbType === 'mysql' || dbType === 'mariadb' || dbType === 'sqlite'

  const load = useCallback(async () => {
    if (!connId) return
    setLoading(true)
    setError(undefined)
    setIndexError(undefined)
    const [colsR, idxR] = await Promise.all([
      request({ op: 'introspect', connId, scope: 'columns', schema, table }),
      request({ op: 'introspect', connId, scope: 'indexes', schema, table }),
    ])
    setLoading(false)
    if (!colsR.ok) {
      setError(describeSqlError(colsR.errorKind, colsR.message))
      return
    }
    setFields(colsR.fields ?? [])
    if (idxR.ok) {
      setIndexes(idxR.indexes ?? [])
    } else {
      setIndexes([])
      setIndexError(idxR.message ?? '索引信息获取失败')
    }
  }, [connId, schema, table, request])

  useEffect(() => {
    void load()
  }, [load])

  // DDL 懒加载：首次切到 DDL 标签才查询（mysql SHOW CREATE / sqlite sqlite_master）
  useEffect(() => {
    if (structTab !== 'ddl' || ddlState !== 'idle' || !connId || !ddlSupported) return
    let cancelled = false
    setDdlState('loading')
    const sql =
      dbType === 'sqlite'
        ? `SELECT sql FROM sqlite_master WHERE type IN ('table','view') AND name = ${sqlStringLit(table)}`
        : `SHOW CREATE ${viewKind === 'view' ? 'VIEW' : 'TABLE'} ${from}`
    void (async () => {
      const r = await request({ op: 'query', connId, sql, maxRows: 1 })
      if (cancelled) return
      // mysql SHOW CREATE 结果第 2 列为建表语句；sqlite sqlite_master.sql 为第 1 列
      const text = r.ok ? String(r.rows?.[0]?.[1] ?? r.rows?.[0]?.[0] ?? '') : undefined
      if (r.ok && text) {
        setDdl(text)
        setDdlState('idle')
      } else {
        setDdlError(r.ok ? '未获取到 DDL' : (r.message ?? 'DDL 查询失败'))
        setDdlState('error')
      }
    })()
    return () => {
      cancelled = true
    }
  }, [structTab, ddlState, connId, ddlSupported, dbType, viewKind, from, table, request])

  const fieldColumns = [
    { title: '列名', dataIndex: 'name', key: 'name', ellipsis: true },
    {
      title: '类型',
      dataIndex: 'dataType',
      key: 'dataType',
      width: 140,
      render: (v?: string) => (v ? <Tag style={{ fontSize: 11 }}>{v}</Tag> : <span>-</span>),
    },
    {
      title: '主键',
      dataIndex: 'pk',
      key: 'pk',
      width: 70,
      render: (v: boolean) => (v ? <Tag color="gold" style={{ fontSize: 11 }}>PK</Tag> : <span>-</span>),
    },
    {
      title: '可空',
      dataIndex: 'nullable',
      key: 'nullable',
      width: 70,
      render: (v: boolean) => (
        <span style={{ color: v ? 'var(--text-secondary)' : 'var(--text-muted)' }}>{v ? '是' : '否'}</span>
      ),
    },
    {
      title: '默认值',
      dataIndex: 'default',
      key: 'default',
      ellipsis: true,
      render: (v?: string) => (v !== undefined && v !== null && v !== '' ? <code>{v}</code> : <span>-</span>),
    },
  ]

  const indexColumns = [
    { title: '索引名', dataIndex: 'name', key: 'name', ellipsis: true },
    {
      title: '唯一',
      dataIndex: 'unique',
      key: 'unique',
      width: 80,
      render: (v: boolean) => (v ? <Tag color="blue" style={{ fontSize: 11 }}>UNIQUE</Tag> : <span>-</span>),
    },
    {
      title: '列',
      dataIndex: 'columns',
      key: 'columns',
      ellipsis: true,
      render: (v: string[]) => <code style={{ fontSize: 12 }}>{v.join(', ')}</code>,
    },
  ]

  const copyDdl = async () => {
    if (!ddl) return
    try {
      await navigator.clipboard.writeText(ddl)
      message.success('DDL 已复制')
    } catch {
      message.error('复制失败')
    }
  }

  const structItems = [
    {
      key: 'columns',
      label: `列 (${fields?.length ?? 0})`,
      children: (
        <Table
          size="small"
          rowKey="name"
          columns={fieldColumns}
          dataSource={fields ?? []}
          pagination={false}
          scroll={{ x: 'max-content' }}
        />
      ),
    },
    {
      key: 'indexes',
      label: `索引 (${indexes?.length ?? 0})`,
      children: indexError ? (
        <Alert type="warning" showIcon message={indexError} />
      ) : (
        <Table
          size="small"
          rowKey="name"
          columns={indexColumns}
          dataSource={indexes ?? []}
          pagination={false}
          scroll={{ x: 'max-content' }}
        />
      ),
    },
    ...(ddlSupported
      ? [
          {
            key: 'ddl',
            label: 'DDL',
            children: ddlState === 'loading' ? (
              <div style={{ textAlign: 'center', padding: 32, color: 'var(--text-muted)' }}>
                <Spin /> <span style={{ marginLeft: 8 }}>加载中…</span>
              </div>
            ) : ddlState === 'error' ? (
              <Alert type="warning" showIcon message={ddlError} />
            ) : (
              <pre
                style={{
                  margin: 0,
                  padding: 12,
                  background: 'var(--bg-inset)',
                  border: '1px solid var(--border-subtle)',
                  borderRadius: 6,
                  fontFamily: 'Consolas, Monaco, monospace',
                  fontSize: 12,
                  whiteSpace: 'pre-wrap',
                  wordBreak: 'break-all',
                  color: 'var(--text-secondary)',
                }}
              >
                {ddl}
              </pre>
            ),
          },
        ]
      : []),
  ]

  return (
    <div style={{ height: '100%', display: 'flex', flexDirection: 'column', background: 'var(--bg-deep)' }}>
      {/* 头部 */}
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: 8,
          padding: '6px 12px',
          borderBottom: '1px solid var(--border-subtle)',
          background: 'var(--bg-base)',
        }}
      >
        <Space size={6}>
          <Typography.Text strong style={{ color: 'var(--text-primary)' }}>
            {schema ? `${schema}.` : ''}
            {table}
          </Typography.Text>
          {viewKind === 'view' && <Tag style={{ fontSize: 11 }}>视图</Tag>}
        </Space>
        <div style={{ flex: 1 }} />
        <Button size="small" icon={<ReloadOutlined />} loading={loading} onClick={() => void load()}>
          刷新
        </Button>
      </div>

      {/* 结构视图 */}
      <div style={{ flex: 1, minHeight: 0, overflow: 'auto', padding: 8 }}>
        {error ? (
          <Alert
            type="error"
            showIcon
            message={error.title}
            description={error.hint && <span style={{ whiteSpace: 'pre-wrap' }}>{error.hint}</span>}
          />
        ) : loading && !fields ? (
          <div style={{ textAlign: 'center', marginTop: 48, color: 'var(--text-muted)' }}>
            <Spin /> <span style={{ marginLeft: 8 }}>加载中…</span>
          </div>
        ) : (
          <>
            <Tabs
              size="small"
              items={structItems}
              activeKey={structTab}
              onChange={setStructTab}
              tabBarStyle={{ marginBottom: 8 }}
            />
            {ddl && structTab === 'ddl' && (
              <Button size="small" icon={<CopyOutlined />} style={{ marginTop: 8 }} onClick={() => void copyDdl()}>
                复制 DDL
              </Button>
            )}
          </>
        )}
      </div>
    </div>
  )
}
