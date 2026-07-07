# Archived validation scripts

Historical one-off validators superseded by the current gate stack. They are
kept for archaeology, are NOT wired into `scripts/gate.sh`, CI, or
`phase8-validation.py`, and are not maintained against the current tree
(several still reference the pre-workspace `cli/` layout).

The current programmatic gate is `scripts/validation/phase8-validation.py`
(invoked by `scripts/gate.sh`); see `docs/TESTING.md`.

| Script | Why archived (evidence: research/scripts-docs-sweep.md) |
|---|---|
| `phase0.sh` | Phase 0 fork-governance + synthetic baseline wrapper; superseded by direct `workload-baseline.py` use and the current gates. |
| `phase6-validation.py` | Phase 6 orchestrator; its factory/read/large-edit/no-real-write legs are covered by `phase7-validation.py` and `phase8-validation.py`. |
| `phase65-validation.py` | Phase 6.5 read fast-path orchestrator; Phase 8 calls `base-read-benchmark.py` and `fuse-serialization-stress.py` directly. |
| `check-fork-governance.sh` | Origin-remote governance check from the fork-era workflow; not CI-wired and no longer an active local policy. `phase0.sh` invokes it dirname-relative, so both live here together. |
| `backend-risk-spike.py` | Turso 0.5.x upgrade decision spike; the workspace has shipped on Turso 0.5 since before the restructure, so this is historical evidence rather than an active probe. |
