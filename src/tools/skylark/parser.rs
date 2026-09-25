//! Parses Skylark's `skylark-model-usage-v1` export.
//!
//! Skylark is a privacy-first VS Code extension backed by a native Rust
//! daemon that routes inference across four backend classes — cloud
//! escalation, the vLLM Semantic Router, a directly dialled vLLM server, and
//! a Hailo Neural Processing Unit lane. The daemon meters every call it
//! routes and appends the record to a rotating JSON Lines log. That log is
//! the export; there is no second format and no translation step on either
//! side.
//!
//! Three properties of that contract shape this parser, and each has a test
//! below:
//!
//! - **The dedup key is the producer's, not ours.** Skylark escalates through
//!   GitHub Copilot, and this dashboard reads Copilot's own session store
//!   from the other side. The export carries `skylark:<trace_id>:<sequence>`
//!   on every record precisely so the same call is not counted twice, and
//!   this adapter uses that key verbatim. Deriving our own would defeat the
//!   field's only purpose.
//! - **Only an exact count may carry a figure.** A record whose
//!   `tokenQuality` is `estimated` or `absent` may carry a recorded zero and
//!   nothing else; a figure beside a count nobody reported is an estimate
//!   becoming money, and this adapter refuses the line rather than totalling
//!   it. The producer refuses it too, which is the point — two independently
//!   released repositories agreeing because both enforce the rule, not
//!   because neither was ever handed a row that breaks it.
//! - **A null cost is not a zero cost.** Skylark prices at import time from a
//!   dated table and writes `costUsd` as a fixed-point decimal *string*, so a
//!   consumer does not read money back as a binary float. A record it could
//!   not price carries an explicit null with its tokens intact. `0.000000` is
//!   a real, recorded rate — what the self-hosted lanes legitimately cost —
//!   and the two must not be conflated. See [`SkylarkRecord::cost_usd`].
//! - **A gap marker is data.** A `record: "gap"` line says rows were lost
//!   before the sink could see them. Skipping it silently would turn "we
//!   stopped listening" into "nothing happened", which is the one inference
//!   the marker exists to prevent.
//!
//! Per this repository's defensive-parsing habit, one unreadable line costs
//! that line and nothing else: a daemon killed mid-write leaves a
//! half-written final object with no newline, and one bad line must never
//! cost a file's history.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};
use color_eyre::Result;
use serde::{Deserialize, Serialize};

use crate::tools::{
    AdapterParse, InteractionMode, ParsedCall, SessionSource, Speed, TimestampQuality, TokenQuality,
};

use super::config;

/// Bump to invalidate persisted resume cursors when parsing semantics change.
const PARSE_VERSION: &str = "1";

/// Bytes hashed immediately before a stored offset, so a file rewritten where
/// the resume would land is detected even when it is at least as long as it
/// was.
const PROBE_BYTES: u64 = 256;

/// The committed conformance fixture, shared with the producing repository.
pub const CONFORMANCE_FIXTURE: &str = include_str!("fixtures/usage_export_v1.jsonl");

/// FNV-1a 64 over [`CONFORMANCE_FIXTURE`]'s bytes.
///
/// The producing repository pins the identical constant over its identical
/// copy. Two independently released repositories with no shared checkout
/// cannot literally test one file, so this digest is what makes the two
/// copies the same file: a byte that moves in either fails that repository's
/// own suite rather than quietly producing two parsers that agree about
/// nothing.
///
/// Moved to `16d3e5c9a95cc6d8` at Skylark's Phase 46 Week 106b Day 3, from
/// `68084f9fc17cdd08`. The producer's fixture moved when DeepInfra's
/// `zai-org/GLM-5.3-Flash` rows landed and were then corrected: the live
/// pricing version reached `2026-06-24+5dc89f85` and the fixture's four
/// non-null `pricingVersion` values followed. This repository was not touched
/// by that change, so its copy kept recording the superseded
/// `2026-06-24+eadff5a2` -- the second silent divergence in three weeks, and
/// the reason the test below now compares the producer's committed bytes
/// directly rather than only re-deriving a digest this repository pinned to
/// itself.
///
/// Only `pricingVersion` differs between the two copies; no `costUsd` moved,
/// and the totals the fixture test asserts are unchanged.
///
/// Moved to `fd41f80cee8af43e` at Skylark's Phase 46 Week 106c Day 4, in step
/// with the producer. Gap lines gained an additive `cause` field
/// (`broadcast_lag`, `write_failure` or `shutdown_drain`): the existing marker
/// now names `broadcast_lag`, and two markers were added so the fixture
/// exercises the other two causes. No usage line changed, so every call and
/// cost total is unchanged; the fixture's lagged-row total rises from 44 to 47.
///
/// Moved to `0743e8b8170cd9bc` at Skylark's Phase 46 Week 106d Day 1, in step
/// with the producer. Usage lines gained an additive `servedModel`, null where
/// no response named a model, and the `vsr` line became the auto-routed shape:
/// `model` is the requested alias `auto` and `servedModel` the model that
/// answered. No count, cost or pricing version moved, so every total is
/// unchanged.
///
/// Moved to `23a4ea93be256b0d` at Skylark's Phase 46 Week 106f Day 2, in step
/// with the producer. Usage lines gained three additive prompt-cache counts,
/// `cacheReadTokens`, `cacheWrite5mTokens` and `cacheWrite1hTokens`: zero on
/// the Sonnet line, which reported them, and null elsewhere. This reader does
/// not consume them yet, and ignores them as it ignores any unknown field, so
/// no row is dropped. The producer's pricing table also moved, so the priced
/// lines now record `2026-06-24+eae90d17`. No count or cost moved, so every
/// total is unchanged.
pub const CONFORMANCE_FIXTURE_DIGEST: &str = "23a4ea93be256b0d";

/// Persisted per-source resume cursor. A source is one log file, and a
/// rotated log file never gains a byte again, which is what makes a byte
/// offset a safe place to resume from.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SourceCursor {
    v: String,
    offset: u64,
    probe: String,
}

/// One validated `usage` line, in the export's own vocabulary.
///
/// This type exists so the distinction the export draws survives as far as
/// this repository's own boundary. [`ParsedCall::cost_usd`] is a plain `f64`
/// and cannot express "unpriced"; `Option<f64>` here can, and the
/// conversion's loss is stated once rather than discovered later.
#[derive(Debug, Clone, PartialEq)]
pub struct SkylarkRecord {
    pub dedup_key: String,
    pub backend: String,
    /// The model identifier the request named. For an auto-routed call this
    /// is the alias, such as `auto`.
    pub model: String,
    /// The model the response named as having served the call (Skylark's
    /// additive `servedModel`, Phase 46 Week 106d Day 1). `None` when the line
    /// predates the field or no response named a model.
    pub served_model: Option<String>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub token_quality: TokenQuality,
    pub latency_ms: u64,
    /// `None` when Skylark could not honestly price the call: an unpriced
    /// model, or counts it refuses to multiply by a non-zero rate. Distinct
    /// from `Some(0.0)`, which is a recorded rate — Skylark's self-hosted
    /// lanes bill nothing per token and say so.
    pub cost_usd: Option<f64>,
    pub pricing_version: Option<String>,
    pub timestamp: Option<DateTime<Utc>>,
    /// The governed run this call belongs to, falling back to the emitting
    /// site when the call was not scoped to a run. This is the attribution an
    /// external scraper reading each backend's own counters can never
    /// reconstruct, and the reason the export exists at all.
    pub session_id: String,
}

/// What one export file yielded.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ExportParse {
    pub records: Vec<SkylarkRecord>,
    /// Rows the daemon recorded as lost, summed across every gap marker in
    /// the file. Surfaced rather than swallowed: totals over a file with a
    /// non-zero gap are a floor, not a figure.
    pub lagged_rows: u64,
    /// Lines that did not parse or did not satisfy the contract. A truncated
    /// final line contributes one.
    pub skipped_lines: usize,
}

/// FNV-1a 64 as lowercase hex, matching the producing repository's helper.
pub fn fnv1a64_hex(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The record's identity, derived exactly as the producer derives it.
///
/// Kept as a function rather than read only off the wire so that a producer
/// writing a key some other way is caught here, by a test, rather than in
/// someone's monthly total.
pub fn dedup_key(trace_id: &str, sequence: u64) -> String {
    format!("skylark:{trace_id}:{sequence}")
}

fn as_opt_u64(value: Option<&serde_json::Value>) -> Option<Option<u64>> {
    let value = value?;
    if value.is_null() {
        return Some(None);
    }
    value.as_u64().map(Some)
}

fn as_opt_str(value: Option<&serde_json::Value>) -> Option<Option<&str>> {
    let value = value?;
    if value.is_null() {
        return Some(None);
    }
    value.as_str().map(Some)
}

/// Whether one line is a gap marker, a usage record, or unreadable.
enum Line {
    Usage(Box<SkylarkRecord>),
    Gap(u64),
    Unreadable,
}

fn parse_line(line: &str) -> Line {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return Line::Unreadable;
    };
    // A line from a contract this adapter was not written against is skipped
    // rather than guessed at: a renamed field read as its old meaning is a
    // wrong number, and a wrong number is worse than a missing one.
    if value.get("contractVersion").and_then(|v| v.as_str())
        != Some(config::EXPORT_CONTRACT_VERSION)
    {
        return Line::Unreadable;
    }

    match value.get("record").and_then(|v| v.as_str()) {
        Some("gap") => match value.get("lagged").and_then(|v| v.as_u64()) {
            Some(lagged) => Line::Gap(lagged),
            None => Line::Unreadable,
        },
        Some("usage") => parse_usage(&value).map_or(Line::Unreadable, |r| Line::Usage(Box::new(r))),
        _ => Line::Unreadable,
    }
}

fn parse_usage(value: &serde_json::Value) -> Option<SkylarkRecord> {
    let stated_key = value.get("dedupKey")?.as_str()?;
    let event = value.get("event")?.as_object()?;
    if event.get("event_type")?.as_str()? != "model.usage" {
        return None;
    }
    let trace_id = event.get("trace_id")?.as_str()?;
    let sequence = event.get("sequence")?.as_u64()?;
    // The producer writes the key; this adapter derives it too and refuses a
    // record where the two disagree. A key computed a slightly different way
    // on either side is the double-count this field exists to prevent, and it
    // is invisible downstream — it looks like usage.
    if stated_key != dedup_key(trace_id, sequence) {
        return None;
    }

    let timestamp_utc = event.get("timestamp_utc")?.as_u64()?;
    let run_id = event.get("run_id")?.as_str()?;
    let emitter = event.get("session_id")?.as_str()?;
    let payload = event.get("payload")?.as_object()?;

    let backend = payload.get("backend")?.as_str()?.to_string();
    if !config::COVERED_BACKENDS.contains(&backend.as_str()) {
        return None;
    }
    let token_quality = match payload.get("tokenQuality")?.as_str()? {
        "exact" => TokenQuality::Exact,
        "estimated" => TokenQuality::Estimated,
        // The export's `absent` means the backend reported no complete
        // accounting — the Neural Processing Unit lane structurally cannot,
        // and Copilot's software development kit has no input-token field.
        // This repository's nearest honest value is `Unknown`; there is no
        // `Absent`, and mapping it to `Estimated` would claim a derivation
        // nobody performed.
        "absent" => TokenQuality::Unknown,
        _ => return None,
    };

    let cost_usd = match as_opt_str(payload.get("costUsd"))? {
        // A fixed-point decimal string, parsed here rather than upstream so
        // the producer never has to serialise money as a JSON number. The
        // `f64` is this repository's own representation and its own
        // rounding; the exact figure stays on the record Skylark wrote.
        Some(raw) => {
            let parsed = raw.trim().parse::<f64>().ok()?;
            // The producer's central integrity rule, mirrored rather than
            // trusted. A row whose counts are not `exact` may carry a
            // recorded zero — zero multiplied by an unknown count is still
            // zero, which is what the self-hosted lanes legitimately cost —
            // and may never carry a figure. Skylark's own validator refuses
            // the pairing; a consumer more forgiving on *this* rule is a
            // consumer that puts an estimate in a spend column.
            if token_quality != TokenQuality::Exact && parsed != 0.0 {
                return None;
            }
            Some(parsed)
        }
        None => None,
    };
    let pricing_version = as_opt_str(payload.get("pricingVersion"))?.map(str::to_string);
    // Additive: an absent field reads as `None`, like a null, and a present
    // value that is neither a string nor null makes the line unreadable.
    let served_model = match payload.get("servedModel") {
        None => None,
        present => as_opt_str(present)?.map(str::to_string),
    };
    // `pricingVersion` is null exactly when `costUsd` is. A figure without
    // the table that produced it asserts a provenance it does not have.
    if cost_usd.is_none() != pricing_version.is_none() {
        return None;
    }

    Some(SkylarkRecord {
        dedup_key: stated_key.to_string(),
        backend,
        model: payload.get("model")?.as_str()?.to_string(),
        served_model,
        prompt_tokens: as_opt_u64(payload.get("promptTokens"))?,
        completion_tokens: as_opt_u64(payload.get("completionTokens"))?,
        token_quality,
        latency_ms: payload.get("latencyMs")?.as_u64()?,
        cost_usd,
        pricing_version,
        timestamp: Utc
            .timestamp_opt(i64::try_from(timestamp_utc).ok()?, 0)
            .single(),
        session_id: if run_id.is_empty() {
            emitter.to_string()
        } else {
            run_id.to_string()
        },
    })
}

/// Every record in one export's text.
pub fn parse_export(text: &str) -> ExportParse {
    let mut parse = ExportParse::default();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match parse_line(line) {
            Line::Usage(record) => parse.records.push(*record),
            Line::Gap(lagged) => parse.lagged_rows = parse.lagged_rows.saturating_add(lagged),
            Line::Unreadable => parse.skipped_lines += 1,
        }
    }
    parse
}

/// One [`SkylarkRecord`] as this repository's own call row.
///
/// The one lossy step in the whole path, stated where it happens:
/// [`ParsedCall::cost_usd`] is a plain `f64`, so a record Skylark could not
/// price contributes `0.0` to a spend total. That is the right *total* — an
/// unpriceable call adds no money to it — but it is not the right *fact*, and
/// this adapter does not repair it by re-pricing the call from this
/// repository's own pricing book. Skylark declined to put a number on that
/// call for a stated reason, and inventing one here would put a fabrication
/// into a column presented as money.
pub fn to_parsed_call(record: &SkylarkRecord, project: &str) -> ParsedCall {
    ParsedCall {
        tool: config::TOOL_ID,
        // The model that did the work when Skylark knows it; an auto-routed
        // call's alias names no model and would group every routed call
        // together under `auto`.
        model: record
            .served_model
            .clone()
            .unwrap_or_else(|| record.model.clone()),
        input_tokens: record.prompt_tokens.unwrap_or(0),
        output_tokens: record.completion_tokens.unwrap_or(0),
        cost_usd: record.cost_usd.unwrap_or(0.0),
        timestamp: record.timestamp,
        speed: Speed::Standard,
        dedup_key: record.dedup_key.clone(),
        session_id: record.session_id.clone(),
        project: project.to_string(),
        elapsed_ms: Some(record.latency_ms),
        interaction_mode: InteractionMode::Agent,
        token_quality: record.token_quality,
        // The export carries the daemon's own emission timestamp on every
        // record; nothing here is inferred from a file's mtime or a session
        // boundary.
        timestamp_quality: TimestampQuality::Exact,
        ..ParsedCall::default()
    }
}

/// Full parse of one log file.
pub fn parse_session(
    source: &SessionSource,
    seen: &mut HashSet<String>,
) -> Result<Vec<ParsedCall>> {
    Ok(parse_session_with_cursor(source, seen, None)?.calls)
}

/// Incremental parse, resuming past `cursor`'s byte offset when it still
/// matches the file on disk.
///
/// Safe because the log is append-only: the daemon writes whole lines and
/// rotates to a new file rather than rewriting one. The probe hash covers the
/// case it cannot promise — a file replaced by a copy or a restore, which can
/// be at least as long as it was and hold different bytes where the resume
/// would land. The probe covers [`PROBE_BYTES`] immediately *before* the
/// offset, not the whole file, so a rewrite confined to a distant prefix goes
/// undetected; that is the same bound every cursor in this repository carries,
/// and it is not reachable for an append-only log that rotates rather than
/// compacts.
pub fn parse_session_with_cursor(
    source: &SessionSource,
    seen: &mut HashSet<String>,
    cursor: Option<&str>,
) -> Result<AdapterParse> {
    let path = &source.path;
    let prior = cursor
        .and_then(|raw| serde_json::from_str::<SourceCursor>(raw).ok())
        .filter(|c| c.v == PARSE_VERSION)
        .filter(|c| {
            c.offset > 0
                && fs::metadata(path)
                    .map(|m| c.offset <= m.len())
                    .unwrap_or(false)
                && probe_hash(path, c.offset).as_deref() == Some(c.probe.as_str())
        });
    let start = prior.as_ref().map(|c| c.offset).unwrap_or(0);
    let resumed_files = usize::from(start > 0);

    let Ok(file) = fs::File::open(path) else {
        return Ok(AdapterParse::default());
    };
    let mut reader = BufReader::new(file);
    if start > 0 && reader.seek(SeekFrom::Start(start)).is_err() {
        return Ok(AdapterParse::default());
    }

    let mut offset = start;
    let mut calls = Vec::new();
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        let read = match reader.read_until(b'\n', &mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => break,
        };
        // A final line with no newline is the half-written object a daemon
        // killed mid-write leaves behind. It is not parsed, and — crucially —
        // the cursor is not advanced past it, so the next sync re-reads it
        // once the daemon has finished writing it.
        if buffer.last() != Some(&b'\n') {
            break;
        }
        offset += read as u64;
        let line = String::from_utf8_lossy(&buffer);
        let line = line.trim_end_matches(['\n', '\r']);
        if line.trim().is_empty() {
            continue;
        }
        if let Line::Usage(record) = parse_line(line) {
            if !seen.insert(record.dedup_key.clone()) {
                continue;
            }
            calls.push(to_parsed_call(&record, &source.project));
        }
    }

    let cursor = probe_hash(path, offset).map(|probe| SourceCursor {
        v: PARSE_VERSION.to_string(),
        offset,
        probe,
    });
    Ok(AdapterParse {
        calls,
        cursor: cursor.and_then(|c| serde_json::to_string(&c).ok()),
        resumed_files,
    })
}

fn probe_hash(path: &Path, offset: u64) -> Option<String> {
    if offset == 0 {
        return Some(fnv1a64_hex(&[]));
    }
    let mut file = fs::File::open(path).ok()?;
    let take = offset.min(PROBE_BYTES);
    file.seek(SeekFrom::Start(offset - take)).ok()?;
    let mut buf = vec![0u8; take as usize];
    file.read_exact(&mut buf).ok()?;
    Some(fnv1a64_hex(&buf))
}

/// Totals a caller can assert against, used by this adapter's own conformance
/// test and by `--list-projects`-style summaries.
pub fn totals(records: &[SkylarkRecord]) -> BTreeMap<&'static str, u64> {
    let mut out = BTreeMap::new();
    out.insert("calls", records.len() as u64);
    out.insert(
        "input_tokens",
        records.iter().filter_map(|r| r.prompt_tokens).sum(),
    );
    out.insert(
        "output_tokens",
        records.iter().filter_map(|r| r.completion_tokens).sum(),
    );
    out.insert(
        "priced",
        records.iter().filter(|r| r.cost_usd.is_some()).count() as u64,
    );
    out.insert(
        "unpriced",
        records.iter().filter(|r| r.cost_usd.is_none()).count() as u64,
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tokenuse-skylark-parser-{}",
            crate::tools::paths::test_run_id()
        ));
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn fixture_source(dir: &Path, text: &str) -> SessionSource {
        let path = dir.join("usage-000001.jsonl");
        let mut f = fs::File::create(&path).expect("create");
        f.write_all(text.as_bytes()).expect("write");
        SessionSource::session(path, config::DISPLAY_NAME, config::TOOL_ID)
    }

    /// The fixture is the contract. If a byte of it moves without the
    /// producing repository's identical constant moving too, the two
    /// repositories are testing different contracts and neither would say so.
    #[test]
    fn the_conformance_fixture_matches_its_pinned_digest() {
        assert_eq!(
            fnv1a64_hex(CONFORMANCE_FIXTURE.as_bytes()),
            CONFORMANCE_FIXTURE_DIGEST,
            "this copy of the Skylark conformance fixture drifted from the one the producing \
             repository pins"
        );
    }

    /// The producing repository's committed copy, when this checkout can see it.
    ///
    /// Resolved from this crate's own manifest directory, never a hard-coded
    /// home path, so the guard works wherever the two repositories are checked
    /// out side by side. The override exists for a lane that keeps them apart.
    fn producers_fixture_path() -> Option<PathBuf> {
        if let Ok(path) = std::env::var("SKYLARK_CONFORMANCE_FIXTURE") {
            let path = PathBuf::from(path);
            // A set-but-missing override is a misconfiguration, not an absent
            // sibling. Skipping it would let a typo downgrade this guard to the
            // in-repo digest check with a green suite.
            assert!(
                path.is_file(),
                "SKYLARK_CONFORMANCE_FIXTURE is set to {} but no file is there; unset it to fall \
                 back to the sibling checkout, or point it at the producer's fixture",
                path.display()
            );
            return Some(path);
        }
        let sibling = Path::new(env!("CARGO_MANIFEST_DIR")).join(
            "../skylark/core/crates/skylark-daemon/src/usage/fixtures/usage_export_v1.jsonl",
        );
        sibling.is_file().then_some(sibling)
    }

    /// The two repositories' copies are the same file, or one of them is
    /// testing a contract the other does not have.
    ///
    /// The pinned digest above catches a fixture that moved *and* was re-pinned
    /// inside the same repository. It cannot catch the failure that actually
    /// happened twice: the producing repository moves, re-pins, and the consumer
    /// is never touched, so both suites stay green over two different contracts.
    /// This test reads the producer's committed copy when a sibling checkout is
    /// present and compares the bytes, so a move on either side is loud here
    /// before the consumer re-pins.
    ///
    /// It skips cleanly when the sibling checkout is absent, which is its
    /// residual bound: a machine with only this repository gets the digest check
    /// and no cross-check.
    #[test]
    fn the_conformance_fixture_is_byte_identical_to_the_producers_copy() {
        let Some(path) = producers_fixture_path() else {
            eprintln!(
                "skylark sibling checkout not found — cross-repository byte check skipped"
            );
            return;
        };
        let theirs = fs::read(&path).expect("the sibling fixture is readable");
        assert_eq!(
            fnv1a64_hex(&theirs),
            CONFORMANCE_FIXTURE_DIGEST,
            "the producing repository's copy at {} hashes differently from the digest this \
             repository pins; the two repositories are testing different contracts",
            path.display()
        );
        assert_eq!(
            theirs,
            CONFORMANCE_FIXTURE.as_bytes(),
            "this repository's conformance fixture is not byte-identical to the producer's at {}",
            path.display()
        );
    }

    /// The gate: ingest the committed fixture and report the expected totals.
    #[test]
    fn the_conformance_fixture_ingests_with_the_expected_totals() {
        let parse = parse_export(CONFORMANCE_FIXTURE);
        assert_eq!(
            parse.skipped_lines, 0,
            "every fixture line must be readable"
        );

        let totals = totals(&parse.records);
        assert_eq!(totals["calls"], 6);
        assert_eq!(totals["input_tokens"], 2046);
        assert_eq!(totals["output_tokens"], 1189);
        assert_eq!(totals["priced"], 4);
        assert_eq!(totals["unpriced"], 2);

        // Every backend class the export claims to cover.
        for backend in config::COVERED_BACKENDS {
            assert!(
                parse.records.iter().any(|r| r.backend == *backend),
                "the fixture exercises `{backend}`, so this adapter must read it"
            );
        }

        // All three of the export's token qualities, through this
        // repository's own vocabulary.
        for quality in [
            TokenQuality::Exact,
            TokenQuality::Estimated,
            TokenQuality::Unknown,
        ] {
            assert!(
                parse.records.iter().any(|r| r.token_quality == quality),
                "the fixture exercises {quality:?}"
            );
        }
    }

    /// A null cost is a known unknown. It must not be re-priced from this
    /// repository's pricing book, and it must not be reported as a free call.
    #[test]
    fn a_null_cost_record_is_not_counted_as_zero() {
        let parse = parse_export(CONFORMANCE_FIXTURE);

        let unpriced: Vec<&SkylarkRecord> = parse
            .records
            .iter()
            .filter(|r| r.cost_usd.is_none())
            .collect();
        assert_eq!(
            unpriced.len(),
            2,
            "the fixture carries two unpriceable calls"
        );
        assert!(
            unpriced.iter().any(|r| r.prompt_tokens.is_some()),
            "an unpriceable call keeps its tokens: not priced is not not used"
        );
        for row in &unpriced {
            assert!(
                row.pricing_version.is_none(),
                "no table produced a figure, so no table is named"
            );
        }

        // ... and the recorded zeros are a different fact, not the same one.
        let real_zeros: Vec<&SkylarkRecord> = parse
            .records
            .iter()
            .filter(|r| r.cost_usd == Some(0.0))
            .collect();
        assert_eq!(
            real_zeros.len(),
            3,
            "three self-hosted lanes bill nothing per token"
        );
        for row in &real_zeros {
            assert!(
                row.pricing_version.is_some(),
                "a zero is a recorded rate, so it names the table that recorded it"
            );
        }

        // The spend total is the priced rows and nothing else.
        let total: f64 = parse.records.iter().filter_map(|r| r.cost_usd).sum();
        assert!(
            (total - 0.0087).abs() < 1e-9,
            "only the one priced non-zero call contributes money; got {total}"
        );
    }

    /// The assertion that would have caught the Copilot double-count, and the
    /// reason both repositories test the same fixture.
    #[test]
    fn the_dedup_derivation_matches_the_fixtures_pinned_keys() {
        let parse = parse_export(CONFORMANCE_FIXTURE);
        assert!(!parse.records.is_empty());
        for record in &parse.records {
            let (trace_id, sequence) = record
                .dedup_key
                .strip_prefix("skylark:")
                .and_then(|rest| rest.rsplit_once(':'))
                .expect("the key is `skylark:<trace_id>:<sequence>`");
            let sequence: u64 = sequence.parse().expect("the sequence is a number");
            assert_eq!(
                record.dedup_key,
                dedup_key(trace_id, sequence),
                "this adapter's derivation must be the producer's, byte for byte"
            );
        }

        // A record whose stated key is not the derived one is refused rather
        // than trusted: that is the shape of an independent implementation
        // computing the key a slightly different way.
        let mut value: serde_json::Value = serde_json::from_str(
            CONFORMANCE_FIXTURE
                .lines()
                .find(|l| l.contains("\"record\":\"usage\""))
                .expect("a usage line"),
        )
        .expect("JSON");
        value["dedupKey"] = serde_json::Value::from("skylark:something-else:1");
        let tampered = serde_json::to_string(&value).expect("re-serialises");
        assert_eq!(parse_export(&tampered).skipped_lines, 1);
    }

    /// A gap marker is data. Turning "we stopped listening" into "nothing
    /// happened" is the one inference the marker exists to prevent.
    #[test]
    fn a_gap_marker_is_surfaced_rather_than_swallowed() {
        let parse = parse_export(CONFORMANCE_FIXTURE);
        assert_eq!(
            parse.lagged_rows, 47,
            "the fixture's three gap markers record 44 rows lost to broadcast lag, 2 to failed \
             appends and 1 to the shutdown drain -- a cause this adapter reads past rather than \
             refusing the line over"
        );
        assert_eq!(
            parse.skipped_lines, 0,
            "a gap line is readable, not unreadable"
        );
    }

    /// A daemon killed mid-write leaves a half-object with no newline, and
    /// one bad line must never cost the file's history.
    #[test]
    fn a_truncated_final_line_costs_only_itself() {
        let mut text = CONFORMANCE_FIXTURE.to_string();
        text.push_str("{\"record\":\"usage\",\"contractVer");
        let parse = parse_export(&text);
        assert_eq!(parse.records.len(), 6, "every intact record still reads");
        assert_eq!(parse.skipped_lines, 1);
        assert_eq!(parse.lagged_rows, 47);

        // ... and the file parser stops before it, so the cursor does not
        // advance past a line that is not finished being written.
        let dir = scratch();
        let source = fixture_source(&dir, &text);
        let mut seen = HashSet::new();
        let parsed =
            parse_session_with_cursor(&source, &mut seen, None).expect("a truncated tail parses");
        assert_eq!(parsed.calls.len(), 6);
        let cursor: SourceCursor =
            serde_json::from_str(&parsed.cursor.expect("a cursor")).expect("cursor JSON");
        assert_eq!(
            cursor.offset,
            CONFORMANCE_FIXTURE.len() as u64,
            "the cursor stops at the last newline-terminated line"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// A line from a contract this adapter was not written against is skipped
    /// rather than read with its old meaning.
    #[test]
    fn an_unknown_contract_version_is_skipped_not_guessed_at() {
        let line = CONFORMANCE_FIXTURE
            .lines()
            .find(|l| l.contains("\"record\":\"usage\""))
            .expect("a usage line")
            .replace(config::EXPORT_CONTRACT_VERSION, "skylark-model-usage-v99");
        let parse = parse_export(&line);
        assert!(parse.records.is_empty());
        assert_eq!(parse.skipped_lines, 1);
    }

    /// The log is append-only, which is what makes resuming by byte offset
    /// safe. This is the property that keeps a 64 MiB export from being
    /// re-read every fifteen minutes.
    #[test]
    fn a_resumed_parse_reads_only_what_was_appended() {
        let dir = scratch();
        let first_three: String = CONFORMANCE_FIXTURE
            .lines()
            .take(3)
            .map(|l| format!("{l}\n"))
            .collect();
        let source = fixture_source(&dir, &first_three);

        let mut seen = HashSet::new();
        let first = parse_session_with_cursor(&source, &mut seen, None).expect("first parse");
        assert_eq!(first.calls.len(), 3);
        assert_eq!(first.resumed_files, 0);
        let cursor = first.cursor.expect("a cursor");

        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&source.path)
            .expect("append");
        f.write_all(
            CONFORMANCE_FIXTURE[first_three.len()..]
                .to_string()
                .as_bytes(),
        )
        .expect("write the rest");
        drop(f);

        let mut seen = HashSet::new();
        let second =
            parse_session_with_cursor(&source, &mut seen, Some(&cursor)).expect("second parse");
        assert_eq!(second.resumed_files, 1);
        assert_eq!(
            second.calls.len(),
            3,
            "only the three appended usage rows, not the whole file again"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// A file replaced by a copy or a restore can be at least as long as it
    /// was and hold different bytes where the resume would land. The probe
    /// catches that and the parse falls back to reading the file whole.
    ///
    /// The rewrite here is in the file's last line on purpose: the probe
    /// covers `PROBE_BYTES` immediately before the offset, which for a fully
    /// consumed file is its tail. A rewrite confined to a distant prefix is
    /// outside what this cursor promises to detect — see
    /// `parse_session_with_cursor`.
    #[test]
    fn a_rewritten_tail_invalidates_the_cursor() {
        let dir = scratch();
        let source = fixture_source(&dir, CONFORMANCE_FIXTURE);
        let mut seen = HashSet::new();
        let first = parse_session_with_cursor(&source, &mut seen, None).expect("first parse");
        let cursor = first.cursor.expect("a cursor");

        let rewritten = CONFORMANCE_FIXTURE.replace(
            r#""lagged":1,"detectedAtUtc":1789776360"#,
            r#""lagged":7,"detectedAtUtc":1789776360"#,
        );
        assert_eq!(
            rewritten.len(),
            CONFORMANCE_FIXTURE.len(),
            "same length, different bytes"
        );
        assert_ne!(rewritten, CONFORMANCE_FIXTURE);
        fs::write(&source.path, &rewritten).expect("rewrite");

        let mut seen = HashSet::new();
        let second =
            parse_session_with_cursor(&source, &mut seen, Some(&cursor)).expect("second parse");
        assert_eq!(
            second.resumed_files, 0,
            "the probe must reject the stale cursor"
        );
        assert_eq!(second.calls.len(), 6, "and the file is read whole");
        fs::remove_dir_all(&dir).ok();
    }

    /// The producer's key is the dedup key, so a record seen twice — a
    /// replayed delivery, an overlapping sync — is one call.
    #[test]
    fn a_record_seen_twice_is_counted_once() {
        let dir = scratch();
        let doubled = format!("{CONFORMANCE_FIXTURE}{CONFORMANCE_FIXTURE}");
        let source = fixture_source(&dir, &doubled);
        let mut seen = HashSet::new();
        let calls = parse_session(&source, &mut seen).expect("parse");
        assert_eq!(calls.len(), 6, "twelve lines, six calls");
        fs::remove_dir_all(&dir).ok();
    }

    /// The correlation an external scraper reading each backend's own
    /// counters can never reconstruct.
    #[test]
    fn a_run_scoped_record_is_attributed_to_its_run() {
        let parse = parse_export(CONFORMANCE_FIXTURE);
        let vsr = parse
            .records
            .iter()
            .find(|r| r.backend == "vsr")
            .expect("the fixture carries a Semantic Router call");
        assert_eq!(vsr.session_id, "run-105");

        let copilot = parse
            .records
            .iter()
            .find(|r| r.model == "github-copilot")
            .expect("the fixture carries a Copilot call");
        assert_eq!(
            copilot.session_id, "escalation_rpc",
            "a call not scoped to a run falls back to the emitting site rather than to an \
             empty string"
        );
    }

    /// Skylark's Phase 46 Week 106d Day 1: an auto-routed call names the
    /// alias it was sent as in `model` and the model that answered it in
    /// `servedModel`. The call row is attributed to the model that did the
    /// work, and the alias is kept on the record.
    #[test]
    fn an_auto_routed_record_is_attributed_to_the_model_that_served_it() {
        let parse = parse_export(CONFORMANCE_FIXTURE);
        let routed = parse
            .records
            .iter()
            .find(|r| r.model == "auto")
            .expect("the fixture's auto-routed row");
        assert_eq!(
            routed.served_model.as_deref(),
            Some("QuantTrio/Qwen3-Coder-30B-A3B-Instruct-AWQ")
        );
        let call = to_parsed_call(routed, config::DISPLAY_NAME);
        assert_eq!(call.model, "QuantTrio/Qwen3-Coder-30B-A3B-Instruct-AWQ");

        let unrouted = parse
            .records
            .iter()
            .find(|r| r.served_model.is_none())
            .expect("a row no response named a model for");
        assert_eq!(
            to_parsed_call(unrouted, config::DISPLAY_NAME).model,
            unrouted.model,
            "with no served model the requested one stands"
        );
    }

    /// `servedModel` is additive: a line from before it existed still reads,
    /// and a present value that is not a string or null is not a model.
    #[test]
    fn a_served_model_is_optional_and_typed() {
        let line = CONFORMANCE_FIXTURE
            .lines()
            .find(|l| l.contains("\"model\":\"auto\""))
            .expect("the auto-routed line");
        let mut value: serde_json::Value = serde_json::from_str(line).expect("JSON");
        value["event"]["payload"]
            .as_object_mut()
            .expect("payload")
            .remove("servedModel");
        let legacy = parse_export(&serde_json::to_string(&value).expect("JSON"));
        assert_eq!(legacy.skipped_lines, 0);
        assert_eq!(legacy.records[0].served_model, None);

        value["event"]["payload"]["servedModel"] = serde_json::Value::from(7);
        let wrong = parse_export(&serde_json::to_string(&value).expect("JSON"));
        assert_eq!(wrong.skipped_lines, 1, "a numeric servedModel is not a model");
    }

    #[test]
    fn a_record_converts_to_a_call_row_without_inventing_a_cost() {
        let parse = parse_export(CONFORMANCE_FIXTURE);
        let unpriced = parse
            .records
            .iter()
            .find(|r| r.cost_usd.is_none() && r.prompt_tokens.is_some())
            .expect("an unpriceable call with tokens");
        let call = to_parsed_call(unpriced, config::DISPLAY_NAME);
        assert_eq!(call.tool, config::TOOL_ID);
        assert_eq!(
            call.cost_usd, 0.0,
            "an unpriceable call adds no money to a spend total"
        );
        assert_eq!(call.input_tokens, unpriced.prompt_tokens.unwrap_or(0));
        assert_eq!(
            call.dedup_key, unpriced.dedup_key,
            "the producer's key, verbatim"
        );
        assert_eq!(call.timestamp_quality, TimestampQuality::Exact);
    }

    #[test]
    fn a_line_that_is_not_json_at_all_costs_only_itself() {
        let text = format!("not json\n{CONFORMANCE_FIXTURE}");
        let parse = parse_export(&text);
        assert_eq!(parse.records.len(), 6);
        assert_eq!(parse.skipped_lines, 1);
    }

    #[test]
    fn a_cost_without_a_pricing_version_is_refused() {
        let mut value: serde_json::Value = serde_json::from_str(
            CONFORMANCE_FIXTURE
                .lines()
                .find(|l| l.contains("\"record\":\"usage\""))
                .expect("a usage line"),
        )
        .expect("JSON");
        value["event"]["payload"]["pricingVersion"] = serde_json::Value::Null;
        let tampered = serde_json::to_string(&value).expect("re-serialises");
        assert_eq!(
            parse_export(&tampered).skipped_lines,
            1,
            "a figure without the table that produced it asserts a provenance it does not have"
        );
    }

    /// The producing repository's central integrity rule, mirrored here.
    ///
    /// Skylark's own validator refuses a row whose `tokenQuality` is not
    /// `exact` and whose `costUsd` is a figure rather than a recorded zero,
    /// because zero multiplied by an unknown count is still zero and anything
    /// else is an estimate becoming money. This adapter has to refuse it too:
    /// a consumer more forgiving than the contract on *this* rule is a
    /// consumer that puts the estimate in a spend column, which is precisely
    /// the outcome the contract exists to prevent.
    #[test]
    fn a_non_exact_row_carrying_a_figure_is_refused_while_its_recorded_zero_is_kept() {
        let zero_line = CONFORMANCE_FIXTURE
            .lines()
            .find(|l| {
                l.contains("\"tokenQuality\":\"absent\"") && l.contains("\"costUsd\":\"0.000000\"")
            })
            .expect("the fixture carries a non-exact row priced at a recorded zero");
        let parsed = parse_export(zero_line);
        assert_eq!(
            parsed.skipped_lines, 0,
            "a recorded zero is a rate that actually applied"
        );
        assert_eq!(parsed.records[0].cost_usd, Some(0.0));

        let mut value: serde_json::Value = serde_json::from_str(zero_line).expect("JSON");
        value["event"]["payload"]["costUsd"] = serde_json::Value::from("99.000000");
        let tampered = serde_json::to_string(&value).expect("re-serialises");
        let parsed = parse_export(&tampered);
        assert_eq!(
            parsed.skipped_lines, 1,
            "a count nobody reported cannot be multiplied into money, so a figure beside one \
             is not a record this adapter may total"
        );
        assert!(parsed.records.is_empty());

        value["event"]["payload"]["tokenQuality"] = serde_json::Value::from("estimated");
        let tampered = serde_json::to_string(&value).expect("re-serialises");
        assert_eq!(
            parse_export(&tampered).skipped_lines,
            1,
            "an estimate least of all"
        );
    }

    /// The per-model fixture, shared with the producing repository (Skylark Phase
    /// 46 Week 106f Day 4). Skylark's usage card and this adapter attribute each
    /// call to the same key, `servedModel` falling back to `model`, and each side
    /// asserts the same per-model keys and counts against this file.
    const MODELS_FIXTURE: &str = include_str!("fixtures/usage_models_v1.jsonl");

    /// FNV-1a 64 over [`MODELS_FIXTURE`], pinned identically in the producer.
    const MODELS_FIXTURE_DIGEST: &str = "c30b42e9bc752f9b";

    /// The per-model breakdown the producer's usage card gives for the same
    /// fixture (`usage::card::tests::MODELS_FIXTURE_EXPECTED` in Skylark):
    /// key, calls, input tokens, output tokens, in key order.
    const MODELS_FIXTURE_EXPECTED: [(&str, u64, u64, u64); 4] = [
        ("QuantTrio/Qwen3-Coder-30B-A3B-Instruct-AWQ", 2, 141, 17),
        ("auto", 1, 40, 5),
        ("claude-haiku-4-5-20251001", 1, 300, 60),
        ("claude-sonnet-4-6", 1, 1200, 340),
    ];

    #[test]
    fn the_shared_model_fixture_matches_its_pinned_digest() {
        assert_eq!(fnv1a64_hex(MODELS_FIXTURE.as_bytes()), MODELS_FIXTURE_DIGEST);
    }

    /// One grouping key on both sides of the repository boundary: the model
    /// that served the call, falling back to the one requested. A routed call
    /// with no served model stays under its alias, and a dated id is its own
    /// key rather than being folded into its alias.
    #[test]
    fn the_shared_model_fixture_groups_per_model_as_the_producers_card_does() {
        let parse = parse_export(MODELS_FIXTURE);
        assert_eq!(parse.skipped_lines, 0, "every fixture line must be readable");
        let mut groups: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new();
        for record in &parse.records {
            let call = to_parsed_call(record, config::DISPLAY_NAME);
            let entry = groups.entry(call.model).or_default();
            entry.0 += 1;
            entry.1 += call.input_tokens;
            entry.2 += call.output_tokens;
        }
        let actual: Vec<(&str, u64, u64, u64)> = groups
            .iter()
            .map(|(key, (calls, input, output))| (key.as_str(), *calls, *input, *output))
            .collect();
        assert_eq!(actual, MODELS_FIXTURE_EXPECTED.to_vec());
    }

    /// The producer's committed copy, byte for byte, when a sibling checkout
    /// is present. Skips cleanly without one, like the conformance check.
    #[test]
    fn the_shared_model_fixture_is_byte_identical_to_the_producers_copy() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../skylark");
        if !root.is_dir() {
            eprintln!("skylark sibling checkout not found — cross-repository byte check skipped");
            return;
        }
        let path = root.join("core/crates/skylark-daemon/src/usage/fixtures/usage_models_v1.jsonl");
        let theirs = fs::read(&path)
            .unwrap_or_else(|_| panic!("skylark is checked out beside this repository but has no {}", path.display()));
        assert_eq!(theirs, MODELS_FIXTURE.as_bytes(), "the two repositories' per-model fixtures differ");
    }
}
