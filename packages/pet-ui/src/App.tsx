import { useEffect, useRef, useState, type CSSProperties, type WheelEvent as ReactWheelEvent } from 'react'
import { Bubble } from './components/Bubble'
import { ChatPanel } from './components/ChatPanel'
import { DebugPanel } from './components/DebugPanel'
import { PetMenu } from './components/PetMenu'
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
  // 精灵尺寸（滚轮缩放，localStorage 持久化；Tauri 宿主联动窗口尺寸，浏览器宿主仅缩 DOM）
  const [petSize, setPetSize] = useState(loadPetSize)

  // 尺寸 / 对话面板开合变化 → 宠物窗随之扩/缩（右下角锚定，宠物本体屏幕位置不变；浏览器宿主 no-op）。
  // 串行执行：滚轮连滚时逐次应用，避免并发 IPC 读到过期的窗口位置
  const applyChainRef = useRef(Promise.resolve())
  useEffect(() => {
    applyChainRef.current = applyChainRef.current
      .then(() => applyPetWindowSize(petSize, chatOpen))
      // TODO(debug): 排查缩放链路（修完删除）
      .catch((err) => {
        if (isTauri) {
          void import('@tauri-apps/api/core').then(({ invoke }) =>
            invoke('dbg_evt', { msg: `applyPetWindowSize failed: ${String(err)}` }),
          )
        }
        console.error('applyPetWindowSize failed', err)
      })
  }, [petSize, chatOpen])

  // TODO(debug): 临时事件探针——定位 dblclick 在 WebView2 中是否产生（修完删除）
  useEffect(() => {
    if (!isTauri) return
    void (async () => {
      const { invoke } = await import('@tauri-apps/api/core')
      const log = (m: string) => void invoke('dbg_evt', { msg: m }).catch((e) => console.error('dbg_evt failed', e))
      log(`pet page mounted: ${location.href}`)
      // TODO(debug): 布局实测探针——把关键 rect 写进窗口标题（修完删除）
      window.setTimeout(() => {
        const img = document.querySelector<HTMLImageElement>('.pet-sprite img')
        const sprite = document.querySelector<HTMLElement>('.pet-sprite')
        const anchor = document.querySelector<HTMLElement>('.pet-anchor')
        if (!img || !sprite || !anchor) {
          document.title = 'probe: missing els'
          return
        }
        const rect = (el: Element) => {
          const b = el.getBoundingClientRect()
          return [b.x, b.y, b.width, b.height].map((v) => Math.round(v * 10) / 10)
        }
        const cs = getComputedStyle(img)
        const payload = JSON.stringify({
          iw: innerWidth,
          ih: innerHeight,
          dpr: devicePixelRatio,
          a: rect(anchor),
          s: rect(sprite),
          i: rect(img),
          nat: [img.naturalWidth, img.naturalHeight],
          fit: cs.objectFit,
          box: [cs.width, cs.height],
        })
        document.title = payload
        log(`layout ${payload}`)
      }, 500)
      for (const t of ['pointerdown', 'pointerup', 'click', 'dblclick'] as const) {
        document.addEventListener(
          t,
          (e) => {
            const me = e as MouseEvent
            const tgt = (e.target as HTMLElement | null)
            log(`${t} detail=${me.detail} tgt=${tgt?.className?.toString().slice(0, 24) ?? tgt?.tagName ?? '?'}`)
          },
          { capture: true, passive: true },
        )
      }
    })()
  }, [])

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
    // TODO(debug): 排查缩放链路（修完删除）
    if (isTauri) {
      void import('@tauri-apps/api/core').then(({ invoke }) =>
        invoke('dbg_evt', { msg: `wheel deltaY=${e.deltaY} ${petSize}->${next}` }),
      )
    }
    savePetSize(next)
    setPetSize(next)
  }

  const debug = new URLSearchParams(location.search).has('debug')

  return (
    <div className="pet-stage" style={{ '--pet-size': `${petSize}px` } as CSSProperties}>
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
