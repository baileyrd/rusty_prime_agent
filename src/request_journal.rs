//! Durable idempotency for `session prompt --request-id <id>`: an
//! append-only journal, written *before* the prompt is dispatched, that
//! survives the worker crash the in-memory version could not.
//!
//! Parity with `prime-agent`'s command-recovery journal (`R-PROTO-01`,
//! `R-PROTO-02`, `R-PROTO-03`): "every mutating command is keyed by
//! clientId + commandId and journaled append-only before dispatch";
//! "repeating a completed command returns its stored result rather than
//! re-executing it"; "a command received but lacking a durable result is
//! reported uncertain and never blindly replayed."
//!
//! # What was wrong with the in-memory version
//!
//! `AgentSession` used to keep a 64-entry `HashMap<request_id,
//! TranscriptEntry>`, and its own doc comment was honest that this was
//! "lost on a worker crash/restart". `COMPARISON.md` §5 put the sharper
//! point on it: a client retrying after a dropped connection is *most
//! likely* retrying because the worker died, which is precisely the case
//! where the cache is gone and the retry double-sends. The mechanism
//! failed in exactly the scenario it existed for.
//!
//! # Placement: per session, not per supervisor
//!
//! Upstream journals at the supervisor, because that is where its public
//! protocol and its `clientId + commandId` keying live. This journal sits
//! beside the `transcript.jsonl` it protects instead, for two reasons:
//! the side effect being deduplicated *is* a transcript append, and the
//! process that must not repeat it is the worker. A supervisor-side
//! journal would also have to survive a supervisor restart to be useful
//! here, and would still not stop a respawned worker from re-executing.
//!
//! # Durability, stated precisely
//!
//! Every append is `flush`ed and `sync_all`ed before the call returns, so
//! a record that has been acknowledged is on stable storage. The
//! transcript itself only `flush`es (see `session::append_transcript_line`),
//! which is the right level for the failure this project actually models
//! -- a *process* crash, where the OS buffer survives -- but it does mean
//! that after a machine-level crash the journal can be strictly ahead of
//! the transcript. [`RequestJournal`] does not assume otherwise: a
//! `Completed` record whose sequence is absent from the transcript is
//! treated as [`Outcome::Uncertain`], not as a result to hand back. See
//! `AgentSession::prompt_with_images_and_request_id` for that check.

use std::collections::{HashMap, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Context, HarnessError, Result};
use crate::paths;

/// How many records accumulate before the journal is rewritten in
/// compacted form. Upstream compacts its own command journal at the same
/// figure (`R-PROTO-15`); there is nothing magic about it beyond being
/// far more than any real retry burst and far less than a file worth
/// worrying about.
const COMPACT_AFTER_RECORDS: usize = 4096;

/// How many distinct request ids survive a compaction, most-recent
/// first. An id evicted here behaves exactly as if it had never been
/// seen -- which is the same bound the previous in-memory cache had at
/// 64, so nothing regresses by forgetting the truly ancient.
const RETAINED_IDS: usize = 256;

/// What the journal knows about one request id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Dispatch started and no durable result was ever recorded. The
    /// caller must be told this and the request must **not** be
    /// re-executed: the first attempt may well have landed, and this
    /// journal is the only thing that would know.
    Uncertain,
    /// The request completed and produced the transcript entry at this
    /// sequence. A repeat returns that entry rather than prompting again.
    Completed(u64),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Record {
    /// Written *before* dispatch. On its own -- with no `result` after
    /// it -- this is what makes a replayed id read as
    /// [`Outcome::Uncertain`].
    Begin { v: u8, request_id: String, at: u64 },
    /// Written after the prompt durably produced `sequence`.
    Result {
        v: u8,
        request_id: String,
        sequence: u64,
    },
}

pub struct RequestJournal {
    path: PathBuf,
    outcomes: HashMap<String, Outcome>,
    /// First-seen order, so compaction can keep the most recent
    /// [`RETAINED_IDS`] and drop the rest deterministically.
    order: VecDeque<String>,
    records: usize,
}

impl RequestJournal {
    /// Replays the journal for `session_dir`. A missing file is an empty
    /// journal, not an error -- that is both a brand-new session and any
    /// session that predates this mechanism.
    ///
    /// A record that fails to parse is skipped rather than failing the
    /// load. A corrupt tail (the classic half-written last line after a
    /// crash) must not make a session unopenable, and the cost of
    /// skipping is only that one id falls back to "never seen".
    pub fn load(context: Context, session_dir: &Path) -> Result<Self> {
        let path = paths::request_journal_path(session_dir);
        let mut journal = RequestJournal {
            path: path.clone(),
            outcomes: HashMap::new(),
            order: VecDeque::new(),
            records: 0,
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(journal),
            Err(e) => return Err(HarnessError::io(context, Some(path), e)),
        };
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let Ok(record) = serde_json::from_str::<Record>(line) else {
                continue;
            };
            journal.records += 1;
            match record {
                Record::Begin { request_id, .. } => {
                    journal.remember(request_id, Outcome::Uncertain);
                }
                Record::Result {
                    request_id,
                    sequence,
                    ..
                } => {
                    journal.remember(request_id, Outcome::Completed(sequence));
                }
            }
        }
        Ok(journal)
    }

    fn remember(&mut self, id: String, outcome: Outcome) {
        if self.outcomes.insert(id.clone(), outcome).is_none() {
            self.order.push_back(id);
        }
    }

    /// What this journal knows about `id`, if anything.
    pub fn lookup(&self, id: &str) -> Option<Outcome> {
        self.outcomes.get(id).copied()
    }

    /// Records that `id` is about to be dispatched. Durable before it
    /// returns -- that ordering is the entire point, since a `begin` that
    /// only reached memory would leave a crashed request looking like one
    /// that never arrived.
    pub async fn begin(&mut self, context: Context, id: &str) -> Result<()> {
        self.append(
            context,
            Record::Begin {
                v: 1,
                request_id: id.to_string(),
                at: paths::now_ms(),
            },
        )
        .await?;
        self.remember(id.to_string(), Outcome::Uncertain);
        Ok(())
    }

    /// Records that `id` completed, producing the transcript entry at
    /// `sequence`.
    pub async fn record_result(&mut self, context: Context, id: &str, sequence: u64) -> Result<()> {
        self.append(
            context,
            Record::Result {
                v: 1,
                request_id: id.to_string(),
                sequence,
            },
        )
        .await?;
        self.remember(id.to_string(), Outcome::Completed(sequence));
        self.outcomes
            .insert(id.to_string(), Outcome::Completed(sequence));
        if self.records >= COMPACT_AFTER_RECORDS {
            self.compact(context).await?;
        }
        Ok(())
    }

    async fn append(&mut self, context: Context, record: Record) -> Result<()> {
        let line = serde_json::to_string(&record)
            .map_err(|e| HarnessError::json(context, Some(self.path.clone()), e))?;
        let path = self.path.clone();
        let join = rusty_tokio::spawn_blocking(move || {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| HarnessError::io(context, Some(path.clone()), e))?;
            writeln!(file, "{line}")
                .map_err(|e| HarnessError::io(context, Some(path.clone()), e))?;
            // `sync_all`, not just `flush`: a journal that is only in the
            // OS page cache still answers the process-crash case this
            // project models, but the whole reason to have a journal
            // rather than a cache is to stop guessing about what survived.
            file.sync_all()
                .map_err(|e| HarnessError::io(context, Some(path), e))
        })
        .await;
        // `??`, not `?`: the outer `Result` is the join handle, the inner
        // one is the actual write/`sync_all`. Dropping the inner error
        // would let a failed fsync report success from a mechanism whose
        // entire job is knowing what reached disk -- caught here by
        // `unused_must_use` rather than by a lost record in production.
        join.map_err(|_| HarnessError::protocol(context, "request journal append task panicked"))??;
        self.records += 1;
        Ok(())
    }

    /// Rewrites the journal keeping only the most recent [`RETAINED_IDS`]
    /// ids, one record each.
    ///
    /// Write-temp-then-rename, so a crash mid-compaction leaves either
    /// the old complete journal or the new one, never a truncated file
    /// that would silently forget live ids. The containing directory is
    /// synced afterwards, since a rename is only durable once the
    /// *directory* entry is -- the same asymmetry upstream documents as a
    /// real gap between two of its own journals (`R-PROTO-17`), avoided
    /// here by simply doing it.
    async fn compact(&mut self, context: Context) -> Result<()> {
        while self.order.len() > RETAINED_IDS {
            if let Some(evicted) = self.order.pop_front() {
                self.outcomes.remove(&evicted);
            }
        }
        let mut lines = Vec::with_capacity(self.order.len());
        for id in &self.order {
            let record = match self.outcomes.get(id) {
                Some(Outcome::Completed(sequence)) => Record::Result {
                    v: 1,
                    request_id: id.clone(),
                    sequence: *sequence,
                },
                Some(Outcome::Uncertain) => Record::Begin {
                    v: 1,
                    request_id: id.clone(),
                    at: paths::now_ms(),
                },
                None => continue,
            };
            lines.push(
                serde_json::to_string(&record)
                    .map_err(|e| HarnessError::json(context, Some(self.path.clone()), e))?,
            );
        }
        let path = self.path.clone();
        let retained = lines.len();
        let join = rusty_tokio::spawn_blocking(move || {
            let tmp = path.with_extension("jsonl.compacting");
            {
                let mut file = std::fs::File::create(&tmp)
                    .map_err(|e| HarnessError::io(context, Some(tmp.clone()), e))?;
                for line in &lines {
                    writeln!(file, "{line}")
                        .map_err(|e| HarnessError::io(context, Some(tmp.clone()), e))?;
                }
                file.sync_all()
                    .map_err(|e| HarnessError::io(context, Some(tmp.clone()), e))?;
            }
            std::fs::rename(&tmp, &path)
                .map_err(|e| HarnessError::io(context, Some(path.clone()), e))?;
            if let Some(dir) = path.parent() {
                // Best-effort: not every platform lets a directory be
                // opened for sync, and failing the whole compaction over
                // it would be worse than the weaker guarantee.
                if let Ok(handle) = std::fs::File::open(dir) {
                    let _ = handle.sync_all();
                }
            }
            Ok::<_, HarnessError>(())
        })
        .await;
        join.map_err(|_| {
            HarnessError::protocol(context, "request journal compaction task panicked")
        })??;
        self.records = retained;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_session_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rpa-journal-{label}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[rusty_tokio::test]
    async fn an_unseen_id_is_unknown_and_a_completed_one_replays() {
        let dir = temp_session_dir("basic");
        let mut journal = RequestJournal::load(Context::Session, &dir).unwrap();
        assert_eq!(journal.lookup("req-1"), None);

        journal.begin(Context::Session, "req-1").await.unwrap();
        assert_eq!(journal.lookup("req-1"), Some(Outcome::Uncertain));

        journal
            .record_result(Context::Session, "req-1", 7)
            .await
            .unwrap();
        assert_eq!(journal.lookup("req-1"), Some(Outcome::Completed(7)));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[rusty_tokio::test]
    async fn a_begin_without_a_result_survives_a_reload_as_uncertain() {
        // The whole point: this is what a worker crash between dispatch
        // and completion looks like to the process that replaces it.
        let dir = temp_session_dir("uncertain");
        {
            let mut journal = RequestJournal::load(Context::Session, &dir).unwrap();
            journal
                .begin(Context::Session, "req-crashed")
                .await
                .unwrap();
            journal.begin(Context::Session, "req-done").await.unwrap();
            journal
                .record_result(Context::Session, "req-done", 3)
                .await
                .unwrap();
        }

        let reloaded = RequestJournal::load(Context::Session, &dir).unwrap();
        assert_eq!(
            reloaded.lookup("req-crashed"),
            Some(Outcome::Uncertain),
            "a dispatched-but-unfinished request must stay uncertain across a restart"
        );
        assert_eq!(
            reloaded.lookup("req-done"),
            Some(Outcome::Completed(3)),
            "a completed request must replay its result across a restart"
        );
        assert_eq!(reloaded.lookup("req-never-seen"), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[rusty_tokio::test]
    async fn a_corrupt_line_is_skipped_rather_than_failing_the_load() {
        let dir = temp_session_dir("corrupt");
        {
            let mut journal = RequestJournal::load(Context::Session, &dir).unwrap();
            journal.begin(Context::Session, "req-good").await.unwrap();
            journal
                .record_result(Context::Session, "req-good", 1)
                .await
                .unwrap();
        }
        // A half-written trailing line, exactly what a crash mid-append
        // leaves behind.
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(paths::request_journal_path(&dir))
                .unwrap();
            write!(f, "{{\"op\":\"resu").unwrap();
        }

        let reloaded = RequestJournal::load(Context::Session, &dir).unwrap();
        assert_eq!(
            reloaded.lookup("req-good"),
            Some(Outcome::Completed(1)),
            "a corrupt tail must not cost the records written before it"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[rusty_tokio::test]
    async fn compaction_bounds_the_file_and_keeps_the_most_recent_ids() {
        let dir = temp_session_dir("compact");
        let mut journal = RequestJournal::load(Context::Session, &dir).unwrap();
        // Two records per id, so this crosses COMPACT_AFTER_RECORDS.
        let ids = COMPACT_AFTER_RECORDS / 2 + RETAINED_IDS;
        for i in 0..ids {
            let id = format!("req-{i}");
            journal.begin(Context::Session, &id).await.unwrap();
            journal
                .record_result(Context::Session, &id, i as u64)
                .await
                .unwrap();
        }

        // Compaction trims to `RETAINED_IDS` *at the moment it runs*, and
        // the live set grows again until the next one -- so the honest
        // bound is "far below the number of ids ever seen", not
        // "`RETAINED_IDS` at every instant".
        assert!(
            journal.order.len() < ids,
            "compaction should have dropped ids, but all {ids} are still live"
        );
        assert!(
            journal.records < ids * 2,
            "the journal should have been rewritten, but still holds {} records for {ids} ids",
            journal.records
        );
        let newest = format!("req-{}", ids - 1);
        assert_eq!(
            journal.lookup(&newest),
            Some(Outcome::Completed((ids - 1) as u64)),
            "the most recent id must survive compaction"
        );
        assert_eq!(
            journal.lookup("req-0"),
            None,
            "the oldest id should have been evicted, behaving as never-seen"
        );

        // And the bound must survive a reload -- i.e. the file itself was
        // rewritten, not just the in-memory map trimmed.
        let reloaded = RequestJournal::load(Context::Session, &dir).unwrap();
        assert_eq!(
            reloaded.lookup(&newest),
            Some(Outcome::Completed((ids - 1) as u64))
        );
        assert_eq!(reloaded.lookup("req-0"), None);
        assert!(reloaded.order.len() < ids);

        std::fs::remove_dir_all(&dir).ok();
    }
}
