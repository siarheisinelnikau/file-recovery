use super::*;
use crate::{
    engine::{Phase, ScanOptions, ScanState},
    preview, restore,
    source::MemorySource,
};
use std::{fs, sync::atomic::AtomicBool};

fn put16(b: &mut [u8], at: usize, n: u16) {
    b[at..at + 2].copy_from_slice(&n.to_le_bytes());
}
fn put32(b: &mut [u8], at: usize, n: u32) {
    b[at..at + 4].copy_from_slice(&n.to_le_bytes());
}
fn put64(b: &mut [u8], at: usize, n: u64) {
    b[at..at + 8].copy_from_slice(&n.to_le_bytes());
}
fn align(n: usize) -> usize {
    n.div_ceil(8) * 8
}
fn resident_attr(kind: u32, value: &[u8]) -> Vec<u8> {
    let mut b = vec![0; align(24 + value.len())];
    let size = b.len() as u32;
    put32(&mut b, 0, kind);
    put32(&mut b, 4, size);
    put32(&mut b, 16, value.len() as u32);
    put16(&mut b, 20, 24);
    b[24..24 + value.len()].copy_from_slice(value);
    b
}
fn filename(name: &str, parent: u64, namespace: u8) -> Vec<u8> {
    let words: Vec<_> = name.encode_utf16().collect();
    let mut b = vec![0; 66 + words.len() * 2];
    put64(&mut b, 0, parent);
    b[64] = words.len() as u8;
    b[65] = namespace;
    for (index, word) in words.into_iter().enumerate() {
        put16(&mut b, 66 + index * 2, word);
    }
    resident_attr(0x30, &b)
}
fn data_attr(
    runs: &[(u64, Option<u64>)],
    size: u64,
    initialized: u64,
    flags: u16,
    cluster_size: u64,
) -> Vec<u8> {
    let mut pairs = vec![];
    let mut previous = 0i64;
    let mut clusters = 0u64;
    let mut physical = 0u64;
    for (count, lcn) in runs {
        clusters += count;
        let length_bytes = if *count <= 255 { 1 } else { 2 };
        let (offset_bytes, delta) = match lcn {
            Some(lcn) => {
                physical += count;
                let delta = *lcn as i64 - previous;
                previous = *lcn as i64;
                (if (-128..=127).contains(&delta) { 1 } else { 2 }, delta)
            }
            None => (0, 0),
        };
        pairs.push((offset_bytes << 4 | length_bytes) as u8);
        pairs.extend_from_slice(&count.to_le_bytes()[..length_bytes]);
        pairs.extend_from_slice(&delta.to_le_bytes()[..offset_bytes]);
    }
    pairs.push(0);
    let header = if flags & 0x8000 != 0 { 72 } else { 64 };
    let mut b = vec![0; align(header + pairs.len())];
    let len = b.len() as u32;
    put32(&mut b, 0, 0x80);
    put32(&mut b, 4, len);
    b[8] = 1;
    put16(&mut b, 12, flags);
    put64(&mut b, 24, clusters - 1);
    put16(&mut b, 32, header as u16);
    put64(&mut b, 40, physical * cluster_size);
    put64(&mut b, 48, size);
    put64(&mut b, 56, initialized);
    b[header..header + pairs.len()].copy_from_slice(&pairs);
    b
}
fn record(size: usize, attrs: &[Vec<u8>], flags: u16, sequence: u16, index: u64) -> Vec<u8> {
    let count = size / 512 + 1;
    let first = align(48 + count * 2);
    let mut b = vec![0; size];
    b[..4].copy_from_slice(b"FILE");
    put16(&mut b, 4, 48);
    put16(&mut b, 6, count as u16);
    put16(&mut b, 16, sequence);
    put16(&mut b, 20, first as u16);
    put16(&mut b, 22, flags);
    put32(&mut b, 28, size as u32);
    put32(&mut b, 44, index as u32);
    let mut pos = first;
    for attr in attrs {
        b[pos..pos + attr.len()].copy_from_slice(attr);
        pos += attr.len();
    }
    put32(&mut b, pos, u32::MAX);
    put32(&mut b, 24, align(pos + 4) as u32);
    b[48..50].copy_from_slice(&[0x7a, 0xa5]);
    for n in 1..count {
        let tail = n * 512 - 2;
        let original = [b[tail], b[tail + 1]];
        b[48 + n * 2..50 + n * 2].copy_from_slice(&original);
        b[tail..tail + 2].copy_from_slice(&[0x7a, 0xa5]);
    }
    b
}

struct Fixture {
    source: MemorySource,
    record_size: usize,
    first_clusters: u64,
    bitmap_offset: usize,
}
impl Fixture {
    fn new(sector: u16, record_size: usize) -> Self {
        let cluster = 4096u64;
        let total_clusters = 512u64;
        let mut b = vec![0; cluster as usize * total_clusters as usize];
        b[..3].copy_from_slice(&[0xeb, 0x52, 0x90]);
        b[3..11].copy_from_slice(b"NTFS    ");
        put16(&mut b, 11, sector);
        b[13] = (cluster / sector as u64) as u8;
        put64(&mut b, 40, cluster * total_clusters / sector as u64);
        put64(&mut b, 48, 4);
        put64(&mut b, 56, 2);
        b[64] = (-(record_size.trailing_zeros() as i8)) as u8;
        b[510..512].copy_from_slice(&[0x55, 0xaa]);
        let first_clusters = (16 * record_size as u64) / cluster;
        let mut f = Self {
            source: MemorySource(b),
            record_size,
            first_clusters,
            bitmap_offset: 0,
        };
        f.put(
            0,
            &[data_attr(
                &[(first_clusters, Some(4)), (first_clusters, Some(64))],
                32 * record_size as u64,
                32 * record_size as u64,
                0,
                cluster,
            )],
            1,
            1,
        );
        let mut version = vec![0; 12];
        version[8] = 3;
        version[9] = 1;
        f.put(3, &[resident_attr(0x70, &version)], 1, 1);
        f.put(5, &[], 3, 1);
        f.put(6, &[resident_attr(0x80, &[0; 64])], 1, 1);
        f.bitmap_offset = f.offset(6) + align(48 + (record_size / 512 + 1) * 2) + 24;
        f.allocated(0);
        for index in 4..4 + first_clusters {
            f.allocated(index);
        }
        for index in 64..64 + first_clusters {
            f.allocated(index);
        }
        f.put(8, &[filename("Folder", (1u64 << 48) | 5, 1)], 3, 7);
        f
    }
    fn offset(&self, index: u64) -> usize {
        let logical = index * self.record_size as u64;
        let first = self.first_clusters * 4096;
        if logical < first {
            (4 * 4096 + logical) as usize
        } else {
            (64 * 4096 + logical - first) as usize
        }
    }
    fn put(&mut self, index: u64, attrs: &[Vec<u8>], flags: u16, sequence: u16) {
        let offset = self.offset(index);
        self.source.0[offset..offset + self.record_size].copy_from_slice(&record(
            self.record_size,
            attrs,
            flags,
            sequence,
            index,
        ));
    }
    fn allocated(&mut self, cluster: u64) {
        self.source.0[self.bitmap_offset + cluster as usize / 8] |= 1 << (cluster % 8);
    }
    fn deleted(&mut self, index: u64, name: &str, attr: Vec<u8>) {
        self.put(index, &[filename(name, (7u64 << 48) | 8, 1), attr], 0, 2);
    }
    fn scan(&mut self) -> ScanState {
        let mut state = ScanState::start(&mut self.source, ScanOptions::default()).unwrap();
        state
            .run(&mut self.source, &AtomicBool::new(false), |_| {})
            .unwrap();
        state
    }
}
fn recovered(source: &mut dyn ReadSource, item: &Candidate) -> Vec<u8> {
    let mut data = vec![0; item.recovered_size as usize];
    item.read_at(source, 0, &mut data).unwrap();
    data
}

#[test]
fn resident_unicode_file_across_fixup_word_restores_exactly() {
    let mut f = Fixture::new(512, 1024);
    let payload: Vec<u8> = (0..700).map(|n| (n % 251) as u8).collect();
    f.deleted(24, "Отчёт 🐾.bin", resident_attr(0x80, &payload));
    let original = f.source.0.clone();
    let state = f.scan();
    assert_eq!(state.ntfs.len(), 1);
    assert!(state.warnings.is_empty(), "{:?}", state.warnings);
    assert_eq!(state.results.len(), 1);
    let item = &state.results[0];
    assert_eq!(item.original_path, "Folder/Отчёт 🐾.bin");
    assert_eq!(item.kind, RecoveryKind::NtfsResident);
    assert_eq!(recovered(&mut f.source, item), payload);
    let dir = tempfile::tempdir().unwrap();
    let report = restore::restore(
        &mut f.source,
        &state.identity,
        &state.results,
        dir.path(),
        &AtomicBool::new(false),
        |_, _| {},
    )
    .unwrap();
    assert!(report.failed.is_empty());
    assert_eq!(fs::read(&report.files[0]).unwrap(), payload);
    assert_eq!(f.source.0, original);
}

#[test]
fn fragmented_mft_and_negative_delta_file_runs_preserve_contents() {
    let mut f = Fixture::new(512, 1024);
    f.deleted(
        24,
        "fragment.txt",
        data_attr(&[(1, Some(200)), (1, Some(190))], 4196, 4196, 0, 4096),
    );
    f.source.0[200 * 4096..201 * 4096].fill(b'A');
    f.source.0[190 * 4096..190 * 4096 + 100].fill(b'B');
    let state = f.scan();
    assert_eq!(state.results.len(), 1);
    assert_eq!(state.results[0].kind, RecoveryKind::NtfsRuns);
    assert_eq!(state.results[0].extents.len(), 2);
    let expected = [vec![b'A'; 4096], vec![b'B'; 100]].concat();
    assert_eq!(recovered(&mut f.source, &state.results[0]), expected);
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("fragment.txt"), b"keep").unwrap();
    let result = restore::restore(
        &mut f.source,
        &state.identity,
        &state.results,
        dir.path(),
        &AtomicBool::new(false),
        |_, _| {},
    )
    .unwrap();
    assert_eq!(fs::read(&result.files[0]).unwrap(), expected);
    assert_eq!(fs::read(dir.path().join("fragment.txt")).unwrap(), b"keep");
}

#[test]
fn sparse_holes_and_uninitialized_tail_are_logical_zeroes() {
    let mut f = Fixture::new(512, 1024);
    f.deleted(
        24,
        "sparse.bin",
        data_attr(
            &[(1, Some(200)), (1, None), (1, Some(190)), (1, None)],
            12388,
            12288,
            0x8000,
            4096,
        ),
    );
    f.source.0[200 * 4096..201 * 4096].fill(9);
    f.source.0[190 * 4096..191 * 4096].fill(7);
    let state = f.scan();
    let item = &state.results[0];
    assert_eq!(item.confidence, Confidence::Unverified);
    assert_eq!(item.recovered_size, 12388);
    let expected = [vec![9; 4096], vec![0; 4096], vec![7; 4096], vec![0; 100]].concat();
    assert_eq!(recovered(&mut f.source, item), expected);
    let dir = tempfile::tempdir().unwrap();
    let result = restore::restore(
        &mut f.source,
        &state.identity,
        &state.results,
        dir.path(),
        &AtomicBool::new(false),
        |_, _| {},
    )
    .unwrap();
    assert_eq!(fs::read(&result.files[0]).unwrap(), expected);
}

#[test]
fn allocated_clusters_stop_at_readable_prefix_without_exposing_reused_data() {
    let mut f = Fixture::new(512, 1024);
    f.deleted(
        24,
        "reused.bin",
        data_attr(&[(2, Some(200))], 8192, 8192, 0, 4096),
    );
    f.allocated(201);
    f.source.0[200 * 4096..201 * 4096].fill(3);
    f.source.0[201 * 4096..202 * 4096].fill(0xEE);
    let state = f.scan();
    let item = &state.results[0];
    assert_eq!(item.confidence, Confidence::Partial);
    assert_eq!(item.recovered_size, 4096);
    assert_eq!(recovered(&mut f.source, item), vec![3; 4096]);
    let dir = tempfile::tempdir().unwrap();
    let result = restore::restore(
        &mut f.source,
        &state.identity,
        &state.results,
        dir.path(),
        &AtomicBool::new(false),
        |_, _| {},
    )
    .unwrap();
    assert!(result.files[0].extension().is_some_and(|s| s == "partial"));
    assert_eq!(fs::read(&result.files[0]).unwrap(), vec![3; 4096]);
}

#[test]
fn compressed_encrypted_and_attribute_list_files_are_unsupported_and_never_written() {
    let mut f = Fixture::new(512, 1024);
    for (index, flags) in [(24, 1), (25, 0x4000)] {
        f.deleted(
            index,
            "unsupported.bin",
            data_attr(&[(1, Some(200))], 100, 100, flags, 4096),
        );
    }
    f.put(
        26,
        &[
            filename("extended.txt", (1u64 << 48) | 5, 1),
            resident_attr(0x20, &[]),
            resident_attr(0x80, b"do not guess"),
        ],
        0,
        1,
    );
    let state = f.scan();
    assert_eq!(state.results.len(), 3);
    assert!(
        state
            .results
            .iter()
            .all(|c| c.confidence == Confidence::Unsupported)
    );
    let dir = tempfile::tempdir().unwrap();
    let result = restore::restore(
        &mut f.source,
        &state.identity,
        &state.results,
        dir.path(),
        &AtomicBool::new(false),
        |_, _| {},
    )
    .unwrap();
    assert_eq!(result.failed.len(), 3);
    assert!(result.files.is_empty());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn torn_records_and_invalid_attribute_lengths_do_not_become_candidates() {
    let mut f = Fixture::new(512, 1024);
    f.deleted(24, "torn.txt", resident_attr(0x80, b"test"));
    let offset = f.offset(24);
    f.source.0[offset + 510] ^= 1;
    f.deleted(25, "bounds.txt", resident_attr(0x80, b"test"));
    let offset = f.offset(25);
    put32(&mut f.source.0, offset + 56 + 4, u32::MAX);
    let state = f.scan();
    assert!(state.results.is_empty());
    assert!(state.capped);
    assert!(state.warnings.iter().any(|w| w.contains("update-sequence")));
    assert!(state.warnings.iter().any(|w| w.contains("bounds")));
}

#[test]
fn stale_parent_and_cycles_keep_filename_without_inventing_original_path() {
    let mut f = Fixture::new(512, 1024);
    f.put(8, &[filename("WrongParent", (7u64 << 48) | 8, 1)], 3, 7);
    f.deleted(24, "cycle.txt", resident_attr(0x80, b"real"));
    f.put(
        25,
        &[
            filename("stale.txt", (8u64 << 48) | 8, 1),
            resident_attr(0x80, b"actual"),
        ],
        0,
        2,
    );
    let state = f.scan();
    assert_eq!(state.results.len(), 2);
    assert!(
        state
            .results
            .iter()
            .all(|c| c.original_path.starts_with("Unknown-parent-8/"))
    );
    assert!(state.warnings.iter().all(|w| w.contains("incomplete")));
}

#[test]
fn four_kn_boot_sectors_still_use_512_byte_update_sequence_stride() {
    let mut f = Fixture::new(4096, 4096);
    let content = vec![0x5a; 2700];
    f.deleted(24, "large-resident.bin", resident_attr(0x80, &content));
    let state = f.scan();
    assert_eq!(state.results.len(), 1);
    assert_eq!(recovered(&mut f.source, &state.results[0]), content);
}

#[test]
fn ntfs_resume_is_tied_to_source_and_record_cursor() {
    let mut f = Fixture::new(512, 1024);
    f.deleted(24, "resume.txt", resident_attr(0x80, b"resume"));
    let cancel = AtomicBool::new(false);
    let mut state = ScanState::start(&mut f.source, ScanOptions::default()).unwrap();
    while state.records_read < 25 {
        state.step(&mut f.source, &cancel).unwrap();
    }
    assert_eq!(state.phase, Phase::NtfsMetadata);
    assert_eq!(state.results.len(), 1);
    cancel.store(true, Ordering::Relaxed);
    assert!(!state.step(&mut f.source, &cancel).unwrap());
    assert_eq!(state.records_read, 25);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scan.json");
    state.save(&path).unwrap();
    let mut restored = ScanState::load(&path, &mut f.source).unwrap();
    cancel.store(false, Ordering::Relaxed);
    restored.run(&mut f.source, &cancel, |_| {}).unwrap();
    assert_eq!(restored.results.len(), 1);
    assert_eq!(restored.records_read, 32);
    assert_eq!(
        preview::preview(&mut f.source, &restored.results[0]).unwrap(),
        "resume"
    );
    f.source.0[72] ^= 1;
    assert!(ScanState::load(&path, &mut f.source).is_err());
}

#[test]
fn signed_and_oversized_mapping_pairs_are_bounded() {
    assert!(mapping_pairs(&[0x11, 1, 0xff, 0], 512, 0, false).is_err());
    assert!(mapping_pairs(&[0x11, 0, 1, 0], 512, 0, false).is_err());
    assert!(mapping_pairs(&[0x11, 1, 200, 0x11, 1, 0, 0], 512, 1, false).is_err());
    assert!(mapping_pairs(&[0x11, 1, 100], 512, 0, false).is_err());
    assert!(
        mapping_pairs(
            &[
                0x88, 255, 255, 255, 255, 255, 255, 255, 255, 1, 0, 0, 0, 0, 0, 0, 0, 0
            ],
            512,
            0,
            false
        )
        .is_err()
    );
    assert!(mapping_pairs(&[0x01, 1, 0], 512, 0, false).is_err());
}

#[test]
fn invalid_boot_mft_and_stream_geometries_fail_without_panics() {
    for field in [11, 13, 40, 48, 64] {
        let mut f = Fixture::new(512, 1024);
        f.source.0[field] = 0xFF;
        let state = f.scan();
        assert!(state.results.is_empty());
        assert!(state.ntfs.is_empty());
        assert!(!state.warnings.is_empty());
    }
    let mut f = Fixture::new(512, 1024);
    f.deleted(
        24,
        "out-of-volume.txt",
        data_attr(&[(1, Some(600))], 20, 20, 0, 4096),
    );
    let state = f.scan();
    assert_eq!(state.results[0].confidence, Confidence::Unsupported);
}

#[test]
fn partitioned_ntfs_at_explicit_offset_restores_resident_data() {
    let mut f = Fixture::new(512, 1024);
    f.deleted(24, "inside.txt", resident_attr(0x80, b"partition"));
    let mut disk = vec![0; 4096];
    disk.extend(f.source.0);
    let mut source = MemorySource(disk);
    let mut state = ScanState::start(
        &mut source,
        ScanOptions {
            filesystem_offset: Some(4096),
        },
    )
    .unwrap();
    state
        .run(&mut source, &AtomicBool::new(false), |_| {})
        .unwrap();
    assert_eq!(state.ntfs[0].volume.region.offset, 4096);
    assert_eq!(recovered(&mut source, &state.results[0]), b"partition");
}

#[test]
fn mbr_ntfs_raw_image_uses_read_only_file_adapter_end_to_end() {
    let mut f = Fixture::new(512, 1024);
    f.deleted(24, "on-disk.txt", resident_attr(0x80, b"raw image content"));
    let mut image = vec![0; 4096];
    image[510..512].copy_from_slice(&[0x55, 0xaa]);
    image[450] = 7;
    put32(&mut image, 454, 8);
    put32(&mut image, 458, (f.source.0.len() / 512) as u32);
    image.extend(f.source.0);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ntfs.img");
    fs::write(&path, &image).unwrap();
    let output = dir.path().join("restored");
    fs::create_dir(&output).unwrap();
    let mut source = crate::source::ImageSource::open(&path).unwrap();
    let mut state = ScanState::start(&mut source, ScanOptions::default()).unwrap();
    state
        .run(&mut source, &AtomicBool::new(false), |_| {})
        .unwrap();
    assert_eq!(state.ntfs.len(), 1);
    assert_eq!(state.results.len(), 1);
    let summary = restore::restore(
        &mut source,
        &state.identity,
        &state.results,
        &output,
        &AtomicBool::new(false),
        |_, _| {},
    )
    .unwrap();
    assert_eq!(fs::read(&summary.files[0]).unwrap(), b"raw image content");
    assert_eq!(fs::read(path).unwrap(), image);
}

#[test]
fn live_files_and_extension_records_are_skipped_while_windows_name_is_preferred() {
    let mut f = Fixture::new(512, 1024);
    f.put(
        24,
        &[
            filename("LONGNA~1.TXT", (1u64 << 48) | 5, 2),
            filename("Long name.txt", (1u64 << 48) | 5, 1),
            resident_attr(0x80, b"deleted"),
        ],
        0,
        2,
    );
    f.put(
        25,
        &[
            filename("live.txt", (1u64 << 48) | 5, 1),
            resident_attr(0x80, b"still live"),
        ],
        1,
        1,
    );
    f.deleted(26, "extension.txt", resident_attr(0x80, b"extension"));
    let offset = f.offset(26);
    put64(&mut f.source.0, offset + 32, (2u64 << 48) | 24);
    let state = f.scan();
    assert_eq!(state.results.len(), 1);
    assert_eq!(state.results[0].original_path, "Long name.txt");
    assert_eq!(
        preview::preview(&mut f.source, &state.results[0]).unwrap(),
        "deleted"
    );
}

#[test]
fn cancellation_during_record_read_keeps_the_mft_cursor_resumable() {
    struct CancelSource<'a> {
        inner: MemorySource,
        stop: &'a AtomicBool,
        at: u64,
        armed: bool,
    }
    impl ReadSource for CancelSource<'_> {
        fn len(&self) -> u64 {
            self.inner.len()
        }
        fn identity(&mut self) -> Result<crate::source::SourceIdentity> {
            self.inner.identity()
        }
        fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<()> {
            self.inner.read_at(offset, out)?;
            if self.armed && offset == self.at {
                self.stop.store(true, Ordering::Relaxed);
            }
            Ok(())
        }
    }
    let mut f = Fixture::new(512, 1024);
    f.deleted(24, "cancel.txt", resident_attr(0x80, b"resumable"));
    let at = f.offset(24) as u64;
    let cancel = AtomicBool::new(false);
    let mut source = CancelSource {
        inner: f.source,
        stop: &cancel,
        at,
        armed: false,
    };
    let mut state = ScanState::start(&mut source, ScanOptions::default()).unwrap();
    while state.records_read < 24 {
        state.step(&mut source, &cancel).unwrap();
    }
    source.armed = true;
    assert!(!state.step(&mut source, &cancel).unwrap());
    assert_eq!(state.ntfs[0].next_record, 24);
    assert!(state.results.is_empty());
    source.armed = false;
    cancel.store(false, Ordering::Relaxed);
    state.run(&mut source, &cancel, |_| {}).unwrap();
    assert_eq!(state.results.len(), 1);
    assert_eq!(state.records_read, 32);
}

#[test]
fn ntfs_checkpoints_reject_changed_layouts_and_out_of_range_cursors() {
    let mut f = Fixture::new(512, 1024);
    let state = ScanState::start(&mut f.source, ScanOptions::default()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("checkpoint.json");
    let mut value = serde_json::to_value(&state).unwrap();
    value["ntfs"][0]["next_record"] = serde_json::json!(33);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(ScanState::load(&path, &mut f.source).is_err());
    value = serde_json::to_value(&state).unwrap();
    value["ntfs"][0]["volume"]["mft"]["runs"][0]["lcn"] = serde_json::json!(500);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(ScanState::load(&path, &mut f.source).is_err());
}

#[test]
fn mutated_record_headers_attributes_and_mapping_pairs_never_panic() {
    let mut f = Fixture::new(512, 1024);
    f.deleted(
        24,
        "mutations.txt",
        data_attr(&[(1, Some(200))], 200, 200, 0, 4096),
    );
    let offset = f.offset(24);
    let original = f.source.0[offset..offset + 1024].to_vec();
    for index in 0..256 {
        let mut bytes = original.clone();
        bytes[index] ^= 0xFF;
        if let Ok(record) = Record::parse(bytes)
            && let Ok(attrs) = record.attributes()
        {
            let _ = file_name(&attrs);
            if let Ok(attr) = data_attribute(&attrs) {
                let _ = Stream::parse(attr, 4096, 512, false);
            }
        }
    }
    for length in 0..16 {
        let _ = mapping_pairs(&vec![0xFF; length], 512, 1, true);
    }
    assert!(mapping_pairs(&[0x21, 1, 200, 0, 0x11, 1, 0, 0], 512, 1, false).is_err());
}
