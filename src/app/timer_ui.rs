// SPDX-License-Identifier: GPL-3.0-only

use crate::{duration_input, timer};
use cosmic::iced::{Alignment, Length, Subscription};
use cosmic::prelude::*;
use cosmic::widget;
use std::time::Duration;

#[derive(Clone, Debug)]
pub enum Message {
    Input(duration_input::Message),
    Play,
    Pause,
    Reset,
    Sound,
    Tick,
    Loaded(Result<timer::Settings, String>),
    Saved(Result<(), String>),
    Notified(u64, Result<(), String>),
    RetryLoad,
    RetrySave,
}

#[derive(Default)]
pub struct TimerUi {
    countdown: timer::Countdown,
    settings: timer::Settings,
    displayed_seconds: u32,
    ready: bool,
    loading: bool,
    confirmed: Option<timer::Settings>,
    pending_save: Option<timer::Settings>,
    load_error: Option<String>,
    save_error: Option<String>,
    clock_error: Option<String>,
    notification_error: Option<String>,
    notification_generation: u64,
}

type Command = Task<cosmic::Action<super::Message>>;

fn message(message: Message) -> cosmic::Action<super::Message> {
    cosmic::Action::App(super::Message::Timer(message))
}

impl TimerUi {
    pub fn load(&mut self) -> Command {
        if self.loading || self.pending_save.is_some() {
            return Task::none();
        }
        self.loading = true;
        Task::perform(timer::load(), |result| message(Message::Loaded(result)))
    }

    fn save(&mut self) -> Command {
        if !self.ready || self.pending_save.is_some() || self.confirmed == Some(self.settings) {
            return Task::none();
        }
        let settings = self.settings;
        self.pending_save = Some(settings);
        Task::perform(timer::save(settings), |result| {
            message(Message::Saved(result))
        })
    }

    fn notify(&mut self, do_not_disturb: bool) -> Command {
        let generation = self.notification_generation;
        let seconds = self.settings.duration_seconds;
        let sound = self.settings.sound && !do_not_disturb;
        Task::perform(timer::notify_finished(seconds, sound), move |result| {
            message(Message::Notified(generation, result))
        })
    }

    fn now(&mut self) -> Option<Duration> {
        match timer::clock_now() {
            Ok(now) => {
                self.clock_error = None;
                Some(now)
            }
            Err(error) => {
                self.clock_error = Some(error);
                None
            }
        }
    }

    pub fn update(&mut self, event: Message, do_not_disturb: bool) -> Command {
        match event {
            Message::Loaded(result) => {
                self.loading = false;
                match result {
                    Ok(settings) => {
                        self.countdown.set_duration(settings.duration_seconds);
                        self.settings = settings;
                        self.displayed_seconds = settings.duration_seconds;
                        self.confirmed = Some(settings);
                        self.ready = true;
                        self.load_error = None;
                    }
                    Err(error) => {
                        self.ready = false;
                        self.load_error = Some(error);
                    }
                }
            }
            Message::Input(duration_input::Message::Changed(seconds)) => {
                if self.ready
                    && matches!(
                        self.countdown.status(),
                        timer::Status::Idle | timer::Status::Expired
                    )
                    && seconds <= timer::MAX_SECONDS
                {
                    self.settings.duration_seconds = seconds;
                    self.countdown.set_duration(seconds);
                    self.displayed_seconds = seconds;
                    return self.save();
                }
            }
            Message::Sound if self.ready => {
                self.settings.sound = !self.settings.sound;
                return self.save();
            }
            Message::Play if self.ready && self.settings.duration_seconds > 0 => {
                if self.countdown.status() == timer::Status::Running {
                    return Task::none();
                }
                if let Some(now) = self.now() {
                    if self.countdown.status() != timer::Status::Paused {
                        self.notification_generation = self.notification_generation.wrapping_add(1);
                    }
                    self.notification_error = None;
                    self.countdown.start(now);
                    self.displayed_seconds = self.countdown.remaining(now);
                    return self.save();
                }
            }
            Message::Pause if self.countdown.status() == timer::Status::Running => {
                if let Some(now) = self.now() {
                    let expired = self.countdown.pause(now);
                    self.displayed_seconds = self.countdown.remaining(now);
                    if expired {
                        return self.notify(do_not_disturb);
                    }
                }
            }
            Message::Reset if self.ready => {
                self.countdown.reset();
                self.displayed_seconds = self.settings.duration_seconds;
                self.clock_error = None;
                self.notification_error = None;
                self.notification_generation = self.notification_generation.wrapping_add(1);
            }
            Message::Tick if self.countdown.status() == timer::Status::Running => {
                if let Some(now) = self.now() {
                    let expired = self.countdown.tick(now);
                    self.displayed_seconds = self.countdown.remaining(now);
                    if expired {
                        return self.notify(do_not_disturb);
                    }
                }
            }
            Message::Saved(result) => {
                let Some(saved) = self.pending_save.take() else {
                    return Task::none();
                };
                match result {
                    Ok(()) => {
                        self.confirmed = Some(saved);
                        self.save_error = None;
                        // Only one atomic write at a time; persist edits made during it next.
                        return self.save();
                    }
                    Err(error) => {
                        self.save_error = Some(format!("Timer preferences were not saved: {error}"))
                    }
                }
            }
            Message::Notified(generation, result) if generation == self.notification_generation => {
                self.notification_error = result.err();
            }
            Message::RetryLoad if !self.ready => return self.load(),
            Message::RetrySave => return self.save(),
            _ => {}
        }
        Task::none()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let Some(deadline) = self.countdown.deadline() else {
            return Subscription::none();
        };
        Subscription::run_with(deadline, |deadline| {
            futures_util::stream::unfold(Some(*deadline), |next| async move {
                let deadline = next?;
                let (delay, final_tick) = match timer::clock_now() {
                    Ok(now) => {
                        let remaining = deadline.saturating_sub(now);
                        let delay = if remaining.is_zero() {
                            Duration::ZERO
                        } else if remaining.subsec_nanos() == 0 {
                            Duration::from_secs(1)
                        } else {
                            Duration::from_nanos(u64::from(remaining.subsec_nanos()))
                        };
                        (delay, remaining <= delay)
                    }
                    Err(_) => (Duration::from_secs(1), false),
                };
                tokio::time::sleep(delay).await;
                Some((Message::Tick, (!final_tick).then_some(deadline)))
            })
        })
    }

    pub fn view(&self) -> Element<'_, super::Message> {
        let status = self.countdown.status();
        let editable = self.ready && matches!(status, timer::Status::Idle | timer::Status::Expired);
        let status_text = if self.loading {
            "Loading…"
        } else {
            match status {
                timer::Status::Idle => "HH:MM:SS",
                timer::Status::Running => "Running",
                timer::Status::Paused => "Paused",
                timer::Status::Expired => "Finished",
            }
        };
        let bell = widget::button::icon(
            widget::icon::from_svg_bytes(
                &include_bytes!("../../resources/timer-bell-symbolic.svg")[..],
            )
            .symbolic(true),
        )
        .name(if self.settings.sound {
            "Disable timer expiry ding"
        } else {
            "Enable timer expiry ding"
        })
        .tooltip(if self.settings.sound {
            "Expiry ding on"
        } else {
            "Expiry ding off"
        })
        .class(if self.settings.sound {
            cosmic::theme::Button::Suggested
        } else {
            cosmic::theme::Button::Icon
        })
        .padding(5)
        .on_press_maybe(self.ready.then_some(super::Message::Timer(Message::Sound)));
        let header = widget::row([
            widget::text::title4("Timer")
                .size(15)
                .width(Length::Fill)
                .into(),
            widget::text(status_text).size(12).into(),
            bell.into(),
        ])
        .align_y(Alignment::Center)
        .spacing(6);
        let controls = widget::row([
            duration_input::input(self.displayed_seconds, editable)
                .map(|event| super::Message::Timer(Message::Input(event))),
            super::icon_button(
                "media-playback-start-symbolic",
                if status == timer::Status::Paused {
                    "Resume timer"
                } else {
                    "Start timer"
                },
                super::Message::Timer(Message::Play),
                self.ready
                    && self.settings.duration_seconds > 0
                    && status != timer::Status::Running,
            ),
            super::icon_button(
                "media-playback-pause-symbolic",
                "Pause timer",
                super::Message::Timer(Message::Pause),
                status == timer::Status::Running,
            ),
            super::icon_button(
                "media-seek-backward-symbolic",
                "Reset to configured duration",
                super::Message::Timer(Message::Reset),
                self.ready && status != timer::Status::Idle,
            ),
        ])
        .align_y(Alignment::Center)
        .spacing(6);
        let mut content = widget::column([header.into(), controls.into()]).spacing(6);
        if let Some(error) = &self.load_error {
            content = content
                .push(super::error_text(error))
                .push(super::action_button(
                    "Retry loading timer",
                    super::Message::Timer(Message::RetryLoad),
                    !self.loading,
                ));
        }
        if let Some(error) = &self.save_error {
            content = content
                .push(super::error_text(error))
                .push(super::action_button(
                    "Retry saving timer",
                    super::Message::Timer(Message::RetrySave),
                    self.pending_save.is_none(),
                ));
        }
        if let Some(error) = &self.clock_error {
            content = content.push(super::error_text(error));
        }
        if let Some(error) = &self.notification_error {
            content = content.push(super::error_text(error));
        }
        content.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded(seconds: u32) -> TimerUi {
        let mut ui = TimerUi::default();
        let _ = ui.update(
            Message::Loaded(Ok(timer::Settings {
                duration_seconds: seconds,
                sound: false,
            })),
            false,
        );
        ui
    }

    #[test]
    fn rapid_edits_coalesce_to_the_latest_preset_without_overlapping_writes() {
        let mut ui = loaded(5);
        for seconds in [4, 40, 2400] {
            let _ = ui.update(
                Message::Input(duration_input::Message::Changed(seconds)),
                false,
            );
        }
        assert_eq!(ui.pending_save.unwrap().duration_seconds, 4);
        assert_eq!(ui.settings.duration_seconds, 2400);
        let _ = ui.update(Message::Saved(Ok(())), false);
        assert_eq!(ui.confirmed.unwrap().duration_seconds, 4);
        assert_eq!(ui.pending_save.unwrap().duration_seconds, 2400);
        let _ = ui.update(Message::Saved(Ok(())), false);
        assert_eq!(ui.confirmed.unwrap().duration_seconds, 2400);
        assert!(ui.pending_save.is_none());
    }

    #[test]
    fn failed_save_keeps_the_new_preset_and_retries_the_latest_value() {
        let mut ui = loaded(5);
        let _ = ui.update(Message::Input(duration_input::Message::Changed(40)), false);
        let _ = ui.update(
            Message::Input(duration_input::Message::Changed(2400)),
            false,
        );
        let _ = ui.update(
            Message::Saved(Err("Read-only settings directory".into())),
            false,
        );
        assert_eq!(ui.confirmed.unwrap().duration_seconds, 5);
        assert_eq!(ui.settings.duration_seconds, 2400);
        assert!(ui.pending_save.is_none());
        assert!(ui.save_error.is_some());
        let _ = ui.update(Message::RetrySave, false);
        assert_eq!(ui.pending_save.unwrap().duration_seconds, 2400);
    }

    #[test]
    fn unreadable_settings_block_changes_instead_of_overwriting_saved_data() {
        let mut ui = TimerUi::default();
        let _ = ui.update(
            Message::Loaded(Err("Unsupported settings version".into())),
            false,
        );
        let _ = ui.update(Message::Input(duration_input::Message::Changed(60)), false);
        let _ = ui.update(Message::Sound, false);
        let _ = ui.update(Message::Play, false);
        assert!(!ui.ready);
        assert!(ui.load_error.is_some());
        assert_eq!(ui.settings, timer::Settings::default());
        assert!(ui.pending_save.is_none());
        assert_eq!(ui.countdown.status(), timer::Status::Idle);
    }

    #[test]
    fn reset_stops_a_paused_timer_and_restores_the_configured_duration() {
        let mut ui = loaded(3600);
        ui.countdown.start(Duration::from_secs(10));
        assert!(!ui.countdown.pause(Duration::from_secs(20)));
        ui.displayed_seconds = 3590;
        let _ = ui.update(Message::Reset, false);
        assert_eq!(ui.countdown.status(), timer::Status::Idle);
        assert_eq!(ui.displayed_seconds, 3600);
        assert_eq!(ui.confirmed.unwrap().duration_seconds, 3600);
        assert!(ui.pending_save.is_none());
    }

    #[test]
    fn entering_the_same_preset_after_expiry_resets_the_zero_display() {
        let mut ui = loaded(5);
        ui.countdown.start(Duration::ZERO);
        assert!(ui.countdown.tick(Duration::from_secs(5)));
        ui.displayed_seconds = 0;
        let _ = ui.update(Message::Input(duration_input::Message::Changed(5)), false);
        assert_eq!(ui.countdown.status(), timer::Status::Idle);
        assert_eq!(ui.displayed_seconds, 5);
        assert!(ui.pending_save.is_none());
    }
}
