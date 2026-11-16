const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');

function releaseInputs(root, context) {
  if (context.eventName !== 'push' || context.ref !== 'refs/heads/main') {
    throw new Error('Releases require an explicit release commit on main.');
  }
  const manifest = fs.readFileSync(path.join(root, 'Cargo.toml'), 'utf8');
  const version = manifest.match(/^version = "(\d+\.\d+\.\d+)"$/m)?.[1];
  const requested = context.payload.head_commit.message.match(/^Release v(\d+\.\d+\.\d+)(?:\s|$)/)?.[1];
  if (!version || requested !== version || !/^[a-f0-9]{40}$/.test(context.sha)) {
    throw new Error('Release commit, package version and source revision must agree.');
  }
  const executable = `FileRecovery-v${version}.exe`;
  const assets = [executable, 'SHA256SUMS.txt', 'LICENSE.txt', 'THIRD-PARTY-NOTICES.txt'].map(name => {
    const file = path.join(root, 'dist', name);
    const stat = fs.lstatSync(file);
    if (!stat.isFile() || stat.isSymbolicLink() || stat.size === 0 || stat.size > 256 * 1024 * 1024) {
      throw new Error(`Invalid release asset: ${name}`);
    }
    const data = fs.readFileSync(file);
    return { name, data, digest: `sha256:${crypto.createHash('sha256').update(data).digest('hex')}` };
  });
  const exe = assets[0].data;
  const pe = exe.length >= 64 ? exe.readUInt32LE(60) : exe.length;
  if (exe.length < 1024 || exe.toString('ascii', 0, 2) !== 'MZ' || pe < 64 || pe + 6 > exe.length ||
      exe.toString('ascii', pe, pe + 4) !== 'PE\x00\x00' || exe.readUInt16LE(pe + 4) !== 0x8664) {
    throw new Error('The release executable is not a Windows x64 PE.');
  }
  const sums = assets[1].data.toString('utf8').trim();
  if (sums !== `${assets[0].digest.slice(7)}  ${executable}`) {
    throw new Error('The executable does not match SHA256SUMS.txt.');
  }
  if (!assets[2].data.toString('utf8').startsWith('MIT License') ||
      !assets[3].data.toString('utf8').includes(`File Recovery ${version}`)) {
    throw new Error('Required license notices are missing or stale.');
  }
  return { version, tag: `v${version}`, assets };
}

function releaseBody(version) {
  return `First public Windows x64 portable release of File Recovery, including deleted-file recovery for NTFS 3.0/3.1.

### Recovery
- Recover resident NTFS files and fragmented nonresident data from MFT records, with update-sequence fixups and current allocation checks through $Bitmap.
- Recover sparse and uninitialized NTFS ranges as logical zeroes. Retain available Unicode names and parent paths; label missing or stale parent paths explicitly.
- Read FAT16/FAT32 deleted entries and retained chains, and carve PNG containers after checking structure and CRCs.
- Scan raw images and supported MBR/GPT regions, pause and resume scans tied to source identity, inspect bounded text/byte previews, and restore to an explicitly chosen destination without overwriting existing files.

### Windows workspace
- Native CPU/GDI interface, light/dark themes, reduced motion, and persistent English, Russian, French, German, Spanish and Italian preferences.
- Startup/manual update checking with an opt-out, and a user-triggered SHA-256-verified portable replacement flow.

### Validation and limits
- Automated checks use generated images, corrupted records and fake adapters only. Formatting, strict Clippy, synthetic recovery/restore tests, release packaging and embedded-icon/PE checks pass in the publishing workflow.
- GUI interaction, idle/hidden performance, real devices and the complete update/restart cycle have not been tested. The physical-device adapter remains unverified; no drivers or elevation are installed/requested.
- NTFS compression, EFS, reparse-point files, attribute lists and split attribute segments are unsupported. A volume whose MFT or allocation bitmap needs an attribute list is unsupported. Metadata scans are bounded to 1,000,000 MFT records per volume; recovery is limited to a 4 GiB prefix per file.
- Recovery uses unnamed DATA streams and one preferred Unicode filename per file. Alternate data streams and hard-link reconstruction are not implemented.
- Currently allocated clusters are excluded from NTFS nonresident file recovery. Earlier overwrites of free clusters and SSD-discarded content cannot be reconstructed or reliably detected. Cleared FAT chains require an unverified contiguous-data inference. PNG pixel decompression is not verified.
- Restored filenames are sanitized and flattened into the chosen folder; partial outputs use .partial. Source identity uses metadata and sparse SHA-256 samples rather than a full-image digest.

### Download
Use FileRecovery-v${version}.exe on Windows 10/11 x64. No installer is required. Verify it against SHA256SUMS.txt. MIT license; third-party copyrights and licenses are included in the notices asset and application.
`;
}

async function publish({github, context, core}, root = path.resolve(__dirname, '..')) {
  const {version, tag, assets} = releaseInputs(root, context);
  const repo = context.repo;
  let release;
  try {
    release = (await github.rest.repos.getReleaseByTag({...repo, tag})).data;
  } catch (error) {
    if (error.status !== 404) throw error;
  }
  if (release && (!release.draft || release.target_commitish !== context.sha)) {
    throw new Error('An existing release belongs to another publication; no changes were made.');
  }
  if (!release) {
    release = (await github.rest.repos.createRelease({...repo, tag_name: tag,
      target_commitish: context.sha, name: `File Recovery ${version}`,
      body: releaseBody(version), draft: true, prerelease: false})).data;
  }
  let existing = (await github.rest.repos.listReleaseAssets({...repo, release_id: release.id, per_page: 100})).data;
  if (existing.some(asset => !assets.some(expected => expected.name === asset.name))) {
    throw new Error('Draft contains unexpected assets; it was preserved.');
  }
  for (const asset of assets) {
    const prior = existing.find(old => old.name === asset.name);
    if (prior) {
      if (prior.digest !== asset.digest || prior.size !== asset.data.length || prior.state !== 'uploaded') {
        throw new Error(`Existing draft asset differs: ${asset.name}. Draft was preserved.`);
      }
      continue;
    }
    await github.rest.repos.uploadReleaseAsset({...repo, release_id: release.id, name: asset.name,
      data: asset.data, headers: {'content-type': 'application/octet-stream', 'content-length': asset.data.length}});
  }
  existing = (await github.rest.repos.listReleaseAssets({...repo, release_id: release.id, per_page: 100})).data;
  if (existing.length !== assets.length || assets.some(asset => !existing.some(remote =>
    remote.name === asset.name && remote.state === 'uploaded' && remote.size === asset.data.length && remote.digest === asset.digest))) {
    throw new Error('Release asset verification failed; the release remains a draft.');
  }
  const published = (await github.rest.repos.updateRelease({...repo, release_id: release.id,
    body: releaseBody(version), draft: false, prerelease: false, make_latest: 'true'})).data;
  core.notice(`Published ${tag}: ${published.html_url}`);
  return published;
}

module.exports = {publish, releaseInputs};
