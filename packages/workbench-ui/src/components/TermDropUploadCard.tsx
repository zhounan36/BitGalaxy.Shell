import { Progress, Typography } from 'antd'
import { CloseOutlined, UploadOutlined } from '@ant-design/icons'
import type { UploadItemState } from '../api/termUpload'
import { formatBytes } from '../utils/format'

export interface TermDropUploadView {
  sessionId: string
  hostName: string
  /** 解析出的远程目录（解析中为空串） */
  dir: string
  /** dir 来源：osc7 = shell 集成实时上报；login = pwd 兜底（登录目录） */
  dirSource?: 'osc7' | 'login'
  /** 拖拽落点终端正运行全屏程序（top/vim 等交替屏幕） */
  altScreen?: boolean
  /** 目录解析/会话级失败信息（有值时不渲染文件列表） */
  error?: string
  items: UploadItemState[]
  finished: boolean
}

/** 终端拖拽上传浮层：右下角显示目标目录与各文件进度，完成后可手动/自动关闭 */
export function TermDropUploadCard({ view, onClose }: { view: TermDropUploadView; onClose: () => void }) {
  const done = view.items.filter((i) => i.status === 'done').length
  const failed = view.items.filter((i) => i.status === 'error').length
  return (
    <div className="term-drop-card">
      <div className="term-drop-card-head">
        <UploadOutlined style={{ color: 'var(--accent)' }} />
        <span className="term-drop-card-title">
          上传到 {view.hostName}
          {view.dir ? `：${view.dir}` : ''}
        </span>
        {view.dirSource === 'login' && view.dir && (
          <span className="term-drop-card-note" title="远端 shell 未上报当前目录（OSC 7），已使用登录目录">
            登录目录
          </span>
        )}
        <button type="button" className="term-drop-card-close" onClick={onClose} aria-label="关闭">
          <CloseOutlined />
        </button>
      </div>
      {view.error ? (
        <Typography.Text type="danger" style={{ fontSize: 12 }}>
          {view.error}
        </Typography.Text>
      ) : view.dir ? (
        <>
          {view.altScreen && (
            <div className="term-drop-card-summary">
              全屏程序运行中，将上传到进入前的目录（退出后恢复跟随）
            </div>
          )}
          {view.items.map((it) => (
            <div key={it.name} className="term-drop-card-item">
              <div className="term-drop-card-item-name" title={it.name}>
                {it.name}
              </div>
              <Progress
                size="small"
                style={{ marginBottom: 0 }}
                status={it.status === 'error' ? 'exception' : it.status === 'done' ? 'success' : 'active'}
                percent={it.total > 0 ? Math.floor((it.transferred / it.total) * 100) : 0}
                format={() =>
                  it.status === 'error'
                    ? '失败'
                    : `${formatBytes(it.transferred)} / ${formatBytes(it.total)}`
                }
              />
              {it.message && (
                <Typography.Text type="danger" style={{ fontSize: 11 }}>
                  {it.message}
                </Typography.Text>
              )}
            </div>
          ))}
          {view.finished && (
            <div className="term-drop-card-summary">
              {failed > 0 ? (
                <Typography.Text type="danger">
                  完成 {done}/{view.items.length}，失败 {failed}
                </Typography.Text>
              ) : (
                <Typography.Text type="success">
                  已上传 {done}/{view.items.length} 个文件
                </Typography.Text>
              )}
            </div>
          )}
        </>
      ) : (
        <div className="term-drop-card-summary">正在解析远程目录…</div>
      )}
    </div>
  )
}
