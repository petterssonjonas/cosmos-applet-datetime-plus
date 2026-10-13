// SPDX-License-Identifier: GPL-3.0-only
//! Persistent normal desktop windows for unresolved local agenda reminders.

mod ipc;

use std::sync::Arc;

use chrono::{Local, TimeDelta, Utc};
use cosmic::{
    app::ApplicationExt,
    iced::{Length, Limits, Size, Subscription, window},
    prelude::*,
    widget,
};

use crate::{
    agenda::{self, Change, Entry, Kind, Snapshot},
    display, storage,
};

const DEFAULT_SIZE: Size = Size::new(820.0, 710.0);

pub(crate) use ipc::{exits, present};

pub(crate) fn run(id: u64) -> Result<(), String> {
    if id == 0 {
        return Err("A reminder entry ID must be nonzero.".into());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("Could not initialize the reminder application: {error}"))?;
    let flags = runtime.block_on(async {
        let path = storage::data_path("agenda.json")?;
        let Some(server) = ipc::claim(&path, id).await? else {
            // A launch racing an existing notifier must not create a second native window.
            ipc::activate_existing(&path, id).await?;
            return Ok::<_, String>(None);
        };
        let snapshot = agenda::load().await?;
        if !snapshot
            .entries
            .iter()
            .any(|entry| entry.id == id && entry.needs_window(Utc::now()))
        {
            return Ok(None);
        }
        Ok(Some(Flags {
            id,
            snapshot,
            server,
        }))
    })?;
    drop(runtime);
    let Some(flags) = flags else {
        return Ok(());
    };
    cosmic::app::run::<Notifier>(
        cosmic::app::Settings::default()
            .size(DEFAULT_SIZE)
            .size_limits(
                Limits::NONE
                    .min_width(DEFAULT_SIZE.width)
                    .min_height(DEFAULT_SIZE.height)
                    .max_width(DEFAULT_SIZE.width)
                    .max_height(DEFAULT_SIZE.height),
            )
            .transparent(false)
            .resizable(None)
            .exit_on_close(false)
            .is_daemon(false),
        flags,
    )
    .map_err(|error| format!("The native reminder window failed: {error}"))
}

struct Flags {
    id: u64,
    snapshot: Snapshot,
    server: Arc<ipc::Server>,
}

#[derive(Clone, Debug)]
enum Message {
    Agenda(agenda::Event),
    Opened(window::Id),
    Closed(window::Id),
    CloseAttempt,
    Activate,
    TransportError(String),
    GeometryChanged,
    ScreenSize(Option<(f32, f32)>),
    RequestDelete,
    CancelDelete,
    Delete,
    Finish,
    Snooze(u32),
    Minutes(String),
    CustomSnooze,
    Applied(Result<Snapshot, String>),
    Reload,
    Reloaded(Result<Snapshot, String>),
}

struct Notifier {
    core: cosmic::Core,
    id: u64,
    snapshot: Snapshot,
    size: Size,
    server: Arc<ipc::Server>,
    valid: bool,
    saving: bool,
    deleting: Option<u64>,
    action_failed: bool,
    minutes: String,
    error: Option<String>,
}

impl Notifier {
    fn entry(&self) -> Option<&Entry> {
        self.snapshot
            .entries
            .iter()
            .find(|entry| entry.id == self.id)
    }

    fn enabled(&self) -> bool {
        self.valid && !self.saving && self.entry().is_some_and(|entry| entry.reminder_pending)
    }

    fn should_close(&self) -> bool {
        !self.saving
            && !self.action_failed
            && !self
                .entry()
                .is_some_and(|entry| entry.needs_window(Utc::now()))
    }

    fn receive(&mut self, snapshot: Snapshot, from_storage: bool) -> Task<cosmic::Action<Message>> {
        // A fresh read also recognizes external deletion or restoration of an older file.
        // Mutation replies must not replace a newer watcher snapshot.
        if from_storage || snapshot.revision >= self.snapshot.revision {
            if self
                .deleting
                .is_some_and(|revision| revision != snapshot.revision)
            {
                self.deleting = None;
                self.error =
                    Some("The agenda changed. Review this entry and confirm Delete again.".into());
            }
            self.snapshot = snapshot;
        }
        self.valid = true;
        if self.should_close() {
            return cosmic::iced::exit();
        }
        Task::none()
    }

    fn apply(&mut self, change: Change) -> Task<cosmic::Action<Message>> {
        if !self.enabled()
            && !(matches!(&change, Change::Finish(_))
                && self.action_failed
                && self.valid
                && !self.saving)
        {
            return Task::none();
        }
        self.saving = true;
        self.error = None;
        let revision = self.snapshot.revision;
        Task::perform(agenda::apply(revision, change), |result| {
            cosmic::Action::App(Message::Applied(result))
        })
    }

    fn snooze(&mut self, minutes: u32) -> Task<cosmic::Action<Message>> {
        if minutes == 0 || minutes > 525_600 {
            self.error = Some("Enter 1–525600 minutes to postpone this reminder.".into());
            return Task::none();
        }
        let Some(until) = Utc::now().checked_add_signed(TimeDelta::minutes(i64::from(minutes)))
        else {
            self.error = Some("The snooze deadline is outside the supported date range.".into());
            return Task::none();
        };
        self.apply(Change::Snooze(self.id, until))
    }
}

impl cosmic::Application for Notifier {
    type Executor = cosmic::executor::Default;
    type Flags = Flags;
    type Message = Message;
    const APP_ID: &'static str = "dev.jp.CosmicAppletDatetimePlus.TaskNotifier";

    fn core(&self) -> &cosmic::Core {
        &self.core
    }
    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    fn init(mut core: cosmic::Core, flags: Flags) -> (Self, Task<cosmic::Action<Message>>) {
        core.window.show_close = false;
        core.window.show_maximize = false;
        let mut app = Self {
            core,
            id: flags.id,
            snapshot: flags.snapshot,
            server: flags.server,
            valid: true,
            saving: false,
            deleting: None,
            action_failed: false,
            minutes: "30".into(),
            error: None,
            size: DEFAULT_SIZE,
        };
        app.set_header_title("Reminder".to_owned());
        let task = if let Some(id) = app.core.main_window_id() {
            app.set_window_title("Datetime Plus — Reminder".to_owned(), id)
        } else {
            Task::none()
        };
        (app, task)
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            agenda::read_only_subscription().map(Message::Agenda),
            ipc::subscription(self.server.clone()),
            window::open_events().map(Message::Opened),
            window::close_events().map(Message::Closed),
            window::close_requests().map(|_| Message::CloseAttempt),
            display::changes().map(|()| Message::GeometryChanged),
        ])
    }

    fn on_app_exit(&mut self) -> Option<Message> {
        Some(Message::CloseAttempt)
    }

    fn on_close_requested(&self, _id: window::Id) -> Option<Message> {
        Some(Message::CloseAttempt)
    }

    fn update(&mut self, message: Message) -> Task<cosmic::Action<Message>> {
        match message {
            Message::Opened(id) if self.core.main_window_id() == Some(id) => {
                self.server.opened.send_replace(true);
                return window::set_resizable(id, false).chain(
                    display::size(&self.core)
                        .map(|size| cosmic::Action::App(Message::ScreenSize(size))),
                );
            }
            Message::Closed(id) if self.core.main_window_id() == Some(id) => {
                // Forced compositor/session shutdown is never blocked or treated as resolution.
                self.server.opened.send_replace(false);
                return cosmic::iced::exit();
            }
            Message::CloseAttempt => {
                self.error = Some("Resolve this reminder with Finished, Snooze, or Delete. You can minimize the window.".into());
            }
            Message::Activate => {
                if let Some(id) = self.core.main_window_id() {
                    return Task::batch([window::minimize(id, false), window::gain_focus(id)]);
                }
            }
            Message::Agenda(agenda::Event::Loaded(snapshot)) => {
                return self.receive(snapshot, true);
            }
            Message::Agenda(agenda::Event::Error(error)) => {
                self.valid = false;
                self.error = Some(error);
            }
            Message::GeometryChanged => {
                return display::size(&self.core)
                    .map(|size| cosmic::Action::App(Message::ScreenSize(size)));
            }
            Message::ScreenSize(Some(monitor)) => {
                let size = fit_size(monitor);
                if size != self.size
                    && let Some(id) = self.core.main_window_id()
                {
                    self.size = size;
                    return window::set_max_size(id, None)
                        .chain(window::set_min_size(id, Some(size)))
                        .chain(window::set_max_size(id, Some(size)))
                        .chain(window::resize(id, size));
                }
            }
            Message::TransportError(error) => {
                self.error = Some(error);
            }
            Message::RequestDelete if self.enabled() => {
                self.deleting = Some(self.snapshot.revision);
            }
            Message::CancelDelete if !self.saving => {
                self.deleting = None;
            }
            Message::Delete if self.deleting == Some(self.snapshot.revision) => {
                return self.apply(Change::Delete(self.id));
            }
            Message::Finish => return self.apply(Change::Finish(self.id)),
            Message::Snooze(minutes) => return self.snooze(minutes),
            Message::Minutes(value) if !self.saving => {
                self.minutes = value;
            }
            Message::CustomSnooze => match self.minutes.trim().parse() {
                Ok(minutes) => return self.snooze(minutes),
                Err(_) => {
                    self.error = Some("Enter a whole number of minutes for Snooze.".into());
                }
            },
            Message::Applied(result) => {
                self.saving = false;
                match result {
                    Ok(snapshot) => {
                        self.action_failed = false;
                        return self.receive(snapshot, false);
                    }
                    Err(error) => {
                        self.action_failed = true;
                        self.deleting = None;
                        self.error = Some(error);
                        // Recover the latest optimistic revision; never retry a mutation automatically.
                        return Task::perform(agenda::load(), |result| {
                            cosmic::Action::App(Message::Reloaded(result))
                        });
                    }
                }
            }
            Message::Reload if !self.saving => {
                return Task::perform(agenda::load(), |result| {
                    cosmic::Action::App(Message::Reloaded(result))
                });
            }
            Message::Reloaded(Ok(snapshot)) => return self.receive(snapshot, true),
            Message::Reloaded(Err(error)) => {
                self.valid = false;
                self.error = Some(error);
            }
            _ => {}
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let enabled = self.enabled();
        let mut content = widget::column([]).spacing(16).width(Length::Fill);
        if let Some(entry) = self.entry() {
            content = content
                .push(widget::text::title3(if entry.kind == Kind::Task {
                    "Task reminder"
                } else {
                    "Event reminder"
                }))
                .push(widget::text::title2(&entry.title))
                .push(widget::text(format!(
                    "Scheduled: {}",
                    entry
                        .start
                        .with_timezone(&Local)
                        .format("%a %-d %b %Y, %H:%M")
                )));
            if let Some(until) = entry.snoozed_until {
                content = content.push(widget::text(format!(
                    "Postponed reminder: {}",
                    until.with_timezone(&Local).format("%a %-d %b, %H:%M")
                )));
            }
            if !entry.reminder_pending && !self.action_failed {
                content = content.push(widget::text(
                    "Saving the opened reminder before actions become available…",
                ));
            }
        }
        if self.action_failed
            && !self
                .entry()
                .is_some_and(|entry| entry.needs_window(Utc::now()))
        {
            content = content.push(widget::text("Resolution is visible, but the save reported an error. Retry Finished to safely confirm the saved state."));
        }
        if self.deleting.is_some() {
            content = content
                .push(widget::text(
                    "Delete this saved entry? This cannot be undone.",
                ))
                .push(
                    widget::row([
                        button("Cancel", Message::CancelDelete, !self.saving),
                        button("Confirm delete", Message::Delete, enabled),
                    ])
                    .spacing(8),
                );
        } else {
            content = content
                .push(
                    widget::row([
                        button(
                            "Finished",
                            Message::Finish,
                            enabled || (self.action_failed && self.valid && !self.saving),
                        ),
                        button(
                            if self.entry().is_some_and(|entry| entry.kind == Kind::Event) {
                                "Delete event"
                            } else {
                                "Delete task"
                            },
                            Message::RequestDelete,
                            enabled,
                        ),
                    ])
                    .spacing(8),
                )
                .push(widget::text(
                    "Snooze reminder (original scheduled time stays unchanged):",
                ))
                .push(
                    widget::row([
                        button("5 min", Message::Snooze(5), enabled),
                        button("15 min", Message::Snooze(15), enabled),
                        button("1 hour", Message::Snooze(60), enabled),
                    ])
                    .spacing(8),
                )
                .push(
                    widget::row([
                        widget::text_input("Minutes", &self.minutes)
                            .on_input(Message::Minutes)
                            .on_submit_maybe(enabled.then_some(|_| Message::CustomSnooze))
                            .width(Length::Fill)
                            .into(),
                        button("Snooze", Message::CustomSnooze, enabled),
                    ])
                    .spacing(8),
                );
        }
        if self.saving {
            content = content.push(widget::text("Saving…"));
        }
        if let Some(error) = &self.error {
            content = content.push(widget::text(error)).push(button(
                "Reload agenda",
                Message::Reload,
                !self.saving,
            ));
        }
        widget::container(widget::scrollable(content))
            .padding(20)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        let theme = cosmic::theme::active();
        let background = theme.cosmic().background(false);
        Some(cosmic::iced::theme::Style {
            background_color: background.base.into(),
            icon_color: background.on.into(),
            text_color: background.on.into(),
        })
    }
}

fn button(label: &str, message: Message, enabled: bool) -> Element<'_, Message> {
    widget::button::text(label)
        .on_press_maybe(enabled.then_some(message))
        .into()
}

fn fit_size((width, height): (f32, f32)) -> Size {
    Size::new(
        DEFAULT_SIZE.width.min(width * 0.9),
        DEFAULT_SIZE.height.min(height * 0.75),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::Application;

    #[test]
    fn reminder_dimensions_fit_small_logical_monitors_without_using_physical_pixels() {
        assert_eq!(fit_size((400.0, 400.0)), Size::new(360.0, 300.0));
    }

    #[tokio::test]
    async fn a_save_error_keeps_the_window_even_when_the_mutation_is_already_visible() {
        let directory = agenda::tests::Directory::new();
        let server = ipc::claim(&directory.agenda(), 1).await.unwrap().unwrap();
        let (mut app, initial) = Notifier::init(
            cosmic::Core::default(),
            Flags {
                id: 1,
                snapshot: Snapshot::default(),
                server,
            },
        );
        drop(initial);
        app.saving = true;
        drop(app.receive(Snapshot::default(), true));
        assert!(!app.should_close());
        drop(app.update(Message::Applied(Err("isolated save failure".into()))));
        assert!(app.action_failed);
        assert!(!app.should_close());
        drop(app.receive(Snapshot::default(), true));
        assert!(!app.should_close());
        drop(app.update(Message::Applied(Ok(Snapshot::default()))));
        assert!(app.should_close());
    }

    #[tokio::test]
    async fn external_edits_cancel_stale_delete_confirmation_and_file_deletion_closes_the_reminder()
    {
        let directory = agenda::tests::Directory::new();
        let server = ipc::claim(&directory.agenda(), 1).await.unwrap().unwrap();
        let snapshot = Snapshot {
            revision: 9,
            entries: vec![Entry {
                id: 1,
                kind: Kind::Task,
                title: "Original task".into(),
                start: chrono::DateTime::<Utc>::MIN_UTC,
                reminder_minutes: 0,
                completed: false,
                notified: Some(chrono::DateTime::<Utc>::MIN_UTC),
                snoozed_until: None,
                reminder_pending: true,
            }],
        };
        let (mut app, initial) = Notifier::init(
            cosmic::Core::default(),
            Flags {
                id: 1,
                snapshot: snapshot.clone(),
                server,
            },
        );
        drop(initial);
        app.deleting = Some(snapshot.revision);
        let mut edited = snapshot;
        edited.revision += 1;
        edited.entries[0].title = "Updated task".into();
        drop(app.receive(edited, true));
        assert_eq!(app.entry().unwrap().title, "Updated task");
        assert_eq!(app.deleting, None);
        assert!(!app.should_close());
        drop(app.receive(Snapshot::default(), true));
        assert!(app.should_close());
    }
}
