//! Public HTTPS update transport and a recoverable portable replacement helper.
use crate::Result;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

pub const REPOSITORY: &str = "siarheisinelnikau/file-recovery";
const MAX_EXE: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct AvailableUpdate {
    pub version: Version,
    pub url: String,
    pub size: u64,
    pub sha256: String,
}
#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}
#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
    digest: Option<String>,
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_per_read(Some(Duration::from_secs(20)))
        .user_agent("FileRecovery/0.1.0")
        .build()
        .into()
}

pub fn check() -> Result<Option<AvailableUpdate>> {
    let url = format!("https://api.github.com/repos/{REPOSITORY}/releases/latest");
    let mut response = agent(Duration::from_secs(25)).get(url).header("Accept", "application/vnd.github+json").call()
        .map_err(|e| format!("Update check unavailable (including repository/release missing or rate limited): {e}"))?;
    let mut data = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(1024 * 1024 + 1)
        .read_to_end(&mut data)
        .map_err(crate::io_error)?;
    if data.len() > 1024 * 1024 {
        return Err("Release metadata exceeds limit".into());
    }
    select(&data, env!("CARGO_PKG_VERSION"))
}

pub fn select(data: &[u8], installed: &str) -> Result<Option<AvailableUpdate>> {
    let release: Release = serde_json::from_slice(data).map_err(crate::io_error)?;
    let version =
        Version::parse(release.tag_name.trim_start_matches('v')).map_err(crate::io_error)?;
    if release.draft || release.prerelease || !version.pre.is_empty() {
        return Err("Latest release is outside the stable channel".into());
    }
    if version <= Version::parse(installed).map_err(crate::io_error)? {
        return Ok(None);
    }
    #[cfg(not(target_arch = "x86_64"))]
    return Err(
        "Automatic installation is supported only by the Windows x64 portable build".into(),
    );
    #[cfg(target_arch = "x86_64")]
    {
        let expected = format!("FileRecovery-v{version}.exe");
        let asset = release
            .assets
            .into_iter()
            .find(|a| a.name == expected)
            .ok_or("Compatible Windows x64 portable asset is missing")?;
        if !official_url(&asset.browser_download_url) || asset.size < 1024 || asset.size > MAX_EXE {
            return Err("Invalid update asset URL or size".into());
        }
        let sha256 = asset
            .digest
            .and_then(|d| d.strip_prefix("sha256:").map(str::to_owned))
            .ok_or("Release has no SHA-256 asset digest; installation is blocked")?;
        validate_hash(&sha256)?;
        Ok(Some(AvailableUpdate {
            version,
            url: asset.browser_download_url,
            size: asset.size,
            sha256,
        }))
    }
}

fn official_url(url: &str) -> bool {
    url.starts_with(&format!(
        "https://github.com/{REPOSITORY}/releases/download/"
    )) && !url.contains(['?', '#', '\\'])
}
fn validate_hash(hash: &str) -> Result<()> {
    if hash.len() != 64 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
        Err("Invalid SHA-256 digest".into())
    } else {
        Ok(())
    }
}

pub fn download(
    update: &AvailableUpdate,
    parent: &Path,
    cancel: &AtomicBool,
    progress: impl FnMut(u64, u64),
) -> Result<PathBuf> {
    if !official_url(&update.url) || update.size > MAX_EXE {
        return Err("Invalid update asset".into());
    }
    validate_hash(&update.sha256)?;
    let staged = parent.join(format!(".FileRecovery-update-{}.exe", std::process::id()));
    let mut response = agent(Duration::from_secs(300))
        .get(&update.url)
        .call()
        .map_err(crate::io_error)?;
    stage_payload(
        response.body_mut().as_reader(),
        &staged,
        update,
        cancel,
        progress,
    )?;
    Ok(staged)
}

pub(crate) fn stage_payload(
    mut reader: impl Read,
    staged: &Path,
    update: &AvailableUpdate,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<()> {
    let mut owned = false;
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(staged)
            .map_err(crate::io_error)?;
        owned = true;
        let mut hash = Sha256::new();
        let mut done = 0u64;
        let mut buffer = [0u8; 65536];
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err("Update download cancelled".into());
            }
            let count = reader.read(&mut buffer).map_err(crate::io_error)?;
            if count == 0 {
                break;
            }
            done += count as u64;
            if done > update.size {
                return Err("Downloaded asset exceeds published size".into());
            }
            hash.update(&buffer[..count]);
            file.write_all(&buffer[..count]).map_err(crate::io_error)?;
            progress(done, update.size);
        }
        if done != update.size
            || format!("{:x}", hash.finalize()) != update.sha256.to_ascii_lowercase()
        {
            return Err("Update length or SHA-256 verification failed".into());
        }
        file.sync_all().map_err(crate::io_error)?;
        drop(file);
        verify_executable(staged, &update.sha256)?;
        if cancel.load(Ordering::Relaxed) {
            return Err("Update download cancelled".into());
        }
        Ok(())
    })();
    if result.is_err() && owned {
        let _ = fs::remove_file(staged);
    }
    result
}

fn verify_executable(path: &Path, expected: &str) -> Result<()> {
    let mut file = fs::File::open(path).map_err(crate::io_error)?;
    let size = file.metadata().map_err(crate::io_error)?.len();
    if !(1024..=MAX_EXE).contains(&size) {
        return Err("Invalid staged executable size".into());
    }
    let mut first = [0u8; 64];
    file.read_exact(&mut first).map_err(crate::io_error)?;
    if &first[..2] != b"MZ" {
        return Err("Update is not a Windows executable".into());
    }
    let pe = crate::partition::le32(&first, 60) as u64;
    if !(64..=1024 * 1024).contains(&pe) || pe + 6 > size {
        return Err("Invalid PE header offset".into());
    }
    file.seek(SeekFrom::Start(pe)).map_err(crate::io_error)?;
    let mut signature = [0u8; 6];
    file.read_exact(&mut signature).map_err(crate::io_error)?;
    if &signature[..4] != b"PE\0\0" || u16::from_le_bytes([signature[4], signature[5]]) != 0x8664 {
        return Err("Update must be a Windows x64 executable".into());
    }
    file.seek(SeekFrom::Start(0)).map_err(crate::io_error)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer).map_err(crate::io_error)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    if format!("{:x}", hash.finalize()) != expected.to_ascii_lowercase() {
        return Err("Staged update digest changed".into());
    }
    Ok(())
}

#[cfg(windows)]
pub fn launch_installer(staged: &Path, expected: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;
    verify_executable(staged, expected)?;
    let current = std::env::current_exe().map_err(crate::io_error)?;
    let helper = current
        .parent()
        .ok_or("Missing executable directory")?
        .join(format!(".FileRecovery-helper-{}.exe", std::process::id()));
    // create_new protects any existing helper. The current executable is the trusted helper.
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&helper)
        .map_err(crate::io_error)?;
    std::io::copy(
        &mut fs::File::open(&current).map_err(crate::io_error)?,
        &mut file,
    )
    .map_err(crate::io_error)?;
    file.sync_all().map_err(crate::io_error)?;
    drop(file);
    std::process::Command::new(&helper)
        .arg("--apply-update")
        .arg(std::process::id().to_string())
        .arg(&current)
        .arg(staged)
        .arg(expected)
        .creation_flags(0x0800_0000)
        .spawn()
        .map_err(crate::io_error)?;
    Ok(())
}

#[cfg(windows)]
pub fn apply_update(pid: u32, target: &Path, staged: &Path, expected: &str) -> Result<()> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };
    let helper = std::env::current_exe()
        .map_err(crate::io_error)?
        .canonicalize()
        .map_err(crate::io_error)?;
    let parent = helper.parent().ok_or("No updater directory")?;
    let target = target.canonicalize().map_err(crate::io_error)?;
    let staged = staged.canonicalize().map_err(crate::io_error)?;
    if target.parent() != Some(parent)
        || staged.parent() != Some(parent)
        || !staged
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .starts_with(".FileRecovery-update-")
        || target == helper
    {
        return Err("Updater paths must be in the application directory".into());
    }
    validate_hash(expected)?;
    verify_executable(&staged, expected)?;
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if !process.is_null() {
        let wait = unsafe { WaitForSingleObject(process, 120_000) };
        unsafe {
            CloseHandle(process);
        }
        if wait != WAIT_OBJECT_0 {
            return Err("Application has not exited; original executable preserved".into());
        }
    }
    let backup = parent.join(format!(".FileRecovery-backup-{pid}.exe"));
    replace_staged(&target, &staged, &backup)?;
    match std::process::Command::new(&target).spawn() {
        Ok(_) => {
            let _ = fs::remove_file(backup);
            Ok(())
        }
        Err(e) => {
            let failed = parent.join(format!(".FileRecovery-failed-{pid}.exe"));
            let _ = crate::restore::commit_noreplace(&target, &failed);
            let rollback = crate::restore::commit_noreplace(&backup, &target);
            Err(format!("Restart failed: {e}; rollback: {rollback:?}"))
        }
    }
}

pub(crate) fn replace_staged(target: &Path, staged: &Path, backup: &Path) -> Result<()> {
    crate::restore::commit_noreplace(target, backup).map_err(crate::io_error)?;
    if let Err(e) = crate::restore::commit_noreplace(staged, target) {
        let rollback = crate::restore::commit_noreplace(backup, target);
        return Err(format!(
            "Update replacement failed: {e}; rollback: {rollback:?}"
        ));
    }
    Ok(())
}
