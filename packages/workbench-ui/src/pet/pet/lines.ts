import type { PetStateKey } from './machine'

/** 台词库（与 assets/pet/manifest.json 各状态 line 对齐；每状态随机取一条） */
export const STATE_LINES: Record<PetStateKey, string[]> = {
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

export function pickLine(state: PetStateKey): string | null {
  const pool = STATE_LINES[state]
  if (!pool || pool.length === 0) return null
  return pool[Math.floor(Math.random() * pool.length)]
}
