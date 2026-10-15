import { useEffect, useState } from 'react'
import { Alert, Form, Input, InputNumber, Modal, Progress, Select, Typography, message } from 'antd'
import type { SqlConnSpec, SqlDbType, SqlRequest, SqlResult } from '@bgshell/protocol'
import { agentSocket } from '../api/ws'
import { describeSqlError } from './sqlTypes'

export interface SqlConnFormValues {
  type: SqlDbType
  host: string
  port: number
  user: string
  password: string
  database: string
  sqlitePath: string
  name: string
}

const TYPE_LABELS: Record<SqlDbType, string> = {
  mysql: 'MySQL',
  mariadb: 'MariaDB',
  postgres: 'PostgreSQL',
  sqlite: 'SQLite',
}

const DEFAULT_PORTS: Record<Exclude<SqlDbType, 'sqlite'>, number> = {
  mysql: 3306,
  mariadb: 3306,
  postgres: 5432,
}

function defaultValues(type: SqlDbType = 'mysql'): SqlConnFormValues {
  return {
    type,
    host: '127.0.0.1',
    port: type === 'sqlite' ? 3306 : DEFAULT_PORTS[type],
    user: 'root',
    password: '',
    database: '',
    sqlitePath: '',
    name: TYPE_LABELS[type],
  }
}

interface DriverProgress {
  stage: string
  percent: number
  transferred: number
  total: number
}

/** 新建连接弹窗：连接表单 + 驱动下载进度（sql-progress）+ 错误提示。
 *  表单值由 SqlPane 按连接记住（内存，密码不落 localStorage）。 */
export function SqlConnModal({
  sessionId,
  hostId,
  open,
  request,
  initialValues,
  onClose,
  onConnected,
}: {
  sessionId: string
  hostId: string
  open: boolean
  request: (op: SqlRequest, timeoutMs?: number) => Promise<SqlResult>
  initialValues?: Partial<SqlConnFormValues>
  onClose: () => void
  onConnected: (r: SqlResult, spec: SqlConnSpec) => void
}) {
  const [form, setForm] = useState<SqlConnFormValues>(defaultValues())
  const [connecting, setConnecting] = useState(false)
  const [progress, setProgress] = useState<DriverProgress>()
  const [error, setError] = useState<{ title: string; hint?: string }>()

  // 每次打开重置为记忆值（缺省补默认值），连接失败不清空已填内容
  useEffect(() => {
    if (open) {
      setForm({ ...defaultValues(initialValues?.type ?? 'mysql'), ...initialValues })
      setProgress(undefined)
      setError(undefined)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open])

  // 驱动下载 / SQLite 文件下载进度（connect/fetch-sqlite 期间后端推送 sql-progress）
  useEffect(() => {
    const off = agentSocket.onControl((frame) => {
      if (frame.t !== 'sql-progress' || frame.s !== sessionId) return
      const m = frame.m
      if (!m.stage) return
      const total = m.total ?? 0
      setProgress({
        stage: m.stage,
        transferred: m.transferred ?? 0,
        total,
        percent: total > 0 ? Math.floor(((m.transferred ?? 0) / total) * 100) : 0,
      })
    })
    return off
  }, [sessionId])

  const setType = (type: SqlDbType) => {
    setForm((f) => ({
      ...f,
      type,
      port: type === 'sqlite' ? f.port : DEFAULT_PORTS[type],
    }))
  }

  const doConnect = async () => {
    const spec: SqlConnSpec = {
      type: form.type,
      hostId,
      host: form.host || undefined,
      port: form.port || undefined,
      user: form.user || undefined,
      password: form.password || undefined,
      database: form.database || undefined,
      sqlitePath: form.type === 'sqlite' ? form.sqlitePath || undefined : undefined,
      name: form.name || undefined,
    }
    if (form.type === 'sqlite' && !spec.sqlitePath) {
      setError({ title: '请填写远端 SQLite 文件路径' })
      return
    }
    setConnecting(true)
    setError(undefined)
    setProgress(undefined)
    try {
      // SQLite：先经 SFTP 拉取远端 .db 到本地临时文件，再以该文件建立连接
      let connSpec = spec
      if (spec.type === 'sqlite') {
        setProgress({ stage: `下载文件 ${spec.sqlitePath}`, percent: 0, transferred: 0, total: 0 })
        const f = await request({ op: 'fetch-sqlite', conn: spec, path: spec.sqlitePath ?? '' }, 600000)
        if (f.ok && f.sqliteRef) {
          connSpec = { ...spec, sqlitePath: f.sqliteRef }
        }
        // fetch 失败不阻断：回退用远端路径直接 connect，由后端裁决
      }
      // 驱动已内置安装包（无下载步骤）；宽超时兜底隧道建立与驱动握手
      const r = await request({ op: 'connect', conn: connSpec }, 180000)
      if (r.ok) {
        message.success(`连接成功${r.serverVersion ? `（${r.serverVersion}）` : ''}`)
        onConnected(r, spec)
        onClose()
      } else {
        setError(describeSqlError(r.errorKind, r.message))
      }
    } finally {
      setConnecting(false)
      setProgress(undefined)
    }
  }

  return (
    <Modal
      title="新建数据库连接"
      open={open}
      onCancel={onClose}
      onOk={() => void doConnect()}
      okText="连接"
      cancelText="取消"
      confirmLoading={connecting}
      width={460}
      destroyOnHidden
    >
      <Form layout="vertical" size="small">
        {error && (
          <Alert
            type="error"
            showIcon
            style={{ marginBottom: 12 }}
            message={error.title}
            description={error.hint && <span style={{ whiteSpace: 'pre-wrap' }}>{error.hint}</span>}
          />
        )}
        <Form.Item label="数据库类型" required>
          <Select<SqlDbType>
            value={form.type}
            onChange={setType}
            options={(Object.keys(TYPE_LABELS) as SqlDbType[]).map((t) => ({
              value: t,
              label: TYPE_LABELS[t],
            }))}
          />
        </Form.Item>
        {form.type === 'sqlite' ? (
          <Form.Item label="远端 SQLite 文件路径" required>
            <Input
              value={form.sqlitePath}
              onChange={(e) => setForm((f) => ({ ...f, sqlitePath: e.target.value }))}
              placeholder="/var/lib/app/data.db"
            />
          </Form.Item>
        ) : (
          <>
            <div style={{ display: 'flex', gap: 8 }}>
              <Form.Item label="主机" style={{ flex: 1, marginBottom: 12 }}>
                <Input
                  value={form.host}
                  onChange={(e) => setForm((f) => ({ ...f, host: e.target.value }))}
                  placeholder="127.0.0.1"
                />
              </Form.Item>
              <Form.Item label="端口" style={{ width: 110, marginBottom: 12 }}>
                <InputNumber
                  value={form.port}
                  min={1}
                  max={65535}
                  style={{ width: '100%' }}
                  onChange={(v) => setForm((f) => ({ ...f, port: v ?? 0 }))}
                />
              </Form.Item>
            </div>
            <Form.Item label="用户名">
              <Input
                value={form.user}
                onChange={(e) => setForm((f) => ({ ...f, user: e.target.value }))}
                placeholder="root"
              />
            </Form.Item>
            <Form.Item label="密码">
              <Input.Password
                value={form.password}
                onChange={(e) => setForm((f) => ({ ...f, password: e.target.value }))}
                autoComplete="new-password"
              />
            </Form.Item>
            <Form.Item label="默认数据库">
              <Input
                value={form.database}
                onChange={(e) => setForm((f) => ({ ...f, database: e.target.value }))}
                placeholder="可留空"
              />
            </Form.Item>
          </>
        )}
        <Form.Item label="连接名" style={{ marginBottom: connecting ? 12 : 0 }}>
          <Input
            value={form.name}
            onChange={(e) => setForm((f) => ({ ...f, name: e.target.value }))}
            placeholder={`${TYPE_LABELS[form.type]} 连接`}
          />
        </Form.Item>
      </Form>
      {progress && (
        <div>
          <Typography.Text type="secondary" style={{ fontSize: 12 }}>
            {progress.stage}
          </Typography.Text>
          <Progress
            percent={progress.percent}
            status="active"
            size="small"
            format={(p) =>
              progress.total > 0 ? `${p}%（${progress.transferred}/${progress.total} 字节）` : '下载中'
            }
          />
        </div>
      )}
    </Modal>
  )
}
