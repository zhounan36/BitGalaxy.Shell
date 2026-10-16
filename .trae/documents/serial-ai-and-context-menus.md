# 串口调试平台：AI 功能 + 右键菜单强化

## Context

串口工具壳（apps/serial，独立 EXE）目前只有基础调试能力：xterm 控制台 + Hex 监视/快捷发送/信号控制面板，**没有任何右键菜单**；AI 侧栏（AiSidebar，连主程序 7777 基座）虽已挂载但未与串口数据打通。本次为串口调试平台补齐 AI 联动与三处右键菜单，全部为 **packages/workbench-ui 纯前端改动**，复用 SQL 面板已验证的 askAi 一次性问答通道与终端的问 AI 桥接，零后端变更。

**已确认的范围**（用户拍板）：
- AI 形态 = 前端联动（askAi + requestAiAsk），不做 AI 代理串口工具
- 右键菜单三处全做：xterm 控制台、Hex 监视条目、快捷发送预设

**CSS 依赖已验证**：`term-menu`/`term-ask-card` 样式在 [index.css](d:\sorce3\BitGalaxy.Shell\packages\workbench-ui\src\index.css#L406-L637) 定义，[serial/main.tsx](d:\sorce3\BitGalaxy.Shell\packages\workbench-ui\src\serial\main.tsx#L5-L6) 已引入 index.css 与 chat.css（`chat-md` markdown 渲染类可用），antd ConfigProvider 环境完备 → **零新增 CSS 文件**。

## 实施步骤

### 1. 新建 `packages/workbench-ui/src/components/SerialAiModal.tsx`（约 130 行）

仿 [SqlAiModal.tsx](d:\sorce3\BitGalaxy.Shell\packages\workbench-ui\src\components\SqlAiModal.tsx) 结构，导出：

- `SERIAL_AI_SYSTEM`：资深嵌入式/串口通信专家提示词；精通帧结构分析（帧头/长度域/命令字/数据域/校验和/CRC8/CRC16/异或/累加和、ASCII 行协议、Modbus RTU/ASCII、AT 指令、NMEA）；回答用中文、准确简洁；分析帧时逐字节标注偏移与含义、指出疑似校验算法；生成指令时只输出指令本身
- `interface SerialAiBlock { content: string; hex: boolean }`
- `extractSerialBlocks(text)`：正则提取代码块；`lang === 'hex'` → hex 块、`'text'` → 文本块；无语言标注时用「纯 hex 字符+空格且去空格后偶数长度」启发式判定 hex（对齐 SqlAiModal 的 SQL_HEAD_RE 思路）
- `SerialAiModal({ open, title, loading, error, text, insertLabel, onInsert?, onClose, autoCopy })`：恒 markdown 渲染（ReactMarkdown + chat-md 类）；可插入内容 = `blocks[0]`；footer = 填入按钮（onInsert 存在且 blocks 非空时）+ 复制 + 关闭；autoCopy 新结果自动进剪贴板（复用 [clipboard.ts](d:\sorce3\BitGalaxy.Shell\packages\workbench-ui\src\api\clipboard.ts) 的 writeClipboardText）

### 2. 改造 `packages/workbench-ui/src/components/SerialDebugPane.tsx`（主文件）

#### 2a. 新增 import

`askAi`（../api/askAi）、`requestAiAsk`（../api/aiBridge）、`readClipboardText`/`writeClipboardText`（../api/clipboard）、`SERIAL_AI_SYSTEM`/`SerialAiModal`（./SerialAiModal）、antd `Menu`/`Modal` 及图标（RobotOutlined、CopyOutlined、EditOutlined、DeleteOutlined 等）。

#### 2b. 新增状态

```ts
// 控制台右键（自定义 term-menu）
const [termMenu, setTermMenu] = useState<{ x, y, hasSelection, selText } | null>(null)
// 问 AI 卡片（term-ask-card 浮层）
const [ask, setAsk] = useState<{ selection: string; question: string } | null>(null)
// Hex 条目 / 预设条目右键（antd Menu + 遮罩，仿 SqlEditor）
const [entryMenu, setEntryMenu] = useState<{ x, y, entry: MonitorEntry }>()
const [presetMenu, setPresetMenu] = useState<{ x, y, preset: SendPreset }>()
// AI 结果弹窗（parse/generate 两动作共用）+ 序号防串台
const [ai, setAi] = useState<{ action: 'parse' | 'generate'; loading; text?; error?; genEnding? }>()
const aiSeqRef = useRef(0)
// AI 生成输入弹窗 + 预设编辑弹窗
const [genOpen, setGenOpen] / [genDesc, setGenDesc] / [genHex, setGenHex] / [genEnding, setGenEnding] / [genWithContext, setGenWithContext](默认 true)
const [editPreset, setEditPreset] = useState<SendPreset | null>(null)
```

#### 2c. 抽公共函数 `formatLogLine(e)`

`${fmtTime(e.ts)} ${e.dir.toUpperCase()}  ${toHex(e.data, 4096)}  ${toAscii(e.data, 4096)}`——「复制整行」与记录导出共用，并替换 toggleRecord 内重复的 map（纯重构）。

#### 2d. 控制台右键菜单（照抄 [TerminalPane.tsx](d:\sorce3\BitGalaxy.Shell\packages\workbench-ui\src\components\TerminalPane.tsx#L260-L374) 三段式）

1. 双栏容器（L432）加 `position: relative`（term-ask-card absolute 锚点）；xterm 宿主 div 加 `onContextMenu`：`e.preventDefault()` + `termRef.current?.getSelection()` 快照（截断 8000 字符）+ `setTermMenu(...)`
2. 关闭 effect：window `mousedown`(capture, closest('.term-menu') 排除) / `Escape` / `wheel`(capture) / `blur` / `resize` → 关菜单
3. menuStyle 边界钳制：`left: Math.max(8, Math.min(x, innerWidth - 200))`，top 同理
4. 菜单项：**复制**（disabled: !hasSelection；写剪贴板 + clearSelection + focus）/ **问 AI**（disabled: !hasSelection → setAsk）/ **粘贴**（readClipboardText 非空 → `term.paste(text)`——粘贴即发送，走 onData → sendInput 路径，tx 记录/统计/回显自动正确）/ 分隔线 / **全选**（selectAll，保持菜单更新 hasSelection）/ **清屏**（term.clear()，Hex 监视不动）
5. 问 AI 卡片（term-ask-card，照抄 TerminalPane L579-610）：term-ask-sel 显示选区（>400 字符截断）+ textarea（Enter 发送 / Shift+Enter 换行 / Esc 取消）。提交：`requestAiAsk({ hostName: `${portName}@${baud}`, selection, question })`——baud 用 state（当前实际波特率），AiSidebar 自动唤起

#### 2e. Hex 监视条目右键（antd Menu + 遮罩）

1. 条目 div 加 `onContextMenu` → `setEntryMenu({ x, y, entry })`
2. 共享遮罩（entryMenu || presetMenu 时渲染，onClick/onContextMenu 关闭）+ Menu position:fixed，**left 钳制 `Math.min(x, innerWidth - 220)`**（面板在窗口右侧 420px，SqlEditor 没加是因为编辑器在左侧，此处必须加）
3. 菜单项：复制 Hex（toHex limit 4096）/ 复制 ASCII / 复制整行日志（formatLogLine）/ 复制时间戳 / divider / **AI 解析此数据**（RobotOutlined）→ startAiParse(entry)

#### 2f. `startAiParse(entry)`

`++aiSeqRef` 序号防护（回包时 seq 不匹配则丢弃，防串台）→ `setAi({ action:'parse', loading:true })` → `askAi(parsePrompt, SERIAL_AI_SYSTEM)`。

parsePrompt 要点：逐字节标注偏移与含义、判断协议类型与帧结构（帧头/长度/命令字/数据域/校验）、指出疑似校验算法与计算范围、ASCII 行协议/AT 指令说明语义；附上下文（方向/时间戳/字节数）+ `toHex(data, 1024)` + `toAscii(data, 1024)`（超限注明已截断）。SerialAiModal 展示，无 onInsert、autoCopy=false。

#### 2g. 快捷发送预设右键 + AI 生成指令

1. 预设条目 div 加 `onContextMenu` → `setPresetMenu({ x, y, preset })`
2. 菜单项：**立即发送** / **编辑预设**（EditOutlined）/ 复制内容 / **AI 生成指令**（RobotOutlined → setGenOpen(true)）/ divider / **删除**（danger，直接删 + message.success，与现有 ✕ 按钮行为一致，不加确认）
3. **编辑 Modal**（受控组件，不用 Form）：label Input / content TextArea（placeholder 随 hex 切换）/ hex Switch / ending Select。onOk：`encodePayload` 试算校验（非法 hex → warning 不关窗）→ `savePresets(presets.map(...))`（id 不变保持 key 稳定）
4. **AI 生成输入 Modal**：genDesc TextArea（placeholder 举例「Modbus RTU 读保持寄存器：从站 1，起始 0x0000，读 2 个」）+ genHex 格式选择 + genEnding 行尾选择（复用现有四选项）+ genWithContext 开关（附最近收发数据）
5. `startAiGenerate()` prompt：需求 + 输出格式说明（hex 空格分隔如 `a1 b2 0d` / 纯文本）+ 明确「不含行尾字符，发送时自动追加」+ genWithContext 时附 entries.slice(-6) 的 toHex(limit 64) 摘要（总长截断约 2000 字符）+ 「只输出指令本身：hex 用 ```hex 块、文本用 ```text 块」
6. 结果经 SerialAiModal：`onInsert=(b) => { setDraft(b.content); setDraftHex(b.hex); setDraftEnding(ai.genEnding ?? 'cr'); setAi(undefined) }`、`autoCopy=true`（生成即复制）；发送按钮现有的 encodePayload 校验兜底，不重复校验
7. **常驻入口**：「存为预设」旁加 `RobotOutlined`「AI 生成」按钮（解决空列表时不可达）

#### 2h. SerialAiModal 渲染（组件尾部）

按 ai.action 区分 title（AI 解析数据帧 / AI 生成指令）与 onInsert 有无。

### 3. 明确不改动的文件

askAi.ts、aiBridge.ts、AiSidebar.tsx、ChatPanel.tsx、SerialApp.tsx、serial/main.tsx、index.css、chat.css——基础设施全部现成，纯消费方接入。

## 关键决策

| 决策 | 理由 |
|---|---|
| 控制台用自定义 term-menu、条目用 antd Menu | 终端场景需 wheel/blur/resize 关闭菜单（TerminalPane 成套逻辑），样式已确认可用；面板条目用 antd Menu 表达力强（图标/divider/disabled），SqlEditor 模式成熟 |
| SerialAiModal 独立成文件 | 提取逻辑（hex 块 vs SQL 头正则）、onInsert 语义（{content,hex} vs 字符串）不同，泛化 SqlAiModal 反而耦合 |
| 预设删除直接删 | 与现有 ✕ 按钮同行为，localStorage 低风险数据 |
| aiSeqRef 序号防串台 | askAi Promise 不可取消，旧请求回包会污染新请求状态（SqlEditor 现存隐患，串口侧一次做对） |
| 粘贴走 term.paste() | 触发 onData → sendInput 全链路，tx 记录/统计/本地回显自动正确 |

## 边界情况

- 选区右键时快照（菜单期间选区可能消失）
- entry.data 超大：AI 解析截断 1024 并注明；复制整行 4096 对齐记录导出
- antd Menu 左侧钳制（面板靠右，防溢出）
- 空剪贴板粘贴静默跳过；串口断开后仍可 AI 解析历史条目（合理，不拦）
- 基座未运行：askAi 内部自动 spawn_ai_base 拉起或返回可读错误（弹窗 error 分支）

## 验证

静态检查：
```
pnpm -F @bgshell/workbench-ui build   # tsc --noEmit && vite build
```

手动验证（串口 EXE 或 dev；无设备时 UI 分支均可验证）：
1. 控制台右键：菜单弹出/复制 disabled 翻转/问 AI 卡片（Enter/Esc）→ AiSidebar 消息头为【来自终端「COM3@115200」的提问】；粘贴内容被发送（Hex 监视出现 TX、统计增加）；清屏后屏幕空但监视保留
2. 菜单边界：右下角右键不溢出；滚动滚轮菜单关闭
3. Hex 条目右键：四种复制内容正确（整行与记录导出格式一致）；AI 解析 → markdown 结果
4. 预设右键：立即发送/编辑（非法 hex 拦截）/复制/删除即时生效且刷新后持久化正确
5. AI 生成：描述「发送 AT+RST」文本格式 → 填入发送框三态（draft/draftHex/draftEnding）联动；hex 场景 autoCopy 生效
6. 串台防护：AI 请求未回时再触发另一个，弹窗只显示后者
7. 回归：快捷发送/定时重发/记录导出（formatLogLine 重构后逐字节一致）/信号控制/页签增删
