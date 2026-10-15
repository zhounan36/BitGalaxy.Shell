import {
  useEffect,
  useRef,
  useState,
  type PointerEvent as ReactPointerEvent,
} from 'react'
import type { PetStateKey } from '../pet/machine'
import { PET_STATE_KEYS } from '../pack/builtinClips'
import type { PackRuntime, ResolvedClip } from '../pack/packRuntime'

/** 呼吸微动：周期 3.6s（贴近真人静息呼吸），scaleY ±1.5%，以脚底为轴（Live2D 式程序化微动） */
const BREATH_MS = 3600
const BREATH_AMP = 0.015

interface PetSpriteProps {
  state: PetStateKey
  /** 动画解析运行时：动作 → clip（用户动作包覆盖，缺失回落内置） */
  runtime: PackRuntime
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
 * 单个 clip 的媒体渲染（多格式）：
 * - frames：帧序列 img，由外部 rAF 驱动翻帧（frameUrl(i)）；
 * - image：单静态图/动图（GIF/APNG/WebP 动图原生播放）；
 * - video：单视频（webm/mp4，含透明 webm），muted 自动循环/驻留。
 */
function ClipMedia({
  clip,
  frame,
  isTop,
  imgRef,
}: {
  clip: ResolvedClip
  frame: number
  isTop: boolean
  imgRef?: (el: HTMLImageElement | null) => void
}) {
  if (clip.kind === 'frames') {
    return (
      <img
        ref={imgRef}
        src={clip.frameUrl(frame)}
        alt="pet-frame"
        draggable={false}
      />
    )
  }
  if (clip.kind === 'image') {
    return <img ref={imgRef} src={clip.src} alt="pet-image" draggable={false} />
  }
  // video：loop=false 时播完驻留末帧（pause 在 ended 处理）
  return (
    <video
      ref={isTop ? undefined : undefined}
      src={clip.src}
      poster={clip.poster}
      muted
      autoPlay
      playsInline
      preload="metadata"
      loop={clip.loop}
      onEnded={(e) => {
        if (!clip.loop) e.currentTarget.pause() // 驻留末帧
      }}
    />
  )
}

/**
 * 精灵渲染器 v3（多格式 clip）：
 * - 单 rAF 时钟按 dt 累积推进帧序列（仅 frames clip），切换无计时错位；
 * - 状态切换时旧层冻结末帧、CSS 140ms 淡出，新层在下层直接起播（交叉淡化）；
 * - idle_breath 时叠加程序化呼吸微动（scaleY 正弦，Live2D 式），直接写 DOM；
 * - image/video 由浏览器原生播放，rAF 不驱动其内部时间轴。
 * 非循环动作的驻留时长由状态机（durationMs）决定。
 * 指针交互：按下并移动=拖拽、原地点击=戳、悬停=招手。
 */
export function PetSprite({
  state,
  runtime,
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
  // runtime 热读（rAF 循环内用），动作包切换后引用更新即生效
  const runtimeRef = useRef(runtime)
  runtimeRef.current = runtime

  // 预载：仅激活 clip 的帧序列/静态图（video 不预载，避免内存峰值）
  useEffect(() => {
    for (const s of PET_STATE_KEYS) {
      const clip = runtime.clip(s)
      if (!clip) continue
      if (clip.kind === 'frames') {
        for (let i = 0; i < clip.frames; i++) {
          const img = new Image()
          img.src = clip.frameUrl(i)
        }
      } else if (clip.kind === 'image') {
        const img = new Image()
        img.src = clip.src
      }
    }
  }, [runtime])

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

  // 淡出动画结束后卸载旧层（video 淡出即卸载 src，释放解码内存）
  const handleFadeEnd = (id: number) => {
    setLayers((ls) => ls.filter((l) => l.id !== id))
  }

  // 单 rAF 时钟：驱动 frames 翻帧与呼吸微动
  const layersRef = useRef(layers)
  layersRef.current = layers
  useEffect(() => {
    let raf = 0
    let last = performance.now()
    const advance = () => {
      setLayers((prev) => {
        const top = prev[prev.length - 1]
        const spec = runtimeRef.current.clip(top.state)
        if (!spec || spec.kind !== 'frames') return prev
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
      const spec = runtimeRef.current.clip(top.state)
      if (spec && spec.kind === 'frames') {
        const frameMs = 1000 / (spec.fps || 6)
        while (accRef.current >= frameMs) {
          accRef.current -= frameMs
          advance()
        }
      } else {
        accRef.current = 0 // image/video 由浏览器原生驱动
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

  // P6.5 桌宠资产可选：没有任何可渲染 clip（内置包未安装且无用户包）时
  // 渲染占位形态——宠物区域保持可见可交互（拖动/右键/双击照常）
  const hasAnyClip = layers.some((l) => runtimeRef.current.clip(l.state) !== null)

  return (
    <div
      className="pet-sprite"
      style={{ width: size, height: size }}
      onPointerEnter={onPointerEnter}
      onPointerDown={handlePointerDown}
      onPointerMove={handlePointerMove}
      onPointerUp={handlePointerUp}
    >
      {!hasAnyClip && <div className="pet-sprite-fallback" title="桌宠资源未安装（可在动作包管理中导入）">🐳</div>}
      {layers.map((l, i) => {
        const isTop = i === layers.length - 1
        const clip = runtimeRef.current.clip(l.state)
        return (
          <div
            key={l.id}
            className={`pet-layer${l.fading ? ' pet-fade-out' : ''}`}
            onAnimationEnd={l.fading ? () => handleFadeEnd(l.id) : undefined}
          >
            {clip && (
              <ClipMedia
                clip={clip}
                frame={l.frame}
                isTop={isTop}
                imgRef={isTop ? (el) => (topImgRef.current = el) : undefined}
              />
            )}
          </div>
        )
      })}
    </div>
  )
}
