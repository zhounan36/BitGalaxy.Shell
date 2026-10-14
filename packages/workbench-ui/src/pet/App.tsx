import { useEffect, useRef, useState, type CSSProperties, type WheelEvent as ReactWheelEvent } from 'react'
import { Bubble } from './components/Bubble'
import { ChatPanel } from '../components/ChatPanel'
import { DebugPanel } from './components/DebugPanel'
import { PetMenu } from './components/PetMenu'
import { PetPackPanel } from './components/PetPackPanel'
import { PetSprite } from './components/PetSprite'
import {
  applyPetWindowSize,
  clampPetSize,
  forwardDropUpload,
  isTauri,
  loadPetSize,
  onFileDrop,
  openWorkbench,
  PET_SIZE_STEP,
  savePetSize,
  startWindowDrag,
} from './api/tauri'
import { onAiAsk, type AiAskPayload } from '../api/aiBridge'
import { usePet } from './pet/usePet'
import { usePetPack } from './pack/usePetPack'

export function App() {
  // 动作包：动画与动作解绑——runtime 决定「哪个动作播哪段动画」（用户包覆盖，缺省回落内置）
  const { runtime, activePack, packs, refresh: refreshPacks } = usePetPack()
  // behavior=用户自定义动作图（pack.json 的 behavior 节）；无则内置默认行为
  const { view, dispatch } = usePet(activePack?.behavior ?? null)
  // 浏览器宿主：拖拽在窗口内移动精灵；Tauri 宿主：拖拽交给原生窗口移动
  const [pos, setPos] = useState<{ x: number; y: number } | null>(null)
  const anchorRef = useRef<HTMLDivElement>(null)
  const anchorStartRef = useRef<{ left: number; top: number } | null>(null)
  // 右键菜单（呼出工作台/主机会话）、AI 对话面板与文件拖放提示
  const [menuOpen, setMenuOpen] = useState(false)
  // 右键菜单内的主机数量：PetMenu 上报，用于扩宠物窗高度（多主机时菜单不被窗口顶部裁剪）
  const [menuHosts, setMenuHosts] = useState(0)
  const [chatOpen, setChatOpen] = useState(false)
  // 动作包管理面板开合（与 chat 面板互斥，同时只开一个）
  const [packOpen, setPackOpen] = useState(false)
  const [dropHover, setDropHover] = useState(false)
  // 终端「问AI」投递（工作台主窗 → ai://ask 事件）：开聊天面板并交给 ChatPanel 组合发送
  const [askReq, setAskReq] = useState<AiAskPayload | null>(null)
  // 精灵尺寸（滚轮缩放，localStorage 持久化；Tauri 宿主联动窗口尺寸，浏览器宿主仅缩 DOM）
  const [petSize, setPetSize] = useState(loadPetSize)

  // 尺寸 / 对话面板开合 / 右键菜单变化 → 宠物窗随之扩/缩（右下角锚定，宠物本体屏幕位置不变；浏览器宿主 no-op）。
  // 串行执行：滚轮连滚时逐次应用，避免并发 IPC 读到过期的窗口位置
  const applyChainRef = useRef(Promise.resolve())
  useEffect(() => {
    applyChainRef.current = applyChainRef.current
      .then(() => applyPetWindowSize(petSize, chatOpen, menuOpen ? menuHosts : 0, packOpen))
      .catch((err) => console.error('applyPetWindowSize failed', err))
  }, [petSize, chatOpen, menuOpen, menuHosts, packOpen])

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

  // 终端「问AI」：关动作包面板、开聊天面板，payload 交给 ChatPanel（按 uid 去重后组合发送）
  useEffect(() => {
    let disposed = false
    let off: (() => void) | undefined
    void onAiAsk((p) => {
      if (disposed || !p?.uid) return
      setPackOpen(false)
      setChatOpen(true)
      setAskReq(p)
    }).then((u) => {
      if (disposed) u()
      else off = u
    })
    return () => {
      disposed = true
      off?.()
    }
  }, [])

  // ── 双击检测（手动配对，不依赖 DOM dblclick）────────────────────────────
  // 背景：Tauri 下拖窗 startDragging 是模态调用，第一次按下若抖动 ≥4px 触发拖拽，
  // pointerup 会被原生吞掉 → 第一次点击永远无法完成 → DOM dblclick 凑不齐两次 click。
  // 因此改为：原地点按（poke）与「短促微拖」（<250ms 即松手的拖拽）都记一次 tap，
  // 450ms 内出现两次 tap 即判定为双击 → 打开工作台。
  const DOUBLE_TAP_MS = 450
  const QUICK_DRAG_MS = 250
  const lastTapAtRef = useRef(0)
  const dragStartAtRef = useRef(0)

  const registerTap = () => {
    const now = performance.now()
    const last = lastTapAtRef.current
    lastTapAtRef.current = now
    if (last && now - last < DOUBLE_TAP_MS) {
      lastTapAtRef.current = 0
      if (isTauri) void import('@tauri-apps/api/core').then(({ invoke }) => invoke('dbg_evt', { msg: '双击判定成立 → openWorkbench' }))
      void openWorkbench()
    }
  }

  /** 拖拽结束：补发 drag-end；短促微拖视为一次 tap 参与双击配对 */
  const settleDrag = () => {
    dispatch({ type: 'drag-end' })
    if (performance.now() - dragStartAtRef.current < QUICK_DRAG_MS) registerTap()
  }

  const onDragStart = () => {
    dragStartAtRef.current = performance.now()
    dispatch({ type: 'drag-start' })
    if (isTauri) {
      // 原生拖窗接管指针后 webview 收不到 pointerup：
      // startDragging 为模态调用，settle（松手/失败）即补发 drag-end
      void startWindowDrag().finally(() => settleDrag())
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

  // 滚轮缩放桌宠：向上放大 / 向下缩小（步进 16，160..480，持久化 localStorage）。
  // 绑定在 pet-anchor 上，对话面板 / 菜单内的滚动不受影响
  const onWheelZoom = (e: ReactWheelEvent) => {
    if (!e.deltaY) return
    const next = clampPetSize(petSize + (e.deltaY < 0 ? PET_SIZE_STEP : -PET_SIZE_STEP))
    savePetSize(next)
    setPetSize(next)
  }

  const debug = new URLSearchParams(location.search).has('debug')

  return (
    <div className="pet-stage" style={{ '--pet-size': `${petSize}px` } as CSSProperties}>
      {debug && <DebugPanel view={view} />}
      {dropHover && <div className="drop-hint">松手，交给工作台上传</div>}
      {menuOpen && (
        <PetMenu
          onClose={() => setMenuOpen(false)}
          onOpenChat={() => {
            setPackOpen(false)
            setChatOpen(true)
          }}
          onOpenPacks={() => {
            setChatOpen(false)
            setPackOpen(true)
          }}
          onHostsChange={setMenuHosts}
        />
      )}
      <ChatPanel open={chatOpen} onClose={() => setChatOpen(false)} incomingAsk={askReq} />
      {packOpen && (
        <PetPackPanel
          packs={packs}
          activePackId={activePack?.id ?? null}
          onChanged={() => void refreshPacks()}
          onClose={() => setPackOpen(false)}
        />
      )}
      <div
        ref={anchorRef}
        className="pet-anchor"
        style={pos && !isTauri ? { left: pos.x, top: pos.y } : { right: 0, bottom: 0 }}
        onWheel={onWheelZoom}
        onContextMenu={(e) => {
          e.preventDefault()
          setMenuOpen(true)
        }}
      >
        {/* 菜单打开时隐藏气泡：菜单悬浮在气泡区上方，避免台词从菜单边缘露出 */}
        {!menuOpen && <Bubble view={view} />}
        <PetSprite
          state={view.state}
          runtime={runtime}
          size={petSize}
          onPointerEnter={() => dispatch({ type: 'pointer-enter' })}
          onPoke={() => {
            registerTap()
            dispatch({ type: 'poke' })
          }}
          onDragStart={onDragStart}
          onDragEnd={settleDrag}
          onDragMove={onDragMove}
        />
      </div>
    </div>
  )
}
