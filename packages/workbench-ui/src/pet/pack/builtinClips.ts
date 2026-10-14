/**
 * 内置动画 clip 表：内置鲸鱼娘已按《动作规范》落成标准动作包
 * （public/pet-packs/builtin-whale/，帧命名 <state>/<state>_NN.png，与用户包同构）。
 * frames/fps/loop 与该包 pack.json 的 states.* 保持一致，**以 pack.json 为准**，
 * 美术管线改动后需同步。动作语义 key 全集来自 pet/machine.ts（PetStateKey）——
 * 动画与动作在此解绑：内置表只是「内置动作包」的 clip 数据，用户包可按相同结构覆盖任意子集。
 */
import type { PetStateKeyV1 } from '../pet/machine'

export interface ClipSpec {
  frames: number
  fps: number
  loop: boolean
}

/** 内置十个固定动作的 clip（key=PetStateKeyV1；用户包可自定义更多 key） */
export const BUILTIN_CLIPS: Record<PetStateKeyV1, ClipSpec> = {
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

/** 内置动作 key 全集（编译期校验覆盖完整） */
export const PET_STATE_KEYS = Object.keys(BUILTIN_CLIPS) as PetStateKeyV1[]

/** 素材根：base 恒为 '/'（MPA 合并后 dev/生产一致）；内置包与用户包统一路径布局 */
const BUILTIN_BASE = `${import.meta.env.BASE_URL}pet-packs/builtin-whale/`

/** 内置帧路径：<state>/<state>_NN.png（00 起、两位补零，与动作规范一致） */
export function builtinFrameUrl(state: PetStateKeyV1, frame: number): string {
  return `${BUILTIN_BASE}${state}/${state}_${String(frame).padStart(2, '0')}.png`
}
