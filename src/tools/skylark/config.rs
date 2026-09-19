use std::path::PathBuf;

use crate::tools::paths;

pub const TOOL_ID: &str = "skylark";
pub const DISPLAY_NAME: &str = "Skylark";

/// The Skylark daemon writes its model-usage export beside the telemetry
/// database it already keeps, at `$HOME/.skylark/usage/`.
pub const DEFAULT_DIR: &str = ".skylark";
pub const USAGE_DIR: &str = "usage";

/// Points the adapter at a different daemon data directory — a second
/// checkout, a copied tree, a fixture.
pub const ENV_OVERRIDE: &str = "SKYLARK_DIR";

/// The rotating log's file naming: `usage-000001.jsonl`.
pub const FILE_PREFIX: &str = "usage-";
pub const FILE_EXT: &str = "jsonl";

/// The export contract this adapter is written against. A line carrying any
/// other value is skipped rather than guessed at — see `parser`.
pub const EXPORT_CONTRACT_VERSION: &str = "skylark-model-usage-v1";

/// The four backend classes the export covers. Recorded here because the
/// export states its own coverage: a consumer must not have to guess whether
/// a missing backend means zero usage or no coverage.
pub const COVERED_BACKENDS: &[&str] = &["cloud_escalation", "vsr", "local_vllm", "npu"];

pub fn usage_root() -> Option<PathBuf> {
    if let Some(path) = paths::env_path(ENV_OVERRIDE) {
        return Some(path.join(USAGE_DIR));
    }
    paths::home().map(|home| home.join(DEFAULT_DIR).join(USAGE_DIR))
}
