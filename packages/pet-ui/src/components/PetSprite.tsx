import { useEffect, useRef, useState, type PointerEvent as ReactPointerEvent } from 'react'
import type { PetStateKey } from '../pet/machine'

/** 素材根：随 vite base（dev '/'、build '/pet/'）变化，保证双宿主路径一致 */
const SPRITE_BASE = `${import.meta.env.BASE_URL}pet/`

/** 帧表：与 public/pet/manifest.json 的 states.*.frames/fps/loop 保持同步 */
const FRAME_TABLE: Record<PetStateKey, { frames: number; fps: number; loop: boolean }> = {
  sleep: { frames: 8, fps: 4, loop: true },
  ready: { frames: 4, fps: 8, loop: false },
  idle_breath: { frames: 8, fps: 8, loop: true },
  idle_blink: { frames: 3, fps: 6, loop: false },
  wave: { frames: 6, fps: 8, loop: false },
  working: { frames: 8, fps: 8, loop: true },
  need_input: { frames: 4, fps: 6, loop: true },
  eating: { frames: 8, fps: 8, loop: true },
  angry: { frames: 6, fps: 10, loop: false },
  drag: { frames: 6, fps: 10, loop: true },
}

const ALL_STATES = Object.keys(FRAME_TABLE) as PetStateKey[]

const frameUrl = (state: PetStateKey, frame: number) =>
  `${SPRITE_BASE}${state}/${state}_${String(frame).padStart(2, '0')}.png`

interface PetSpriteProps {
  state: PetStateKey
  size?: number
  onPointerEnter?: () => void
  onPoke?: () => void
  onDragStart?: () => void
  onDragEnd?: () => void
  onDragMove?: (x: number, y: number) => void
}

/**
 * 精灵渲染器：按 manifest 帧表循环播放当前状态；非循环状态停在末帧，
 * 由状态机的瞬态计时负责切走。
 * 指针交互：按下并移动=拖拽（drag 状态）、原地点击=戳、悬停=招手。
 */
export function PetSprite({
  state,
  size = 256,
  onPointerEnter,
  onPoke,
  onDragStart,
  onDragEnd,
  onDragMove,
}: PetSpriteProps) {
  const spec = FRAME_TABLE[state]
  const [frame, setFrame] = useState(0)

  // 预载全部状态的所有帧，避免状态切换/翻帧闪白
  useEffect(() => {
    for (const s of ALL_STATES) {
      for (let i = 0; i < FRAME_TABLE[s].frames; i++) {
        const img = new Image()
        img.src = frameUrl(s, i)
      }
    }
  }, [])

  // 帧序列播放：状态切换重置到第 0 帧，按 fps 推进；非循环停在末帧
  useEffect(() => {
    setFrame(0)
    const timer = window.setInterval(() => {
      setFrame((f) => {
        const next = f + 1
        if (next < spec.frames) return next
        return spec.loop ? 0 : f
      })
    }, 1000 / spec.fps)
    return () => window.clearInterval(timer)
  }, [state, spec])

  const dragOriginRef = useRef<{ x: number; y: number; moved: boolean } | null>(null)

  const handlePointerDown = (e: ReactPointerEvent<HTMLDivElement>) => {
    ;(e.currentTarget as HTMLDivElement).setPointerCapture(e.pointerId)
    dragOriginRef.current = { x: e.clientX, y: e.clientY, moved: false }
  }

  const handlePointerMove = (e: ReactPointerEvent<HTMLDivElement>) => {
    const origin = dragOriginRef.current
    if (!origin) return
    const dx = e.clientX - origin.x
    const dy = e.clientY - origin.y
    if (!origin.moved && Math.hypot(dx, dy) < 4) return
    if (!origin.moved) {
      origin.moved = true
      onDragStart?.()
    }
    onDragMove?.(dx, dy)
  }

  const handlePointerUp = (e: ReactPointerEvent<HTMLDivElement>) => {
    const origin = dragOriginRef.current
    dragOriginRef.current = null
    ;(e.currentTarget as HTMLDivElement).releasePointerCapture?.(e.pointerId)
    if (!origin) return
    if (origin.moved) onDragEnd?.()
    else onPoke?.()
  }

  return (
    <div
      className="pet-sprite"
      style={{ width: size, height: size }}
      onPointerEnter={onPointerEnter}
      onPointerDown={handlePointerDown}
      onPointerMove={handlePointerMove}
      onPointerUp={handlePointerUp}
    >
      <img src={frameUrl(state, frame)} alt={state} draggable={false} />
    </div>
  )
}
