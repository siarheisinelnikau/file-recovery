use crate::{
    engine::{Confidence, Extent, Phase, RecoveryKind, ScanOptions, ScanState},
    partition, preview, restore,
    settings::{Settings, Theme},
    source::{self, MemorySource, ReadSource},
    updates,
};
use std::{
    fs,
    sync::atomic::{AtomicBool, Ordering},
};

fn u16at(v: &mut [u8], p: usize, n: u16) {
    v[p..p + 2].copy_from_slice(&n.to_le_bytes());
}
fn u32at(v: &mut [u8], p: usize, n: u32) {
    v[p..p + 4].copy_from_slice(&n.to_le_bytes());
}
fn u64at(v: &mut [u8], p: usize, n: u64) {
    v[p..p + 8].copy_from_slice(&n.to_le_bytes());
}
fn fixture(fat32: bool) -> MemorySource {
    let (total, fat_sectors, reserved, root_entries) = if fat32 {
        (66070, 512, 32, 0)
    } else {
        (5000, 20, 1, 64)
    };
    let mut b = vec![0; total * 512];
    b[..3].copy_from_slice(&[0xeb, 0x3c, 0x90]);
    u16at(&mut b, 11, 512);
    b[13] = 1;
    u16at(&mut b, 14, reserved as u16);
    b[16] = 1;
    u16at(&mut b, 17, root_entries);
    if fat32 {
        u32at(&mut b, 32, total as u32);
        u32at(&mut b, 36, fat_sectors as u32);
        u32at(&mut b, 44, 2);
    } else {
        u16at(&mut b, 19, total as u16);
        u16at(&mut b, 22, fat_sectors as u16);
    }
    b[510..512].copy_from_slice(&[0x55, 0xaa]);
    if fat32 {
        u32at(&mut b, reserved * 512 + 8, 0x0fff_ffff);
    }
    MemorySource(b)
}
fn data_offset(fat32: bool, c: usize) -> usize {
    (if fat32 { 32 + 512 } else { 1 + 20 + 4 } + c - 2) * 512
}
fn root(fat32: bool) -> usize {
    if fat32 {
        data_offset(true, 2)
    } else {
        21 * 512
    }
}
fn entry(s: &mut MemorySource, fat32: bool, slot: usize, cluster: u16, size: u32) {
    let p = root(fat32) + slot * 32;
    s.0[p..p + 11].copy_from_slice(b"_EST    TXT");
    s.0[p] = 0xe5;
    s.0[p + 11] = 0x20;
    u16at(&mut s.0, p + 26, cluster);
    u32at(&mut s.0, p + 28, size);
}
fn fat(s: &mut MemorySource, fat32: bool, cluster: usize, next: u32) {
    if fat32 {
        u32at(&mut s.0, 32 * 512 + cluster * 4, next);
    } else {
        u16at(&mut s.0, 512 + cluster * 2, next as u16);
    }
}
fn scan(s: &mut dyn ReadSource) -> ScanState {
    let mut state = ScanState::start(s, ScanOptions::default()).unwrap();
    state.run(s, &AtomicBool::new(false), |_| {}).unwrap();
    state
}
fn content(s: &mut dyn ReadSource, item: &crate::engine::Candidate) -> Vec<u8> {
    item.extents
        .iter()
        .flat_map(|e| source::bytes(s, e.offset, e.length as usize).unwrap())
        .collect()
}

#[test]
fn fat16_fragmented_and_contiguous_restore_byte_for_byte_without_writing_source() {
    let mut s = fixture(false);
    entry(&mut s, false, 0, 2, 700);
    entry(&mut s, false, 1, 7, 700);
    fat(&mut s, false, 2, 5);
    fat(&mut s, false, 5, 0xffff);
    s.0[data_offset(false, 2)..data_offset(false, 2) + 512].fill(b'A');
    s.0[data_offset(false, 5)..data_offset(false, 5) + 188].fill(b'B');
    s.0[data_offset(false, 7)..data_offset(false, 7) + 700].fill(b'C');
    let before = s.0.clone();
    let state = scan(&mut s);
    assert_eq!(state.results.len(), 2);
    assert_eq!(state.results[0].kind, RecoveryKind::FatChain);
    assert_eq!(state.results[0].extents.len(), 2);
    assert_eq!(state.results[1].kind, RecoveryKind::FatContiguous);
    let dest = tempfile::tempdir().unwrap();
    fs::write(dest.path().join("_EST.TXT"), b"keep").unwrap();
    let summary = restore::restore(
        &mut s,
        &state.identity,
        &state.results,
        dest.path(),
        &AtomicBool::new(false),
        |_, _| {},
    )
    .unwrap();
    assert_eq!(summary.files.len(), 2);
    assert!(summary.failed.is_empty());
    assert_eq!(
        fs::read(&summary.files[0]).unwrap(),
        [vec![b'A'; 512], vec![b'B'; 188]].concat()
    );
    assert_eq!(fs::read(&summary.files[1]).unwrap(), vec![b'C'; 700]);
    assert_eq!(fs::read(dest.path().join("_EST.TXT")).unwrap(), b"keep");
    assert_eq!(s.0, before);
}
#[test]
fn fat32_deleted_file_and_nested_directory() {
    let mut s = fixture(true);
    let p = root(true);
    s.0[p..p + 11].copy_from_slice(b"HOLIDAY    ");
    s.0[p + 11] = 0x10;
    u16at(&mut s.0, p + 26, 3);
    fat(&mut s, true, 3, 0x0fff_ffff);
    let p = data_offset(true, 3);
    s.0[p..p + 11].copy_from_slice(b"_EST    TXT");
    s.0[p] = 0xe5;
    s.0[p + 11] = 0x20;
    u16at(&mut s.0, p + 26, 4);
    u32at(&mut s.0, p + 28, 5);
    s.0[data_offset(true, 4)..data_offset(true, 4) + 5].copy_from_slice(b"hello");
    let state = scan(&mut s);
    assert_eq!(state.results.len(), 1);
    assert_eq!(state.results[0].original_path, "HOLIDAY/_EST.TXT");
    assert_eq!(
        preview::preview(&mut s, &state.results[0]).unwrap(),
        "hello"
    );
}
#[test]
fn retained_deleted_long_filename() {
    let mut s = fixture(false);
    entry(&mut s, false, 1, 2, 4);
    let p = root(false);
    s.0[p] = 0xe5;
    s.0[p + 11] = 0x0f;
    let short = b"TEST    TXT";
    s.0[p + 13] = short
        .iter()
        .fold(0u8, |v, b| v.rotate_right(1).wrapping_add(*b));
    let mut chars: Vec<u16> = "Résumé.txt".encode_utf16().collect();
    chars.push(0);
    chars.resize(13, 0xffff);
    for (index, c) in [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30]
        .into_iter()
        .zip(chars)
    {
        u16at(&mut s.0, p + index, c);
    }
    let state = scan(&mut s);
    assert_eq!(state.results[0].original_path, "Résumé.txt");
}
#[test]
fn corrupt_chains_are_partial_and_cannot_follow_cycles() {
    let mut s = fixture(false);
    entry(&mut s, false, 0, 2, 1200);
    fat(&mut s, false, 2, 2);
    entry(&mut s, false, 1, 8, 700);
    fat(&mut s, false, 9, 0xffff);
    entry(&mut s, false, 2, 65500, 99);
    let state = scan(&mut s);
    assert_eq!(
        state
            .results
            .iter()
            .map(|c| c.recovered_size)
            .collect::<Vec<_>>(),
        [512, 512, 0]
    );
    assert!(
        state
            .results
            .iter()
            .all(|c| c.confidence == Confidence::Partial)
    );
}
#[test]
fn cancellation_checkpoint_resume_and_identity_change() {
    let mut s = fixture(false);
    entry(&mut s, false, 0, 2, 9);
    let mut state = ScanState::start(&mut s, ScanOptions::default()).unwrap();
    let stop = AtomicBool::new(true);
    assert!(!state.step(&mut s, &stop).unwrap());
    assert_eq!(state.directories_read, 0);
    stop.store(false, Ordering::Relaxed);
    state.step(&mut s, &stop).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scan.json");
    state.save(&path).unwrap();
    let mut resumed = ScanState::load(&path, &mut s).unwrap();
    resumed.run(&mut s, &stop, |_| {}).unwrap();
    assert_eq!(resumed.phase, Phase::Finished);
    assert_eq!(resumed.results.len(), 1);
    s.0[1] ^= 1;
    assert!(ScanState::load(&path, &mut s).is_err());
    assert!(
        restore::restore(
            &mut s,
            &resumed.identity,
            &resumed.results,
            dir.path(),
            &stop,
            |_, _| {}
        )
        .is_err()
    );
}

fn png() -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |kind: &[u8], data: &[u8]| {
        out.extend((data.len() as u32).to_be_bytes());
        out.extend(kind);
        out.extend(data);
        out.extend(crc32fast::hash(&[kind, data].concat()).to_be_bytes());
    };
    chunk(b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
    // A valid zlib stream for a one-pixel RGBA scanline.
    chunk(
        b"IDAT",
        &[
            0x78, 0x01, 0x01, 0x05, 0x00, 0xfa, 0xff, 0x00, 0xff, 0x00, 0x00, 0xff, 0x05, 0x00,
            0x01, 0xff,
        ],
    );
    chunk(b"IEND", &[]);
    out
}
#[test]
fn png_validated_across_block_boundary_invalid_crc_and_duplicates() {
    let image = png();
    let offset = 1024 * 1024 - 3;
    let mut s = MemorySource(vec![0; 2 * 1024 * 1024]);
    s.0[offset..offset + image.len()].copy_from_slice(&image);
    s.0[500..508].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    let state = scan(&mut s);
    assert_eq!(state.results.len(), 1);
    assert_eq!(state.results[0].kind, RecoveryKind::PngCarved);
    assert_eq!(content(&mut s, &state.results[0]), image);
    s.0[offset + 30] ^= 1;
    assert!(scan(&mut s).results.is_empty());
    let mut s = fixture(false);
    entry(&mut s, false, 0, 2, image.len() as u32);
    let p = data_offset(false, 2);
    s.0[p..p + image.len()].copy_from_slice(&image);
    assert_eq!(scan(&mut s).results.len(), 1);
}
#[test]
fn mbr_gpt_and_explicit_offsets_are_bounded() {
    let image = fixture(false).0;
    let mut s = MemorySource(vec![0; image.len() + 1024]);
    s.0[1024..].copy_from_slice(&image);
    s.0[510..512].copy_from_slice(&[0x55, 0xaa]);
    s.0[450] = 6;
    u32at(&mut s.0, 454, 2);
    u32at(&mut s.0, 458, 5000);
    assert_eq!(partition::discover(&mut s, None).unwrap()[0].offset, 1024);
    assert_eq!(
        ScanState::start(&mut s, ScanOptions::default())
            .unwrap()
            .volumes
            .len(),
        1
    );
    assert!(partition::discover(&mut s, Some(u64::MAX)).is_err());
    u32at(&mut s.0, 458, u32::MAX);
    assert!(partition::discover(&mut s, None).is_err());
    let mut s = MemorySource(vec![0; 100 * 512]);
    s.0[510..512].copy_from_slice(&[0x55, 0xaa]);
    s.0[450] = 0xee;
    s.0[512..520].copy_from_slice(b"EFI PART");
    u32at(&mut s.0, 524, 92);
    u64at(&mut s.0, 536, 1);
    u64at(&mut s.0, 544, 99);
    u64at(&mut s.0, 552, 34);
    u64at(&mut s.0, 560, 98);
    u64at(&mut s.0, 584, 2);
    u32at(&mut s.0, 592, 1);
    u32at(&mut s.0, 596, 128);
    s.0[1024] = 1;
    u64at(&mut s.0, 1056, 40);
    u64at(&mut s.0, 1064, 80);
    let crc = crc32fast::hash(&s.0[1024..1152]);
    u32at(&mut s.0, 600, crc);
    let crc = crc32fast::hash(&s.0[512..604]);
    u32at(&mut s.0, 528, crc);
    assert_eq!(
        partition::discover(&mut s, None).unwrap()[0].length,
        41 * 512
    );
    s.0[540] ^= 1;
    assert!(partition::discover(&mut s, None).is_err());
}
#[test]
fn truncations_mutations_and_malformed_result_do_not_panic() {
    let original = fixture(false).0;
    for length in [512, 513, 4096, original.len() - 1] {
        let mut s = MemorySource(original[..length].to_vec());
        let _ = scan(&mut s);
    }
    for index in 0..64 {
        let mut s = MemorySource(original.clone());
        s.0[11 + index % 42] = index as u8 * 3;
        let _ = scan(&mut s);
    }
    let mut s = fixture(false);
    entry(&mut s, false, 0, 2, 2);
    let mut item = scan(&mut s).results.remove(0);
    item.extents = vec![Extent {
        offset: u64::MAX,
        length: 2,
    }];
    assert!(preview::preview(&mut s, &item).is_err());
}
#[test]
fn failed_and_cancelled_restore_remove_temporary_output() {
    struct Failing {
        inner: MemorySource,
        reads: usize,
    }
    impl ReadSource for Failing {
        fn len(&self) -> u64 {
            self.inner.len()
        }
        fn identity(&mut self) -> crate::Result<source::SourceIdentity> {
            self.inner.identity()
        }
        fn read_at(&mut self, o: u64, b: &mut [u8]) -> crate::Result<()> {
            self.reads += 1;
            if self.reads > 1 {
                return Err("synthetic read failure".into());
            }
            self.inner.read_at(o, b)
        }
    }
    let mut s = fixture(false);
    entry(&mut s, false, 0, 2, 700);
    fat(&mut s, false, 2, 5);
    fat(&mut s, false, 5, 0xffff);
    let state = scan(&mut s);
    let dest = tempfile::tempdir().unwrap();
    let mut failing = Failing { inner: s, reads: 0 };
    let report = restore::restore(
        &mut failing,
        &state.identity,
        &state.results,
        dest.path(),
        &AtomicBool::new(false),
        |_, _| {},
    )
    .unwrap();
    assert!(report.files.is_empty());
    assert_eq!(report.failed.len(), 1);
    assert_eq!(fs::read_dir(dest.path()).unwrap().count(), 0);
    let cancel = AtomicBool::new(false);
    let mut s = failing.inner;
    let report = restore::restore(
        &mut s,
        &state.identity,
        &state.results,
        dest.path(),
        &cancel,
        |_, _| cancel.store(true, Ordering::Relaxed),
    )
    .unwrap();
    assert!(report.cancelled);
    assert!(report.files.is_empty());
    assert_eq!(fs::read_dir(dest.path()).unwrap().count(), 0);
}
#[test]
fn preferences_and_safe_names() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let s = Settings {
        language: "ru".into(),
        theme: Theme::Dark,
        startup_updates: false,
        reduced_motion: true,
    };
    s.save(&path).unwrap();
    let loaded = Settings::load(&path).unwrap();
    assert_eq!(loaded.language, "ru");
    assert_eq!(loaded.theme, Theme::Dark);
    assert!(!loaded.startup_updates);
    assert_eq!(restore::safe_name("../../CON.txt"), "_CON.txt");
    assert_eq!(restore::safe_name("x:evil?.txt"), "x_evil_.txt");
}
#[test]
fn update_versions_assets_and_checksums() {
    let release = |tag: &str, name: &str, digest: &str| {
        serde_json::to_vec(&serde_json::json!({"tag_name":tag,"draft":false,"prerelease":false,"assets":[{"name":name,"size":2000,"browser_download_url":"https://github.com/siarheisinelnikau/file-recovery/releases/download/v0.2.0/FileRecovery-v0.2.0.exe","digest":digest}]})).unwrap()
    };
    let digest = format!("sha256:{}", "a".repeat(64));
    assert!(
        updates::select(
            &release("v0.10.0", "FileRecovery-v0.10.0.exe", &digest),
            "0.9.0"
        )
        .unwrap()
        .is_some()
    );
    assert!(
        updates::select(&release("v0.1.0", "x", &digest), "0.1.0")
            .unwrap()
            .is_none()
    );
    assert!(
        updates::select(
            &release("v0.2.0", "FileRecovery-v0.2.0.exe", "bad"),
            "0.1.0"
        )
        .is_err()
    );
    assert!(updates::select(&release("v0.2.0", "wrong.exe", &digest), "0.1.0").is_err());
}

#[test]
fn cancelled_png_validation_resumes_the_same_block() {
    struct Cancelling<'a> {
        inner: MemorySource,
        cancel: &'a AtomicBool,
        armed: bool,
        at: u64,
    }
    impl ReadSource for Cancelling<'_> {
        fn len(&self) -> u64 {
            self.inner.len()
        }
        fn identity(&mut self) -> crate::Result<source::SourceIdentity> {
            self.inner.identity()
        }
        fn read_at(&mut self, offset: u64, out: &mut [u8]) -> crate::Result<()> {
            let result = self.inner.read_at(offset, out);
            if self.armed && offset == self.at {
                self.cancel.store(true, Ordering::Relaxed);
            }
            result
        }
    }
    let cancel = AtomicBool::new(false);
    let image = png();
    let mut bytes = vec![0; 4096];
    bytes[600..600 + image.len()].copy_from_slice(&image);
    let mut s = Cancelling {
        inner: MemorySource(bytes),
        cancel: &cancel,
        armed: false,
        at: 608,
    };
    let mut state = ScanState::start(&mut s, ScanOptions::default()).unwrap();
    state.step(&mut s, &cancel).unwrap();
    assert_eq!(state.phase, Phase::Signatures);
    s.armed = true;
    assert!(!state.step(&mut s, &cancel).unwrap());
    assert_eq!(state.carve_offset, 0);
    assert!(state.results.is_empty());
    cancel.store(false, Ordering::Relaxed);
    s.armed = false;
    state.run(&mut s, &cancel, |_| {}).unwrap();
    assert_eq!(state.results.len(), 1);
}

#[test]
fn cyclic_directory_paths_are_reported_as_incomplete() {
    let mut s = fixture(false);
    let p = root(false);
    s.0[p..p + 11].copy_from_slice(b"LOOP       ");
    s.0[p + 11] = 0x10;
    u16at(&mut s.0, p + 26, 2);
    fat(&mut s, false, 2, 0xffff);
    let p = data_offset(false, 2);
    s.0.copy_within(root(false)..root(false) + 32, p);
    let state = scan(&mut s);
    assert!(state.capped);
    assert!(state.warnings.iter().any(|w| w.contains("cycle")));
    assert_eq!(state.directories_read, 2);
}

#[test]
fn checkpoints_reject_untrusted_queues_and_extents() {
    let mut s = fixture(false);
    entry(&mut s, false, 0, 2, 10);
    let state = ScanState::start(&mut s, ScanOptions::default()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let mut value = serde_json::to_value(&state).unwrap();
    value["queue"][0]["volume"] = serde_json::json!(9999);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(ScanState::load(&path, &mut s).is_err());
    let scanned = scan(&mut s);
    let mut value = serde_json::to_value(&scanned).unwrap();
    value["results"][0]["extents"][0]["length"] = serde_json::json!(u64::MAX);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(ScanState::load(&path, &mut s).is_err());
}

#[test]
fn raw_image_adapter_is_read_only_and_identity_tracks_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.img");
    fs::write(&path, fixture(false).0).unwrap();
    let mut source = source::ImageSource::open(&path).unwrap();
    let identity = source.identity().unwrap();
    let mut sample = [0; 512];
    source.read_at(0, &mut sample).unwrap();
    assert!(source.read_at(identity.length, &mut sample).is_err());
    #[cfg(windows)]
    assert!(fs::OpenOptions::new().write(true).open(&path).is_err());
    drop(source);
    let mut changed = fs::read(&path).unwrap();
    changed[1] ^= 1;
    fs::write(&path, changed).unwrap();
    let mut source = source::ImageSource::open(&path).unwrap();
    assert_ne!(identity, source.identity().unwrap());
}

#[test]
fn malformed_png_palette_and_truncated_tail_are_rejected() {
    let image = png();
    for cut in [8, 32, image.len() - 1] {
        let mut s = MemorySource(vec![0; 4096]);
        s.0[1024..1024 + cut].copy_from_slice(&image[..cut]);
        assert!(scan(&mut s).results.is_empty());
    }
    let mut image = png();
    image[25] = 3; // Indexed color requires PLTE, even with a recomputed IHDR CRC.
    let crc = crc32fast::hash(&image[12..29]);
    image[29..33].copy_from_slice(&crc.to_be_bytes());
    let mut s = MemorySource(vec![0; 4096]);
    s.0[1000..1000 + image.len()].copy_from_slice(&image);
    assert!(scan(&mut s).results.is_empty());
}

fn fake_pe() -> Vec<u8> {
    let mut data = vec![0u8; 2048];
    data[..2].copy_from_slice(b"MZ");
    u32at(&mut data, 60, 128);
    data[128..132].copy_from_slice(b"PE\0\0");
    u16at(&mut data, 132, 0x8664);
    data
}
#[test]
fn update_staging_checks_hash_length_architecture_and_cancellation() {
    use sha2::{Digest, Sha256};
    let data = fake_pe();
    let mut update = updates::AvailableUpdate {
        version: semver::Version::new(0, 2, 0),
        url: String::new(),
        size: data.len() as u64,
        sha256: format!("{:x}", Sha256::digest(&data)),
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("update.exe");
    let stop = AtomicBool::new(false);
    updates::stage_payload(data.as_slice(), &path, &update, &stop, |_, _| {}).unwrap();
    assert_eq!(fs::read(&path).unwrap(), data);
    assert!(updates::stage_payload(data.as_slice(), &path, &update, &stop, |_, _| {}).is_err());
    assert_eq!(fs::read(&path).unwrap(), data);
    fs::remove_file(&path).unwrap();
    update.sha256 = "a".repeat(64);
    assert!(updates::stage_payload(data.as_slice(), &path, &update, &stop, |_, _| {}).is_err());
    assert!(!path.exists());
    update.sha256 = format!("{:x}", Sha256::digest(&data));
    assert!(updates::stage_payload(&data[..1000], &path, &update, &stop, |_, _| {}).is_err());
    assert!(!path.exists());
    assert!(
        updates::stage_payload(data.as_slice(), &path, &update, &stop, |_, _| stop
            .store(true, Ordering::Relaxed))
        .is_err()
    );
    assert!(!path.exists());
    stop.store(false, Ordering::Relaxed);
    let mut wrong = data;
    u16at(&mut wrong, 132, 0xaa64);
    update.sha256 = format!("{:x}", Sha256::digest(&wrong));
    assert!(updates::stage_payload(wrong.as_slice(), &path, &update, &stop, |_, _| {}).is_err());
    assert!(!path.exists());
}

#[test]
fn update_replacement_retains_backup_and_rolls_back_failed_rename() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("FileRecovery.exe");
    let staged = dir.path().join("new.exe");
    let backup = dir.path().join("backup.exe");
    fs::write(&target, b"old executable").unwrap();
    assert!(updates::replace_staged(&target, &staged, &backup).is_err());
    assert_eq!(fs::read(&target).unwrap(), b"old executable");
    assert!(!backup.exists());
    fs::write(&staged, b"new executable").unwrap();
    updates::replace_staged(&target, &staged, &backup).unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"new executable");
    assert_eq!(fs::read(&backup).unwrap(), b"old executable");
    fs::write(&staged, b"next executable").unwrap();
    assert!(updates::replace_staged(&target, &staged, &backup).is_err());
    assert_eq!(fs::read(&target).unwrap(), b"new executable");
}
