import { useEffect, useState } from 'react'
import type { HostRecord } from '@bgshell/protocol'
import { agentSocket } from '../api/ws'
import { openHostSession, openWorkbench } from '../api/tauri'

/** 宠物右键菜单：呼出 AI 对话 / 工作台 / 已存主机（点击开 SSH 终端，"文件"开文件管理） */
export function PetMenu({ onClose, onOpenChat }: { onClose: () => void; onOpenChat: () => void }) {
  const [hosts, setHosts] = useState<HostRecord[]>()

  useEffect(() => {
    let alive = true
    let off: (() => void) | undefined
    agentSocket
      .connect()
      .then(() => {
        if (!alive) return
        off = agentSocket.onControl((frame) => {
          if (frame.t === 'hosts' && frame.m.ok && frame.m.hosts) setHosts(frame.m.hosts)
        })
        agentSocket.sendControl({ t: 'hosts', m: { op: 'list' } })
      })
      .catch(() => {
        if (alive) setHosts([])
      })
    return () => {
      alive = false
      off?.()
    }
  }, [])

  return (
    <>
      <div
        className="pet-menu-mask"
        onClick={onClose}
        onContextMenu={(e) => {
          e.preventDefault()
          onClose()
        }}
      />
      <div className="pet-menu">
        <button
          type="button"
          className="pet-menu-btn pet-menu-strong"
          onClick={() => {
            void openWorkbench()
            onClose()
          }}
        >
          打开工作台
        </button>
        <button
          type="button"
          className="pet-menu-btn"
          title="与鲸鱼娘对话，让她帮你操作主机"
          onClick={() => {
            onOpenChat()
            onClose()
          }}
        >
          对话 · AI 助手
        </button>
        <div className="pet-menu-sep" />
        {hosts === undefined ? (
          <div className="pet-menu-empty">读取主机中…</div>
        ) : hosts.length === 0 ? (
          <div className="pet-menu-empty">暂无主机，请在工作台添加</div>
        ) : (
          hosts.map((h) => (
            <div key={h.id} className="pet-menu-row">
              <button
                type="button"
                className="pet-menu-btn"
                title={`${h.username}@${h.host}:${h.port}`}
                onClick={() => {
                  void openHostSession(h.id, 'shell')
                  onClose()
                }}
              >
                {h.name}
              </button>
              <button
                type="button"
                className="pet-menu-btn pet-menu-files"
                title="打开文件管理"
                onClick={() => {
                  void openHostSession(h.id, 'sftp')
                  onClose()
                }}
              >
                文件
              </button>
            </div>
          ))
        )}
      </div>
    </>
  )
}
