const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const {coreHash} = require('./check-shared-core.cjs');

function verify(reports, binaries, expectedCommit, expectedCore, windowsOnly = false) {
  assert.equal(typeof windowsOnly, 'boolean', 'explicit release scope required');
  assert.equal(reports.length, windowsOnly ? 2 : 4, 'complete OS/mode reports required for release scope');
  const targets = windowsOnly ? ['x86_64-pc-windows-msvc'] : ['aarch64-apple-darwin','x86_64-pc-windows-msvc'];
  const expected = new Set(targets.flatMap(t => ['live','paper'].map(m => `${t}/${m}`)));
  const reference = reports[0];
  for (const r of reports) {
    assert(expected.delete(`${r.target}/${r.runtime}`), 'unexpected/duplicate OS/mode');
    assert.equal(r.dirty, false, 'uncommitted source');
    assert.equal(r.source_commit, expectedCommit, 'different source commit');
    assert.equal(r.strategy_core_sha256, expectedCore, 'different strategy source');
    assert.equal(r.version, reference.version, 'different program version');
    assert.match(r.trace_sha256, /^[a-f0-9]{64}$/);
    assert.equal(r.trace_sha256, reference.trace_sha256, 'different strategy decision trace');
    assert.equal(r.observations, 192, 'incomplete parity scenarios');
    assert.equal(r.parameters.length, 4, 'both markets and both capital tiers required');
    assert.deepEqual(r.parameters, reference.parameters, 'different strategy parameters');
  }
  assert.equal(binaries.length, windowsOnly ? 1 : 2, 'packaged binaries required for release scope');
  const expectedBinaries = new Set(windowsOnly ? ['x86_64-pc-windows-msvc/live'] : ['aarch64-apple-darwin/paper','x86_64-pc-windows-msvc/live']);
  for (const b of binaries) {
    assert(expectedBinaries.delete(`${b.target}/${b.runtime}`), 'unexpected packaged runtime');
    for (const key of ['version','source_commit','strategy_core_sha256','dirty']) assert.deepEqual(b[key], reference[key], `packaged binary ${key} mismatch`);
  }
  return {targets,version:reference.version,source_commit:expectedCommit,strategy_core_sha256:expectedCore,trace_sha256:reference.trace_sha256,observations:reference.observations,parameters:reference.parameters};
}
if (require.main === module) {
  const dir = process.argv[2];
  const reports=[], binaries=[];
  const walk=d=>fs.readdirSync(d,{withFileTypes:true}).flatMap(e=>e.isDirectory()?walk(path.join(d,e.name)):[path.join(d,e.name)]);
  for (const file of walk(dir)) {
    if (/parity-(live|paper)\.json$/.test(file)) reports.push(JSON.parse(fs.readFileSync(file)));
    if (path.basename(file)==='build-info.json') binaries.push(JSON.parse(fs.readFileSync(file)));
  }
  const scope=process.argv[3] || 'all';
  assert(['all','windows'].includes(scope), 'unknown release scope');
  const result=verify(reports,binaries,process.env.GITHUB_SHA,coreHash(),scope==='windows');
  fs.writeFileSync(path.join(dir,'shared-core-verification.json'),JSON.stringify(result,null,2)+'\n');
  console.log(`PASS: ${scope==='windows'?'Windows':'Mac/Windows'} paper/live traces, source, parameters and packaged binaries match`);
}
module.exports={verify};
