# 桌宠动作与动画解绑 · 动作包 DIY 实施计划

## Context

当前桌宠的「动作」（状态机语义：idle/working/wave…）与「动画」（帧序列素材）硬编码耦合：[PetSprite.tsx](file:///d:\sorce3\BitGalaxy.Shell\packages\workbench-ui\src\pet\components\PetSprite.tsx) 中 `FRAME_TABLE` 写死 10 个状态的帧数/FPS/循环，帧路径固定为 `/pet/<state>/<state>_NN.png`，用户想换形象只能改源码。

目标：
1. **动画与动作解绑**——用户上传「动作包」（zip）即可 DIY 自己的宠物，可导入/启用/删除/恢复内置；
2. **我们只提供动作规范**——一份 pack.json schema 规范文档 + 程序化校验，不绑定任何形象。

已拍板：上传形式 = **zip 动作包**；覆盖粒度 = **整包 + 缺省回落**（包可只提供部分动作，缺失动作回落内置鲸鱼娘）；存储 = **本地磁盘目录**（数据目录下 `pet-packs/`，Rust 命令读写；浏览器宿主降级为仅内置）。

## 一、动作规范（pack spec v1）

### zip 结构

```
my-pack.zip
├── pack.json
├── idle_breath/
│   ├── idle_breath_00.png
│   └── ...
└── wave/
    └── wave_00.png
```

### pack.json schema

```ts
interface PackManifest {
  schema: 1
  id: string        // [a-z0-9_-]{1,32}，唯一，落盘目录名
  name: string      // 展示名
  author?: string
  version: string
  states: Partial<Record<PetStateKey, ClipSpec>>  // 可只提供部分动作 → 缺省回落内置
}
interface ClipSpec {
  frames: number    // 1..48，与实际 PNG 数严格一致
  fps: number       // 1..30
  loop: boolean
}
```

- 动作 key 全集 = 状态机 10 个 `PetStateKey`（基础层 `sleep / idle_breath / working / eating / need_input / drag` + 叠加层 `ready / wave / idle_blink / angry`），key 与语义由 `machine.ts` 状态机定义，**包只负责给这些 key 提供动画**——这正是解绑点。
- 帧命名：`<state>/<state>_NN.png`（00 起、两位补零、连续无跳号），与内置 `public/pet/` 同构，两套资源共用一个加载器。

### 校验规则（导入时 Rust 侧全量执行）

- zip ≤ 30MB、解压后总大小 ≤ 80MB、条目数 ≤ 600（防 zip 炸弹）；
- 路径安全：拒绝 `..`、绝对路径、盘符、符号链接（防 zip-slip）；
- 每张 PNG 校验 magic `\x89PNG\r\n\x1a\n` + IHDR 宽高 ≤ 2048；
- `frames` 与实际文件数一致、文件名严格匹配；未知 state key 拒绝；重复 `id` 拒绝（提示先删旧包）。

### 文档与模板

- `docs/pet-pack-spec.md`：规范全文 + 示例结构 + 美术建议（PNG 透明、建议 512×512 以内、同状态帧使用联合取景框防抖动、足底锚点对齐）。
- `public/pet-pack-template/`：最小模板 zip（仅 pack.json + 1-2 帧示例），管理面板内「下载模板」入口。

## 二、前端解绑：PackRuntime

新增 `packages/workbench-ui/src/pet/pack/packRuntime.ts`：

```ts
interface ResolvedClip {
  frames: number; fps: number; loop: boolean
  frameUrl(i: number): string
}
interface PackRuntime {
  clip(key: PetStateKey): ResolvedClip   // 用户包缺失的动作 → 回落内置 clip
}
```

- **内置 clip**：新增 `pack/builtinClips.ts`，从 `public/pet/manifest.json` 的 `states.*` 提取 `frames/fps/loop`（替代 `FRAME_TABLE` 硬编码，注释注明以 manifest.json 为准）；`frameUrl` = `${BASE_URL}pet/<state>/<state>_NN.png`。
- **用户包 clip**：帧文件在数据目录 `pet-packs/<id>/` 下，用 Tauri `convertFileSrc(absPath)` 生成 asset URL（Windows 下为 `http://asset.localhost/...`）。
- **PetSprite 改动**：`FRAME_TABLE` 全部替换为 `runtime.clip(state)`（预载循环、rAF 的 `spec`、`frameUrl` 三处）；rAF 时钟/交叉淡化/呼吸微动/指针交互逻辑不动。runtime 作为 prop 注入，包切换时 `key={packId}` 重挂载清帧缓存。

### Tauri 配置（现状均缺，必须同步补）

1. [tauri.conf.json](file:///d:\sorce3\BitGalaxy.Shell\apps\desktop\src-tauri\tauri.conf.json) `app.security` 增 `"assetProtocol": { "enable": true, "scope": ["<数据目录>/pet-packs/**"] }`；
2. [Cargo.toml](file:///d:\sorce3\BitGalaxy.Shell\apps\desktop\src-tauri\Cargo.toml) `tauri` features 加 `"asset-protocol"`（现有 `["tray-icon"]`）；
3. CSP 现为 `null` 不拦截，暂不动（若日后收紧需 `img-src 'self' asset: http://asset.localhost`）。

## 三、Rust 新命令（新模块 `pet_pack.rs`）

```rust
#[derive(Serialize)] struct PetPackInfo { id, name, version, author, states: Vec<String>, source: /* builtin|user */ }
#[derive(Serialize)] struct PackImportError { code: String, message: String, detail: Option<String> }

#[tauri::command] fn pet_pack_import(app, path: String) -> Result<PetPackInfo, PackImportError>
#[tauri::command] fn pet_packs_list(app) -> Result<Vec<PetPackInfo>, String>
#[tauri::command] fn pet_pack_delete(app, id: String) -> Result<(), String>
#[tauri::command] fn pet_pack_set_enabled(app, id: Option<String>) -> Result<(), String>  // None = 恢复内置
```

- 存储布局：复用 `lib.rs` 的 `resolve_data_dir()`（与 db/logs 同根）：`<data_dir>/pet-packs/<id>/{pack.json, <state>/*.png}`；导入先解到 `.tmp-<uuid>` 校验通过后原子 rename。
- **启用标记放磁盘**：`pet-packs/active.json`（`{"active": "id"|null}`），单一真相；delete 时若为 active 同步置 null。
- zip 解压用 `zip` crate（新增依赖）；校验逻辑写成独立纯函数，便于 `cargo test`（zip-slip、坏 PNG、帧数不符、超限）。
- lib.rs `generate_handler!` 注册 4 个命令。
- 错误约定：`Err(PackImportError)` 序列化为 `{code, message, detail}`，前端红字直接展示。

## 四、管理 UI

新增 `packages/workbench-ui/src/pet/components/PetPackPanel.tsx`，[PetMenu](file:///d:\sorce3\BitGalaxy.Shell\packages\workbench-ui\src\pet\components\PetMenu.tsx) 增「动作包管理」入口：

- 对齐 ChatPanel 覆盖式抽屉风格；pet 窗小（约 340×420），进入面板时调 `petWindowSize` 扩至约 420×560、关闭还原（复用 [tauri.ts](file:///d:\sorce3\BitGalaxy.Shell\packages\workbench-ui\src\pet\api\tauri.ts) 的 `applyPetWindowSize`，需小幅扩展支持自定义尺寸档）。
- 内容：包列表（内置鲸鱼娘 + 用户包：名称 / 覆盖动作数 / 启用单选）、导入（`tauri-plugin-dialog` 的 `dialog.open` 选 zip → `pet_pack_import`）、删除（确认后 `pet_pack_delete`）、恢复内置、折叠「规范说明」+「下载模板」。
- **浏览器宿主**（`!isTauri`）：入口提示「仅桌面版可自定义动作包」，列表仅内置只读。
- 导入/切换后重建 PackRuntime 立即生效。

## 五、实施步骤

1. **Rust 层**：`pet_pack.rs`（解压/校验/布局/4 命令）+ Cargo.toml 加 `zip`、`asset-protocol` + lib.rs 注册 + tauri.conf.json assetProtocol scope → `cargo test` 过校验单测。
2. **前端 runtime**：`pack/packRuntime.ts` + `pack/builtinClips.ts` + PetSprite 接入 → 浏览器回归内置动画零变化。
3. **管理 UI**：PetPackPanel + PetMenu 入口 + 扩窗逻辑。
4. **规范文档/模板**：`docs/pet-pack-spec.md` + 模板 zip。

## 六、本次不动（控范围）

- `machine.ts` 语义层（PetBaseState/PetActionKey/ACTION_PRIORITY/ACTION_MS 及事件逻辑）；
- Bubble 台词库、fullbody/bubble UI 素材；
- PetSprite 的呼吸微动/交叉淡化/指针交互；
- 工作台 SFTP 拖放上传（`forwardDropUpload` 路径不变，非 zip 的拖放仍转发工作台）。

## 七、验证

- `cargo test`（apps/desktop/src-tauri）：校验函数单测（zip-slip、坏 PNG、帧数不符、超限）。
- 浏览器宿主 `npm run dev`：内置动画渲染与现状一致（回归），管理面板降级提示正确。
- Tauri 宿主：导入只含 2 个动作的测试 zip → 对应动作切换为自定义动画、其余动作回落内置；启用/删除/恢复内置即时生效；重启后 active 状态保持；重复 id/坏 zip 弹出明确错误。
- 制作一个仅 `wave` + `idle_breath` 的测试包做端到端目测（悬停招手/待机呼吸）。

## 风险点

- asset protocol 未同时改 scope + feature 会导致用户帧 404——两处必须配套；
- zip 炸弹/zip-slip：解压前逐条目校验路径与总量上限；
- 用户帧尺寸差异导致锚点跳动：规范强约束取景，运行时不缩放矫正（保持简单）；
- pet 窗过小：面板必须扩窗，否则列表不可用。
