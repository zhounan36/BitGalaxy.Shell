import { useEffect, useRef, useState } from 'react'
import { Alert, Button, Input, List, Modal, Progress, Select, Typography, message } from 'antd'
import type { HostRecord, SftpRequest, SftpResult } from '@bgshell/protocol'
import { agentSocket } from '../api/ws'
import { formatBytes } from '../utils/format'

const CHUNK = 256 * 1024
const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window

interface DropFile {
  path: string
  name: string
  sizeB: number
  isDir: boolean
  statError?: string
}

interface TransferState {
  transferred: number
  total: number
  status: 'uploading' | 'done' | 'error'
  message?: string
}

interface DropUploadModalProps {
  open: boolean
  paths: string[]
  hosts: HostRecord[]
  onClose: () => void
  /** 上传完成后打开该主机的文件管理页签 */
  onOpenSftp: (hostId: string) => void
}

/**
 * 拖拽上传面板：文件拖到桌宠后由宠物窗转发打开。
 * 选目标主机与远程目录 → 打开独立 SFTP 会话 → Tauri 分块读本地文件 → 0x10 数据帧上传。
 * 首次连接的 hostkey 确认由 App 全局 hostkey-unknown 弹窗兜底。
 */
export function DropUploadModal({ open, paths, hosts, onClose, onOpenSftp }: DropUploadModalProps) {
  const [files, setFiles] = useState<DropFile[]>([])
  const [hostId, setHostId] = useState<string>()
  const [dir, setDir] = useState('/')
  const [phase, setPhase] = useState<'ready' | 'uploading' | 'done'>('ready')
  const [transfers, setTransfers] = useState<Record<string, TransferState>>({})
  const cancelRef = useRef(false)

  // 打开时 stat 本地文件（大小/是否目录）
  useEffect(() => {
    if (!open) return
    setPhase('ready')
    setTransfers({})
    setDir('/')
    setHostId(hosts[0]?.id)
    cancelRef.current = false
    if (!isTauri) {
      setFiles([])
      return
    }
    let alive = true
    void (async () => {
      const { invoke } = await import('@tauri-apps/api/core')
      const stats = await Promise.all(
        paths.map(async (path): Promise<DropFile> => {
          try {
            const s = await invoke<{ name: string; size: number; is_dir: boolean }>('stat_file', { path })
            return { path, name: s.name, sizeB: s.size, isDir: s.is_dir }
          } catch (err) {
            return {
              path,
              name: path.split(/[\\/]/).pop() ?? path,
              sizeB: 0,
              isDir: false,
              statError: String(err),
            }
          }
        }),
      )
      if (alive) setFiles(stats)
    })()
    return () => {
      alive = false
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, paths.join('|')])

  const join = (d: string, name: string) => (d.endsWith('/') ? `${d}${name}` : `${d}/${name}`)

  /** SFTP 请求/响应关联（同 SftpPane 的 rid 机制，独立会话） */
  const request = async (sid: string, op: SftpRequest): Promise<SftpResult> => {
    const rid = crypto.randomUUID()
    return await new Promise<SftpResult>((resolve) => {
      const timer = window.setTimeout(() => {
        off()
        resolve({ op: op.op, rid, ok: false, message: '请求超时' })
      }, 20000)
      const off = agentSocket.onControl((frame) => {
        if (frame.t === 'sftp-result' && frame.s === sid && frame.m.rid === rid) {
          clearTimeout(timer)
          off()
          resolve(frame.m)
        }
      })
      agentSocket.sendControl({ t: 'sftp', s: sid, m: { ...op, rid } })
    })
  }

  const start = async () => {
    if (!hostId) {
      message.warning('请先选择目标主机')
      return
    }
    const targets = files.filter((f) => !f.isDir && !f.statError)
    if (targets.length === 0) {
      message.warning('没有可上传的文件')
      return
    }
    setPhase('uploading')
    cancelRef.current = false
    const sid = crypto.randomUUID()
    const { invoke } = await import('@tauri-apps/api/core')
    try {
      await new Promise<void>((resolve, reject) => {
        const timer = window.setTimeout(() => {
          off()
          reject(new Error('打开 SFTP 会话超时'))
        }, 20000)
        const off = agentSocket.onControl((frame) => {
          if (frame.t === 'session-opened' && frame.s === sid) {
            clearTimeout(timer)
            off()
            resolve()
          } else if (frame.t === 'error' && frame.s === sid) {
            clearTimeout(timer)
            off()
            reject(new Error(frame.m.message))
          }
        })
        agentSocket.sendControl({ t: 'open', s: sid, m: { hostId, kind: 'sftp' } })
      })

      for (const f of targets) {
        if (cancelRef.current) break
        setTransfers((prev) => ({
          ...prev,
          [f.path]: { transferred: 0, total: f.sizeB, status: 'uploading' },
        }))
        const openRes = await request(sid, { op: 'upload-open', path: join(dir, f.name) })
        if (!openRes.ok || !openRes.opId) {
          setTransfers((prev) => ({
            ...prev,
            [f.path]: { transferred: 0, total: f.sizeB, status: 'error', message: openRes.message },
          }))
          continue
        }
        for (let offset = 0; offset < f.sizeB; offset += CHUNK) {
          if (cancelRef.current) break
          const buf = await invoke<ArrayBuffer>('read_file_chunk', { path: f.path, offset, len: CHUNK })
          await agentSocket.sendSftpChunk(sid, openRes.opId, new Uint8Array(buf))
          setTransfers((prev) => ({
            ...prev,
            [f.path]: {
              ...prev[f.path],
              transferred: Math.min(offset + buf.byteLength, f.sizeB),
              total: f.sizeB,
              status: 'uploading',
            },
          }))
        }
        const closeRes = await request(sid, { op: 'upload-close', opId: openRes.opId })
        setTransfers((prev) => ({
          ...prev,
          [f.path]: {
            ...prev[f.path],
            transferred: f.sizeB,
            status: closeRes.ok ? 'done' : 'error',
            message: closeRes.ok ? undefined : closeRes.message,
          },
        }))
      }
    } catch (err) {
      message.error(`上传失败：${err instanceof Error ? err.message : String(err)}`)
    } finally {
      agentSocket.sendControl({ t: 'close', s: sid, m: { reason: 'upload-done' } })
      setPhase(cancelRef.current ? 'ready' : 'done')
    }
  }

  const uploading = phase === 'uploading'
  const doneCount = files.filter((f) => transfers[f.path]?.status === 'done').length

  return (
    <Modal
      title={`上传拖入的 ${files.length} 个文件`}
      open={open}
      width={540}
      maskClosable={!uploading}
      onCancel={() => {
        if (uploading) cancelRef.current = true
        onClose()
      }}
      footer={
        phase === 'done' ? [
          <Button key="close" onClick={onClose}>
            关闭
          </Button>,
          <Button
            key="open"
            type="primary"
            disabled={!hostId}
            onClick={() => {
              if (hostId) onOpenSftp(hostId)
              onClose()
            }}
          >
            打开文件管理
          </Button>,
        ] : [
          <Button key="cancel" onClick={onClose}>
            取消
          </Button>,
          <Button key="start" type="primary" loading={uploading} onClick={() => void start()}>
            {uploading ? '上传中…' : '开始上传'}
          </Button>,
        ]
      }
    >
      {!isTauri && <Alert type="warning" showIcon message="拖拽上传需要在桌面壳（BitGalaxy Shell 桌面版）中使用" />}
      <Select
        style={{ width: '100%', marginTop: 12 }}
        placeholder="选择目标主机"
        value={hostId}
        onChange={setHostId}
        disabled={uploading}
        options={hosts.map((h) => ({ value: h.id, label: `${h.name}（${h.username}@${h.host}:${h.port}）` }))}
      />
      <Input
        style={{ marginTop: 8 }}
        addonBefore="目标目录"
        value={dir}
        onChange={(e) => setDir(e.target.value)}
        placeholder="/"
        disabled={uploading}
      />
      <List
        size="small"
        style={{ marginTop: 8, maxHeight: 260, overflow: 'auto' }}
        dataSource={files}
        locale={{ emptyText: isTauri ? '读取文件中…' : '—' }}
        renderItem={(f) => {
          const t = transfers[f.path]
          return (
            <List.Item style={{ display: 'block', padding: '6px 0' }}>
              <div style={{ display: 'flex', justifyContent: 'space-between', gap: 8 }}>
                <Typography.Text ellipsis style={{ maxWidth: 380 }}>
                  {f.name}
                  {f.isDir && <Typography.Text type="secondary">（目录，跳过）</Typography.Text>}
                </Typography.Text>
                <Typography.Text type="secondary">{f.isDir ? '—' : formatBytes(f.sizeB)}</Typography.Text>
              </div>
              {f.statError && <Typography.Text type="danger">{f.statError}</Typography.Text>}
              {t && (
                <Progress
                  size="small"
                  status={t.status === 'error' ? 'exception' : t.status === 'done' ? 'success' : 'active'}
                  percent={t.total > 0 ? Math.floor((t.transferred / t.total) * 100) : t.status === 'done' ? 100 : 0}
                  format={() => `${formatBytes(t.transferred)} / ${formatBytes(t.total)}`}
                />
              )}
              {t?.message && <Typography.Text type="danger">{t.message}</Typography.Text>}
            </List.Item>
          )
        }}
      />
      {phase === 'done' && (
        <Typography.Text type="success">
          完成：{doneCount}/{files.length} 个文件已上传到 {dir}
        </Typography.Text>
      )}
    </Modal>
  )
}
