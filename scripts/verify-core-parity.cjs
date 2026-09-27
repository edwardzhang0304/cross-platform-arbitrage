const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const {coreHash} = require('./check-shared-core.cjs');

function verify(reports, binaries, expectedCommit, expectedCore) {
  assert.equal(reports.length, 4, 'four OS/mode reports required');
  const expected = new Set(['aarch64-apple-darwin/live','aarch64-apple-darwin/paper','x86_64-pc-windows-msvc/live','x86_64-pc-windows-msvc/paper']);
  const reference = reports[0];
  for (const r of reports) {
    assert(expected.delete(`${r.target}/${r.runtime}`), 'unexpected/duplicate OS/mode');
    assert.equal(r.dirty, false, 'uncommitted source');
    assert.equal(r.source_commit, expectedCommit, 'different source commit');
    assert.equal(r.strategy_core_sha256, expectedCore, 'different strategy source');
    assert.equal(r.version, reference.version, 'different program version');
    assert.match(r.trace_sha256, /^[a-f0-9]{64}$/);
    assert.equal(r.trace_sha256, reference.trace_sha256, 'different strategy decision trace');
    assert.equal(r.observations, 96, 'incomplete parity scenarios');
    assert.equal(r.parameters.length, 2, 'both market parameter sets required');
    assert.deepEqual(r.parameters, reference.parameters, 'different strategy parameters');
  }
  assert.equal(binaries.length, 2, 'both packaged binaries required');
  const expectedBinaries = new Set(['aarch64-apple-darwin/paper','x86_64-pc-windows-msvc/live']);
  for (const b of binaries) {
    assert(expectedBinaries.delete(`${b.target}/${b.runtime}`), 'unexpected packaged runtime');
    for (const key of ['version','source_commit','strategy_core_sha256','dirty']) assert.deepEqual(b[key], reference[key], `packaged binary ${key} mismatch`);
  }
  return {version:reference.version,source_commit:expectedCommit,strategy_core_sha256:expectedCore,trace_sha256:reference.trace_sha256,observations:reference.observations,parameters:reference.parameters};
}
if (require.main === module) {
  const dir = process.argv[2];
  const reports=[], binaries=[];
  const walk=d=>fs.readdirSync(d,{withFileTypes:true}).flatMap(e=>e.isDirectory()?walk(path.join(d,e.name)):[path.join(d,e.name)]);
  for (const file of walk(dir)) {
    if (/parity-(live|paper)\.json$/.test(file)) reports.push(JSON.parse(fs.readFileSync(file)));
    if (path.basename(file)==='build-info.json') binaries.push(JSON.parse(fs.readFileSync(file)));
  }
  const result=verify(reports,binaries,process.env.GITHUB_SHA,coreHash());
  fs.writeFileSync(path.join(dir,'shared-core-verification.json'),JSON.stringify(result,null,2)+'\n');
  console.log('PASS: Mac/Windows × paper/live traces, source, parameters and packaged binaries match');
}
module.exports={verify};
