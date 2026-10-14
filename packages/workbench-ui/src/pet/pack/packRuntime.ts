/**
 * 动作包运行时：动画与动作解绑的渲染侧入口。
 *
 * 动作语义 key（PetStateKey）由状态机定义；本模块把「哪个动作播哪段动画」
 * 解析为 ResolvedClip：
 * - 用户动作包（pet-packs/<id>/，Tauri 磁盘）可覆盖任意动作子集；
 * - 未覆盖的动作回落内置鲸鱼娘动画（builtinClips）。
 * 浏览器宿主无磁盘动作包能力，恒为内置。
 */
import type { PetStateKey } from '../pet/machine'
import { BUILTIN_CLIPS, builtinFrameUrl, type ClipSpec } from './builtinClips'

export interface ResolvedClip extends ClipSpec {
  /** 第 i 帧的可加载 URL（内置 = /pet/ 相对路径；用户包 = convertFileSrc asset URL） */
  frameUrl(i: number): string
}

/** Rust PetPackInfo（pet_packs_list / pet_pack_import 返回） */
export interface PetPackInfo {
  id: string
  name: string
  version: string
  author?: string | null
  states: Array<{ key: PetStateKey; frames: number; fps: number; loop: boolean }>
  /** 动作包落盘绝对目录（帧文件在 <dir>/<key>/<key>_NN.png） */
  dir: string
  active: boolean
}

/** 内置包信息（列表展示用；dir 为空，帧走 /pet/） */
export const BUILTIN_PACK_INFO: PetPackInfo = {
  id: '',
  name: '内置 · 鲸鱼娘',
  version: '',
  author: undefined,
  states: (Object.keys(BUILTIN_CLIPS) as PetStateKey[]).map((key) => ({
    key,
    frames: BUILTIN_CLIPS[key].frames,
    fps: BUILTIN_CLIPS[key].fps,
    loop: BUILTIN_CLIPS[key].loop,
  })),
  dir: '',
  active: true,
}

export interface PackRuntime {
  clip(key: PetStateKey): ResolvedClip
}

const builtinClip = (key: PetStateKey): ResolvedClip => ({
  ...BUILTIN_CLIPS[key],
  frameUrl: (i) => builtinFrameUrl(key, i),
})

/**
 * 构建运行时：用户包覆盖其 states 中的动作，缺失动作回落内置。
 * frameUrl 由调用方注入（Tauri 下 convertFileSrc(dir)，浏览器/内置传 undefined），
 * 便于双宿主共用解析逻辑。
 */
export function createPackRuntime(
  pack: PetPackInfo | null,
  toUrl: (absDir: string, file: string) => string = (dir, file) => `${dir}/${file}`,
): PackRuntime {
  const overrides = new Map<PetStateKey, ResolvedClip>()
  if (pack) {
    for (const s of pack.states) {
      overrides.set(s.key, {
        frames: s.frames,
        fps: s.fps,
        loop: s.loop,
        frameUrl: (i) =>
          toUrl(pack.dir, `${s.key}/${s.key}_${String(i).padStart(2, '0')}.png`),
      })
    }
  }
  return {
    clip(key) {
      return overrides.get(key) ?? builtinClip(key)
    },
  }
}
