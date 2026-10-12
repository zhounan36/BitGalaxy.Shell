# 方案：SQLite 存储迁移 + 配置页面优化

## Context（背景与动机）

当前所有配置散落在 dataDir 下的多个 JSON 文件中：
- `hosts.json`（HostStore — 主机档案与凭据）
- `known-hosts.json`（KnownHosts — SSH 指纹 TOFU）
- `settings.json`（desktop.ts — 仅 `openOnStart`）
- `audit-queue.jsonl`（AuditQueue — 审计日志）
- `ai.json`（AI 配置 — apiKey/baseUrl/model，已被删除）

唯一"配置页"是工作台侧边栏底部的 `AgentControls.tsx`——仅 2 个开关 + 退出按钮，无 AI 配置入口、无统一设置页。

**目标**：用 SQLite 统一存储全部配置，新建完整配置页面（AI 配置 / 通用设置 / 关于）。

## 技术选型：`node:sqlite`（Node 内置）

Node 24.21 内置 `node:sqlite`（DatabaseSync），无需 npm 依赖、同步 API、SEA 可用（内置模块不参与 esbuild 打包）。

## 实施步骤

### 1. 数据库层 `packages/connector-core/src/db.ts`（新建）

`Db` 类封装 `DatabaseSync`：
- 打开 `dataDir/bitgalaxy.db`
- 建表（WAL 模式，PRAGMA journal_size_limit）：
  ```sql
  hosts(id TEXT PK, name, host, port, username, auth_kind, password, private_key_path, passphrase, tags, created_at, updated_at)
  known_hosts(host, port, key_type, fingerprint, PK(host,port))
  settings(key TEXT PK, value TEXT)            -- JSON value
  audit_log(id INTEGER PK AUTOINCREMENT, ts, type, meta TEXT)
  ```
- `migrateFromJson(dataDir)`：首次打开时，若 JSON 文件存在且表空 → 导入全部记录 → 重命名 JSON 为 `.bak`
- `close()`

### 2. 存储类重写（保持公开 API 不变）

| 文件 | 改动 |
|------|------|
| `hostStore.ts` | 构造接 `Db`；list/get 同步查询；create/update/delete 用 prepared stmt（保留 async 签名兼容调用方 `await`） |
| `knownHosts.ts` | 构造接 `Db`；find/save 用 prepared stmt；`load()` 变空操作 |
| `auditQueue.ts` | 构造接 `Db`；`append()` 写 audit_log 表（保留 async 签名） |
| `settingsStore.ts`（新建） | key-value：`get(key)`/`set(key, value)`/`getAll()`/`delete(key)`；替代 settings.json + ai.json |
| `index.ts` | 导出 `Db`、`SettingsStore` |

### 3. 协议帧扩展 `packages/protocol/src/index.ts`

```ts
type SettingsOp =
  | { op: 'list' }
  | { op: 'get'; key: string }
  | { op: 'set'; key: string; value: unknown }
  | { op: 'delete'; key: string }

// ClientFrame 追加：
| { t: 'settings'; m: SettingsOp }

// ServerFrame 追加：
| { t: 'settings-result'; m: { ok: boolean; items?: Record<string, unknown>; value?: unknown; message?: string } }
```

### 4. transport-ws 接线 `packages/transport-ws/src/index.ts`

- `startAgentServer` 内创建 `Db` 实例（dataDir/bitgalaxy.db）→ 调 `migrateFromJson` → 传给 HostStore/KnownHosts/AuditQueue/SettingsStore
- 去掉 `await hostStore.load()` / `await knownHosts.load()`（同步 DB 无需）
- `handleControlFrame` 新增 `settings` case → 通过 SettingsStore 处理 list/get/set/delete
- `agentControl` 回调改为从 SettingsStore 读写 `openOnStart`（替代 desktop.ts 的 settings.json）

### 5. Agent 入口调整 `apps/agent/src/`

- `desktop.ts`：`loadSettings`/`saveSettings` 改为接 SettingsStore 参数（autostart 仍走注册表不变）
- `main.ts`：传入 SettingsStore 给 agentControl 回调

### 6. 配置页面 `packages/workbench-ui/src/components/SettingsModal.tsx`（新建）

antd `Modal` + `Tabs`，三个页签：

**通用**（替代 AgentControls）
- 开机自启 Switch（走 agent set-autostart）
- 启动时打开工作台 Switch（走 settings set `app.openOnStart`）
- 退出 Agent 按钮（走 agent shutdown）

**AI 助手**
- API Key（Input.Password）、Base URL、Model
- 保存按钮 → `settings set ai.apiKey/ai.baseUrl/ai.model`
- 加载时 → `settings list` 过滤 `ai.*` 前缀

**关于**
- Agent 版本、数据目录路径、DB 文件大小

### 7. 工作台接入

- `AgentControls.tsx`：简化为单个"设置"按钮（打开 SettingsModal），或直接移除由侧边栏底部按钮替代
- `App.tsx`：加 `settingsOpen` state，渲染 `<SettingsModal>`

### 8. 构建脚本 `scripts/build-exe.mjs`

- esbuild target `node20` → `node22`（`node:sqlite` 最低 Node 22.5）

## 迁移安全

- 首次 DB 打开自动从 JSON 导入（hosts/known_hosts/settings/audit）
- 导入后 JSON 重命名为 `.bak`（不删）
- 无 JSON 文件 → 空库启动
- 不回滚：SQLite 成为唯一存储后 JSON 不再写入

## 验证

1. `npm run build:node` — 全 TS 包编译通过
2. `npm run build:exe` — SEA 打包成功（`node:sqlite` 在 SEA 可用）
3. 启动 agent → 验证 hosts.json 数据迁移到 SQLite（查 bitgalaxy.db）
4. 工作台 → 设置弹窗 → 通用/AI/关于 三个页签正常加载保存
5. 主机增删改 → 持久化在 SQLite（重启后仍存在）
6. AI 配置（apiKey 等）保存后在 DB 中可查
7. 开机自启 / openOnStart 开关仍生效
