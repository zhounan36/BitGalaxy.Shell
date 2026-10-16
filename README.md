# BitGalaxy Shell

开源的 SSH / SQL / 串口三合一开发者控制台，内置 AI 助手与桌面宠物。

基于 Tauri 2 构建：Rust 后端 + React 前端，单安装器内含四个独立 EXE（AI 基座 + 三个工具），当前面向 Windows 平台发布。

![License](https://img.shields.io/badge/License-MIT-blue.svg)
![Tauri](https://img.shields.io/badge/Tauri-2-orange.svg)
![Platform](https://img.shields.io/badge/Platform-Windows-informational.svg)

## 功能特性

### SSH 终端

- 真 PTY 终端（xterm.js）多标签工作台，窗口尺寸实时同步，长输出折行标注，专注模式（Ctrl+Shift+F）
- SFTP 文件管理：拖拽上传 / 下载、断点续传、传输预检（体积 / 磁盘 / 重名策略）、细粒度错误分类与建议
- 连接复用注册表：同主机共享一条 TCP，PTY / SFTP / exec / 监控多通道复用，空闲自动回收
- TOFU 主机密钥确认、看门狗检测半开连接、断线自动重连
- AI 侧栏：选中终端内容直接提问

### SQL 客户端

- 支持 MySQL / PostgreSQL / SQLite，驱动随安装包内置，开箱即用
- SQL 编辑器（CodeMirror）+ 结构树导航，双击表即开数据页签
- 表数据页签：服务端分页、远程列头排序、WHERE 筛选、行数统计、CSV 导出
- 表结构页签：字段明细、索引明细、DDL 视图；页签切换状态不丢失
- 驱动以独立宿主进程运行，单个驱动崩溃不影响主程序

### 串口调试

- 控制台收发：Hex / ASCII 双视图监视，带时间戳与偏移标注
- 预设指令管理：立即发送、编辑、AI 生成指令（自动联动 Hex 开关与行尾设置）
- 右键菜单：复制 Hex / ASCII / 整行日志，AI 解析串口数据（逐字节标注偏移、识别协议类型与校验算法）

### AI 助手

- 各工具内嵌 AI 侧栏，桌宠亦可直接对话
- 工具调用：远程执行命令、读写远端文件、SFTP 传输，全程可视化卡片
- 审批链路：敏感操作弹卡片人工批准（fail-closed 超时兜底）；sudo 提权密码经安全卡片索取，仅内存缓存不落盘
- 对话历史 SQLite 持久化，重启可恢复；支持多会话管理与删除
- 支持 MCP stdio，可接入自有工具链

### 桌面宠物

- AI 基座托盘常驻，桌宠承担对话入口
- 动作包自定义：帧序列 / 单图动图 / 视频（WebM 透明通道等），zip 导入 + 完整性与 zip 炸弹校验，热切换、缺失动作自动回落内置包

## 架构：一包四 EXE

```
BitGalaxy Shell（NSIS 单安装器，组件可选装）
│
├─ BitGalaxy Shell.exe     AI 基座：托盘后台服务 + 桌宠 + 内嵌 Agent
│                            └─ 托盘 / 桌宠拉起以下三工具
├─ BitGalaxy SSH.exe       SSH 终端 + SFTP 工作台
├─ BitGalaxy SQL.exe       SQL 客户端（驱动宿主进程随包内置）
└─ BitGalaxy Serial.exe    串口调试工具
```

四个 EXE 共享同一套 Rust workspace（`crates/bg-*`）与 React 工作台 UI（`packages/workbench-ui`），经 WebSocket 协议（`packages/protocol`）与后端通信。

## 安装

从 [Releases](https://github.com/zhounan36/BitGalaxy.Shell/releases) 下载安装器：

| 安装器 | 说明 |
| --- | --- |
| 全量版（约 109MB） | 含桌宠资源包 |
| 精简版（约 24MB） | 不含桌宠资源，其余功能完整 |

安装时五个组件（核心、SSH、SQL + 驱动、串口、桌宠）可按需勾选；默认 per-user 安装，无需管理员权限。

## 开发

环境要求：

- Node.js ≥ 20
- Rust stable（MSVC 工具链）
- NSIS 由 Tauri CLI 自动下载，无需单独安装

```bash
npm install          # 安装依赖（npm workspaces）

npm run desktop:dev          # 开发模式：UI dev server + tauri dev
npm run desktop:build        # 构建四个 EXE（release）
npm run desktop:pack         # 全量 NSIS 安装器（含桌宠）
npm run desktop:pack:slim    # 精简 NSIS 安装器（不含桌宠）
```

注意：tauri-build 对 SQL 驱动资源 glob 做编译期强校验，直接 `cargo build` 前必须先执行 `node scripts/prep-drivers.mjs`（`desktop:assets` / `desktop:dev` 等命令已内置此步骤）。

## 项目结构

```
apps/
  desktop/          # AI 基座主壳（托盘 + 桌宠 + Agent 服务）
  ssh/              # SSH 工具壳
  sql/              # SQL 工具壳
  serial/           # 串口工具壳
crates/
  bg-core/          # 核心共享（主机档案、密钥库、存储等）
  bg-ssh/           # SSH/SFTP 数据面（russh）
  bg-sql/           # SQL 会话与驱动宿主通信
  bg-serial/       # 串口数据面
  bg-ai/            # AI Agent（工具调用、审批、MCP）
  bg-server/        # WS 服务端
drivers/
  host-mysql/       # MySQL 驱动宿主（独立进程）
  host-postgres/    # PostgreSQL 驱动宿主
  host-sqlite/      # SQLite 驱动宿主
  host-common/      # 驱动宿主协议共享
packages/
  protocol/         # 前后端 WS 帧协议定义
  workbench-ui/     # React 工作台 UI（三工具共用）
scripts/            # 构建脚本（NSIS 合包、驱动准备、图标生成等）
```

## 技术栈

| 层 | 技术 |
| --- | --- |
| 桌面框架 | Tauri 2（WebView2） |
| 前端 | React 19 · TypeScript · Vite · Ant Design 6 · xterm.js · CodeMirror 6 |
| 后端 | Rust · tokio · russh / russh-sftp · serde |
| SQL 驱动 | 独立宿主进程（MySQL / PostgreSQL / SQLite） |
| AI | 内嵌 Agent（工具调用 + 审批链路）· MCP stdio |
| 安装器 | NSIS（组件可选装 / 可卸载） |

## 贡献

欢迎提交 Issue 与 Pull Request。

## License

[MIT](./LICENSE)
