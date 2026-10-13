import { useEffect, useState } from 'react'
import type { HostRecord } from '@bgshell/protocol'
import { agentSocket } from '../api/ws'
import { openHostSession, openWorkbench, hidePet, quitApp } from '../api/tauri'

/** 宠物右键菜单：呼出 AI 对话 / 工作台 / 已存主机（点击开 SSH 终端，"文件"开文件管理） */
export function PetMenu({
  onClose,
  onOpenChat,
  onHostsChange,
}: {
  onClose: () => void
  onOpenChat: () => void
  /** 主机数量上报：App 据此扩宠物窗高度，菜单才不会被窗口顶部裁剪 */
  onHostsChange: (n: number) => void
}) {
  const [hosts, setHosts] = useState<HostRecord[]>()

  useEffect(() => {
    onHostsChange(hosts?.length ?? 0)
  }, [hosts, onHostsChange])

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
          /* 屏幕高度放不下完整列表时在菜单内滚动，固定项（工作台/对话/隐藏/退出）始终可见 */
          <div className="pet-menu-hosts">
            {hosts.map((h) => (
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
            ))}
          </div>
        )}
        <div className="pet-menu-sep" />
        <button
          type="button"
          className="pet-menu-btn"
          title="隐藏后从系统托盘「显示桌宠」唤回"
          onClick={() => {
            void hidePet()
            onClose()
          }}
        >
          隐藏桌宠
        </button>
        <button
          type="button"
          className="pet-menu-btn pet-menu-quit"
          title="退出整个应用（含托盘）"
          onClick={() => {
            void quitApp()
            onClose()
          }}
        >
          退出
        </button>
      </div>
    </>
  )
}
