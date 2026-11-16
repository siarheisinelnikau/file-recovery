use crate::{
    Result,
    engine::{Candidate, Confidence},
    source::{ReadSource, SourceIdentity},
};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

static SERIAL: AtomicU64 = AtomicU64::new(0);
#[derive(Debug, Default)]
pub struct RestoreSummary {
    pub files: Vec<PathBuf>,
    pub bytes: u64,
    pub failed: Vec<String>,
    pub cancelled: bool,
}

struct Pending(PathBuf);
impl Drop for Pending {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn restore(
    source: &mut dyn ReadSource,
    identity: &SourceIdentity,
    chosen: &[Candidate],
    destination: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<RestoreSummary> {
    if source.identity()? != *identity {
        return Err("Source identity changed. Scan this source again before restoring.".into());
    }
    if chosen.len() > crate::engine::MAX_RESULTS {
        return Err("Too many selected files".into());
    }
    let destination = destination.canonicalize().map_err(crate::io_error)?;
    if !destination.is_dir() {
        return Err("Choose an existing destination folder".into());
    }
    if source.is_device() {
        #[cfg(windows)]
        reject_same_disk(&identity.locator, &destination)?;
        #[cfg(not(windows))]
        return Err("Cannot verify the destination device".into());
    }
    let mut total = 0u64;
    for item in chosen {
        item.validate(source.len())?;
        total = total
            .checked_add(item.recovered_size)
            .ok_or("Restore size overflow")?;
    }
    let mut summary = RestoreSummary::default();
    let mut processed = 0u64;
    for item in chosen {
        if cancel.load(Ordering::Relaxed) {
            summary.cancelled = true;
            break;
        }
        let restored = restore_one(source, item, &destination, cancel, |n| {
            progress(processed + n, total)
        });
        match restored {
            Ok(path) => {
                summary.files.push(path);
                summary.bytes += item.recovered_size;
            }
            Err(_) if cancel.load(Ordering::Relaxed) => {
                summary.cancelled = true;
                break;
            }
            Err(e) => summary.failed.push(format!("{}: {e}", item.original_path)),
        }
        if cancel.load(Ordering::Relaxed) {
            summary.cancelled = true;
            break;
        }
        processed += item.recovered_size;
        progress(processed, total);
    }
    Ok(summary)
}

fn restore_one(
    source: &mut dyn ReadSource,
    item: &Candidate,
    dir: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64),
) -> Result<PathBuf> {
    if item.confidence == Confidence::Unsupported {
        return Err("This NTFS layout is unsupported; no output was written".into());
    }
    if item.recovered_size == 0 && item.expected_size != 0 {
        return Err("No readable data for this entry".into());
    }
    let temp = dir.join(format!(
        ".file-recovery-{}-{}.part",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)
        .map_err(crate::io_error)?;
    let pending = Pending(temp.clone());
    let mut buffer = vec![0; 1024 * 1024];
    let mut copied = 0;
    if let Some(data) = &item.inline_data {
        if cancel.load(Ordering::Relaxed) {
            return Err("Restore cancelled".into());
        }
        file.write_all(data).map_err(crate::io_error)?;
        copied = data.len() as u64;
        progress(copied);
    }
    for extent in &item.extents {
        let mut done = 0;
        while done < extent.length {
            if cancel.load(Ordering::Relaxed) {
                return Err("Restore cancelled".into());
            }
            let n = (extent.length - done).min(buffer.len() as u64) as usize;
            if extent.zero_fill {
                buffer[..n].fill(0);
            } else {
                source.read_at(extent.offset + done, &mut buffer[..n])?;
            }
            file.write_all(&buffer[..n]).map_err(crate::io_error)?;
            done += n as u64;
            copied += n as u64;
            progress(copied);
        }
    }
    file.flush().map_err(crate::io_error)?;
    file.sync_all().map_err(crate::io_error)?;
    drop(file);
    if cancel.load(Ordering::Relaxed) {
        return Err("Restore cancelled".into());
    }
    let mut name = safe_name(&item.original_path);
    if item.confidence == Confidence::Partial {
        name.push_str(".partial");
    }
    // Flatten original paths intentionally: no recovered path can create directories or follow links.
    let stem = Path::new(&name)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let extension = Path::new(&name)
        .extension()
        .map(|x| format!(".{}", x.to_string_lossy()))
        .unwrap_or_default();
    for suffix in 0..10_000 {
        let name = if suffix == 0 {
            name.clone()
        } else {
            format!("{stem} ({suffix}){extension}")
        };
        let target = dir.join(name);
        match commit_noreplace(&temp, &target) {
            Ok(()) => {
                drop(pending);
                return Ok(target);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("Too many destination filename collisions".into())
}

pub fn safe_name(path: &str) -> String {
    let base = path.rsplit(['/', '\\']).next().unwrap_or("Recovered");
    let mut out: String = base
        .chars()
        .take(160)
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    out = out.trim_matches([' ', '.']).into();
    if out.is_empty() {
        out = "Recovered".into();
    }
    let reserved = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    if matches!(reserved.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (reserved.len() == 4
            && (reserved.starts_with("COM") || reserved.starts_with("LPT"))
            && reserved.as_bytes()[3].is_ascii_digit())
    {
        out.insert(0, '_');
    }
    out
}

#[cfg(windows)]
pub(crate) fn commit_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
#[cfg(not(windows))]
pub(crate) fn commit_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::hard_link(from, to)?;
    fs::remove_file(from)
}

#[cfg(windows)]
fn reject_same_disk(source_name: &str, destination: &Path) -> Result<()> {
    use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::{
        Storage::FileSystem::{
            FILE_SHARE_READ, FILE_SHARE_WRITE, GetVolumeNameForVolumeMountPointW,
            GetVolumePathNameW,
        },
        System::IO::DeviceIoControl,
    };
    // CTL_CODE(IOCTL_VOLUME_BASE='V', 0, METHOD_BUFFERED, FILE_ANY_ACCESS).
    const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x00560000;
    let path: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut mount = vec![0u16; 32768];
    let mut name = vec![0u16; 32768];
    if unsafe { GetVolumePathNameW(path.as_ptr(), mount.as_mut_ptr(), mount.len() as u32) } == 0
        || unsafe {
            GetVolumeNameForVolumeMountPointW(mount.as_ptr(), name.as_mut_ptr(), name.len() as u32)
        } == 0
    {
        return Err(
            "Cannot establish destination volume; physical-source restore is blocked".into(),
        );
    }
    let end = name
        .iter()
        .position(|c| *c == 0)
        .ok_or("Invalid volume name")?;
    let volume = String::from_utf16_lossy(&name[..end])
        .trim_end_matches('\\')
        .to_string();
    let handle = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(volume)
        .map_err(|e| format!("Cannot verify destination disk: {e}"))?;
    let mut data = [0u8; 4096];
    let mut returned = 0;
    if unsafe {
        DeviceIoControl(
            handle.as_raw_handle(),
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            std::ptr::null(),
            0,
            data.as_mut_ptr().cast(),
            data.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    } == 0
        || returned < 8
    {
        return Err("Destination disk mapping unavailable; restore blocked".into());
    }
    let count = partition_u32(&data, 0) as usize;
    if count == 0 || count > 128 || 8 + count * 24 > returned as usize {
        return Err("Unsupported destination disk mapping".into());
    }
    let source_number: u32 = source_name
        .strip_prefix(r"\\.\PhysicalDrive")
        .ok_or("Invalid physical source")?
        .parse()
        .map_err(crate::io_error)?;
    for index in 0..count {
        if partition_u32(&data, 8 + index * 24) == source_number {
            return Err("Choose a destination on a different physical disk".into());
        }
    }
    Ok(())
}
#[cfg(windows)]
fn partition_u32(b: &[u8], p: usize) -> u32 {
    crate::partition::le32(b, p)
}
