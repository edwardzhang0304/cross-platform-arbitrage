# Scope

This is the isolated Windows delivery of the existing OPENAI paired strategy.
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
