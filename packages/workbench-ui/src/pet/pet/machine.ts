/**
 * 鲸鱼娘桌宠状态机 v3（框架无关，可单测）· 数据驱动。
 *
 * 在 v2「双层」结构（基础逻辑层 + 动作叠加层）之上，把「动作图」抽成可配置的
 * BehaviorTable，实现动画与动作、行为的完全解绑：
 * - actions：动作/基础态定义（layer/priority/durationMs/cooldownMs），key 可自定义；
 * - triggers：事件 → 目标动作 key 映射；
 * - transitions：定时转换图（from 停留 afterMs → to），替代硬编码 tick 分支；
 * - natural：自然基线三槽（active/idle/sleep）；
 * - config：时间参数。
 *
 * 内置 DEFAULT_BEHAVIOR 精确复刻 v1/v2 行为；动作包可经 pack.json 的 behavior 节
 * override（按 key 合并）或 replace（全量替换）——不改代码即可自定义动作图。
 *
 * 渲染状态（snapshot.state）= action ?? base，key 为字符串（支持自定义动作名）。
 */

// ── 内置动作语义（v1 十个固定 key，仅作内置默认表的类型约束/文档） ──────────
export type PetBaseState =
  | 'sleep' // 断线 / 长时间空闲 · DeepSleep
  | 'idle_breath' // 待机呼吸
  | 'working' // 会话有输出 · 打字+内心独白
  | 'eating' // working 30s 无输出 · 干饭
  | 'need_input' // TOFU 指纹确认 / 等用户输入
  | 'drag' // 被用户拖拽 · 鲸尾扑腾

export type PetActionKey = 'ready' | 'wave' | 'idle_blink' | 'angry'

/** 内置十个固定动作 key（动作包可自定义更多，故渲染态用 string） */
export type PetStateKeyV1 = PetBaseState | PetActionKey

/** 渲染/台词/clip 查询的通用动作 key（内置 = PetStateKeyV1，动作包可自定义） */
export type PetStateKey = string

// ── 事件（不变：Agent 事件流 + 交互 + 计时） ───────────────────────────────
export type TriggerName =
  | 'ws-open'
  | 'ws-close'
  | 'session-opened'
  | 'session-closed'
  | 'output'
  | 'hostkey-unknown'
  | 'hostkey-confirmed'
  | 'agent-error'
  | 'pointer-enter'
  | 'poke'
  | 'drag-start'
  | 'drag-end'
  | 'blink'

export type PetEvent =
  | { type: 'ws-open' } // Agent WS 已连接
  | { type: 'ws-close' } // Agent WS 断开
  | { type: 'session-opened' }
  | { type: 'session-closed' }
  | { type: 'output' } // 任一 shell 会话产生 stdout/stderr
  | { type: 'hostkey-unknown'; fingerprint: string }
  | { type: 'hostkey-confirmed' }
  | { type: 'agent-error' } // open-failed / probe 失败等
  | { type: 'pointer-enter' }
  | { type: 'poke' } // 单击宠物
  | { type: 'drag-start' }
  | { type: 'drag-end' }
  | { type: 'blink' } // 眨眼调度器注入
  | { type: 'tick'; now: number }

// ── 行为表结构（pack.json 的 behavior 节 = BehaviorSpec；运行时 = BehaviorTable） ──
export interface ActionDef {
  /** base=基础循环层（进入后驻留直到被切换）；oneshot=叠加层（驻留 durationMs 后回落） */
  layer: 'base' | 'oneshot'
  /** oneshot 打断门控：数值高者可打断低者（0..10） */
  priority?: number
  /** oneshot 驻留时长（ms） */
  durationMs?: number
  /** 触发冷却（ms）：pointer-enter（悬停）受冷却约束，poke 等主动交互不受限 */
  cooldownMs?: number
}

/** 定时转换时钟：afterMs 从何时起算 */
export type TransitionClock = 'enter' | 'output' | 'activity'

export interface TransitionDef {
  from: string
  afterMs: number
  to: string
  /** 缺省 'enter'（从进入 from 计时） */
  clock?: TransitionClock
}

/** 动作包 behavior 节（用户自定义动作图） */
export interface BehaviorSpec {
  mode?: 'override' | 'replace' // 缺省 override：按 key 合并内置表
  config?: Partial<PetConfig>
  actions: Record<string, ActionDef>
  triggers?: Partial<Record<TriggerName, string>>
  transitions?: TransitionDef[]
  /** 自然基线三槽（缺省沿用内置） */
  natural?: { active?: string; idle?: string; sleep?: string }
}

export interface PetConfig {
  /** working 无输出多久进入干饭 */
  eatingAfterMs: number
  /** 干饭多久没动静转为深睡 */
  eatingToSleepMs: number
  /** 空闲多久进入深睡 */
  sleepAfterMs: number
}

export const DEFAULT_PET_CONFIG: PetConfig = {
  eatingAfterMs: 30_000,
  eatingToSleepMs: 60_000,
  sleepAfterMs: 5 * 60_000,
}

/** 运行时解析完成的行为表 */
export interface BehaviorTable {
  config: PetConfig
  actions: Record<string, ActionDef>
  triggers: Partial<Record<TriggerName, string>>
  transitions: TransitionDef[]
  natural: { active: string; idle: string; sleep: string }
}

/**
 * 内置默认行为：精确复刻 v1/v2。
 * 值不变——ACTION_PRIORITY/ACTION_MS 等旧常量的表化。
 */
export const DEFAULT_BEHAVIOR: BehaviorTable = {
  config: DEFAULT_PET_CONFIG,
  actions: {
    sleep: { layer: 'base' },
    idle_breath: { layer: 'base' },
    working: { layer: 'base' },
    eating: { layer: 'base' },
    need_input: { layer: 'base' },
    drag: { layer: 'base' },
    ready: { layer: 'oneshot', priority: 2, durationMs: 1600 },
    wave: { layer: 'oneshot', priority: 1, durationMs: 1500, cooldownMs: 15_000 },
    idle_blink: { layer: 'oneshot', priority: 0, durationMs: 600 },
    angry: { layer: 'oneshot', priority: 3, durationMs: 3000 },
  },
  triggers: {
    'ws-open': 'ready',
    'ws-close': 'sleep',
    'session-opened': 'working',
    output: 'working',
    'hostkey-unknown': 'need_input',
    'agent-error': 'angry',
    'pointer-enter': 'wave',
    poke: 'wave',
    'drag-start': 'drag',
    blink: 'idle_blink',
    // session-closed / hostkey-confirmed / drag-end 自然回落 naturalBase（无需固定映射）
  },
  transitions: [], // 由 resolveBehavior 按 config 生成（见 buildBuiltinTransitions）
  natural: { active: 'working', idle: 'idle_breath', sleep: 'sleep' },
}

/** v1 三条计时转换（键名=内置基础态）：clock 复刻 v1 时钟语义 */
function buildBuiltinTransitions(config: PetConfig): TransitionDef[] {
  return [
    { from: 'working', to: 'eating', clock: 'output', afterMs: config.eatingAfterMs },
    { from: 'eating', to: 'sleep', clock: 'activity', afterMs: config.eatingToSleepMs },
    { from: 'idle_breath', to: 'sleep', clock: 'activity', afterMs: config.sleepAfterMs },
  ]
}

/**
 * 合成最终行为表：内置默认 + 动作包 behavior（override 按 key 合并 / replace 全量）。
 * 转换规则按 `from` 去重，动作包规则优先。
 */
export function resolveBehavior(builtin: BehaviorTable, spec?: BehaviorSpec): BehaviorTable {
  if (!spec) return { ...builtin, transitions: buildBuiltinTransitions(builtin.config) }
  const mode = spec.mode ?? 'override'
  const config: PetConfig = { ...builtin.config, ...spec.config }
  const actions: Record<string, ActionDef> =
    mode === 'replace' ? { ...spec.actions } : { ...builtin.actions, ...spec.actions }
  const natural = {
    active: spec.natural?.active ?? builtin.natural.active,
    idle: spec.natural?.idle ?? builtin.natural.idle,
    sleep: spec.natural?.sleep ?? builtin.natural.sleep,
  }
  const triggers: Partial<Record<TriggerName, string>> =
    mode === 'replace' ? { ...spec.triggers } : { ...builtin.triggers, ...spec.triggers }

  const userTransitions = spec.transitions ?? []
  let transitions: TransitionDef[]
  if (mode === 'replace') {
    transitions = [...userTransitions]
  } else {
    const builtinT = buildBuiltinTransitions(config).filter(
      (bt) => !userTransitions.some((ut) => ut.from === bt.from),
    )
    transitions = [...builtinT, ...userTransitions]
  }
  return { config, actions, triggers, transitions, natural }
}

export interface PetSnapshot {
  /** 渲染状态 = action ?? base（key 可自定义） */
  state: PetStateKey
  /** 基础逻辑层真实状态 */
  base: PetStateKey
  /** 当前叠加的动作；null = 纯基础层 */
  action: PetStateKey | null
  connected: boolean
  sessions: number
  /** 待确认的主机指纹（need_input 时展示） */
  pendingHostkey: string | null
  /** 最近事件（调试面板用，新→旧） */
  log: PetEvent[]
}

/** drag 兜底：drag-end 丢失（如原生拖窗吞掉 pointerup）时最迟回落时间 */
const DRAG_STUCK_MS = 10_000

export class PetMachine {
  private bh: BehaviorTable
  private base: PetStateKey
  private action: PetStateKey | null = null
  private actionStartedAt = 0
  private readonly lastPlayedAt = new Map<PetStateKey, number>()
  private connected = false
  private sessions = 0
  private pendingHostkey: string | null = null
  private baseEnteredAt = Date.now()
  private lastOutputAt = Date.now()
  private lastActivityAt = Date.now()
  private readonly log: PetEvent[] = []

  constructor(spec?: BehaviorSpec) {
    this.bh = resolveBehavior(DEFAULT_BEHAVIOR, spec)
    this.base = this.bh.natural.sleep // 初始：断线/未连接 = 深睡
  }

  /** 热切换行为表（换动作包时），保留当前状态 */
  applyBehavior(spec?: BehaviorSpec): void {
    this.bh = resolveBehavior(DEFAULT_BEHAVIOR, spec)
  }

  /** 当前应渲染的状态（动作层压在基础层上） */
  get state(): PetStateKey {
    return this.action ?? this.base
  }

  snapshot(): PetSnapshot {
    return {
      state: this.state,
      base: this.base,
      action: this.action,
      connected: this.connected,
      sessions: this.sessions,
      pendingHostkey: this.pendingHostkey,
      log: this.log.slice(0, 20),
    }
  }

  handle(ev: PetEvent): void {
    this.log.unshift(ev)
    if (this.log.length > 20) this.log.pop()
    const bh = this.bh
    const key = (t: TriggerName) => bh.triggers[t]

    switch (ev.type) {
      case 'ws-open':
        this.connected = true
        this.touch()
        this.enterBase(this.naturalBase())
        this.playAction(key('ws-open')) // 唤醒登场
        return
      case 'ws-close':
        this.connected = false
        this.sessions = 0
        this.pendingHostkey = null
        this.clearAction()
        this.enterBase(key('ws-close') ?? bh.natural.sleep) // 没饭吃就睡
        return
      case 'session-opened':
        this.sessions += 1
        this.pendingHostkey = null
        this.lastOutputAt = Date.now()
        this.touch()
        // 基础层立即切到工作；若 oneshot 正在叠放则播完自然回落
        this.enterBase(key('session-opened') ?? bh.natural.active)
        return
      case 'session-closed':
        this.sessions = Math.max(0, this.sessions - 1)
        this.touch()
        this.enterBase(key('session-closed') ?? this.naturalBase())
        return
      case 'output':
        this.lastOutputAt = Date.now()
        this.touch()
        // 干饭/深睡中被输出唤醒 → 回工作岗位（sessions>0 且未在活跃态）
        if (this.sessions > 0 && this.base !== bh.natural.active) {
          this.enterBase(key('output') ?? bh.natural.active)
        }
        return
      case 'hostkey-unknown':
        this.pendingHostkey = ev.fingerprint
        this.touch()
        this.enterBase(key('hostkey-unknown') ?? bh.natural.idle)
        return
      case 'hostkey-confirmed':
        this.pendingHostkey = null
        this.touch()
        this.enterBase(key('hostkey-confirmed') ?? this.naturalBase())
        return
      case 'agent-error':
        this.touch()
        this.playAction(key('agent-error')) // 最高优先级，可打断一切动作
        return
      case 'pointer-enter': {
        const target = key('pointer-enter')
        if (target && this.base === bh.natural.idle && this.cooled(target)) {
          this.playAction(target)
        }
        return
      }
      case 'poke': {
        this.touch()
        if (this.base === (key('drag-start') ?? 'drag')) return // 拖拽中不理会
        if (this.base === bh.natural.sleep) {
          this.enterBase(bh.natural.idle) // 戳醒
          this.playAction(key('ws-open')) // 登场动作
        } else {
          this.playAction(key('poke')) // 任意清醒状态都回应
        }
        return
      }
      case 'drag-start':
        // 用户直接交互永远优先（VPet 原则）：打断当前动作进入拖拽
        this.clearAction()
        this.enterBase(key('drag-start') ?? 'drag')
        return
      case 'drag-end':
        this.touch()
        this.enterBase(key('drag-end') ?? this.naturalBase())
        return
      case 'blink': {
        const target = key('blink')
        if (target && this.base === bh.natural.idle) this.playAction(target)
        return
      }
      case 'tick':
        this.tick(ev.now)
        return
    }
  }

  private tick(now: number): void {
    // ① 动作层到期：与基础层计时无关，直接清除回落
    if (this.action) {
      const def = this.bh.actions[this.action]
      const dur = def?.durationMs ?? 0
      if (dur > 0 && now - this.actionStartedAt >= dur) this.clearAction()
    }
    // ② 基础层定时转换图（不受动作叠放影响）
    for (const t of this.bh.transitions) {
      if (this.base !== t.from) continue
      const ref =
        t.clock === 'output'
          ? this.lastOutputAt
          : t.clock === 'activity'
            ? this.lastActivityAt
            : this.baseEnteredAt
      if (now - ref >= t.afterMs) {
        this.enterTarget(t.to)
        break
      }
    }
    // ③ drag 自愈（drag-end 丢失时需自愈，否则永久扑腾）
    const dragKey = this.bh.triggers['drag-start'] ?? 'drag'
    if (this.base === dragKey && now - this.baseEnteredAt >= DRAG_STUCK_MS) {
      this.enterBase(this.naturalBase())
    }
  }

  /** 自然基线：有会话 = 工作中，否则 = 待机 */
  private naturalBase(): PetStateKey {
    return this.connected && this.sessions > 0 ? this.bh.natural.active : this.bh.natural.idle
  }

  /** 目标按 layer 分派：base 进基础层，oneshot 播叠加动作 */
  private enterTarget(key: PetStateKey): void {
    if (this.bh.actions[key]?.layer === 'oneshot') this.playAction(key)
    else this.enterBase(key)
  }

  /** 切换基础层（不影响动作叠放） */
  private enterBase(next: PetStateKey): void {
    if (this.base === next) {
      this.baseEnteredAt = Date.now()
      return
    }
    this.base = next
    this.baseEnteredAt = Date.now()
  }

  /**
   * 叠加动作：优先级门控——
   * - drag 中不叠新动作（先 drag-end / tick 自愈）
   * - 请求优先级 ≤ 当前动作时丢弃（防 hover 反复触发 wave 抖动）
   */
  private playAction(next: PetStateKey | undefined): void {
    if (!next) return
    const dragKey = this.bh.triggers['drag-start'] ?? 'drag'
    if (this.base === dragKey) return
    const nextDef = this.bh.actions[next]
    if (this.action) {
      const curDef = this.bh.actions[this.action]
      const nextPri = nextDef?.priority ?? 0
      const curPri = curDef?.priority ?? 0
      if (nextPri <= curPri) return
    }
    this.action = next
    this.actionStartedAt = Date.now()
    this.lastPlayedAt.set(next, this.actionStartedAt) // 记录冷却起点
  }

  /** 悬停冷却判定（poke 不受此约束）：首次（未播过）恒通过 */
  private cooled(key: PetStateKey): boolean {
    const cd = this.bh.actions[key]?.cooldownMs ?? 0
    if (cd <= 0) return true
    const last = this.lastPlayedAt.get(key) ?? 0
    return Date.now() - last >= cd
  }

  private clearAction(): void {
    this.action = null
  }

  private touch(): void {
    this.lastActivityAt = Date.now()
  }
}
