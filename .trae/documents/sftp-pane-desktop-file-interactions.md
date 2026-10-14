# 文件管理桌面化交互：拖入上传 / 拖出下载 / 面板内移动 / 右键菜单 / 多选批量

## Context

SftpPane（[SftpPane.tsx](file:///d:/sorce3/BitGalaxy.Shell/packages/workbench-ui/src/components/SftpPane.tsx)）目前交互单一：按钮 + 行内操作列。且存在实际缺陷：Tauri 桌面模式下 OS 拖入**收不到文件**——Tauri `dragDropEnabled`（默认开）拦截了 WebView 的 HTML5 拖放，App.tsx 的全局 `onDragDropEvent` 只命中测试终端面板（[App.tsx#L292](file:///d:/sorce3/BitGalaxy.Shell/packages/workbench-ui/src/App.tsx#L292)），拖到文件面板无任何反应；HTML5 `onDrop` 路径只在浏览器宿主生效。

目标：像桌面系统一样操作文件——
1. **拖入** OS 文件/文件夹 → 上传到当前远端目录（原生 put-file/put-dir，断点续传）
2. **拖出** 远端文件到资源管理器/桌面任意文件夹 → 直接流式下载到该文件夹
3. **面板内拖动**远端条目到文件夹/面包屑 → 移动（尽力而为，WebView2 限制见风险节）
4. **右键菜单**（行菜单 / 空白区菜单 / 选区菜单）
5. **多选 + 批量**下载/删除/移动；移除行内按钮列，纯桌面化（用户已确认）

用户决策：面板内移动要（尽力而为）；移除按钮列；加多选批量。

## 核心技术方案

### 拖出下载 = HTML5 `DownloadURL` + Agent 端口上的纯 HTTP 流式端点

- WebView2（Chromium）支持拖出协议 `dataTransfer.setData('DownloadURL', 'mime:name:url')`：拖到 Explorer 文件夹时由下载管理器请求该 URL 并写入目标文件夹。
- URL 由后端签发能力令牌，形如 `http://127.0.0.1:7777/dl/{token}?p={远端路径}&n={文件名}`。
- Agent 的 WS 端口（server.rs，127.0.0.1）上新增**最小 HTTP/1.1 处理**：`peek` 首包，`GET /dl` 前缀分流到下载端点，其余走原 WS 握手。SFTP 直读（64KB 块）直写 TCP，不整读入内存，大文件安全。
- 复用既有设施：`registry.acquire_sftp`（[pool.rs#L135](file:///d:/sorce3/BitGalaxy.Shell/apps/desktop/src-tauri/src/pool.rs#L135)）、`stat_deadline`/`open_read_deadline`（[sftp.rs#L210-L230](file:///d:/sorce3/BitGalaxy.Shell/apps/desktop/src-tauri/src/sftp.rs#L210-L230)）、`classify_sftp_error`。
- 因 `dragstart` 内不能 await（setData 必须同步），令牌按**会话级**签发（新 sftp op `dl-open`，面板在 ls 成功后请求一次，缓存 `dlBase`），拖出行时前端同步拼 URL。

### 拖入上传 = 面板级 Tauri `onDragDropEvent` 命中测试

仿照 App.tsx 终端模式：SftpPane 自注册 `getCurrentWebview().onDragDropEvent`，`elementFromPoint(物理坐标/DPR)` 命中自己根元素（`data-sftp-session` 属性）才处理；与 App.tsx 的终端 listener 互不干扰。浏览器宿主保留现有 HTML5 onDrop 回退。

### 面板内移动（尽力而为）双通道

- 浏览器模式：标准 HTML5——`dragstart` 写入自定义 MIME `application/x-bgshell-remote`，文件夹行/面包屑项 `dragover/drop`。
- Tauri 模式：HTML5 dragover/drop 被 wry 拦截收不到，改为：`dragstart` 时记录 `draggingEntries`（ref），借 pane 自己的 `onDragDropEvent`（`paths` 为空 + dragging 存在即内部拖动）在 `over` 时命中测试 `data-drop-dir` 高亮、`drop` 时执行移动。若 wry 对非文件拖动不转发事件 → 该功能静默降级（无副作用）。
- 移动语义 = 既有 `rename` op（同主机移动）。选中多项时拖其中一项 → 整个选区移动。

## 后端改动（Rust，apps/desktop/src-tauri）

### 1. 新文件 `src/dl_http.rs`
- `pub struct DlTickets`：`Arc<StdMutex<HashMap<String, DlCap>>>` + `port: u16`；`DlCap { host_id, expires: Instant }`，TTL 30 分钟，插入/查询时惰性清理过期项。
- `pub fn issue(&self, host_id) -> String`：生成 uuid v4 token，返回完整基址 `http://127.0.0.1:{port}/dl/{token}`。
- `pub async fn try_handle(stream: TcpStream, tickets: &DlTickets, registry: &ConnRegistry) -> bool`：
  - `stream.peek` 首包，非 `GET /dl` 前缀返回 false（交回 WS 握手流程）。
  - 读取请求头至 `\r\n\r\n`，解析 `token`、`p`（远端路径）、`n`（文件名）；token 失效 → `404 text/plain`。
  - `stat_deadline` 校验文件（不存在 → `404`，附 classify 后的短文案）；`registry.acquire_sftp(host_id, "drag-download")` 失败 → `500`。
  - `open_read_deadline` 打开，先发响应头（`200 OK`、`Content-Length: size`、`Content-Type: application/octet-stream`、`Content-Disposition: attachment; filename*=UTF-8''<n>`、`Connection: close`），再 64KB 循环 `write_all`（每块 60s 超时，写失败/客户端断开即终止）。
  - 读句柄随任务结束 drop（与 `download_open` 一致；写路径的 flush+close 约定不涉及）。
- 纯手写 HTTP（不引新依赖）；`tokio::io::AsyncReadExt/AsyncWriteExt`。

### 2. [server.rs](file:///d:/sorce3/BitGalaxy.Shell/apps/desktop/src-tauri/src/server.rs)
- `start_server`：创建 `Arc<DlTickets>`（进程级单例，持 port）。
- `handle_conn` 入口先 `dl_http::try_handle(...)`，true 则结束该连接。
- `SessionManager::new` 增参 `dl: Arc<DlTickets>`（唯一调用点在 server.rs#L135，mcp.rs 不用 SessionManager，已确认）。

### 3. [session.rs](file:///d:/sorce3/BitGalaxy.Shell/apps/desktop/src-tauri/src/session.rs)
- `SessionManager` 加字段 `dl: Arc<DlTickets>`（Clone 语义不变）。
- `handle_sftp` match 加 `"dl-open"` 分支：entry.sftp 就绪 + host_id 存在 → `dl.issue(host_id)` → `ok_sftp("dl-open", rid, |m| m.dl_base = Some(url))`；会话未就绪走既有 err_sftp。位置：放在拿 sftp 控制器之后（同 ls/mkdir 一组，无需 sftp 操作但需会话身份）。

### 4. [protocol.rs](file:///d:/sorce3/BitGalaxy.Shell/apps/desktop/src-tauri/src/protocol.rs)
- `SftpResultM` 加 `#[serde(skip_serializing_if)] pub dl_base: Option<String>`。

## 前端改动

### 5. [protocol/index.ts](file:///d:/sorce3/BitGalaxy.Shell/packages/protocol/src/index.ts)
- `SftpRequest` 加 `| { op: 'dl-open' }`；`SftpResult` 加 `dlBase?: string`。

### 6. [SftpPane.tsx](file:///d:/sorce3/BitGalaxy.Shell/packages/workbench-ui/src/components/SftpPane.tsx)（主改造）
**状态新增**：`dlBase?`、`selected: string[]`（rowSelection）、`menu?: { x, y, entry?, entries? }`、`dragOver: boolean`、`draggingRef`（内部拖动记录）、`hoverDropDirRef`。

**拖入（Tauri）**：`useEffect` 注册 `getCurrentWebview().onDragDropEvent`：
- `enter/over`：命中自己根元素 → `dragOver=true`（paths>0 拖入高亮；paths 空 + draggingRef 有值 → 内部拖动，命中 `data-drop-dir` 加高亮类）。
- `drop`：paths>0 → `uploadLocalPaths(paths)`（新函数）；paths 空 + draggingRef → `moveEntries(draggingRef, 命中的 data-drop-dir)`。
- `leave`：清高亮。
- `uploadLocalPaths`：`stat_file` invoke（含 is_dir）→ items（is_dir → put-dir）→ `preflightBatch('upload')` → `startTransfer` → 完成后 `ls(path)`。文件夹拖入即递归上传（超越终端只跳过目录的现状，原生传输引擎本身支持）。
- 根元素加 `data-sftp-session={sessionId}`；浏览器模式保留现有 `onDrop` → `uploadFiles`。

**拖出**：Table `onRow`：
- `draggable`、`onDragStart`：记录 draggingRef（该行在选区 → 整个选区，否则单行）；`type==='file' && dlBase` 时 setData：`DownloadURL`（name 做 Windows 非法字符 `<>:"/\|?*` → `_` 清洗）、`text/plain`、`application/x-bgshell-remote`（JSON：sessionId/path/names）；`onDragEnd` 清 draggingRef。
- `dlBase` 获取：`ls` 成功后 `request({op:'dl-open'})` 一次，失败静默（下次 ls 重试）。文件夹不设 DownloadURL（拖出仅文件；提示文案说明）。
- 仅 `isTauri` 或 location 为 localhost 时启用（云端工作台宿主直连 127.0.0.1 不可达）。

**面板内移动**：`moveEntries(fromDir, names, toDir)`：目标父目录 == 源目录则提示无效；逐项 `request({op:'rename', from, to})`，`Modal.confirm` 确认（N 项移动到 X），完成后刷新 + 汇总 toast。文件夹行/面包屑项加 `data-drop-dir` 属性 + 浏览器模式 onDragOver/onDrop。

**多选**：`rowSelection={{ selectedRowKeys: selected, onChange }}`；出现选中时工具栏显示浮动条：「已选 N 项 · 批量下载 · 批量删除 · 取消」。
- 批量下载（Tauri）：`open({directory:true})` 选目录 → items（dir→get-dir / file→get-file，target=dir+sep+name）→ preflightBatch('download') → 逐项 startTransfer。
- 批量删除：confirm 汇总（目录数/文件数）→ 逐项 rm → 汇总成功/失败 → 刷新。
- 浏览器宿主批量下载：串行走现有流式 `download()`（回退体验，可接受）。

**右键菜单**：
- `onRow` `onContextMenu`：命中行在选区 → 选区菜单；否则单行菜单（并清除其余选择）。根元素 `onContextMenu`（target 非行时）→ 空白区菜单。
- 实现：`menu` state + fixed 定位浮层渲染 antd `Menu`；全局 `mousedown`/`contextmenu`/`Escape` 关闭。菜单项复用现有 `doMkdir/doRename/doDelete/download/downloadDir/pickAndUpload`。
- 行菜单（文件）：下载到本地… / 复制名称 / 复制完整路径 / 重命名 / 删除；行菜单（目录）：打开 / 下载文件夹… / 复制路径 / 重命名 / 删除；空白区：上传文件… / 上传文件夹… / 新建文件夹 / 刷新。
- 双击文件行 → `download(entry)`；双击目录行 → 进入（等同单击）。

**移除操作列**：columns 去掉「操作」列（下载/重命名/删除按钮列），列表仅 名称/大小/修改时间。顶部 Tag 文案改为「拖入上传 · 拖出下载到目标文件夹 · 右键更多操作」。

### 7. [index.css](file:///d:/sorce3/BitGalaxy.Shell/packages/workbench-ui/src/index.css)
- `.sftp-pane.drag-over`（拖入高亮：边框/背景 + 顶部提示条「松开以上传到 xxx」）；`.sftp-row-drop-hover`（文件夹目标行高亮）；右键菜单浮层少量样式（如需）。

## 关键细节 / 风险

- **dragstart 不能 await** → 会话级 `dlBase` 预取方案（见上）。
- **wry 内部拖动事件不确定**：Tauri 模式面板内移动依赖 wry 对非文件拖动转发 `enter/over/drop`（paths 空）。不转发则该子功能静默失效，其余不受影响；实现放最后阶段，实测决定去留（浏览器模式始终可用）。
- **DownloadURL 拖出为 Chromium 核心能力**（Electron 同款机制），WebView2 大概率可用；必须 dev 实测。若拖到 Explorer 无反应 → 回退策略：保留右键/双击「下载」为主路径，拖出暂缓（或后续换 Windows 原生 DoDragDrop+临时文件方案，本次不做）。
- **拖出仅单文件**（DownloadURL 协议限制）；文件夹/多选拖出无效果，文案与菜单引导用批量下载。
- 拖出行松手落回面板内：Tauri 模式 wry 拦截 → 无 HTML5 drop → 不会触发 Chromium 误下载；浏览器模式根元素 `onDragOver preventDefault` 已兜住。
- **dl 令牌安全**：仅绑 127.0.0.1 监听；uuid v4 能力令牌按会话签发、30min TTL；`p` 任意路径可下载该主机上会话用户可读文件（与面板本身同权限面，可接受）。
- **HTTP 端点无 Range**（200+Content-Length 单流）；中断重试需重新拖。后续可加（russh-sftp File 支持 AsyncSeek，sftp.rs#L1079 已有先例）。
- 后端必须先于前端超时：HTTP 端点复用 15s META deadline 语义（stat/open），错误文案含 "timeout" 以便归类。

## 实施顺序

1. 后端：dl_http.rs（DlTickets + try_handle + 流式下载）→ server.rs 分流/传参 → session.rs dl-open → protocol.rs dl_base；`cargo test` + `cargo build`。
2. 前端协议：index.ts dl-open/dlBase；根目录 `npm run build` 验证类型。
3. SftpPane：拖出（onRow draggable + DownloadURL + dlBase 获取）+ 右键菜单 + 移除操作列 + 双击下载 → dev 实测拖出到资源管理器。
4. 拖入（onDragDropEvent listener + uploadLocalPaths + 高亮 + 文件夹 put-dir）→ 实测。
5. 多选 + 批量下载/删除 + 选区右键/批量移动。
6. 面板内移动（浏览器 HTML5 + Tauri draggingRef 双通道）→ 实测 Tauri 行为，决定保留/降级（若 wry 不转发，代码保留但自然失效）。

## 验证

- `cargo test`（src-tauri，含 dl_http 解析/token 单测：新模块补 token 过期/路径解析的最小测试）
- `cargo build`；根目录 `npm run build`（tsc + vite）
- `npm run desktop:dev` 手动验证清单：
  1. 拖桌面文件入面板 → 高亮 → 松手 → 预检弹窗 → put-file 上传成功、列表刷新
  2. 拖本地**文件夹**入面板 → put-dir 递归上传
  3. 拖面板内文件出到资源管理器任意文件夹/桌面 → 文件完整落盘（对比大小）、中文名/带空格名正常
  4. 文件夹拖出 → 不生效（无 DownloadURL），右键可下载文件夹
  5. 右键文件/目录/空白区菜单各项逐一可用；复制路径、重命名、删除
  6. 勾选多个 → 批量下载到本地目录、批量删除
  7. 面板内把文件拖到文件夹行 → 移动成功（若 Tauri 不生效则记录为已知限制）
  8. 断开主机后拖出/拖入 → 明确报错（404/预检失败），无挂死
  9. 浏览器宿主（vite 17177 直接打开）HTML5 回退路径：拖入上传、内部移动、右键可用
