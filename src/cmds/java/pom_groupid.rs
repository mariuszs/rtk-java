//! Autodetects the application Java package from `pom.xml <groupId>`.
//! Used by `surefire_reports` / `stack_trace` to classify application frames.
//! Can be overridden by `RTK_MVN_APP_PACKAGE` env var.

use quick_xml::events::Event;
use quick_xml::Reader;
use std::path::Path;

const OVERRIDE_ENV: &str = "RTK_MVN_APP_PACKAGE";

/// Detect the Maven groupId of `cwd`'s `pom.xml`.
///
/// Resolution order:
/// 1. If env var `RTK_MVN_APP_PACKAGE` is set and non-empty, return it.
/// 2. Read `cwd/pom.xml` and extract top-level `<project>/<groupId>`.
/// 3. Fall back to `<project>/<parent>/<groupId>`.
/// 4. Otherwise `None`.
pub fn detect(cwd: &Path) -> Option<String> {
    if let Ok(value) = std::env::var(OVERRIDE_ENV) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

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
    fn detect_missing_pom_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(detect(tmp.path()).is_none());
    }

    #[test]
    fn detect_env_override_wins() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::copy(
            "tests/fixtures/java/poms/single-module-pom.xml",
            tmp.path().join("pom.xml"),
        )
        .unwrap();

        // Serial to avoid concurrent env mutation with other tests — this is
        // tested in isolation; we restore the var on exit.
        let guard = EnvGuard::set(OVERRIDE_ENV, "com.override");
        assert_eq!(detect(tmp.path()).as_deref(), Some("com.override"));
        drop(guard);
    }

    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            // SAFETY: single-threaded test; no other thread reads this env var.
            unsafe { std::env::set_var(key, value) };
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: single-threaded test; restoring env var on drop.
            match &self.original {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }
}
