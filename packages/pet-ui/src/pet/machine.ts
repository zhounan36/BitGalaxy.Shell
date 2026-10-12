/**
 * 鲸鱼娘桌宠状态机（框架无关，可单测）。
 *
 * 状态与素材一一对应：public/pet/<state>/<state>_00.png（见 assets/pet/manifest.json）。
 * 事件来源：Agent 控制帧（agentSocket.onControl）、PTY 数据帧流量、指针交互、tick 计时器。
 * 时间常量与 manifest 的 transitions 声明保持一致。
 */

export type PetStateKey =
  | 'sleep' // 空闲 5 分钟 / 断线 · DeepSleep
  | 'ready' // 唤醒/连上 Agent 登场亮相（瞬态）
  | 'idle_breath' // 待机呼吸
  | 'idle_blink' // 待机眨眼（瞬态）
  | 'wave' // hover 问候 / 戳醒（瞬态）
  | 'working' // 会话有输出 · 打字+内心独白
  | 'need_input' // TOFU 指纹确认 / 等用户输入
  | 'eating' // working 30s 无输出 · 干饭
  | 'angry' // open-failed / probe 失败 · 炸毛（3s 自愈）
  | 'drag' // 被用户拖拽 · 鲸尾扑腾

/** 瞬态：到时自动回落；持续态：由事件/计时器切换 */
const TRANSIENT: Partial<Record<PetStateKey, number>> = {
  ready: 1600,
  wave: 1500,
  idle_blink: 450,
  angry: 3000,
}

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
  state: PetStateKey
  /** 无瞬态时的自然基线（决定瞬态结束后回落到哪） */
  base: 'idle_breath' | 'working'
  connected: boolean
  sessions: number
  /** 待确认的主机指纹（need_input 时展示） */
  pendingHostkey: string | null
  /** 最近事件（调试面板用，新→旧） */
  log: PetEvent[]
}

export class PetMachine {
  private state: PetStateKey = 'sleep'
  private base: 'idle_breath' | 'working' = 'idle_breath'
  private connected = false
  private sessions = 0
  private pendingHostkey: string | null = null
  private enteredAt = Date.now()
  private lastOutputAt = Date.now()
  private lastActivityAt = Date.now()
  private readonly log: PetEvent[] = []

  constructor(private readonly config: PetConfig = DEFAULT_PET_CONFIG) {}

  snapshot(): PetSnapshot {
    return {
      state: this.state,
      base: this.base,
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
        this.enter('ready')
        return
      case 'ws-close':
        this.connected = false
        this.sessions = 0
        this.pendingHostkey = null
        this.enter('sleep') // 没饭吃就睡
        return
      case 'session-opened':
        this.sessions += 1
        this.pendingHostkey = null
        this.lastOutputAt = Date.now()
        this.touch()
        if (this.state !== 'angry') this.enter('working')
        return
      case 'session-closed':
        this.sessions = Math.max(0, this.sessions - 1)
        this.touch()
        this.enter(this.base)
        return
      case 'output':
        this.lastOutputAt = Date.now()
        this.touch()
        // 干饭/深睡中被输出唤醒 → 回工作岗位
        if ((this.state === 'eating' || this.state === 'sleep') && this.sessions > 0) {
          this.enter('working')
        }
        return
      case 'hostkey-unknown':
        this.pendingHostkey = ev.fingerprint
        this.touch()
        this.enter('need_input')
        return
      case 'hostkey-confirmed':
        this.pendingHostkey = null
        this.touch()
        this.enter(this.base)
        return
      case 'agent-error':
        this.touch()
        this.enter('angry')
        return
      case 'pointer-enter':
        if (this.state === 'idle_breath' || this.state === 'idle_blink') this.enter('wave')
        return
      case 'poke':
        this.touch()
        if (this.state === 'sleep') this.enter('ready')
        return
      case 'drag-start':
        if (this.state !== 'angry') this.enter('drag')
        return
      case 'drag-end':
        this.touch()
        this.enter(this.base)
        return
      case 'blink':
        if (this.state === 'idle_breath') this.enter('idle_blink')
        return
      case 'tick':
        this.tick(ev.now)
        return
    }
  }

  private tick(now: number): void {
    const held = now - this.enteredAt
    const transientMs = TRANSIENT[this.state]
    if (transientMs !== undefined) {
      if (held >= transientMs) this.enter(this.base)
      return
    }
    switch (this.state) {
      case 'working':
        if (now - this.lastOutputAt >= this.config.eatingAfterMs) this.enter('eating')
        return
      case 'eating':
        if (now - this.lastActivityAt >= this.config.eatingToSleepMs) this.enter('sleep')
        return
      case 'idle_breath':
      case 'idle_blink':
        if (now - this.lastActivityAt >= this.config.sleepAfterMs) this.enter('sleep')
        return
      case 'need_input':
        // 指纹确认实际在完整工作台完成；宠物只负责提醒（骨架阶段不做弹窗）
        return
      case 'drag':
        // 无瞬态表项，但 drag-end 丢失时需自愈，否则永久扑腾
        if (held >= DRAG_STUCK_MS) this.enter(this.base)
        return
      default:
        return
    }
  }

  /** 基线随会话数变化：有会话=工作中，无会话=待机 */
  private syncBase(): void {
    this.base = this.connected && this.sessions > 0 ? 'working' : 'idle_breath'
  }

  private enter(next: PetStateKey): void {
    this.syncBase()
    if (this.state === next) {
      this.enteredAt = Date.now()
      return
    }
    this.state = next
    this.enteredAt = Date.now()
  }

  private touch(): void {
    this.lastActivityAt = Date.now()
  }
}
