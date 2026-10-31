// SPDX-License-Identifier: GPL-3.0-only

use std::{collections::HashMap, path::Path, process::Stdio, time::Duration};

use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use zbus::{Proxy, connection::Builder, zvariant::Value};

use crate::{launcher::desktop_command, storage};

/// The largest duration representable by HH:MM:SS (99:59:59).
pub const MAX_SECONDS: u32 = 99 * 3600 + 59 * 60 + 59;
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub duration_seconds: u32,
    pub sound: bool,
}

impl Settings {
    pub fn validate(&self) -> Result<(), String> {
        if self.duration_seconds > MAX_SECONDS {
            return Err("The timer duration must be between 00:00:00 and 99:59:59.".into());
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsFile {
    version: u32,
    settings: Settings,
}

fn decode(bytes: &[u8]) -> Result<Settings, String> {
    let file: SettingsFile = serde_json::from_slice(bytes)
        .map_err(|error| format!("Saved timer settings contain invalid JSON: {error}"))?;
    if file.version != 1 {
        return Err(
            "Saved timer settings use an unsupported version; the file has not been changed."
                .into(),
        );
    }
    file.settings.validate()?;
    Ok(file.settings)
}

pub async fn load() -> Result<Settings, String> {
    load_from(&storage::data_path("timer.json")?).await
}

async fn load_from(path: &Path) -> Result<Settings, String> {
    match storage::read(path).await? {
        Some(bytes) => decode(&bytes),
        None => Ok(Settings::default()),
    }
}

pub async fn save(settings: Settings) -> Result<(), String> {
    save_to(&storage::data_path("timer.json")?, settings).await
}

async fn save_to(path: &Path, settings: Settings) -> Result<(), String> {
    settings.validate()?;
    storage::write_json(
        path,
        &SettingsFile {
            version: 1,
            settings,
        },
    )
    .await
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    Idle,
    Running,
    Paused,
    Expired,
}

#[derive(Clone, Copy, Debug, Default)]
enum State {
    #[default]
    Idle,
    Running {
        deadline: Duration,
    },
    Paused {
        remaining: Duration,
    },
    Expired,
}

/// Runtime state only; persisted settings never restore a running timer.
#[derive(Debug, Default)]
pub struct Countdown {
    duration_seconds: u32,
    state: State,
}

impl Countdown {
    /// Set a validated preset and return to idle, discarding any previous countdown.
    pub fn set_duration(&mut self, seconds: u32) {
        self.duration_seconds = seconds;
        self.reset();
    }

    /// Start/restart the preset or resume the exact paused duration.
    pub fn start(&mut self, now: Duration) {
        let remaining = match self.state {
            State::Idle | State::Expired => Duration::from_secs(self.duration_seconds.into()),
            State::Paused { remaining } => remaining,
            State::Running { .. } => return,
        };
        if !remaining.is_zero() {
            self.state = State::Running {
                deadline: now.saturating_add(remaining),
            };
        }
    }

    /// Return true only when pausing discovers an expiry not previously reported.
    pub fn pause(&mut self, now: Duration) -> bool {
        if self.tick(now) {
            return true;
        }
        if let State::Running { deadline } = self.state {
            self.state = State::Paused {
                remaining: deadline.saturating_sub(now),
            };
        }
        false
    }

    pub fn reset(&mut self) {
        self.state = State::Idle;
    }

    /// Report expiry exactly once per start, including a deadline passed during suspend.
    pub fn tick(&mut self, now: Duration) -> bool {
        if let State::Running { deadline } = self.state
            && now >= deadline
        {
            self.state = State::Expired;
            return true;
        }
        false
    }

    /// Display whole seconds rounded up without rounding the actual paused state.
    pub fn remaining(&self, now: Duration) -> u32 {
        let remaining = match self.state {
            State::Idle => Duration::from_secs(self.duration_seconds.into()),
            State::Running { deadline } => deadline.saturating_sub(now),
            State::Paused { remaining } => remaining,
            State::Expired => Duration::ZERO,
        };
        remaining.as_secs() as u32 + u32::from(remaining.subsec_nanos() != 0)
    }

    /// The suspend-inclusive deadline, present only while running.
    pub fn deadline(&self) -> Option<Duration> {
        match self.state {
            State::Running { deadline } => Some(deadline),
            _ => None,
        }
    }

    pub fn status(&self) -> Status {
        match self.state {
            State::Idle => Status::Idle,
            State::Running { .. } => Status::Running,
            State::Paused { .. } => Status::Paused,
            State::Expired => Status::Expired,
        }
    }
}

/// Unlike CLOCK_MONOTONIC/Instant, Linux boottime includes time spent suspended.
pub fn clock_now() -> Result<Duration, String> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: value is a valid, aligned output pointer to a timespec for this call.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut value) } != 0 {
        return Err(format!(
            "Could not read the suspend-inclusive timer clock: {}",
            std::io::Error::last_os_error()
        ));
    }
    if value.tv_sec < 0 || !(0..1_000_000_000).contains(&value.tv_nsec) {
        return Err("The suspend-inclusive timer clock returned an invalid timestamp.".into());
    }
    Ok(Duration::new(value.tv_sec as u64, value.tv_nsec as u32))
}

/// Always attempt desktop delivery and, when enabled, native sound independently.
pub async fn notify_finished(duration_seconds: u32, sound: bool) -> Result<(), String> {
    let (notification, ding) = tokio::join!(send_notification(duration_seconds), async {
        if sound { play_ding().await } else { Ok(()) }
    });
    match (notification, ding) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(notification), Err(ding)) => Err(format!("{notification} {ding}")),
    }
}

async fn send_notification(duration_seconds: u32) -> Result<(), String> {
    timeout(CALL_TIMEOUT, async {
        let connection = Builder::session()
            .map_err(|error| format!("The timer notification session bus is unavailable: {error}"))?
            .max_queued(16)
            .method_timeout(CALL_TIMEOUT)
            .build()
            .await
            .map_err(|error| format!("Could not connect the timer to the session bus: {error}"))?;
        let proxy = Proxy::new(
            &connection,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
        )
        .await
        .map_err(|error| format!("Could not open desktop timer notifications: {error}"))?;
        let hints = HashMap::from([
            ("desktop-entry", Value::from(crate::config::APP_ID)),
            // The optional local player owns the ding, avoiding duplicate daemon playback.
            ("suppress-sound", Value::from(true)),
        ]);
        let body = format!(
            "The {:02}:{:02}:{:02} timer has finished.",
            duration_seconds / 3600,
            duration_seconds / 60 % 60,
            duration_seconds % 60
        );
        let actions: &[&str] = &[];
        let _: u32 = proxy
            .call(
                "Notify",
                &(
                    "Cosmos Date & Time",
                    0_u32,
                    "alarm-symbolic",
                    "Timer finished",
                    body.as_str(),
                    actions,
                    hints,
                    -1_i32,
                ),
            )
            .await
            .map_err(|error| format!("Could not deliver the timer notification: {error}"))?;
        Ok(())
    })
    .await
    .map_err(|_| "Desktop timer notification delivery timed out.".to_owned())?
}

async fn play_ding() -> Result<(), String> {
    let mut child = desktop_command("canberra-gtk-play")
        // The bell is an explicit opt-in; Do Not Disturb is checked before this runs.
        .env("CANBERRA_FORCE_EVENT_SOUNDS", "1")
        .arg("--id=complete")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            format!(
                "Could not start the timer ding: {error}. Install canberra-gtk-play and a sound theme providing the complete event."
            )
        })?;
    let status = timeout(CALL_TIMEOUT, child.wait())
        .await
        .map_err(|_| "Timer ding playback timed out.".to_owned())?
        .map_err(|error| format!("Could not wait for the timer ding: {error}"))?;
    if !status.success() {
        return Err(format!(
            "Timer ding playback failed ({status}); check canberra-gtk-play, the complete sound event and session audio."
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct Directory(PathBuf);

    impl Directory {
        fn new() -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "datetime-timer-test-{}-{stamp}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }

        fn settings(&self) -> PathBuf {
            self.0.join("private").join("timer.json")
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn deadline_is_exposed_only_while_running_and_changes_on_resume() {
        let mut timer = Countdown::default();
        timer.set_duration(5);
        assert_eq!(timer.deadline(), None);
        timer.start(Duration::from_secs(10));
        assert_eq!(timer.deadline(), Some(Duration::from_secs(15)));
        assert!(!timer.pause(Duration::from_secs(11)));
        assert_eq!(timer.deadline(), None);
        timer.start(Duration::from_secs(20));
        assert_eq!(timer.deadline(), Some(Duration::from_secs(24)));
        assert!(timer.tick(Duration::from_secs(24)));
        assert_eq!(timer.deadline(), None);
        timer.reset();
        assert_eq!(timer.deadline(), None);
    }

    #[test]
    fn zero_never_starts_or_expires() {
        let mut timer = Countdown::default();
        timer.start(Duration::from_secs(10));
        assert_eq!(timer.status(), Status::Idle);
        assert_eq!(timer.remaining(Duration::from_secs(100)), 0);
        assert!(!timer.pause(Duration::from_secs(100)));
        assert!(!timer.tick(Duration::from_secs(100)));
    }

    #[test]
    fn running_start_is_noop_and_display_rounds_up() {
        let mut timer = Countdown::default();
        timer.set_duration(2);
        timer.start(Duration::from_millis(10_250));
        assert_eq!(timer.status(), Status::Running);
        assert_eq!(timer.remaining(Duration::from_millis(11_249)), 2);
        assert_eq!(timer.remaining(Duration::from_millis(11_250)), 1);
        timer.start(Duration::from_secs(11));
        assert!(!timer.tick(Duration::from_millis(12_249)));
        assert!(timer.tick(Duration::from_millis(12_250)));
    }

    #[test]
    fn pause_resume_preserves_fractional_remaining_without_counting_pause_time() {
        let mut timer = Countdown::default();
        timer.set_duration(10);
        timer.start(Duration::from_secs(100));
        assert!(!timer.pause(Duration::from_millis(103_250)));
        assert_eq!(timer.status(), Status::Paused);
        assert_eq!(timer.remaining(Duration::from_secs(10_000)), 7);
        assert!(!timer.tick(Duration::from_secs(10_000)));
        assert!(!timer.pause(Duration::from_secs(10_000)));
        timer.start(Duration::from_millis(20_000_500));
        assert!(!timer.tick(Duration::from_millis(20_007_249)));
        assert_eq!(timer.remaining(Duration::from_millis(20_007_249)), 1);
        assert!(timer.tick(Duration::from_millis(20_007_250)));
    }

    #[test]
    fn reset_restores_preset_from_running_paused_and_expired_states() {
        let mut timer = Countdown::default();
        timer.set_duration(10);
        for state in [Status::Running, Status::Paused, Status::Expired] {
            timer.start(Duration::from_secs(100));
            match state {
                Status::Paused => assert!(!timer.pause(Duration::from_secs(103))),
                Status::Expired => assert!(timer.tick(Duration::from_secs(110))),
                _ => {}
            }
            assert_eq!(timer.status(), state);
            timer.reset();
            assert_eq!(timer.status(), Status::Idle);
            assert_eq!(timer.remaining(Duration::from_secs(1000)), 10);
            assert!(!timer.tick(Duration::from_secs(1000)));
        }
    }

    #[test]
    fn expiry_after_suspend_is_exactly_once_and_play_restarts_saved_preset() {
        let mut timer = Countdown::default();
        timer.set_duration(5);
        timer.start(Duration::from_secs(10));
        assert_eq!(timer.remaining(Duration::from_secs(1000)), 0);
        assert!(timer.tick(Duration::from_secs(1000)));
        assert_eq!(timer.status(), Status::Expired);
        assert!(!timer.tick(Duration::from_secs(1001)));
        assert!(!timer.pause(Duration::from_secs(1001)));
        timer.start(Duration::from_secs(2000));
        assert_eq!(timer.remaining(Duration::from_secs(2000)), 5);
        assert!(timer.tick(Duration::from_secs(2005)));
        assert!(!timer.tick(Duration::from_secs(2005)));
    }

    #[test]
    fn pause_at_deadline_reports_expiry_once_instead_of_pausing() {
        let mut timer = Countdown::default();
        timer.set_duration(1);
        timer.start(Duration::from_secs(10));
        assert!(timer.pause(Duration::from_secs(11)));
        assert_eq!(timer.status(), Status::Expired);
        assert!(!timer.tick(Duration::from_secs(11)));
        assert!(!timer.pause(Duration::from_secs(12)));
    }

    #[test]
    fn changing_duration_resets_runtime_and_maximum_counts_down_exactly() {
        let mut timer = Countdown::default();
        timer.set_duration(10);
        timer.start(Duration::from_secs(10));
        timer.set_duration(MAX_SECONDS);
        assert_eq!(timer.status(), Status::Idle);
        assert_eq!(timer.remaining(Duration::ZERO), MAX_SECONDS);
        timer.start(Duration::ZERO);
        assert_eq!(timer.remaining(Duration::from_nanos(1)), MAX_SECONDS);
        assert!(!timer.tick(Duration::from_secs(u64::from(MAX_SECONDS) - 1)));
        assert!(timer.tick(Duration::from_secs(MAX_SECONDS.into())));
        timer.set_duration(0);
        timer.start(Duration::from_secs(1));
        assert_eq!(timer.status(), Status::Idle);
    }

    #[tokio::test]
    async fn boundary_presets_and_sound_round_trip_in_private_versioned_storage() {
        let directory = Directory::new();
        let path = directory.settings();
        for settings in [
            Settings {
                duration_seconds: 0,
                sound: false,
            },
            Settings {
                duration_seconds: MAX_SECONDS,
                sound: true,
            },
        ] {
            save_to(&path, settings).await.unwrap();
            assert_eq!(load_from(&path).await.unwrap(), settings);
        }
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
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
        let file: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(file["version"], 1);
    }

    #[tokio::test]
    async fn corrupt_unsupported_and_out_of_range_settings_are_rejected_without_changes() {
        let directory = Directory::new();
        let path = directory.settings();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for original in [
            "not JSON",
            r#"{"version":0,"settings":{"duration_seconds":0,"sound":false}}"#,
            r#"{"version":2,"settings":{"duration_seconds":0,"sound":false}}"#,
            r#"{"version":1,"settings":{"duration_seconds":360000,"sound":false}}"#,
            r#"{"version":1,"settings":{"duration_seconds":-1,"sound":false}}"#,
            r#"{"version":1,"settings":{"duration_seconds":4294967296,"sound":false}}"#,
            r#"{"version":1,"settings":{"duration_seconds":1.5,"sound":false}}"#,
            r#"{"version":1,"settings":{"duration_seconds":0,"sound":"false"}}"#,
            r#"{"version":1,"settings":{"duration_seconds":0}}"#,
            r#"{"version":1,"settings":{"sound":false}}"#,
            r#"{"version":1,"settings":{"duration_seconds":0,"sound":false,"unknown":true}}"#,
            r#"{"version":1,"settings":{"duration_seconds":0,"duration_seconds":1,"sound":false}}"#,
            r#"{"settings":{"duration_seconds":0,"sound":false}}"#,
            r#"{"version":1,"settings":{"duration_seconds":0,"sound":false},"unknown":true}"#,
        ] {
            std::fs::write(&path, original).unwrap();
            assert!(
                load_from(&path).await.is_err(),
                "Unexpectedly accepted {original}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), original.as_bytes());
        }
    }

    #[tokio::test]
    async fn invalid_save_does_not_replace_a_valid_preset() {
        let directory = Directory::new();
        let path = directory.settings();
        let settings = Settings {
            duration_seconds: MAX_SECONDS,
            sound: true,
        };
        save_to(&path, settings).await.unwrap();
        let original = std::fs::read(&path).unwrap();
        assert!(
            save_to(
                &path,
                Settings {
                    duration_seconds: MAX_SECONDS + 1,
                    sound: false
                }
            )
            .await
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(load_from(&path).await.unwrap(), settings);
    }
}
