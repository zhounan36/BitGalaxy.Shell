/**
 * 鲸鱼娘桌宠状态机 v2（框架无关，可单测）。
 *
 * 采用成熟桌宠方案（Shimeji-ee / VPet-Simulator / Unity Animator）验证过的
 * 「双层」结构：
 * - 基础逻辑层 PetBaseState：sleep / idle_breath / working / eating / need_input / drag
 *   由连接状态、会话流量、空闲计时驱动，计时与动作完全解耦。
 * - 动作叠加层 PetActionKey：ready / wave / idle_blink / angry
 *   叠在基础层之上播放，到时自动清除并回落基础层；带优先级门控，
 *   高优先级才能打断低优先级（angry(3) > ready(2) > wave(1) > idle_blink(0)）。
 *
 * 渲染状态（snapshot.state）= action ?? base，对 v1 消费方完全兼容。
 * 非循环动作（ready/wave/angry）播完后末帧驻留，动作总时长由 ACTION_MS
 * 决定，与精灵图帧数/帧率无关。
 */

export type PetBaseState =
  | 'sleep' // 断线 / 长时间空闲 · DeepSleep
  | 'idle_breath' // 待机呼吸
  | 'working' // 会话有输出 · 打字+内心独白
  | 'eating' // working 30s 无输出 · 干饭
  | 'need_input' // TOFU 指纹确认 / 等用户输入
  | 'drag' // 被用户拖拽 · 鲸尾扑腾

export type PetActionKey = 'ready' | 'wave' | 'idle_blink' | 'angry'

/** 兼容 v1 消费方（PetSprite / Bubble / DebugPanel）的渲染状态并集 */
export type PetStateKey = PetBaseState | PetActionKey

/** 动作叠加层优先级：数值高者可打断低者，同/低级请求被丢弃（防 wave 抖动） */
export const ACTION_PRIORITY: Record<PetActionKey, number> = {
  idle_blink: 0,
  wave: 1,
  ready: 2,
  angry: 3,
}

/** 动作驻留时长（ms）：由状态机决定节奏，不依赖剪辑播完 */
export const ACTION_MS: Record<PetActionKey, number> = {
  ready: 1600,
  wave: 1500,
  idle_blink: 600,
  angry: 3000,
}

/** wave 冷却：悬停反复进出不重复招手；poke 等主动交互不受限 */
const WAVE_COOLDOWN_MS = 15_000

/** drag 兜底：drag-end 丢失（如原生拖窗吞掉 pointerup）时最迟回落时间 */
const DRAG_STUCK_MS = 10_000

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

export interface PetSnapshot {
  /** 渲染状态 = action ?? base（兼容 v1） */
  state: PetStateKey
  /** 基础逻辑层真实状态 */
  base: PetBaseState
  /** 当前叠加的动作；null = 纯基础层 */
  action: PetActionKey | null
  connected: boolean
  sessions: number
  /** 待确认的主机指纹（need_input 时展示） */
  pendingHostkey: string | null
  /** 最近事件（调试面板用，新→旧） */
  log: PetEvent[]
}

export class PetMachine {
  private base: PetBaseState = 'sleep'
  private action: PetActionKey | null = null
  private actionStartedAt = 0
  private lastWaveAt = 0
  private connected = false
  private sessions = 0
  private pendingHostkey: string | null = null
  private baseEnteredAt = Date.now()
  private lastOutputAt = Date.now()
  private lastActivityAt = Date.now()
  private readonly log: PetEvent[] = []

  constructor(private readonly config: PetConfig = DEFAULT_PET_CONFIG) {}

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
    switch (ev.type) {
      case 'ws-open':
        this.connected = true
        this.touch()
        this.enterBase(this.naturalBase())
        this.playAction('ready') // 唤醒登场
        return
      case 'ws-close':
        this.connected = false
        this.sessions = 0
        this.pendingHostkey = null
        this.clearAction()
        this.enterBase('sleep') // 没饭吃就睡
        return
      case 'session-opened':
        this.sessions += 1
        this.pendingHostkey = null
        this.lastOutputAt = Date.now()
        this.touch()
        // 基础层立即切到工作；若 angry 正在叠放则播完自然回落
        this.enterBase('working')
        return
      case 'session-closed':
        this.sessions = Math.max(0, this.sessions - 1)
        this.touch()
        this.enterBase(this.naturalBase())
        return
      case 'output':
        this.lastOutputAt = Date.now()
        this.touch()
        // 干饭/深睡中被输出唤醒 → 回工作岗位
        if ((this.base === 'eating' || this.base === 'sleep') && this.sessions > 0) {
          this.enterBase('working')
        }
        return
      case 'hostkey-unknown':
        this.pendingHostkey = ev.fingerprint
        this.touch()
        this.enterBase('need_input')
        return
      case 'hostkey-confirmed':
        this.pendingHostkey = null
        this.touch()
        this.enterBase(this.naturalBase())
        return
      case 'agent-error':
        this.touch()
        this.playAction('angry') // 最高优先级，可打断一切动作
        return
      case 'pointer-enter':
        if (
          this.base === 'idle_breath' &&
          Date.now() - this.lastWaveAt >= WAVE_COOLDOWN_MS
        ) {
          this.playAction('wave')
        }
        return
      case 'poke':
        this.touch()
        if (this.base === 'drag') return // 拖拽中不理会
        if (this.base === 'sleep') {
          this.enterBase('idle_breath') // 戳醒
          this.playAction('ready')
        } else {
          this.playAction('wave') // 任意清醒状态都回应
        }
        return
      case 'drag-start':
        // 用户直接交互永远优先（VPet 原则）：打断当前动作进入拖拽
        this.clearAction()
        this.enterBase('drag')
        return
      case 'drag-end':
        this.touch()
        this.enterBase(this.naturalBase())
        return
      case 'blink':
        // usePet 只在显示态为 idle_breath 时调度；有动作叠放时由优先级门控丢弃
        if (this.base === 'idle_breath') this.playAction('idle_blink')
        return
      case 'tick':
        this.tick(ev.now)
        return
    }
  }

  private tick(now: number): void {
    // ① 动作层到期：与基础层计时无关，直接清除回落
    if (this.action && now - this.actionStartedAt >= ACTION_MS[this.action]) {
      this.clearAction()
    }
    // ② 基础层计时：不受动作叠放影响
    const held = now - this.baseEnteredAt
    switch (this.base) {
      case 'working':
        if (now - this.lastOutputAt >= this.config.eatingAfterMs) this.enterBase('eating')
        return
      case 'eating':
        if (now - this.lastActivityAt >= this.config.eatingToSleepMs) this.enterBase('sleep')
        return
      case 'idle_breath':
        if (now - this.lastActivityAt >= this.config.sleepAfterMs) this.enterBase('sleep')
        return
      case 'need_input':
        // 指纹确认实际在完整工作台完成；宠物只负责提醒（骨架阶段不做弹窗）
        return
      case 'drag':
        // drag-end 丢失时需自愈，否则永久扑腾
        if (held >= DRAG_STUCK_MS) this.enterBase(this.naturalBase())
        return
      default:
        return
    }
  }

  /** 自然基线：有会话 = 工作中，否则 = 待机 */
  private naturalBase(): PetBaseState {
    return this.connected && this.sessions > 0 ? 'working' : 'idle_breath'
  }

  /** 切换基础层（不影响动作叠放） */
  private enterBase(next: PetBaseState): void {
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
  private playAction(next: PetActionKey): void {
    if (this.base === 'drag') return
    if (this.action && ACTION_PRIORITY[next] <= ACTION_PRIORITY[this.action]) return
    this.action = next
    this.actionStartedAt = Date.now()
    if (next === 'wave') this.lastWaveAt = this.actionStartedAt
  }

  private clearAction(): void {
    this.action = null
  }

  private touch(): void {
    this.lastActivityAt = Date.now()
  }
}
