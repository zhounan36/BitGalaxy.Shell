/**
 * 内置动画 clip 表：内置鲸鱼娘已按《动作规范》落成标准动作包
 * （P6.5 外置到随包资源 pet-packs/builtin-whale/，帧命名 <state>/<state>_NN.png，
 * 与用户包同构，经 asset protocol 加载）。
 * frames/fps/loop 与该包 pack.json 的 states.* 保持一致，**以 pack.json 为准**，
 * 美术管线改动后需同步。动作语义 key 全集来自 pet/machine.ts（PetStateKey）——
 * 动画与动作在此解绑：内置表只是「内置动作包」的 clip 数据，用户包可按相同结构覆盖任意子集。
 *
 * 桌宠资产可选安装：包缺失（未随包分发 / 浏览器 dev）时 builtinAvailable()=false，
 * 内置动画整体跳过（渲染层回落占位形态），AI 对话能力不受影响。
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

/** 内置包帧 URL 工厂（resolveBuiltin 成功后非空）：file = "<state>/<state>_NN.png" */
let builtinToUrl: ((file: string) => string) | null = null

/** 解析内置动作包根目录（幂等）：Tauri 走 builtin_pack_root + asset protocol；
 *  未安装 / 浏览器 dev → 保持 null（降级：无精灵，仅占位形态） */
export async function resolveBuiltin(): Promise<void> {
  if (builtinToUrl || typeof window === 'undefined') return
  if (!('__TAURI_INTERNALS__' in window)) return // 浏览器 dev：资源已不在 vite public
  try {
    const { invoke, convertFileSrc } = await import('@tauri-apps/api/core')
    const root = await invoke<string>('builtin_pack_root')
    const norm = root.replace(/\\/g, '/')
    builtinToUrl = (file) => convertFileSrc(`${norm}/${file}`)
  } catch {
    builtinToUrl = null // 未安装桌宠资源：占位形态
  }
}

/** 内置动画是否可用（false = 桌宠资源未安装，渲染层用占位形态） */
export function builtinAvailable(): boolean {
  return builtinToUrl !== null
}

/** 内置帧路径：<state>/<state>_NN.png（00 起、两位补零，与动作规范一致） */
export function builtinFrameUrl(state: PetStateKeyV1, frame: number): string {
  return builtinToUrl!(`${state}/${state}_${String(frame).padStart(2, '0')}.png`)
}
