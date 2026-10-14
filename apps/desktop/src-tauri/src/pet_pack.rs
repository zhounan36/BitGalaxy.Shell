//! 桌宠动作包（pet pack）：用户 DIY 动画的导入、校验与管理（规范 v2）。
//!
//! 动作语义由前端状态机定义（见 workbench-ui pet/machine.ts），
//! 本模块负责按《桌宠动作包规范 v2》校验 zip 并落盘动画资源：
//! `<data_dir>/pet-packs/<id>/{pack.json, <key>/...}`。
//! clip 支持三种形态：帧序列（frames，兼容 v1 平铺字段）、单视频（video）、单图/动图（image）。
//! 可选 behavior 节描述用户自定义动作图（触发映射 + 转换规则），校验通过后原样透传前端。
//! 启用标记放 `<data_dir>/pet-packs/active.json`（单一真相），None = 恢复内置形象。

use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

/// pack.json 的 schema 版本（动作规范 v1/v2 共用，v2 向后兼容 v1）
const PACK_SCHEMA: u64 = 1;
/// zip 压缩包体积上限（防滥用）
const MAX_ZIP_BYTES: u64 = 30 * 1024 * 1024;
/// 解压后总大小上限（防 zip 炸弹）
const MAX_UNCOMPRESSED_BYTES: u64 = 80 * 1024 * 1024;
/// zip 条目数上限
const MAX_ENTRIES: usize = 600;
/// 单素材文件大小上限（帧图/视频/单图）
const MAX_FILE_BYTES: u64 = 15 * 1024 * 1024;
/// 图片宽/高上限（像素，查 IHDR）
const MAX_PNG_DIM: u32 = 2048;
/// 单动作帧数上限
const MAX_FRAMES: u32 = 48;
/// 帧序列允许的扩展名（同状态统一格式）
const FRAME_EXTS: [&str; 5] = ["png", "jpg", "jpeg", "webp", "gif"];
/// video.src 允许的扩展名
const VIDEO_EXTS: [&str; 2] = ["webm", "mp4"];
/// image.src / video.poster 允许的扩展名
const IMAGE_EXTS: [&str; 5] = ["png", "jpg", "jpeg", "webp", "gif"];
/// TriggerName 全集（13 个，与前端事件类型一致）
const TRIGGER_NAMES: [&str; 13] = [
    "ws-open",
    "ws-close",
    "session-opened",
    "session-closed",
    "output",
    "hostkey-unknown",
    "hostkey-confirmed",
    "agent-error",
    "pointer-enter",
    "poke",
    "drag-start",
    "drag-end",
    "blink",
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

/// clip 形态（pack.json states.* 判别联合）
#[derive(Clone, Debug)]
pub enum ClipKind {
    /// 帧序列 `<key>/<key>_NN.<ext>`，NN 从 00 两位补零（v1 平铺字段兼容）
    Frames { frames: u32, fps: u32, loop_: bool, ext: String },
    /// 单视频文件（webm/mp4）
    Video { src: String, loop_: bool, poster: Option<String> },
    /// 单静态图/动图（png/jpg/webp/gif，GIF/APNG/WebP 动图原生播放）
    Image { src: String, fps: Option<u32>, loop_: Option<bool> },
}

/// 单个动作的动画规格（pack.json states.*）
#[derive(Clone, Debug)]
pub struct PackClip {
    pub key: String,
    pub kind: ClipKind,
}

/// clip 对外序列化：`{"key","kind","frames"?,"fps"?,"loop"?,"src"?,"poster"?}`
/// （loop 仅在显式声明时输出，image 两个可选项均可缺省）
impl Serialize for PackClip {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("key", &self.key)?;
        match &self.kind {
            ClipKind::Frames { frames, fps, loop_, ext } => {
                map.serialize_entry("kind", "frames")?;
                map.serialize_entry("frames", frames)?;
                map.serialize_entry("fps", fps)?;
                map.serialize_entry("loop", loop_)?;
                // 帧扩展名（png/jpg/webp/gif）：前端据此拼帧 URL，缺省 png
                if ext != "png" {
                    map.serialize_entry("ext", ext)?;
                }
            }
            ClipKind::Video { src, loop_, poster } => {
                map.serialize_entry("kind", "video")?;
                map.serialize_entry("src", src)?;
                map.serialize_entry("loop", loop_)?;
                if let Some(p) = poster {
                    map.serialize_entry("poster", p)?;
                }
            }
            ClipKind::Image { src, fps, loop_ } => {
                map.serialize_entry("kind", "image")?;
                map.serialize_entry("src", src)?;
                if let Some(f) = fps {
                    map.serialize_entry("fps", f)?;
                }
                if let Some(l) = loop_ {
                    map.serialize_entry("loop", l)?;
                }
            }
        }
        map.end()
    }
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
    /// behavior 节原样透传（前端渲染自定义动作图信息；无则 None = 内置默认行为）
    pub behavior: Option<serde_json::Value>,
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
    behavior: Option<serde_json::Value>,
}

/// active.json 内容
#[derive(Serialize, Deserialize, Default)]
struct ActiveFile {
    active: Option<String>,
}

// ── 校验工具 ─────────────────────────────────────────────────────────────

/// 动作 key 命名：[a-z0-9_-]{1,24}（behavior.actions 放宽到 24）
fn valid_action_key(k: &str) -> bool {
    let len = k.len();
    (1..=24).contains(&len) && k.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

/// id 约束：[a-z0-9_-]{1,32}（同时是落盘目录名）
fn valid_pack_id(id: &str) -> bool {
    let len = id.len();
    (1..=32).contains(&len) && id.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

/// 去掉点号后的小写扩展名（无扩展名返回 None）
fn file_ext(rel: &str) -> Option<String> {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())
}

/// 数值型可选字段校验：缺失返回 None，非整数/越界返回 bad-manifest
fn opt_u64_field(
    obj: &serde_json::Map<String, serde_json::Value>,
    ctx: &str,
    field: &str,
    lo: u64,
    hi: u64,
) -> Result<Option<u64>, PackImportError> {
    match obj.get(field) {
        None => Ok(None),
        Some(serde_json::Value::Number(n)) if n.as_u64().is_some() => {
            let v = n.as_u64().unwrap();
            if v < lo || v > hi {
                return Err(PackImportError::new(
                    "bad-manifest",
                    format!("{ctx}.{field} 必须在 {lo}..={hi}"),
                    Some(v.to_string()),
                ));
            }
            Ok(Some(v))
        }
        Some(_) => Err(PackImportError::new(
            "bad-manifest",
            format!("{ctx}.{field} 必须是 {lo}..={hi} 的整数"),
            None,
        )),
    }
}

/// 按扩展名 + magic 校验素材内容；png（含 apng）另查 IHDR ≤ MAX_PNG_DIM
fn check_media_bytes(bytes: &[u8], ext: &str) -> Option<&'static str> {
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Some("素材文件超大小上限（≤15MB）");
    }
    match ext {
        "png" => {
            if png_dims(bytes).is_none() {
                return Some("PNG/APNG 不合法（magic/IHDR 错误或尺寸超 2048）");
            }
        }
        "jpg" | "jpeg" => {
            if bytes.len() < 3 || bytes[..3] != [0xffu8, 0xd8, 0xff] {
                return Some("JPEG magic 不匹配（FF D8 FF）");
            }
        }
        "webp" => {
            if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
                return Some("WebP magic 不匹配（RIFF..WEBP）");
            }
        }
        "gif" => {
            let ok = bytes.len() >= 6
                && (bytes[..6] == *b"GIF89a" || bytes[..6] == *b"GIF87a");
            if !ok {
                return Some("GIF magic 不匹配（GIF89a/GIF87a）");
            }
        }
        "webm" => {
            if bytes.len() < 4 || bytes[..4] != [0x1au8, 0x45, 0xdf, 0xa3] {
                return Some("WebM magic 不匹配（EBML 1A 45 DF A3）");
            }
        }
        "mp4" => {
            if bytes.len() < 8 || &bytes[4..8] != b"ftyp" {
                return Some("MP4 magic 不匹配（ftyp）");
            }
        }
        _ => return Some("不支持的素材扩展名"),
    }
    None
}

/// 校验单个素材文件：存在 + 按扩展名白名单 + magic
fn check_media_file(
    root: &Path,
    rel: &str,
    allowed_exts: &[&str],
    what: &str,
) -> Result<(), PackImportError> {
    let Some(ext) = file_ext(rel) else {
        return Err(PackImportError::new("bad-media", format!("{what}缺少扩展名"), Some(rel.to_string())));
    };
    if !allowed_exts.contains(&ext.as_str()) {
        return Err(PackImportError::new(
            "bad-media",
            format!("{what}扩展名不受支持"),
            Some(format!("{rel}（允许: {allowed_exts:?}）")),
        ));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(root.join(rel))
        .and_then(|mut f| f.read_to_end(&mut bytes))
        .map_err(|e| PackImportError::new("io", format!("读取{what}失败"), Some(e.to_string())))?;
    if let Some(msg) = check_media_bytes(&bytes, &ext) {
        return Err(PackImportError::new("bad-media", msg, Some(rel.to_string())));
    }
    Ok(())
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

// ── behavior 行为表校验 ──────────────────────────────────────────────────

/// 校验 behavior 节（自定义动作图）：mode/config/actions/triggers/transitions
fn validate_behavior(b: &serde_json::Value) -> Result<(), PackImportError> {
    let bad = |msg: &str, detail: Option<String>| PackImportError::new("bad-behavior", msg, detail);
    let obj = b.as_object().ok_or_else(|| bad("behavior 必须是对象", None))?;

    // mode：默认 override
    match obj.get("mode") {
        None | Some(serde_json::Value::Null) => {}
        Some(serde_json::Value::String(m)) if m == "override" || m == "replace" => {}
        Some(v) => {
            return Err(bad(
                "behavior.mode 只能是 override/replace",
                Some(v.to_string()),
            ))
        }
    }

    // config：三个时间参数可选，>0 且 ≤86400000
    if let Some(c) = obj.get("config") {
        if !c.is_null() {
            let c = c.as_object().ok_or_else(|| bad("behavior.config 必须是对象", None))?;
            for (k, v) in c {
                if !matches!(k.as_str(), "eatingAfterMs" | "eatingToSleepMs" | "sleepAfterMs") {
                    return Err(bad("behavior.config 包含未知参数", Some(k.clone())));
                }
                let n = v.as_u64().ok_or_else(|| {
                    bad("behavior.config 值必须是正整数", Some(k.clone()))
                })?;
                if n == 0 || n > 86_400_000 {
                    return Err(bad(
                        "behavior.config 值必须在 1..=86400000",
                        Some(format!("{k}={n}")),
                    ));
                }
            }
        }
    }

    // actions：动作定义表（layer 必填，其余可选且有范围）
    let mut action_keys: HashSet<String> = HashSet::new();
    if let Some(a) = obj.get("actions") {
        if !a.is_null() {
            let a = a.as_object().ok_or_else(|| bad("behavior.actions 必须是对象", None))?;
            for (key, spec) in a {
                if !valid_action_key(key) {
                    return Err(bad("动作 key 必须匹配 [a-z0-9_-]{1,24}", Some(key.clone())));
                }
                action_keys.insert(key.clone());
                let ctx = format!("behavior.actions.{key}");
                let s = spec.as_object().ok_or_else(|| {
                    bad("动作定义必须是对象", Some(key.clone()))
                })?;
                match s.get("layer") {
                    Some(serde_json::Value::String(l)) if l == "base" || l == "oneshot" => {}
                    _ => return Err(bad("动作 layer 必须是 base/oneshot", Some(key.clone()))),
                }
                // 数值范围错误统一归为 bad-behavior
                let remap = |r: Result<Option<u64>, PackImportError>| {
                    r.map_err(|mut e| {
                        e.code = "bad-behavior".into();
                        e
                    })
                };
                remap(opt_u64_field(s, &ctx, "priority", 0, 10))?;
                remap(opt_u64_field(s, &ctx, "durationMs", 100, 60_000))?;
                remap(opt_u64_field(s, &ctx, "cooldownMs", 0, 600_000))?;
            }
        }
    }

    // replace 模式必须至少有一个基础层动作
    if obj.get("mode").and_then(|m| m.as_str()) == Some("replace") {
        let has_base = obj.get("actions").and_then(|a| a.as_object()).map_or(false, |a| {
            a.values().any(|s| s.get("layer").and_then(|l| l.as_str()) == Some("base"))
        });
        if !has_base {
            return Err(bad("replace 模式必须至少有一个 layer=base 的动作", None));
        }
    }

    // triggers：TriggerName → 动作 key（Partial，可选）
    if let Some(t) = obj.get("triggers") {
        if !t.is_null() {
            let t = t.as_object().ok_or_else(|| bad("behavior.triggers 必须是对象", None))?;
            for (name, val) in t {
                if !TRIGGER_NAMES.contains(&name.as_str()) {
                    return Err(bad("behavior.triggers 包含非法触发器名", Some(name.clone())));
                }
                let target = val.as_str().ok_or_else(|| {
                    bad("behavior.triggers 值必须是动作 key 字符串", Some(name.clone()))
                })?;
                if !action_keys.contains(target) {
                    return Err(bad(
                        "behavior.triggers 引用了未定义的动作",
                        Some(format!("{name} → {target}")),
                    ));
                }
            }
        }
    }

    // transitions：定时转换表（from/to ∈ actions，afterMs 有范围，禁止环）
    if let Some(ts) = obj.get("transitions") {
        if !ts.is_null() {
            let arr = ts.as_array().ok_or_else(|| {
                bad("behavior.transitions 必须是数组", None)
            })?;
            let mut graph: HashMap<&str, &str> = HashMap::new();
            for (i, tr) in arr.iter().enumerate() {
                let ctx = format!("behavior.transitions[{i}]");
                let tr = tr.as_object().ok_or_else(|| bad("转换项必须是对象", Some(ctx.clone())))?;
                let from = tr.get("from").and_then(|x| x.as_str()).ok_or_else(|| {
                    bad("转换项缺少 from", Some(ctx.clone()))
                })?;
                let to = tr.get("to").and_then(|x| x.as_str()).ok_or_else(|| {
                    bad("转换项缺少 to", Some(ctx.clone()))
                })?;
                if !action_keys.contains(from) || !action_keys.contains(to) {
                    return Err(bad(
                        "behavior.transitions 的 from/to 必须是已定义动作",
                        Some(format!("{from} → {to}")),
                    ));
                }
                if from == to {
                    return Err(bad("behavior.transitions 不允许自环", Some(from.to_string())));
                }
                let after = tr.get("afterMs").and_then(|x| x.as_u64()).ok_or_else(|| {
                    bad("转换项 afterMs 必须是 100..=6000000 的整数", Some(ctx.clone()))
                })?;
                if !(100..=6_000_000).contains(&after) {
                    return Err(bad(
                        "转换项 afterMs 必须在 100..=6000000",
                        Some(after.to_string()),
                    ));
                }
                // 每个 from 至多一条出边（检测到重复直接拒绝，环检测即单链遍历）
                if graph.insert(from, to).is_some() {
                    return Err(bad(
                        "behavior.transitions 存在重复的 from",
                        Some(from.to_string()),
                    ));
                }
            }
            for start in graph.keys() {
                let mut seen: HashSet<&str> = HashSet::new();
                seen.insert(start);
                let mut cur = *start;
                while let Some(&next) = graph.get(cur) {
                    if !seen.insert(next) {
                        return Err(bad("behavior.transitions 转换图不允许有环", Some(start.to_string())));
                    }
                    cur = next;
                }
            }
        }
    }
    Ok(())
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
        let entry = archive
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

/// 解析 states.* 单个 clip（判别联合；无 kind 或 kind 缺省 = frames，兼容 v1 平铺字段）
fn parse_clip(key: &str, spec: &serde_json::Value) -> Result<ClipKind, PackImportError> {
    let s = spec.as_object().ok_or_else(|| {
        PackImportError::new("bad-manifest", format!("states.{key} 必须是对象"), None)
    })?;
    let kind = match s.get("kind") {
        None | Some(serde_json::Value::Null) => "frames",
        Some(serde_json::Value::String(k)) => k.as_str(),
        Some(_) => {
            return Err(PackImportError::new(
                "bad-manifest",
                "states.*.kind 必须是 frames/video/image",
                Some(key.to_string()),
            ))
        }
    };
    let ctx = format!("states.{key}");
    match kind {
        "frames" => {
            let frames = opt_u64_field(s, &ctx, "frames", 1, MAX_FRAMES as u64)?.ok_or_else(|| {
                PackImportError::new("bad-manifest", format!("states.{key}.frames 缺失"), None)
            })?;
            let fps = opt_u64_field(s, &ctx, "fps", 1, 30)?.ok_or_else(|| {
                PackImportError::new("bad-manifest", format!("states.{key}.fps 缺失"), None)
            })?;
            let loop_ = s.get("loop").and_then(|x| x.as_bool()).unwrap_or(false);
            // 帧扩展名：可选 ext 字段（缺省 png），必须在帧格式白名单内
            let ext = match s.get("ext") {
                None | Some(serde_json::Value::Null) => "png".to_string(),
                Some(serde_json::Value::String(e)) => {
                    let e = e.trim_start_matches('.').to_ascii_lowercase();
                    if !FRAME_EXTS.contains(&e.as_str()) {
                        return Err(PackImportError::new(
                            "bad-manifest",
                            "states.*.ext 必须是 png/jpg/jpeg/webp/gif",
                            Some(format!("{}: {e}", key)),
                        ));
                    }
                    e
                }
                Some(_) => {
                    return Err(PackImportError::new(
                        "bad-manifest",
                        "states.*.ext 必须是字符串",
                        Some(ctx.clone()),
                    ))
                }
            };
            Ok(ClipKind::Frames { frames: frames as u32, fps: fps as u32, loop_, ext })
        }
        "video" => {
            let src = s.get("src").and_then(|x| x.as_str()).filter(|x| !x.is_empty()).ok_or_else(|| {
                PackImportError::new("bad-manifest", format!("states.{key}.src 必须是非空字符串"), None)
            })?;
            if !matches!(file_ext(src).as_deref(), Some("webm" | "mp4")) {
                return Err(PackImportError::new(
                    "bad-manifest",
                    "video.src 必须是 .webm/.mp4 文件",
                    Some(src.to_string()),
                ));
            }
            let loop_ = s.get("loop").and_then(|x| x.as_bool()).unwrap_or(false);
            let poster = match s.get("poster") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(p)) if !p.is_empty() => Some(p.clone()),
                Some(_) => {
                    return Err(PackImportError::new(
                        "bad-manifest",
                        format!("states.{key}.poster 必须是非空字符串"),
                        None,
                    ))
                }
            };
            if let Some(p) = &poster {
                if !IMAGE_EXTS.contains(&file_ext(p).unwrap_or_default().as_str()) {
                    return Err(PackImportError::new(
                        "bad-manifest",
                        "video.poster 必须是图片文件",
                        Some(p.clone()),
                    ));
                }
            }
            Ok(ClipKind::Video { src: src.to_string(), loop_, poster })
        }
        "image" => {
            let src = s.get("src").and_then(|x| x.as_str()).filter(|x| !x.is_empty()).ok_or_else(|| {
                PackImportError::new("bad-manifest", format!("states.{key}.src 必须是非空字符串"), None)
            })?;
            if !IMAGE_EXTS.contains(&file_ext(src).unwrap_or_default().as_str()) {
                return Err(PackImportError::new(
                    "bad-manifest",
                    "image.src 必须是 .png/.jpg/.jpeg/.webp/.gif 文件",
                    Some(src.to_string()),
                ));
            }
            let fps = opt_u64_field(s, &ctx, "fps", 1, 30)?.map(|v| v as u32);
            let loop_ = s.get("loop").map(|x| x.as_bool().unwrap_or(false));
            Ok(ClipKind::Image { src: src.to_string(), fps, loop_ })
        }
        other => Err(PackImportError::new(
            "bad-manifest",
            "states.*.kind 必须是 frames/video/image",
            Some(format!("{key}: kind={other}")),
        )),
    }
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

    // states：key 命名校验（自定义动作 key 放宽到 [a-z0-9_-]{1,24}），clip 判别联合解析
    let states_obj = obj
        .get("states")
        .and_then(|x| x.as_object())
        .filter(|m| !m.is_empty())
        .ok_or_else(|| {
            PackImportError::new("bad-manifest", "states 必须是非空对象", None)
        })?;
    let mut states = Vec::new();
    for (key, spec) in states_obj {
        if !valid_action_key(key) {
            return Err(PackImportError::new(
                "bad-manifest",
                "states 动作 key 必须匹配 [a-z0-9_-]{1,24}",
                Some(key.clone()),
            ));
        }
        let kind = parse_clip(key, spec)?;
        states.push(PackClip { key: key.clone(), kind });
    }
    states.sort_by(|a, b| a.key.cmp(&b.key));

    // behavior：可选（缺失 = 内置默认行为，合法）
    let behavior = match obj.get("behavior") {
        None | Some(serde_json::Value::Null) => None,
        Some(b) => {
            validate_behavior(b)?;
            Some(b.clone())
        }
    };

    Ok(PackManifest { id, name, version, author, states, behavior })
}

/// 校验解压目录：每个 clip 按 kind 引用素材（帧序列连续无多余/单视频/单图），
/// 实际文件与引用集严格相等（无多余文件），所有素材过扩展名 + magic 校验
fn validate_extracted(root: &Path, manifest: &PackManifest) -> Result<(), PackImportError> {
    let mut expected: BTreeSet<String> = BTreeSet::new();
    expected.insert("pack.json".to_string());
    let mut media: Vec<(String, Vec<&'static str>, &'static str)> = Vec::new();
    // 实际文件集只扫一次，frames kind 据此定位帧文件并确定扩展名
    let mut found: BTreeSet<String> = BTreeSet::new();
    collect_files(root, root, &mut found).map_err(|e| {
        PackImportError::new("io", "扫描解压目录失败", Some(e.to_string()))
    })?;
    for clip in &manifest.states {
        match &clip.kind {
            ClipKind::Frames { frames, .. } => {
                // 帧序列：命名必须是 <key>/<key>_NN.<ext>，NN 两位补零且 < frames，扩展名统一
                let dir = format!("{}/", clip.key);
                let prefix = format!("{}_", clip.key);
                let mut ext: Option<String> = None;
                let mut count = 0u32;
                for f in found.iter() {
                    let Some(name) = f.strip_prefix(&dir) else { continue };
                    let Some((stem, e)) = name.rsplit_once('.') else { continue };
                    let Some(nn) = stem.strip_prefix(&prefix) else { continue };
                    if nn.len() != 2 || !nn.bytes().all(|b| b.is_ascii_digit()) {
                        continue;
                    }
                    if nn.parse::<u32>().unwrap_or(u32::MAX) >= *frames {
                        continue;
                    }
                    let e = e.to_ascii_lowercase();
                    if !FRAME_EXTS.contains(&e.as_str()) {
                        continue;
                    }
                    match &ext {
                        None => ext = Some(e),
                        Some(x) if *x == e => {}
                        Some(x) => {
                            return Err(PackImportError::new(
                                "bad-frames",
                                "帧序列扩展名不统一",
                                Some(format!("{}: {x} vs {e}", clip.key)),
                            ))
                        }
                    }
                    expected.insert(f.clone());
                    count += 1;
                }
                let Some(ext) = ext else {
                    return Err(PackImportError::new(
                        "bad-frames",
                        "帧序列缺少素材文件",
                        Some(clip.key.clone()),
                    ));
                };
                if count != *frames {
                    return Err(PackImportError::new(
                        "bad-frames",
                        "帧文件数量与 pack.json 声明不一致",
                        Some(format!("{}: 声明 {frames}，实际 {count}", clip.key)),
                    ));
                }
                for i in 0..*frames {
                    media.push((
                        format!("{}/{}_{:02}.{ext}", clip.key, clip.key, i),
                        FRAME_EXTS.to_vec(),
                        "帧图",
                    ));
                }
            }
            ClipKind::Video { src, poster, .. } => {
                let rel = format!("{}/{}", clip.key, src);
                expected.insert(rel.clone());
                media.push((rel, VIDEO_EXTS.to_vec(), "视频"));
                if let Some(p) = poster {
                    let rel = format!("{}/{}", clip.key, p);
                    expected.insert(rel.clone());
                    media.push((rel, IMAGE_EXTS.to_vec(), "视频封面"));
                }
            }
            ClipKind::Image { src, .. } => {
                let rel = format!("{}/{}", clip.key, src);
                expected.insert(rel.clone());
                media.push((rel, IMAGE_EXTS.to_vec(), "图片"));
            }
        }
    }
    if found != expected {
        let missing: Vec<_> = expected.difference(&found).cloned().collect();
        let extra: Vec<_> = found.difference(&expected).cloned().collect();
        return Err(PackImportError::new(
            "bad-frames",
            "素材文件与 pack.json 声明不一致",
            Some(format!("缺失: {missing:?}；多余: {extra:?}")),
        ));
    }
    // 素材合法性逐个校验（扩展名白名单 + magic，png 另查 IHDR）
    for (rel, allowed, what) in media {
        check_media_file(root, &rel, &allowed, what)?;
    }
    Ok(())
}

fn collect_files(root: &Path, dir: &Path, out: &mut BTreeSet<String>) -> std::io::Result<()> {
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

/// 完整导入流程：校验 zip → 解压到临时目录 → 校验 manifest/素材/behavior → 原子落盘
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
        behavior: manifest.behavior.clone(),
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
            behavior: manifest.behavior.clone(),
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

    /// 造最小 JPEG magic（FF D8 FF + 填充）
    fn fake_jpeg() -> Vec<u8> {
        vec![0xff, 0xd8, 0xff, 0xe0, 0, 0, 0, 0]
    }

    /// 造最小 GIF89a magic
    fn fake_gif() -> Vec<u8> {
        b"GIF89a\x00\x00\x00\x00".to_vec()
    }

    /// 造最小 WebM（EBML）magic
    fn fake_webm() -> Vec<u8> {
        vec![0x1a, 0x45, 0xdf, 0xa3, 0, 0, 0, 0]
    }

    /// 造最小 MP4（.. ftyp）magic
    fn fake_mp4() -> Vec<u8> {
        b"\x00\x00\x00\x18ftypisom\x00\x00\x00\x00".to_vec()
    }

    /// 造最小 WebP（RIFF..WEBP）magic
    fn fake_webp() -> Vec<u8> {
        let mut b = b"RIFF\x08\x00\x00\x00WEBP".to_vec();
        b.extend_from_slice(b"VP8 ");
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

    /// 带 behavior 节的 pack.json
    fn manifest_json_beh(id: &str, states: &str, behavior: &str) -> String {
        format!(
            r#"{{"schema":1,"id":"{id}","name":"测试包","version":"1.0.0","states":{states},"behavior":{behavior}}}"#
        )
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
        // 对外序列化形状：frames 形态
        let s = serde_json::to_value(&info.states[0]).unwrap();
        assert_eq!(s["kind"], "frames");
        assert_eq!(s["frames"], 2);
        assert_eq!(s["loop"], false);
        assert!(info.behavior.is_none());
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
        // 非 PNG 内容（magic 不匹配）
        let z2 = root.join("b.zip");
        write_zip(
            &z2,
            &[
                ("pack.json", manifest_json("p2", r#"{"wave":{"frames":1,"fps":6,"loop":false}}"#).into_bytes()),
                ("wave/wave_00.png", b"garbage".to_vec()),
            ],
        );
        assert_eq!(import_pack(&z2, &packs).unwrap_err().code, "bad-media");
        // 非法动作 key（命名不匹配 [a-z0-9_-]{1,24}）
        let z3 = root.join("u.zip");
        write_zip(
            &z3,
            &[
                ("pack.json", manifest_json("p3", r#"{"Bad Key!":{"frames":1,"fps":6,"loop":false}}"#).into_bytes()),
                ("Bad Key!/Bad Key!_00.png", fake_png(512, 512)),
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

    #[test]
    fn import_jpeg_frames_pack() {
        let root = tmpdir();
        let z = root.join("j.zip");
        write_zip(
            &z,
            &[
                ("pack.json", manifest_json("jpg-pack", r#"{"wave":{"kind":"frames","frames":2,"fps":6,"loop":true}}"#).into_bytes()),
                ("wave/wave_00.jpg", fake_jpeg()),
                ("wave/wave_01.jpg", fake_jpeg()),
            ],
        );
        let packs = root.join("pet-packs");
        let info = import_pack(&z, &packs).unwrap();
        assert_eq!(info.states.len(), 1);
        assert!(packs.join("jpg-pack/wave/wave_01.jpg").exists());
        let s = serde_json::to_value(&info.states[0]).unwrap();
        assert_eq!(s["kind"], "frames");
        assert_eq!(s["loop"], true);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn import_video_and_image_clips() {
        let root = tmpdir();
        let packs = root.join("pet-packs");
        // video（webm + poster）
        let z1 = root.join("v.zip");
        write_zip(
            &z1,
            &[
                ("pack.json", manifest_json("vid", r#"{"wave":{"kind":"video","src":"wave.webm","loop":true,"poster":"poster.png"}}"#).into_bytes()),
                ("wave/wave.webm", fake_webm()),
                ("wave/poster.png", fake_png(64, 64)),
            ],
        );
        let info = import_pack(&z1, &packs).unwrap();
        let s = serde_json::to_value(&info.states[0]).unwrap();
        assert_eq!(s["kind"], "video");
        assert_eq!(s["src"], "wave.webm");
        assert_eq!(s["poster"], "poster.png");
        assert_eq!(s["loop"], true);
        assert!(s.get("frames").is_none());
        // image（gif 动图）
        let z2 = root.join("i.zip");
        write_zip(
            &z2,
            &[
                ("pack.json", manifest_json("img", r#"{"wave":{"kind":"image","src":"anim.gif","fps":12,"loop":true}}"#).into_bytes()),
                ("wave/anim.gif", fake_gif()),
            ],
        );
        let info = import_pack(&z2, &packs).unwrap();
        let s = serde_json::to_value(&info.states[0]).unwrap();
        assert_eq!(s["kind"], "image");
        assert_eq!(s["src"], "anim.gif");
        assert_eq!(s["fps"], 12);
        assert_eq!(s["loop"], true);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ext_mismatch_rejected() {
        let root = tmpdir();
        let packs = root.join("pet-packs");
        // .png 里塞 JPEG 数据 → bad-media
        let z1 = root.join("f.zip");
        write_zip(
            &z1,
            &[
                ("pack.json", manifest_json("fake", r#"{"wave":{"frames":1,"fps":6,"loop":false}}"#).into_bytes()),
                ("wave/wave_00.png", fake_jpeg()),
            ],
        );
        assert_eq!(import_pack(&z1, &packs).unwrap_err().code, "bad-media");
        // video.src 扩展名白名单外（.mp3）→ bad-manifest
        let z2 = root.join("e.zip");
        write_zip(
            &z2,
            &[
                ("pack.json", manifest_json("badext", r#"{"wave":{"kind":"video","src":"song.mp3","loop":false}}"#).into_bytes()),
                ("wave/song.mp3", fake_webm()),
            ],
        );
        assert_eq!(import_pack(&z2, &packs).unwrap_err().code, "bad-manifest");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn behavior_valid_import() {
        let root = tmpdir();
        let z = root.join("b.zip");
        let behavior = r#"{
            "mode":"replace",
            "config":{"eatingAfterMs":30000,"sleepAfterMs":300000},
            "actions":{
                "work":{"layer":"base"},
                "wave":{"layer":"oneshot","priority":3,"durationMs":1200,"cooldownMs":15000}
            },
            "triggers":{"poke":"wave","output":"work","blink":"wave"},
            "transitions":[{"from":"work","afterMs":30000,"to":"wave"}]
        }"#;
        write_zip(
            &z,
            &[
                (
                    "pack.json",
                    manifest_json_beh(
                        "bh",
                        r#"{"work":{"kind":"image","src":"work.webp"},"wave":{"kind":"video","src":"wave.mp4","loop":false}}"#,
                        behavior,
                    )
                    .into_bytes(),
                ),
                ("work/work.webp", fake_webp()),
                ("wave/wave.mp4", fake_mp4()),
            ],
        );
        let packs = root.join("pet-packs");
        let info = import_pack(&z, &packs).unwrap();
        // behavior 原样透传
        let b = info.behavior.unwrap();
        assert_eq!(b["mode"], "replace");
        assert_eq!(b["actions"]["wave"]["priority"], 3);
        assert_eq!(b["triggers"]["poke"], "wave");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn behavior_rejects() {
        let root = tmpdir();
        let packs = root.join("pet-packs");
        let mk = |behavior: &str| -> PackImportError {
            let z = root.join(format!("bb-{}.zip", uuid::Uuid::new_v4()));
            write_zip(
                &z,
                &[
                    (
                        "pack.json",
                        manifest_json_beh(
                            "bad-beh",
                            r#"{"wave":{"frames":1,"fps":6,"loop":false}}"#,
                            behavior,
                        )
                        .into_bytes(),
                    ),
                    ("wave/wave_00.png", fake_png(64, 64)),
                ],
            );
            import_pack(&z, &packs).unwrap_err()
        };
        // 非法 trigger 名
        let e = mk(r#"{"actions":{"wave":{"layer":"oneshot"}},"triggers":{"no-such-trigger":"wave"}}"#);
        assert_eq!(e.code, "bad-behavior");
        // trigger 引用缺失 action
        let e = mk(r#"{"actions":{"wave":{"layer":"oneshot"}},"triggers":{"poke":"ghost"}}"#);
        assert_eq!(e.code, "bad-behavior");
        // transition 自环
        let e = mk(
            r#"{"actions":{"wave":{"layer":"base"}},"transitions":[{"from":"wave","afterMs":1000,"to":"wave"}]}"#,
        );
        assert_eq!(e.code, "bad-behavior");
        // transition 成环（A→B→A）
        let e = mk(
            r#"{"actions":{"a":{"layer":"base"},"b":{"layer":"base"}},"transitions":[
                {"from":"a","afterMs":1000,"to":"b"},
                {"from":"b","afterMs":1000,"to":"a"}
            ]}"#,
        );
        assert_eq!(e.code, "bad-behavior");
        // replace 模式无 base 层动作
        let e = mk(r#"{"mode":"replace","actions":{"wave":{"layer":"oneshot"}}}"#);
        assert_eq!(e.code, "bad-behavior");
        // priority 越界
        let e = mk(r#"{"actions":{"wave":{"layer":"oneshot","priority":11}}}"#);
        assert_eq!(e.code, "bad-behavior");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn v1_pack_regression() {
        // v1 包：无 kind、无 behavior，平铺 {frames,fps,loop}，.png 帧
        let root = tmpdir();
        let z = root.join("v1.zip");
        write_zip(
            &z,
            &[
                ("pack.json", manifest_json("old-pack", r#"{"wave":{"frames":1,"fps":6,"loop":false}}"#).into_bytes()),
                ("wave/wave_00.png", fake_png(128, 128)),
            ],
        );
        let packs = root.join("pet-packs");
        let info = import_pack(&z, &packs).unwrap();
        assert_eq!(info.id, "old-pack");
        assert!(info.behavior.is_none());
        let s = serde_json::to_value(&info.states[0]).unwrap();
        assert_eq!(s["kind"], "frames");
        assert_eq!(s["frames"], 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}
