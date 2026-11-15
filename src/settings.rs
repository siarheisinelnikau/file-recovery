use crate::Result;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Theme {
    #[default]
    Light,
    Dark,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: Theme,
    pub language: String,
    pub reduced_motion: bool,
    pub startup_updates: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::Light,
            language: "en".into(),
            reduced_motion: false,
            startup_updates: true,
        }
    }
}

impl Settings {
    pub fn directory() -> PathBuf {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("FileRecovery")
    }
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        if fs::metadata(path).map_err(crate::io_error)?.len() > 64 * 1024 {
            return Err("Preferences file is too large".into());
        }
        let mut s: Self = serde_json::from_slice(&fs::read(path).map_err(crate::io_error)?)
            .map_err(crate::io_error)?;
        if !["en", "ru", "fr", "de", "es", "it"].contains(&s.language.as_str()) {
            s.language = "en".into();
        }
        Ok(s)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        atomic_json(path, self)
    }
}

pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().ok_or("No preferences directory")?;
    fs::create_dir_all(parent).map_err(crate::io_error)?;
    let data = serde_json::to_vec(value).map_err(crate::io_error)?;
    if data.len() > 16 * 1024 * 1024 {
        return Err("State exceeds persistence limit".into());
    }
    let temp = parent.join(format!(
        ".state-{}-{}.tmp",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .map_err(crate::io_error)?;
        file.write_all(&data).map_err(crate::io_error)?;
        file.sync_all().map_err(crate::io_error)?;
        drop(file);
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            use windows_sys::Win32::Storage::FileSystem::{
                MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
            };
            let a: Vec<u16> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
            let b: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            if unsafe {
                MoveFileExW(
                    a.as_ptr(),
                    b.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error().to_string());
            }
        }
        #[cfg(not(windows))]
        fs::rename(&temp, path).map_err(crate::io_error)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
