// SPDX-License-Identifier: GPL-3.0-only

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use super::*;
use crate::agenda::{
    Draft,
    tests::{Directory, draft, now},
};

fn reporter() -> Reporter {
    reports::channel(16).0
}

async fn send_count(counter: Arc<AtomicUsize>, _id: u64) -> Result<(), String> {
    counter.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

#[tokio::test]
async fn overdue_reminder_persists_once_across_independent_runtime_passes() {
    let directory = Directory::new();
    let path = directory.agenda();
    let initial = apply_at(path.clone(), 0, Change::Upsert(draft(Kind::Event)))
        .await
        .unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let overdue = now() + TimeDelta::days(7);
    let first = deliver_with(path.clone(), overdue, reporter(), Duration::ZERO, |entry| {
        send_count(counter.clone(), entry)
    })
    .await
    .unwrap();
    assert_eq!(first.delivered, Some(initial.entries[0].id));
    assert_eq!(first.snapshot.entries[0].notified, Some(overdue));
    assert_eq!(first.snapshot.revision, initial.revision + 1);
    let restart = deliver_with(
        path.clone(),
        overdue + TimeDelta::days(1),
        reporter(),
        Duration::ZERO,
        |entry| send_count(counter.clone(), entry),
    )
    .await
    .unwrap();
    assert_eq!(restart.delivered, None);
    assert_eq!(restart.snapshot, load_at(&path).await.unwrap());
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn transport_failure_keeps_reminder_pending_until_one_successful_delivery() {
    let directory = Directory::new();
    let path = directory.agenda();
    let initial = apply_at(path.clone(), 0, Change::Upsert(draft(Kind::Task)))
        .await
        .unwrap();
    let bytes = tokio::fs::read(&path).await.unwrap();
    let failed = deliver_with(path.clone(), now(), reporter(), Duration::ZERO, |_| async {
        Err("isolated transport failure".into())
    })
    .await;
    assert!(failed.is_err());
    assert_eq!(tokio::fs::read(&path).await.unwrap(), bytes);
    assert_eq!(load_at(&path).await.unwrap(), initial);
    let counter = Arc::new(AtomicUsize::new(0));
    let success = deliver_with(path.clone(), now(), reporter(), Duration::ZERO, |entry| {
        send_count(counter.clone(), entry)
    })
    .await
    .unwrap();
    assert!(success.snapshot.entries[0].notified.is_some());
    let repeat = deliver_with(path, now(), reporter(), Duration::ZERO, |entry| {
        send_count(counter.clone(), entry)
    })
    .await
    .unwrap();
    assert_eq!(repeat.delivered, None);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn completed_deleted_and_future_entries_are_not_sent() {
    let directory = Directory::new();
    let path = directory.agenda();
    let mut completed = draft(Kind::Task);
    completed.completed = true;
    let first = apply_at(path.clone(), 0, Change::Upsert(completed))
        .await
        .unwrap();
    let second = apply_at(
        path.clone(),
        first.revision,
        Change::Upsert(draft(Kind::Event)),
    )
    .await
    .unwrap();
    let deleted_id = second.entries[1].id;
    let third = apply_at(path.clone(), second.revision, Change::Delete(deleted_id))
        .await
        .unwrap();
    let mut future = draft(Kind::Event);
    future.start = now() + TimeDelta::hours(1);
    apply_at(path.clone(), third.revision, Change::Upsert(future))
        .await
        .unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let pass = deliver_with(path, now(), reporter(), Duration::ZERO, |entry| {
        send_count(counter.clone(), entry)
    })
    .await
    .unwrap();
    assert_eq!(pass.delivered, None);
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn title_edit_does_not_repeat_but_new_timing_does() {
    let directory = Directory::new();
    let path = directory.agenda();
    apply_at(path.clone(), 0, Change::Upsert(draft(Kind::Task)))
        .await
        .unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let first = deliver_with(path.clone(), now(), reporter(), Duration::ZERO, |entry| {
        send_count(counter.clone(), entry)
    })
    .await
    .unwrap();
    let mut edit = Draft {
        id: Some(first.snapshot.entries[0].id),
        title: "New title".into(),
        ..draft(Kind::Task)
    };
    let renamed = apply_at(
        path.clone(),
        first.snapshot.revision,
        Change::Upsert(edit.clone()),
    )
    .await
    .unwrap();
    let no_repeat = deliver_with(path.clone(), now(), reporter(), Duration::ZERO, |entry| {
        send_count(counter.clone(), entry)
    })
    .await
    .unwrap();
    assert_eq!(no_repeat.delivered, None);
    edit.reminder_minutes = 10;
    apply_at(path.clone(), renamed.revision, Change::Upsert(edit))
        .await
        .unwrap();
    let rearmed = deliver_with(path, now(), reporter(), Duration::ZERO, |entry| {
        send_count(counter.clone(), entry)
    })
    .await
    .unwrap();
    assert!(rearmed.delivered.is_some());
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn competing_deliveries_use_independent_file_descriptors_and_only_send_once() {
    let directory = Directory::new();
    let path = directory.agenda();
    apply_at(path.clone(), 0, Change::Upsert(draft(Kind::Event)))
        .await
        .unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let first_path = path.clone();
    let first_counter = counter.clone();
    let first = tokio::spawn(async move {
        let mut started = Some(started_tx);
        let mut release = Some(release_rx);
        deliver_with(first_path, now(), reporter(), Duration::ZERO, move |_| {
            let started = started.take().unwrap();
            let release = release.take().unwrap();
            let counter = first_counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                started.send(()).unwrap();
                release.await.unwrap();
                Ok(())
            }
        })
        .await
    });
    started_rx.await.unwrap();
    let second_counter = counter.clone();
    let second = tokio::spawn(async move {
        deliver_with(path, now(), reporter(), Duration::ZERO, |entry| {
            send_count(second_counter.clone(), entry)
        })
        .await
    });
    release_tx.send(()).unwrap();
    let (first, second) = tokio::join!(first, second);
    assert!(first.unwrap().unwrap().delivered.is_some());
    assert!(second.unwrap().unwrap().delivered.is_none());
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn persistence_failure_retains_receipt_and_lock_and_retries_only_storage() {
    let directory = Directory::new();
    let path = directory.agenda();
    apply_at(path.clone(), 0, Change::Upsert(draft(Kind::Event)))
        .await
        .unwrap();
    let saved_bytes = tokio::fs::read(&path).await.unwrap();
    let parent = path.parent().unwrap().to_path_buf();
    let parked = directory.root().join("parked");
    let counter = Arc::new(AtomicUsize::new(0));
    let (sender, mut errors) = reports::channel(16);
    let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
    let job_counter = counter.clone();
    let job_path = path.clone();
    let job_parent = parent.clone();
    let job_parked = parked.clone();
    let job = tokio::spawn(async move {
        let result = deliver_with(job_path, now(), sender, Duration::ZERO, move |_| {
            let parent = job_parent.clone();
            let parked = job_parked.clone();
            let counter = job_counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                // A deterministic real filesystem failure, independent of UID/permissions.
                tokio::fs::rename(&parent, &parked).await.unwrap();
                tokio::fs::write(&parent, b"temporary obstruction")
                    .await
                    .unwrap();
                Ok(())
            }
        })
        .await;
        completed_tx
            .send(result)
            .unwrap_or_else(|_| panic!("The completion receiver disappeared"));
    });
    let error = timeout(Duration::from_secs(5), errors.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(error.contains("reminder was sent"));
    let competing = OpenOptions::new()
        .read(true)
        .write(true)
        .open(parked.join("agenda.lock"))
        .unwrap();
    // SAFETY: the independent descriptor is live and the operation is nonblocking.
    assert_eq!(
        unsafe { libc::flock(competing.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        -1
    );
    assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
    // Detaching the monitor's task handle must not drop the successful receipt or lock.
    drop(job);
    let corrupt = b"{ preserved corrupt agenda";
    tokio::fs::write(parked.join("agenda.json"), corrupt)
        .await
        .unwrap();
    tokio::fs::remove_file(&parent).await.unwrap();
    tokio::fs::rename(&parked, &parent).await.unwrap();
    timeout(Duration::from_secs(5), async {
        while !errors.recv().await.unwrap().contains("agenda JSON") {}
    })
    .await
    .unwrap();
    assert_eq!(tokio::fs::read(&path).await.unwrap(), corrupt);
    // SAFETY: this independent descriptor remains live and the retained transaction owns its lock.
    assert_eq!(
        unsafe { libc::flock(competing.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        -1
    );
    tokio::fs::write(&path, saved_bytes).await.unwrap();
    let pass = timeout(Duration::from_secs(5), completed_rx)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(pass.snapshot.entries[0].notified.is_some());
    assert_eq!(load_at(&path).await.unwrap(), pass.snapshot);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    // SAFETY: the descriptor is still owned and a completed transaction has released its lock.
    assert_eq!(
        unsafe { libc::flock(competing.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
}

#[tokio::test]
async fn watch_observes_atomic_agenda_replacement_without_lock_or_temporary_noise() {
    let directory = Directory::new();
    let path = directory.agenda();
    let (_watcher, mut changes) = watch(&path).await.unwrap();
    let snapshot = apply_at(path.clone(), 0, Change::Upsert(draft(Kind::Task)))
        .await
        .unwrap();
    timeout(Duration::from_secs(5), changes.changed())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(*changes.borrow_and_update(), Wake::Changed));
    assert_eq!(load_at(&path).await.unwrap(), snapshot);
}

#[tokio::test]
async fn realtime_deadline_can_be_disarmed_rearmed_and_expired_without_waiting_for_future_time() {
    let timer = Deadline::new().unwrap();
    timer.arm(None).unwrap();
    timer.arm(Some(Utc::now() - TimeDelta::days(1))).unwrap();
    timeout(Duration::from_secs(5), timer.elapsed())
        .await
        .unwrap()
        .unwrap();
    timer.arm(None).unwrap();
    timer.arm(Some(Utc::now() - TimeDelta::days(1))).unwrap();
    timeout(Duration::from_secs(5), timer.elapsed())
        .await
        .unwrap()
        .unwrap();
}

#[test]
fn failed_reminder_startup_does_not_starve_other_due_or_restored_reminders() {
    let mut state = Envelope::default();
    state.change(0, Change::Upsert(draft(Kind::Task))).unwrap();
    state.change(1, Change::Upsert(draft(Kind::Event))).unwrap();
    let first = state.snapshot.entries[0].id;
    let second = state.snapshot.entries[1].id;
    let presented = HashSet::new();
    let blocked = HashMap::from([(first, now() + TimeDelta::seconds(30))]);
    assert_eq!(
        ready_window(&state.snapshot, now(), &presented, &blocked),
        Some((second, false))
    );
    state.snapshot.entries[1].notified = Some(now());
    state.snapshot.entries[1].reminder_pending = true;
    assert_eq!(
        ready_window(&state.snapshot, now(), &presented, &blocked),
        Some((second, true))
    );
    let presented = HashSet::from([second]);
    assert_eq!(
        window_deadline(&state.snapshot, now(), &presented, &blocked),
        Some(now() + TimeDelta::seconds(30))
    );
}

#[tokio::test]
async fn unchanged_snapshots_do_not_refresh_ui_but_recovery_is_reported_once() {
    use futures_util::{FutureExt, StreamExt};
    let (mut output, mut receiver) = mpsc::channel(4);
    let mut published = None;
    let snapshot = Snapshot::default();
    assert!(publish(&mut output, &mut published, snapshot.clone(), false).await);
    assert_eq!(receiver.next().await, Some(Event::Loaded(snapshot.clone())));
    assert!(publish(&mut output, &mut published, snapshot.clone(), false).await);
    assert!(receiver.next().now_or_never().is_none());
    assert!(publish(&mut output, &mut published, snapshot.clone(), true).await);
    assert_eq!(receiver.next().await, Some(Event::Loaded(snapshot)));
}

#[tokio::test]
async fn pending_window_restores_after_restart_without_changing_delivery_or_revision() {
    let directory = Directory::new();
    let path = directory.agenda();
    apply_at(path.clone(), 0, Change::Upsert(draft(Kind::Event)))
        .await
        .unwrap();
    let first = deliver_with(path.clone(), now(), reporter(), Duration::ZERO, |_| async {
        Ok(())
    })
    .await
    .unwrap();
    assert!(first.snapshot.entries[0].reminder_pending);
    let counter = Arc::new(AtomicUsize::new(0));
    let id = first.snapshot.entries[0].id;
    let restored = restore_pending(path.clone(), id, |entry| send_count(counter.clone(), entry))
        .await
        .unwrap();
    assert_eq!(restored.presented, Some(id));
    assert_eq!(restored.delivered, None);
    assert_eq!(restored.snapshot, first.snapshot);
    assert_eq!(load_at(&path).await.unwrap(), first.snapshot);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failed_restart_restoration_preserves_pending_bytes_until_recovery() {
    let directory = Directory::new();
    let path = directory.agenda();
    apply_at(path.clone(), 0, Change::Upsert(draft(Kind::Task)))
        .await
        .unwrap();
    let first = deliver_with(path.clone(), now(), reporter(), Duration::ZERO, |_| async {
        Ok(())
    })
    .await
    .unwrap();
    let bytes = tokio::fs::read(&path).await.unwrap();
    let result = restore_pending(path.clone(), first.snapshot.entries[0].id, |_| async {
        Err("isolated native window startup failure".into())
    })
    .await;
    assert!(result.is_err());
    assert_eq!(tokio::fs::read(&path).await.unwrap(), bytes);
    assert!(load_at(&path).await.unwrap().entries[0].reminder_pending);
}

#[tokio::test]
async fn old_delivered_but_resolved_history_is_not_restored() {
    let directory = Directory::new();
    let path = directory.agenda();
    apply_at(path.clone(), 0, Change::Upsert(draft(Kind::Event)))
        .await
        .unwrap();
    let first = deliver_with(path.clone(), now(), reporter(), Duration::ZERO, |_| async {
        Ok(())
    })
    .await
    .unwrap();
    let id = first.snapshot.entries[0].id;
    let finished = apply_at(path.clone(), first.snapshot.revision, Change::Finish(id))
        .await
        .unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let pass = restore_pending(path, id, |entry| send_count(counter.clone(), entry))
        .await
        .unwrap();
    assert_eq!(pass.snapshot, finished);
    assert_eq!(pass.presented, None);
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_connected_transport_without_window_acknowledgement_does_not_mark_delivery() {
    let directory = Directory::new();
    let path = directory.agenda();
    let saved = apply_at(path.clone(), 0, Change::Upsert(draft(Kind::Task)))
        .await
        .unwrap();
    let (connected, wait_connected) = tokio::sync::oneshot::channel();
    let (ready, wait_ready) = tokio::sync::oneshot::channel();
    let job_path = path.clone();
    let job = tokio::spawn(async move {
        let mut connected = Some(connected);
        let mut wait_ready = Some(wait_ready);
        deliver_with(job_path, now(), reporter(), Duration::ZERO, move |_| {
            let connected = connected.take().unwrap();
            let wait_ready = wait_ready.take().unwrap();
            async move {
                connected.send(()).unwrap();
                wait_ready.await.unwrap();
                Ok(())
            }
        })
        .await
    });
    wait_connected.await.unwrap();
    assert_eq!(load_at(&path).await.unwrap(), saved);
    assert!(!job.is_finished());
    ready.send(()).unwrap();
    let delivered = job.await.unwrap().unwrap();
    assert!(delivered.snapshot.entries[0].reminder_pending);
    assert!(delivered.snapshot.entries[0].notified.is_some());
}

#[tokio::test]
async fn finishing_again_safely_persists_visible_resolution_without_reopening_history() {
    let directory = Directory::new();
    let path = directory.agenda();
    apply_at(path.clone(), 0, Change::Upsert(draft(Kind::Event)))
        .await
        .unwrap();
    let delivered = deliver_with(path.clone(), now(), reporter(), Duration::ZERO, |_| async {
        Ok(())
    })
    .await
    .unwrap();
    let id = delivered.snapshot.entries[0].id;
    let resolved = apply_at(
        path.clone(),
        delivered.snapshot.revision,
        Change::Finish(id),
    )
    .await
    .unwrap();
    let confirmed = apply_at(path.clone(), resolved.revision, Change::Finish(id))
        .await
        .unwrap();
    assert_eq!(confirmed, resolved);
    assert_eq!(load_at(&path).await.unwrap(), resolved);
    assert_eq!(due_entry(&confirmed, now()), None);
    let deleted = apply_at(path.clone(), confirmed.revision, Change::Delete(id))
        .await
        .unwrap();
    assert_eq!(
        apply_at(path, deleted.revision, Change::Finish(id))
            .await
            .unwrap(),
        deleted
    );
}
