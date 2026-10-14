/**
 * 动作包运行时：动画与动作解绑的渲染侧入口。
 *
 * 动作语义 key 由状态机（machine.ts）定义；本模块把「哪个动作播哪段动画」
 * 解析为 ResolvedClip（判别联合，多格式）：
 * - 用户动作包（pet-packs/<id>/，Tauri 磁盘）可覆盖任意动作子集、任意格式；
 * - 未覆盖的动作回落内置鲸鱼娘动画（builtinClips）。
 * 浏览器宿主无磁盘动作包能力，恒为内置。
 */
import type { BehaviorSpec, PetStateKey, PetStateKeyV1 } from '../pet/machine'
import { BUILTIN_CLIPS, builtinFrameUrl, type ClipSpec } from './builtinClips'

/** 统一 clip：多格式动画（frames=帧序列 / image=单图·动图 / video=单视频） */
export type ResolvedClip =
  | {
      kind: 'frames'
      frames: number
      fps: number
      loop: boolean
      /** 第 i 帧的可加载 URL */
      frameUrl(i: number): string
    }
  | {
      kind: 'image'
      src: string
      /** 动图覆盖步进（可选）；静态图缺省=驻留 */
      fps?: number
      loop: boolean
    }
  | {
      kind: 'video'
      src: string
      loop: boolean
      poster?: string
    }

/** Rust PackClipStates 项（pet_packs_list / pet_pack_import 返回） */
export interface PackClipState {
  key: PetStateKey
  kind: 'frames' | 'image' | 'video'
  /** frames 专用 */
  frames?: number
  fps?: number
  /** frames/video 必有；image 可缺省 */
  loop?: boolean
  /** frames 的帧扩展名（png/jpg/webp/gif），缺省 png */
  ext?: string
  /** image/video 的包内相对路径 */
  src?: string
  /** video 海报图（包内相对路径） */
  poster?: string
}

/** Rust PetPackInfo（pet_packs_list / pet_pack_import 返回） */
export interface PetPackInfo {
  id: string
  name: string
  version: string
  author?: string | null
  states: PackClipState[]
  /** 动作包落盘绝对目录（素材在 <dir>/ 下） */
  dir: string
  active: boolean
  /** 动作包 behavior 节（用户自定义动作图，Rust 原样透传） */
  behavior?: BehaviorSpec | null
}

/** 内置包信息（列表展示用；dir 为空，帧走 /pet-packs/builtin-whale/） */
export const BUILTIN_PACK_INFO: PetPackInfo = {
  id: '',
  name: '内置 · 鲸鱼娘',
  version: '',
  author: undefined,
  states: (Object.keys(BUILTIN_CLIPS) as PetStateKeyV1[]).map((key) => ({
    key,
    kind: 'frames' as const,
    frames: BUILTIN_CLIPS[key].frames,
    fps: BUILTIN_CLIPS[key].fps,
    loop: BUILTIN_CLIPS[key].loop,
  })),
  dir: '',
  active: true,
}

export interface PackRuntime {
  clip(key: PetStateKey): ResolvedClip | null
  /** 该 runtime 是否包含此动作的自定义动画（false=回落内置） */
  has(key: PetStateKey): boolean
}

const builtinClip = (key: PetStateKey): ResolvedClip | null => {
  const spec = (BUILTIN_CLIPS as Partial<Record<string, ClipSpec>>)[key]
  if (!spec) return null // 自定义 key 无内置动画 → 渲染层跳过该图层
  return {
    kind: 'frames',
    frames: spec.frames,
    fps: spec.fps,
    loop: spec.loop,
    frameUrl: (i) => builtinFrameUrl(key as PetStateKeyV1, i),
  }
}

/**
 * 构建运行时：用户包覆盖其 states 中的动作，缺失动作回落内置。
 * URL 由调用方注入（Tauri 下 convertFileSrc(dir)，浏览器/内置传默认相对拼接），
 * 便于双宿主共用解析逻辑。
 */
export function createPackRuntime(
  pack: PetPackInfo | null,
  toUrl: (absDir: string, file: string) => string = (dir, file) => `${dir}/${file}`,
): PackRuntime {
  const overrides = new Map<PetStateKey, ResolvedClip>()
  if (pack) {
    for (const s of pack.states) {
      if (s.kind === 'frames') {
        const ext = (s.ext ?? 'png').replace(/^\./, '')
        overrides.set(s.key, {
          kind: 'frames',
          frames: s.frames ?? 1,
          fps: s.fps ?? 6,
          loop: s.loop ?? true,
          frameUrl: (i) =>
            toUrl(pack.dir, `${s.key}/${s.key}_${String(i).padStart(2, '0')}.${ext}`),
        })
      } else if (s.kind === 'image') {
        overrides.set(s.key, {
          kind: 'image',
          src: toUrl(pack.dir, `${s.key}/${s.src}`),
          fps: s.fps,
          loop: s.loop ?? true,
        })
      } else {
        overrides.set(s.key, {
          kind: 'video',
          src: toUrl(pack.dir, `${s.key}/${s.src}`),
          loop: s.loop ?? true,
          poster: s.poster ? toUrl(pack.dir, `${s.key}/${s.poster}`) : undefined,
        })
      }
    }
  }
  return {
    clip(key) {
      return overrides.get(key) ?? builtinClip(key)
    },
    has(key) {
      return overrides.has(key)
    },
  }
}
