//! Autodetects application Java packages for stack-trace frame classification.
//!
//! Two heuristics, merged and deduplicated:
//! 1. `pom.xml` `<groupId>` (top-level or parent fallback).
//! 2. `src/main/java/` directory walk — follow unique-child dirs to find the
//!    common package prefix (e.g. `src/main/java/com/example/app/` → `com.example.app`).

use quick_xml::events::Event;
use quick_xml::Reader;
use std::path::Path;

/// Detect application packages from `cwd`.
///
/// Returns a deduplicated list from pom.xml groupId + source directory walk.
/// Empty list means "no detection" — caller treats all frames as application.
pub fn detect(cwd: &Path) -> Vec<String> {
    let mut pkgs = Vec::new();

    if let Some(gid) = detect_from_pom(cwd) {
        pkgs.push(gid);
    }

    if let Some(src) = detect_from_sources(cwd) {
        if !pkgs.contains(&src) {
            pkgs.push(src);
        }
    }

    pkgs
}

/// Walk `src/main/java/` following unique-child directories.
/// Stops when a directory has 0 or 2+ subdirectories.
fn detect_from_sources(cwd: &Path) -> Option<String> {
    let mut dir = cwd.join("src/main/java");
    if !dir.is_dir() {
        return None;
    }
    let mut parts = Vec::new();
    loop {
        let dirs: Vec<_> = std::fs::read_dir(&dir)
            .ok()?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().ok().is_some_and(|t| t.is_dir()))
            .collect();
        if dirs.len() != 1 {
            break;
        }
        parts.push(dirs[0].file_name().to_string_lossy().into_owned());
        dir = dirs[0].path();
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("."))
    }
}

fn detect_from_pom(cwd: &Path) -> Option<String> {
    let pom_path = cwd.join("pom.xml");
    let content = std::fs::read_to_string(&pom_path).ok()?;
    extract_groupid(&content)
}

pub(crate) fn extract_groupid(xml: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    // Tag stack tracked as simple Vec<String> of local names.
    let mut stack: Vec<String> = Vec::new();
    let mut top_level_groupid: Option<String> = None;
    let mut parent_groupid: Option<String> = None;
    let mut capturing = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = std::str::from_utf8(e.name().as_ref())
                    .ok()
                    .and_then(|s| s.rsplit(':').next())
                    .unwrap_or("")
                    .to_string();
                stack.push(name);

                if is_top_level_groupid(&stack) || is_parent_groupid(&stack) {
                    capturing = true;
                }
            }
            Ok(Event::Text(t)) => {
                if capturing {
                    if let Ok(text) = t.unescape() {
                        let text = text.trim();
                        if !text.is_empty() {
                            if is_top_level_groupid(&stack) && top_level_groupid.is_none() {
                                top_level_groupid = Some(text.to_string());
                            } else if is_parent_groupid(&stack) && parent_groupid.is_none() {
                                parent_groupid = Some(text.to_string());
                            }
                        }
                    }
                }
            }
            Ok(Event::End(_)) => {
                stack.pop();
                capturing = false;
                if top_level_groupid.is_some() {
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }

    top_level_groupid.or(parent_groupid)
}

fn is_top_level_groupid(stack: &[String]) -> bool {
    stack.len() == 2 && stack[0] == "project" && stack[1] == "groupId"
}

fn is_parent_groupid(stack: &[String]) -> bool {
    stack.len() == 3
        && stack[0] == "project"
        && stack[1] == "parent"
        && stack[2] == "groupId"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_single_module_groupid() {
        let xml = include_str!("../../../tests/fixtures/java/poms/single-module-pom.xml");
        assert_eq!(extract_groupid(xml).as_deref(), Some("com.example.app"));
    }

    #[test]
    fn extract_falls_back_to_parent_groupid() {
        let xml = include_str!("../../../tests/fixtures/java/poms/child-pom.xml");
        assert_eq!(extract_groupid(xml).as_deref(), Some("com.example.parent"));
    }

    #[test]
    fn extract_no_groupid_returns_none() {
        let xml = include_str!("../../../tests/fixtures/java/poms/no-groupid-pom.xml");
        assert!(extract_groupid(xml).is_none());
    }

    #[test]
    fn extract_malformed_returns_none() {
        assert!(extract_groupid("<not-xml>>>>").is_none());
    }

    #[test]
    fn detect_empty_dir_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(detect(tmp.path()).is_empty());
    }

    #[test]
    fn detect_pom_only() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::copy(
            "tests/fixtures/java/poms/single-module-pom.xml",
            tmp.path().join("pom.xml"),
        )
        .unwrap();
        assert_eq!(detect(tmp.path()), vec!["com.example.app"]);
    }

    #[test]
    fn detect_sources_only() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_dir = tmp.path().join("src/main/java/com/example/myapp/service");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        // Also add a sibling so myapp has 2 children → walk stops at myapp
        std::fs::create_dir_all(tmp.path().join("src/main/java/com/example/myapp/controller"))
            .unwrap();
        assert_eq!(detect(tmp.path()), vec!["com.example.myapp"]);
    }

    #[test]
    fn detect_pom_and_sources_deduplicates() {
        let tmp = tempfile::tempdir().unwrap();
        // pom.xml with groupId=com.example.app
        std::fs::copy(
            "tests/fixtures/java/poms/single-module-pom.xml",
            tmp.path().join("pom.xml"),
        )
        .unwrap();
        // src/main/java/com/example/app/ → same as pom
        let pkg_dir = tmp.path().join("src/main/java/com/example/app/service");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::create_dir_all(tmp.path().join("src/main/java/com/example/app/model")).unwrap();
        assert_eq!(detect(tmp.path()), vec!["com.example.app"]);
    }

    #[test]
    fn detect_pom_and_sources_different_packages() {
        let tmp = tempfile::tempdir().unwrap();
        // pom.xml groupId=com.example.app
        std::fs::copy(
            "tests/fixtures/java/poms/single-module-pom.xml",
            tmp.path().join("pom.xml"),
        )
        .unwrap();
        // but sources live under pl.company.project
        let pkg_dir = tmp.path().join("src/main/java/pl/company/project/service");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::create_dir_all(tmp.path().join("src/main/java/pl/company/project/model"))
            .unwrap();
        let result = detect(tmp.path());
        assert_eq!(result, vec!["com.example.app", "pl.company.project"]);
    }
}
