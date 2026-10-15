import { useEffect, useRef, useState, type CSSProperties, type WheelEvent as ReactWheelEvent } from 'react'
import { Bubble } from './components/Bubble'
import { ChatPanel, CHAT_PANEL_DEFAULT_SIZE, loadChatSize, type ChatPanelSize } from '../components/ChatPanel'
import { DebugPanel } from './components/DebugPanel'
import { PetMenu } from './components/PetMenu'
import { PetPackPanel } from './components/PetPackPanel'
import { PetSprite } from './components/PetSprite'
import { SettingsModal } from '../components/SettingsModal'
import {
  applyPetWindowSize,
  clampPetSize,
  isTauri,
  loadPetSize,
  PET_SIZE_STEP,
  savePetSize,
  startWindowDrag,
} from './api/tauri'
import { usePet } from './pet/usePet'
import { usePetPack } from './pack/usePetPack'

/** 对话面板尺寸上限：屏幕工作区减去宠物区与边距（宠物窗随面板联动放大，故以屏幕为界而非本窗） */
function chatSizeMax(petSize: number): ChatPanelSize {
  const sw = window.screen?.availWidth ?? 960
  const sh = window.screen?.availHeight ?? 840
  // 面板 right 偏移 petSize-46、左侧至少留 26px；上下各留 20px
  return { w: sw - petSize + 20, h: sh - 40 }
}

export function App() {
  // 动作包：动画与动作解绑——runtime 决定「哪个动作播哪段动画」（用户包覆盖，缺省回落内置）
  const { runtime, activePack, packs, refresh: refreshPacks } = usePetPack()
  // behavior=用户自定义动作图（pack.json 的 behavior 节）；无则内置默认行为
  const { view, dispatch } = usePet(activePack?.behavior ?? null)
  // 浏览器宿主：拖拽在窗口内移动精灵；Tauri 宿主：拖拽交给原生窗口移动
  const [pos, setPos] = useState<{ x: number; y: number } | null>(null)
  const anchorRef = useRef<HTMLDivElement>(null)
  const anchorStartRef = useRef<{ left: number; top: number } | null>(null)
  // 右键菜单（呼出工具/主机列表）、AI 对话面板与设置弹窗
  const [menuOpen, setMenuOpen] = useState(false)
  // 右键菜单内的主机数量：PetMenu 上报，用于扩宠物窗高度（多主机时菜单不被窗口顶部裁剪）
  const [menuHosts, setMenuHosts] = useState(0)
  const [chatOpen, setChatOpen] = useState(false)
  // 动作包管理面板开合（与 chat 面板互斥，同时只开一个）
  const [packOpen, setPackOpen] = useState(false)
  // 设置弹窗（AI 配置等，经 7777 settings 帧直达基座）
  const [settingsOpen, setSettingsOpen] = useState(false)
  // 精灵尺寸（滚轮缩放，localStorage 持久化；Tauri 宿主联动窗口尺寸，浏览器宿主仅缩 DOM）
  const [petSize, setPetSize] = useState(loadPetSize)
  // 对话面板尺寸（ChatPanel 拖拽调整后上报；窗口尺寸随之联动，宠物本体位置不动）。
  // 读取时按屏幕上限夹取（不能按本窗——宠物窗此刻还是 rest 尺寸，会把存好的大面板夹小）
  const [chatSize, setChatSize] = useState<ChatPanelSize>(
    () => loadChatSize(chatSizeMax(loadPetSize())) ?? CHAT_PANEL_DEFAULT_SIZE,
  )

  // 尺寸 / 对话面板开合与大小 / 右键菜单变化 → 宠物窗随之扩/缩（右下角锚定，宠物本体屏幕位置不变；浏览器宿主 no-op）。
  // 串行执行防并发 IPC 读到过期窗口位置；目标过期则跳过（拖拽面板时 40ms 一次上报只应用最新，避免松手后追赶旧尺寸）
  const applyChainRef = useRef(Promise.resolve())
  const applySeqRef = useRef(0)
  useEffect(() => {
    const seq = ++applySeqRef.current
    applyChainRef.current = applyChainRef.current
      .then(() => {
        if (seq !== applySeqRef.current) return
        return applyPetWindowSize(petSize, chatOpen, menuOpen ? menuHosts : 0, packOpen, chatSize)
      })
      .catch((err) => console.error('applyPetWindowSize failed', err))
  }, [petSize, chatOpen, menuOpen, menuHosts, packOpen, chatSize])

  // ── 双击检测（手动配对，不依赖 DOM dblclick）────────────────────────────
  // 背景：Tauri 下拖窗 startDragging 是模态调用，第一次按下若抖动 ≥4px 触发拖拽，
  // pointerup 会被原生吞掉 → 第一次点击永远无法完成 → DOM dblclick 凑不齐两次 click。
  // 因此改为：原地点按（poke）与「短促微拖」（<250ms 即松手的拖拽）都记一次 tap，
  // 450ms 内出现两次 tap 即判定为双击 → 打开 AI 对话面板（基座 P6：桌宠即 AI 入口）。
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
      setPackOpen(false)
      setChatOpen(true)
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
          onOpenSettings={() => {
            setMenuOpen(false)
            setSettingsOpen(true)
          }}
          onHostsChange={setMenuHosts}
        />
      )}
      <ChatPanel
        open={chatOpen}
        onClose={() => setChatOpen(false)}
        onPanelResize={setChatSize}
        sizeMax={chatSizeMax(petSize)}
      />
      {packOpen && (
        <PetPackPanel
          packs={packs}
          activePackId={activePack?.id ?? null}
          onChanged={() => void refreshPacks()}
          onClose={() => setPackOpen(false)}
        />
      )}
      <SettingsModal open={settingsOpen} onClose={() => setSettingsOpen(false)} variant="pet" />
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
