import { useEffect, useRef, useState } from 'react'
import { Alert, Button, Input, List, Modal, Progress, Select, Typography, message } from 'antd'
import type { HostRecord } from '@bgshell/protocol'
import { agentSocket } from '../api/ws'
import { preflightBatch, sftpRequest, showTransferError } from '../api/transferService'
import { formatBytes } from '../utils/format'

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

  const start = async () => {
    if (!hostId) {
      message.warning('请先选择目标主机')
      return
    }
    const targets = files.filter((f) => !f.statError)
    if (targets.length === 0) {
      message.warning('没有可上传的文件')
      return
    }
    setPhase('uploading')
    cancelRef.current = false
    const sid = crypto.randomUUID()
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

      // 统一预检 + 确认（重名/权限/空间/盘格式；覆盖 / 重命名 / 取消）
      const decisions = await preflightBatch(
        sid,
        'upload',
        targets.map((f) => ({ path: f.path, name: f.name, sizeB: f.sizeB, target: join(dir, f.name) })),
      )
      if (decisions === null) {
        cancelRef.current = true // 用户取消预检：finally 按「取消」处理（回到 ready）
        return
      }

      // 原生传输：put-file / put-dir（Agent 直读本地文件，断点续传；目录递归上传）
      const opIds = new Map<string, string>() // path → opId
      for (let i = 0; i < targets.length; i++) {
        if (cancelRef.current) break
        const f = targets[i]
        const d = decisions[i]
        if (d.skip) {
          setTransfers((prev) => ({
            ...prev,
            [f.path]: { transferred: 0, total: f.sizeB, status: 'error', message: '已跳过' },
          }))
          continue
        }
        const r = await sftpRequest(
          sid,
          f.isDir
            ? { op: 'put-dir' as const, path: f.path, to: d.target, fresh: d.fresh }
            : { op: 'put-file' as const, path: f.path, to: d.target, fresh: d.fresh },
        )
        if (!r.ok || !r.opId) {
          showTransferError(`${f.name} 上传失败`, r.errorKind, r.message)
          setTransfers((prev) => ({
            ...prev,
            [f.path]: { transferred: 0, total: f.sizeB, status: 'error', message: r.message },
          }))
          continue
        }
        opIds.set(f.path, r.opId)
        setTransfers((prev) => ({
          ...prev,
          [f.path]: { transferred: 0, total: f.sizeB, status: 'uploading' },
        }))
      }

      await new Promise<void>((resolve) => {
        let pending = opIds.size
        if (pending === 0) return resolve()
        const off = agentSocket.onControl((frame) => {
          if (!('s' in frame) || frame.s !== sid) return
          if (frame.t === 'sftp-progress') {
            const { opId, transferred, total } = frame.m
            const target = [...opIds.entries()].find(([, id]) => id === opId)
            if (target) {
              setTransfers((prev) => ({
                ...prev,
                [target[0]]: { ...prev[target[0]], transferred, total },
              }))
            }
          }
          if (frame.t === 'sftp-result' && frame.m.op === 'transfer-done' && frame.m.opId) {
            const target = [...opIds.entries()].find(([, id]) => id === frame.m.opId)
            if (target) {
              const ok = frame.m.ok
              setTransfers((prev) => ({
                ...prev,
                [target[0]]: {
                  ...prev[target[0]],
                  transferred: ok ? frame.m.sizeB ?? prev[target[0]]?.total ?? 0 : prev[target[0]]?.transferred ?? 0,
                  status: ok ? 'done' : 'error',
                  message: ok ? undefined : frame.m.message,
                },
              }))
              opIds.delete(target[0])
              pending -= 1
              if (pending <= 0) {
                off()
                resolve()
              }
            }
          }
        })
      })
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
                  {f.isDir && <Typography.Text type="secondary">（目录，递归上传）</Typography.Text>}
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
