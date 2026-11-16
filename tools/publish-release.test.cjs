const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const crypto = require('node:crypto');
const {publish} = require('./publish-release.cjs');

function fixture(t) {
  const base=fs.realpathSync(os.tmpdir());
  const root = fs.mkdtempSync(path.join(base, 'file-recovery-release-'));
  t.after(() => {
    const resolved=fs.realpathSync(root);
    assert.equal(path.dirname(resolved),base);
    assert.ok(path.basename(resolved).startsWith('file-recovery-release-'));
    fs.rmSync(resolved, {recursive:true, force:true});
  });
  fs.mkdirSync(path.join(root, 'dist'));
  fs.writeFileSync(path.join(root, 'Cargo.toml'), '[package]\nversion = "0.2.0"\n');
  const exe = Buffer.alloc(2048);exe.write('MZ');exe.writeUInt32LE(128,60);exe.write('PE\x00\x00',128);exe.writeUInt16LE(0x8664,132);
  const hash = crypto.createHash('sha256').update(exe).digest('hex');
  fs.writeFileSync(path.join(root, 'dist', 'FileRecovery-v0.2.0.exe'), exe);
  fs.writeFileSync(path.join(root, 'dist', 'SHA256SUMS.txt'), `${hash}  FileRecovery-v0.2.0.exe\n`);
  fs.writeFileSync(path.join(root, 'dist', 'LICENSE.txt'), 'MIT License\n');
  fs.writeFileSync(path.join(root, 'dist', 'THIRD-PARTY-NOTICES.txt'), 'File Recovery 0.2.0\n');
  const context = {eventName:'push',ref:'refs/heads/main',sha:'a'.repeat(40),repo:{owner:'test',repo:'fixture'},payload:{head_commit:{message:'Release v0.2.0'}}};
  const calls = [];
  const assets = [];
  let release;
  const repos = {
    getReleaseByTag: async () => {if (!release) throw Object.assign(new Error('not found'),{status:404});return {data:release};},
    createRelease: async params => {calls.push(['create',params]);release={...params,id:1};return {data:release};},
    listReleaseAssets: async () => ({data:assets}),
    uploadReleaseAsset: async params => {calls.push(['upload',params.name]);const digest='sha256:'+crypto.createHash('sha256').update(params.data).digest('hex');assets.push({name:params.name,size:params.data.length,digest,state:'uploaded'});return {data:assets.at(-1)};},
    updateRelease: async params => {calls.push(['publish',params]);release={...release,...params,html_url:'https://example.test/release'};return {data:release};}
  };
  return {root,context,calls,assets,repos,client:{github:{rest:{repos}},context,core:{notice:()=>{}}}};
}

test('publish only after every generated asset is uploaded and its digest verified',async t=>{
  const f=fixture(t);const result=await publish(f.client,f.root);assert.equal(result.draft,false);assert.equal(f.calls[0][1].draft,true);assert.equal(f.calls.filter(c=>c[0]==='upload').length,4);assert.equal(f.calls.at(-1)[0],'publish');
});
test('bad checksum fails before creating a draft',async t=>{
  const f=fixture(t);fs.appendFileSync(path.join(f.root,'dist','FileRecovery-v0.2.0.exe'),'changed');await assert.rejects(publish(f.client,f.root),/SHA256SUMS/);assert.equal(f.calls.length,0);
});
test('upload failure leaves a draft and supports retry with the same verified assets',async t=>{
  const f=fixture(t);const upload=f.repos.uploadReleaseAsset;let count=0;f.repos.uploadReleaseAsset=async p=>{if (++count===3) throw new Error('synthetic upload failure');return upload(p);};
  await assert.rejects(publish(f.client,f.root),/synthetic upload failure/);assert.equal(f.calls.some(c=>c[0]==='publish'),false);
  f.repos.uploadReleaseAsset=upload;const result=await publish(f.client,f.root);assert.equal(result.draft,false);assert.equal(f.assets.length,4);
});
test('published releases cannot be overwritten',async t=>{
  const f=fixture(t);await publish(f.client,f.root);const count=f.calls.length;await assert.rejects(publish(f.client,f.root),/existing release/);assert.equal(f.calls.length,count);
});
test('a mismatched server digest prevents publication',async t=>{
  const f=fixture(t);const upload=f.repos.uploadReleaseAsset;f.repos.uploadReleaseAsset=async p=>{const r=await upload(p);r.data.digest='sha256:'+'0'.repeat(64);return r;};await assert.rejects(publish(f.client,f.root),/verification failed/);assert.equal(f.calls.some(c=>c[0]==='publish'),false);
});
test('ordinary commits and mismatched versions never publish',async t=>{
  const f=fixture(t);f.context.payload.head_commit.message='Update source';await assert.rejects(publish(f.client,f.root),/must agree/);f.context.payload.head_commit.message='Release v9.0.0';await assert.rejects(publish(f.client,f.root),/must agree/);assert.equal(f.calls.length,0);
});
