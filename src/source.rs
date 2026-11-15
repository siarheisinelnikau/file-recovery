//! The engine can read a source, but its interface deliberately cannot write one.
use crate::{Result, io_error};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceIdentity {
    pub locator: String,
    pub length: u64,
    pub modified_ns: u128,
    /// Sparse fingerprint, not a promise that every byte has remained unchanged.
    pub sample_sha256: String,
}

pub trait ReadSource: Send {
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn read_at(&mut self, offset: u64, bytes: &mut [u8]) -> Result<()>;
    fn identity(&mut self) -> Result<SourceIdentity>;
    fn image_path(&self) -> Option<&Path> {
        None
    }
    fn is_device(&self) -> bool {
        false
    }
}

pub fn checked_range(offset: u64, length: u64, bound: u64) -> Result<()> {
    if offset.checked_add(length).is_none_or(|end| end > bound) {
        return Err("Read extends beyond the source boundary".into());
    }
    Ok(())
}

pub fn bytes(source: &mut dyn ReadSource, offset: u64, count: usize) -> Result<Vec<u8>> {
    if count > 16 * 1024 * 1024 {
        return Err("Read exceeds the allocation limit".into());
    }
    checked_range(offset, count as u64, source.len())?;
    let mut data = vec![0; count];
    source.read_at(offset, &mut data)?;
    Ok(data)
}

pub struct ImageSource {
    file: File,
    path: PathBuf,
    length: u64,
    modified_ns: u128,
}

impl ImageSource {
    pub fn open(path: &Path) -> Result<Self> {
        let path = path.canonicalize().map_err(io_error)?;
        #[cfg(windows)]
        let file = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ)
                .open(&path)
                .map_err(io_error)?
        };
        #[cfg(not(windows))]
        let file = File::open(&path).map_err(io_error)?;
        let meta = file.metadata().map_err(io_error)?;
        if !meta.is_file() || meta.len() < 512 {
            return Err("Choose a regular raw disk image of at least 512 bytes".into());
        }
        let modified_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        Ok(Self {
            length: meta.len(),
            modified_ns,
            path,
            file,
        })
    }
}

fn fingerprint(source: &mut dyn ReadSource) -> Result<String> {
    let length = source.len();
    let count = length.min(65536) as usize;
    let mut hash = Sha256::new();
    hash.update(length.to_le_bytes());
    for offset in [
        0,
        length.saturating_sub(count as u64) / 2,
        length.saturating_sub(count as u64),
    ] {
        hash.update(offset.to_le_bytes());
        hash.update(bytes(source, offset, count)?);
    }
    Ok(format!("{:x}", hash.finalize()))
}

impl ReadSource for ImageSource {
    fn len(&self) -> u64 {
        self.length
    }
    fn read_at(&mut self, offset: u64, bytes: &mut [u8]) -> Result<()> {
        checked_range(offset, bytes.len() as u64, self.length)?;
        self.file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        self.file.read_exact(bytes).map_err(io_error)
    }
    fn identity(&mut self) -> Result<SourceIdentity> {
        // Refresh metadata: a changed open image must not retain its old identity.
        let meta = self.file.metadata().map_err(io_error)?;
        if meta.len() != self.length {
            return Err("Source length changed; reopen and scan again".into());
        }
        self.modified_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        Ok(SourceIdentity {
            locator: self.path.to_string_lossy().into_owned(),
            length: self.length,
            modified_ns: self.modified_ns,
            sample_sha256: fingerprint(self)?,
        })
    }
    fn image_path(&self) -> Option<&Path> {
        Some(&self.path)
    }
}

/// Explicit device access only; no enumeration, writes, repair, or elevation.
#[cfg(windows)]
pub struct PhysicalSource {
    file: File,
    length: u64,
    name: String,
}

#[cfg(windows)]
impl PhysicalSource {
    pub fn open(name: &str) -> Result<Self> {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::{
            Storage::FileSystem::{FILE_FLAG_RANDOM_ACCESS, FILE_SHARE_READ, FILE_SHARE_WRITE},
            System::{IO::DeviceIoControl, Ioctl::IOCTL_DISK_GET_LENGTH_INFO},
        };
        let number = name
            .strip_prefix(r"\\.\PhysicalDrive")
            .ok_or(r"Expected \\.\PhysicalDriveN")?;
        if number.is_empty() || number.len() > 4 || !number.bytes().all(|b| b.is_ascii_digit()) {
            return Err("Invalid physical device name".into());
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_RANDOM_ACCESS)
            .open(name)
            .map_err(|e| format!("Cannot open device read-only (no elevation requested): {e}"))?;
        let mut length: i64 = 0;
        let mut returned = 0;
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                IOCTL_DISK_GET_LENGTH_INFO,
                std::ptr::null(),
                0,
                (&mut length as *mut i64).cast(),
                8,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || returned != 8 || length < 512 {
            return Err(format!(
                "Device size unavailable: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self {
            file,
            length: length as u64,
            name: name.into(),
        })
    }
}

#[cfg(windows)]
impl ReadSource for PhysicalSource {
    fn len(&self) -> u64 {
        self.length
    }
    fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<()> {
        checked_range(offset, output.len() as u64, self.length)?;
        if output.is_empty() {
            return Ok(());
        }
        // Buffered aligned reads accommodate both 512-byte and 4 KiB devices.
        let start = offset / 4096 * 4096;
        let end = offset
            .checked_add(output.len() as u64)
            .and_then(|e| e.checked_add(4095))
            .ok_or("Device offset overflow")?
            / 4096
            * 4096;
        let end = end.min(self.length);
        let span = usize::try_from(end - start).map_err(io_error)?;
        if span > 16 * 1024 * 1024 {
            return Err("Device read exceeds limit".into());
        }
        let mut data = vec![0; span];
        self.file.seek(SeekFrom::Start(start)).map_err(io_error)?;
        self.file.read_exact(&mut data).map_err(io_error)?;
        let skip = (offset - start) as usize;
        output.copy_from_slice(&data[skip..skip + output.len()]);
        Ok(())
    }
    fn identity(&mut self) -> Result<SourceIdentity> {
        Ok(SourceIdentity {
            locator: self.name.clone(),
            length: self.length,
            modified_ns: 0,
            sample_sha256: fingerprint(self)?,
        })
    }
    fn is_device(&self) -> bool {
        true
    }
}

pub fn open(locator: &str) -> Result<Box<dyn ReadSource>> {
    if locator.starts_with(r"\\.\") {
        #[cfg(windows)]
        {
            return Ok(Box::new(PhysicalSource::open(locator)?));
        }
        #[cfg(not(windows))]
        {
            return Err("Physical sources require Windows".into());
        }
    }
    Ok(Box::new(ImageSource::open(Path::new(locator))?))
}

#[cfg(test)]
pub(crate) struct MemorySource(pub Vec<u8>);
#[cfg(test)]
impl ReadSource for MemorySource {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<()> {
        checked_range(offset, out.len() as u64, self.len())?;
        out.copy_from_slice(&self.0[offset as usize..offset as usize + out.len()]);
        Ok(())
    }
    fn identity(&mut self) -> Result<SourceIdentity> {
        Ok(SourceIdentity {
            locator: "synthetic-memory".into(),
            length: self.len(),
            modified_ns: 0,
            sample_sha256: fingerprint(self)?,
        })
    }
}
