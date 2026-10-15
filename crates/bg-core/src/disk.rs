//! 本地磁盘预检（下载方向）：剩余空间、卷格式、单文件大小上限。
//! FAT32 单文件上限 4GiB-1（准确值 0xFFFFFFFF 字节）；NTFS/exFAT/ReFS 视为无限制。
use std::path::Path;

/// FAT32 单文件上限（0xFFFFFFFF）
pub const FAT32_MAX_FILE: u64 = 4 * 1024 * 1024 * 1024 - 1;

/// 本地路径所在磁盘的可用字节数（含配额；失败 → None）
pub fn available_space(path: &Path) -> Option<u64> {
    // 目标可能尚不存在：逐级回退到最近存在的祖先（盘符根一定存在）
    let mut cur = Some(path);
    while let Some(p) = cur {
        match fs2::available_space(p) {
            Ok(n) => return Some(n),
            Err(_) => cur = p.parent(),
        }
    }
    None
}

/// 本地路径所在卷的文件系统格式（"FAT32"/"NTFS"/"exFAT"/…；失败 → None）
#[cfg(windows)]
pub fn fs_name(path: &Path) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;

    // 逐级回退到最近存在的祖先，取其盘根
    let mut cur = Some(path);
    while let Some(p) = cur {
        if p.exists() {
            let root = ancestor_root(p);
            let wide: Vec<u16> = root
                .to_string_lossy()
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let mut name_buf = [0u16; 32];
            let mut fs_buf = [0u16; 32];
            let ok = unsafe {
                GetVolumeInformationW(
                    wide.as_ptr(),
                    name_buf.as_mut_ptr(),
                    name_buf.len() as u32,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    fs_buf.as_mut_ptr(),
                    fs_buf.len() as u32,
                )
            };
            if ok != 0 {
                let len = fs_buf.iter().position(|&c| c == 0).unwrap_or(0);
                return Some(String::from_utf16_lossy(&fs_buf[..len]));
            }
            return None;
        }
        cur = p.parent();
    }
    None
}

/// 路径的盘根（Windows：`C:\`；其他平台：`/`）
#[cfg(windows)]
fn ancestor_root(p: &Path) -> std::path::PathBuf {
    let s = p.to_string_lossy();
    if let Some(idx) = s.find(':') {
        return std::path::PathBuf::from(format!("{}\\", &s[..=idx]));
    }
    std::path::PathBuf::from("\\")
}

#[cfg(not(windows))]
pub fn fs_name(_path: &Path) -> Option<String> {
    None // 非 Windows 宿主当前不涉及
}

/// 单文件大小上限：FAT32 → 4GiB-1；其他/未知 → None（不设限）
pub fn max_file_bytes(fs: &str) -> Option<u64> {
    if fs.eq_ignore_ascii_case("FAT32") || fs.eq_ignore_ascii_case("FAT") {
        Some(FAT32_MAX_FILE)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fat32_limit_mapping() {
        assert_eq!(max_file_bytes("FAT32"), Some(FAT32_MAX_FILE));
        assert_eq!(max_file_bytes("fat"), Some(FAT32_MAX_FILE));
        assert_eq!(max_file_bytes("NTFS"), None);
        assert_eq!(max_file_bytes("exFAT"), None);
        assert_eq!(FAT32_MAX_FILE, 4_294_967_295);
    }

    #[test]
    fn available_space_of_temp() {
        // 系统临时目录一定能取到剩余空间
        assert!(available_space(&std::env::temp_dir()).is_some());
        // 不存在的深层路径也应回退成功
        let deep = std::env::temp_dir().join("bg/x/y/z");
        assert!(available_space(&deep).is_some());
    }

    #[cfg(windows)]
    #[test]
    fn fs_name_of_temp() {
        let name = fs_name(&std::env::temp_dir());
        assert!(name.is_some());
    }
}
