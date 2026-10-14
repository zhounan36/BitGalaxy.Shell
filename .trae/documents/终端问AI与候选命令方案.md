# 终端「问AI」与 AI 候选命令方案

> 状态：已完成（cargo check ✅ / npm run build ✅；待真机验收）
> 来源：用户需求（两条原始消息），会话中断后依据本文档恢复执行

## 一、原始需求（原文）

1. 「SSH终端右键添加一个问AI的按钮，功能是当我鼠标框选了一长串日志文字后，就能够自动加入AI的对话中，根据我的问题回复我。以及提供指令列表，比如AI不执行指令，那么AI把将要执行的指令单独列成一个列表在我的浮窗一边，然后我点击那个列表的指令，就自动复制粘贴上我的SSH终端上，只需要我敲回车执行。」

2. 「也可以不是悬浮窗，就在Ssh终端窗口就像，主要是我发现Ai很多时候没有执行的权限，尽量在Ai对话助手中也不要让Ai直接执行代码，而是使用代码这个候选列表让用户执行。」

## 二、需求理解与拆解

| # | 需求点 | 决策 |
|---|---|---|
| R1 | 终端右键「问AI」 | 框选日志 → 右键菜单「问 AI」→ 弹输入框写问题 → 选区+问题自动进入 AI 对话 |
| R2 | AI 对话入口 | 桌宠聊天面板（宠物窗 ChatPanel，常驻挂载）；回答显示在对话里 |
| R3 | 指令候选列表 | **放在 SSH 终端窗口内**（第二条消息修正：不悬浮窗）——工作台右下角浮动面板 |
| R4 | AI 不直接执行代码 | 从 LLM 工具集**移除** exec_command / console_command / local_exec；新增 `propose_command` 工具让 AI 提交候选命令 |
| R5 | 点击候选 → 粘贴执行 | 点击命令 → `term.paste()` 粘贴到对应终端（括号粘贴模式，**不自动执行**）→ 用户敲回车 |
| R6 | 本机命令 | target=local 的命令只复制到剪贴板（不自动进终端），用户自己去 PowerShell 粘贴执行 |
| R7 | MCP 通道不受影响 | 执行工具仅从 LLM 工具集移除；`mcp_tool_defs()` 保持原样（外部 MCP 客户端仍可执行） |

### 核心安全/权限原则

- **执行权 100% 在用户手上**：粘贴永不自动补 `\r`；AI 只能"提议"，不能执行
- AI 被明确告知（system prompt）：命令尚未执行、不得编造输出、等用户贴回结果再继续分析
- host 不匹配时警告但不阻止（AI 给的主机名可能不精确）

## 三、技术方案

### 数据流

```
问AI：
  终端框选 → 右键「问 AI」→ 输入问题
  → Tauri 事件 ai://ask（主窗 emitTo('pet')，1.2s 补发 + uid 去重）
  → 宠物窗 ChatPanel 组合消息（主机名 + 选区代码块 + 问题）
  → { t:'ai', m:{ op:'chat' } } → AI 回复（busy 时排队，ai-result 后补发）

候选命令：
  AI 调 propose_command（LLM 工具集内）
  → ai.rs run_tool 拦截（不进 toolcore）→ 广播 ServerFrame::AiCommands
  → workbench 自己的 WS 就能收到（OutHub 广播所有连接，无需跨窗口转发）
  → AiCommandPanel 右下角浮层累积显示
  → 点击 → pasteRegistry(sessionId → term.paste) 粘贴 → 用户回车执行
```

### 协议（Rust `protocol.rs` / TS `packages/protocol`）

```rust
#[serde(rename = "ai-commands")]
AiCommands { m: AiCommandsM }

AiCommandM { id, command, purpose?, target: "ssh"|"local", host? }
AiCommandsM { chat_id, commands: Vec<AiCommandM> }
```

TS 侧对应 `AiCommand` 接口与 `{ t: 'ai-commands' }` 帧变体。

### 工具清单（`toolcore.rs`）

- `llm_tools_json()`：filter 掉三个执行工具 + push `propose_command` spec
  - 参数：command（必填单行）/ purpose / target(enum ssh|local) / host
- `mcp_tool_defs()`：不动

### AI 核心逻辑（`ai.rs`）

- `system_prompt()` 重写：「你没有直接执行任何命令的权限…必须 propose_command…不得假装已执行/编造输出…用户会贴回结果」
- `run_tool()` 开头拦截 propose_command → `emit_proposed_command()`
- `emit_proposed_command()`：解析参数（空命令返回 bad-args）→ 发帧 → 返回 `{ok, proposed:true, message:"尚未执行…"}` 给 LLM
- 走完整卡片事件（running→done）+ 审计 + SQLite 落库
- `tool_detail()` 加 propose_command → command；`tool_result_summary()` 加 proposed 分支

### 前端组件

| 文件 | 职责 |
|---|---|
| `src/api/aiBridge.ts`（新建） | requestAiAsk（emitTo pet + uid 去重复发）/ onAiAsk（订阅；dev 用 BroadcastChannel） |
| `src/components/TerminalPane.tsx` | 右键菜单「问 AI」（无选区禁用）；TermMenuState.selText 快照（截 8000 字符）；问题输入浮层（回车发送/Esc 取消）；模块级 pasteRegistry（sessionId→term.paste），导出 pasteIntoTerminal/pasteTargets |
| `src/components/AiCommandPanel.tsx`（新建） | 候选命令浮层（工作台右下角）：ssh 项按 host 匹配终端粘贴；local 项复制剪贴板；清空/关闭按钮 |
| `src/App.tsx` | onControl 加 case 'ai-commands' 累积候选；挂载 AiCommandPanel（传 terminals/activeKey）；TerminalPane 传 hostName |
| `src/pet/App.tsx` | 监听 onAiAsk → setPackOpen(false); setChatOpen(true)；把 payload 传 ChatPanel |
| `src/pet/components/ChatPanel.tsx` | incomingAsk prop：新 uid 时组合 user 消息文本并发送；busy 排队、ai-result 后补发 |
| `src/index.css` | `.term-ask-card`（问AI输入浮层）、`.term-ask-sel/input/send`、`.ai-cmd-panel/head/list/item/text/meta/purpose/btn` |

### 消息组合格式（ChatPanel 内）

```
【来自终端「{hostName}」的提问】
{question}

终端选区内容：
```
{selection}
```
```

（选区为空时省略代码块段；hostName 为空时省略主机名）

## 四、任务清单与进度

| # | 任务 | 状态 |
|---|---|---|
| T1 | protocol.rs：ai-commands 帧 + AiCommandM/AiCommandsM | ✅ 完成 |
| T2 | toolcore.rs：llm_tools_json 过滤 + propose_command spec | ✅ 完成 |
| T3 | ai.rs：system_prompt 重写 + run_tool 拦截 + emit_proposed_command + tool_detail/summary | ✅ 完成 |
| T4 | packages/protocol：AiCommand 接口 + 帧变体 | ✅ 完成 |
| T5 | aiBridge.ts（新建） | ✅ 完成 |
| T6 | TerminalPane.tsx：问AI 菜单/输入浮层/选区捕获/pasteRegistry | ✅ 完成 |
| T7 | AiCommandPanel.tsx（新建） | ✅ 完成 |
| T8 | App.tsx：case 'ai-commands' + 挂载面板 + hostName 透传 | ✅ 完成 |
| T9 | pet/App.tsx 监听 onAiAsk → 开聊天面板 | ✅ 完成 |
| T10 | ChatPanel.tsx：incomingAsk prop + 消息组合 + busy 排队 | ✅ 完成 |
| T11 | CSS：term-ask-* 与 ai-cmd-* 样式 | ✅ 完成 |
| T12 | 构建验证：cargo check ✅ + 前端 tsc/vite build ✅ | ✅ 完成 |

## 五、验收标准

1. 框选日志 → 右键「问 AI」→ 输入问题回车 → 桌宠聊天面板出现组合消息，AI 基于选区回答
2. 让 AI 执行命令（如「帮我看下 nginx 状态」）→ AI 调 propose_command → 工作台右下角出现候选命令列表
3. 点击候选命令 → 命令粘贴到对应主机终端（host 匹配）→ 未自动执行 → 用户回车后执行
4. target=local 命令点击后仅复制剪贴板并提示
5. AI 对话中不再出现 exec_command / console_command / local_exec 工具调用卡片
6. MCP 客户端 tools/list 仍能看到三个执行工具（行为不变）
7. cargo check + 前端构建通过
