//! 桌宠动作包（pet pack）：用户 DIY 动画的导入、校验与管理。
//!
//! 动作语义（key 全集）由前端状态机定义（见 workbench-ui pet/machine.ts），
//! 本模块只负责按《动作规范》（docs/pet-pack-spec.md）校验 zip 并落盘动画资源：
//! `<data_dir>/pet-packs/<id>/{pack.json, <state>/<state>_NN.png}`。
//! 启用标记放 `<data_dir>/pet-packs/active.json`（单一真相），None = 恢复内置形象。

use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};

/// pack.json 的 schema 版本（动作规范 v1）
const PACK_SCHEMA: u64 = 1;
/// zip 压缩包体积上限（防滥用）
const MAX_ZIP_BYTES: u64 = 30 * 1024 * 1024;
/// 解压后总大小上限（防 zip 炸弹）
const MAX_UNCOMPRESSED_BYTES: u64 = 80 * 1024 * 1024;
/// zip 条目数上限
const MAX_ENTRIES: usize = 600;
/// 帧图 PNG 宽/高上限（像素）
const MAX_PNG_DIM: u32 = 2048;
/// 单动作帧数上限
const MAX_FRAMES: u32 = 48;
/// 动作 key 全集（与前端 PetStateKey 一致）：基础层 6 + 叠加层 4
const STATE_KEYS: [&str; 10] = [
    "sleep",
    "idle_breath",
    "working",
    "eating",
    "need_input",
    "drag",
    "ready",
    "wave",
    "idle_blink",
    "angry",
];

/// 导入失败：结构化错误，前端面板红字直接展示 message/detail
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PackImportError {
    pub code: String,
    pub message: String,
    pub detail: Option<String>,
}

impl PackImportError {
    fn new(code: &str, message: impl Into<String>, detail: Option<String>) -> Self {
        Self { code: code.into(), message: message.into(), detail }
    }
}

impl std::fmt::Display for PackImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.detail {
            Some(d) => write!(f, "{}（{}）", self.message, d),
            None => write!(f, "{}", self.message),
        }
    }
}

/// 单个动作的动画规格（pack.json states.*）
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PackClip {
    pub key: String,
    pub frames: u32,
    pub fps: u32,
    #[serde(rename = "loop")]
    pub loop_: bool,
}

/// 动作包信息（列表/导入返回）
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PetPackInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub author: Option<String>,
    pub states: Vec<PackClip>,
    /// 落盘绝对目录（前端 convertFileSrc 生成 asset URL 用）
    pub dir: String,
    /// 是否为当前启用的动作包
    pub active: bool,
}

/// pack.json 解析结果（校验通过后使用）
#[derive(Debug)]
struct PackManifest {
    id: String,
    name: String,
    version: String,
    author: Option<String>,
    states: Vec<PackClip>,
}

/// active.json 内容
#[derive(Serialize, Deserialize, Default)]
struct ActiveFile {
    active: Option<String>,
}

// ── 校验工具 ─────────────────────────────────────────────────────────────

fn is_state_key(k: &str) -> bool {
    STATE_KEYS.contains(&k)
}

/// id 约束：[a-z0-9_-]{1,32}（同时是落盘目录名）
fn valid_pack_id(id: &str) -> bool {
    let len = id.len();
    (1..=32).contains(&len) && id.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

/// PNG 校验：magic + IHDR 宽高 ≤ MAX_PNG_DIM。返回 (width, height)。
fn png_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    const MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if bytes.len() < 24 || bytes[..8] != MAGIC {
        return None;
    }
    // 8 签名 + 4 长度 + 4 "IHDR" + 4 宽 + 4 高
    if &bytes[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    if w == 0 || h == 0 || w > MAX_PNG_DIM || h > MAX_PNG_DIM {
        return None;
    }
    Some((w, h))
}

/// zip 条目相对路径安全检查（防 zip-slip）：返回规范化正斜杠路径（目录条目去尾斜杠）
fn safe_entry_rel_path(name: &str) -> Option<String> {
    let norm = name.replace('\\', "/");
    let norm = norm.trim_end_matches('/');
    if norm.is_empty() || norm.starts_with('/') || norm.contains(':') {
        return None;
    }
    if norm.split('/').any(|seg| seg == ".." || seg.is_empty()) {
        return None;
    }
    Some(norm.to_string())
}

// ── 导入：解压 + 校验 + 原子落盘 ─────────────────────────────────────────

/// 解压 zip 到 dest 并逐条目校验路径/大小，返回所有相对文件路径
fn extract_zip(zip_path: &Path, dest: &Path) -> Result<Vec<String>, PackImportError> {
    let file = std::fs::File::open(zip_path).map_err(|e| {
        PackImportError::new("io", "打开动作包失败", Some(e.to_string()))
    })?;
    let zip_bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
    if zip_bytes > MAX_ZIP_BYTES {
        return Err(PackImportError::new(
            "too-large",
            "动作包体积超限（≤30MB）",
            Some(format!("{zip_bytes} bytes")),
        ));
    }
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| PackImportError::new("bad-zip", "不是有效的 zip 动作包", Some(e.to_string())))?;
    if archive.len() > MAX_ENTRIES {
        return Err(PackImportError::new(
            "too-large",
            "动作包条目数超限（≤600）",
            Some(format!("{} entries", archive.len())),
        ));
    }

    let mut files = Vec::new();
    let mut total: u64 = 0;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| PackImportError::new("bad-zip", "读取 zip 条目失败", Some(e.to_string())))?;
        // 符号链接拒绝（unix_mode 高位 0o120000）
        if entry.unix_mode().map(|m| m & 0o170000 == 0o120000).unwrap_or(false) {
            return Err(PackImportError::new(
                "zip-slip",
                "动作包包含符号链接条目",
                Some(entry.name().to_string()),
            ));
        }
        let rel = safe_entry_rel_path(entry.name()).ok_or_else(|| {
            PackImportError::new("zip-slip", "动作包包含非法路径条目", Some(entry.name().to_string()))
        })?;
        let out = dest.join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)
                .map_err(|e| PackImportError::new("io", "创建目录失败", Some(e.to_string())))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| PackImportError::new("io", "创建目录失败", Some(e.to_string())))?;
        }
        let mut w = std::fs::File::create(&out)
            .map_err(|e| PackImportError::new("io", "写入文件失败", Some(e.to_string())))?;
        // 实际解压量限流（条目声明的 size 可造假）
        let mut take = entry.take(MAX_UNCOMPRESSED_BYTES.saturating_sub(total) + 1);
        let n = std::io::copy(&mut take, &mut w)
            .map_err(|e| PackImportError::new("io", "解压文件失败", Some(e.to_string())))?;
        total += n;
        if total > MAX_UNCOMPRESSED_BYTES {
            return Err(PackImportError::new(
                "too-large",
                "动作包解压后体积超限（≤80MB）",
                None,
            ));
        }
        files.push(rel);
    }
    Ok(files)
}

/// 解析并校验 pack.json（纯函数，可单测）
fn parse_manifest(json: &[u8]) -> Result<PackManifest, PackImportError> {
    let v: serde_json::Value = serde_json::from_slice(json)
        .map_err(|e| PackImportError::new("bad-manifest", "pack.json 不是合法 JSON", Some(e.to_string())))?;
    let obj = v.as_object().ok_or_else(|| {
        PackImportError::new("bad-manifest", "pack.json 根节点必须是对象", None)
    })?;

    let schema = obj.get("schema").and_then(|x| x.as_u64()).unwrap_or(0);
    if schema != PACK_SCHEMA {
        return Err(PackImportError::new(
            "bad-manifest",
            "不支持的 pack.json schema 版本",
            Some(format!("期望 {PACK_SCHEMA}，实际 {schema}")),
        ));
    }
    let str_field = |k: &str, required: bool| -> Result<Option<String>, PackImportError> {
        match obj.get(k) {
            Some(serde_json::Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
            Some(_) => Err(PackImportError::new("bad-manifest", format!("字段 {k} 必须是非空字符串"), None)),
            None if required => Err(PackImportError::new("bad-manifest", format!("缺少必填字段 {k}"), None)),
            None => Ok(None),
        }
    };
    let id = str_field("id", true)?.unwrap_or_default();
    if !valid_pack_id(&id) {
        return Err(PackImportError::new(
            "bad-manifest",
            "id 必须匹配 [a-z0-9_-]{1,32}",
            Some(id),
        ));
    }
    let name = str_field("name", true)?.unwrap_or_default();
    let version = str_field("version", true)?.unwrap_or_default();
    let author = str_field("author", false)?;

    let states_obj = obj
        .get("states")
        .and_then(|x| x.as_object())
        .filter(|m| !m.is_empty())
        .ok_or_else(|| {
            PackImportError::new("bad-manifest", "states 必须是非空对象", None)
        })?;
    let mut states = Vec::new();
    for (key, spec) in states_obj {
        if !is_state_key(key) {
            return Err(PackImportError::new(
                "bad-manifest",
                "states 包含未知动作 key",
                Some(key.clone()),
            ));
        }
        let s = spec.as_object().ok_or_else(|| {
            PackImportError::new("bad-manifest", format!("states.{key} 必须是对象"), None)
        })?;
        let frames = s.get("frames").and_then(|x| x.as_u64()).unwrap_or(0);
        let fps = s.get("fps").and_then(|x| x.as_u64()).unwrap_or(0);
        let loop_ = s.get("loop").and_then(|x| x.as_bool()).unwrap_or(false);
        if frames == 0 || frames > MAX_FRAMES as u64 {
            return Err(PackImportError::new(
                "bad-manifest",
                format!("states.{key}.frames 必须在 1..={MAX_FRAMES}"),
                Some(frames.to_string()),
            ));
        }
        if fps == 0 || fps > 30 {
            return Err(PackImportError::new(
                "bad-manifest",
                format!("states.{key}.fps 必须在 1..=30"),
                Some(fps.to_string()),
            ));
        }
        states.push(PackClip {
            key: key.clone(),
            frames: frames as u32,
            fps: fps as u32,
            loop_,
        });
    }
    states.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(PackManifest { id, name, version, author, states })
}

/// 校验解压目录：帧文件严格匹配 pack.json 声明，无多余文件，PNG 合法
fn validate_extracted(root: &Path, manifest: &PackManifest) -> Result<(), PackImportError> {
    let mut expected: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    expected.insert("pack.json".to_string());
    for clip in &manifest.states {
        for i in 0..clip.frames {
            expected.insert(format!("{}/{}_{:02}.png", clip.key, clip.key, i));
        }
    }
    let mut found: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    collect_files(root, root, &mut found).map_err(|e| {
        PackImportError::new("io", "扫描解压目录失败", Some(e.to_string()))
    })?;
    if found != expected {
        let missing: Vec<_> = expected.difference(&found).cloned().collect();
        let extra: Vec<_> = found.difference(&expected).cloned().collect();
        return Err(PackImportError::new(
            "bad-frames",
            "帧文件与 pack.json 声明不一致",
            Some(format!("缺失: {missing:?}；多余: {extra:?}")),
        ));
    }
    // PNG 合法性逐张校验
    for rel in found.iter().filter(|f| f.ends_with(".png")) {
        let mut bytes = Vec::new();
        std::fs::File::open(root.join(rel))
            .and_then(|mut f| f.read_to_end(&mut bytes))
            .map_err(|e| PackImportError::new("io", "读取帧图失败", Some(rel.clone())))?;
        if png_dims(&bytes).is_none() {
            return Err(PackImportError::new(
                "bad-png",
                "帧图不是合法 PNG（或尺寸超 2048）",
                Some(rel.clone()),
            ));
        }
    }
    Ok(())
}

fn collect_files(root: &Path, dir: &Path, out: &mut std::collections::BTreeSet<String>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            collect_files(root, &p, out)?;
        } else {
            let rel = p.strip_prefix(root).unwrap_or(&p).to_string_lossy().replace('\\', "/");
            out.insert(rel);
        }
    }
    Ok(())
}

/// 完整导入流程：校验 zip → 解压到临时目录 → 校验 manifest/帧 → 原子落盘
pub fn import_pack(zip_path: &Path, packs_root: &Path) -> Result<PetPackInfo, PackImportError> {
    std::fs::create_dir_all(packs_root)
        .map_err(|e| PackImportError::new("io", "创建动作包目录失败", Some(e.to_string())))?;
    let tmp = packs_root.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<PackManifest, PackImportError> {
        extract_zip(zip_path, &tmp)?;
        let manifest_bytes = std::fs::read(tmp.join("pack.json")).map_err(|_| {
            PackImportError::new("bad-manifest", "zip 根目录缺少 pack.json", None)
        })?;
        let manifest = parse_manifest(&manifest_bytes)?;
        validate_extracted(&tmp, &manifest)?;
        Ok(manifest)
    })();
    let manifest = match result {
        Ok(m) => m,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
    };

    let final_dir = packs_root.join(&manifest.id);
    if final_dir.exists() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(PackImportError::new(
            "dup-id",
            "已存在同 id 的动作包，请先删除旧包",
            Some(manifest.id),
        ));
    }
    std::fs::rename(&tmp, &final_dir)
        .map_err(|e| PackImportError::new("io", "动作包落盘失败", Some(e.to_string())))?;
    Ok(PetPackInfo {
        id: manifest.id.clone(),
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        author: manifest.author.clone(),
        states: manifest.states.clone(),
        dir: final_dir.to_string_lossy().into_owned(),
        active: false,
    })
}

// ── 列表 / 启用 / 删除 ────────────────────────────────────────────────────

fn read_active(packs_root: &Path) -> Option<String> {
    let bytes = std::fs::read(packs_root.join("active.json")).ok()?;
    let f: ActiveFile = serde_json::from_slice(&bytes).ok()?;
    f.active
}

fn write_active(packs_root: &Path, active: Option<String>) -> Result<(), String> {
    std::fs::create_dir_all(packs_root).map_err(|e| e.to_string())?;
    let f = ActiveFile { active };
    let json = serde_json::to_vec_pretty(&f).map_err(|e| e.to_string())?;
    let tmp = packs_root.join(format!(".tmp-active-{}", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, &json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, packs_root.join("active.json")).map_err(|e| e.to_string())
}

/// 列出用户动作包（损坏的包目录跳过，不拖垮整表）
pub fn list_packs(packs_root: &Path) -> Vec<PetPackInfo> {
    let active = read_active(packs_root);
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(packs_root) else { return out };
    for entry in rd.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(id) = dir.file_name().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        if id.starts_with('.') || !valid_pack_id(&id) {
            continue;
        }
        let Ok(bytes) = std::fs::read(dir.join("pack.json")) else { continue };
        let Ok(manifest) = parse_manifest(&bytes) else { continue };
        if manifest.id != id {
            continue;
        }
        out.push(PetPackInfo {
            id: manifest.id.clone(),
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            author: manifest.author.clone(),
            states: manifest.states.clone(),
            dir: dir.to_string_lossy().into_owned(),
            active: active.as_deref() == Some(manifest.id.as_str()),
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

pub fn delete_pack(packs_root: &Path, id: &str) -> Result<(), String> {
    if !valid_pack_id(id) {
        return Err("非法动作包 id".into());
    }
    let dir = packs_root.join(id);
    if !dir.exists() {
        return Err("动作包不存在".into());
    }
    std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    if read_active(packs_root).as_deref() == Some(id) {
        write_active(packs_root, None)?;
    }
    Ok(())
}

// ── Tauri IPC 命令 ────────────────────────────────────────────────────────

fn packs_root() -> PathBuf {
    crate::resolve_data_dir().join("pet-packs")
}

/// 导入动作包 zip（校验失败返回结构化 PackImportError）
#[tauri::command]
pub fn pet_pack_import(path: String) -> Result<PetPackInfo, PackImportError> {
    import_pack(Path::new(&path), &packs_root())
}

/// 列出全部用户动作包（含 active 标记）
#[tauri::command]
pub fn pet_packs_list() -> Result<Vec<PetPackInfo>, String> {
    Ok(list_packs(&packs_root()))
}

/// 删除动作包（若为当前启用项则回落内置）
#[tauri::command]
pub fn pet_pack_delete(id: String) -> Result<(), String> {
    delete_pack(&packs_root(), &id)
}

/// 设置启用的动作包；None = 恢复内置形象
#[tauri::command]
pub fn pet_pack_set_enabled(id: Option<String>) -> Result<(), String> {
    let root = packs_root();
    if let Some(id) = &id {
        if !valid_pack_id(id) {
            return Err("非法动作包 id".into());
        }
        if !root.join(id).join("pack.json").exists() {
            return Err("动作包不存在".into());
        }
    }
    write_active(&root, id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// 造一张满足 magic+IHDR 的最小 PNG 头（24 字节）
    fn fake_png(w: u32, h: u32) -> Vec<u8> {
        let mut b = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        b.extend_from_slice(&13u32.to_be_bytes());
        b.extend_from_slice(b"IHDR");
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&h.to_be_bytes());
        b
    }

    fn write_zip(path: &Path, entries: &[(&str, Vec<u8>)]) {
        let f = std::fs::File::create(path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        for (name, data) in entries {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap();
    }

    fn manifest_json(id: &str, states: &str) -> String {
        format!(r#"{{"schema":1,"id":"{id}","name":"测试包","version":"1.0.0","states":{states}}}"#)
    }

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("petpack-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn png_magic_and_dims() {
        assert!(png_dims(&fake_png(512, 512)).is_some());
        assert!(png_dims(&fake_png(4096, 512)).is_none()); // 超 2048
        assert!(png_dims(b"not a png").is_none());
    }

    #[test]
    fn zip_slip_rejected() {
        assert!(safe_entry_rel_path("../evil.png").is_none());
        assert!(safe_entry_rel_path("/abs/evil.png").is_none());
        assert!(safe_entry_rel_path("C:/evil.png").is_none());
        assert!(safe_entry_rel_path("a\\..\\evil.png").is_none());
        assert_eq!(safe_entry_rel_path("wave/wave_00.png").as_deref(), Some("wave/wave_00.png"));
    }

    #[test]
    fn import_valid_partial_pack() {
        let root = tmpdir();
        let zip_path = root.join("p.zip");
        write_zip(
            &zip_path,
            &[
                ("pack.json", manifest_json("my-pet", r#"{"wave":{"frames":2,"fps":6,"loop":false}}"#).into_bytes()),
                ("wave/wave_00.png", fake_png(512, 512)),
                ("wave/wave_01.png", fake_png(512, 512)),
            ],
        );
        let packs = root.join("pet-packs");
        let info = import_pack(&zip_path, &packs).unwrap();
        assert_eq!(info.id, "my-pet");
        assert_eq!(info.states.len(), 1);
        assert!(packs.join("my-pet/wave/wave_01.png").exists());
        // 重复 id 拒绝
        assert_eq!(import_pack(&zip_path, &packs).unwrap_err().code, "dup-id");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn import_rejects_frames_mismatch_and_bad_png() {
        let root = tmpdir();
        let packs = root.join("pet-packs");
        // frames 声明 2 但只有 1 帧
        let z1 = root.join("m.zip");
        write_zip(
            &z1,
            &[
                ("pack.json", manifest_json("p1", r#"{"wave":{"frames":2,"fps":6,"loop":false}}"#).into_bytes()),
                ("wave/wave_00.png", fake_png(512, 512)),
            ],
        );
        assert_eq!(import_pack(&z1, &packs).unwrap_err().code, "bad-frames");
        // 非 PNG 内容
        let z2 = root.join("b.zip");
        write_zip(
            &z2,
            &[
                ("pack.json", manifest_json("p2", r#"{"wave":{"frames":1,"fps":6,"loop":false}}"#).into_bytes()),
                ("wave/wave_00.png", b"garbage".to_vec()),
            ],
        );
        assert_eq!(import_pack(&z2, &packs).unwrap_err().code, "bad-png");
        // 未知动作 key
        let z3 = root.join("u.zip");
        write_zip(
            &z3,
            &[
                ("pack.json", manifest_json("p3", r#"{"fly":{"frames":1,"fps":6,"loop":false}}"#).into_bytes()),
                ("fly/fly_00.png", fake_png(512, 512)),
            ],
        );
        assert_eq!(import_pack(&z3, &packs).unwrap_err().code, "bad-manifest");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn active_lifecycle() {
        let root = tmpdir();
        let packs = root.join("pet-packs");
        let z = root.join("p.zip");
        write_zip(
            &z,
            &[
                ("pack.json", manifest_json("cute", r#"{"wave":{"frames":1,"fps":6,"loop":false}}"#).into_bytes()),
                ("wave/wave_00.png", fake_png(64, 64)),
            ],
        );
        import_pack(&z, &packs).unwrap();
        write_active(&packs, Some("cute".into())).unwrap();
        assert!(list_packs(&packs)[0].active);
        delete_pack(&packs, "cute").unwrap();
        // 删除后 active 自动回落内置
        assert_eq!(read_active(&packs), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
