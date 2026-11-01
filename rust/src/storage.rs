use serde::Serialize;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let data = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    atomic_write(path, &data)
}
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("数据文件缺少父目录")?;
    fs::create_dir_all(parent)
        .map_err(|e| format!("无法创建数据目录 {}：{e}", parent.display()))?;
    let temp = parent.join(format!(
        ".sog-{}-{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .map_err(|e| e.to_string())?;
        f.write_all(bytes)
            .and_then(|_| f.sync_all())
            .map_err(|e| e.to_string())?;
        drop(f);
        replace(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
#[cfg(windows)]
fn replace(source: &Path, destination: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        core::PCWSTR,
        Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        },
    };
    let s: Vec<_> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let d: Vec<_> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    unsafe {
        MoveFileExW(
            PCWSTR(s.as_ptr()),
            PCWSTR(d.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|e| format!("原子写入失败，旧文件已保留：{e}"))
}
#[cfg(not(windows))]
fn replace(source: &Path, destination: &Path) -> Result<(), String> {
    fs::rename(source, destination).map_err(|e| e.to_string())
}

pub fn default_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("ScreenOffGuard")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn atomic_unicode_roundtrip_and_failed_replace_preserves_target() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            "test-data-storage-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let file = dir.join("中文 路径/settings.json");
        atomic_write(&file, "原值".as_bytes()).unwrap();
        atomic_write(&file, "新值".as_bytes()).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "新值");
        let blocked = dir.join("directory.json");
        fs::create_dir(&blocked).unwrap();
        assert!(atomic_write(&blocked, b"must fail").is_err());
        assert!(blocked.is_dir());
        assert!(!fs::read_dir(&dir).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
        fs::remove_file(&file).unwrap();
        fs::remove_dir(file.parent().unwrap()).unwrap();
        fs::remove_dir(blocked).unwrap();
        fs::remove_dir(dir).unwrap();
    }
}
