import type { PetStateKey } from './machine'

/** 内置台词库（与 public/pet-packs/builtin-whale/art-manifest.json 各状态 line 对齐；每状态随机取一条） */
export const STATE_LINES: Record<string, string[]> = {
  sleep: ['深度求索……深度睡眠……', 'Zzz……'],
  ready: ['探索未至之境！', '我回来啦，主人～'],
  idle_breath: ['我只是一条蹭饭的大肥鱼……', '（裙摆随呼吸轻轻晃动）'],
  idle_blink: [],
  wave: ['嗨～要连哪台主机？', '叫我有什么事吗？'],
  working: ['让我想想……（内心独白中）', '咳咳，正在处理……'],
  need_input: ['主人！有个指纹要你确认！'],
  eating: ['我先去吃饭，好了叫我～', '干饭是第一生产力！'],
  angry: ['真当我是便宜货？！', '哼！连不上才不怪我！'],
  drag: ['呀！放、放我下来啦！', '鲸尾要抽筋了……'],
}

/** 自定义动作 key 缺台词时的通用兜底池 */
export const GENERIC_LINES: string[] = [
  '主人好～',
  '（歪头）',
  '有什么吩咐吗？',
  '随时待命！',
]

/**
 * 取台词：优先内置状态台词；自定义 key 或无词条回落通用池。
 * 显式空池（如 idle_blink）返回 null（不显示台词）；完全没有 key 才用 GENERIC。
 */
export function pickLine(state: PetStateKey): string | null {
  const pool = Object.prototype.hasOwnProperty.call(STATE_LINES, state)
    ? STATE_LINES[state]
    : GENERIC_LINES
  if (!pool || pool.length === 0) return null
  return pool[Math.floor(Math.random() * pool.length)]
}
