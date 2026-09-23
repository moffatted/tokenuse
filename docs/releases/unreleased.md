# Unreleased

Changes that should be included in the next release go here. Keep this file current during normal development; move the relevant notes into `docs/releases/<version>.md` only when preparing a release.

## Added

- **Skylark tool adapter.** Reads the `skylark-model-usage-v1` JSON Lines export the Skylark daemon writes to `~/.skylark/usage/`, covering its cloud-escalation, vLLM Semantic Router, direct vLLM and Hailo NPU lanes. Uses the producer's own `skylark:<trace_id>:<sequence>` dedup key verbatim, so a call Skylark escalated through GitHub Copilot is not counted twice against Copilot's own session store. Resumes incrementally by byte offset, surfaces the export's gap markers rather than swallowing them, and never re-prices a record Skylark declined to price. `SKYLARK_DIR` overrides the data directory. See [docs/development/tools/skylark.md](../development/tools/skylark.md).

## Changed

- **Skylark gap markers name their cause.** The Skylark export's `gap` lines now carry an additive `cause` field (`broadcast_lag`, `write_failure` or `shutdown_drain`). The adapter still sums `lagged` across every cause, so no total changes meaning; the committed conformance fixture and its digest moved in step with the producer. See [docs/development/tools/skylark.md](../development/tools/skylark.md).

## Removed

No removals recorded yet.
