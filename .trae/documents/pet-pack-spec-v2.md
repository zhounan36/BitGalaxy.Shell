# 桌宠动作包规范 v2 · 多格式美术 + 用户自定义动作图

## Context

v1 已完成「动画与动作解绑 + zip 动作包 DIY」：帧序列 PNG、10 个固定动作 key、导入/启用/删除。本次扩展三点：

1. **更详细的动作包说明**——规范文档 v2 全字段表、格式矩阵、行为教程、示例；
2. **更多美术格式**——静态图 PNG/JPG/WebP、动图 GIF/APNG/WebP 动图、视频 WebM/MP4（WebM 透明）；
3. **更丰富的动画状态机管理**——做到**用户自定义动作图**：动作包可定义自己的动作 key、触发器映射、转换规则与时间参数，不改代码即可改行为（内置默认表兜底，v1 包照常工作）。

已拍板：全格式（静态+动图+视频）；用户自定义动作图；WebM 透明。

## 一、pack.json schema v2（向后兼容 v1）

### clip 多格式（`states.*` 判别联合）

```ts
type ClipSpecV2 =
  | { kind?: 'frames'; frames: 1..48; fps: 1..30; loop: boolean }  // <key>/<key>_NN.ext 帧序列（= v1 语义）
  | { kind: 'video'; src: string; loop: boolean; poster?: string } // webm/mp4 单文件
  | { kind: 'image'; src: string; fps?: number; loop?: boolean }   // png/jpg/webp/gif/apng 动图；fps=覆盖步进，静态=驻留
```

- 帧序列扩展名可为 `.png/.jpg/.jpeg/.webp/.gif`（同状态统一格式）；
- `video.src` / `image.src` 为包内相对路径；
- 无 `kind` 或 `kind:"frames"` = v1 行为（Rust 反序列化 `#[serde(default)]` 兼容旧平铺字段）。

### behavior 行为表（用户自定义动作图，重点）

```ts
interface BehaviorSpec {
  mode?: 'override' | 'replace'            // 默认 override=按 key 合并内置表；replace=全量替换
  config?: Partial<PetConfig>              // eatingAfterMs / eatingToSleepMs / sleepAfterMs 可被包覆盖
  actions: Record<string, {                // 动作 key 放开为 [a-z0-9_-]{1,24}
    layer: 'base' | 'oneshot'              // base=基础循环层；oneshot=叠加层（驻留后回落）
    priority?: number                      // 0..10，oneshot 打断门控（默认按内置表规则）
    durationMs?: number                    // 100..60000，oneshot 驻留时长
    cooldownMs?: number                    // 冷却（如 wave 15s）
  }>
  triggers: Partial<Record<TriggerName, string>>   // 事件 → 动作 key
  transitions?: Array<{ from: string; afterMs: number; to: string }>  // 定时转换（如 working 30s→eating）
}
type TriggerName = 'ws-open' | 'ws-close' | 'session-opened' | 'session-closed' | 'output'
  | 'hostkey-unknown' | 'hostkey-confirmed' | 'agent-error' | 'pointer-enter' | 'poke'
  | 'drag-start' | 'drag-end' | 'blink'
```

- 无 `behavior` 节 = 内置默认行为（v1 回归不变）；
- `mode:"replace"` 缺基础层动作 → 导入拒绝；Rust 校验 transitions 无环（DFS）。

## 二、machine.ts 数据驱动改造

- `PetMachine(config, behavior: BehaviorTable)`；导出 `DEFAULT_BEHAVIOR`（= 现 ACTION_PRIORITY/ACTION_MS/PetConfig/触发分支的表化，**值不变**）；`resolveBehavior(builtin, pack.behavior)` 合成最终表。
- `handle` 事件分支 → `triggers[ev.type]` 查表（`playAction`/`enterBase`）；`tick` → 遍历 `transitions`（from 停留 afterMs 后切 to）+ oneshot `durationMs` 到期回落；WAVE_COOLDOWN/DRAG_STUCK 移入表内 `cooldownMs`/固定兜底。
- `PetStateKey` → `string`（保留 `PetStateKeyV1` 联合类型作内置默认 key），`PetSnapshot.state: string`。
- 适配面：[usePet.ts](file:///d:\sorce3\BitGalaxy.Shell\packages\workbench-ui\src\pet\pet\usePet.ts)、[Bubble.tsx](file:///d:\sorce3\BitGalaxy.Shell\packages\workbench-ui\src\pet\components\Bubble.tsx)、[lines.ts](file:///d:\sorce3\BitGalaxy.Shell\packages\workbench-ui\src\pet\pet\lines.ts)（自定义 key 缺台词 → `GENERIC_LINES` 通用池兜底）、DebugPanel。

## 三、渲染层：PetSprite 泛化为 clip 渲染器

`ResolvedClip` 判别联合：`frames`（保留单 rAF 翻帧）/ `image`（`<img>`，GIF/APNG 原生播放）/ `video`（`<video muted loop autoplay playsInline>`）。两层交叉淡化骨架共存：

- oneshot video：`durationMs` 到期回落（以状态机为准，防视频超长）；`ended` 时 `loop=false` 驻留末帧；
- video `onError` / 加载失败 → 回落内置 clip；
- 透明 WebM 零特殊处理（Chromium 原生 alpha）；
- 内存：仅激活 clip 预载（frames 逐帧、image 预取），**video 不预载**，交叉淡化短暂共存后卸载 `src`。

Tauri asset protocol 对 webm/mp4 同 `convertFileSrc` 路径（Windows `http://asset.localhost`），无需额外配置。

## 四、Rust 校验扩展（pet_pack.rs）

- 扩展名白名单 + magic：PNG `89504E47`、JPEG `FFD8FF`、WEBP `RIFF..WEBP`、GIF `GIF89a`、APNG=PNG、webm `1A45DFA3`、mp4 `ftyp`；单文件 ≤15MB；图片仍查 IHDR ≤2048；视频只查 magic+大小（不读头）。
- behavior 校验：key 命名、trigger 枚举、priority 0..10、durationMs 100..60000、触发器/转换引用的 key 必须存在、transitions 无环、replace 模式必有 base 层动作。
- 测试：多格式 magic 通过/伪造扩展名拒绝、behavior 合法/非法 trigger/引用缺失/transition 环、v1 包（无 kind 无 behavior）回归。

## 五、文档 v2

[pet-pack-spec.md](file:///d:\sorce3\BitGalaxy.Shell\docs\pet-pack-spec.md) 扩写：目录树 → pack.json/behavior 全字段表 → **格式矩阵**（格式 × 透明/循环/oneshot/适用场景）→ **behavior 教程**（trigger 表 + 3 个示例：自定义动作名、自定义触发、转换规则）→ 多格式示例 → 兼容性（v1 照常、无 behavior=内置）→ 校验清单。[PetPackPanel.tsx](file:///d:\sorce3\BitGalaxy.Shell\packages\workbench-ui\src\pet\components\PetPackPanel.tsx) 折叠说明同步 v2 + `public/pet-pack-spec.md` 同步。

## 六、实施步骤

1. **P1 Rust**：pet_pack.rs（PackClip 枚举 + behavior 结构 + 多格式/规则校验 + 测试）→ `cargo test`
2. **P2 前端解析**：packRuntime.ts（ResolvedClip 判别联合、PetPackInfo 扩展 kind/files）+ builtinClips.ts（内置补 `kind:'frames'`）
3. **P3 行为表**：machine.ts 数据驱动重构（DEFAULT_BEHAVIOR + resolveBehavior + string 化 key）→ usePet/Bubble/lines/DebugPanel 适配
4. **P4 渲染**：PetSprite.tsx 三分支 clip 渲染器 + video 驻留/回落/失败兜底
5. **P5 文档+面板**：spec v2 + PetPackPanel 说明 + 模板 zip 升级（含 behavior 示例）

## 七、验证

- `cargo test`（apps/desktop/src-tauri）：旧 5 用例 + 新增多格式/behavior 用例；
- `npm run build`（packages/workbench-ui）+ `node scripts/prep-assets.mjs` 同步产物；
- 自制 v2 测试包（gif + 透明 webm + 自定义 behavior）端到端：Tauri 导入 → 自定义动作名/触发器生效、视频透明播放、混合格式缺省回落内置；
- v1 旧包回归导入（无 kind 无 behavior）行为不变。

## 风险

- video 解码失败/格式不支持 → onError 回落内置帧；
- `PetStateKey` string 化触及 PetSprite/Bubble/lines/usePet/DebugPanel 类型面，需全量 tsc；
- transitions 环（A→B→A）与自环在 Rust 导入期拦截；
- 双 video 交叉淡化内存峰值——短共存即卸载 src。
