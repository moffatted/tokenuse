use std::fs;
use std::path::{Path, PathBuf};

use color_eyre::Result;

use crate::tools::SessionSource;

use super::config;

/// One source per log file. The daemon rotates at 8 MiB and evicts the oldest
/// file once the set passes 64 MiB, so the file set is small, bounded, and —
/// crucially for the resume cursor — append-only: a rotated file never gains
/// a byte again.
pub fn discover() -> Result<Vec<SessionSource>> {
    let Some(root) = config::usage_root() else {
        return Ok(Vec::new());
    };
    discover_in(&root)
}

pub(super) fn discover_in(root: &Path) -> Result<Vec<SessionSource>> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut files: Vec<(u32, PathBuf)> = Vec::new();
    for entry in fs::read_dir(root)?.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Some(index) = file_index(&path) {
            files.push((index, path));
        }
    }
    // Ordered by the numeric index in the name rather than by modification
    // time: a copied or restored tree has whatever mtimes the copy gave it,
    // and the index is what actually records emission order.
    files.sort_by_key(|(index, _)| *index);
    Ok(files
        .into_iter()
        .map(|(_, path)| SessionSource::session(path, config::DISPLAY_NAME, config::TOOL_ID))
        .collect())
}

fn file_index(path: &Path) -> Option<u32> {
    if path.extension().and_then(|e| e.to_str()) != Some(config::FILE_EXT) {
        return None;
    }
    path.file_stem()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix(config::FILE_PREFIX))
        .and_then(|n| n.parse::<u32>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tokenuse-skylark-discovery-{}",
            crate::tools::paths::test_run_id()
        ));
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn touch(dir: &Path, name: &str) {
        let mut f = fs::File::create(dir.join(name)).expect("create");
        writeln!(f, "{{}}").expect("write");
    }

    #[test]
    fn discovers_usage_logs_in_emission_order() {
        let dir = scratch();
        touch(&dir, "usage-000010.jsonl");
        touch(&dir, "usage-000002.jsonl");
        touch(&dir, "usage-000001.jsonl");

        let sources = discover_in(&dir).expect("discover");
        let names: Vec<String> = sources
            .iter()
            .map(|s| s.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            names,
            vec![
                "usage-000001.jsonl",
                "usage-000002.jsonl",
                "usage-000010.jsonl"
            ],
            "the numeric index records emission order; ten sorts after two"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ignores_files_that_are_not_usage_logs() {
        let dir = scratch();
        touch(&dir, "usage-000001.jsonl");
        touch(&dir, "telemetry.db");
        touch(&dir, "usage-notanumber.jsonl");
        touch(&dir, "usage-000002.json");

        assert_eq!(discover_in(&dir).expect("discover").len(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_directory_is_no_sources_rather_than_an_error() {
        let dir = scratch().join("never-created");
        assert!(discover_in(&dir)
            .expect("a missing root is not an error")
            .is_empty());
    }
}
