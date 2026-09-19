//! Persist schedule jobs under `$DEVO_HOME/schedules.json`.
//!
//! The store is process-shared via a mutex; wake/dispatch runs outside the
//! session actor mailbox (L2-DES-SERVER-002).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use devo_protocol::native::ids::{JobId, SessionId};
use devo_protocol::native::rpc_schedule::{
    ScheduleJob, ScheduleJobKind, ScheduleJobStatus, ScheduleUpdateAction,
    SessionScheduleUpsertParams,
};

const SCHEDULES_FILE_NAME: &str = "schedules.json";

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct SchedulesFile {
    #[serde(default)]
    jobs: Vec<ScheduleJob>,
}

pub(crate) struct ScheduleStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl ScheduleStore {
    pub(crate) fn new(devo_home: impl Into<PathBuf>) -> Self {
        let path = devo_home.into().join(SCHEDULES_FILE_NAME);
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    pub(crate) fn list(
        &self,
        session_id: Option<&SessionId>,
        cwd: Option<&Path>,
    ) -> Result<Vec<ScheduleJob>> {
        self.list_filtered(session_id, cwd, /*include_stopped*/ false)
    }

    /// Like [`Self::list`], optionally keeping [`ScheduleJobStatus::Stopped`] jobs.
    pub(crate) fn list_filtered(
        &self,
        session_id: Option<&SessionId>,
        cwd: Option<&Path>,
        include_stopped: bool,
    ) -> Result<Vec<ScheduleJob>> {
        let _guard = self.lock.lock().expect("schedule store mutex poisoned");
        let jobs = self.read_unlocked()?.jobs;
        Ok(jobs
            .into_iter()
            .filter(|job| {
                if !include_stopped && matches!(job.status, ScheduleJobStatus::Stopped) {
                    return false;
                }
                if let Some(session_id) = session_id
                    && &job.session_id != session_id
                {
                    return false;
                }
                if let Some(cwd) = cwd {
                    match job.cwd.as_deref() {
                        Some(job_cwd) => job_cwd == cwd,
                        None => true,
                    }
                } else {
                    true
                }
            })
            .collect())
    }

    pub(crate) fn upsert(&self, params: SessionScheduleUpsertParams) -> Result<ScheduleJob> {
        let _guard = self.lock.lock().expect("schedule store mutex poisoned");
        let mut file = self.read_unlocked()?;
        let now = Utc::now();
        let text = params
            .prompt
            .clone()
            .or_else(|| params.instruction.clone())
            .unwrap_or_default();
        let interval_ms = params
            .interval_ms
            .or_else(|| params.schedule.as_deref().and_then(parse_interval_ms));
        let next_run_at = next_run_from_interval(interval_ms, now);

        let job = if let Some(job_id) = params.job_id {
            let Some(existing) = file.jobs.iter_mut().find(|job| job.job_id == job_id) else {
                anyhow::bail!("job not found: {}", job_id.as_str());
            };
            existing.kind = params.kind;
            existing.session_id = params.session_id;
            existing.cwd = params.cwd;
            existing.schedule = params.schedule;
            existing.interval_ms = interval_ms;
            match params.kind {
                ScheduleJobKind::Cron => {
                    existing.prompt = Some(text.clone()).filter(|s| !s.is_empty());
                    existing.instruction = None;
                }
                ScheduleJobKind::Heartbeat => {
                    existing.instruction = Some(text.clone()).filter(|s| !s.is_empty());
                    existing.prompt = None;
                }
            }
            existing.delivery_mode = params.delivery_mode;
            existing.label = params.label;
            existing.updated_at = now;
            if existing.status == ScheduleJobStatus::Active {
                existing.next_run_at = next_run_at;
            }
            existing.clone()
        } else if params.kind == ScheduleJobKind::Heartbeat {
            // User `/heartbeat` is one-per-session (no label): replace the latest
            // active/paused heartbeat. Labeled RLM heartbeats may coexist.
            let replace_unlabeled = params.label.as_ref().is_none_or(|l| l.trim().is_empty());
            if replace_unlabeled
                && let Some(existing) = file.jobs.iter_mut().rev().find(|job| {
                    job.kind == ScheduleJobKind::Heartbeat
                        && job.session_id == params.session_id
                        && job.label.as_ref().is_none_or(|l| l.trim().is_empty())
                        && matches!(
                            job.status,
                            ScheduleJobStatus::Active | ScheduleJobStatus::Paused
                        )
                })
            {
                existing.cwd = params.cwd;
                existing.schedule = params.schedule;
                existing.interval_ms = interval_ms;
                existing.instruction = Some(text.clone()).filter(|s| !s.is_empty());
                existing.prompt = None;
                existing.delivery_mode = params.delivery_mode;
                existing.label = params.label;
                existing.status = ScheduleJobStatus::Active;
                existing.updated_at = now;
                existing.next_run_at = next_run_at;
                existing.clone()
            } else {
                let job = new_job(params, text, interval_ms, next_run_at, now);
                file.jobs.push(job.clone());
                job
            }
        } else {
            let job = new_job(params, text, interval_ms, next_run_at, now);
            file.jobs.push(job.clone());
            job
        };

        self.write_unlocked(&file)?;
        Ok(job)
    }

    pub(crate) fn update(
        &self,
        job_id: &JobId,
        action: ScheduleUpdateAction,
    ) -> Result<ScheduleJob> {
        let _guard = self.lock.lock().expect("schedule store mutex poisoned");
        let mut file = self.read_unlocked()?;
        let Some(job) = file.jobs.iter_mut().find(|job| &job.job_id == job_id) else {
            anyhow::bail!("job not found: {}", job_id.as_str());
        };
        let now = Utc::now();
        match action {
            ScheduleUpdateAction::Pause => {
                job.status = ScheduleJobStatus::Paused;
                job.next_run_at = None;
            }
            ScheduleUpdateAction::Resume => {
                job.status = ScheduleJobStatus::Active;
                job.next_run_at = next_run_from_interval(job.interval_ms, now);
            }
            ScheduleUpdateAction::Stop => {
                job.status = ScheduleJobStatus::Stopped;
                job.next_run_at = None;
            }
        }
        job.updated_at = now;
        let updated = job.clone();
        self.write_unlocked(&file)?;
        Ok(updated)
    }

    pub(crate) fn delete(&self, job_id: &JobId) -> Result<ScheduleJob> {
        let _guard = self.lock.lock().expect("schedule store mutex poisoned");
        let mut file = self.read_unlocked()?;
        let Some(index) = file.jobs.iter().position(|job| &job.job_id == job_id) else {
            anyhow::bail!("job not found: {}", job_id.as_str());
        };
        let mut job = file.jobs.remove(index);
        job.status = ScheduleJobStatus::Stopped;
        job.updated_at = Utc::now();
        job.next_run_at = None;
        self.write_unlocked(&file)?;
        Ok(job)
    }

    pub(crate) fn claim_due(&self, now: DateTime<Utc>) -> Result<Vec<ScheduleJob>> {
        let _guard = self.lock.lock().expect("schedule store mutex poisoned");
        let mut file = self.read_unlocked()?;
        let mut due = Vec::new();
        for job in &mut file.jobs {
            if job.status != ScheduleJobStatus::Active {
                continue;
            }
            let Some(next_run_at) = job.next_run_at else {
                continue;
            };
            if next_run_at > now {
                continue;
            }
            job.last_run_at = Some(now);
            job.run_count = job.run_count.saturating_add(1);
            job.next_run_at = next_run_from_interval(job.interval_ms, now);
            job.updated_at = now;
            due.push(job.clone());
        }
        if !due.is_empty() {
            self.write_unlocked(&file)?;
        }
        Ok(due)
    }

    fn read_unlocked(&self) -> Result<SchedulesFile> {
        match fs::read_to_string(&self.path) {
            Ok(raw) if raw.trim().is_empty() => Ok(SchedulesFile::default()),
            Ok(raw) => serde_json::from_str(&raw)
                .with_context(|| format!("parse schedules file {}", self.path.display())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(SchedulesFile::default()),
            Err(error) => {
                Err(error).with_context(|| format!("read schedules file {}", self.path.display()))
            }
        }
    }

    fn write_unlocked(&self, file: &SchedulesFile) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create schedule dir {}", parent.display()))?;
        }
        let raw = serde_json::to_string_pretty(file).context("serialize schedules file")?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, raw).with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), self.path.display()))?;
        Ok(())
    }
}

fn new_job(
    params: SessionScheduleUpsertParams,
    text: String,
    interval_ms: Option<u64>,
    next_run_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> ScheduleJob {
    let (prompt, instruction) = match params.kind {
        ScheduleJobKind::Cron => (Some(text).filter(|s| !s.is_empty()), None),
        ScheduleJobKind::Heartbeat => (None, Some(text).filter(|s| !s.is_empty())),
    };
    ScheduleJob {
        job_id: JobId::new(),
        kind: params.kind,
        status: ScheduleJobStatus::Active,
        session_id: params.session_id,
        cwd: params.cwd,
        schedule: params.schedule,
        interval_ms,
        prompt,
        instruction,
        delivery_mode: params.delivery_mode,
        label: params.label,
        created_at: now,
        updated_at: now,
        next_run_at,
        last_run_at: None,
        run_count: 0,
    }
}

/// Parses `every 5m` / `5m` / `every 30s` / `1h` style intervals.
pub(crate) fn parse_interval_ms(schedule: &str) -> Option<u64> {
    let trimmed = schedule.trim().to_ascii_lowercase();
    let rest = trimmed
        .strip_prefix("every ")
        .unwrap_or(trimmed.as_str())
        .trim();
    if rest.is_empty() {
        return None;
    }
    let (num, unit) = rest.split_at(
        rest.find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len()),
    );
    let amount: u64 = num.parse().ok()?;
    let unit = unit.trim();
    let multiplier = match unit {
        "s" | "sec" | "secs" | "second" | "seconds" => 1_000,
        "m" | "min" | "mins" | "minute" | "minutes" => 60_000,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600_000,
        _ => return None,
    };
    Some(amount.saturating_mul(multiplier))
}

fn next_run_from_interval(interval_ms: Option<u64>, from: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let interval_ms = interval_ms.filter(|ms| *ms > 0)?;
    Some(from + Duration::milliseconds(interval_ms as i64))
}

#[cfg(test)]
mod tests {
    use devo_protocol::native::rpc_schedule::ScheduleDeliveryMode;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    use super::*;

    /// Trace: L2-DES-APP-012
    /// Verifies: schedule upsert/list/delete round-trips through schedules.json.
    #[test]
    fn upsert_list_delete_round_trip() {
        let dir = tempdir().expect("tempdir");
        let store = ScheduleStore::new(dir.path());
        let session_id = SessionId::from_string("ses_sched_test".into());
        let upserted = store
            .upsert(SessionScheduleUpsertParams {
                kind: ScheduleJobKind::Cron,
                session_id,
                job_id: None,
                cwd: Some(dir.path().to_path_buf()),
                schedule: Some("every 5m".into()),
                interval_ms: None,
                prompt: Some("ping".into()),
                instruction: None,
                delivery_mode: ScheduleDeliveryMode::Steer,
                label: Some("cron".into()),
            })
            .expect("upsert");
        assert_eq!(upserted.interval_ms, Some(300_000));
        assert!(dir.path().join("schedules.json").is_file());

        let listed = store.list(Some(&session_id), None).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].job_id, upserted.job_id);

        let deleted = store.delete(&upserted.job_id).expect("delete");
        assert_eq!(deleted.status, ScheduleJobStatus::Stopped);
        assert!(
            store
                .list(Some(&session_id), None)
                .expect("list")
                .is_empty()
        );
    }

    #[test]
    fn parse_every_interval() {
        assert_eq!(parse_interval_ms("every 5m"), Some(300_000));
        assert_eq!(parse_interval_ms("5m"), Some(300_000));
        assert_eq!(parse_interval_ms("every 30s"), Some(30_000));
        assert_eq!(parse_interval_ms("every 1h"), Some(3_600_000));
        assert_eq!(parse_interval_ms("0 */5 * * * *"), None);
    }

    #[test]
    fn labeled_heartbeats_coexist() {
        let dir = tempdir().expect("tempdir");
        let store = ScheduleStore::new(dir.path());
        let session_id = SessionId::from_string("ses_hb_multi".into());
        let first = store
            .upsert(SessionScheduleUpsertParams {
                kind: ScheduleJobKind::Heartbeat,
                session_id,
                job_id: None,
                cwd: None,
                schedule: Some("every 5m".into()),
                interval_ms: None,
                prompt: None,
                instruction: Some("check a".into()),
                delivery_mode: ScheduleDeliveryMode::Steer,
                label: Some("a".into()),
            })
            .expect("upsert a");
        let second = store
            .upsert(SessionScheduleUpsertParams {
                kind: ScheduleJobKind::Heartbeat,
                session_id,
                job_id: None,
                cwd: None,
                schedule: Some("every 10m".into()),
                interval_ms: None,
                prompt: None,
                instruction: Some("check b".into()),
                delivery_mode: ScheduleDeliveryMode::FollowUp,
                label: Some("b".into()),
            })
            .expect("upsert b");
        let listed = store.list(Some(&session_id), None).expect("list");
        assert_eq!(listed.len(), 2);
        assert_ne!(first.job_id, second.job_id);
    }
}
