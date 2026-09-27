const {test}=require('node:test');
const assert=require('node:assert/strict');
const {checkArchitecture,assertCoreSource,strategyParameters}=require('../scripts/check-shared-core.cjs');
const {verify}=require('../scripts/verify-core-parity.cjs');

test('both frontends use the same platform-independent strategy source and parameter templates',()=>assert.match(checkArchitecture(),/^[a-f0-9]{64}$/));
test('architecture guard rejects OS, feature and paper-only decisions',()=>{
  for(const text of ['#[cfg(windows)] fn execute() {}','#[cfg(feature="paper-runtime")] fn entry() {}','if s.config.mode == Mode::Paper {}']) assert.throws(()=>assertCoreSource('strategy.rs',text,true));
  assert.doesNotThrow(()=>assertCoreSource('strategy.rs','#[cfg(test)] mod tests;\nfn entry() {}',true));
  assert.notDeepEqual(strategyParameters({grid:'5'}),strategyParameters({grid:'2'}));
});
test('release gate rejects mismatched decisions, parameters, source and packaged binaries',()=>{
  const base={version:'test',source_commit:'commit',strategy_core_sha256:'a'.repeat(64),dirty:false,trace_sha256:'b'.repeat(64),observations:96,parameters:['openai','anth']};
  const reports=['aarch64-apple-darwin','x86_64-pc-windows-msvc'].flatMap(target=>['live','paper'].map(runtime=>({...base,target,runtime})));
  const binaries=[reports[1],reports[2]].map(r=>({...r}));
  assert.doesNotThrow(()=>verify(reports,binaries,'commit',base.strategy_core_sha256));
  for(const [key,value] of Object.entries({trace_sha256:'c'.repeat(64),source_commit:'old',parameters:['different'],dirty:true,observations:95,target:'wrong'})) {
    const broken=structuredClone(reports);broken[1][key]=value;
    assert.throws(()=>verify(broken,binaries,'commit',base.strategy_core_sha256),key);
  }
  const old=structuredClone(binaries);old[0].strategy_core_sha256='old';
  assert.throws(()=>verify(reports,old,'commit',base.strategy_core_sha256));
});
