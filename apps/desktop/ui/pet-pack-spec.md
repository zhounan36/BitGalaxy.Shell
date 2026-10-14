# 桌宠动作包规范（Pet Pack Spec v2）

动作包是一个 zip 压缩包，用来给桌宠的**动作**提供自定义**动画**与**行为图**。
「动作」（语义）由应用状态机调度；「动画」（素材）与「行为」（何时播哪个动作）
完全由你决定——这实现了动画、动作、行为的三重解绑：你按本规范提供素材与规则，
应用负责执行。

- v2 新增：**多格式美术资源**（静态图 / 动图 / 视频）、**behavior 行为表**（自定义
  动作 key、触发器、转换规则）。
- **v1 包完全兼容**：无 `kind` 字段、无 `behavior` 节的旧包照常导入，按内置行为播放。

---

## 1. zip 结构

```
my-pack.zip
├── pack.json                 # 必须，位于 zip 根目录
├── idle_breath/              # 动作目录（按需提供，可缺省）
│   ├── idle_breath_00.png    # kind:"frames"：帧序列
│   └── idle_breath_01.png
├── wave/
│   └── wave.webm             # kind:"video"：单视频
└── hello/
    └── hello.gif             # kind:"image"：单图/动图
```

- 除 `pack.json` 与被 clip 引用的素材外，**不要放其它文件**（多余文件导入失败）；
- 素材放在对应动作目录 `<key>/` 下；`src` / `poster` 写**动作目录内的文件名**
  （如 `wave.webm`），实际落盘路径为 `<key>/<src>`（如 `wave/wave.webm`）；
- 只提供部分动作可以：**未提供的动作自动回落内置动画**。

> 内置鲸鱼娘也是按本规范落盘的标准动作包（`public/pet-packs/builtin-whale/`），
> 其 `pack.json` 可作为最完整的参考示例。

---

## 2. pack.json 全字段

```json
{
  "schema": 2,
  "id": "my-pet",
  "name": "我的宠物",
  "author": "你的名字",
  "version": "1.0.0",
  "states": {
    "idle_breath": { "kind": "frames", "frames": 8, "fps": 6, "loop": true },
    "wave":        { "kind": "video",  "src": "wave.webm", "loop": false },
    "hello":       { "kind": "image",  "src": "hello.gif", "loop": true }
  },
  "behavior": { "...": "见第 4 节" }
}
```

| 字段 | 约束 |
| --- | --- |
| `schema` | `1` 或 `2`（缺省视为 1）；v1 = 仅 frames 帧序列 |
| `id` | `[a-z0-9_-]{1,32}`，全局唯一（重复 id 拒绝，需先删旧包） |
| `name` | 非空字符串，列表展示名 |
| `version` | 必填非空 |
| `author` | 可选 |
| `states` | 非空对象；key = 动作名 `[a-z0-9_-]{1,24}`（v1 限 10 个固定 key，v2 放开自定义） |
| `behavior` | 可选；自定义行为图，缺省 = 内置默认行为 |

### 2.1 clip 三种形态（`states.*`）

| kind | 说明 | 字段 |
| --- | --- | --- |
| `frames` | 帧序列：`<key>/<key>_NN.<ext>`，NN 从 `00` 起两位补零、连续无跳号，同状态扩展名统一 | `frames`(1..48) `fps`(1..30) `loop` |
| `video` | 单视频文件（webm/mp4） | `src` `loop` `poster?`（海报图文件名） |
| `image` | 单静态图 / 动图（gif/apng/webp 动图原生播放） | `src` `fps?`（动图覆盖步进）`loop?` |

> `src` / `poster` 均为**动作目录内的文件名**（不含目录前缀），实际路径 `<key>/<src>`。

- **v1 兼容**：`states.*` 写平铺 `{ "frames", "fps", "loop" }`（不写 `kind`）等价
  `kind:"frames"`，帧扩展名默认 `.png`；
- `frames` 的帧扩展名可为 `.png .jpg .jpeg .webp .gif`（同一状态统一，用 `ext`
  字段声明，缺省 `png`）。

### 2.2 格式矩阵

| 格式 | kind | 透明 | 循环 | 适用 |
| --- | --- | --- | --- | --- |
| PNG / WebP / JPG | frames 或 image | PNG/WebP 有 | ✓ | 静态、逐帧 |
| GIF / APNG / WebP 动图 | image | GIF 1bit/APNG/WebP 有 | ✓ | 小体积动图 |
| WebM（VP9/VP8） | video | **✓ alpha 透明** | ✓ | 高质量复杂动画（推荐） |
| MP4 | video | ✗（黑底矩形） | ✓ | 背景类动画 |

---

## 3. 素材美术要求

- 角色类动画**背景透明**（PNG/WebP/APNG/WebM-VP9 alpha）；MP4 无透明，仅适合背景；
- 宽/高 ≤ 2048 像素；建议 512×512 以内；单文件 ≤ 15MB；
- **同一状态的素材取景一致**（角色在画布中的位置、大小一致），否则播放会抖动；
- 脚底（或底边中点）建议对齐画布同一锚点，动作间切换不跳位；
- 视频建议短小（2~8 秒）循环片段；oneshot 视频超出驻留时长会被提前截断。

---

## 4. behavior 行为表（自定义动作图，v2 新增）

「行为」= 何时播哪个动作。`behavior` 节让你不改代码就能重新编排宠物的全部行为。

```json
{
  "behavior": {
    "mode": "override",
    "config": { "eatingAfterMs": 30000, "eatingToSleepMs": 60000, "sleepAfterMs": 300000 },
    "actions": {
      "idle_breath": { "layer": "base" },
      "working":     { "layer": "base" },
      "wave":        { "layer": "oneshot", "priority": 1, "durationMs": 1500, "cooldownMs": 15000 },
      "angry":       { "layer": "oneshot", "priority": 3, "durationMs": 3000 }
    },
    "triggers": {
      "ws-open": "ready", "poke": "wave", "agent-error": "angry",
      "pointer-enter": "wave", "blink": "idle_blink", "drag-start": "drag"
    },
    "transitions": [
      { "from": "working", "afterMs": 30000, "to": "eating", "clock": "output" }
    ],
    "natural": { "active": "working", "idle": "idle_breath", "sleep": "sleep" }
  }
}
```

### 4.1 字段表

| 字段 | 说明 |
| --- | --- |
| `mode` | `override`（缺省，按 key 合并内置表）/ `replace`（全量替换，此时必须至少一个 `layer:"base"`） |
| `config` | 时间参数，各值 >0 且 ≤86400000：`eatingAfterMs` `eatingToSleepMs` `sleepAfterMs` |
| `actions` | 动作定义表，key `[a-z0-9_-]{1,24}`：`layer`（`base`/`oneshot`）、`priority`（0..10，oneshot 打断门控，高者打断低者）、`durationMs`（100..60000，oneshot 驻留）、`cooldownMs`（0..600000，触发冷却，如悬停招手） |
| `triggers` | 事件 → 动作 key（值必须在 `actions` 键集合内） |
| `transitions` | 定时转换：`from` 停留 `afterMs`（100..6000000）后切 `to`；`clock` = `enter`（缺省，从进入 from 计时）/ `output`（从最近输出计时）/ `activity`（从最近活动计时）。**不能自环、不能成环** |
| `natural` | 自然基线三槽：`active`（有会话）/ `idle`（无会话）/ `sleep`（断线空闲） |

### 4.2 触发器（trigger）全集（13 个）

| TriggerName | 事件含义 | 内置默认动作 |
| --- | --- | --- |
| `ws-open` | Agent 已连接 | ready |
| `ws-close` | Agent 断开 | sleep（进基础层） |
| `session-opened` | 会话打开 | working（进基础层） |
| `session-closed` | 会话关闭 | 自然回落 |
| `output` | 终端有输出 | working（唤醒，仅非活跃态） |
| `hostkey-unknown` | 待确认指纹 | need_input |
| `hostkey-confirmed` | 指纹已确认 | 自然回落 |
| `agent-error` | 连接/探针失败 | angry |
| `pointer-enter` | 鼠标悬停 | wave（待机态 + 冷却约束） |
| `poke` | 单击宠物 | wave（睡眠态先戳醒） |
| `drag-start` | 开始拖拽 | drag |
| `drag-end` | 结束拖拽 | 自然回落 |
| `blink` | 眨眼调度 | idle_blink（待机态） |

### 4.3 三个示例

**例 1 · 换个动作名（自定义 key）**：把「招手」改叫 `greet`，动画用视频：

```json
{
  "states": { "greet": { "kind": "video", "src": "greet.webm", "loop": false } },
  "behavior": {
    "actions": { "greet": { "layer": "oneshot", "priority": 1, "durationMs": 1500, "cooldownMs": 15000 } },
    "triggers": { "pointer-enter": "greet", "poke": "greet" }
  }
}
```

**例 2 · 自定义触发**：被戳时生气（而不是招手）：

```json
{
  "behavior": {
    "triggers": { "poke": "angry" }
  }
}
```

**例 3 · 自定义转换规则**：工作 10 秒就去吃饭（比内置 30 秒更勤）：

```json
{
  "behavior": {
    "transitions": [
      { "from": "working", "afterMs": 10000, "to": "eating", "clock": "output" }
    ]
  }
}
```

---

## 5. 导入校验（不满足即拒绝）

- zip ≤ 30MB，解压后 ≤ 80MB，条目数 ≤ 600；单素材文件 ≤ 15MB；
- 路径安全：拒绝 `..`、绝对路径、盘符、符号链接（zip-slip）；
- **扩展名 ↔ 文件头（magic）必须匹配**（防伪造扩展名）：
  PNG `89 50 4E 47…`、JPEG `FF D8 FF`、WEBP `RIFF….WEBP`、GIF `GIF89a/87a`、
  WebM `1A 45 DF A3`、MP4 `…ftyp`；图片另查尺寸 ≤2048；
- frames 的帧数严格匹配、编号连续、无多余文件；
- behavior：key 命名、trigger 枚举、priority/duration/cooldown 范围、
  triggers/transitions 引用的 key 必须存在、transitions 无环、replace 模式必有 base 层。

---

## 6. 兼容性

- **v1 包**（平铺 frames、无 behavior）：照常导入，动画按帧序列播放，行为 = 内置默认；
- **v2 包**：多格式动画 + 自定义行为图；
- 浏览器预览模式仅内置形象，自定义动作包需桌面版（Tauri）。

---

## 7. 使用方式

桌宠右键菜单 →「动作包管理」→「导入动作包（zip）」→ 选中你的 zip。
导入后点「启用」立即生效；可随时删除或「恢复内置」。
