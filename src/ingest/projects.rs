use std::{
    collections::{BTreeSet, HashMap},
    path::Path,
};

use crate::currency::CurrencyFormatter;

pub(crate) fn project_identity(raw: &str) -> String {
    let normalized = normalized_project_path(raw);
    nearest_git_root(&normalized).unwrap_or(normalized)
}

pub(crate) fn raw_project_display(raw: &str) -> String {
    normalized_project_path(raw)
}

fn normalized_project_path(raw: &str) -> String {
    let normalized = raw.trim().replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    if trimmed.is_empty() {
        "(unknown)".into()
    } else {
        trimmed.to_string()
    }
}

fn nearest_git_root(project: &str) -> Option<String> {
    let path = Path::new(project);
    if !path.is_absolute() {
        return None;
    }

    path.ancestors().find(|ancestor| is_git_root(ancestor)).map(path_to_project_string)
}

/// Whether `ancestor` is the root of a repository.
///
/// A bare `.git` entry is not enough. An empty `.git` directory is not a
/// repository, and one sitting above the log directory — `/tmp/.git` is the
/// real case this was found by — collapsed every project under it to that
/// directory's own name, so `/tmp/proj/...` reported as `tmp`. A linked
/// worktree or submodule records a `.git` *file* instead, which is a
/// repository too.
fn is_git_root(ancestor: &Path) -> bool {
    let dot_git = ancestor.join(".git");
    dot_git.is_file() || (dot_git.is_dir() && dot_git.join("HEAD").exists())
}

pub(super) fn path_to_project_string(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    if trimmed.is_empty() {
        normalized
    } else {
        trimmed.to_string()
    }
}

pub(crate) fn project_label_lookup<I, S>(raw_projects: I) -> HashMap<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let identities: BTreeSet<String> = raw_projects
        .into_iter()
        .map(|raw| project_identity(raw.as_ref()))
        .collect();

    identities
        .iter()
        .map(|identity| {
            (
                identity.clone(),
                shortest_unique_project_label(identity, &identities),
            )
        })
        .collect()
}

pub(crate) fn project_label(labels: &HashMap<String, String>, identity: &str) -> String {
    labels.get(identity).cloned().unwrap_or_else(|| {
        shortest_unique_project_label(identity, &BTreeSet::from([identity.to_string()]))
    })
}

fn shortest_unique_project_label(identity: &str, identities: &BTreeSet<String>) -> String {
    let parts = project_parts(identity);
    if parts.is_empty() {
        return "(unknown)".into();
    }

    for suffix_len in 1..=parts.len() {
        let candidate = project_suffix(&parts, suffix_len);
        let conflicts = identities
            .iter()
            .filter(|other| other.as_str() != identity)
            .any(|other| {
                let other_parts = project_parts(other);
                other_parts.len() >= suffix_len
                    && project_suffix(&other_parts, suffix_len) == candidate
            });

        if !conflicts {
            return candidate;
        }
    }

    parts.join("/")
}

fn project_parts(identity: &str) -> Vec<&str> {
    if identity == "(unknown)" {
        return vec![identity];
    }
    identity
        .trim_start_matches('/')
        .split('/')
        .filter(|part| !part.is_empty())
        .collect()
}

fn project_suffix(parts: &[&str], suffix_len: usize) -> String {
    parts[parts.len().saturating_sub(suffix_len)..].join("/")
}

pub(super) fn format_tool_mix(
    tools: &HashMap<&'static str, f64>,
    currency: &CurrencyFormatter,
) -> String {
    let mut rows: Vec<(&'static str, f64)> =
        tools.iter().map(|(tool, cost)| (*tool, *cost)).collect();
    rows.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| tool_short_label(a.0).cmp(tool_short_label(b.0)))
    });

    if rows.is_empty() {
        return "-".into();
    }

    rows.into_iter()
        .take(3)
        .map(|(tool, cost)| {
            format!(
                "{} {}",
                tool_short_label(tool),
                currency.format_money_short(cost)
            )
        })
        .collect::<Vec<_>>()
        .join("  ")
}

pub(crate) fn tool_short_label(tool: &str) -> &'static str {
    match tool {
        "claude-code" => "Claude",
        "cursor" => "Cursor",
        "codex" => "Codex",
        "copilot" => "Copilot",
        "gemini" => "Gemini",
        "skylark" => "Skylark",
        _ => "Other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A scratch directory unique to this test, removed by its caller.
    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tokenuse-projects-{}-{name}", std::process::id()))
    }

    #[test]
    fn an_empty_dot_git_directory_is_not_a_project_root() {
        // The real case: `/tmp/.git` exists as an empty directory. Treating it
        // as a repository makes every project logged under `/tmp` report the
        // identity `/tmp`, and `project_identity` collapses the short label
        // from the project's own name to `tmp`. The scratch root here carries
        // the same empty `.git`, with the project beneath it carrying none.
        let dir = scratch("stray");
        std::fs::remove_dir_all(&dir).ok();
        let project = dir.join("proj");
        std::fs::create_dir_all(dir.join(".git")).expect("stray .git");
        std::fs::create_dir_all(&project).expect("project");

        assert_eq!(
            project_identity(&project.to_string_lossy()),
            path_to_project_string(&project),
            "an empty .git directory must not be treated as a repository root"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_dot_git_directory_holding_head_is_a_project_root() {
        let dir = scratch("real");
        std::fs::remove_dir_all(&dir).ok();
        let root = dir.join("repo");
        let nested = root.join("src").join("inner");
        std::fs::create_dir_all(&nested).expect("nested");
        std::fs::create_dir_all(root.join(".git")).expect("real .git");
        std::fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");

        assert_eq!(
            project_identity(&nested.to_string_lossy()),
            path_to_project_string(&root),
            "the nearest real repository root must still win"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_dot_git_file_is_a_project_root() {
        // A linked worktree or submodule records `.git` as a file, so a real
        // checkout must not be mistaken for the empty-directory case.
        let dir = scratch("gitfile");
        std::fs::remove_dir_all(&dir).ok();
        let root = dir.join("worktree");
        let nested = root.join("src");
        std::fs::create_dir_all(&nested).expect("nested");
        std::fs::write(root.join(".git"), "gitdir: /elsewhere/.git/worktrees/worktree\n").expect(".git file");

        assert_eq!(
            project_identity(&nested.to_string_lossy()),
            path_to_project_string(&root),
            "a .git file is a repository root"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
