import { useEffect, useState } from 'react'
import type { HostRecord } from '@bgshell/protocol'
import { agentSocket } from '../api/ws'
import { hidePet, launchTool, quitApp } from '../api/tauri'

/** 菜单内主机最多显示的行数：再多会把窗口/屏幕撑爆，其余收进「更多主机」去 SSH 应用看 */
const MAX_MENU_HOSTS = 6

/** 宠物右键菜单（P6 基座）：AI 对话 / 三工具拉起 / 已存主机（点击拉起 SSH 应用）/ 设置 / 动作包 */
export function PetMenu({
  onClose,
  onOpenChat,
  onOpenPacks,
  onOpenSettings,
  onHostsChange,
}: {
  onClose: () => void
  onOpenChat: () => void
  /** 打开动作包管理面板（DIY 宠物动画） */
  onOpenPacks: () => void
  /** 打开设置弹窗（AI 配置等，经 7777 直达基座） */
  onOpenSettings: () => void
  /** 主机行数上报：App 据此扩宠物窗高度，菜单才不会被窗口顶部裁剪（已封顶，不会无限增长） */
  onHostsChange: (n: number) => void
}) {
  const [hosts, setHosts] = useState<HostRecord[]>()
  const shown = hosts?.slice(0, MAX_MENU_HOSTS) ?? []
  const restCount = hosts ? hosts.length - shown.length : 0

  useEffect(() => {
    // 上报实际渲染行数（超出上限时含「更多主机」一行）
    onHostsChange(hosts === undefined ? 0 : shown.length + (restCount > 0 ? 1 : 0))
  }, [hosts, shown.length, restCount, onHostsChange])

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
          title="与鲸鱼娘对话，让她帮你操作主机"
          onClick={() => {
            onOpenChat()
            onClose()
          }}
        >
          对话 · AI 助手
        </button>
        <div className="pet-menu-row">
          <button
            type="button"
            className="pet-menu-btn"
            onClick={() => {
              void launchTool('ssh')
              onClose()
            }}
          >
            SSH 终端
          </button>
          <button
            type="button"
            className="pet-menu-btn"
            onClick={() => {
              void launchTool('sql')
              onClose()
            }}
          >
            SQL
          </button>
          <button
            type="button"
            className="pet-menu-btn"
            onClick={() => {
              void launchTool('serial')
              onClose()
            }}
          >
            串口
          </button>
        </div>
        <button
          type="button"
          className="pet-menu-btn"
          title="AI 配置（API Key / Base URL / 模型）等"
          onClick={() => {
            onOpenSettings()
          }}
        >
          设置
        </button>
        <button
          type="button"
          className="pet-menu-btn"
          title="导入/启用/删除 DIY 动作包，换掉宠物动画"
          onClick={() => {
            onOpenPacks()
            onClose()
          }}
        >
          动作包管理
        </button>
        <div className="pet-menu-sep" />
        {hosts === undefined ? (
          <div className="pet-menu-empty">读取主机中…</div>
        ) : hosts.length === 0 ? (
          <div className="pet-menu-empty">暂无主机，请在 SSH 应用中添加</div>
        ) : (
          /* 最多渲染 MAX_MENU_HOSTS 行 + 「更多主机」；屏幕极矮时仍由 pet-menu-hosts 内部滚动兜底 */
          <div className="pet-menu-hosts">
            {shown.map((h) => (
              <div key={h.id} className="pet-menu-row">
                <button
                  type="button"
                  className="pet-menu-btn"
                  title={`${h.username}@${h.host}:${h.port}（在 SSH 应用中打开）`}
                  onClick={() => {
                    void launchTool('ssh')
                    onClose()
                  }}
                >
                  {h.name}
                </button>
              </div>
            ))}
            {restCount > 0 && (
              <button
                type="button"
                className="pet-menu-btn pet-menu-more"
                title="打开 SSH 应用查看全部主机"
                onClick={() => {
                  void launchTool('ssh')
                  onClose()
                }}
              >
                更多主机（{restCount} 台）…
              </button>
            )}
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
