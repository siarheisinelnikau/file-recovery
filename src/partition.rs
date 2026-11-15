use crate::{
    Result,
    source::{self, ReadSource},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    pub offset: u64,
    pub length: u64,
    pub label: String,
}

pub fn discover(source: &mut dyn ReadSource, explicit_offset: Option<u64>) -> Result<Vec<Region>> {
    if let Some(offset) = explicit_offset {
        source::checked_range(offset, 512, source.len())?;
        return Ok(vec![Region {
            offset,
            length: source.len() - offset,
            label: "Explicit offset".into(),
        }]);
    }
    let boot = source::bytes(source, 0, 512)?;
    // A plausible BPB identifies a superfloppy before interpreting its code as MBR entries.
    if boot[510..512] == [0x55, 0xaa]
        && matches!(le16(&boot, 11), 512 | 1024 | 2048 | 4096)
        && boot[13].is_power_of_two()
        && boot[13] <= 128
        && le16(&boot, 14) > 0
        && matches!(boot[16], 1 | 2)
    {
        return Ok(vec![Region {
            offset: 0,
            length: source.len(),
            label: "Filesystem image".into(),
        }]);
    }
    if boot[510..512] != [0x55, 0xaa] {
        return Ok(vec![]);
    }
    if (0..4).any(|i| boot[446 + i * 16 + 4] == 0xee) {
        return gpt(source);
    }
    let mut regions = vec![];
    for i in 0..4 {
        let pos = 446 + i * 16;
        let kind = boot[pos + 4];
        if kind == 0 {
            continue;
        }
        let offset = le32(&boot, pos + 8) as u64 * 512;
        let length = le32(&boot, pos + 12) as u64 * 512;
        if length < 512 || offset < 512 {
            return Err("Invalid MBR partition range".into());
        }
        source::checked_range(offset, length, source.len())?;
        regions.push(Region {
            offset,
            length,
            label: format!("MBR partition {}", i + 1),
        });
    }
    reject_overlaps(&regions)?;
    Ok(regions)
}

fn gpt(source: &mut dyn ReadSource) -> Result<Vec<Region>> {
    let header = source::bytes(source, 512, 512)?;
    if &header[..8] != b"EFI PART" {
        return Err("Protective MBR without a supported 512-byte GPT header".into());
    }
    let size = le32(&header, 12) as usize;
    if !(92..=512).contains(&size) || le64(&header, 24) != 1 {
        return Err("Invalid GPT header size/location".into());
    }
    let mut checked = header[..size].to_vec();
    checked[16..20].fill(0);
    if crc32fast::hash(&checked) != le32(&header, 16) {
        return Err("GPT header CRC mismatch".into());
    }
    let first = le64(&header, 40);
    let last = le64(&header, 48);
    let count = le32(&header, 80) as usize;
    let stride = le32(&header, 84) as usize;
    if first < 2
        || first > last
        || last >= source.len() / 512
        || le64(&header, 32) <= last
        || le64(&header, 32) >= source.len() / 512
        || count == 0
        || count > 4096
        || !(128..=1024).contains(&stride)
        || !stride.is_multiple_of(8)
    {
        return Err("GPT entry geometry exceeds supported bounds".into());
    }
    let offset = le64(&header, 72)
        .checked_mul(512)
        .ok_or("GPT offset overflow")?;
    let table = source::bytes(source, offset, count * stride)?;
    if offset < 1024 || offset.saturating_add((count * stride) as u64) > first.saturating_mul(512) {
        return Err("GPT table overlaps usable space or its header".into());
    }
    if crc32fast::hash(&table) != le32(&header, 88) {
        return Err("GPT partition array CRC mismatch".into());
    }
    let mut regions = vec![];
    for row in table.chunks_exact(stride) {
        if row[..16].iter().all(|v| *v == 0) {
            continue;
        }
        let start = le64(row, 32);
        let end = le64(row, 40);
        if start < first || end > last || start > end {
            return Err("GPT partition outside usable range".into());
        }
        let offset = start.checked_mul(512).ok_or("GPT offset overflow")?;
        let length = (end - start + 1)
            .checked_mul(512)
            .ok_or("GPT length overflow")?;
        source::checked_range(offset, length, source.len())?;
        let words: Vec<u16> = row[56..128]
            .chunks_exact(2)
            .map(|x| u16::from_le_bytes([x[0], x[1]]))
            .take_while(|v| *v != 0)
            .collect();
        regions.push(Region {
            offset,
            length,
            label: String::from_utf16_lossy(&words),
        });
    }
    reject_overlaps(&regions)?;
    Ok(regions)
}

fn reject_overlaps(regions: &[Region]) -> Result<()> {
    let mut sorted = regions.to_vec();
    sorted.sort_by_key(|r| r.offset);
    if sorted
        .windows(2)
        .any(|w| w[0].offset + w[0].length > w[1].offset)
    {
        return Err("Overlapping partition ranges".into());
    }
    Ok(())
}

pub(crate) fn le16(b: &[u8], p: usize) -> u16 {
    u16::from_le_bytes([b[p], b[p + 1]])
}
pub(crate) fn le32(b: &[u8], p: usize) -> u32 {
    u32::from_le_bytes(b[p..p + 4].try_into().expect("fixed parser range"))
}
pub(crate) fn le64(b: &[u8], p: usize) -> u64 {
    u64::from_le_bytes(b[p..p + 8].try_into().expect("fixed parser range"))
}
