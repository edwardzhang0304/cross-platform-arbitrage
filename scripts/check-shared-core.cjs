const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const {createHash} = require('node:crypto');
const root = path.resolve(__dirname, '..');
const manifest = JSON.parse(fs.readFileSync(path.join(root, 'config/shared-core-files.json')));
const read = file => fs.readFileSync(path.join(root, file), 'utf8').replace(/\r\n/g, '\n');

function coreHash() {
  const hash = createHash('sha256');
  for (const file of manifest.files) hash.update(file).update('\0').update(read(file)).update('\0');
  return hash.digest('hex');
}
function assertCoreSource(file, source, modeAgnostic) {
  assert(!/#\s*\[\s*cfg(?:_attr)?\s*\([^\]]*\b(?:windows|unix|target_os|target_arch|feature)\b/s.test(source), `${file}: platform/feature branch in shared core`);
  if (modeAgnostic) assert(!/\bMode::(?:Paper|Live)\b|\bconfig\.mode\b/.test(source), `${file}: mode-dependent trading rule`);
}
function strategyParameters(config) {
  const result = structuredClone(config);
  for (const key of ['market','mode','lighter_account','lighter_account_index','lighter_address','entropy_account','entropy_address','paper_capital_per_venue']) delete result[key];
  return result;
}
function checkArchitecture() {
  for (const file of manifest.files.filter(f => f.endsWith('.rs'))) assertCoreSource(file, read(file), manifest.mode_agnostic.includes(file));
  const lib = read('src/lib.rs');
  assert.match(lib, /#\[path\s*=\s*"strategies\/openai_inventory\/mod.rs"\]\s*pub mod openai_inventory;/);
  assert.match(read('src/main.rs'), /openai_paired_trader::/);
  assert.match(read('src/bin/paired-paper.rs'), /openai_paired_trader::/);
  // A second copied decision implementation cannot silently become another core.
  const walk = dir => fs.readdirSync(dir, {withFileTypes:true}).flatMap(e => e.isDirectory() ? walk(path.join(dir,e.name)) : [path.join(dir,e.name)]);
  const sources = walk(path.join(root, 'src')).filter(f=>f.endsWith('.rs')).map(f=>fs.readFileSync(f,'utf8')).join('\n');
  for (const name of ['timed_entry_candidates','group_exit_eligible','evaluate_inner']) assert.equal([...sources.matchAll(new RegExp(`\\bfn\\s+${name}\\s*\\(`,'g'))].length, 1, `duplicate strategy implementation: ${name}`);
  assert.deepEqual(strategyParameters(JSON.parse(read('config/strategy.example.json'))), strategyParameters(JSON.parse(read('config/strategy.anth.example.json'))));
  return coreHash();
}
if (require.main === module) console.log(`PASS: one shared core ${checkArchitecture()}`);
module.exports = {coreHash, checkArchitecture, assertCoreSource, strategyParameters};
