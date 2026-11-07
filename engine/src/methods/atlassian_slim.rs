//! `atlassian-slim` layer: drop the hypermedia and envelope fields Atlassian
//! REST / Rovo MCP responses repeat on every nested object.
//!
//! Fork-only method — not present upstream.
//!
//! Every Jira object Rovo returns (issue, status, issuetype, priority, project,
//! user, link type) carries its own `self` API URL, `iconUrl`, four
//! `avatarUrls`, and the issue an `expand` list; the response ends in a
//! `context` envelope of session ids and feature flags. None of it is content
//! an agent acts on (`webUrl` and `key` stay), yet it is 43.5% of the bytes:
//! measured 2026-11-07 over 1483 real Rovo JSON results from Claude Code
//! transcripts (7.72M → 4.36M chars; getJiraIssue −37.5%,
//! createJiraIssue −45.1%, searchJiraIssuesUsingJql −46.6%).
//!
//! Scope it to the Atlassian server in a rule — `self` is an ordinary field
//! name elsewhere:
//!
//! ```toml
//! [[hook.after_tool.mcp]]
//! name = "rovo-slim"
//! when = { server = "claude_ai_Atlassian_Rovo" }
//! use  = ["atlassian-slim"]
//! ```

use crate::block::{Block, ContentFormat};
use crate::method::{CostClass, Method, MethodCtx, MethodInfo, Outcome, Produces, Recovery};
use serde_json::Value;

/// Dropped at any depth.
const NOISE_KEYS: &[&str] = &["self", "iconUrl", "avatarUrls", "expand"];

/// Fields that mark a top-level `context` object as Rovo's request envelope
/// rather than data that happens to be called `context`.
const ENVELOPE_MARKERS: &[&str] = &["cloudId", "toolName", "mcpClientName", "invocationId"];

pub struct AtlassianSlim;

impl Method for AtlassianSlim {
    fn name(&self) -> &'static str {
        "atlassian-slim"
    }

    fn info(&self) -> MethodInfo {
        MethodInfo {
            cost: CostClass::Fast,
            consumes: &[ContentFormat::Json],
            produces: Produces::Format(ContentFormat::Json),
            // Drops fields outright; the original stays in the recall store.
            lossy: true,
            recovery: Recovery::Recall,
            ..MethodInfo::LOSSLESS_TRANSFORM
        }
    }

    fn apply(&self, block: &Block, _ctx: &MethodCtx) -> Outcome {
        match slim(block.text) {
            Some(out) => Outcome::Continue(out),
            None => Outcome::Skip,
        }
    }
}

/// Returns `None` when the input is not JSON or carries none of the fields,
/// so an untouched block keeps its original bytes.
fn slim(input: &str) -> Option<String> {
    let mut value: Value = serde_json::from_str(input.trim()).ok()?;
    let mut dropped = drop_envelope(&mut value);
    dropped |= drop_noise(&mut value);
    if !dropped {
        return None;
    }
    serde_json::to_string(&value).ok()
}

fn drop_envelope(value: &mut Value) -> bool {
    let Some(obj) = value.as_object_mut() else {
        return false;
    };
    let is_envelope = obj
        .get("context")
        .and_then(Value::as_object)
        .is_some_and(|ctx| ENVELOPE_MARKERS.iter().any(|k| ctx.contains_key(*k)));
    is_envelope && obj.shift_remove("context").is_some()
}

fn drop_noise(value: &mut Value) -> bool {
    match value {
        Value::Object(obj) => {
            let before = obj.len();
            obj.retain(|k, _| !NOISE_KEYS.contains(&k.as_str()));
            let mut dropped = obj.len() != before;
            for child in obj.values_mut() {
                dropped |= drop_noise(child);
            }
            dropped
        }
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |acc, item| drop_noise(item) | acc),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> MethodCtx<'static> {
        MethodCtx { source: "mcp" }
    }

    fn run(text: &str) -> Outcome {
        AtlassianSlim.apply(&crate::testutil::block(text, ContentFormat::Json), &ctx())
    }

    /// The shape of a real `createJiraIssue` result (2026-11-07), values
    /// replaced with example.com ones.
    const CREATED: &str = r#"{"issues":{"nodes":[{"expand":"renderedFields,names,schema,operations,editmeta,changelog,versionedRepresentations","id":"10001","self":"https://api.example.com/ex/jira/0000/rest/api/3/issue/10001","key":"DEV-1","fields":{"summary":"Revoke refresh tokens on logout","issuetype":{"self":"https://api.example.com/ex/jira/0000/rest/api/3/issuetype/10103","id":"10103","description":"A problem or error.","iconUrl":"https://api.example.com/ex/jira/0000/rest/api/2/universal_avatar/view/type/issuetype/avatar/10303?size=medium","name":"Bug","subtask":false,"avatarId":10303,"hierarchyLevel":0},"project":{"self":"https://api.example.com/ex/jira/0000/rest/api/3/project/10001","id":"10001","key":"DEV","name":"Development","projectTypeKey":"software","simplified":false,"avatarUrls":{"48x48":"https://api.example.com/ex/jira/0000/rest/api/3/universal_avatar/view/type/project/avatar/10400","24x24":"https://api.example.com/ex/jira/0000/rest/api/3/universal_avatar/view/type/project/avatar/10400?size=small","16x16":"https://api.example.com/ex/jira/0000/rest/api/3/universal_avatar/view/type/project/avatar/10400?size=xsmall","32x32":"https://api.example.com/ex/jira/0000/rest/api/3/universal_avatar/view/type/project/avatar/10400?size=medium"}},"description":"Logging out only clears the cookie.","assignee":null,"status":{"self":"https://api.example.com/ex/jira/0000/rest/api/3/status/10000","description":"","iconUrl":"https://api.example.com/ex/jira/0000/","name":"Backlog","id":"10000","statusCategory":{"self":"https://api.example.com/ex/jira/0000/rest/api/3/statuscategory/2","id":2,"key":"new","colorName":"blue-gray","name":"To Do"}}},"webUrl":"https://example.atlassian.net/browse/DEV-1"}]},"context":{"atlassianAccountId":"000000:00000000-0000-0000-0000-000000000000","cloudId":"0000","clientName":"claude.ai","mcpClientName":"claude-code","toolName":"createJiraIssue","endpoint":"v1-authv2:streamable-http","sessionId":"r11-00000000","invocationId":"00000000","env":"prod","featureFlags":{"mcp-app-v2-primary":false,"analytics-jira-mcp-app":true}}}"#;

    #[test]
    fn test_drops_hypermedia_and_envelope_keeps_content() {
        let Outcome::Continue(out) = run(CREATED) else {
            panic!("must continue");
        };
        for gone in [
            "\"self\"",
            "iconUrl",
            "avatarUrls",
            "\"expand\"",
            "featureFlags",
        ] {
            assert!(!out.contains(gone), "{gone} survived: {out}");
        }
        for kept in [
            "\"key\":\"DEV-1\"",
            "Revoke refresh tokens on logout",
            "\"webUrl\":\"https://example.atlassian.net/browse/DEV-1\"",
            "\"name\":\"Backlog\"",
            "\"key\":\"new\"",
            "\"assignee\":null",
            "Logging out only clears the cookie.",
        ] {
            assert!(out.contains(kept), "{kept} lost: {out}");
        }
    }

    #[test]
    fn test_preserves_key_order() {
        let Outcome::Continue(out) = run(CREATED) else {
            panic!("must continue");
        };
        let id = out.find("\"id\":\"10001\"").expect("id");
        let key = out.find("\"key\":\"DEV-1\"").expect("key");
        let fields = out.find("\"fields\"").expect("fields");
        assert!(id < key && key < fields, "{out}");
    }

    #[test]
    fn test_savings_at_least_twenty_percent() {
        let Outcome::Continue(out) = run(CREATED) else {
            panic!("must continue");
        };
        let savings = 100.0 - out.len() as f64 / CREATED.len() as f64 * 100.0;
        assert!(savings >= 20.0, "expected ≥20%, got {savings:.1}%");
    }

    #[test]
    fn test_context_without_envelope_markers_is_data() {
        let input = r#"{"context":{"note":"kept"},"name":"x"}"#;
        assert_eq!(run(input), Outcome::Skip);
    }

    #[test]
    fn test_nested_context_is_never_treated_as_envelope() {
        let input = r#"{"a":{"context":{"cloudId":"1"}}}"#;
        assert_eq!(run(input), Outcome::Skip);
    }

    #[test]
    fn test_noise_inside_arrays_is_dropped() {
        let input = r#"[{"self":"u","key":"A-1"},{"self":"u","key":"A-2"}]"#;
        assert_eq!(
            run(input),
            Outcome::Continue(r#"[{"key":"A-1"},{"key":"A-2"}]"#.to_string())
        );
    }

    #[test]
    fn test_nothing_to_drop_skips() {
        assert_eq!(run(r#"{"key":"A-1"}"#), Outcome::Skip);
    }

    #[test]
    fn test_non_json_skips() {
        assert_eq!(run("Issue DEV-1 created"), Outcome::Skip);
        assert_eq!(run(""), Outcome::Skip);
    }

    #[test]
    fn test_idempotent() {
        let Outcome::Continue(once) = run(CREATED) else {
            panic!("must continue");
        };
        assert_eq!(run(&once), Outcome::Skip);
    }
}
