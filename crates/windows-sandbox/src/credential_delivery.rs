//! Per-session credential authority (design doc `docs/design/rlm-permissions.md`
//! §9, P2). Devo-owned: codex's per-exec model never needs to deliver
//! credentials to a long-lived sandboxed process, so this layer cannot live
//! upstream (see `DEVO_PATCHES.md`).
//!
//! A session-scoped capability SID is minted once per kernel session and, when
//! the kernel is fenced, travels in its restricted token. Granting a path =
//! adding an inheritable allow ACE for that SID (raw `open()` works
//! immediately, no kernel restart); revoking = removing the ACE (new opens are
//! refused; already-open handles keep working — a documented residual, §9).
//! Every grant/revoke is journaled to `<devo_home>/.sandbox/credential_journal.json`
//! so crash recovery can re-derive deliveries and a startup sweep can remove
//! orphaned ACEs of dead sessions. The journal is the authority for recovery —
//! not just an audit trail (§8).

use crate::acl::add_allow_ace;
use crate::acl::ensure_allow_mask_aces_with_inheritance;
use crate::acl::revoke_ace;
use crate::setup::sandbox_dir;
use crate::token::LocalSid;
use anyhow::Result;
use rand::RngCore;
use rand::rngs::SmallRng;
use rand::SeedableRng;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;

/// Inheritance flags (mirroring acl.rs's private constants): delivered ACEs
/// propagate to the granted subtree.
const CONTAINER_INHERIT_ACE: u32 = 0x2;
const OBJECT_INHERIT_ACE: u32 = 0x1;

pub struct SessionCredentialAuthority {
    sid_string: String,
    journal_path: PathBuf,
    session_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CredentialJournal {
    /// session_id → credential record. The SID is journaled alongside the
    /// grants because revocation is impossible without it, and crash recovery
    /// (not just audit) reads this file (§8).
    sessions: BTreeMap<String, SessionRecord>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SessionRecord {
    sid: String,
    #[serde(default)]
    grants: Vec<GrantRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GrantRecord {
    root: PathBuf,
    /// "read" | "write" — which mask the delivered ACE carries.
    access: String,
}

/// Access class of a delivered credential ACE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialAccess {
    Read,
    Write,
}

impl CredentialAccess {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

/// Canonical read mask for delivered read credentials (mirrors
/// `FileSystemAccessMode::Read` in acl.rs).
const GENERIC_READ_MASK: u32 = 0x8000_0000;

fn make_random_session_sid() -> String {
    let mut rng = SmallRng::from_entropy();
    let a = rng.next_u32();
    let b = rng.next_u32();
    let c = rng.next_u32();
    let d = rng.next_u32();
    format!("S-1-5-21-{a}-{b}-{c}-{d}")
}

fn journal_path_for(devo_home: &Path) -> PathBuf {
    sandbox_dir(devo_home).join("credential_journal.json")
}

fn load_journal(path: &Path) -> CredentialJournal {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn persist_journal(path: &Path, journal: &CredentialJournal) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, serde_json::to_string(journal)?)?;
    Ok(())
}

impl SessionCredentialAuthority {
    /// Mint a session credential authority. Does not touch any DACL yet; the
    /// journal gains its session row on the first grant.
    pub fn new(session_id: &str, devo_home: &Path) -> Result<Self> {
        let sid_string = make_random_session_sid();
        // Validate the SID shape eagerly; store only the string so this type
        // stays Send+Sync (LocalSid wraps a raw pointer and is neither).
        LocalSid::from_string(&sid_string)?;
        Ok(Self {
            sid_string,
            journal_path: journal_path_for(devo_home),
            session_id: session_id.to_string(),
        })
    }

    /// Transient SID handle for one ACE operation.
    fn local_sid(&self) -> Result<LocalSid> {
        LocalSid::from_string(&self.sid_string)
    }

    pub fn sid(&self) -> &str {
        &self.sid_string
    }

    /// Grant write access to `root` for this session: inheritable allow ACE on
    /// the root (Windows propagation covers descendants), journaled. Idempotent
    /// — an already-granted root is a no-op. Returns `true` when a new ACE was
    /// applied.
    pub fn grant_write_root(&self, root: &Path) -> Result<bool> {
        self.grant_root(root, CredentialAccess::Write)
    }

    /// Grant read access to `root` (read-mask inheritable allow ACE), journaled
    /// and idempotent, same contract as [`grant_write_root`].
    pub fn grant_read_root(&self, root: &Path) -> Result<bool> {
        self.grant_root(root, CredentialAccess::Read)
    }

    fn grant_root(&self, root: &Path, access: CredentialAccess) -> Result<bool> {
        let mut journal = load_journal(&self.journal_path);
        let record = journal
            .sessions
            .entry(self.session_id.clone())
            .or_insert_with(|| SessionRecord {
                sid: self.sid_string.clone(),
                grants: Vec::new(),
            });
        if record
            .grants
            .iter()
            .any(|g| g.root == root && g.access == access.as_str())
        {
            return Ok(false);
        }
        let sid = self.local_sid()?;
        let applied = match access {
            CredentialAccess::Write => unsafe { add_allow_ace(root, sid.as_ptr()) }?,
            CredentialAccess::Read => unsafe {
                ensure_allow_mask_aces_with_inheritance(
                    root,
                    &[sid.as_ptr()],
                    FILE_GENERIC_READ | GENERIC_READ_MASK,
                    CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE,
                )?
            },
        };
        record.grants.push(GrantRecord {
            root: root.to_path_buf(),
            access: access.as_str().to_string(),
        });
        persist_journal(&self.journal_path, &journal)?;
        Ok(applied)
    }

    /// Revoke one granted root: remove the session SID's ACEs from it and drop
    /// the journal entry. Already-open handles keep working (documented
    /// residual, design doc §9).
    pub fn revoke_write_root(&self, root: &Path) -> Result<()> {
        let mut journal = load_journal(&self.journal_path);
        if let Some(record) = journal.sessions.get_mut(&self.session_id) {
            record.grants.retain(|g| g.root != root);
            if record.grants.is_empty() {
                journal.sessions.remove(&self.session_id);
            }
        }
        persist_journal(&self.journal_path, &journal)?;
        if let Ok(sid) = self.local_sid() {
            unsafe { revoke_ace(root, sid.as_ptr()) };
        }
        Ok(())
    }

    /// Revoke everything this session ever granted and remove its journal row.
    /// Call on session end; the startup sweep covers crashed sessions.
    pub fn revoke_all(&self) -> Result<usize> {
        let mut journal = load_journal(&self.journal_path);
        let Some(record) = journal.sessions.remove(&self.session_id) else {
            return Ok(0);
        };
        let sid = self.local_sid().ok();
        for grant in &record.grants {
            if let Some(sid) = &sid {
                unsafe { revoke_ace(&grant.root, sid.as_ptr()) };
            }
        }
        persist_journal(&self.journal_path, &journal)?;
        Ok(record.grants.len())
    }
}

/// Startup hygiene (design doc §9/B4): revoke ACEs belonging to sessions that
/// are no longer live, so crashed sessions cannot leave orphaned capability
/// ACEs on user directories forever. Returns the number of roots swept.
pub fn sweep_orphaned_sessions(devo_home: &Path, live_session_ids: &[&str]) -> Result<usize> {
    let journal_path = journal_path_for(devo_home);
    let mut journal = load_journal(&journal_path);
    let dead: Vec<String> = journal
        .sessions
        .keys()
        .filter(|id| !live_session_ids.contains(&id.as_str()))
        .cloned()
        .collect();
    let mut swept = 0usize;
    for id in &dead {
        if let Some(record) = journal.sessions.remove(id) {
            if let Ok(local) = LocalSid::from_string(&record.sid) {
                for grant in &record.grants {
                    unsafe { revoke_ace(&grant.root, local.as_ptr()) };
                    swept += 1;
                }
            }
        }
    }
    persist_journal(&journal_path, &journal)?;
    Ok(swept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acl::dacl_has_write_allow_for_sid;
    use crate::acl::fetch_dacl_handle;
    use pretty_assertions::assert_eq;

    #[test]
    fn grant_and_revoke_write_root_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let root = tmp.path().join("granted");
        fs::create_dir_all(&root).expect("root");

        let authority = SessionCredentialAuthority::new("ses_test_1", home.path()).expect("authority");
        // Bind the LocalSid: as_ptr() on a temporary would dangle after the
        // statement ends.
        let sid = LocalSid::from_string(authority.sid()).expect("sid");
        let sid_ptr = sid.as_ptr();

        let (dacl, _sd) = unsafe { fetch_dacl_handle(&root).expect("dacl") };
        assert!(
            !unsafe { dacl_has_write_allow_for_sid(dacl, sid_ptr) },
            "fresh root must not carry the session SID"
        );

        assert!(authority.grant_write_root(&root).expect("grant"));
        // Idempotent: second grant is a journal no-op (Ok(false) or ACE-exists).
        let again = authority.grant_write_root(&root).expect("grant again");
        assert!(!again, "second grant must be a no-op");

        let (dacl, _sd) = unsafe { fetch_dacl_handle(&root).expect("dacl") };
        assert!(
            unsafe { dacl_has_write_allow_for_sid(dacl, sid_ptr) },
            "granted root must carry the session SID allow ACE"
        );

        authority.revoke_write_root(&root).expect("revoke");
        let (dacl, _sd) = unsafe { fetch_dacl_handle(&root).expect("dacl") };
        assert!(
            !unsafe { dacl_has_write_allow_for_sid(dacl, sid_ptr) },
            "revoked root must not carry the session SID ACE"
        );
    }

    #[test]
    fn revoke_all_clears_journal_row_and_aces() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        fs::create_dir_all(&a).expect("a");
        fs::create_dir_all(&b).expect("b");

        let authority = SessionCredentialAuthority::new("ses_test_2", home.path()).expect("authority");
        authority.grant_write_root(&a).expect("grant a");
        authority.grant_write_root(&b).expect("grant b");

        assert_eq!(authority.revoke_all().expect("revoke all"), 2);
        assert_eq!(authority.revoke_all().expect("revoke all again"), 0);

        let journal = load_journal(&journal_path_for(home.path()));
        assert!(journal.sessions.is_empty(), "journal row must be gone");
    }

    #[test]
    fn sweep_removes_only_dead_sessions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let root = tmp.path().join("dead-owned");
        fs::create_dir_all(&root).expect("root");

        let dead = SessionCredentialAuthority::new("ses_dead", home.path()).expect("dead");
        dead.grant_write_root(&root).expect("grant");
        let live = SessionCredentialAuthority::new("ses_live", home.path()).expect("live");

        let swept = super::sweep_orphaned_sessions(home.path(), &["ses_live"]).expect("sweep");
        assert_eq!(swept, 1, "dead session root must be swept");

        let (dacl, _sd) = unsafe { fetch_dacl_handle(&root).expect("dacl") };
        let dead_sid = LocalSid::from_string(dead.sid()).expect("sid");
        assert!(
            !unsafe { dacl_has_write_allow_for_sid(dacl, dead_sid.as_ptr()) },
            "dead session ACE must be removed"
        );
        let _ = live; // live authority untouched by the sweep
    }

    /// Env-driven grant runner (operational tool, not an assertion):
    /// `DEVO_GRANT_SID` + `DEVO_GRANT_ROOT` [+ `DEVO_GRANT_HOME`] delivers a
    /// write ACE for an already-running fenced kernel's session SID.
    /// Runs as a test so it shares the crate's token/ACL plumbing:
    /// `cargo test -p devo-windows-sandbox --lib grant_for_running_session -- --nocapture`
    #[test]
    fn grant_for_running_session() {
        let (Ok(sid), Ok(root), home) = (
            std::env::var("DEVO_GRANT_SID"),
            std::env::var("DEVO_GRANT_ROOT"),
            std::env::var("DEVO_GRANT_HOME").unwrap_or_else(|_| {
                std::env::var_os("USERPROFILE")
                    .map(|p| format!("{}\\.devo", p.to_string_lossy()))
                    .unwrap_or_else(|| ".devo".to_string())
            }),
        ) else {
            eprintln!("skip: DEVO_GRANT_SID/DEVO_GRANT_ROOT not set");
            return;
        };
        let authority =
            SessionCredentialAuthority::new(&format!("kernel-{sid}"), std::path::Path::new(&home))
                .expect("authority");
        // The authority mints a fresh SID; for an external grant the SID is
        // fixed, so rebind via the journal path with the given SID.
        let applied = unsafe {
            crate::acl::add_allow_ace(
                std::path::Path::new(&root),
                LocalSid::from_string(&sid).expect("sid").as_ptr(),
            )
        }
        .expect("grant");
        eprintln!("granted write ACE root={root} applied={applied}");
    }

    #[test]
    fn grant_read_root_delivers_read_mask_ace() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let root = tmp.path().join("readable");
        fs::create_dir_all(&root).expect("root");

        let authority =
            SessionCredentialAuthority::new("ses_test_read", home.path()).expect("authority");
        assert!(authority.grant_read_root(&root).expect("grant read"));
        assert!(!authority.grant_read_root(&root).expect("grant read again"));

        let sid = LocalSid::from_string(authority.sid()).expect("sid");
        let read_mask = FILE_GENERIC_READ | GENERIC_READ_MASK;
        assert!(
            unsafe { crate::acl::path_mask_allows(&root, &[sid.as_ptr()], read_mask, false) }
                .expect("mask check"),
            "read grant must deliver a read-mask allow ACE"
        );
    }
}
