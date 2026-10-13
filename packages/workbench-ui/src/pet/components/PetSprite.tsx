import { useEffect, useRef, useState, type PointerEvent as ReactPointerEvent } from 'react'
import type { PetStateKey } from '../pet/machine'

/** 素材根：base 恒为 '/'（MPA 合并后 dev/生产一致），public/pet → /pet/** */
const SPRITE_BASE = `${import.meta.env.BASE_URL}pet/`

/** 帧表：与 public/pet/manifest.json 的 states.*.frames/fps/loop 保持同步 */
const FRAME_TABLE: Record<PetStateKey, { frames: number; fps: number; loop: boolean }> = {
  sleep: { frames: 8, fps: 3, loop: true },
  ready: { frames: 4, fps: 6, loop: false },
  idle_breath: { frames: 8, fps: 6, loop: true },
  idle_blink: { frames: 3, fps: 5, loop: false },
  wave: { frames: 6, fps: 6, loop: false },
  working: { frames: 8, fps: 6, loop: true },
  need_input: { frames: 4, fps: 5, loop: true },
  eating: { frames: 8, fps: 6, loop: true },
  angry: { frames: 6, fps: 8, loop: false },
  drag: { frames: 8, fps: 8, loop: true },
}

const ALL_STATES = Object.keys(FRAME_TABLE) as PetStateKey[]

/** 呼吸微动：周期 3.6s（贴近真人静息呼吸），scaleY ±1.5%，以脚底为轴（Live2D 式程序化微动） */
const BREATH_MS = 3600
const BREATH_AMP = 0.015

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

/** 图层栈节点：末位是当前层，前面的层冻结末帧正在淡出（状态切换交叉淡化） */
interface PetLayer {
  id: number
  state: PetStateKey
  frame: number
  fading: boolean
}

/**
 * 精灵渲染器 v2：
 * - 单 rAF 时钟按 dt 累积推进帧序列（替代每状态 setInterval，切换无计时错位）；
 * - 状态切换时旧层冻结末帧、CSS 140ms 淡出，新层在下层直接起播（交叉淡化）；
 * - idle_breath 时叠加程序化呼吸微动（scaleY 正弦，Live2D 式），直接写 DOM。
 * 非循环状态停在末帧，动作总时长由状态机 ACTION_MS 决定。
 * 指针交互：按下并移动=拖拽、原地点击=戳、悬停=招手。
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
  const [layers, setLayers] = useState<PetLayer[]>([{ id: 0, state, frame: 0, fading: false }])
  const nextIdRef = useRef(1)
  const topImgRef = useRef<HTMLImageElement | null>(null)
  const accRef = useRef(0)

  // 预载全部状态的所有帧，避免状态切换/翻帧闪白
  useEffect(() => {
    for (const s of ALL_STATES) {
      for (let i = 0; i < FRAME_TABLE[s].frames; i++) {
        const img = new Image()
        img.src = frameUrl(s, i)
      }
    }
  }, [])

  // 状态切换：旧层冻结末帧淡出，新层从第 0 帧起播；帧相位归零
  useEffect(() => {
    setLayers((ls) => {
      const top = ls[ls.length - 1]
      if (top.state === state) return ls
      return [
        ...ls.map((l) => ({ ...l, fading: true })),
        { id: nextIdRef.current++, state, frame: 0, fading: false },
      ]
    })
    accRef.current = 0
  }, [state])

  // 淡出动画结束后卸载旧层
  const handleFadeEnd = (id: number) => {
    setLayers((ls) => ls.filter((l) => l.id !== id))
  }

  // 单 rAF 时钟：驱动翻帧与呼吸微动
  const layersRef = useRef(layers)
  layersRef.current = layers
  useEffect(() => {
    let raf = 0
    let last = performance.now()
    const advance = () => {
      setLayers((prev) => {
        const top = prev[prev.length - 1]
        const spec = FRAME_TABLE[top.state]
        const next = top.frame + 1
        const f = next < spec.frames ? next : spec.loop ? 0 : top.frame
        if (f === top.frame) return prev // 非循环已驻留末帧，不触发渲染
        const copy = prev.slice()
        copy[copy.length - 1] = { ...top, frame: f }
        return copy
      })
    }
    const step = (now: number) => {
      const dt = Math.min(now - last, 100) // 后台标签页回来时不追帧
      last = now
      accRef.current += dt
      const top = layersRef.current[layersRef.current.length - 1]
      const frameMs = 1000 / FRAME_TABLE[top.state].fps
      while (accRef.current >= frameMs) {
        accRef.current -= frameMs
        advance()
      }
      // 呼吸微动：绕过 React 直接写 DOM，帧切换不受影响
      const img = topImgRef.current
      if (img) {
        if (top.state === 'idle_breath') {
          const phase = (now % BREATH_MS) / BREATH_MS
          const scaleY = 1 + BREATH_AMP * Math.sin(phase * Math.PI * 2)
          img.style.transform = `scaleY(${scaleY.toFixed(4)})`
        } else if (img.style.transform) {
          img.style.transform = ''
        }
      }
      raf = requestAnimationFrame(step)
    }
    raf = requestAnimationFrame(step)
    return () => cancelAnimationFrame(raf)
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
      {layers.map((l, i) => {
        const isTop = i === layers.length - 1
        return (
          <img
            key={l.id}
            ref={isTop ? topImgRef : undefined}
            className={l.fading ? 'pet-fade-out' : undefined}
            src={frameUrl(l.state, l.frame)}
            alt={l.state}
            draggable={false}
            onAnimationEnd={l.fading ? () => handleFadeEnd(l.id) : undefined}
          />
        )
      })}
    </div>
  )
}
