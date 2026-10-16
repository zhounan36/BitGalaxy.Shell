# 单 NSIS 合包（P7）：可选安装 SSH / SQL / 串口 / PET 组件

## Context

四 EXE 架构（P4-P6 已完成）：主程序 `bgshell-desktop.exe` + 三工具壳 `bgshell-ssh/sql/serial.exe`，全部**平铺同目录**（`spawn_sibling_exe`/`ai_base` 按 `exe 同级` 解析）。目前只有 desktop 配置了 NSIS bundle，三壳 `bundle.active=false`（裸 EXE），尚无合包安装包（P7 待做）。

**目标**：单个 NSIS 安装包，安装时可选 SSH / SQL / 串口 / PET 四个组件；卸载时同样可选卸载各组件与用户数据。

**已验证的关键事实**：
- NSIS 工具链已缓存：`%LOCALAPPDATA%\tauri\NSIS\makensis.exe`（tauri CLI 之前打过包自动下载的，3.x，含 MUI2/LogicLib 标准库）
- 前端资产**嵌入 EXE**（tauri v2 frontendDist 编译进二进制），`ui/` 不需要进安装包
- SQL 驱动：SQL 壳 `bundle.resources` → 运行时按 exe 同级 `drivers/{name}/host.exe`（tauri-build 拷到 `target/release/drivers/`）
- PET 资源：`<安装目录>/resources/pet-packs/builtin-whale`（[lib.rs builtin_pack_root](d:\sorce3\BitGalaxy.Shell\apps\desktop\src-tauri\src\lib.rs#L82-L110)），**缺目录自动降级**无精灵形态 → PET 组件可整体拆分
- 用户数据：`%APPDATA%\bitgalaxy-shell`（hosts/chat/sql 连接等 SQLite）

**方案**：自写完整 NSIS 脚本（不依赖 tauri 模板定制，无版本耦合），用已缓存的 makensis 编译。`tauri build` 不再用于出安装包（各壳直接 `cargo build --release`，图标/版本资源由 tauri-build 在 build.rs 阶段嵌入）。

## 实施步骤

### 1. 新建 `scripts/prep-pack.mjs`（产物汇总）

从各 `apps/*/src-tauri/target/release/` 与源目录汇总到 `target/pack/`：

```
target/pack/
  bgshell-desktop.exe
  bgshell-ssh.exe
  bgshell-sql.exe
  bgshell-serial.exe
  drivers/mysql/host.exe, drivers/postgres/host.exe, drivers/sqlite/host.exe   ← 从 apps/sql/src-tauri/target/release/drivers/ 或 prep-drivers 产物
  resources/pet-packs/**/*                                                     ← 从 apps/desktop/src-tauri/resources/pet-packs/（源即产物，src-tauri 下非 gitignore）
  MicrosoftEdgeWebview2Setup.exe                                                ← 缓存 target/webview2/，无则从 https://go.microsoft.com/fwlink/p/?LinkId=2124703 下载
```

- 复用 `scripts/prep-drivers.mjs` 的输出位置（先读它确认 drivers 在 sql 壳 target 下的实际路径）
- exe 路径：各壳 `target/release/{crate-name}.exe`（先读各 Cargo.toml 确认产物名是 bgshell-ssh 还是别的）
- WebView2 bootstrapper 下载一次缓存，离线时存在即跳过

### 2. 新建 `scripts/installer.nsi`（核心交付物，约 350 行）

- **基础**：`RequestExecutionLevel user`（currentUser）；MUI2 + `!insertmacro MUI_LANGUAGE "SimpChinese"` + "English"；`LogicLib.nsh`
- **页面**：欢迎 → 组件页 → 目录 → 安装 → 完成；卸载：确认 → **组件页** → 卸载 → 完成
- **组件 Section**（安装页，全部默认勾选）：
  - `"-核心程序"`（ReadOnly 必装）：`bgshell-desktop.exe`
  - `"SSH 终端"`：`bgshell-ssh.exe`
  - `"SQL 客户端"`：`bgshell-sql.exe` + `drivers\`（约 8MB）
  - `"串口调试"`：`bgshell-serial.exe`
  - `"PET 桌宠资源"`：`resources\pet-packs\`（体积大头）
- **WebView2**：注册表检测 `HKLM/HKCU "SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"`（version ≥ 90 简化为存在即过）；缺失 → 安装组件前 `ExecWait '"$INSTDIR\MicrosoftEdgeWebview2Setup.exe" /silent /install'`（bootstrapper 装完删除）
- **安装前**：`taskkill /f /im` 四个 exe（静默失败，升级/覆盖场景）
- **快捷方式**：开始菜单文件夹 `BitGalaxy Shell`（主程序必建；组件按勾选建）+ 桌面主程序快捷方式
- **注册表**：`HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\BitGalaxy Shell"`（UninstallString/DisplayIcon/DisplayVersion/EstimatedSize/Publisher）
- **卸载组件页**：按 `$INSTDIR` 文件存在性生成 Section（bgshell-ssh.exe 存在 → 显示"SSH 终端"勾选项）；`resources\pet-packs` 存在 → 显示 PET 项；最后"残留清理"项：删除快捷方式/注册表项/**可选勾选删除 `%APPDATA%\bitgalaxy-shell` 用户数据**（默认不勾，防误删）
- **输出**：`dist/BitGalaxy-Shell_<version>_x64-setup.exe`（版本从 apps/desktop/src-tauri/tauri.conf.json 读出注入 `!define VERSION`，用 `-DVERSION=x.y.z` 传参）

### 3. 根 `package.json` 新增 scripts

```
"desktop:pack": 串联 → build:ui → prep-assets → prep-drivers → cargo build --release（四壳）→ prep-pack → makensis
```

具体形式（npm workspace，PowerShell 兼容）：一个 `scripts/build-installer.mjs` 用 child_process 串起来（cargo 用 `--workspace`？先读四壳 Cargo 结构——可能已是 workspace members，一条 `cargo build --release -p bgshell-desktop -p bgshell-ssh -p bgshell-sql -p bgshell-serial` 即可），最后调 `makensis`（路径 `%LOCALAPPDATA%\tauri\NSIS\makensis.exe`，env 可覆盖 `BGSH_NSIS`）。

### 4. 明确不改动的文件

四个 `tauri.conf.json` 全部不动（desktop 的 nsis bundle 配置留着无妨，我们不再调 tauri build 出包）；`prep-assets.mjs`、`prep-drivers.mjs` 原样复用。

## 关键决策

| 决策 | 理由 |
|---|---|
| 自写 NSIS 而非定制 tauri 模板 | 官方模板拉取失败（网络），且定制模板与 tauri 版本强耦合；自写完全控制组件页/卸载组件页，工具链（makensis）已本地缓存 |
| currentUser（per-user）安装 | tauri v2 默认一致；无需管理员权限；$LOCALAPPDATA\Programs 下写文件 |
| 前端不进包 | 已嵌入 EXE（删除 devUrl 后 plain cargo build 用内嵌资产的既有结论） |
| PET = 资源组件 | builtin_pack_root 缺目录降级已验证；体积大头拆出最划算 |
| 卸载按文件存在性判断组件 | 不需要注册表记组件状态，简单可靠（重装/覆盖场景自然正确） |
| WebView2 bootstrapper 进包 | 离线环境也能装；~2MB 代价可接受 |

## 边界情况

- **升级安装**：安装前 taskkill 全部四个进程；文件覆盖（SetOverwrite on）
- **只装部分组件**：托盘/桌宠菜单拉起未装工具 → `spawn_sibling_exe` 报「找不到」——现状即如此（错误提示），本次不改 Rust 侧（可后续优化：菜单项检测文件存在性禁用）
- **卸载时程序在跑**：卸载前同样 taskkill
- **重复卸载**：文件不存在时 Section 自动跳过；注册表 DeleteRegKey 幂等
- **bootstrapper 下载失败**（离线首次构建）：提示手动放置路径后退出，缓存存在即跳过
- **四壳 exe 产物名**：以各壳 Cargo.toml `[package] name` 为准，prep-pack 实现前先读确认（bgshell-ssh vs ssh 壳 crate 名）

## 验证

1. 全量构建：`npm run desktop:pack` → 产出 dist 下 setup.exe
2. **干净安装**（本机卸载旧版后）：只勾核心+SSH → 验证：安装目录只有 desktop/ssh exe；托盘拉 SSH 正常；拉 SQL/串口报找不到（预期）；桌宠无精灵（预期降级）
3. **全量安装**：四组件全勾 → 四工具可拉起、SQL 查询用驱动、桌宠有精灵
4. **组件补装**：重跑安装包只勾 SQL → drivers/ 落盘、SQL 可用
5. **卸载**：组件页只勾 SSH 卸载 → ssh exe 消失其余保留；全卸载（勾删数据）→ 目录/快捷方式/注册表/APPDATA 清干净；不勾删数据 → APPDATA 保留
6. 升级覆盖：装旧版再装新版，进程被杀、文件被覆盖
