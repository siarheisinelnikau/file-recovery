//! Bounded FAT16/FAT32 reader. FAT12, exFAT, NTFS and repair are not implemented.
use crate::{
    Result,
    engine::{Candidate, Confidence, Extent, RecoveryKind},
    partition::{Region, le16, le32},
    source::{self, ReadSource},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    sync::atomic::{AtomicBool, Ordering},
};

const MAX_CHAIN: usize = 65_536;
const MAX_DIRECTORY_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FatVolume {
    pub region: Region,
    pub fat32: bool,
    sector: u64,
    cluster_size: u64,
    cluster_count: u32,
    fat_offset: u64,
    fat_bytes: u64,
    root_offset: u64,
    root_bytes: u64,
    root_cluster: u32,
    data_offset: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectoryTask {
    pub volume: usize,
    pub cluster: u32,
    pub path: String,
    pub depth: u8,
}

pub struct DirectoryRead {
    pub found: Vec<Candidate>,
    pub directories: Vec<DirectoryTask>,
    pub warnings: Vec<String>,
    pub limited: bool,
}

impl FatVolume {
    pub fn parse(source: &mut dyn ReadSource, region: Region) -> Result<Self> {
        let b = source::bytes(source, region.offset, 512)?;
        let sector = le16(&b, 11) as u64;
        let spc = b[13] as u64;
        let reserved = le16(&b, 14) as u64;
        let copies = b[16] as u64;
        let roots = le16(&b, 17) as u64;
        let sectors16 = le16(&b, 19) as u64;
        let sectors = if sectors16 == 0 {
            le32(&b, 32) as u64
        } else {
            sectors16
        };
        let fat16_sectors = le16(&b, 22) as u64;
        let fat_sectors = if fat16_sectors == 0 {
            le32(&b, 36) as u64
        } else {
            fat16_sectors
        };
        if b[510..512] != [0x55, 0xaa]
            || !matches!(sector, 512 | 1024 | 2048 | 4096)
            || !spc.is_power_of_two()
            || spc > 128
            || reserved == 0
            || !matches!(copies, 1 | 2)
            || fat_sectors == 0
        {
            return Err("Unsupported or invalid FAT boot sector".into());
        }
        let root_sectors = (roots * 32).div_ceil(sector);
        let overhead = reserved + copies * fat_sectors + root_sectors;
        if overhead >= sectors || sectors * sector > region.length {
            return Err("FAT geometry exceeds partition".into());
        }
        let clusters = (sectors - overhead) / spc;
        if !(4085..0x0fff_fff5).contains(&clusters) {
            return Err("FAT12 or invalid FAT cluster count is unsupported".into());
        }
        let fat32 = clusters >= 65525;
        if (fat32 && (roots != 0 || fat16_sectors != 0 || le16(&b, 42) != 0))
            || (!fat32 && (roots == 0 || fat16_sectors == 0))
        {
            return Err("Inconsistent FAT type/geometry".into());
        }
        if fat_sectors * sector / if fat32 { 4 } else { 2 } < clusters + 2 {
            return Err("FAT is too short for its data area".into());
        }
        let active = if fat32 && le16(&b, 40) & 0x80 != 0 {
            (le16(&b, 40) & 15) as u64
        } else {
            0
        };
        if active >= copies {
            return Err("Invalid active FAT".into());
        }
        let root_cluster = if fat32 { le32(&b, 44) & 0x0fff_ffff } else { 0 };
        if fat32 && !(2..clusters as u32 + 2).contains(&root_cluster) {
            return Err("Invalid FAT32 root cluster".into());
        }
        let base = region.offset;
        Ok(Self {
            region,
            fat32,
            sector,
            cluster_size: sector * spc,
            cluster_count: clusters as u32,
            fat_offset: base + (reserved + active * fat_sectors) * sector,
            fat_bytes: fat_sectors * sector,
            root_offset: base + (reserved + copies * fat_sectors) * sector,
            root_bytes: roots * 32,
            root_cluster,
            data_offset: base + overhead * sector,
        })
    }

    pub fn root(&self, volume: usize) -> DirectoryTask {
        DirectoryTask {
            volume,
            cluster: self.root_cluster,
            path: String::new(),
            depth: 0,
        }
    }

    fn valid_cluster(&self, c: u32) -> bool {
        c >= 2 && c < self.cluster_count + 2 && c < if self.fat32 { 0x0fff_fff0 } else { 0xfff0 }
    }
    fn offset(&self, c: u32) -> Result<u64> {
        if !self.valid_cluster(c) {
            return Err("Cluster outside data area".into());
        }
        Ok(self.data_offset + (c as u64 - 2) * self.cluster_size)
    }
    fn next(&self, source: &mut dyn ReadSource, c: u32) -> Result<u32> {
        let stride = if self.fat32 { 4 } else { 2 };
        let position = c as u64 * stride;
        source::checked_range(position, stride, self.fat_bytes)?;
        let b = source::bytes(source, self.fat_offset + position, stride as usize)?;
        Ok(if self.fat32 {
            le32(&b, 0) & 0x0fff_ffff
        } else {
            le16(&b, 0) as u32
        })
    }
    fn end(&self, c: u32) -> bool {
        c >= if self.fat32 { 0x0fff_fff8 } else { 0xfff8 }
    }

    fn layout(
        &self,
        source: &mut dyn ReadSource,
        start: u32,
        size: u64,
        deleted: bool,
        cancel: &AtomicBool,
    ) -> Result<(Vec<Extent>, bool, bool)> {
        if size == 0 {
            return Ok((vec![], true, false));
        }
        if !self.valid_cluster(start) {
            return Ok((vec![], false, false));
        }
        let contiguous = deleted && self.next(source, start)? == 0;
        let mut c = start;
        let mut seen = HashSet::new();
        let mut remaining = size;
        let mut extents: Vec<Extent> = vec![];
        for _ in 0..MAX_CHAIN {
            if cancel.load(Ordering::Relaxed) {
                return Err("Scan cancelled".into());
            }
            if !self.valid_cluster(c) || !seen.insert(c) {
                break;
            }
            let next = self.next(source, c)?;
            if contiguous && next != 0 {
                break;
            } // Never infer continuity through allocated clusters.
            let count = remaining.min(self.cluster_size);
            let offset = self.offset(c)?;
            if let Some(last) = extents
                .last_mut()
                .filter(|last| last.offset + last.length == offset)
            {
                last.length += count;
            } else {
                extents.push(Extent {
                    offset,
                    length: count,
                });
            }
            remaining -= count;
            if remaining == 0 {
                return Ok((extents, true, contiguous));
            }
            if contiguous {
                c += 1;
            } else if self.end(next) || !self.valid_cluster(next) {
                break;
            } else {
                c = next;
            }
        }
        Ok((extents, false, contiguous))
    }

    pub fn read_directory(
        &self,
        source: &mut dyn ReadSource,
        task: &DirectoryTask,
        cancel: &AtomicBool,
    ) -> Result<DirectoryRead> {
        let (extents, complete) = if !self.fat32 && task.depth == 0 {
            (
                vec![Extent {
                    offset: self.root_offset,
                    length: self.root_bytes,
                }],
                true,
            )
        } else {
            // Directory length has no metadata; read until EOC with a strict byte cap.
            let mut c = task.cluster;
            let mut seen = HashSet::new();
            let mut extents = vec![];
            let mut complete = false;
            while self.valid_cluster(c) && extents.len() < MAX_CHAIN && seen.insert(c) {
                if cancel.load(Ordering::Relaxed) {
                    return Err("Scan cancelled".into());
                }
                if (extents.len() as u64 + 1) * self.cluster_size > MAX_DIRECTORY_BYTES as u64 {
                    break;
                }
                extents.push(Extent {
                    offset: self.offset(c)?,
                    length: self.cluster_size,
                });
                let next = self.next(source, c)?;
                if self.end(next) {
                    complete = true;
                    break;
                }
                c = next;
            }
            (extents, complete)
        };
        let length: u64 = extents.iter().map(|e| e.length).sum();
        if length > MAX_DIRECTORY_BYTES as u64 {
            return Err("Directory exceeds 8 MiB limit".into());
        }
        let mut data = Vec::with_capacity(length as usize);
        for e in extents {
            data.extend(source::bytes(source, e.offset, e.length as usize)?);
        }
        let mut out = DirectoryRead {
            found: vec![],
            directories: vec![],
            warnings: vec![],
            limited: false,
        };
        let mut extent_count = 0usize;
        if !complete {
            out.warnings.push(format!(
                "Truncated, cyclic or oversized directory: {}",
                task.path
            ));
        }
        let mut lfn: Vec<[u8; 32]> = vec![];
        for (slot, row) in data.as_chunks::<32>().0.iter().enumerate() {
            if slot >= 32768
                || out.found.len() >= 2048
                || out.directories.len() >= 4096
                || extent_count >= 262144
            {
                out.limited = true;
                out.warnings
                    .push("Directory result/entry limit reached; traversal is partial.".into());
                break;
            }
            if cancel.load(Ordering::Relaxed) {
                return Err("Scan cancelled".into());
            }
            if row[0] == 0 {
                break;
            }
            if row[11] == 0x0f {
                if lfn.len() < 20 {
                    lfn.push(*row);
                } else {
                    lfn.clear();
                }
                continue;
            }
            let deleted = row[0] == 0xe5;
            let name = long_name(&lfn, row, deleted).unwrap_or_else(|| short_name(row));
            lfn.clear();
            if row[11] & 0x08 != 0 || name == "." || name == ".." || name.is_empty() {
                continue;
            }
            let cluster = (if self.fat32 {
                ((le16(row, 20) as u32) & 0x0fff) << 16
            } else {
                0
            }) | le16(row, 26) as u32;
            let path = if task.path.is_empty() {
                name
            } else {
                format!("{}/{}", task.path, name)
            };
            if path.len() > 4096 {
                out.limited = true;
                continue;
            }
            if row[11] & 0x10 != 0 {
                // Deleted directories with cleared FAT cannot safely establish child paths.
                if !deleted && task.depth < 32 && self.valid_cluster(cluster) {
                    out.directories.push(DirectoryTask {
                        volume: task.volume,
                        cluster,
                        path,
                        depth: task.depth + 1,
                    });
                } else if out.warnings.len() < 98 {
                    out.limited = true;
                    out.warnings.push(format!(
                        "Deleted, invalid or depth-limited directory not traversed: {path}"
                    ));
                }
                continue;
            }
            if !deleted {
                continue;
            }
            let size = le32(row, 28) as u64;
            let (extents, full, contiguous) = self.layout(source, cluster, size, true, cancel)?;
            extent_count += extents.len();
            let recovered: u64 = extents.iter().map(|e| e.length).sum();
            out.found.push(Candidate { id: 0, original_path: path, expected_size: size, recovered_size: recovered, extents,
                kind: if contiguous { RecoveryKind::FatContiguous } else { RecoveryKind::FatChain },
                confidence: if full { Confidence::Unverified } else { Confidence::Partial },
                note: if !full { "Only a bounded prefix is readable; the chain is invalid, cyclic, allocated or truncated." }
                    else if contiguous { "FAT entries were cleared. Contiguous free clusters are an inference; fragmentation/overwrite cannot be excluded." }
                    else { "A retained FAT chain supplies the layout. Deleted data may have been reused; verify the restored file." }.into() });
        }
        Ok(out)
    }
}

fn checksum(name: &[u8]) -> u8 {
    name[..11]
        .iter()
        .fold(0u8, |v, b| v.rotate_right(1).wrapping_add(*b))
}
fn long_name(parts: &[[u8; 32]], short: &[u8], deleted: bool) -> Option<String> {
    if parts.is_empty() || parts.len() > 20 {
        return None;
    }
    let expected = parts[0][13];
    if parts
        .iter()
        .any(|r| r[13] != expected || r[12] != 0 || r[26] != 0 || r[27] != 0)
    {
        return None;
    }
    if deleted {
        if parts.iter().any(|r| r[0] != 0xe5) {
            return None;
        }
        let mut restored = short[..11].to_vec();
        if !(1..=255)
            .filter(|b| !matches!(*b, 0xe5 | 0x20 | 0x2e))
            .any(|first| {
                restored[0] = first;
                checksum(&restored) == expected
            })
        {
            return None;
        }
    } else {
        if checksum(short) != expected || parts[0][0] != (parts.len() as u8 | 0x40) {
            return None;
        }
        for (i, r) in parts.iter().rev().enumerate() {
            if r[0] & 0x3f != i as u8 + 1 {
                return None;
            }
        }
    }
    let mut chars = vec![];
    for row in parts.iter().rev() {
        for index in [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30] {
            chars.push(le16(row, index));
        }
    }
    let end = chars
        .iter()
        .position(|c| *c == 0 || *c == 0xffff)
        .unwrap_or(chars.len());
    let name = String::from_utf16(&chars[..end]).ok()?;
    if name.is_empty()
        || name.len() > 1024
        || name
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control())
    {
        return None;
    }
    Some(name)
}

fn short_name(row: &[u8]) -> String {
    let part = |b: &[u8]| -> String {
        b.iter()
            .map(|c| {
                if *c == 0xe5 {
                    '_'
                } else if (0x20..=0x7e).contains(c) {
                    *c as char
                } else {
                    '_'
                }
            })
            .collect::<String>()
            .trim_end()
            .into()
    };
    let base = part(&row[..8]);
    let ext = part(&row[8..11]);
    if ext.is_empty() {
        base
    } else {
        format!("{base}.{ext}")
    }
}
