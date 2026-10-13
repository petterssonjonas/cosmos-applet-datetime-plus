// SPDX-License-Identifier: GPL-3.0-only
//! Private local agenda storage and the always-live reminder subscription.
//!
//! Reminder windows acknowledge native creation before delivery is persisted. Explicit
//! unresolved state survives restart; a separate per-entry lock deduplicates windows
//! even if a process stops between the window acknowledgement and the atomic save.

mod runtime;
#[cfg(test)]
pub(crate) mod tests;

use std::{collections::HashSet, fmt, path::Path};

use chrono::{DateTime, Local, LocalResult, NaiveDate, NaiveTime, TimeDelta, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::storage;

pub use runtime::{read_only_subscription, subscription};

const VERSION: u32 = 1;
const MAX_ENTRIES: usize = 512;
const MAX_TITLE_CHARS: usize = 160;
const MAX_REMINDER_MINUTES: u32 = 365 * 24 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Event,
    Task,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: u64,
    pub kind: Kind,
    pub title: String,
    pub start: DateTime<Utc>,
    pub reminder_minutes: u32,
    pub completed: bool,
    pub notified: Option<DateTime<Utc>>,
    #[serde(default)]
    pub snoozed_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub reminder_pending: bool,
}

// Loaded snapshots also appear in the app's diagnostic message formatting.
impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Entry")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("title", &"[private]")
            .field("start", &self.start)
            .field("reminder_minutes", &self.reminder_minutes)
            .field("completed", &self.completed)
            .field("notified", &self.notified)
            .field("snoozed_until", &self.snoozed_until)
            .field("reminder_pending", &self.reminder_pending)
            .finish()
    }
}

impl Entry {
    /// Validated entries always have a representable reminder deadline.
    pub fn reminder_at(&self) -> DateTime<Utc> {
        self.snoozed_until
            .unwrap_or_else(|| self.start - TimeDelta::minutes(i64::from(self.reminder_minutes)))
    }

    fn pending(&self) -> bool {
        self.notified.is_none() && !(self.kind == Kind::Task && self.completed)
    }

    /// Includes an unacknowledged due window during its delivery transaction.
    pub(crate) fn needs_window(&self, now: DateTime<Utc>) -> bool {
        !(self.kind == Kind::Task && self.completed)
            && (self.reminder_pending || (self.notified.is_none() && self.reminder_at() <= now))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub revision: u64,
    pub entries: Vec<Entry>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Draft {
    pub id: Option<u64>,
    pub kind: Kind,
    pub title: String,
    pub start: DateTime<Utc>,
    pub reminder_minutes: u32,
    pub completed: bool,
}

impl fmt::Debug for Draft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Draft")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("title", &"[private]")
            .field("start", &self.start)
            .field("reminder_minutes", &self.reminder_minutes)
            .field("completed", &self.completed)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Upsert(Draft),
    Delete(u64),
    Complete(u64, bool),
    Snooze(u64, DateTime<Utc>),
    /// Resolve a reminder without deleting a legacy event.
    Finish(u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Loaded(Snapshot),
    Error(String),
    /// The agenda entry ID whose native reminder window acknowledged creation.
    Delivered(u64),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u32,
    next_id: u64,
    snapshot: Snapshot,
}

impl Default for Envelope {
    fn default() -> Self {
        Self {
            version: VERSION,
            next_id: 1,
            snapshot: Snapshot::default(),
        }
    }
}

impl Envelope {
    fn validate(&self) -> Result<(), String> {
        if self.version != VERSION {
            return Err(format!(
                "Unsupported agenda file version {}. Saved data was not changed.",
                self.version
            ));
        }
        if self.snapshot.entries.len() > MAX_ENTRIES {
            return Err(format!(
                "The agenda can contain at most {MAX_ENTRIES} entries."
            ));
        }
        let mut ids = HashSet::with_capacity(self.snapshot.entries.len());
        for entry in &self.snapshot.entries {
            if entry.id == 0 || !ids.insert(entry.id) || entry.id >= self.next_id {
                return Err("The agenda contains invalid or duplicate entry IDs. Saved data was not changed.".into());
            }
            validate_fields(
                &entry.title,
                entry.kind,
                entry.start,
                entry.reminder_minutes,
                entry.completed,
            )?;
        }
        if self.next_id == 0 {
            return Err("The agenda ID counter is invalid. Saved data was not changed.".into());
        }
        Ok(())
    }

    fn change(&mut self, expected_revision: u64, change: Change) -> Result<bool, String> {
        if self.snapshot.revision != expected_revision {
            return Err("The agenda changed in another window or a reminder was delivered. Review the latest entries and save again.".into());
        }
        // Check exhaustion before changing any owned state.
        let next_revision = self
            .snapshot
            .revision
            .checked_add(1)
            .ok_or("The agenda revision counter is exhausted. Saved data was not changed.")?;
        match change {
            Change::Upsert(mut draft) => {
                if draft.title.trim().len() != draft.title.len() {
                    draft.title = draft.title.trim().to_owned();
                }
                validate_fields(
                    &draft.title,
                    draft.kind,
                    draft.start,
                    draft.reminder_minutes,
                    draft.completed,
                )?;
                if let Some(id) = draft.id {
                    let entry = self
                        .snapshot
                        .entries
                        .iter_mut()
                        .find(|entry| entry.id == id)
                        .ok_or("The agenda entry no longer exists.")?;
                    let timing_changed = entry.start != draft.start
                        || entry.reminder_minutes != draft.reminder_minutes;
                    if entry.kind == draft.kind
                        && entry.title == draft.title
                        && !timing_changed
                        && entry.completed == draft.completed
                    {
                        return Ok(false);
                    }
                    entry.kind = draft.kind;
                    entry.title = draft.title;
                    entry.start = draft.start;
                    entry.reminder_minutes = draft.reminder_minutes;
                    entry.completed = draft.completed;
                    if timing_changed {
                        entry.snoozed_until = None;
                        entry.reminder_pending = false;
                        entry.notified = None;
                    }
                    if draft.completed {
                        entry.reminder_pending = false;
                    }
                } else {
                    if self.snapshot.entries.len() >= MAX_ENTRIES {
                        return Err(format!(
                            "The agenda can contain at most {MAX_ENTRIES} entries."
                        ));
                    }
                    let next_id = self
                        .next_id
                        .checked_add(1)
                        .ok_or("The agenda ID counter is exhausted. Saved data was not changed.")?;
                    self.snapshot.entries.push(Entry {
                        id: self.next_id,
                        kind: draft.kind,
                        title: draft.title,
                        start: draft.start,
                        reminder_minutes: draft.reminder_minutes,
                        completed: draft.completed,
                        notified: None,
                        snoozed_until: None,
                        reminder_pending: false,
                    });
                    self.next_id = next_id;
                }
            }
            Change::Delete(id) => {
                let position = self
                    .snapshot
                    .entries
                    .iter()
                    .position(|entry| entry.id == id)
                    .ok_or("The agenda entry no longer exists.")?;
                self.snapshot.entries.remove(position);
            }
            Change::Complete(id, completed) => {
                let entry = self
                    .snapshot
                    .entries
                    .iter_mut()
                    .find(|entry| entry.id == id)
                    .ok_or("The agenda entry no longer exists.")?;
                if entry.kind != Kind::Task {
                    return Err("Only tasks can be marked complete.".into());
                }
                if entry.completed == completed {
                    return Ok(false);
                }
                // Reopening an already-notified task does not deliver it a second time.
                entry.completed = completed;
                if completed {
                    entry.reminder_pending = false;
                }
            }
            Change::Snooze(id, until) => {
                if until <= Utc::now() {
                    return Err("Choose a snooze time in the future.".into());
                }
                let entry = self
                    .snapshot
                    .entries
                    .iter_mut()
                    .find(|entry| entry.id == id)
                    .ok_or("The agenda entry no longer exists.")?;
                if entry.kind == Kind::Task && entry.completed {
                    return Err("A completed task cannot be snoozed.".into());
                }
                entry.snoozed_until = Some(until);
                entry.notified = None;
                entry.reminder_pending = false;
            }
            Change::Finish(id) => {
                let Some(entry) = self
                    .snapshot
                    .entries
                    .iter_mut()
                    .find(|entry| entry.id == id)
                else {
                    return Ok(false);
                };
                if !entry.reminder_pending {
                    // A failed directory sync may already have made resolution visible.
                    // The runtime still persists this confirmation before reporting success.
                    return Ok(false);
                }
                entry.reminder_pending = false;
                if entry.kind == Kind::Task {
                    entry.completed = true;
                }
            }
        }
        self.snapshot.revision = next_revision;
        Ok(true)
    }
}

fn validate_fields(
    title: &str,
    kind: Kind,
    start: DateTime<Utc>,
    minutes: u32,
    completed: bool,
) -> Result<(), String> {
    if title.trim().is_empty()
        || title.chars().count() > MAX_TITLE_CHARS
        || title.chars().any(char::is_control)
    {
        return Err(format!(
            "Enter a title of 1–{MAX_TITLE_CHARS} characters without control characters."
        ));
    }
    if minutes > MAX_REMINDER_MINUTES {
        return Err("Reminder advance must be between 0 and 525600 minutes (365 days).".into());
    }
    if start
        .checked_sub_signed(TimeDelta::minutes(i64::from(minutes)))
        .is_none()
    {
        return Err("The reminder time is outside the supported date range.".into());
    }
    if kind == Kind::Event && completed {
        return Err("Only tasks can be marked complete.".into());
    }
    Ok(())
}

async fn read_envelope(path: &Path) -> Result<Envelope, String> {
    let Some(bytes) = storage::read(path).await? else {
        return Ok(Envelope::default());
    };
    decode_envelope(&bytes)
}

fn decode_envelope(bytes: &[u8]) -> Result<Envelope, String> {
    let state: Envelope = serde_json::from_slice(bytes).map_err(|error| {
        format!(
            "Could not read agenda JSON at line {}, column {}. Saved data was not changed.",
            error.line(),
            error.column()
        )
    })?;
    state.validate()?;
    Ok(state)
}

async fn read_saved_envelope(path: &Path) -> Result<Envelope, String> {
    let bytes = storage::read(path).await?
        .ok_or("The agenda file is temporarily missing. The sent reminder is retained until safe persistence is possible.")?;
    decode_envelope(&bytes)
}

pub async fn load() -> Result<Snapshot, String> {
    let path = storage::data_path("agenda.json")?;
    runtime::load_at(&path).await
}

pub async fn apply(expected_revision: u64, change: Change) -> Result<Snapshot, String> {
    let path = storage::data_path("agenda.json")?;
    runtime::apply_at(path, expected_revision, change).await
}

/// Resolve the system-local wall time, rejecting DST gaps and choosing folds explicitly.
pub fn resolve_local(
    date: NaiveDate,
    time: NaiveTime,
    second_occurrence: bool,
) -> Result<DateTime<Utc>, String> {
    resolve_in(&Local, date, time, second_occurrence)
}

fn resolve_in<T: TimeZone>(
    zone: &T,
    date: NaiveDate,
    time: NaiveTime,
    second_occurrence: bool,
) -> Result<DateTime<Utc>, String> {
    match zone.from_local_datetime(&date.and_time(time)) {
        LocalResult::Single(value) => Ok(value.with_timezone(&Utc)),
        LocalResult::Ambiguous(first, second) => {
            let first = first.with_timezone(&Utc);
            let second = second.with_timezone(&Utc);
            Ok(if second_occurrence { first.max(second) } else { first.min(second) })
        }
        LocalResult::None => Err("This local time does not exist because of a daylight-saving transition. Choose another time.".into()),
    }
}

fn next_deadline(snapshot: &Snapshot) -> Option<DateTime<Utc>> {
    snapshot
        .entries
        .iter()
        .filter(|entry| entry.pending())
        .map(Entry::reminder_at)
        .min()
}

fn due_entry(snapshot: &Snapshot, now: DateTime<Utc>) -> Option<usize> {
    snapshot
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.pending() && entry.reminder_at() <= now)
        .min_by_key(|(_, entry)| (entry.reminder_at(), entry.id))
        .map(|(index, _)| index)
}
