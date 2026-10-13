// SPDX-License-Identifier: GPL-3.0-only

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use super::*;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

pub(crate) struct Directory(PathBuf);

impl Directory {
    pub(crate) fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "datetime-agenda-test-{}-{stamp}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }

    pub(super) fn root(&self) -> &Path {
        &self.0
    }
    pub(crate) fn agenda(&self) -> PathBuf {
        self.0.join("private").join("agenda.json")
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0)
        .single()
        .unwrap()
}

pub(super) fn draft(kind: Kind) -> Draft {
    Draft {
        id: None,
        kind,
        title: "Private meeting".into(),
        start: now(),
        reminder_minutes: 0,
        completed: false,
    }
}

#[tokio::test]
async fn crud_round_trip_uses_private_files_and_never_reuses_deleted_ids() {
    let directory = Directory::new();
    let path = directory.agenda();
    assert_eq!(runtime::load_at(&path).await.unwrap(), Snapshot::default());
    assert!(!path.exists());
    let first = runtime::apply_at(path.clone(), 0, Change::Upsert(draft(Kind::Task)))
        .await
        .unwrap();
    let id = first.entries[0].id;
    let complete = runtime::apply_at(path.clone(), first.revision, Change::Complete(id, true))
        .await
        .unwrap();
    assert!(complete.entries[0].completed);
    let reopened = runtime::apply_at(path.clone(), complete.revision, Change::Complete(id, false))
        .await
        .unwrap();
    assert!(!reopened.entries[0].completed);
    let deleted = runtime::apply_at(path.clone(), reopened.revision, Change::Delete(id))
        .await
        .unwrap();
    assert!(deleted.entries.is_empty());
    let second = runtime::apply_at(
        path.clone(),
        deleted.revision,
        Change::Upsert(draft(Kind::Event)),
    )
    .await
    .unwrap();
    assert!(second.entries[0].id > id);
    assert_eq!(runtime::load_at(&path).await.unwrap(), second);
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(path.with_extension("lock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

#[tokio::test]
async fn competing_mutations_have_exactly_one_winner_and_preserve_both_revision_and_data() {
    let directory = Directory::new();
    let path = directory.agenda();
    let mut second = draft(Kind::Task);
    second.title = "Another task".into();
    let (first, second) = tokio::join!(
        runtime::apply_at(path.clone(), 0, Change::Upsert(draft(Kind::Event))),
        runtime::apply_at(path.clone(), 0, Change::Upsert(second)),
    );
    assert_ne!(first.is_ok(), second.is_ok());
    let saved = runtime::load_at(&path).await.unwrap();
    assert_eq!(saved.revision, 1);
    assert_eq!(saved.entries.len(), 1);
    assert_eq!(saved, first.or(second).unwrap());
}

#[tokio::test]
async fn malformed_unsupported_and_semantically_invalid_files_are_never_replaced() {
    let directory = Directory::new();
    let path = directory.agenda();
    storage::prepare_parent(&path).await.unwrap();
    let mut unsupported = Envelope::default();
    unsupported.version += 1;
    let mut duplicate = Envelope::default();
    duplicate
        .change(0, Change::Upsert(draft(Kind::Task)))
        .unwrap();
    duplicate
        .snapshot
        .entries
        .push(duplicate.snapshot.entries[0].clone());
    for bytes in [
        b"{ not JSON".to_vec(),
        serde_json::to_vec(&unsupported).unwrap(),
        serde_json::to_vec(&duplicate).unwrap(),
    ] {
        tokio::fs::write(&path, &bytes).await.unwrap();
        assert!(runtime::load_at(&path).await.is_err());
        assert!(
            runtime::apply_at(path.clone(), 0, Change::Upsert(draft(Kind::Event)))
                .await
                .is_err()
        );
        assert_eq!(tokio::fs::read(&path).await.unwrap(), bytes);
    }
}

#[tokio::test]
async fn invalid_changes_and_stale_revisions_leave_saved_bytes_unchanged() {
    let directory = Directory::new();
    let path = directory.agenda();
    let saved = runtime::apply_at(path.clone(), 0, Change::Upsert(draft(Kind::Event)))
        .await
        .unwrap();
    let bytes = tokio::fs::read(&path).await.unwrap();
    let mut invalid = draft(Kind::Task);
    invalid.reminder_minutes = MAX_REMINDER_MINUTES + 1;
    for (revision, change) in [
        (0, Change::Delete(saved.entries[0].id)),
        (saved.revision, Change::Complete(saved.entries[0].id, true)),
        (saved.revision, Change::Delete(u64::MAX)),
        (saved.revision, Change::Upsert(invalid)),
    ] {
        assert!(
            runtime::apply_at(path.clone(), revision, change)
                .await
                .is_err()
        );
        assert_eq!(tokio::fs::read(&path).await.unwrap(), bytes);
    }
}

#[test]
fn title_and_completion_edits_preserve_delivery_but_timing_changes_rearm() {
    let mut state = Envelope::default();
    state.change(0, Change::Upsert(draft(Kind::Task))).unwrap();
    let id = state.snapshot.entries[0].id;
    state.snapshot.entries[0].notified = Some(now());
    let mut edit = draft(Kind::Task);
    edit.id = Some(id);
    edit.title = "Renamed task".into();
    state.change(1, Change::Upsert(edit.clone())).unwrap();
    assert_eq!(state.snapshot.entries[0].notified, Some(now()));
    state.change(2, Change::Complete(id, true)).unwrap();
    state.change(3, Change::Complete(id, false)).unwrap();
    assert_eq!(state.snapshot.entries[0].notified, Some(now()));
    edit.start += TimeDelta::minutes(1);
    state.change(4, Change::Upsert(edit.clone())).unwrap();
    assert_eq!(state.snapshot.entries[0].notified, None);
    state.snapshot.entries[0].notified = Some(now());
    edit.reminder_minutes = 10;
    state.change(5, Change::Upsert(edit)).unwrap();
    assert_eq!(state.snapshot.entries[0].notified, None);
}

#[test]
fn unchanged_edits_do_not_advance_revision_or_rearm() {
    let mut state = Envelope::default();
    state.change(0, Change::Upsert(draft(Kind::Task))).unwrap();
    let mut edit = draft(Kind::Task);
    edit.id = Some(state.snapshot.entries[0].id);
    assert!(!state.change(1, Change::Upsert(edit)).unwrap());
    assert!(
        !state
            .change(1, Change::Complete(state.snapshot.entries[0].id, false))
            .unwrap()
    );
    assert_eq!(state.snapshot.revision, 1);
}

#[test]
fn deadline_selection_handles_advance_boundary_overdue_completed_and_notified_entries() {
    let mut state = Envelope::default();
    let mut later = draft(Kind::Event);
    later.start = now() + TimeDelta::hours(1);
    later.reminder_minutes = 30;
    state.change(0, Change::Upsert(later)).unwrap();
    state.change(1, Change::Upsert(draft(Kind::Task))).unwrap();
    assert_eq!(next_deadline(&state.snapshot), Some(now()));
    assert_eq!(
        due_entry(&state.snapshot, now() - TimeDelta::nanoseconds(1)),
        None
    );
    assert_eq!(due_entry(&state.snapshot, now()), Some(1));
    state
        .change(2, Change::Complete(state.snapshot.entries[1].id, true))
        .unwrap();
    assert_eq!(
        next_deadline(&state.snapshot),
        Some(now() + TimeDelta::minutes(30))
    );
    assert_eq!(
        due_entry(&state.snapshot, now() + TimeDelta::days(30)),
        Some(0)
    );
    state.snapshot.entries[0].notified = Some(now());
    assert_eq!(next_deadline(&state.snapshot), None);
    assert_eq!(
        due_entry(&state.snapshot, now() + TimeDelta::days(30)),
        None
    );
}

#[test]
fn dst_gap_is_rejected_and_fold_occurrences_are_ordered_without_environment_changes() {
    let chicago = chrono_tz::America::Chicago;
    let gap = NaiveDate::from_ymd_opt(2026, 3, 8).unwrap();
    let gap_time = NaiveTime::from_hms_opt(2, 30, 0).unwrap();
    assert!(resolve_in(&chicago, gap, gap_time, false).is_err());
    assert!(resolve_in(&chicago, gap, gap_time, true).is_err());
    let fold = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
    let fold_time = NaiveTime::from_hms_opt(1, 30, 0).unwrap();
    let first = resolve_in(&chicago, fold, fold_time, false).unwrap();
    let second = resolve_in(&chicago, fold, fold_time, true).unwrap();
    assert_eq!(
        first,
        Utc.with_ymd_and_hms(2026, 11, 1, 6, 30, 0)
            .single()
            .unwrap()
    );
    assert_eq!(second - first, TimeDelta::hours(1));
    let regular_time = NaiveTime::from_hms_opt(12, 0, 0).unwrap();
    assert_eq!(
        resolve_in(&chicago, fold, regular_time, false),
        resolve_in(&chicago, fold, regular_time, true)
    );
}

#[test]
fn validation_bounds_prevent_overflow_and_fit_the_storage_limit() {
    let mut state = Envelope::default();
    let mut input = draft(Kind::Task);
    input.title = "🗓".repeat(MAX_TITLE_CHARS);
    input.reminder_minutes = MAX_REMINDER_MINUTES;
    for _ in 0..MAX_ENTRIES {
        state
            .change(state.snapshot.revision, Change::Upsert(input.clone()))
            .unwrap();
    }
    state.validate().unwrap();
    assert!(serde_json::to_vec_pretty(&state).unwrap().len() < 1024 * 1024);
    assert!(
        state
            .change(state.snapshot.revision, Change::Upsert(input))
            .is_err()
    );
    assert!(validate_fields("valid", Kind::Task, DateTime::<Utc>::MIN_UTC, 1, false).is_err());
    assert!(validate_fields("\n", Kind::Task, now(), 0, false).is_err());
    assert!(
        validate_fields(
            &"a".repeat(MAX_TITLE_CHARS + 1),
            Kind::Task,
            now(),
            0,
            false
        )
        .is_err()
    );
}

#[test]
fn exhausted_counters_fail_before_changing_existing_state() {
    let mut state = Envelope::default();
    state.snapshot.revision = u64::MAX;
    let original = state.clone();
    assert!(
        state
            .change(u64::MAX, Change::Upsert(draft(Kind::Task)))
            .is_err()
    );
    assert_eq!(state, original);
    state.snapshot.revision = 0;
    state.next_id = u64::MAX;
    let original = state.clone();
    assert!(state.change(0, Change::Upsert(draft(Kind::Task))).is_err());
    assert_eq!(state, original);
}

#[test]
fn diagnostic_debug_redacts_private_titles_through_changes_and_loaded_events() {
    let mut state = Envelope::default();
    let input = draft(Kind::Task);
    state.change(0, Change::Upsert(input.clone())).unwrap();
    assert!(!format!("{:?}", Change::Upsert(input)).contains("Private meeting"));
    assert!(!format!("{:?}", Event::Loaded(state.snapshot)).contains("Private meeting"));
}

#[test]
fn legacy_delivered_history_defaults_to_resolved_without_reviving_completed_tasks_or_events() {
    let mut state = Envelope::default();
    state.change(0, Change::Upsert(draft(Kind::Task))).unwrap();
    state.change(1, Change::Upsert(draft(Kind::Event))).unwrap();
    for entry in &mut state.snapshot.entries {
        entry.notified = Some(now());
    }
    state.snapshot.entries[0].completed = true;
    let mut legacy = serde_json::to_value(&state).unwrap();
    for entry in legacy["snapshot"]["entries"].as_array_mut().unwrap() {
        let entry = entry.as_object_mut().unwrap();
        entry.remove("snoozed_until");
        entry.remove("reminder_pending");
    }
    let migrated = decode_envelope(&serde_json::to_vec(&legacy).unwrap()).unwrap();
    assert_eq!(migrated, state);
    assert!(
        migrated
            .snapshot
            .entries
            .iter()
            .all(|entry| !entry.needs_window(now()))
    );
}

#[test]
fn snooze_changes_only_reminder_deadline_and_title_edits_preserve_it() {
    let mut state = Envelope::default();
    state.change(0, Change::Upsert(draft(Kind::Task))).unwrap();
    let id = state.snapshot.entries[0].id;
    state.snapshot.entries[0].notified = Some(now());
    state.snapshot.entries[0].reminder_pending = true;
    let until = DateTime::<Utc>::MAX_UTC;
    state.change(1, Change::Snooze(id, until)).unwrap();
    let entry = &state.snapshot.entries[0];
    assert_eq!(entry.start, now());
    assert_eq!(entry.reminder_at(), until);
    assert_eq!(entry.notified, None);
    assert!(!entry.reminder_pending);
    let mut edit = draft(Kind::Task);
    edit.id = Some(id);
    edit.title = "Renamed while snoozed".into();
    state.change(2, Change::Upsert(edit.clone())).unwrap();
    assert_eq!(state.snapshot.entries[0].snoozed_until, Some(until));
    edit.start += TimeDelta::hours(1);
    state.change(3, Change::Upsert(edit)).unwrap();
    assert_eq!(state.snapshot.entries[0].snoozed_until, None);
    assert_eq!(
        state.snapshot.entries[0].reminder_at(),
        now() + TimeDelta::hours(1)
    );
}

#[test]
fn finishing_legacy_events_retains_data_and_does_not_use_task_completion() {
    let mut state = Envelope::default();
    state.change(0, Change::Upsert(draft(Kind::Event))).unwrap();
    let id = state.snapshot.entries[0].id;
    state.snapshot.entries[0].notified = Some(now());
    state.snapshot.entries[0].reminder_pending = true;
    state.change(1, Change::Finish(id)).unwrap();
    assert_eq!(state.snapshot.entries.len(), 1);
    assert!(!state.snapshot.entries[0].completed);
    assert!(!state.snapshot.entries[0].reminder_pending);
    assert_eq!(state.snapshot.entries[0].notified, Some(now()));
    assert!(!state.snapshot.entries[0].needs_window(now() + TimeDelta::days(1)));
    assert!(!state.snapshot.entries[0].pending());
}

#[test]
fn finishing_tasks_clears_pending_and_completed_history_does_not_restore() {
    let mut state = Envelope::default();
    state.change(0, Change::Upsert(draft(Kind::Task))).unwrap();
    let id = state.snapshot.entries[0].id;
    state.snapshot.entries[0].notified = Some(now());
    state.snapshot.entries[0].reminder_pending = true;
    state.change(1, Change::Finish(id)).unwrap();
    assert!(state.snapshot.entries[0].completed);
    assert!(!state.snapshot.entries[0].reminder_pending);
    state.change(2, Change::Complete(id, false)).unwrap();
    assert!(!state.snapshot.entries[0].needs_window(now()));
    assert_eq!(due_entry(&state.snapshot, now()), None);
}

#[test]
fn stale_finish_and_past_snooze_preserve_unresolved_state() {
    let mut state = Envelope::default();
    state.change(0, Change::Upsert(draft(Kind::Task))).unwrap();
    let id = state.snapshot.entries[0].id;
    state.snapshot.entries[0].notified = Some(now());
    state.snapshot.entries[0].reminder_pending = true;
    let original = state.clone();
    assert!(state.change(0, Change::Finish(id)).is_err());
    assert_eq!(state, original);
    assert!(
        state
            .change(1, Change::Snooze(id, DateTime::<Utc>::MIN_UTC))
            .is_err()
    );
    assert_eq!(state, original);
}
