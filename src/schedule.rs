//! Parity with `prime-agent schedule <list|add|cancel>`: prompts the
//! daemon itself injects into a session later, without any client
//! attached -- durable across a daemon restart (persisted per-session,
//! same as everything else this project keeps on disk rather than only
//! in memory).
//!
//! Owned entirely by the daemon supervisor -- `daemon::Supervisor`'s own
//! background firing loop (`daemon::run`'s spawned task, calling
//! `Supervisor::fire_due_schedules` on a fixed interval) is the only
//! thing that ever reads a due entry and turns it into an ordinary
//! internal `SessionPrompt`, the same relay path a real client's request
//! takes (`ensure_worker_running` + a private-socket round trip) -- a
//! fired schedule is indistinguishable, from the worker's point of view,
//! from a client-issued prompt.

use std::path::Path;

use crate::error::{Context, HarnessError, Result};
use crate::paths;
use crate::protocol::{ScheduleEntry, ScheduleKind};

/// Async: see `catalog::read_session_state`'s doc comment for why every
/// blocking file read the daemon supervisor's own async handlers and
/// background loops make -- this included -- has to go through
/// `spawn_blocking` rather than running inline.
pub async fn read_all(session_dir: &Path) -> Result<Vec<ScheduleEntry>> {
    let session_dir = session_dir.to_path_buf();
    rusty_tokio::spawn_blocking(move || read_all_sync(&session_dir))
        .await
        .map_err(|_| HarnessError::protocol(Context::Session, "schedule read task panicked"))?
}

fn read_all_sync(session_dir: &Path) -> Result<Vec<ScheduleEntry>> {
    let path = paths::schedules_path(session_dir);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| HarnessError::json(Context::Session, Some(path), e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(HarnessError::io(Context::Session, Some(path), e)),
    }
}

fn write_all(session_dir: &Path, entries: &[ScheduleEntry]) -> Result<()> {
    let path = paths::schedules_path(session_dir);
    let json = serde_json::to_string_pretty(entries)
        .map_err(|e| HarnessError::json(Context::Session, Some(path.clone()), e))?;
    std::fs::write(&path, json).map_err(|e| HarnessError::io(Context::Session, Some(path), e))
}

/// Same shape/uniqueness reasoning as `session::new_session_id` -- a
/// display id, not a security-sensitive one.
fn new_schedule_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("sched-{nanos:x}")
}

pub async fn add(session_dir: &Path, text: String, kind: ScheduleKind) -> Result<String> {
    let session_dir = session_dir.to_path_buf();
    rusty_tokio::spawn_blocking(move || add_sync(&session_dir, text, kind))
        .await
        .map_err(|_| HarnessError::protocol(Context::Session, "schedule add task panicked"))?
}

fn add_sync(session_dir: &Path, text: String, kind: ScheduleKind) -> Result<String> {
    let mut entries = read_all_sync(session_dir)?;
    let schedule_id = new_schedule_id();
    let next_fire_ms = match kind {
        ScheduleKind::Once { at_ms } => at_ms,
        ScheduleKind::Every { interval_ms } => paths::now_ms() + interval_ms,
    };
    entries.push(ScheduleEntry {
        schedule_id: schedule_id.clone(),
        text,
        kind,
        next_fire_ms,
        created_at_ms: paths::now_ms(),
    });
    write_all(session_dir, &entries)?;
    Ok(schedule_id)
}

pub async fn cancel(session_dir: &Path, schedule_id: &str) -> Result<bool> {
    let session_dir = session_dir.to_path_buf();
    let schedule_id = schedule_id.to_string();
    rusty_tokio::spawn_blocking(move || cancel_sync(&session_dir, &schedule_id))
        .await
        .map_err(|_| HarnessError::protocol(Context::Session, "schedule cancel task panicked"))?
}

fn cancel_sync(session_dir: &Path, schedule_id: &str) -> Result<bool> {
    let mut entries = read_all_sync(session_dir)?;
    let before = entries.len();
    entries.retain(|e| e.schedule_id != schedule_id);
    let found = entries.len() != before;
    if found {
        write_all(session_dir, &entries)?;
    }
    Ok(found)
}

/// Pops every entry from `session_dir`'s schedule file that's due at or
/// before `now_ms`, advancing (`Every`) or removing (`Once`) each in
/// place, and returns the due ones' `(schedule_id, text)` for the caller
/// to actually fire. Read-modify-write, not read-then-separately-write,
/// so a schedule this call decides is due is already rescheduled/removed
/// on disk before the caller does anything that could itself fail (a
/// worker that's slow to respond must not cause the same entry to fire
/// twice on the next poll).
pub async fn take_due(session_dir: &Path, now_ms: u64) -> Result<Vec<(String, String)>> {
    let session_dir = session_dir.to_path_buf();
    rusty_tokio::spawn_blocking(move || take_due_sync(&session_dir, now_ms))
        .await
        .map_err(|_| HarnessError::protocol(Context::Session, "schedule due-check task panicked"))?
}

fn take_due_sync(session_dir: &Path, now_ms: u64) -> Result<Vec<(String, String)>> {
    let mut entries = read_all_sync(session_dir)?;
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let mut due = Vec::new();
    entries.retain_mut(|entry| {
        if entry.next_fire_ms > now_ms {
            return true;
        }
        due.push((entry.schedule_id.clone(), entry.text.clone()));
        match &entry.kind {
            ScheduleKind::Once { .. } => false,
            ScheduleKind::Every { interval_ms } => {
                // Skip forward past `now_ms` rather than firing a burst
                // of catch-up prompts if the daemon was down a while.
                let mut next = entry.next_fire_ms + interval_ms;
                while next <= now_ms {
                    next += interval_ms;
                }
                entry.next_fire_ms = next;
                true
            }
        }
    });
    if !due.is_empty() {
        write_all(session_dir, &entries)?;
    }
    Ok(due)
}
