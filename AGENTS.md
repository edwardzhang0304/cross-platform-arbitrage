# Scope

This is the single Rust source for Mac paper and Windows live delivery.
Both runtimes must import the same openai_inventory library. Do not copy the
strategy into platform folders or add OS/mode branches to decision, execution,
position, quota, accounting or moving-average math. Differences belong in venue
adapters, credentials/data ownership, startup, OS power controls and packaging.
Run the shared-core architecture check and both feature test suites on Mac and
Windows; verified artifacts require matching source and deterministic decisions.
Do not change trading rules while packaging. Preserve the strategy state and
vault formats, exact-base pair quantity, unknown-fill reconciliation, v2
residual recovery and original risk gates. Trade submissions must stay inside
the existing venue workers and guarded execution path.

Never commit data/, runtime/, real configuration, vaults, credentials, session
tokens or logs. Test with synthetic fixtures only. Never launch a live account
for CI or a packaging test. All remote repositories and artifacts are private.

Run strategy unit tests, signer reference vectors, frontend tests, and a
credential-free Windows packaged-binary smoke test before creating a release.
Keep configuration, encrypted keys and the durable ledger separate. Migration
requires a stopped source with no unresolved operation and a consistent backup.
Do not claim this prevents manual sleep, power loss, forced reboot or lid actions.
