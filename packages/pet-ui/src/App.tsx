import { useEffect, useRef, useState } from 'react'
import { Bubble } from './components/Bubble'
import { ChatPanel } from './components/ChatPanel'
import { DebugPanel } from './components/DebugPanel'
import { PetMenu } from './components/PetMenu'
import { PetSprite } from './components/PetSprite'
import { forwardDropUpload, isTauri, onFileDrop, openWorkbench, startWindowDrag } from './api/tauri'
import { usePet } from './pet/usePet'

export function App() {
  const { view, dispatch } = usePet()
  // 浏览器宿主：拖拽在窗口内移动精灵；Tauri 宿主：拖拽交给原生窗口移动
  const [pos, setPos] = useState<{ x: number; y: number } | null>(null)
  const anchorRef = useRef<HTMLDivElement>(null)
  const anchorStartRef = useRef<{ left: number; top: number } | null>(null)
  // 右键菜单（呼出工作台/主机会话）、AI 对话面板与文件拖放提示
  const [menuOpen, setMenuOpen] = useState(false)
  const [chatOpen, setChatOpen] = useState(false)
  const [dropHover, setDropHover] = useState(false)

  // OS 级文件拖放（仅 Tauri 宿主）：悬停亮提示，落下转发工作台开上传面板
  useEffect(() => {
    if (!isTauri) return
    let disposed = false
    let off: (() => void) | undefined
    void onFileDrop((s) => {
      if (disposed) return
      if (s.type === 'hover') setDropHover(true)
      else if (s.type === 'leave') setDropHover(false)
      else {
        setDropHover(false)
        dispatch({ type: 'poke' })
        void forwardDropUpload(s.paths)
      }
    }).then((u) => {
      if (disposed) u()
      else off = u
    })
    return () => {
      disposed = true
      off?.()
    }
  }, [dispatch])

  const onDragStart = () => {
    dispatch({ type: 'drag-start' })
    if (isTauri) {
      void startWindowDrag()
      return
    }
    const rect = anchorRef.current?.getBoundingClientRect()
    if (rect) anchorStartRef.current = { left: rect.left, top: rect.top }
  }

  const onDragMove = (dx: number, dy: number) => {
    if (isTauri) return // 原生拖窗，无需 DOM 移动
    const start = anchorStartRef.current
    if (!start) return
    setPos({ x: Math.max(0, start.left + dx), y: Math.max(0, start.top + dy) })
  }

  const debug = new URLSearchParams(location.search).has('debug')

  return (
    <div className="pet-stage">
      {debug && <DebugPanel view={view} />}
      {dropHover && <div className="drop-hint">松手，交给工作台上传</div>}
      {menuOpen && (
        <PetMenu onClose={() => setMenuOpen(false)} onOpenChat={() => setChatOpen(true)} />
      )}
      <ChatPanel open={chatOpen} onClose={() => setChatOpen(false)} />
      <div
        ref={anchorRef}
        className="pet-anchor"
        style={pos && !isTauri ? { left: pos.x, top: pos.y } : { right: 0, bottom: 0 }}
        onDoubleClick={() => void openWorkbench()}
        onContextMenu={(e) => {
          e.preventDefault()
          setMenuOpen(true)
        }}
      >
        <Bubble view={view} />
        <PetSprite
          state={view.state}
          onPointerEnter={() => dispatch({ type: 'pointer-enter' })}
          onPoke={() => dispatch({ type: 'poke' })}
          onDragStart={onDragStart}
          onDragEnd={() => dispatch({ type: 'drag-end' })}
          onDragMove={onDragMove}
        />
      </div>
    </div>
  )
}
