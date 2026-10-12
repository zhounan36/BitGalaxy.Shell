import { useCallback, useEffect, useRef, useState } from 'react'
import {
  Alert,
  Breadcrumb,
  Button,
  Empty,
  Input,
  Modal,
  Popconfirm,
  Progress,
  Space,
  Spin,
  Table,
  Tag,
  Tooltip,
  message,
} from 'antd'
import {
  ArrowUpOutlined,
  FileOutlined,
  FolderAddOutlined,
  FolderOutlined,
  LinkOutlined,
  ReloadOutlined,
  UploadOutlined,
} from '@ant-design/icons'
import type { SftpEntry, SftpRequest, SftpResult } from '@bgshell/protocol'
import { agentSocket } from '../api/ws'
import { formatBytes, formatTime } from '../utils/format'

const CHUNK = 64 * 1024

interface TransferView {
  key: string
  name: string
  dir: 'up' | 'down'
  transferred: number
  total: number
  done: boolean
}

/** SFTP 文件管理面板：目录浏览 / 上传(含拖拽) / 下载 / 新建/重命名/删除 */
export function SftpPane({ sessionId }: { sessionId: string }) {
  const [path, setPath] = useState<string>()
  const [entries, setEntries] = useState<SftpEntry[]>()
  const [loading, setLoading] = useState(false)
  const [opened, setOpened] = useState(false)
  const [error, setError] = useState<string>()
  const [transfers, setTransfers] = useState<TransferView[]>([])
  const pendingRef = useRef(new Map<string, { resolve: (r: SftpResult) => void; timer: number }>())
  const doneWaitersRef = useRef(new Map<string, () => void>())
  const fileInputRef = useRef<HTMLInputElement>(null)

  /** 请求/响应关联（rid）+ 超时兜底 */
  const request = useCallback(
    async (op: SftpRequest): Promise<SftpResult> => {
      const rid = crypto.randomUUID()
      return await new Promise<SftpResult>((resolve) => {
        const timer = window.setTimeout(() => {
          if (pendingRef.current.delete(rid)) resolve({ op: op.op, rid, ok: false, message: '请求超时' })
        }, 20000)
        pendingRef.current.set(rid, { resolve, timer })
        agentSocket.sendControl({ t: 'sftp', s: sessionId, m: { ...op, rid } })
      })
    },
    [sessionId],
  )

  const ls = useCallback(
    async (p: string) => {
      setLoading(true)
      try {
        const r = await request({ op: 'ls', path: p })
        if (r.ok && r.entries) {
          setPath(r.resolvedPath)
          setEntries(r.entries)
          setError(undefined)
        } else if (!r.ok) {
          setError(r.message ?? '读取目录失败')
          message.error(r.message ?? '读取目录失败')
        }
      } finally {
        setLoading(false)
      }
    },
    [request],
  )

  // 订阅结果/进度/下载完成信号
  useEffect(() => {
    const off = agentSocket.onControl((frame) => {
      if (frame.t === 'session-opened' && frame.s === sessionId) setOpened(true)
      if (frame.t === 'error' && frame.s === sessionId) setError(frame.m.message)
      if (frame.t === 'sftp-result' && frame.s === sessionId) {
        const { m } = frame
        if (m.rid) {
          const pending = pendingRef.current.get(m.rid)
          if (pending) {
            clearTimeout(pending.timer)
            pendingRef.current.delete(m.rid)
            pending.resolve(m)
          }
        }
        if (m.op === 'download-done' && m.opId) {
          doneWaitersRef.current.get(m.opId)?.()
        }
      }
      if (frame.t === 'sftp-progress' && frame.s === sessionId) {
        setTransfers((prev) =>
          prev.map((t) =>
            t.key === frame.m.opId ? { ...t, transferred: frame.m.transferred, total: frame.m.total } : t,
          ),
        )
      }
    })
    return off
  }, [sessionId])

  // 首个 ls 等会话就绪：session-opened 在 SSH 建连（含指纹确认）并挂载 SFTP 子系统后才发出，
  // 提前请求会被后端以"SFTP 会话未就绪"拒绝
  useEffect(() => {
    if (opened) void ls('.')
  }, [opened, ls])

  const join = (dir: string, name: string) => (dir.endsWith('/') ? `${dir}${name}` : `${dir}/${name}`)
  const parent = (p: string) => {
    const parts = p.split('/').filter(Boolean)
    return parts.length <= 1 ? '/' : `/${parts.slice(0, -1).join('/')}`
  }

  const doMkdir = () => {
    let value = ''
    Modal.confirm({
      title: '新建文件夹',
      content: <Input placeholder="文件夹名称" onChange={(e) => (value = e.target.value)} />,
      onOk: async () => {
        if (!value || !path) return
        const r = await request({ op: 'mkdir', path: join(path, value) })
        if (r.ok) void ls(path)
        else message.error(r.message ?? '新建失败')
      },
    })
  }

  const doRename = (entry: SftpEntry) => {
    let value = entry.name
    Modal.confirm({
      title: `重命名「${entry.name}」`,
      content: <Input defaultValue={entry.name} onChange={(e) => (value = e.target.value)} />,
      onOk: async () => {
        if (!value || !path || value === entry.name) return
        const r = await request({ op: 'rename', from: join(path, entry.name), to: join(path, value) })
        if (r.ok) void ls(path)
        else message.error(r.message ?? '重命名失败')
      },
    })
  }

  const doDelete = async (entry: SftpEntry) => {
    if (!path) return
    const r = await request({
      op: 'rm',
      path: join(path, entry.name),
      recursive: entry.type === 'dir',
    })
    if (r.ok) {
      message.success(`已删除 ${entry.name}`)
      void ls(path)
    } else {
      message.error(r.message ?? '删除失败')
    }
  }

  const uploadFiles = async (files: File[]) => {
    if (!path) return
    for (const file of files) {
      const key = crypto.randomUUID()
      setTransfers((prev) => [...prev, { key, name: file.name, dir: 'up', transferred: 0, total: file.size, done: false }])
      const open = await request({ op: 'upload-open', path: join(path, file.name) })
      if (!open.ok || !open.opId) {
        message.error(`${file.name} 上传失败：${open.message ?? '无法打开远程文件'}`)
        setTransfers((prev) => prev.filter((t) => t.key !== key))
        continue
      }
      const reader = file.stream().getReader()
      for (;;) {
        const { done, value } = await reader.read()
        if (done) break
        if (value) {
          await agentSocket.sendSftpChunk(sessionId, open.opId, value)
          setTransfers((prev) =>
            prev.map((t) =>
              t.key === key ? { ...t, transferred: t.transferred + value.byteLength } : t,
            ),
          )
        }
      }
      const close = await request({ op: 'upload-close', opId: open.opId })
      if (close.ok) message.success(`${file.name} 上传完成`)
      else message.error(`${file.name} 上传失败：${close.message}`)
      setTransfers((prev) => prev.filter((t) => t.key !== key))
    }
    void ls(path)
  }

  const download = async (entry: SftpEntry) => {
    if (!path) return
    const open = await request({ op: 'download-open', path: join(path, entry.name) })
    if (!open.ok || !open.opId) {
      message.error(open.message ?? '打开下载失败')
      return
    }
    const key = open.opId
    const total = open.sizeB ?? entry.sizeB
    setTransfers((prev) => [...prev, { key, name: entry.name, dir: 'down', transferred: 0, total, done: false }])

    const chunks: Uint8Array[] = []
    let received = 0
    const off = agentSocket.registerDownloadChunk(key, (chunk) => {
      chunks.push(chunk)
      received += chunk.byteLength
      setTransfers((prev) => prev.map((t) => (t.key === key ? { ...t, transferred: received } : t)))
    })
    await new Promise<void>((resolve) => {
      doneWaitersRef.current.set(key, resolve)
      // 大文件兜底超时（20 分钟）
      window.setTimeout(resolve, 20 * 60 * 1000)
    })
    off()
    doneWaitersRef.current.delete(key)
    agentSocket.sendControl({
      t: 'sftp',
      s: sessionId,
      m: { op: 'download-close', opId: key, rid: crypto.randomUUID() },
    })

    const blob = new Blob(chunks as unknown as BlobPart[])
    const a = document.createElement('a')
    a.href = URL.createObjectURL(blob)
    a.download = entry.name
    a.click()
    URL.revokeObjectURL(a.href)
    setTransfers((prev) => prev.filter((t) => t.key !== key))
    message.success(`已下载 ${entry.name}（${formatBytes(blob.size)}）`)
  }

  const crumbs = (path ?? '').split('/').filter(Boolean)

  const columns = [
    {
      title: '名称',
      dataIndex: 'name',
      render: (_: unknown, e: SftpEntry) => (
        <Space>
          {e.type === 'dir' ? <FolderOutlined /> : e.type === 'symlink' ? <LinkOutlined /> : <FileOutlined />}
          <a
            onClick={() => {
              if (e.type === 'dir' && path) void ls(join(path, e.name))
            }}
            style={e.type === 'dir' ? {} : { color: 'inherit', cursor: 'default' }}
          >
            {e.name}
          </a>
        </Space>
      ),
    },
    { title: '大小', dataIndex: 'sizeB', width: 110, render: (v: number, e: SftpEntry) => (e.type === 'dir' ? '—' : formatBytes(v)) },
    { title: '修改时间', dataIndex: 'mtimeMs', width: 180, render: (v: number) => formatTime(v) },
    {
      title: '操作',
      key: 'actions',
      width: 200,
      render: (_: unknown, e: SftpEntry) => (
        <Space size={4}>
          {e.type !== 'dir' && (
            <Button type="link" size="small" onClick={() => void download(e)}>
              下载
            </Button>
          )}
          <Button type="link" size="small" onClick={() => doRename(e)}>
            重命名
          </Button>
          <Popconfirm title={`删除「${e.name}」？${e.type === 'dir' ? '（含全部内容）' : ''}`} onConfirm={() => void doDelete(e)}>
            <Button type="link" size="small" danger>
              删除
            </Button>
          </Popconfirm>
        </Space>
      ),
    },
  ]

  return (
    <div
      style={{ height: '100%', overflow: 'auto', background: '#fff', padding: 12 }}
      onDragOver={(e) => e.preventDefault()}
      onDrop={(e) => {
        e.preventDefault()
        void uploadFiles([...e.dataTransfer.files])
      }}
    >
      <Space wrap style={{ marginBottom: 8 }}>
        <Tooltip title="返回上级">
          <Button icon={<ArrowUpOutlined />} disabled={!path || path === '/'} onClick={() => path && void ls(parent(path))} />
        </Tooltip>
        <Button icon={<ReloadOutlined />} onClick={() => path && void ls(path)} />
        <Button icon={<FolderAddOutlined />} onClick={doMkdir}>
          新建文件夹
        </Button>
        <Button type="primary" icon={<UploadOutlined />} onClick={() => fileInputRef.current?.click()}>
          上传
        </Button>
        <input
          ref={fileInputRef}
          type="file"
          multiple
          hidden
          onChange={(e) => {
            void uploadFiles([...(e.target.files ?? [])])
            e.target.value = ''
          }}
        />
        <Tag style={{ marginLeft: 'auto' }}>拖拽文件到此区域即可上传</Tag>
      </Space>

      {path && (
        <Breadcrumb
          style={{ marginBottom: 8 }}
          items={[
            { title: <a onClick={() => void ls('/')}>/</a> },
            ...crumbs.map((c, i) => ({
              title: <a onClick={() => void ls('/' + crumbs.slice(0, i + 1).join('/'))}>{c}</a>,
            })),
          ]}
        />
      )}

      {transfers.length > 0 && (
        <div style={{ marginBottom: 8, display: 'flex', flexDirection: 'column', gap: 4 }}>
          {transfers.map((t) => (
            <div key={t.key} style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
              <span style={{ width: 220, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                {t.dir === 'up' ? '↑' : '↓'} {t.name}
              </span>
              <Progress
                style={{ flex: 1, marginBottom: 0 }}
                size="small"
                percent={t.total > 0 ? Math.floor((t.transferred / t.total) * 100) : 0}
                format={() => `${formatBytes(t.transferred)} / ${formatBytes(t.total)}`}
              />
            </div>
          ))}
        </div>
      )}

      <Table
        size="small"
        rowKey="name"
        columns={columns}
        dataSource={entries}
        loading={loading}
        pagination={false}
        locale={{
          emptyText: loading ? (
            <Spin />
          ) : (
            <Empty description={error ?? (path ? '目录为空' : '连接主机中…')} />
          ),
        }}
      />
    </div>
  )
}
