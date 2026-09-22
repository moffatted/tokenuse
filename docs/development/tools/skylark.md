# Skylark

[Skylark](https://github.com/moffatted/skylark) is a privacy-first VS Code extension backed by a native Rust daemon (`skylark-daemon`). The daemon routes every inference it performs across four backend classes and meters what each one consumed, appending the record to a rotating JSON Lines log. That log is the export this adapter reads. There is no second format, no translation step, and no network call on either side.

This adapter is the consuming half of Skylark's `skylark-model-usage-v1` export contract, specified in that repository at `skylark_docs/docs/design/model_usage_export_contract.md`.

## Source format

| | |
| --- | --- |
| Location | `$HOME/.skylark/usage/usage-NNNNNN.jsonl` |
| Env override | `SKYLARK_DIR` (the adapter appends `usage/`) |
| Shape | JSON Lines, one record per line, append-only |
| Rotation | 8 MiB per file, 64 MiB retained, oldest file evicted whole |
| Contract version | `skylark-model-usage-v1`, on every line |

Each `usage-NNNNNN.jsonl` is one `SessionSource`. Files are ordered by the numeric index in the name, not by modification time: a copied or restored tree has whatever mtimes the copy gave it, and the index is what actually records emission order.

### A `usage` line

```json
{
  "record": "usage",
  "contractVersion": "skylark-model-usage-v1",
  "dedupKey": "skylark:<trace_id>:<sequence>",
  "event": {
    "contract_version": "watch-me-working-v1",
    "event_type": "model.usage",
    "timestamp_utc": 1789776000,
    "session_id": "escalation_rpc",
    "trace_id": "...",
    "run_id": "run-105",
    "span_id": "span-...",
    "source": "core",
    "severity": "info",
    "sequence": 1001,
    "payload": {
      "backend": "cloud_escalation",
      "model": "claude-sonnet-4-6",
      "promptTokens": 1200,
      "completionTokens": 340,
      "tokenQuality": "exact",
      "latencyMs": 812,
      "costUsd": "0.008700",
      "pricingVersion": "2026-06-24+..."
    }
  }
}
```

Two contract versions appear on every line and they are deliberately different strings. `contractVersion` at the top level is this export's, which Skylark bumps when a consumer that parsed the previous version would get a wrong answer from this one. `event.contract_version` is Skylark's internal Watch Me Work event-bus version, which governs the envelope and moves on schedules this adapter does not care about. A line whose top-level `contractVersion` is not the one in `config.rs` is skipped rather than guessed at.

### A `gap` line

```json
{"record":"gap","contractVersion":"skylark-model-usage-v1","lagged":44,"detectedAtUtc":1789776300}
```

Skylark's sink subscribes to a bounded broadcast channel. A subscriber that falls behind is told how many events it missed, and the sink writes that number to the log rather than losing it silently. `ExportParse::lagged_rows` sums these. **Totals over a file with a non-zero gap are a floor, not a figure** — the marker exists so "we stopped listening" is not read as "nothing happened".

## Backend coverage

The export states its own coverage, so a consumer never has to guess whether a missing backend means zero usage or no coverage.

| `backend` | What it is |
| --- | --- |
| `cloud_escalation` | Anthropic, Gemini, DeepSeek or GitHub Copilot, via the daemon's escalation route |
| `vsr` | The vLLM Semantic Router's data plane |
| `local_vllm` | A vLLM server dialled directly, with no router in front of it |
| `npu` | The Hailo Neural Processing Unit lane |

There is no separate remote-vLLM value. The Semantic Router's local and remote endpoints are the router's own choice, made behind its data plane, and are not distinguishable from the daemon's side; a remote endpoint reached through the router is recorded as `vsr`.

## Token quality

Skylark derives `tokenQuality` from what the backend actually returned — it is never passed in at a call site.

| Skylark | This repository | Meaning |
| --- | --- | --- |
| `exact` | `TokenQuality::Exact` | The backend returned both counts |
| `estimated` | `TokenQuality::Estimated` | The counts were derived by some means |
| `absent` | `TokenQuality::Unknown` | Not a complete backend-reported accounting |

`absent` has no exact equivalent here, and mapping it to `Estimated` would claim a derivation nobody performed. Two backends are permanently `absent`: the NPU lane's embed and classify responses carry no token field at all, and GitHub Copilot's software development kit structurally cannot report prompt tokens. Whichever half *was* reported is still carried verbatim — a Copilot row keeps its real completion count.

## Cost

Skylark prices at import time from a dated table and writes the figure as a **fixed-point decimal string**, not a JSON number, precisely so a consumer does not read money back as a binary float. Three states must stay distinguishable:

| `costUsd` | `pricingVersion` | Meaning |
| --- | --- | --- |
| `"0.008700"` | set | A real figure from a published per-token rate |
| `"0.000000"` | set | A real, recorded zero — the self-hosted lanes bill nothing per token |
| `null` | `null` | Unpriceable: no rate, or counts Skylark refuses to multiply by a non-zero rate |

`pricingVersion` is null exactly when `costUsd` is; a figure without the table that produced it asserts a provenance it does not have, and a line where the two disagree is refused.

`SkylarkRecord::cost_usd` is `Option<f64>` so the null survives to this repository's boundary. `ParsedCall::cost_usd` is a plain `f64`, so an unpriceable call contributes `0.0` to a spend total. That is the right total — an unpriceable call adds no money to it — but it is not the right fact, and **this adapter does not repair it by re-pricing the call from this repository's own pricing book**. Skylark declined to put a number on that call for a stated reason, and inventing one here would put a fabrication into a column presented as money.

## Deduplication

`dedupKey` is `skylark:<trace_id>:<sequence>`, and this adapter uses it verbatim. Both halves are properties of the record rather than of the act of writing it, so a record delivered twice — a replayed sync, an overlapping parse — yields one key and therefore one row.

The key exists for one specific hazard. Skylark escalates through GitHub Copilot, and this dashboard reads Copilot's own session store from the other side. Without a key both sides derive identically, that call is counted twice and the error is invisible — it looks like usage. `parser::dedup_key` re-derives the key and refuses a record whose stated key does not match, which is what would catch an independent implementation computing it a slightly different way.

## Incremental parsing

`parse_with_cursor` resumes from a stored byte offset. This is safe because the log is append-only: the daemon writes whole lines and rotates to a new file rather than rewriting one. A probe hash over the 256 bytes immediately before the offset catches a file replaced by a copy or a restore that is at least as long as it was.

A final line with no newline is the half-written object a daemon killed mid-write leaves behind. It is not parsed, and the cursor is not advanced past it, so the next sync re-reads it once the daemon has finished writing it.

## Conformance fixture

`src/tools/skylark/fixtures/usage_export_v1.jsonl` is a byte-for-byte copy of the fixture committed in the Skylark repository. Both repositories pin the same FNV-1a 64 digest over it (`CONFORMANCE_FIXTURE_DIGEST`), which is what makes two copies the same file across two independently released repositories with no shared checkout: a byte that moves in either fails that repository's own suite rather than quietly producing two parsers that agree about nothing.

The fixture exercises every contract field, all four backend classes, all three token qualities, all three cost states, and a gap marker. Its expected totals — six calls, 2046 input tokens, 1189 output tokens, four priced rows, two unpriced, $0.0087 of spend, 44 lagged rows — are asserted in `parser::tests`.

Updating the fixture means updating the digest in **both** repositories in the same change.

That sentence is a human process, and it failed twice in three weeks: the producing repository moved, re-pinned, and this one was never touched, so both suites stayed green over two different contracts. The digest cannot catch that by construction — each repository's own suite only ever hashes its own copy. `parser::tests::the_conformance_fixture_is_byte_identical_to_the_producers_copy` closes the gap by reading the producer's committed file directly and comparing the bytes. It resolves the sibling checkout from `CARGO_MANIFEST_DIR` (never a hard-coded home path) or from `SKYLARK_CONFORMANCE_FIXTURE`, and it skips when no sibling checkout is present — which is its residual bound: a machine holding only this repository gets the digest check and no cross-check. An override that is set but points at no file is a misconfiguration and fails rather than skipping.

## Caveats

- Skylark records the model identifier byte for byte, including a vendor path such as `QuantTrio/Qwen3-Coder-30B-A3B-Instruct-AWQ`. Normalising it upstream would destroy information a consumer can never recover.
- Only successful calls are metered. A failed call consumed nothing Skylark can honestly account for, and a record full of zeros would be indistinguishable from a free call.
- `session_id` here is Skylark's `run_id` — the governed agent run — falling back to the emitting site's name for calls not scoped to a run.
- This adapter reads Skylark's own routed inference only. Skylark deliberately does not ingest usage from tools it does not route; that is this dashboard's job, and the export is how it gets Skylark's share.
