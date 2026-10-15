//! 驱动管理器：内置驱动清单 + 解析。
//! 驱动宿主 exe 随安装包发布（tauri bundle resources → 安装目录 drivers/{name}/host.exe，
//! 三个宿主合计约 8MB，无需按需下载）。解析顺序：
//!   1. exe 同级 `drivers/{name}/host(.exe)`（安装包内置；开发期 tauri-build 同样拷到 target 目录）
//!   2. 用户数据目录 `{data_dir}/drivers/{name}/{version}/{platform}/host(.exe)`
//!      （开发期手动放置 + manifest.json 即被识别）
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 驱动清单条目
pub struct DriverManifest {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
}

/// 已安装状态（与 manifest.json 对应）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstalledMarker {
    name: String,
    version: String,
    sha256: String,
}

/// 当前平台标识
pub fn platform_id() -> &'static str {
    if cfg!(target_os = "windows") {
        if cfg!(target_arch = "x86_64") { "windows-x86_64" }
        else if cfg!(target_arch = "aarch64") { "windows-aarch64" }
        else { "windows-unknown" }
    } else if cfg!(target_os = "linux") {
        if cfg!(target_arch = "x86_64") { "linux-x86_64" }
        else if cfg!(target_arch = "aarch64") { "linux-aarch64" }
        else { "linux-unknown" }
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") { "darwin-aarch64" }
        else { "darwin-x86_64" }
    } else {
        "unknown"
    }
}

fn exe_name() -> &'static str {
    if cfg!(target_os = "windows") { "host.exe" } else { "host" }
}

/// 内置驱动清单（mysql / postgres / sqlite；version 与 drivers workspace 各 crate 版本一致）
pub fn builtin_manifests() -> Vec<DriverManifest> {
    vec![
        DriverManifest {
            name: "mysql".into(),
            version: "0.1.0".into(),
            description: Some("MySQL / MariaDB 驱动".into()),
        },
        DriverManifest {
            name: "postgres".into(),
            version: "0.1.0".into(),
            description: Some("PostgreSQL 驱动".into()),
        },
        DriverManifest {
            name: "sqlite".into(),
            version: "0.1.0".into(),
            description: Some("SQLite 驱动（本地只读）".into()),
        },
    ]
}

pub struct DriverManager {
    pub root: PathBuf,
}

impl DriverManager {
    pub fn new(data_dir: &Path) -> DriverManager {
        DriverManager { root: data_dir.join("drivers") }
    }

    fn dir_for(&self, name: &str, version: &str) -> PathBuf {
        self.root.join(name).join(version).join(platform_id())
    }

    /// base 目录下存在 `drivers/{name}/host(.exe)` 即视为内置可用
    fn bundled_in(base: &Path, name: &str) -> Option<PathBuf> {
        let exe = base.join("drivers").join(name).join(exe_name());
        exe.is_file().then_some(exe)
    }

    /// 安装包内置驱动：exe 同级 `drivers/{name}/host(.exe)`
    pub fn bundled(name: &str) -> Option<PathBuf> {
        std::env::current_exe()
            .ok()?
            .parent()
            .and_then(|p| Self::bundled_in(p, name))
    }

    /// 用户数据目录已安装（手动放置）且校验通过 → 返回可执行文件路径
    pub fn installed(&self, name: &str, version: &str) -> Option<PathBuf> {
        let dir = self.dir_for(name, version);
        let exe = dir.join(exe_name());
        let marker = dir.join("manifest.json");
        if !exe.is_file() || !marker.is_file() {
            return None;
        }
        let text = std::fs::read_to_string(&marker).ok()?;
        let m: InstalledMarker = serde_json::from_str(&text).ok()?;
        if m.name != name || m.version != version {
            return None;
        }
        Some(exe)
    }

    /// 确保驱动可用：安装包内置 → 用户数据目录手动放置。
    pub fn ensure(&self, name: &str) -> Result<PathBuf, String> {
        let manifest = builtin_manifests()
            .into_iter()
            .find(|m| m.name == name)
            .ok_or_else(|| format!("driver_unavailable: 未知驱动 {name}"))?;

        if let Some(exe) = Self::bundled(name) {
            return Ok(exe);
        }
        if let Some(exe) = self.installed(name, &manifest.version) {
            return Ok(exe);
        }

        // 手动安装识别：目录里已有 host(.exe)（开发期放置），只写 marker 跳过校验
        let dir = self.dir_for(name, &manifest.version);
        let exe = dir.join(exe_name());
        if exe.is_file() {
            let marker = InstalledMarker {
                name: name.to_string(),
                version: manifest.version.clone(),
                sha256: String::new(),
            };
            std::fs::create_dir_all(&dir).map_err(|e| format!("driver_unavailable: {e}"))?;
            let _ = std::fs::write(dir.join("manifest.json"), serde_json::to_string(&marker).unwrap_or_default());
            return Ok(exe);
        }

        Err(format!(
            "driver_unavailable: 驱动 {name} 缺失（安装包未内置或已损坏，请重新安装；开发期可手动放置到 {}）",
            dir.display()
        ))
    }

    /// 驱动清单（drivers-list op）；installed = 内置或用户目录已就绪
    pub fn list(&self) -> Vec<(String, String, bool, Option<String>)> {
        builtin_manifests()
            .into_iter()
            .map(|m| {
                let ok =
                    Self::bundled(&m.name).is_some() || self.installed(&m.name, &m.version).is_some();
                (m.name, m.version, ok, m.description)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_and_manifests() {
        assert!(!platform_id().is_empty());
        let ms = builtin_manifests();
        assert_eq!(ms.len(), 3);
        assert!(ms.iter().any(|m| m.name == "mysql"));
        assert!(ms.iter().any(|m| m.name == "postgres"));
        assert!(ms.iter().any(|m| m.name == "sqlite"));
    }

    #[test]
    fn installed_recognizes_manual_drop() {
        let tmp = std::env::temp_dir().join(format!("sql-dm-test-{}", uuid::Uuid::new_v4()));
        let mgr = DriverManager::new(&tmp);
        assert!(mgr.installed("mysql", "0.1.0").is_none());
        // 手动放置：host(.exe) + manifest.json
        let dir = mgr.dir_for("mysql", "0.1.0");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(exe_name()), b"fake").unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            r#"{"name":"mysql","version":"0.1.0","sha256":""}"#,
        )
        .unwrap();
        assert!(mgr.installed("mysql", "0.1.0").is_some());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn bundled_in_resolves_flat_layout() {
        let tmp = std::env::temp_dir().join(format!("sql-bd-test-{}", uuid::Uuid::new_v4()));
        let dir = tmp.join("drivers").join("mysql");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(exe_name()), b"fake").unwrap();
        assert!(DriverManager::bundled_in(&tmp, "mysql").is_some());
        assert!(DriverManager::bundled_in(&tmp, "postgres").is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
