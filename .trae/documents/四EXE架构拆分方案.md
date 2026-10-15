# "一包四 EXE"架构拆分方案

## 背景与目标

当前 `bgshell-desktop.exe` 单进程承载全部功能（SSH 工作台 + SFTP + AI 对话 + SQL 面板 + 串口 + 桌宠 + 托盘），单 WS 端口分发所有帧，模块间深度耦合（SQL 经 SSH 隧道连库、AI 工具复用 SSH 连接池、串口会话嵌在 SessionManager）。

**目标**：一个 NSIS 安装包内置四个 EXE，各自完整独立：
| EXE | 职责 | 端口 |
|---|---|---|
| `bgshell-desktop` | **AI 基座（后台服务，无主工作台窗）**：AI 对话引擎 + 远程工具（exec_command/SFTP）+ Pet + 托盘 + 主机档案管理 | 7777 |
| `bgshell-ssh` | SSH 终端 + SFTP 工作台（含拖出下载 /dl）+ 主机管理 + metrics | 7771 |
| `bgshell-sql` | SQL 客户端：**直连数据库**（用户名/密码，Navicat 习惯），驱动宿主随包 | 7772 |
| `bgshell-serial` | 串口工具（SerialDebugPane 为主体） | 7773 |

用户已拍板：三个工具应用是完整 Tauri 桌面应用（复用 workbench-ui 组件与风格）；SQL 移除 SSH 隧道与远端 SQLite（SFTP 下载）依赖，SQLite 只开本地文件；**AI 是基座服务**——三应用内嵌 AI 侧栏（复用 ChatPanel），支持右键框选界面数据作为上下文对话、展示 AI 工具调用列表；Pet 只是 AI 的形象，无 Pet 时 AI 能力不消失；AI 远程工具由 AI 基座自行执行（保留 SSH 引擎），不依赖 SSH 应用运行。

**2026-10-15 补充拍板（AI 基座「不单独存在」）**：
- 基座**移除主工作台窗口**，可见形态仅托盘 + 桌宠；托盘提供设置入口与退出
- 完整 AI 对话界面 = **三工具侧栏 + 桌宠面板双入口**（广播机制天然支持，按 uid 去重）
- 基座**不注册开机自启**；任一工具启动时检测 7777 未监听则静默 spawn（spawn_ai_base），用户从不手动打开基座

---

## 一、Rust 代码组织

### 1.1 根 Cargo workspace（新建）

历史顾虑（drivers 独立 workspace 是为不改变 desktop 构建方式）在本大改中失效，统一根 workspace 共享 target 与依赖缓存。根 `Cargo.toml` members：

```
crates/*          # 新建共享 crate（见下表）
apps/desktop/src-tauri   # 瘦身为 AI 基座壳
apps/ssh/src-tauri       # 新建薄壳
apps/sql/src-tauri       # 新建薄壳
apps/serial/src-tauri    # 新建薄壳
drivers/host-common, host-mysql, host-postgres, host-sqlite   # 目录不动，仅注册
```

> `scripts/prep-drivers.mjs` 产物路径需从 `drivers/target/release` 改为根 `target/release`。

### 1.2 共享 crate 划分（`crates/`）

| crate | 迁入内容（现 apps/desktop/src-tauri/src/） | 依赖 |
|---|---|---|
| `bg-core` | protocol.rs、wire.rs、db.rs、stores.rs、policy.rs、disk.rs | rusqlite/serde/uuid/chrono |
| `bg-ssh` | ssh.rs、pool.rs、session.rs（剥离 serial 分支）、sftp.rs、dl_http.rs、metrics.rs | bg-core |
| `bg-ai` | ai.rs、toolcore.rs、mcp.rs | bg-core、bg-ssh |
| `bg-sql` | sql/{mod,driver,driver_mgr,error}.rs（直连改造后，删 tunnel.rs） | bg-core |
| `bg-serial` | serial.rs + 从 session.rs 抽出的 SerialSessionManager（open kind=serial / serial_ctl / input / close 分支） | bg-core |
| `bg-server` | server.rs 泛化：`WsServer::start(port, origins, Dispatcher)`，`Dispatcher` 为 trait，各壳组装自己的路由表 | bg-core |

- 单测随模块搬迁（stores/session/sql 现有 35 个单测）。
- 四个 Tauri 壳 crate 很薄：窗口/托盘/single-instance/IPC 命令（dialog 文件选择、spawn_ai_base 等），setup 时建 Stores 并组装 Dispatcher。`--mcp-stdio` 子模式留主程序。
- bg-ssh 被两个 bin 依赖（SSH 壳 + AI 基座的 toolcore），源码一份、两处编译，可接受。

### 1.3 session.rs 拆分点

`SessionEntry` 精简为 SSH kind（shell/sftp/metrics）；`open` 中 `kind=="serial"` 分支、`serial_ctl`、串口相关字段整体移入 bg-serial 的 SerialSessionManager（serial.rs 本身不动，只换会话管理层）。

---

## 二、AI 基座跨进程接口

- **发现机制**：固定端口 7777（保留 `BGSH_PORT` 环境变量覆盖）。三个 Tauri 壳的 WebView Origin 均为 `http://tauri.localhost`，AI 基座现有 allowed_origins 天然放行；dev 模式 17177 已在白名单。
- **未运行兜底**：应用 AI 侧栏显示"AI 基座未运行"+ 拉起按钮 → 各壳 IPC `spawn_ai_base`：`current_exe().parent().join("bgshell-desktop.exe")` spawn（与托盘拉起三应用同一模式）。
- **上下文注入**：protocol（bg-core 与 @bgshell/protocol 同步）扩展 Ai op 新值 `"ask-context"`：`{uid, source: "terminal"|"sql-table"|"serial", context: 选区文本, question}`。AI 基座把 context 拼入 user 消息前缀。**aiBridge.ts 的 Tauri emitTo 同进程事件必须改为经 aiSocket 发 WS 帧**（跨进程后 emitTo 不可达）。
- **多客户端天然兼容**（已核实代码）：OutHub 广播所有连接，approvals/inputs 表按 req_id 路由、任一连接可答。前端 ChatPanel 按 uid 去重（现有 aiBridge uid 机制沿用）。

## 三、数据存储：按域拆库 + 迁移

单库多进程并发写隐患多，按域拆分（数据目录仍 `%APPDATA%\bitgalaxy-shell`，全部启用 WAL + `busy_timeout=2000`）：

- `hosts.db`：hosts + known_hosts（SSH 应用与 AI 基座共用，双方读写）
- `settings.db`：settings + audit_log
- `chat.db`：chat_messages（仅 AI 基座）
- `sql-connections.db`：SQL 应用专用，**新增 sql_connections 表**（id/name/type/host/port/user/password/database/sqlite_path/created_at/updated_at），行映射照抄 HostStore 模式；密码明文沿用 hosts 表先例（Secrets 是进程内存缓存无法跨启动）；新增 `ClientFrame::SqlConns` CRUD 帧照 Hosts 帧模式
- 串口应用无持久化需求（预设已在 localStorage）

**迁移**：主程序/AI 基座首次启动检测旧 `bitgalaxy.db`（db.rs 与旧 Node Agent 兼容的库）→ 按表拆到新库 → 旧库改名 `.migrated.bak`。回退点：若迁移失败保持只读旧库。

## 四、各应用改造要点

### SQL 直连（bg-sql）
- `SqlHandler::new` 删 `registry` 参数；`SqlSession` 删 `tunnel`/`lease` 字段；connect 删远端 SQLite 分支（本地 sqlite_path 直开）
- 删 sql/tunnel.rs、`op_fetch_sqlite` 及 dispatch 分支、SFTP 下载辅助、spawn_cleanup 中 lease release；error/driver/driver_mgr 不动
- 前端 SqlConnModal：sqlitePath 改 Tauri dialog 本地文件选择，删"远端 .db"交互，加"保存连接"（连 sql-connections.db）
- drivers 随 SQL 壳：prep-drivers.mjs 拷贝目标改为 `apps/sql/src-tauri/drivers/`，DriverManager"exe 同级 drivers/{name}/host.exe"解析逻辑不变

### SSH 应用（bg-ssh）
- 全量 SSH/SFTP/主机管理/TestHost/Probe/ConfirmHostKey/RetryAuth/Metrics 帧 + dl_http 拖出下载随迁
- 前端从现 App.tsx 抽 hosts + tabs + TerminalPane/SftpPane/DropUpload/Settings（删 SQL/串口入口）

### 主程序后台服务化（AI 基座）
- **移除主工作台窗口**：基座无独立应用界面，可见形态仅托盘 + 桌宠窗；设置 / 主机档案 / AI 对话面板 UI 移入桌宠窗（托盘「设置」打开桌宠窗对应面板）
- dispatch 删 Open/Close/Sftp/Sql/Serial*/Metrics/Probe/TestHost 分支，保留 Ai/AiApprove/Settings/Agent/Hosts
- serial/sftp/sql 代码不再链接进主 bin，主程序体积下降
- 托盘菜单加三项拉起工具 EXE（`std::process::Command` spawn）；MCP stdio 留主程序
- 基座不注册开机自启（移除现有 autostart 注册逻辑），由三工具 spawn_ai_base 按需拉起

---

## 五、前端组织（单 package 五入口 MPA）

- `packages/workbench-ui` 保持单 package，Vite rollupOptions.input 五入口：
  `index.html`（基座兜底页：桌宠关闭时的引导/拉起入口，不承载工作台）、`ssh/index.html`、`sql/index.html`、`serial/index.html`、`pet/index.html`（基座主 UI：桌宠 + AI 对话面板 + 设置 + 主机管理）
- App.tsx（593 行）拆分为 `src/apps/{desktop,ssh,sql,serial}/App.tsx` 四个入口 root；共享组件留 `src/components`；删除 `BGSH_FLAVOR`/`__BGSH_PET__` 精简版机制（五入口取代其概念）
- **双连接**：后端 WS URL 按入口注入——**每入口 HTML 内嵌 `<script>window.__BGSH_WS__="7771"</script>`**（vite define 是全局的，不能按入口区分；dev 与 prod 统一 `ws://127.0.0.1:${window.__BGSH_WS__}/ws` 直连，删 vite proxy，各后端白名单放行 17177）
- 新增 `aiSocket`（复用 AgentSocket 类，连 7777）供三应用 AI 侧栏；ChatPanel ws 依赖注入化，Pet 窗与侧栏各自按 uid 去重
- `scripts/prep-assets.mjs`：改为按入口分发 `dist/{ssh,sql,serial,pet}` + `dist/index.html` → 各壳 `apps/{x}/ui/`（每壳 tauri.conf.json `frontendDist: "../ui"`）

## 六、打包与构建

- **单 NSIS**：三个工具壳 `bundle.active=false` + `tauri build --no-bundle` 产裸 EXE → 新增 `scripts/prep-apps.mjs` 拷入 `apps/desktop/src-tauri/bins/` → 主程序 tauri.conf.json `bundle.resources` 增加 `"bins/bgshell-ssh.exe"` 等（沿 drivers host.exe 先例）
- **构建顺序约束**：tauri-build 对 resources glob 编译期 panic（无匹配即失败），prep-apps 必须先产出 bins 再 build 主程序
- 四份 tauri.conf.json 差异：identifier `com.bitgalaxy.shell.{desktop|ssh|sql|serial}`、productName/icon 各异；SQL 壳 resources 含 `drivers/*/host.exe`
- 快捷方式：MVP 靠托盘/主窗拉起；P7 用 NSIS installerHooks 追加开始菜单三项；卸载清理含 db 的 -wal/-shm
- 根 package.json：`desktop:build = build:ui → prep-assets → prep-drivers(sql壳) → prep-apps → tauri build(desktop)`；dev 按应用拆 `dev:ssh` 等（一个 vite dev server 五入口，各壳 devUrl 指向 `http://localhost:17177/{app}/`）

## 七、分阶段实施（每阶段可独立验证）

| 阶段 | 内容 | 验证 |
|---|---|---|
| P1 | 根 workspace + 六 crate 抽取，desktop 壳改依赖 bg-* 并组装全量 dispatch（**行为不变**） | cargo check/test、现有 desktop 全功能手动回归 |
| P2 | SQL 直连改造 + sql_connections 表/帧 + apps/sql 壳 + sql/App 入口 | 直连 MySQL/PG、本地 SQLite、连接保存/恢复 |
| P3 | SerialSessionManager 抽取 + apps/serial 壳 | 串口开合/DTR/RTS/热插拔/预设 |
| P4 | apps/ssh 壳 + ssh/App 拆分，dl_http 随迁 | SSH/SFTP/上传/拖出下载/metrics 回归 |
| P5 ✅ 2026-10-15 | aiSocket 打通（ChatPanel socket 注入化）、AiSidebar 共享组件（FAB + 基座闸门 + 拉起）、spawn_ai_base（bg-core::ai_base + 三壳 IPC）、ai-commands 面板随 SSH 壳、askAi 失败自动拉起、基座 single-instance --silent 抑制弹窗 | 双客户端（Pet+侧栏）同收流式、任一处审批、未运行可拉起 |

> P5 实施说明：原计划的 `ask-context` 新帧**取消**——终端「问AI」改为投递到本窗 AI 侧栏（emitTo 目标按 `__BGSH_WS__` 入口分流：工具壳发本窗 main、工作台发 pet），上下文仍由 ChatPanel `composeAskText` 拼装后走既有 `op:"chat"` 帧，基座零改动；跨端同步由 OutHub 广播天然覆盖（桌宠与侧栏同收流式）。
| P6 ✅ 2026-10-16 | 基座后台服务化：删主工作台窗、dispatch 删数据面分支（保留 Ai/审批/设置/Agent/Hosts-直连 HostStore）、托盘三项拉起三工具、去自启、删 BGSH_FLAVOR 精简版机制、桌宠菜单改工具拉起+设置入口（SettingsModal variant=pet）、串口壳补 write_text_file | 基座无主窗仍全功能、hosts/settings/agent 帧冒烟通过、三工具可拉起 |
| P6.5（下轮） | 按域拆库：hosts.db / settings.db / chat.db + 旧 bitgalaxy.db 迁移（**勿漏 bg-ai/mcp.rs**）；桌宠资产外置（builtin-whale → bundle.resources，运行时缺包则纯托盘+聊天窗）——即「Pet 可选安装」的技术前提 | 拆库后四壳并发读写无锁冲突；无 pet 资源时 AI 能力不消失 |
| P7 | 单 NSIS 合包、prep-apps.mjs、快捷方式 hook、卸载清理 | 全新安装后四 EXE 互拉、升级覆盖 |

## 风险与回退

- P1 搬迁面最广，靠"行为不变回归"卡关，可按 crate 粒度分 PR 回退
- drivers 并入根 workspace 改变 target 路径，prep-drivers.mjs 必须同 PR 修改
- hosts.db 多进程写冲突靠 WAL+busy_timeout 兜底；若锁残留可退化为"AI 基座只读 hosts"
- 四个 identifier 各异，single-instance 插件按 identifier 隔离，无冲突；WebView2 downloadBootstrapper 共享

## 关键文件

- [server.rs](file:///d:/sorce3/BitGalaxy.Shell/apps/desktop/src-tauri/src/server.rs)（dispatch 拆分与 bg-server 泛化核心）
- [session.rs](file:///d:/sorce3/BitGalaxy.Shell/apps/desktop/src-tauri/src/session.rs)（serial 分支剥离、SessionEntry 拆分）
- [sql/mod.rs](file:///d:/sorce3/BitGalaxy.Shell/apps/desktop/src-tauri/src/sql/mod.rs)（直连改造）
- [db.rs](file:///d:/sorce3/BitGalaxy.Shell/apps/desktop/src-tauri/src/db.rs)（按域拆库与迁移）
- [ws.ts](file:///d:/sorce3/BitGalaxy.Shell/packages/workbench-ui/src/api/ws.ts)（多连接参数化与 aiSocket）
- [aiBridge.ts](file:///d:/sorce3/BitGalaxy.Shell/packages/workbench-ui/src/api/aiBridge.ts)（emitTo → WS 帧）
- [vite.config.ts](file:///d:/sorce3/BitGalaxy.Shell/packages/workbench-ui/vite.config.ts)（五入口 MPA）
- [App.tsx](file:///d:/sorce3/BitGalaxy.Shell/packages/workbench-ui/src/App.tsx)（四入口 root 拆分）
