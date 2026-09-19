use std::collections::HashSet;

use color_eyre::Result;

use super::{fingerprint_source, AdapterParse, ParsedCall, SessionSource, ToolAdapter};

pub mod config;
pub mod discovery;
pub mod parser;

pub struct Skylark;

/// Bump when the parser learns to extract new fields so archived sessions
/// re-parse through it on the next sync.
const SOURCE_FINGERPRINT_VERSION: &str = "skylark-v1-usage-export";

impl ToolAdapter for Skylark {
    fn id(&self) -> &'static str {
        config::TOOL_ID
    }

    fn display_name(&self) -> &'static str {
        config::DISPLAY_NAME
    }

    fn discover(&self) -> Result<Vec<SessionSource>> {
        discovery::discover()
    }

    fn parse(&self, source: &SessionSource, seen: &mut HashSet<String>) -> Result<Vec<ParsedCall>> {
        parser::parse_session(source, seen)
    }

    /// The usage log is append-only and rotates rather than rewrites, so a
    /// byte offset is a safe place to resume from — which is what keeps a
    /// 64 MiB export from being re-read on every fifteen-minute refresh.
    fn parse_with_cursor(
        &self,
        source: &SessionSource,
        seen: &mut HashSet<String>,
        cursor: Option<&str>,
    ) -> Result<AdapterParse> {
        parser::parse_session_with_cursor(source, seen, cursor)
    }

    fn source_fingerprint(&self, source: &SessionSource) -> Result<String> {
        Ok(format!(
            "{SOURCE_FINGERPRINT_VERSION}:{}",
            fingerprint_source(source)?
        ))
    }

    fn probe_roots(&self) -> Vec<super::ProbeRoot> {
        config::usage_root()
            .map(|root| vec![super::ProbeRoot::new("usage", root)])
            .unwrap_or_default()
    }

    fn env_overrides(&self) -> &'static [&'static str] {
        &[config::ENV_OVERRIDE]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_version_forces_archived_sessions_through_new_parser() {
        let source = SessionSource::session(
            "/tokenuse-skylark-fingerprint-test-missing".into(),
            config::DISPLAY_NAME,
            config::TOOL_ID,
        );
        let legacy = fingerprint_source(&source).unwrap();

        assert_eq!(
            Skylark.source_fingerprint(&source).unwrap(),
            format!("{SOURCE_FINGERPRINT_VERSION}:{legacy}")
        );
    }

    /// `config::TOOL_ID` and the literal `ingest::matches_tool` compares
    /// against are stringly typed across that boundary, and this repository's
    /// contributor guide says so. This is the assertion that keeps them
    /// equal.
    #[test]
    fn the_tool_id_matches_the_ingest_filter_literal() {
        let call = ParsedCall {
            tool: config::TOOL_ID,
            ..ParsedCall::default()
        };
        assert!(crate::ingest::matches_tool(
            &call,
            crate::app::Tool::Skylark
        ));
        assert!(!crate::ingest::matches_tool(
            &call,
            crate::app::Tool::ClaudeCode
        ));
    }

    #[test]
    fn the_adapter_is_registered() {
        assert!(
            super::super::registry()
                .iter()
                .any(|a| a.id() == config::TOOL_ID),
            "an adapter that is not in the registry never runs"
        );
    }
}
