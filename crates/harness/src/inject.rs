//! Host helper that loads H and formats the prompt digest.
//!
//! Call sites: cold prompt assembly (after catalog skills, before hidden goal).
//! See L2-DES-HARNESS-001 DD-5 and docs/rlm-native-api.md section 7.

use std::path::Path;

use crate::digest::format_harness_state_for_prompt;
use crate::state::{HarnessState, HarnessStateError};

/// Loads session harness state and returns the model-visible digest string.
///
/// Missing state yields an empty string. Corrupt JSON fails closed ([`Err`]);
/// callers should omit the digest rather than invent content.
#[derive(Debug, Default, Clone, Copy)]
pub struct HarnessDigestInjector;

impl HarnessDigestInjector {
    /// Load `session_dir/harness/harness_state.json` and format for prompt.
    pub fn digest_for_session_dir(session_dir: &Path) -> Result<String, HarnessStateError> {
        let path = HarnessState::file_path(session_dir);
        if !path.exists() {
            return Ok(String::new());
        }
        let state = HarnessState::load(&path)?;
        Ok(format_harness_state_for_prompt(&state))
    }

    /// Best-effort load: missing or corrupt yields empty (caller may log).
    pub fn digest_or_empty(session_dir: &Path) -> String {
        Self::digest_for_session_dir(session_dir).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{HarnessEntry, HarnessKind, HarnessScope};
    use chrono::Utc;
    use pretty_assertions::assert_eq;

    /// Trace: L2-DES-HARNESS-001
    /// Verifies: injector returns digest content after a successful state write.
    #[test]
    fn injector_loads_digest_for_prompt_site() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(HarnessDigestInjector::digest_or_empty(dir.path()), "");

        let path = HarnessState::file_path(dir.path());
        let mut state = HarnessState::default();
        let now = Utc::now();
        state.entries.get_mut("memory").unwrap().insert(
            "m1".into(),
            HarnessEntry {
                id: "m1".into(),
                kind: HarnessKind::Memory,
                title: "pref".into(),
                content: "prefer tabs".into(),
                path: None,
                scope: Some(HarnessScope::Local),
                reference: serde_json::json!({}),
                arguments: serde_json::json!({}),
                metadata: serde_json::json!({}),
                source: "test".into(),
                created_at: now,
                updated_at: now,
                version: 1,
            },
        );
        state.save_atomic(&path).unwrap();

        let digest = HarnessDigestInjector::digest_for_session_dir(dir.path()).unwrap();
        assert!(digest.contains("prefer tabs"));
        assert!(digest.contains("refine.run"));
        assert!(digest.contains("Continual harness") || digest.contains("harness-digest"));
    }
}
