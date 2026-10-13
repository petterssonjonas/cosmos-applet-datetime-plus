// SPDX-License-Identifier: GPL-3.0-only

use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    future::Future,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    time::Duration,
};

use crate::notifier;
use chrono::{DateTime, TimeDelta, Utc};
use cosmic::iced::{Subscription, futures::channel::mpsc, stream};
use futures_util::SinkExt;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::{
    io::unix::AsyncFd,
    sync::{mpsc as reports, watch as updates},
    task::JoinHandle,
    time::{Instant, sleep, timeout},
};

use super::{Change, Envelope, Event, Kind, Snapshot, due_entry, next_deadline, read_envelope};
use crate::storage;

const CALL_TIMEOUT: Duration = Duration::from_secs(15);
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const TRANSACTION_TIMEOUT: Duration = Duration::from_secs(15);
const RETRY: Duration = Duration::from_secs(30);

type Reporter = reports::Sender<String>;

#[derive(Clone)]
enum Wake {
    Changed,
    Broken(String),
}

/// Uses inotify for disk changes and a realtime timerfd for deadlines, including suspend.
pub fn subscription() -> Subscription<Event> {
    Subscription::run(|| {
        stream::channel(16, |mut output| async move {
            let mut last_error = None;
            loop {
                let result = match storage::data_path("agenda.json") {
                    Ok(path) => monitor(path, &mut output, false).await,
                    Err(error) => Err(error),
                };
                match result {
                    Ok(()) => return,
                    Err(error) => {
                        if last_error.as_ref() != Some(&error) {
                            if output.send(Event::Error(error.clone())).await.is_err() {
                                return;
                            }
                            last_error = Some(error);
                        }
                        sleep(RETRY).await;
                    }
                }
            }
        })
    })
}

/// Notifier windows observe edits without running another delivery scheduler.
pub fn read_only_subscription() -> Subscription<Event> {
    Subscription::run(|| {
        stream::channel(16, |mut output| async move {
            let mut last_error = None;
            loop {
                let result = match storage::data_path("agenda.json") {
                    Ok(path) => monitor(path, &mut output, true).await,
                    Err(error) => Err(error),
                };
                match result {
                    Ok(()) => return,
                    Err(error) => {
                        if !emit_error(&mut output, &mut last_error, error).await {
                            return;
                        }
                        sleep(RETRY).await;
                    }
                }
            }
        })
    })
}

pub(super) async fn load_at(path: &Path) -> Result<Snapshot, String> {
    Ok(timeout(IO_TIMEOUT, read_envelope(path))
        .await
        .map_err(|_| "Reading the agenda timed out. Saved data was not changed.".to_owned())??
        .snapshot)
}

pub(super) async fn apply_at(
    path: PathBuf,
    revision: u64,
    change: Change,
) -> Result<Snapshot, String> {
    // Dropping a GUI command must not cancel an in-flight atomic save and release its lock.
    let mut job = tokio::spawn(async move {
        let guard = lock(&path).await?;
        let mut state = timeout(IO_TIMEOUT, read_envelope(&path))
            .await
            .map_err(|_| {
                "Reading the agenda timed out. Saved data was not changed.".to_owned()
            })??;
        let confirm_resolution = matches!(&change, Change::Finish(_));
        if state.change(revision, change)? || confirm_resolution {
            commit_once(&path, &state, None).await?;
        }
        drop(guard);
        Ok::<_, String>(state.snapshot)
    });
    timeout(TRANSACTION_TIMEOUT, &mut job).await
        .map_err(|_| "The agenda save is still finishing. Do not repeat the edit; its result will appear when storage recovers.".to_owned())?
        .map_err(|error| format!("The agenda save task failed: {error}"))?
}

async fn lock(path: &Path) -> Result<File, String> {
    timeout(IO_TIMEOUT, storage::prepare_parent(path))
        .await
        .map_err(|_| "Preparing the agenda directory timed out.".to_owned())??;
    let lock_path = path.with_extension("lock");
    let file = timeout(
        IO_TIMEOUT,
        tokio::task::spawn_blocking(move || {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(lock_path)
                .map_err(|error| format!("Could not open the agenda lock: {error}"))?;
            let metadata = file
                .metadata()
                .map_err(|error| format!("Could not inspect the agenda lock: {error}"))?;
            // SAFETY: geteuid has no arguments or memory access requirements.
            if !metadata.is_file()
                || metadata.nlink() != 1
                || metadata.uid() != unsafe { libc::geteuid() }
            {
                return Err("The agenda lock must be a private, owned regular file.".to_owned());
            }
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|error| format!("Could not secure the agenda lock: {error}"))?;
            Ok::<_, String>(file)
        }),
    )
    .await
    .map_err(|_| "Opening the agenda lock timed out.".to_owned())?
    .map_err(|error| format!("The agenda lock task failed: {error}"))??;
    let until = Instant::now() + LOCK_TIMEOUT;
    loop {
        // SAFETY: the owned descriptor remains open and LOCK_NB never waits for another process.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(file);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::WouldBlock && error.kind() != io::ErrorKind::Interrupted {
            return Err(format!("Could not lock the agenda: {error}"));
        }
        if Instant::now() >= until {
            return Err("The agenda is busy saving or delivering a reminder. Try again after storage recovers.".into());
        }
        sleep(Duration::from_millis(25)).await;
    }
}

fn report(sender: Option<&Reporter>, error: String) {
    if let Some(sender) = sender {
        // An output consumer must never indefinitely extend the disk transaction.
        let _ = sender.try_send(error);
    }
}

async fn commit_once(
    path: &Path,
    state: &Envelope,
    sender: Option<&Reporter>,
) -> Result<(), String> {
    let write = storage::write_json(path, state);
    tokio::pin!(write);
    loop {
        // Timeout the wait, not the write: cancellation can leave Tokio filesystem work
        // running. Keep the same future and file lock until that operation finishes.
        match timeout(IO_TIMEOUT, &mut write).await {
            Ok(result) => return result,
            Err(_) => report(sender, "Persisting the agenda is taking longer than expected. The transaction remains locked to protect saved data.".into()),
        }
    }
}

struct Pass {
    snapshot: Snapshot,
    delivered: Option<u64>,
    presented: Option<u64>,
}

async fn deliver_due(
    path: PathBuf,
    now: DateTime<Utc>,
    sender: Reporter,
    id: u64,
) -> Result<Pass, String> {
    deliver_selected(path, now, sender, RETRY, Some(id), notifier::present).await
}

// Injection is private and only separates the real transport from storage/scheduling tests.
#[cfg(test)]
async fn deliver_with<F, Fut>(
    path: PathBuf,
    now: DateTime<Utc>,
    sender: Reporter,
    retry: Duration,
    send: F,
) -> Result<Pass, String>
where
    F: FnMut(u64) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    deliver_selected(path, now, sender, retry, None, send).await
}

async fn deliver_selected<F, Fut>(
    path: PathBuf,
    now: DateTime<Utc>,
    sender: Reporter,
    retry: Duration,
    selected: Option<u64>,
    mut send: F,
) -> Result<Pass, String>
where
    F: FnMut(u64) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let guard = lock(&path).await?;
    let mut state = timeout(IO_TIMEOUT, read_envelope(&path))
        .await
        .map_err(|_| "Reading the agenda timed out. No reminder was sent.".to_owned())??;
    let index = match selected {
        Some(id) => state
            .snapshot
            .entries
            .iter()
            .position(|entry| entry.id == id && entry.pending() && entry.reminder_at() <= now),
        None => due_entry(&state.snapshot, now),
    };
    let Some(index) = index else {
        return Ok(Pass {
            snapshot: state.snapshot,
            delivered: None,
            presented: None,
        });
    };
    let revision = state
        .snapshot
        .revision
        .checked_add(1)
        .ok_or("The agenda revision counter is exhausted. No reminder was sent.")?;
    let entry = &state.snapshot.entries[index];
    let id = entry.id;
    let start = entry.start;
    let minutes = entry.reminder_minutes;
    let snoozed_until = entry.snoozed_until;
    timeout(CALL_TIMEOUT, send(id))
        .await
        .map_err(|_| "Sending the agenda reminder timed out. It will be retried.".to_owned())??;
    state.snapshot.entries[index].notified = Some(now);
    state.snapshot.entries[index].reminder_pending = true;
    state.snapshot.revision = revision;
    loop {
        match commit_once(&path, &state, Some(&sender)).await {
            Ok(()) => break,
            Err(error) => report(
                Some(&sender),
                format!(
                    "A reminder was sent but its delivery state could not be saved: {error} The agenda remains locked; only persistence will be retried."
                ),
            ),
        }
        // Keep the window receipt and lock alive even when the subscription is dropped.
        // Retry only persistence; the notifier's per-entry lock also survives applet restarts.
        loop {
            sleep(retry).await;
            let current = match timeout(IO_TIMEOUT, super::read_saved_envelope(&path)).await {
                Ok(Ok(current)) => current,
                Ok(Err(error)) => {
                    report(
                        Some(&sender),
                        format!("The sent reminder is awaiting safe persistence: {error}"),
                    );
                    continue;
                }
                Err(_) => {
                    report(Some(&sender), "The sent reminder is awaiting safe persistence: reading the agenda timed out.".into());
                    continue;
                }
            };
            let Some(position) = current
                .snapshot
                .entries
                .iter()
                .position(|entry| entry.id == id)
            else {
                // A non-cooperating external edit deleted it. Never recreate user data.
                return Ok(Pass {
                    snapshot: current.snapshot,
                    delivered: Some(id),
                    presented: Some(id),
                });
            };
            let current_entry = &current.snapshot.entries[position];
            if current_entry.start != start
                || current_entry.reminder_minutes != minutes
                || current_entry.snoozed_until != snoozed_until
                || (current_entry.kind == Kind::Task && current_entry.completed)
                || current_entry.notified.is_some_and(|time| time != now)
            {
                return Ok(Pass {
                    snapshot: current.snapshot,
                    delivered: Some(id),
                    presented: Some(id),
                });
            }
            state = current;
            if state.snapshot.entries[position].notified.is_none() {
                let Some(revision) = state.snapshot.revision.checked_add(1) else {
                    report(Some(&sender), "The sent reminder is awaiting safe persistence: the agenda revision counter is exhausted.".into());
                    continue;
                };
                state.snapshot.entries[position].notified = Some(now);
                state.snapshot.entries[position].reminder_pending = true;
                state.snapshot.revision = revision;
            }
            break;
        }
    }
    drop(guard);
    Ok(Pass {
        snapshot: state.snapshot,
        delivered: Some(id),
        presented: Some(id),
    })
}

async fn restore_pending<F, Fut>(path: PathBuf, id: u64, mut present: F) -> Result<Pass, String>
where
    F: FnMut(u64) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let guard = lock(&path).await?;
    let state = timeout(IO_TIMEOUT, read_envelope(&path))
        .await
        .map_err(|_| {
            "Reading the agenda timed out. The pending reminder will be retried.".to_owned()
        })??;
    let entry = state.snapshot.entries.iter().find(|entry| {
        entry.id == id && entry.reminder_pending && !(entry.kind == Kind::Task && entry.completed)
    });
    let presented = if let Some(entry) = entry {
        timeout(CALL_TIMEOUT, present(entry.id))
            .await
            .map_err(|_| {
                "Restoring the reminder window timed out. It will be retried.".to_owned()
            })??;
        Some(id)
    } else {
        None
    };
    drop(guard);
    Ok(Pass {
        snapshot: state.snapshot,
        delivered: None,
        presented,
    })
}

async fn watch(path: &Path) -> Result<(RecommendedWatcher, updates::Receiver<Wake>), String> {
    let directory = timeout(IO_TIMEOUT, storage::prepare_parent(path))
        .await
        .map_err(|_| "Preparing the agenda watch timed out.".to_owned())??
        .to_path_buf();
    let agenda_path = path.to_path_buf();
    let (sender, receiver) = updates::channel(Wake::Changed);
    let watcher = timeout(
        IO_TIMEOUT,
        tokio::task::spawn_blocking(move || {
            let root = directory.clone();
            let mut watcher =
                notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                    let wake = match event {
                        Ok(event) if matches!(event.kind, EventKind::Access(_)) => return,
                        Ok(event)
                            if matches!(
                                event.kind,
                                EventKind::Remove(_)
                                    | EventKind::Modify(notify::event::ModifyKind::Name(_))
                            ) && event.paths.iter().any(|path| path == &root) =>
                        {
                            Wake::Broken(
                                "The agenda directory watch changed. Reconnecting reminders."
                                    .into(),
                            )
                        }
                        Ok(event)
                            if event.need_rescan()
                                || event.paths.is_empty()
                                || event.paths.iter().any(|path| path == &agenda_path) =>
                        {
                            Wake::Changed
                        }
                        Ok(_) => return,
                        Err(error) => {
                            Wake::Broken(format!("The agenda directory watch failed: {error}"))
                        }
                    };
                    // Coalesce arbitrary write bursts into one full reload. A broken watch is
                    // sticky so later file traffic cannot hide a required reconnection.
                    sender.send_if_modified(|last| {
                        if matches!(last, Wake::Broken(_)) {
                            return false;
                        }
                        *last = wake;
                        true
                    });
                })
                .map_err(|error| format!("Could not create the agenda directory watch: {error}"))?;
            watcher
                .watch(&directory, RecursiveMode::NonRecursive)
                .map_err(|error| format!("Could not watch the agenda directory: {error}"))?;
            Ok::<_, String>(watcher)
        }),
    )
    .await
    .map_err(|_| "Creating the agenda watch timed out.".to_owned())?
    .map_err(|error| format!("The agenda watch task failed: {error}"))??;
    Ok((watcher, receiver))
}

/// CLOCK_REALTIME absolute deadlines expire during suspend; CANCEL_ON_SET wakes on clock jumps.
struct Deadline(AsyncFd<OwnedFd>);

impl Deadline {
    fn new() -> Result<Self, String> {
        // SAFETY: timerfd_create takes only validated kernel constants.
        let descriptor = unsafe {
            libc::timerfd_create(libc::CLOCK_REALTIME, libc::TFD_CLOEXEC | libc::TFD_NONBLOCK)
        };
        if descriptor < 0 {
            return Err(format!(
                "Could not create the reminder deadline timer: {}",
                io::Error::last_os_error()
            ));
        }
        // SAFETY: a successful timerfd_create returns a new descriptor owned by this object.
        let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
        AsyncFd::new(descriptor)
            .map(Self)
            .map_err(|error| format!("Could not monitor reminder deadlines: {error}"))
    }

    fn arm(&self, deadline: Option<DateTime<Utc>>) -> Result<(), String> {
        let (seconds, nanos) = match deadline {
            Some(value) => {
                // Past dates must expire immediately, not become the all-zero disarm value.
                let value = value.max(Utc::now());
                let seconds = libc::time_t::try_from(value.timestamp()).map_err(|_| {
                    "The reminder deadline is outside the native timer's date range.".to_owned()
                })?;
                (seconds, libc::c_long::from(value.timestamp_subsec_nanos()))
            }
            None => (0, 0),
        };
        let timer = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: libc::timespec {
                tv_sec: seconds,
                tv_nsec: nanos,
            },
        };
        // SAFETY: the timer descriptor and initialized timespec are valid; no old-value output is requested.
        if unsafe {
            libc::timerfd_settime(
                self.0.get_ref().as_raw_fd(),
                libc::TFD_TIMER_ABSTIME | libc::TFD_TIMER_CANCEL_ON_SET,
                &timer,
                std::ptr::null_mut(),
            )
        } < 0
        {
            return Err(format!(
                "Could not arm the reminder deadline: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    async fn elapsed(&self) -> Result<(), String> {
        loop {
            let mut ready = self
                .0
                .readable()
                .await
                .map_err(|error| format!("The reminder deadline timer failed: {error}"))?;
            let result = ready.try_io(|descriptor| {
                let mut expirations = 0_u64;
                // SAFETY: expirations provides eight writable bytes and this nonblocking descriptor is owned.
                let bytes = unsafe {
                    libc::read(
                        descriptor.get_ref().as_raw_fd(),
                        (&mut expirations as *mut u64).cast(),
                        std::mem::size_of::<u64>(),
                    )
                };
                if bytes == std::mem::size_of::<u64>() as isize {
                    Ok(())
                } else if bytes < 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::ECANCELED) {
                        Ok(())
                    } else {
                        Err(error)
                    }
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "Incomplete reminder timer event",
                    ))
                }
            });
            match result {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(error)) => {
                    return Err(format!("The reminder deadline timer failed: {error}"));
                }
                Err(_) => continue,
            }
        }
    }
}

async fn emit_error(
    output: &mut mpsc::Sender<Event>,
    last: &mut Option<String>,
    error: String,
) -> bool {
    if last.as_ref() == Some(&error) {
        return true;
    }
    if output.send(Event::Error(error.clone())).await.is_err() {
        return false;
    }
    *last = Some(error);
    true
}

async fn publish(
    output: &mut mpsc::Sender<Event>,
    published: &mut Option<Snapshot>,
    snapshot: Snapshot,
    recovered: bool,
) -> bool {
    if !recovered && published.as_ref() == Some(&snapshot) {
        return true;
    }
    if output.send(Event::Loaded(snapshot.clone())).await.is_err() {
        return false;
    }
    *published = Some(snapshot);
    true
}

fn ready_window(
    snapshot: &Snapshot,
    now: DateTime<Utc>,
    presented: &HashSet<u64>,
    blocked: &HashMap<u64, DateTime<Utc>>,
) -> Option<(u64, bool)> {
    snapshot
        .entries
        .iter()
        .filter(|entry| {
            !(entry.kind == Kind::Task && entry.completed)
                && blocked.get(&entry.id).is_none_or(|until| *until <= now)
                && ((entry.reminder_pending && !presented.contains(&entry.id))
                    || (entry.pending() && entry.reminder_at() <= now))
        })
        .min_by_key(|entry| (entry.reminder_at(), entry.id))
        .map(|entry| (entry.id, entry.reminder_pending))
}

fn window_deadline(
    snapshot: &Snapshot,
    now: DateTime<Utc>,
    presented: &HashSet<u64>,
    blocked: &HashMap<u64, DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    let due = if blocked.is_empty() {
        next_deadline(snapshot)
    } else {
        snapshot
            .entries
            .iter()
            .filter(|entry| entry.pending())
            .map(|entry| {
                entry.reminder_at().max(
                    blocked
                        .get(&entry.id)
                        .copied()
                        .unwrap_or(entry.reminder_at()),
                )
            })
            .min()
    };
    let restore = snapshot
        .entries
        .iter()
        .filter(|entry| {
            entry.reminder_pending
                && !(entry.kind == Kind::Task && entry.completed)
                && !presented.contains(&entry.id)
        })
        .map(|entry| blocked.get(&entry.id).copied().unwrap_or(now))
        .min();
    due.into_iter().chain(restore).min()
}

async fn monitor(
    path: PathBuf,
    output: &mut mpsc::Sender<Event>,
    read_only: bool,
) -> Result<(), String> {
    let (_watcher, mut changes) = watch(&path).await?;
    let timer = Deadline::new()?;
    let (reporter, mut errors) = reports::channel(16);
    let mut published = None;
    let mut current_valid = false;
    let mut last_error = None;
    let mut retry_at = None;
    let mut job: Option<JoinHandle<Result<Pass, String>>> = None;
    let mut refresh = true;
    let mut presented = HashSet::new();
    let mut exits = notifier::exits();
    let mut blocked = HashMap::new();
    let mut running_id = None;
    loop {
        if refresh {
            refresh = false;
            match load_at(&path).await {
                Ok(snapshot) => {
                    presented.retain(|id| {
                        snapshot
                            .entries
                            .iter()
                            .any(|entry| entry.id == *id && entry.reminder_pending)
                    });
                    blocked.retain(|id, _| snapshot.entries.iter().any(|entry| entry.id == *id));
                    if !publish(output, &mut published, snapshot, !current_valid).await {
                        return Ok(());
                    }
                    if !current_valid {
                        last_error = None;
                    }
                    current_valid = true;
                }
                Err(error) => {
                    // Keep the last visible snapshot, but never deliver from a stale/corrupt read.
                    current_valid = false;
                    retry_at = Some(Utc::now() + TimeDelta::seconds(RETRY.as_secs() as i64));
                    if !emit_error(output, &mut last_error, error).await {
                        return Ok(());
                    }
                }
            }
        }
        let now = Utc::now();
        if job.is_none()
            && !read_only
            && current_valid
            && retry_at.is_none_or(|retry| retry <= now)
            && let Some((id, restoring)) = published
                .as_ref()
                .and_then(|snapshot| ready_window(snapshot, now, &presented, &blocked))
        {
            running_id = Some(id);
            job = Some(if restoring {
                tokio::spawn(restore_pending(path.clone(), id, notifier::present))
            } else {
                tokio::spawn(deliver_due(path.clone(), now, reporter.clone(), id))
            });
            retry_at = None;
        }
        let deadline = if job.is_some() {
            None
        } else {
            match retry_at {
                Some(retry) => Some(retry),
                None if current_valid && !read_only => published
                    .as_ref()
                    .and_then(|snapshot| window_deadline(snapshot, now, &presented, &blocked)),
                None => None,
            }
        };
        timer.arm(deadline)?;
        tokio::select! {
            exit = exits.recv(), if !read_only => {
                match exit {
                    Ok(id) => {
                        presented.remove(&id);
                        blocked.insert(id, Utc::now() + TimeDelta::seconds(RETRY.as_secs() as i64));
                    }
                    Err(_) => { presented.clear(); }
                }
                refresh = true;
            }
            wake = changes.changed() => {
                wake.map_err(|_| "The agenda directory watch stopped.".to_owned())?;
                match changes.borrow_and_update().clone() {
                    Wake::Changed => refresh = true,
                    Wake::Broken(error) => return Err(error),
                }
            }
            result = timer.elapsed(), if job.is_none() => {
                result?;
                retry_at = None;
                refresh = true;
            }
            Some(error) = errors.recv() => {
                if !emit_error(output, &mut last_error, error).await { return Ok(()); }
            }
            result = async { match job.as_mut() { Some(job) => job.await, None => std::future::pending().await } }, if job.is_some() => {
                job = None;
                let id = running_id.take();
                match result {
                    Ok(Ok(pass)) => {
                        if let Some(id) = id { blocked.remove(&id); }
                        if let Some(id) = pass.presented { presented.insert(id); }
                        if let Some(id) = pass.delivered
                            && output.send(Event::Delivered(id)).await.is_err() { return Ok(()); }
                        if !publish(output, &mut published, pass.snapshot, last_error.is_some()).await { return Ok(()); }
                        current_valid = true;
                        last_error = None;
                        retry_at = None;
                    }
                    Ok(Err(error)) => {
                        if !emit_error(output, &mut last_error, error).await { return Ok(()); }
                        if let Some(id) = id {
                            blocked.insert(id, Utc::now() + TimeDelta::seconds(RETRY.as_secs() as i64));
                        }
                        refresh = true;
                    }
                    Err(error) => {
                        if !emit_error(output, &mut last_error, format!("The reminder task failed: {error}")).await { return Ok(()); }
                        if let Some(id) = id {
                            blocked.insert(id, Utc::now() + TimeDelta::seconds(RETRY.as_secs() as i64));
                        }
                        refresh = true;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
