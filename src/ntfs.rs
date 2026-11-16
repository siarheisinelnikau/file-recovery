//! Read-only NTFS 3.0/3.1 MFT recovery; no mounting, repair, or platform filesystem APIs.
//! Layout references: Microsoft FILE_RECORD_SEGMENT_HEADER, MULTI_SECTOR_HEADER,
//! ATTRIBUTE_RECORD_HEADER (learn.microsoft.com/en-us/windows/win32/devnotes/).
use crate::{
    Result,
    engine::{Candidate, Confidence, Extent, RecoveryKind},
    partition::{Region, le16, le32, le64},
    source::{self, ReadSource},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    sync::atomic::{AtomicBool, Ordering},
};

pub const MAX_RECORDS: u64 = 1_000_000;
const MAX_RUNS: usize = 4096;
const MAX_RECOVERY: u64 = 4 * 1024 * 1024 * 1024;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Run {
    vcn: u64,
    clusters: u64,
    lcn: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Stream {
    resident: Option<Vec<u8>>,
    runs: Vec<Run>,
    size: u64,
    initialized: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NtfsVolume {
    pub region: Region,
    cluster_size: u64,
    cluster_count: u64,
    record_size: u32,
    mft: Stream,
    bitmap: Stream,
    pub record_count: u64,
}
#[derive(Clone, Debug)]
struct FileName {
    name: String,
    parent: u64,
    namespace: u8,
    size: u64,
    attributes: u32,
}
struct Attribute<'a> {
    kind: u32,
    bytes: &'a [u8],
    named: bool,
    nonresident: bool,
    flags: u16,
}
struct Record {
    bytes: Vec<u8>,
    first: usize,
    used: usize,
    flags: u16,
    sequence: u16,
    base: u64,
}
pub struct RecordRead {
    pub candidate: Option<Candidate>,
    pub warning: Option<String>,
}

impl Record {
    fn parse(mut bytes: Vec<u8>) -> Result<Self> {
        if bytes.len() < 512
            || bytes.len() > 65536
            || !bytes.len().is_multiple_of(512)
            || &bytes[..4] != b"FILE"
        {
            return Err("Invalid NTFS FILE record signature/size".into());
        }
        let usa = le16(&bytes, 4) as usize;
        let count = le16(&bytes, 6) as usize;
        let first = le16(&bytes, 20) as usize;
        let used = le32(&bytes, 24) as usize;
        if usa < 40
            || !usa.is_multiple_of(2)
            || count != bytes.len() / 512 + 1
            || usa + count * 2 > 510
            || first < 48
            || !first.is_multiple_of(8)
            || first < usa + count * 2
            || used < first + 4
            || used > bytes.len()
            || le32(&bytes, 28) as usize != bytes.len()
        {
            return Err("Invalid NTFS record header or fixup bounds".into());
        }
        let sequence = [bytes[usa], bytes[usa + 1]];
        for sector in 1..count {
            let tail = sector * 512 - 2;
            if bytes[tail..tail + 2] != sequence {
                return Err("NTFS update-sequence mismatch (torn record)".into());
            }
        }
        for sector in 1..count {
            let tail = sector * 512 - 2;
            let value = [bytes[usa + sector * 2], bytes[usa + sector * 2 + 1]];
            bytes[tail..tail + 2].copy_from_slice(&value);
        }
        Ok(Self {
            flags: le16(&bytes, 22),
            sequence: le16(&bytes, 16),
            base: le64(&bytes, 32),
            bytes,
            first,
            used,
        })
    }

    fn attributes(&self) -> Result<Vec<Attribute<'_>>> {
        let mut pos = self.first;
        let mut attrs = vec![];
        while pos + 4 <= self.used {
            let kind = le32(&self.bytes, pos);
            if kind == u32::MAX {
                return Ok(attrs);
            }
            if attrs.len() == 256 || pos + 16 > self.used {
                return Err("NTFS attribute count/header limit".into());
            }
            let size = le32(&self.bytes, pos + 4) as usize;
            let form = self.bytes[pos + 8];
            let name_len = self.bytes[pos + 9] as usize;
            let name_at = le16(&self.bytes, pos + 10) as usize;
            let minimum = if form == 0 {
                24
            } else if form == 1 {
                64
            } else {
                return Err("Unknown NTFS attribute form".into());
            };
            if size < minimum
                || !size.is_multiple_of(8)
                || size > self.used - pos
                || name_len > 0 && (name_at < minimum || name_at + name_len * 2 > size)
            {
                return Err("Invalid NTFS attribute bounds".into());
            }
            attrs.push(Attribute {
                kind,
                bytes: &self.bytes[pos..pos + size],
                named: name_len != 0,
                nonresident: form == 1,
                flags: le16(&self.bytes, pos + 12),
            });
            pos += size;
        }
        Err("NTFS attribute terminator missing".into())
    }
}

fn resident<'a>(attr: &Attribute<'a>) -> Result<&'a [u8]> {
    if attr.nonresident {
        return Err("NTFS resident attribute required".into());
    }
    let size = le32(attr.bytes, 16) as usize;
    let offset = le16(attr.bytes, 20) as usize;
    let name_end = if attr.named {
        le16(attr.bytes, 10) as usize + attr.bytes[9] as usize * 2
    } else {
        24
    };
    if offset < 24
        || offset < name_end
        || offset > attr.bytes.len()
        || size > attr.bytes.len() - offset
    {
        return Err("Invalid NTFS resident value bounds".into());
    }
    Ok(&attr.bytes[offset..offset + size])
}

fn unsigned(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .enumerate()
        .fold(0, |n, (i, b)| n | (*b as u64) << (8 * i))
}
fn mapping_pairs(
    bytes: &[u8],
    clusters: u64,
    highest: u64,
    allow_sparse: bool,
) -> Result<Vec<Run>> {
    let mut pos = 0usize;
    let mut lcn = 0i128;
    let mut vcn = 0u64;
    let mut runs = vec![];
    loop {
        let header = *bytes
            .get(pos)
            .ok_or("NTFS mapping-pair terminator missing")?;
        pos += 1;
        if header == 0 {
            break;
        }
        let length_bytes = (header & 15) as usize;
        let offset_bytes = (header >> 4) as usize;
        if !(1..=8).contains(&length_bytes)
            || offset_bytes > 8
            || pos + length_bytes + offset_bytes > bytes.len()
            || runs.len() == MAX_RUNS
        {
            return Err("Invalid or excessive NTFS mapping pairs".into());
        }
        let count = unsigned(&bytes[pos..pos + length_bytes]);
        pos += length_bytes;
        if count == 0 {
            return Err("NTFS run has zero length".into());
        }
        let physical = if offset_bytes == 0 {
            if !allow_sparse {
                return Err("Unexpected sparse NTFS run".into());
            }
            None
        } else {
            let raw = &bytes[pos..pos + offset_bytes];
            let value = unsigned(raw) as i128;
            let delta = if raw[offset_bytes - 1] & 0x80 != 0 {
                value - (1i128 << (offset_bytes * 8))
            } else {
                value
            };
            lcn += delta;
            if lcn < 0 || lcn > u64::MAX as i128 {
                return Err("NTFS run LCN overflow".into());
            }
            let lcn = lcn as u64;
            if lcn.checked_add(count).is_none_or(|end| end > clusters) {
                return Err("NTFS run extends beyond the volume".into());
            }
            Some(lcn)
        };
        pos += offset_bytes;
        runs.push(Run {
            vcn,
            clusters: count,
            lcn: physical,
        });
        vcn = vcn.checked_add(count).ok_or("NTFS VCN overflow")?;
        if vcn > clusters {
            return Err("NTFS logical stream exceeds the volume limit".into());
        }
    }
    if runs.is_empty() || highest.checked_add(1) != Some(vcn) {
        return Err("NTFS run coverage does not match HighestVcn".into());
    }
    let mut occupied: Vec<_> = runs
        .iter()
        .filter_map(|r| r.lcn.map(|lcn| (lcn, lcn + r.clusters)))
        .collect();
    occupied.sort_unstable();
    if occupied.windows(2).any(|p| p[0].1 > p[1].0) {
        return Err("Overlapping NTFS physical runs".into());
    }
    Ok(runs)
}

impl Stream {
    fn parse(
        attr: &Attribute<'_>,
        cluster_size: u64,
        cluster_count: u64,
        metadata: bool,
    ) -> Result<Self> {
        if attr.flags & 0x7fff != 0 || (metadata && attr.flags != 0) {
            return Err(
                "Compressed, encrypted or unknown NTFS stream flags are unsupported".into(),
            );
        }
        if !attr.nonresident {
            if attr.flags != 0 {
                return Err("Resident NTFS stream has unsupported flags".into());
            }
            let data = resident(attr)?.to_vec();
            return Ok(Self {
                size: data.len() as u64,
                initialized: data.len() as u64,
                resident: Some(data),
                runs: vec![],
            });
        }
        if le64(attr.bytes, 16) != 0 {
            return Err(
                "NTFS extension DATA segments require an unsupported attribute list".into(),
            );
        }
        if le16(attr.bytes, 34) != 0 {
            return Err("NTFS compressed units are unsupported".into());
        }
        let offset = le16(attr.bytes, 32) as usize;
        let minimum = if attr.flags & 0x8000 != 0 { 72 } else { 64 };
        let name_end = if attr.named {
            le16(attr.bytes, 10) as usize + attr.bytes[9] as usize * 2
        } else {
            minimum
        };
        if offset < minimum || offset < name_end || offset >= attr.bytes.len() {
            return Err("Invalid NTFS mapping-pair offset".into());
        }
        let size = le64(attr.bytes, 48);
        let initialized = le64(attr.bytes, 56);
        let allocated = le64(attr.bytes, 40);
        let volume_bytes = cluster_size
            .checked_mul(cluster_count)
            .ok_or("NTFS volume overflow")?;
        if initialized > size
            || size > volume_bytes
            || allocated > volume_bytes
            || !allocated.is_multiple_of(cluster_size)
        {
            return Err("Invalid NTFS stream sizes".into());
        }
        let runs = mapping_pairs(
            &attr.bytes[offset..],
            cluster_count,
            le64(attr.bytes, 24),
            attr.flags & 0x8000 != 0,
        )?;
        let coverage = runs
            .last()
            .and_then(|r| r.vcn.checked_add(r.clusters))
            .and_then(|n| n.checked_mul(cluster_size))
            .ok_or("NTFS stream coverage overflow")?;
        if size > coverage || metadata && initialized != size {
            return Err("Incomplete NTFS stream coverage/initialization".into());
        }
        Ok(Self {
            resident: None,
            runs,
            size,
            initialized,
        })
    }

    fn read_at(
        &self,
        source: &mut dyn ReadSource,
        volume: &NtfsVolume,
        offset: u64,
        out: &mut [u8],
    ) -> Result<()> {
        source::checked_range(offset, out.len() as u64, self.size)?;
        if let Some(data) = &self.resident {
            out.copy_from_slice(
                data.get(offset as usize..offset as usize + out.len())
                    .ok_or("NTFS resident read overflow")?,
            );
            return Ok(());
        }
        let mut done = 0usize;
        while done < out.len() {
            let logical = offset + done as u64;
            if logical >= self.initialized {
                out[done..].fill(0);
                break;
            }
            let vcn = logical / volume.cluster_size;
            let index = self.runs.partition_point(|r| r.vcn + r.clusters <= vcn);
            let run = self.runs.get(index).ok_or("NTFS stream gap")?;
            let within = logical - run.vcn * volume.cluster_size;
            let count = (run.clusters * volume.cluster_size - within)
                .min(self.initialized - logical)
                .min((out.len() - done) as u64) as usize;
            if count == 0 {
                return Err("NTFS stream made no progress".into());
            }
            if let Some(lcn) = run.lcn {
                source.read_at(
                    volume.region.offset + lcn * volume.cluster_size + within,
                    &mut out[done..done + count],
                )?;
            } else {
                out[done..done + count].fill(0);
            }
            done += count;
        }
        Ok(())
    }
}

fn data_attribute<'a, 'b>(attrs: &'a [Attribute<'b>]) -> Result<&'a Attribute<'b>> {
    let mut matches = attrs.iter().filter(|a| a.kind == 0x80 && !a.named);
    let data = matches
        .next()
        .ok_or("Unnamed NTFS DATA attribute missing")?;
    if matches.next().is_some() {
        return Err("Multiple NTFS DATA segments are unsupported".into());
    }
    Ok(data)
}

fn file_name(attrs: &[Attribute<'_>]) -> Result<Option<FileName>> {
    let mut names = vec![];
    for attr in attrs.iter().filter(|a| a.kind == 0x30 && !a.named) {
        let b = resident(attr)?;
        if b.len() < 66 {
            return Err("Truncated NTFS FILE_NAME attribute".into());
        }
        let count = b[64] as usize;
        let namespace = b[65];
        if count == 0 || namespace > 3 || 66 + count * 2 > b.len() {
            return Err("Invalid NTFS FILE_NAME length/namespace".into());
        }
        let words: Vec<_> = b[66..66 + count * 2]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| u16::from_le_bytes(*p))
            .collect();
        let name = String::from_utf16(&words).map_err(crate::io_error)?;
        if name
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
            || name == "."
            || name == ".."
        {
            return Err("Unsafe NTFS filename metadata".into());
        }
        names.push(FileName {
            name,
            parent: le64(b, 0),
            namespace,
            size: le64(b, 48),
            attributes: le32(b, 56),
        });
    }
    names.sort_by_key(|n| match n.namespace {
        1 | 3 => 0,
        0 => 1,
        _ => 2,
    });
    Ok(names.into_iter().next())
}

impl NtfsVolume {
    pub fn parse(source: &mut dyn ReadSource, region: Region) -> Result<Self> {
        source::checked_range(region.offset, region.length, source.len())?;
        let b = source::bytes(source, region.offset, 512)?;
        if &b[3..11] != b"NTFS    " || b[510..512] != [0x55, 0xaa] {
            return Err("Not an NTFS boot sector".into());
        }
        let sector = le16(&b, 11) as u64;
        let spc = b[13] as u64;
        let sectors = le64(&b, 40);
        if !matches!(sector, 512 | 1024 | 2048 | 4096) || !spc.is_power_of_two() || spc > 128 {
            return Err("Unsupported NTFS sector/cluster geometry".into());
        }
        let cluster_size = sector * spc;
        let cluster_count = sectors / spc;
        let volume_bytes = sectors
            .checked_mul(sector)
            .ok_or("NTFS volume length overflow")?;
        if cluster_count < 16 || volume_bytes > region.length {
            return Err("NTFS volume exceeds source region".into());
        }
        let mft_lcn = le64(&b, 48);
        if mft_lcn >= cluster_count {
            return Err("NTFS MFT start is outside the volume".into());
        }
        let encoded = b[64] as i8;
        let record_size = if encoded < 0 {
            1u64.checked_shl(-(encoded as i16) as u32)
                .ok_or("Invalid NTFS record-size exponent")?
        } else {
            cluster_size
                .checked_mul(encoded as u64)
                .ok_or("NTFS record-size overflow")?
        };
        if !(512..=65536).contains(&record_size) || !record_size.is_power_of_two() {
            return Err("Unsupported NTFS MFT record size".into());
        }
        source::checked_range(mft_lcn * cluster_size, record_size, volume_bytes)?;
        let record = Record::parse(source::bytes(
            source,
            region.offset + mft_lcn * cluster_size,
            record_size as usize,
        )?)?;
        let attrs = record.attributes()?;
        if record.flags & 1 == 0 || record.base != 0 || attrs.iter().any(|a| a.kind == 0x20) {
            return Err("Unsupported NTFS MFT base record/attribute list".into());
        }
        let mft = Stream::parse(data_attribute(&attrs)?, cluster_size, cluster_count, true)?;
        if mft.resident.is_some()
            || mft.size < 16 * record_size
            || !mft.size.is_multiple_of(record_size)
            || mft.runs[0].lcn != Some(mft_lcn)
            || mft.runs[0].clusters * cluster_size < record_size
        {
            return Err("Invalid NTFS MFT layout".into());
        }
        let empty = Stream {
            resident: None,
            runs: vec![],
            size: 0,
            initialized: 0,
        };
        let record_count = mft.size / record_size;
        let mut volume = Self {
            region,
            cluster_size,
            cluster_count,
            record_size: record_size as u32,
            mft,
            bitmap: empty,
            record_count,
        };
        let version = volume
            .record(source, 3)?
            .ok_or("NTFS $Volume record missing")?;
        let attrs = version.attributes()?;
        let attr = attrs
            .iter()
            .find(|a| a.kind == 0x70 && !a.named)
            .ok_or("NTFS VOLUME_INFORMATION missing")?;
        let info = resident(attr)?;
        if version.flags & 1 == 0 || info.len() < 12 || info[8] != 3 || info[9] > 1 {
            return Err("Only NTFS 3.0 and 3.1 are supported".into());
        }
        let bitmap = volume
            .record(source, 6)?
            .ok_or("NTFS allocation bitmap record missing")?;
        let attrs = bitmap.attributes()?;
        if bitmap.flags & 1 == 0 || bitmap.base != 0 || attrs.iter().any(|a| a.kind == 0x20) {
            return Err("Unsupported NTFS allocation bitmap record".into());
        }
        volume.bitmap = Stream::parse(data_attribute(&attrs)?, cluster_size, cluster_count, true)?;
        if volume.bitmap.size < cluster_count.div_ceil(8) {
            return Err("NTFS allocation bitmap is too short".into());
        }
        Ok(volume)
    }

    fn record(&self, source: &mut dyn ReadSource, index: u64) -> Result<Option<Record>> {
        if index >= self.record_count {
            return Err("NTFS MFT reference outside the table".into());
        }
        let mut bytes = vec![0; self.record_size as usize];
        self.mft
            .read_at(source, self, index * self.record_size as u64, &mut bytes)?;
        if bytes.iter().all(|b| *b == 0) {
            return Ok(None);
        }
        Record::parse(bytes).map(Some)
    }

    fn path(
        &self,
        source: &mut dyn ReadSource,
        name: &FileName,
        cancel: &AtomicBool,
    ) -> (String, Option<String>) {
        let mut parts = vec![name.name.clone()];
        let mut reference = name.parent;
        let mut seen = HashSet::new();
        for _ in 0..32 {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let index = reference & 0x0000_ffff_ffff_ffff;
            let seq = (reference >> 48) as u16;
            if !seen.insert(index) {
                break;
            }
            let parent = match self.record(source, index) {
                Ok(Some(p)) => p,
                _ => break,
            };
            if parent.flags & 2 == 0 || parent.sequence != seq || parent.base != 0 {
                break;
            }
            if index == 5 {
                parts.reverse();
                return (parts.join("/"), None);
            }
            let next = parent.attributes().and_then(|a| file_name(&a));
            let Ok(Some(parent_name)) = next else {
                break;
            };
            if parts.iter().map(String::len).sum::<usize>() + parent_name.name.len() + parts.len()
                > 3500
            {
                break;
            }
            parts.push(parent_name.name);
            reference = parent_name.parent;
        }
        parts.reverse();
        (format!("Unknown-parent-{}/{path}",name.parent&0x0000_ffff_ffff_ffff,path=parts.join("/")),Some("Parent path is incomplete (missing/reused record, sequence mismatch, cycle or depth limit).".into()))
    }

    fn free_prefix(
        &self,
        source: &mut dyn ReadSource,
        lcn: u64,
        clusters: u64,
        cancel: &AtomicBool,
    ) -> Result<u64> {
        let mut checked = 0;
        while checked < clusters {
            if cancel.load(Ordering::Relaxed) {
                return Err("Scan cancelled".into());
            }
            let start = lcn + checked;
            let count = (clusters - checked).min(32768 - (start % 8));
            let bytes = ((start % 8 + count).div_ceil(8)) as usize;
            let mut bits = vec![0; bytes];
            self.bitmap.read_at(source, self, start / 8, &mut bits)?;
            for n in 0..count {
                let bit = start % 8 + n;
                if bits[(bit / 8) as usize] & (1 << (bit % 8)) != 0 {
                    return Ok(checked + n);
                }
            }
            checked += count;
        }
        Ok(checked)
    }

    pub fn read_deleted(
        &self,
        source: &mut dyn ReadSource,
        index: u64,
        cancel: &AtomicBool,
    ) -> Result<RecordRead> {
        let Some(record) = self.record(source, index)? else {
            return Ok(RecordRead {
                candidate: None,
                warning: None,
            });
        };
        if index < 16 || record.flags & 3 != 0 || record.base != 0 {
            return Ok(RecordRead {
                candidate: None,
                warning: None,
            });
        }
        let attrs = record.attributes()?;
        let Some(name) = file_name(&attrs)? else {
            return Ok(RecordRead {
                candidate: None,
                warning: Some(format!(
                    "Deleted MFT record {index} has no usable filename."
                )),
            });
        };
        let (path, path_warning) = self.path(source, &name, cancel);
        let attr = data_attribute(&attrs);
        let kind = if attr.as_ref().is_ok_and(|a| !a.nonresident) {
            RecoveryKind::NtfsResident
        } else {
            RecoveryKind::NtfsRuns
        };
        let expected = attr.as_ref().ok().map_or(name.size, |a| {
            if a.nonresident {
                le64(a.bytes, 48)
            } else {
                le32(a.bytes, 16) as u64
            }
        });
        let mut candidate = Candidate {
            id: 0,
            original_path: path,
            expected_size: expected,
            recovered_size: 0,
            extents: vec![],
            kind,
            confidence: Confidence::Unverified,
            note: String::new(),
            inline_data: None,
        };
        let stream = (|| {
            if attrs.iter().any(|a| a.kind == 0x20)
                || name.attributes & (0x400 | 0x800 | 0x4000) != 0
            {
                return Err("Attribute lists, reparse points, compressed and encrypted files are unsupported".into());
            }
            Stream::parse(attr?, self.cluster_size, self.cluster_count, false)
        })();
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                candidate.confidence = Confidence::Unsupported;
                candidate.note = e;
                return Ok(RecordRead {
                    candidate: Some(candidate),
                    warning: path_warning,
                });
            }
        };
        if let Some(data) = stream.resident {
            candidate.recovered_size = data.len() as u64;
            candidate.inline_data = Some(data);
            candidate.note="Resident bytes recovered after update-sequence fixups. Deleted data may have been reused; verify the result.".into();
        } else {
            let limit = stream.size.min(MAX_RECOVERY);
            let initialized = stream.initialized.min(limit);
            let mut complete = true;
            for run in &stream.runs {
                let logical = run.vcn * self.cluster_size;
                if logical >= initialized {
                    break;
                }
                if cancel.load(Ordering::Relaxed) {
                    return Err("Scan cancelled".into());
                }
                let wanted = (run.clusters * self.cluster_size).min(initialized - logical);
                let length = if let Some(lcn) = run.lcn {
                    let free =
                        self.free_prefix(source, lcn, wanted.div_ceil(self.cluster_size), cancel)?;
                    (free * self.cluster_size).min(wanted)
                } else {
                    wanted
                };
                if length > 0 {
                    candidate.extents.push(Extent {
                        offset: run
                            .lcn
                            .map_or(0, |lcn| self.region.offset + lcn * self.cluster_size),
                        length,
                        zero_fill: run.lcn.is_none(),
                    });
                    candidate.recovered_size += length;
                }
                if length < wanted {
                    complete = false;
                    break;
                }
            }
            if complete && limit > initialized {
                candidate.extents.push(Extent {
                    offset: 0,
                    length: limit - initialized,
                    zero_fill: true,
                });
                candidate.recovered_size += limit - initialized;
            }
            if candidate.recovered_size < stream.size {
                candidate.confidence = Confidence::Partial;
                candidate.note="Only a prefix is recoverable: clusters are currently allocated or the 4 GiB per-file recovery limit was reached.".into();
            } else {
                candidate.note="MFT runlist recovered; current $Bitmap allocations excluded. Sparse and uninitialized bytes are zero-filled. Previously overwritten free clusters cannot be detected.".into();
            }
        }
        candidate.validate(source.len())?;
        Ok(RecordRead {
            candidate: Some(candidate),
            warning: path_warning,
        })
    }
}
