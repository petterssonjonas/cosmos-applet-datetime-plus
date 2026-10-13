// SPDX-License-Identifier: GPL-3.0-only

use super::{Message as AppMessage, action_button, choice_button, error_text};
use crate::agenda::{self, Change, Entry, Kind, Snapshot};
use chrono::{Local, NaiveDate, NaiveTime, TimeZone, Utc};
use cosmic::{
    iced::{Alignment, Length},
    prelude::*,
    widget,
};

#[derive(Clone)]
pub enum Message {
    Event(agenda::Event),
    Retry,
    Loaded(u64, Result<Snapshot, String>),
    New(Kind, NaiveDate),
    Edit(u64),
    Title(String),
    Date(String),
    Time(String),
    Reminder(String),
    SecondOccurrence(bool),
    Completed(bool),
    Save,
    Cancel,
    RequestDelete(u64),
    CancelDelete,
    ConfirmDelete,
    Complete(u64, bool),
    Applied(u64, Result<Snapshot, String>),
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Event(_) => "AgendaEvent",
            Self::Retry => "RetryAgenda",
            Self::Loaded(..) => "AgendaLoaded",
            Self::New(..) => "NewEntry",
            Self::Edit(_) => "EditEntry",
            Self::Title(_) => "EntryTitle",
            Self::Date(_) => "EntryDate",
            Self::Time(_) => "EntryTime",
            Self::Reminder(_) => "EntryReminder",
            Self::SecondOccurrence(_) => "SecondOccurrence",
            Self::Completed(_) => "EntryCompleted",
            Self::Save => "SaveEntry",
            Self::Cancel => "CancelEntry",
            Self::RequestDelete(_) => "RequestDeleteEntry",
            Self::CancelDelete => "CancelDeleteEntry",
            Self::ConfirmDelete => "ConfirmDeleteEntry",
            Self::Complete(..) => "CompleteEntry",
            Self::Applied(..) => "AgendaApplied",
        })
    }
}

struct Editor {
    id: Option<u64>,
    kind: Kind,
    title: String,
    date: String,
    time: String,
    reminder: String,
    completed: bool,
    second_occurrence: bool,
    revision: u64,
    original: Option<Entry>,
    title_id: widget::Id,
    date_id: widget::Id,
    time_id: widget::Id,
    reminder_id: widget::Id,
}

impl Editor {
    fn new(kind: Kind, date: NaiveDate, revision: u64) -> Self {
        Self {
            id: None,
            kind,
            title: String::new(),
            date: date.to_string(),
            time: "09:00".into(),
            reminder: "0".into(),
            completed: false,
            second_occurrence: false,
            revision,
            original: None,
            title_id: widget::Id::unique(),
            date_id: widget::Id::unique(),
            time_id: widget::Id::unique(),
            reminder_id: widget::Id::unique(),
        }
    }

    fn from_entry(entry: &Entry, revision: u64) -> Self {
        let local = entry.start.with_timezone(&Local);
        let mut editor = Self::new(entry.kind, local.date_naive(), revision);
        editor.id = Some(entry.id);
        editor.title = entry.title.clone();
        editor.time = local.format("%H:%M:%S").to_string();
        editor.reminder = entry.reminder_minutes.to_string();
        editor.completed = entry.completed;
        editor.second_occurrence = matches!(Local.from_local_datetime(&local.naive_local()),
            chrono::LocalResult::Ambiguous(_, second) if second.with_timezone(&Utc) == entry.start);
        editor.original = Some(entry.clone());
        editor
    }

    fn local_time(&self) -> Result<(NaiveDate, NaiveTime), String> {
        let date = NaiveDate::parse_from_str(&self.date, "%Y-%m-%d")
            .map_err(|_| "Enter a valid date as YYYY-MM-DD.".to_owned())?;
        let time = NaiveTime::parse_from_str(&self.time, "%H:%M")
            .or_else(|_| NaiveTime::parse_from_str(&self.time, "%H:%M:%S"))
            .map_err(|_| "Enter a valid local time as HH:MM (24-hour clock).".to_owned())?;
        Ok((date, time))
    }

    fn draft(&self) -> Result<agenda::Draft, String> {
        let (date, time) = self.local_time()?;
        let reminder_minutes =
            self.reminder.trim().parse::<u32>().map_err(|_| {
                "Reminder minutes must be a whole number from 0 to 525600.".to_owned()
            })?;
        if reminder_minutes > 525_600 {
            return Err("Reminder minutes must be between 0 and 525600.".into());
        }
        if self.title.trim().is_empty() {
            return Err("Enter an event or task title.".into());
        }
        Ok(agenda::Draft {
            id: self.id,
            kind: self.kind,
            title: self.title.trim().to_owned(),
            start: agenda::resolve_local(date, time, self.second_occurrence)?,
            reminder_minutes,
            completed: self.kind == Kind::Task && self.completed,
        })
    }
}

#[derive(Default)]
pub struct AgendaUi {
    snapshot: Snapshot,
    ready: bool,
    loading: bool,
    load_generation: u64,
    mutation_generation: u64,
    pending: bool,
    error: Option<String>,
    editor: Option<Editor>,
    delete: Option<(u64, u64)>,
    show_day: bool,
}

fn message(message: Message) -> AppMessage {
    AppMessage::Agenda(message)
}

impl AgendaUi {
    pub fn editing(&self) -> bool {
        self.editor.is_some()
    }

    pub fn select_date(&mut self) {
        self.show_day = true;
    }

    pub fn has_tasks(&self, date: NaiveDate) -> bool {
        self.snapshot.entries.iter().any(|entry| {
            entry.kind == Kind::Task && entry.start.with_timezone(&Local).date_naive() == date
        })
    }

    pub fn add_task_button(&self, date: NaiveDate) -> Element<'_, AppMessage> {
        action_button(
            "Add task",
            message(Message::New(Kind::Task, date)),
            self.ready && !self.pending && !self.loading,
        )
    }

    fn receive(&mut self, snapshot: Snapshot) {
        if self.ready && snapshot.revision < self.snapshot.revision {
            return;
        }
        // Unrelated entries and reminder delivery may advance the revision, but
        // never replace typed input or authorize overwriting concurrent edits.
        if let Some(editor) = &mut self.editor {
            let unchanged = match &editor.original {
                None => true,
                Some(original) => snapshot
                    .entries
                    .iter()
                    .find(|entry| entry.id == original.id)
                    .is_some_and(|entry| {
                        entry.kind == original.kind
                            && entry.title == original.title
                            && entry.start == original.start
                            && entry.reminder_minutes == original.reminder_minutes
                            && entry.completed == original.completed
                            && entry.snoozed_until == original.snoozed_until
                    }),
            };
            if unchanged {
                editor.revision = snapshot.revision;
            }
        }
        self.snapshot = snapshot;
        self.ready = true;
    }

    fn load(&mut self) -> Task<cosmic::Action<AppMessage>> {
        if self.loading || self.pending {
            return Task::none();
        }
        self.loading = true;
        self.load_generation = self.load_generation.wrapping_add(1);
        let generation = self.load_generation;
        Task::perform(agenda::load(), move |result| {
            cosmic::Action::App(message(Message::Loaded(generation, result)))
        })
    }

    fn apply(&mut self, revision: u64, change: Change) -> Task<cosmic::Action<AppMessage>> {
        if !self.ready || self.pending || self.loading {
            return Task::none();
        }
        self.pending = true;
        self.error = None;
        self.mutation_generation = self.mutation_generation.wrapping_add(1);
        let generation = self.mutation_generation;
        Task::perform(agenda::apply(revision, change), move |result| {
            cosmic::Action::App(message(Message::Applied(generation, result)))
        })
    }

    pub fn update(&mut self, event: Message) -> Task<cosmic::Action<AppMessage>> {
        match event {
            Message::Event(agenda::Event::Loaded(snapshot)) => {
                self.receive(snapshot);
                if self.editor.is_none() && self.delete.is_none() {
                    self.error = None;
                }
            }
            Message::Event(agenda::Event::Error(error)) => self.error = Some(error),
            Message::Event(agenda::Event::Delivered(_)) => {}
            Message::Retry => {
                self.error = None;
                return self.load();
            }
            Message::Loaded(generation, result) => {
                if generation != self.load_generation || !self.loading {
                    return Task::none();
                }
                self.loading = false;
                match result {
                    Ok(snapshot) => self.receive(snapshot),
                    Err(error) => self.error = Some(error),
                }
            }
            Message::New(kind, date) if self.ready && !self.pending && self.editor.is_none() => {
                self.editor = Some(Editor::new(kind, date, self.snapshot.revision));
                self.delete = None;
                self.error = None;
                self.show_day = true;
            }
            Message::Edit(id) if !self.pending && self.editor.is_none() => {
                if let Some(entry) = self.snapshot.entries.iter().find(|entry| entry.id == id) {
                    self.editor = Some(Editor::from_entry(entry, self.snapshot.revision));
                    self.delete = None;
                    self.error = None;
                }
            }
            Message::Title(value) if !self.pending => {
                if let Some(editor) = &mut self.editor {
                    editor.title = value;
                }
            }
            Message::Date(value) if !self.pending => {
                if let Some(editor) = &mut self.editor {
                    editor.date = value;
                }
            }
            Message::Time(value) if !self.pending => {
                if let Some(editor) = &mut self.editor {
                    editor.time = value;
                }
            }
            Message::Reminder(value) if !self.pending => {
                if let Some(editor) = &mut self.editor {
                    editor.reminder = value;
                }
            }
            Message::SecondOccurrence(value) if !self.pending => {
                if let Some(editor) = &mut self.editor {
                    editor.second_occurrence = value;
                }
            }
            Message::Completed(value) if !self.pending => {
                if let Some(editor) = &mut self.editor {
                    editor.completed = value;
                }
            }
            Message::Save if !self.pending => {
                if let Some(editor) = &self.editor {
                    let revision = editor.revision;
                    match editor.draft() {
                        Ok(draft) => return self.apply(revision, Change::Upsert(draft)),
                        Err(error) => self.error = Some(error),
                    }
                }
            }
            Message::Cancel if !self.pending => {
                self.editor = None;
                self.error = None;
            }
            Message::RequestDelete(id) if !self.pending && self.editor.is_none() => {
                if self.snapshot.entries.iter().any(|entry| entry.id == id) {
                    self.delete = Some((id, self.snapshot.revision));
                }
            }
            Message::CancelDelete if !self.pending => self.delete = None,
            Message::ConfirmDelete if !self.pending => {
                if let Some((id, revision)) = self.delete {
                    return self.apply(revision, Change::Delete(id));
                }
            }
            Message::Complete(id, completed) if !self.pending && self.editor.is_none() => {
                if self
                    .snapshot
                    .entries
                    .iter()
                    .any(|entry| entry.id == id && entry.kind == Kind::Task)
                {
                    return self.apply(self.snapshot.revision, Change::Complete(id, completed));
                }
            }
            Message::Applied(generation, result) => {
                if generation != self.mutation_generation || !self.pending {
                    return Task::none();
                }
                self.pending = false;
                match result {
                    Ok(snapshot) => {
                        self.receive(snapshot);
                        self.editor = None;
                        self.delete = None;
                        self.error = None;
                    }
                    Err(error) => {
                        self.error = Some(error);
                        // Refresh revisions without discarding the editor on failure.
                        return self.load();
                    }
                }
            }
            _ => {}
        }
        Task::none()
    }

    pub fn day_view(&self, date: NaiveDate, twenty_four_hour: bool) -> Element<'_, AppMessage> {
        let enabled = self.ready && !self.pending && !self.loading;
        let mut content = widget::column([]).spacing(7).width(Length::Fill);
        if !self.ready {
            content = content.push(
                widget::text(if self.error.is_some() {
                    "Agenda could not be loaded."
                } else {
                    "Loading local agenda…"
                })
                .size(12),
            );
        }
        if let Some(error) = &self.error {
            content = content.push(error_text(error));
        }
        if self.error.is_some() {
            content = content.push(action_button(
                "Reload agenda",
                message(Message::Retry),
                !self.loading && !self.pending,
            ));
        }
        if !self.show_day {
            return content.into();
        }
        let mut entries: Vec<_> = self
            .snapshot
            .entries
            .iter()
            .filter(|entry| entry.start.with_timezone(&Local).date_naive() == date)
            .collect();
        entries.sort_by_key(|entry| (entry.start, entry.id));
        for entry in entries {
            let local = entry.start.with_timezone(&Local);
            let time = local.format(if twenty_four_hour {
                "%H:%M"
            } else {
                "%-I:%M %p"
            });
            let mut item = widget::column([])
                .spacing(4)
                .push(widget::text(format!("{time} · {}", entry.title)).size(13));
            if entry.kind == Kind::Task {
                item =
                    item.push(
                        widget::toggler(entry.completed)
                            .label(Some("Completed".into()))
                            .text_size(12)
                            .on_toggle_maybe(enabled.then_some(move |value| {
                                message(Message::Complete(entry.id, value))
                            })),
                    );
            }
            item = item
                .push(
                    widget::text(if entry.completed {
                        "Completed task · reminder suppressed".to_owned()
                    } else {
                        format!(
                            "{} · reminder {} min before",
                            if entry.kind == Kind::Task {
                                "Task"
                            } else {
                                "Event"
                            },
                            entry.reminder_minutes
                        )
                    })
                    .size(11),
                )
                .push(
                    widget::row([
                        action_button("Edit", message(Message::Edit(entry.id)), enabled),
                        action_button("Delete", message(Message::RequestDelete(entry.id)), enabled),
                    ])
                    .spacing(6),
                );
            if self.delete.is_some_and(|(id, _)| id == entry.id) {
                item = item
                    .push(widget::text("Delete this saved entry? This cannot be undone.").size(12))
                    .push(
                        widget::row([
                            action_button("Cancel", message(Message::CancelDelete), !self.pending),
                            action_button(
                                "Confirm delete",
                                message(Message::ConfirmDelete),
                                enabled,
                            ),
                        ])
                        .spacing(6),
                    );
            }
            content = content.push(super::card(item));
        }
        if self.pending {
            content = content.push(widget::text("Saving agenda…").size(12));
        }
        content.into()
    }

    pub fn editor_view(&self) -> Element<'_, AppMessage> {
        let Some(editor) = &self.editor else {
            return widget::column([]).into();
        };
        let enabled = !self.pending;
        let mut title = widget::text_input("Title", &editor.title)
            .label("Title")
            .id(editor.title_id.clone());
        let mut date = widget::text_input("YYYY-MM-DD", &editor.date)
            .label("Local date")
            .id(editor.date_id.clone());
        let mut time = widget::text_input("HH:MM", &editor.time)
            .label("Local time (24-hour)")
            .id(editor.time_id.clone());
        let mut reminder = widget::text_input("0", &editor.reminder)
            .label("Reminder minutes before start")
            .id(editor.reminder_id.clone());
        if enabled {
            title = title.on_input(|value| message(Message::Title(value)));
            date = date.on_input(|value| message(Message::Date(value)));
            time = time.on_input(|value| message(Message::Time(value)));
            reminder = reminder.on_input(|value| message(Message::Reminder(value)));
        }
        let mut body = widget::column([]).spacing(14).width(Length::Fill)
            .push(title).push(date).push(time).push(reminder)
            .push(widget::text("0 sends a reminder at the chosen time. Maximum: 525600 minutes (365 days). Reminders work while this applet runs, even with the popup closed. Times use your system timezone.").size(12));
        if editor.kind == Kind::Task {
            body = body.push(
                widget::toggler(editor.completed)
                    .label(Some("Completed (suppress reminder)".into()))
                    .on_toggle_maybe(enabled.then_some(|value| message(Message::Completed(value)))),
            );
        }
        if let Ok((date, time)) = editor.local_time() {
            match Local.from_local_datetime(&date.and_time(time)) {
                chrono::LocalResult::Ambiguous(first, second) => {
                    body = body.push(widget::text(format!("This local time occurs twice: {} and {}.", first.format("%Z %:z"), second.format("%Z %:z"))).size(12))
                        .push(widget::row([
                            choice_button("First occurrence", message(Message::SecondOccurrence(false)), !editor.second_occurrence, enabled),
                            choice_button("Second occurrence", message(Message::SecondOccurrence(true)), editor.second_occurrence, enabled),
                        ]).spacing(8));
                }
                chrono::LocalResult::None => body = body.push(error_text("This local time does not exist because the clocks change. Choose another time.")),
                chrono::LocalResult::Single(_) => {}
            }
        }
        if let Some(error) = &self.error {
            body = body.push(error_text(error));
        }
        if self.pending {
            body = body.push(widget::text("Saving agenda…").size(12));
        }
        widget::column([
            widget::text::title3(format!(
                "{} {}",
                if editor.id.is_some() { "Edit" } else { "New" },
                if editor.kind == Kind::Task {
                    "task"
                } else {
                    "event"
                }
            ))
            .into(),
            widget::scrollable(body).height(Length::Fill).into(),
            widget::row([
                widget::Space::new().width(Length::Fill).into(),
                action_button("Cancel", message(Message::Cancel), enabled),
                action_button(
                    "Save",
                    message(Message::Save),
                    enabled && self.ready && !self.loading,
                ),
            ])
            .spacing(8)
            .align_y(Alignment::Center)
            .into(),
        ])
        .spacing(16)
        .height(Length::Fill)
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> Entry {
        Entry {
            id: 1,
            kind: Kind::Task,
            title: "Saved".into(),
            start: Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap(),
            reminder_minutes: 10,
            completed: false,
            notified: None,
            snoozed_until: None,
            reminder_pending: false,
        }
    }

    #[test]
    fn backend_updates_preserve_typed_editor_and_advance_only_safe_revisions() {
        let mut ui = AgendaUi::default();
        ui.receive(Snapshot {
            revision: 1,
            entries: vec![entry()],
        });
        let _ = ui.update(Message::Edit(1));
        let _ = ui.update(Message::Title("Unsaved".into()));
        ui.receive(Snapshot {
            revision: 2,
            entries: vec![entry()],
        });
        assert_eq!(
            (
                &ui.editor.as_ref().unwrap().title,
                ui.editor.as_ref().unwrap().revision
            ),
            (&"Unsaved".to_owned(), 2)
        );
        let mut changed = entry();
        changed.title = "External edit".into();
        ui.receive(Snapshot {
            revision: 3,
            entries: vec![changed],
        });
        assert_eq!(ui.editor.as_ref().unwrap().revision, 2);
    }

    #[test]
    fn failed_and_stale_mutations_do_not_discard_editor() {
        let mut ui = AgendaUi::default();
        ui.receive(Snapshot {
            revision: 1,
            entries: vec![entry()],
        });
        let _ = ui.update(Message::Edit(1));
        ui.pending = true;
        ui.mutation_generation = 5;
        let _ = ui.update(Message::Applied(4, Ok(Snapshot::default())));
        assert!(ui.pending);
        let _ = ui.update(Message::Applied(5, Err("conflict".into())));
        assert!(ui.editor.is_some());
        assert!(!ui.pending);
    }

    #[test]
    fn reminder_input_rejects_negative_overflow_and_over_limit() {
        let mut editor = Editor::new(
            Kind::Event,
            NaiveDate::from_ymd_opt(2026, 10, 13).unwrap(),
            1,
        );
        editor.title = "Title".into();
        for value in ["-1", "4294967296", "525601"] {
            editor.reminder = value.into();
            assert!(editor.draft().is_err());
        }
    }

    #[test]
    fn calendar_marks_task_dates_without_marking_legacy_events() {
        let task = entry();
        let task_date = task.start.with_timezone(&Local).date_naive();
        let mut event = entry();
        event.id = 2;
        event.kind = Kind::Event;
        event.start += chrono::Duration::days(1);
        let event_date = event.start.with_timezone(&Local).date_naive();
        let mut ui = AgendaUi::default();
        ui.receive(Snapshot {
            revision: 1,
            entries: vec![task, event],
        });
        assert!(ui.has_tasks(task_date));
        assert!(!ui.has_tasks(event_date));
        let _ = ui.update(Message::Edit(2));
        assert_eq!(ui.editor.as_ref().unwrap().kind, Kind::Event);
    }

    #[test]
    fn adding_task_opens_editor_for_selected_date() {
        let mut ui = AgendaUi::default();
        ui.receive(Snapshot::default());
        let selected = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
        let _ = ui.update(Message::New(Kind::Task, selected));
        let editor = ui.editor.as_ref().unwrap();
        assert_eq!(
            (editor.kind, editor.date.as_str()),
            (Kind::Task, "2026-10-20")
        );
        assert!(
            ui.show_day,
            "The new task remains reachable after returning from its editor"
        );
    }

    #[test]
    fn concurrent_snooze_does_not_authorize_stale_editor_revision() {
        let mut ui = AgendaUi::default();
        ui.receive(Snapshot {
            revision: 1,
            entries: vec![entry()],
        });
        let _ = ui.update(Message::Edit(1));
        let mut snoozed = entry();
        snoozed.snoozed_until = Some(snoozed.start + chrono::Duration::minutes(30));
        ui.receive(Snapshot {
            revision: 2,
            entries: vec![snoozed],
        });
        assert_eq!(ui.editor.as_ref().unwrap().revision, 1);
    }
}
