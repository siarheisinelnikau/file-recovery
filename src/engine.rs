use crate::{
    Result,
    fat::{DirectoryTask, FatVolume},
    ntfs::{MAX_RECORDS, NtfsVolume},
    partition,
    source::{self, ReadSource, SourceIdentity},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

pub const MAX_RESULTS: usize = 10_000;
const MAX_DIRECTORIES: usize = 4096;
const CHUNK: u64 = 1024 * 1024;
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
const MAX_INLINE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extent {
    pub offset: u64,
    pub length: u64,
    #[serde(default)]
    pub zero_fill: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryKind {
    FatChain,
    FatContiguous,
    PngCarved,
    NtfsResident,
    NtfsRuns,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Confidence {
    Unverified,
    Partial,
    StructureVerified,
    Unsupported,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub id: u64,
    pub original_path: String,
    pub expected_size: u64,
    pub recovered_size: u64,
    pub extents: Vec<Extent>,
    pub kind: RecoveryKind,
    pub confidence: Confidence,
    pub note: String,
    #[serde(default)]
    pub inline_data: Option<Vec<u8>>,
}

impl Candidate {
    pub fn validate(&self, source_len: u64) -> Result<()> {
        if self.extents.len() > 65_536 || self.original_path.len() > 4096 || self.note.len() > 2048
        {
            return Err("Result exceeds safety limits".into());
        }
        let mut length = self
            .inline_data
            .as_ref()
            .map_or(0, |data| data.len() as u64);
        if self
            .inline_data
            .as_ref()
            .is_some_and(|data| data.len() > 65536)
            || (self.inline_data.is_some() && !self.extents.is_empty())
            || (self.inline_data.is_some() && self.kind != RecoveryKind::NtfsResident)
        {
            return Err("Invalid resident recovery data".into());
        }
        for e in &self.extents {
            if !e.zero_fill {
                source::checked_range(e.offset, e.length, source_len)?;
            }
            if e.zero_fill && !matches!(self.kind, RecoveryKind::NtfsRuns) {
                return Err("Zero-filled ranges are valid only for NTFS streams".into());
            }
            length = length
                .checked_add(e.length)
                .ok_or("Result length overflow")?;
        }
        if length != self.recovered_size
            || length > self.expected_size
            || length > 4 * 1024 * 1024 * 1024
        {
            return Err("Inconsistent recovered length".into());
        }
        Ok(())
    }

    pub fn read_at(&self, source: &mut dyn ReadSource, offset: u64, out: &mut [u8]) -> Result<()> {
        source::checked_range(offset, out.len() as u64, self.recovered_size)?;
        if let Some(data) = &self.inline_data {
            let start = usize::try_from(offset).map_err(crate::io_error)?;
            out.copy_from_slice(
                data.get(
                    start
                        ..start
                            .checked_add(out.len())
                            .ok_or("Resident offset overflow")?,
                )
                .ok_or("Resident content is truncated")?,
            );
            return Ok(());
        }
        let mut logical = 0u64;
        let mut written = 0;
        for extent in &self.extents {
            let end = logical
                .checked_add(extent.length)
                .ok_or("Stream overflow")?;
            if offset + (written as u64) < end {
                let skip = (offset + written as u64).saturating_sub(logical);
                let count = (extent.length - skip).min((out.len() - written) as u64) as usize;
                if extent.zero_fill {
                    out[written..written + count].fill(0);
                } else {
                    source.read_at(extent.offset + skip, &mut out[written..written + count])?;
                }
                written += count;
                if written == out.len() {
                    return Ok(());
                }
            }
            logical = end;
        }
        if out.is_empty() {
            Ok(())
        } else {
            Err("Incomplete recovery layout".into())
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanOptions {
    pub filesystem_offset: Option<u64>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Metadata,
    NtfsMetadata,
    Signatures,
    Finished,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NtfsCursor {
    pub volume: NtfsVolume,
    pub next_record: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScanState {
    version: u32,
    pub identity: SourceIdentity,
    pub options: ScanOptions,
    pub volumes: Vec<FatVolume>,
    #[serde(default)]
    pub ntfs: Vec<NtfsCursor>,
    pub phase: Phase,
    queue: VecDeque<DirectoryTask>,
    visited: Vec<(usize, u32)>,
    pub carve_offset: u64,
    pub results: Vec<Candidate>,
    pub warnings: Vec<String>,
    pub directories_read: u64,
    #[serde(default)]
    pub records_read: u64,
    pub capped: bool,
}

#[derive(Clone, Debug)]
pub struct Progress {
    pub phase: Phase,
    pub directories: u64,
    pub bytes_scanned: u64,
    pub total_bytes: u64,
    pub found: usize,
    pub records: u64,
    pub total_records: u64,
}

impl ScanState {
    pub fn start(source: &mut dyn ReadSource, options: ScanOptions) -> Result<Self> {
        let identity = source.identity()?;
        let regions = partition::discover(source, options.filesystem_offset);
        if options.filesystem_offset.is_some() && regions.is_err() {
            return Err(regions.err().unwrap_or_default());
        }
        let mut warnings = vec![];
        let mut volumes = vec![];
        let mut ntfs = vec![];
        match regions {
            Ok(regions) => {
                if regions.len() > 64 {
                    warnings.push("Volume count exceeds 64; metadata scanning is partial.".into());
                }
                for region in regions.into_iter().take(64) {
                    if source::bytes(source, region.offset, 512)
                        .is_ok_and(|b| &b[3..11] == b"NTFS    ")
                    {
                        match NtfsVolume::parse(source, region.clone()) {
                            Ok(volume) => ntfs.push(NtfsCursor {
                                volume,
                                next_record: 0,
                            }),
                            Err(e) => warnings.push(format!("{}: {e}", region.label)),
                        }
                        continue;
                    }
                    match FatVolume::parse(source, region.clone()) {
                        Ok(v) => volumes.push(v),
                        Err(e) => warnings.push(format!("{}: {e}", region.label)),
                    }
                }
            }
            Err(e) => warnings.push(e),
        }
        if volumes.is_empty() && ntfs.is_empty() {
            warnings.push("No supported FAT16/FAT32/NTFS volume. Only validated PNG carving is available; original paths cannot be recovered.".into());
        }
        let queue = volumes.iter().enumerate().map(|(i, v)| v.root(i)).collect();
        Ok(Self {
            version: 2,
            identity,
            options,
            volumes,
            ntfs,
            phase: Phase::Metadata,
            queue,
            visited: vec![],
            carve_offset: 0,
            results: vec![],
            warnings,
            directories_read: 0,
            records_read: 0,
            capped: false,
        })
    }

    pub fn load(path: &Path, source: &mut dyn ReadSource) -> Result<Self> {
        let length = fs::metadata(path).map_err(crate::io_error)?.len();
        if length > 16 * 1024 * 1024 {
            return Err("Checkpoint exceeds 16 MiB".into());
        }
        let state: Self = serde_json::from_slice(&fs::read(path).map_err(crate::io_error)?)
            .map_err(crate::io_error)?;
        let fresh = Self::start(source, state.options.clone())?;
        if !matches!(state.version, 1 | 2)
            || state.identity != fresh.identity
            || state.volumes != fresh.volumes
            || state.ntfs.len() != fresh.ntfs.len()
            || state.ntfs.iter().zip(&fresh.ntfs).any(|(a, b)| {
                a.volume != b.volume || a.next_record > a.volume.record_count.min(MAX_RECORDS)
            })
        {
            return Err("Checkpoint does not match this source; start a new scan".into());
        }
        if state.results.len() > MAX_RESULTS
            || state.queue.len() > MAX_DIRECTORIES
            || state.visited.len() > MAX_DIRECTORIES
            || state.warnings.len() > 100
            || state.carve_offset > source.len()
            || state.directories_read > MAX_DIRECTORIES as u64
            || state.records_read > MAX_RECORDS * state.ntfs.len() as u64
        {
            return Err("Checkpoint exceeds scan limits".into());
        }
        for task in &state.queue {
            if task.volume >= state.volumes.len() || task.depth > 32 || task.path.len() > 4096 {
                return Err("Invalid directory checkpoint".into());
            }
        }
        for c in &state.results {
            c.validate(source.len())?;
        }
        if state.results.iter().map(|c| c.extents.len()).sum::<usize>() > 262144 {
            return Err("Checkpoint exceeds the extent budget".into());
        }
        if state
            .results
            .iter()
            .filter_map(|c| c.inline_data.as_ref())
            .map(Vec::len)
            .sum::<usize>()
            > MAX_INLINE_BYTES
        {
            return Err("Checkpoint exceeds the resident data budget".into());
        }
        Ok(state)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        crate::settings::atomic_json(path, self)
    }

    fn warning(&mut self, s: String) {
        if self.warnings.len() < 100 && !self.warnings.contains(&s) {
            self.warnings.push(s);
        }
    }

    pub fn progress(&self) -> Progress {
        Progress {
            phase: self.phase,
            directories: self.directories_read,
            bytes_scanned: self.carve_offset,
            total_bytes: self.identity.length,
            found: self.results.len(),
            records: self.records_read,
            total_records: self
                .ntfs
                .iter()
                .map(|v| v.volume.record_count.min(MAX_RECORDS))
                .sum(),
        }
    }

    /// One bounded directory or one 1 MiB carving block. Cancellation leaves a resumable cursor.
    pub fn step(&mut self, source: &mut dyn ReadSource, cancel: &AtomicBool) -> Result<bool> {
        if cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        if self.results.len() >= MAX_RESULTS {
            self.capped = true;
            self.warning("Stopped at the 10,000-result limit. The scan is partial.".into());
            self.phase = Phase::Finished;
        }
        match self.phase {
            Phase::Metadata => {
                if let Some(task) = self.queue.front().cloned() {
                    if self.visited.contains(&(task.volume, task.cluster)) {
                        self.capped = true;
                        self.warning(
                            "Directory alias or cycle skipped; paths may be incomplete.".into(),
                        );
                        self.queue.pop_front();
                        return Ok(true);
                    }
                    if self.visited.len() >= MAX_DIRECTORIES {
                        self.warning(
                            "Directory limit reached. Filesystem traversal is partial.".into(),
                        );
                        self.queue.clear();
                        self.capped = true;
                        return Ok(true);
                    }
                    let read = self.volumes[task.volume].read_directory(source, &task, cancel);
                    if cancel.load(Ordering::Relaxed) {
                        return Ok(false);
                    }
                    self.queue.pop_front();
                    self.visited.push((task.volume, task.cluster));
                    self.directories_read += 1;
                    match read {
                        Ok(read) => {
                            self.capped |= read.limited;
                            let mut total_extents: usize =
                                self.results.iter().map(|c| c.extents.len()).sum();
                            for mut c in read.found {
                                if self.results.len() == MAX_RESULTS {
                                    self.capped = true;
                                    break;
                                }
                                if total_extents + c.extents.len() > 262144 {
                                    self.capped = true;
                                    self.warning(
                                        "Scan extent budget reached; results are partial.".into(),
                                    );
                                    break;
                                }
                                total_extents += c.extents.len();
                                c.id = self.results.len() as u64;
                                c.validate(source.len())?;
                                self.results.push(c);
                            }
                            for dir in read.directories {
                                if self.queue.len() + self.visited.len() < MAX_DIRECTORIES {
                                    self.queue.push_back(dir);
                                } else {
                                    self.capped = true;
                                    self.warning(
                                        "Directory queue limit reached; traversal is partial."
                                            .into(),
                                    );
                                }
                            }
                            for w in read.warnings {
                                self.warning(w);
                            }
                        }
                        Err(e) => {
                            self.capped = true;
                            self.warning(e);
                        }
                    }
                } else {
                    self.phase = Phase::NtfsMetadata;
                }
            }
            Phase::NtfsMetadata => {
                let index = self
                    .ntfs
                    .iter()
                    .position(|c| c.next_record < c.volume.record_count.min(MAX_RECORDS));
                let Some(index) = index else {
                    if self
                        .ntfs
                        .iter()
                        .any(|c| c.volume.record_count > MAX_RECORDS)
                    {
                        self.capped = true;
                        self.warning(
                            "NTFS MFT record limit reached; metadata scan is partial.".into(),
                        );
                    }
                    self.phase = Phase::Signatures;
                    return Ok(true);
                };
                let record = self.ntfs[index].next_record;
                let result = self.ntfs[index].volume.read_deleted(source, record, cancel);
                if cancel.load(Ordering::Relaxed) {
                    return Ok(false);
                }
                self.ntfs[index].next_record += 1;
                self.records_read += 1;
                match result {
                    Ok(read) => {
                        if let Some(w) = read.warning {
                            self.warning(format!("MFT record {record}: {w}"));
                        }
                        if let Some(mut c) = read.candidate {
                            let inline: usize = self
                                .results
                                .iter()
                                .filter_map(|c| c.inline_data.as_ref())
                                .map(Vec::len)
                                .sum();
                            let extents: usize = self.results.iter().map(|c| c.extents.len()).sum();
                            if inline + c.inline_data.as_ref().map_or(0, Vec::len)
                                > MAX_INLINE_BYTES
                                || extents + c.extents.len() > 262144
                            {
                                self.capped = true;
                                self.warning(
                                    "Recovery metadata budget reached; NTFS results are partial."
                                        .into(),
                                );
                                self.ntfs[index].next_record =
                                    self.ntfs[index].volume.record_count.min(MAX_RECORDS);
                            } else {
                                if c.confidence == Confidence::Unsupported {
                                    self.warning(format!("MFT record {record}: {}", c.note));
                                }
                                c.id = self.results.len() as u64;
                                c.validate(source.len())?;
                                self.results.push(c);
                            }
                        }
                    }
                    Err(e) => {
                        self.capped = true;
                        self.warning(format!("MFT record {record}: {e}"));
                    }
                }
            }
            Phase::Signatures => {
                if self.carve_offset >= source.len() {
                    self.phase = Phase::Finished;
                    return Ok(true);
                }
                let end = self.carve_offset.saturating_add(CHUNK).min(source.len());
                let read_end = end.saturating_add(7).min(source.len());
                let data = source::bytes(
                    source,
                    self.carve_offset,
                    (read_end - self.carve_offset) as usize,
                )?;
                let mut consumed = self.carve_offset;
                for (i, window) in data.windows(8).enumerate() {
                    let offset = self.carve_offset + i as u64;
                    if offset >= end {
                        break;
                    }
                    if offset < consumed || window != PNG {
                        continue;
                    }
                    if cancel.load(Ordering::Relaxed) {
                        return Ok(false);
                    }
                    let length = png_length(source, offset, cancel)?;
                    if cancel.load(Ordering::Relaxed) {
                        return Ok(false);
                    }
                    if let Some(length) = length {
                        let overlaps = self.results.iter().any(|c| {
                            c.extents.iter().any(|e| {
                                !e.zero_fill
                                    && e.offset < offset + length
                                    && offset < e.offset + e.length
                            })
                        });
                        if !overlaps {
                            let id = self.results.len() as u64;
                            self.results.push(Candidate { id, original_path: format!("PNG-{offset:016X}.png"), expected_size: length, recovered_size: length,
                                extents: vec![Extent { offset, length, zero_fill:false }], inline_data:None,kind: RecoveryKind::PngCarved, confidence: Confidence::StructureVerified,
                                note: "PNG chunk order, lengths and CRCs validated. Pixel decompression is not verified. Original name/path unknown; the file may also be live.".into() });
                            if self.results.len() == MAX_RESULTS {
                                self.capped = true;
                                break;
                            }
                        }
                        consumed = offset + length;
                    }
                }
                if cancel.load(Ordering::Relaxed) {
                    return Ok(false);
                }
                self.carve_offset = end.max(consumed).min(source.len());
            }
            Phase::Finished => return Ok(false),
        }
        Ok(true)
    }

    pub fn run(
        &mut self,
        source: &mut dyn ReadSource,
        cancel: &AtomicBool,
        mut notify: impl FnMut(Progress),
    ) -> Result<()> {
        while self.step(source, cancel)? {
            notify(self.progress());
        }
        Ok(())
    }
}

fn png_length(source: &mut dyn ReadSource, start: u64, cancel: &AtomicBool) -> Result<Option<u64>> {
    let Some(mut pos) = start.checked_add(8) else {
        return Ok(None);
    };
    let mut idat = false;
    let mut ended_idat = false;
    let mut color = 0u8;
    let mut depth = 0u8;
    let mut palette = false;
    let mut idat_bytes = 0u64;
    for index in 0..4096 {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        if source::checked_range(pos, 12, source.len()).is_err() {
            return Ok(None);
        }
        let header = source::bytes(source, pos, 8)?;
        let count = u32::from_be_bytes(header[..4].try_into().expect("PNG length")) as u64;
        let Some(chunk_end) = pos.checked_add(count + 12) else {
            return Ok(None);
        };
        let kind = &header[4..8];
        if !kind.iter().all(u8::is_ascii_alphabetic)
            || !kind[2].is_ascii_uppercase()
            || count > 64 * 1024 * 1024
            || chunk_end - start > 64 * 1024 * 1024
            || source::checked_range(pos, count + 12, source.len()).is_err()
        {
            return Ok(None);
        }
        if index == 0 {
            if kind != b"IHDR" || count != 13 {
                return Ok(None);
            }
            let h = source::bytes(source, pos + 8, 13)?;
            let width = u32::from_be_bytes(h[..4].try_into().expect("PNG width"));
            let height = u32::from_be_bytes(h[4..8].try_into().expect("PNG height"));
            depth = h[8];
            color = h[9];
            let valid = match color {
                0 => matches!(depth, 1 | 2 | 4 | 8 | 16),
                2 | 4 | 6 => matches!(depth, 8 | 16),
                3 => matches!(depth, 1 | 2 | 4 | 8),
                _ => false,
            };
            if width == 0
                || height == 0
                || width as u64 * height as u64 > 100_000_000
                || !valid
                || h[10] != 0
                || h[11] != 0
                || h[12] > 1
            {
                return Ok(None);
            }
        } else if kind == b"IHDR" {
            return Ok(None);
        }
        if kind == b"PLTE" {
            if palette
                || idat
                || matches!(color, 0 | 4)
                || count == 0
                || count > 768
                || !count.is_multiple_of(3)
                || (color == 3 && count / 3 > 1u64 << depth)
            {
                return Ok(None);
            }
            palette = true;
        }
        if kind == b"IDAT" {
            if ended_idat || (color == 3 && !palette) {
                return Ok(None);
            }
            idat = true;
            idat_bytes += count;
        } else if idat {
            ended_idat = true;
        }
        if kind == b"IEND" && (count != 0 || !idat || idat_bytes == 0) {
            return Ok(None);
        }
        if kind[0].is_ascii_uppercase() && !matches!(kind, b"IHDR" | b"IDAT" | b"IEND" | b"PLTE") {
            return Ok(None);
        }
        let mut crc = crc32fast::Hasher::new();
        crc.update(kind);
        let mut done = 0;
        while done < count {
            if cancel.load(Ordering::Relaxed) {
                return Ok(None);
            }
            let n = (count - done).min(CHUNK) as usize;
            crc.update(&source::bytes(source, pos + 8 + done, n)?);
            done += n as u64;
        }
        let checksum = source::bytes(source, pos + 8 + count, 4)?;
        if crc.finalize() != u32::from_be_bytes(checksum.try_into().expect("PNG CRC")) {
            return Ok(None);
        }
        pos += count + 12;
        if kind == b"IEND" {
            return Ok(Some(pos - start));
        }
    }
    Ok(None)
}
