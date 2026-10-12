import { useEffect, useRef, type PointerEvent as ReactPointerEvent } from 'react'
import type { PetStateKey } from '../pet/machine'

/** 素材根：随 vite base（dev '/'、build '/pet/'）变化，保证双宿主路径一致 */
const SPRITE_BASE = `${import.meta.env.BASE_URL}pet/`

const ALL_STATES: PetStateKey[] = [
  'sleep',
  'ready',
  'idle_breath',
  'idle_blink',
  'wave',
  'working',
  'need_input',
  'eating',
  'angry',
  'drag',
]

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
 * 精灵渲染器：当前状态关键帧（后续补帧后改为按 manifest 帧序列播放）。
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
  // 预载全部状态图，避免状态切换闪白
  useEffect(() => {
    for (const s of ALL_STATES) {
      const img = new Image()
      img.src = `${SPRITE_BASE}${s}/${s}_00.png`
    }
  }, [])

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
      <img src={`${SPRITE_BASE}${state}/${state}_00.png`} alt={state} draggable={false} />
    </div>
  )
}
