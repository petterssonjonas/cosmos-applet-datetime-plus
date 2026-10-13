// SPDX-License-Identifier: GPL-3.0-only
// Popup/application wiring follows the GPL COSMIC applet conventions,
// Copyright System76 contributors. This applet does not replace the daemon.

#[path = "calendar.rs"]
mod calendar;

pub mod agenda_ui;

use crate::{
    agenda,
    config::{self, Preferences, Section, Side, TemperatureUnit, WorldClock},
    media, notifications, places,
};
use chrono::{DateTime, Datelike, FixedOffset, Local, NaiveDate, Offset, Utc};
use chrono_tz::Tz;
use cosmic::iced::platform_specific::shell::wayland::commands::popup::{destroy_popup, get_popup};
use cosmic::iced::{Alignment, Length, Limits, Subscription, window::Id};
use cosmic::prelude::*;
use cosmic::widget;
use cosmic_applet_datetime_plus::weather::{self, WeatherLocation};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

const POPUP_WIDTH: f32 = 820.0;
const INITIAL_POPUP_HEIGHT: f32 = 710.0;
const HISTORY_LIMIT: usize = 100;
const WEATHER_INTERVAL: Duration = Duration::from_secs(15 * 60);

struct HistoryEntry {
    notification: notifications::Notification,
    active: bool,
    revision: u64,
    unread: bool,
}

#[derive(Default)]
struct History {
    entries: VecDeque<HistoryEntry>,
    revision: u64,
    pending: HashMap<u32, u64>,
}

impl History {
    fn receive(&mut self, notification: notifications::Notification) {
        self.entries
            .retain(|entry| entry.notification.id != notification.id);
        self.pending.remove(&notification.id);
        self.revision = self.revision.wrapping_add(1);
        self.entries.push_front(HistoryEntry {
            notification,
            active: true,
            revision: self.revision,
            unread: true,
        });
        self.entries.truncate(HISTORY_LIMIT);
        self.pending.retain(|id, _| {
            self.entries
                .iter()
                .any(|entry| entry.notification.id == *id)
        });
    }

    fn unread(&self) -> bool {
        self.entries.iter().any(|entry| entry.unread)
    }

    fn mark_viewed(&mut self) {
        for entry in &mut self.entries {
            entry.unread = false;
        }
    }

    fn action_revision(&self, id: u32, key: &str) -> Option<u64> {
        self.entries
            .iter()
            .find(|entry| {
                entry.notification.id == id
                    && entry.active
                    && !self.pending.contains_key(&id)
                    && entry
                        .notification
                        .actions
                        .iter()
                        .any(|action| action.key == key)
            })
            .map(|entry| entry.revision)
    }

    fn close(&mut self, id: u32, reason: u32) {
        self.pending.remove(&id);
        self.entries.retain_mut(|entry| {
            if entry.notification.id != id {
                return true;
            }
            if reason == 1 && !entry.notification.transient {
                entry.active = false;
                true
            } else {
                false
            }
        });
    }

    fn disconnect(&mut self) {
        self.pending.clear();
        self.entries.retain_mut(|entry| {
            entry.active = false;
            !entry.notification.transient
        });
    }

    fn finish(
        &mut self,
        id: u32,
        revision: u64,
        dismiss: bool,
        result: &Result<(), String>,
    ) -> bool {
        if self.pending.get(&id) != Some(&revision) {
            return false;
        }
        self.pending.remove(&id);
        if dismiss && result.is_ok() {
            self.entries
                .retain(|entry| entry.notification.id != id || entry.revision != revision);
        }
        true
    }
}

pub struct AppModel {
    core: cosmic::Core,
    popup: Option<Id>,
    screen_size: Option<(f32, f32)>,
    screen_generation: u64,
    popup_pending: bool,
    now: DateTime<Local>,
    local_zone: Option<Tz>,
    local_zone_minute: i64,
    calendar: calendar::Calendar,
    agenda: agenda_ui::AgendaUi,
    preferences: Preferences,
    draft: Option<Preferences>,
    config_ready: bool,
    config_loading: bool,
    config_generation: u64,
    config_error: Option<String>,
    pending_save: Option<Preferences>,
    timezone_query: String,
    clock_label: String,
    timezone_matches: Vec<Tz>,
    place_query: String,
    place_matches: Vec<places::Place>,
    place_id: widget::Id,
    city_query: String,
    cities: Vec<WeatherLocation>,
    city_generation: u64,
    city_loading: bool,
    city_searched: bool,
    city_error: Option<String>,
    timezone_id: widget::Id,
    clock_label_id: widget::Id,
    city_id: widget::Id,
    history: History,
    notification_client: Option<Arc<notifications::Client>>,
    notification_error: Option<String>,
    notification_action_error: Option<String>,
    dnd: bool,
    dnd_known: bool,
    dnd_pending: Option<bool>,
    dnd_revision: u64,
    dnd_requested_revision: u64,
    dnd_error: Option<String>,
    players: Vec<media::Player>,
    media_pending: HashSet<String>,
    media_error: Option<String>,
    media_artwork: HashMap<PathBuf, Option<widget::image::Handle>>,
    artwork_requests: HashMap<PathBuf, u64>,
    artwork_revision: u64,
    weather_client: Option<Arc<weather::Client>>,
    weather_client_error: Option<String>,
    weather_launch_pending: bool,
    weather_launch_error: Option<String>,
    forecast: Option<weather::Forecast>,
    forecast_generation: u64,
    forecast_loading: bool,
    forecast_error: Option<String>,
    credit_error: Option<String>,
    weather_check: Option<Instant>,
}

#[derive(Clone)]
pub enum Message {
    TogglePopup,
    PopupClosed(Id),
    ScreenChanged,
    ScreenSize(u64, Option<(f32, f32)>),
    OverviewViewed(Id),
    Tick,
    WeatherTick,
    PreviousMonth,
    NextMonth,
    SelectDate(NaiveDate),
    Today,
    OpenSettings,
    Back,
    CancelSettings,
    SaveSettings,
    RetryConfig,
    ConfigLoaded(u64, Result<Preferences, String>),
    ConfigSaved(u64, Result<(), String>),
    TwentyFourHour(bool),
    ShowSeconds(bool),
    MondayFirst(bool),
    WeekNumbers(bool),
    Unit(TemperatureUnit),
    TimezoneQuery(String),
    ClockLabel(String),
    AddClock(Tz),
    RemoveClock(usize),
    PlaceQuery(String),
    AddPlace(places::Place),
    MoveClock(usize, bool),
    MoveSection(Section, bool),
    SectionSide(Section, Side),
    Agenda(agenda_ui::Message),
    CityQuery(String),
    SearchCities,
    CitiesLoaded(u64, Result<Vec<WeatherLocation>, String>),
    SelectCity(usize),
    RemoveCity,
    RefreshWeather,
    OpenWeather,
    WeatherOpened(Option<Id>, Result<(), String>),
    ForecastLoaded(u64, WeatherLocation, Result<weather::Forecast, String>),
    NotificationEvent(notifications::Event),
    DismissNotification(u32),
    NotificationAction(u32, String),
    NotificationFinished(u32, u64, bool, Result<(), String>),
    ClearNotifications,
    DoNotDisturb(bool),
    DndFinished(bool, Result<(), String>),
    MediaEvent(media::Event),
    MediaControl(String, media::Action),
    MediaFinished(String, Result<(), String>),
    ArtworkLoaded(PathBuf, u64, Option<widget::image::Handle>),
    OpenMetCredit,
    OpenGeoCredit,
    CreditOpened(Result<(), String>),
}

// Never put notification contents, city queries or media metadata in diagnostics.
impl std::fmt::Debug for Message {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::TogglePopup => "TogglePopup",
            Self::PopupClosed(_) => "PopupClosed",
            Self::ScreenChanged => "ScreenChanged",
            Self::ScreenSize(..) => "ScreenSize",
            Self::OverviewViewed(_) => "OverviewViewed",
            Self::Tick => "Tick",
            Self::WeatherTick => "WeatherTick",
            Self::PreviousMonth => "PreviousMonth",
            Self::NextMonth => "NextMonth",
            Self::SelectDate(_) => "SelectDate",
            Self::Today => "Today",
            Self::OpenSettings => "OpenSettings",
            Self::Back => "Back",
            Self::CancelSettings => "CancelSettings",
            Self::SaveSettings => "SaveSettings",
            Self::RetryConfig => "RetryConfig",
            Self::ConfigLoaded(..) => "ConfigLoaded",
            Self::ConfigSaved(..) => "ConfigSaved",
            Self::TwentyFourHour(_) => "TwentyFourHour",
            Self::ShowSeconds(_) => "ShowSeconds",
            Self::MondayFirst(_) => "MondayFirst",
            Self::WeekNumbers(_) => "WeekNumbers",
            Self::Unit(_) => "Unit",
            Self::TimezoneQuery(_) => "TimezoneQuery",
            Self::ClockLabel(_) => "ClockLabel",
            Self::AddClock(_) => "AddClock",
            Self::RemoveClock(_) => "RemoveClock",
            Self::PlaceQuery(_) => "PlaceQuery",
            Self::AddPlace(_) => "AddPlace",
            Self::MoveClock(..) => "MoveClock",
            Self::MoveSection(..) => "MoveSection",
            Self::SectionSide(..) => "SectionSide",
            Self::Agenda(_) => "Agenda",
            Self::CityQuery(_) => "CityQuery",
            Self::SearchCities => "SearchCities",
            Self::CitiesLoaded(..) => "CitiesLoaded",
            Self::SelectCity(_) => "SelectCity",
            Self::RemoveCity => "RemoveCity",
            Self::RefreshWeather => "RefreshWeather",
            Self::OpenWeather => "OpenWeather",
            Self::WeatherOpened(..) => "WeatherOpened",
            Self::ForecastLoaded(..) => "ForecastLoaded",
            Self::NotificationEvent(_) => "NotificationEvent",
            Self::DismissNotification(_) => "DismissNotification",
            Self::NotificationAction(..) => "NotificationAction",
            Self::NotificationFinished(..) => "NotificationFinished",
            Self::ClearNotifications => "ClearNotifications",
            Self::DoNotDisturb(_) => "DoNotDisturb",
            Self::DndFinished(..) => "DndFinished",
            Self::MediaEvent(_) => "MediaEvent",
            Self::MediaControl(..) => "MediaControl",
            Self::MediaFinished(..) => "MediaFinished",
            Self::ArtworkLoaded(..) => "ArtworkLoaded",
            Self::OpenMetCredit => "OpenMetCredit",
            Self::OpenGeoCredit => "OpenGeoCredit",
            Self::CreditOpened(_) => "CreditOpened",
        })
    }
}

impl AppModel {
    fn new(core: cosmic::Core) -> Self {
        let now = Local::now();
        let (weather_client, weather_client_error) = match weather::Client::new() {
            Ok(client) => (Some(Arc::new(client)), None),
            Err(error) => (None, Some(error)),
        };
        Self {
            core,
            popup: None,
            screen_size: None,
            screen_generation: 0,
            popup_pending: false,
            now,
            local_zone: system_timezone(),
            local_zone_minute: now.timestamp().div_euclid(60),
            calendar: calendar::Calendar::new(now.date_naive()),
            agenda: agenda_ui::AgendaUi::default(),
            preferences: Preferences::default(),
            draft: None,
            config_ready: false,
            config_loading: false,
            config_generation: 0,
            config_error: None,
            pending_save: None,
            timezone_query: String::new(),
            clock_label: String::new(),
            timezone_matches: Vec::new(),
            place_query: String::new(),
            place_matches: Vec::new(),
            place_id: widget::Id::unique(),
            city_query: String::new(),
            cities: Vec::new(),
            city_generation: 0,
            city_loading: false,
            city_searched: false,
            city_error: None,
            timezone_id: widget::Id::unique(),
            clock_label_id: widget::Id::unique(),
            city_id: widget::Id::unique(),
            history: History::default(),
            notification_client: None,
            notification_error: Some("Connecting to COSMIC notifications…".into()),
            notification_action_error: None,
            dnd: false,
            dnd_known: false,
            dnd_pending: None,
            dnd_error: None,
            dnd_revision: 0,
            dnd_requested_revision: 0,
            players: Vec::new(),
            media_pending: HashSet::new(),
            media_error: None,
            media_artwork: HashMap::new(),
            artwork_requests: HashMap::new(),
            artwork_revision: 0,
            weather_client,
            weather_client_error,
            weather_launch_pending: false,
            weather_launch_error: None,
            forecast: None,
            forecast_generation: 0,
            forecast_loading: false,
            forecast_error: None,
            credit_error: None,
            weather_check: None,
        }
    }

    fn popup_size(&self) -> (f32, f32) {
        self.screen_size
            .map(|(width, height)| (POPUP_WIDTH.min(width), height * 0.75))
            .unwrap_or((POPUP_WIDTH, INITIAL_POPUP_HEIGHT))
    }

    fn refresh_now(&mut self) {
        self.now = Local::now();
        let minute = self.now.timestamp().div_euclid(60);
        if minute != self.local_zone_minute {
            self.local_zone = system_timezone();
            self.local_zone_minute = minute;
        }
    }

    fn query_screen_size(&mut self) -> Task<cosmic::Action<Message>> {
        self.screen_generation = self.screen_generation.wrapping_add(1);
        let generation = self.screen_generation;
        crate::display::size(&self.core)
            .map(move |size| cosmic::Action::App(Message::ScreenSize(generation, size)))
    }

    fn open_popup(&mut self) -> Task<cosmic::Action<Message>> {
        let Some(parent) = self.core.main_window_id() else {
            return Task::none();
        };
        let id = Id::unique();
        self.popup = Some(id);
        self.refresh_now();
        let mut settings = self
            .core
            .applet
            .get_popup_settings(parent, id, None, None, None);
        let (width, height) = self.popup_size();
        settings.positioner.size_limits = Limits::NONE
            .min_width(width)
            .max_width(width)
            .max_height(height);
        Task::batch([get_popup(settings), self.fetch_weather()])
    }

    fn overview_viewed(&mut self, id: Id) {
        if self.popup == Some(id) && self.draft.is_none() && !self.agenda.editing() {
            self.history.mark_viewed();
        }
    }

    fn editable(&self) -> bool {
        self.draft.is_some()
            && self.config_ready
            && !self.config_loading
            && self.pending_save.is_none()
    }

    fn add_clock(&mut self, clock: WorldClock) {
        if !self.editable() {
            return;
        }
        let Some(draft) = &mut self.draft else {
            return;
        };
        if draft.clocks.iter().any(|existing| existing == &clock) {
            return;
        }
        draft.clocks.push(clock);
        match draft.validate() {
            Ok(()) => self.config_error = None,
            Err(error) => {
                draft.clocks.pop();
                self.config_error = Some(error);
            }
        }
    }

    fn load_config(&mut self) -> Task<cosmic::Action<Message>> {
        if self.config_loading || self.pending_save.is_some() {
            return Task::none();
        }
        self.config_loading = true;
        self.config_generation = self.config_generation.wrapping_add(1);
        let generation = self.config_generation;
        Task::perform(config::load(), move |result| {
            cosmic::Action::App(Message::ConfigLoaded(generation, result))
        })
    }

    fn begin_save(&mut self) -> Option<(u64, Preferences)> {
        if !self.editable() {
            return None;
        }
        let draft = self.draft.as_ref()?;
        if let Err(error) = draft.validate() {
            self.config_error = Some(error);
            return None;
        }
        let preferences = draft.clone();
        self.invalidate_city_search();
        self.pending_save = Some(preferences.clone());
        self.config_generation = self.config_generation.wrapping_add(1);
        self.config_error = None;
        Some((self.config_generation, preferences))
    }

    fn finish_save(&mut self, generation: u64, result: Result<(), String>) -> bool {
        if generation != self.config_generation {
            return false;
        }
        let Some(saved) = self.pending_save.take() else {
            return false;
        };
        match result {
            Ok(()) => {
                let location_changed = saved.weather_location != self.preferences.weather_location;
                self.preferences = saved;
                self.config_error = None;
                self.leave_settings();
                if location_changed {
                    self.forecast_generation = self.forecast_generation.wrapping_add(1);
                    self.forecast = None;
                    self.forecast_loading = false;
                    self.forecast_error = None;
                    self.weather_check = None;
                }
                true
            }
            Err(error) => {
                self.config_error = Some(format!(
                    "Settings were not confirmed saved. Previous settings remain active. {error}"
                ));
                false
            }
        }
    }

    fn leave_settings(&mut self) {
        self.draft = None;
        self.invalidate_city_search();
        self.city_query.clear();
        self.timezone_query.clear();
        self.timezone_matches.clear();
        self.clock_label.clear();
        self.place_query.clear();
        self.place_matches.clear();
    }

    fn invalidate_city_search(&mut self) {
        self.city_generation = self.city_generation.wrapping_add(1);
        self.city_loading = false;
        self.city_searched = false;
        self.cities.clear();
        self.city_error = None;
    }

    fn weather_due(&self) -> bool {
        self.weather_check
            .is_none_or(|check| check.elapsed() >= WEATHER_INTERVAL)
    }

    fn fetch_weather(&mut self) -> Task<cosmic::Action<Message>> {
        if self.popup.is_none() || self.forecast_loading || !self.weather_due() {
            return Task::none();
        }
        let Some(location) = self.preferences.weather_location.clone() else {
            return Task::none();
        };
        let Some(client) = self.weather_client.clone() else {
            return Task::none();
        };
        self.forecast_loading = true;
        self.forecast_generation = self.forecast_generation.wrapping_add(1);
        self.weather_check = Some(Instant::now());
        let generation = self.forecast_generation;
        Task::perform(
            async move {
                let result = client.forecast(&location).await;
                (location, result)
            },
            move |(location, result)| {
                cosmic::Action::App(Message::ForecastLoaded(generation, location, result))
            },
        )
    }

    fn finish_forecast(
        &mut self,
        generation: u64,
        location: WeatherLocation,
        result: Result<weather::Forecast, String>,
    ) {
        if generation != self.forecast_generation
            || self.preferences.weather_location.as_ref() != Some(&location)
        {
            return;
        }
        self.forecast_loading = false;
        match result {
            Ok(forecast) if forecast.location == location => {
                self.forecast_error = None;
                self.forecast = Some(forecast);
            }
            Ok(_) => {
                self.forecast_error =
                    Some("The forecast did not match the selected location.".into())
            }
            Err(error) => {
                self.forecast_error = Some(error);
                if let Some(forecast) = &mut self.forecast {
                    forecast.stale = true;
                }
            }
        }
    }

    fn search_cities(&mut self) -> Task<cosmic::Action<Message>> {
        if !self.editable() || self.city_loading {
            return Task::none();
        }
        let query = self.city_query.trim().to_owned();
        if query.chars().count() < 2 {
            self.city_error = Some("Enter at least two characters to search for a city.".into());
            return Task::none();
        }
        let Some(client) = self.weather_client.clone() else {
            self.city_error = self.weather_client_error.clone();
            return Task::none();
        };
        self.city_generation = self.city_generation.wrapping_add(1);
        let generation = self.city_generation;
        self.city_loading = true;
        self.city_searched = true;
        self.city_error = None;
        self.cities.clear();
        Task::perform(async move { client.search(&query).await }, move |result| {
            cosmic::Action::App(Message::CitiesLoaded(generation, result))
        })
    }

    fn finish_cities(&mut self, generation: u64, result: Result<Vec<WeatherLocation>, String>) {
        if generation != self.city_generation || self.draft.is_none() {
            return;
        }
        self.city_loading = false;
        match result {
            Ok(cities) => {
                self.cities = cities;
                self.city_error = None;
            }
            Err(error) => {
                self.cities.clear();
                self.city_error = Some(error);
            }
        }
    }

    fn refresh_artwork(&mut self) -> Task<cosmic::Action<Message>> {
        let paths: HashSet<_> = self
            .players
            .iter()
            .filter_map(|player| player.artwork.clone())
            .collect();
        self.media_artwork.retain(|path, _| paths.contains(path));
        self.artwork_requests.retain(|path, _| paths.contains(path));
        let mut tasks = Vec::new();
        for path in paths {
            if self.media_artwork.contains_key(&path) || self.artwork_requests.contains_key(&path) {
                continue;
            }
            self.artwork_revision = self.artwork_revision.wrapping_add(1);
            let revision = self.artwork_revision;
            self.artwork_requests.insert(path.clone(), revision);
            tasks.push(Task::perform(load_artwork(path.clone()), move |image| {
                cosmic::Action::App(Message::ArtworkLoaded(path.clone(), revision, image))
            }));
        }
        Task::batch(tasks)
    }

    fn notification_operation(
        &mut self,
        id: u32,
        action: Option<String>,
    ) -> Task<cosmic::Action<Message>> {
        let Some(entry) = self
            .history
            .entries
            .iter()
            .find(|entry| entry.notification.id == id)
        else {
            return Task::none();
        };
        if self.history.pending.contains_key(&id) {
            return Task::none();
        }
        if !entry.active {
            if action.is_none() {
                self.history
                    .entries
                    .retain(|entry| entry.notification.id != id);
            }
            return Task::none();
        }
        if let Some(key) = &action
            && self.history.action_revision(id, key).is_none()
        {
            return Task::none();
        }
        let Some(client) = self.notification_client.clone() else {
            return Task::none();
        };
        let revision = entry.revision;
        self.history.pending.insert(id, revision);
        self.notification_action_error = None;
        let dismiss = action.is_none();
        Task::perform(
            async move {
                match action {
                    Some(key) => client.invoke(id, key).await,
                    None => client.dismiss(id).await,
                }
            },
            move |result| {
                cosmic::Action::App(Message::NotificationFinished(id, revision, dismiss, result))
            },
        )
    }

    fn clock_view(&self) -> Element<'_, Message> {
        let local_abbreviation = local_clock_abbreviation(self.now.fixed_offset(), self.local_zone);
        let mut clocks = widget::column([])
            .spacing(6)
            .width(Length::Fill)
            .push(widget::text::title4("World Clocks").size(15));
        if self.preferences.clocks.is_empty() {
            clocks = clocks.push(widget::text("Add a timezone in Settings.").size(13));
        }
        for clock in &self.preferences.clocks {
            let Ok(zone) = clock.timezone.parse::<Tz>() else {
                continue;
            };
            let reading = world_clock_reading(self.now.fixed_offset(), zone, &self.preferences);
            let indicator = reading.indicator(&local_abbreviation);
            let day = match reading.day_difference {
                0 => String::new(),
                1 => "Tomorrow".into(),
                -1 => "Yesterday".into(),
                offset => format!("{offset:+} days"),
            };
            let mut time = widget::column([widget::text(reading.time).size(16).into()])
                .align_x(Alignment::End);
            if !day.is_empty() {
                time = time.push(widget::text(day).size(10));
            }
            clocks = clocks.push(
                widget::row([
                    cosmic::iced::widget::rich_text::<(), _, _, _>([
                        cosmic::iced::widget::span(&clock.label).size(12),
                        cosmic::iced::widget::span(" "),
                        cosmic::iced::widget::span(indicator).size(10),
                    ])
                    .font(cosmic::font::default())
                    .size(12)
                    .width(Length::Fill)
                    .into(),
                    time.into(),
                ])
                .spacing(6)
                .align_y(Alignment::Center),
            );
        }
        card(clocks)
    }

    fn calendar_view(&self) -> Element<'_, Message> {
        let mut content = widget::column([]).spacing(3).width(Length::Fill).push(
            widget::row([
                icon_button(
                    "pan-start-symbolic",
                    "Previous month",
                    Message::PreviousMonth,
                    true,
                ),
                widget::text(self.calendar.month.format("%B %Y").to_string())
                    .size(15)
                    .width(Length::Fill)
                    .align_x(Alignment::Center)
                    .into(),
                icon_button("pan-end-symbolic", "Next month", Message::NextMonth, true),
            ])
            .align_y(Alignment::Center)
            .spacing(4),
        );
        let monday = self.preferences.week_start_monday;
        let names = if monday {
            ["M", "T", "W", "T", "F", "S", "S"]
        } else {
            ["S", "M", "T", "W", "T", "F", "S"]
        };
        let mut header = widget::row([]).spacing(2);
        if self.preferences.show_week_numbers {
            header = header.push(widget::text("Wk").size(10).width(Length::Fixed(24.0)));
        }
        for name in names {
            header = header.push(
                widget::text(name)
                    .size(11)
                    .width(Length::FillPortion(1))
                    .align_x(Alignment::Center),
            );
        }
        content = content.push(header);
        let days = self.calendar.days(monday);
        for week in days.chunks(7) {
            let mut row = widget::row([]).spacing(2).align_y(Alignment::Center);
            if self.preferences.show_week_numbers {
                let iso_day = if monday { week.first() } else { week.get(1) };
                row = row.push(
                    widget::text(
                        iso_day
                            .map(|day| day.iso_week().week().to_string())
                            .unwrap_or_default(),
                    )
                    .size(10)
                    .width(Length::Fixed(24.0)),
                );
            }
            for &date in week {
                let name = if date.month() != self.calendar.month.month() {
                    format!("{} (adjacent month)", date.format("%A, %-d %B %Y"))
                } else {
                    date.format("%A, %-d %B %Y").to_string()
                };
                let theme = cosmic::theme::active();
                let mut color: cosmic::iced::Color = if date == self.calendar.selected {
                    theme.cosmic().accent_button.on.into()
                } else {
                    theme.cosmic().background(false).on.into()
                };
                if date.month() != self.calendar.month.month() && date != self.calendar.selected {
                    color.a *= 0.55;
                }
                let marked = self.agenda.has_tasks(date);
                let dot = widget::container(widget::Space::new())
                    .width(Length::Fixed(4.0))
                    .height(Length::Fixed(4.0))
                    .style(move |_| widget::container::Style {
                        background: marked.then_some(color.into()),
                        border: cosmic::iced::Border {
                            radius: 2.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    });
                let mut button = widget::button::custom(
                    widget::column([
                        widget::text(date.day().to_string())
                            .size(13)
                            .class(cosmic::theme::Text::Color(color))
                            .into(),
                        dot.into(),
                    ])
                    .spacing(1)
                    .align_x(Alignment::Center)
                    .width(Length::Fill),
                )
                .name(if marked {
                    format!("{name}; tasks scheduled")
                } else {
                    name.clone()
                })
                .on_press(Message::SelectDate(date))
                .width(Length::FillPortion(1))
                .height(Length::Fixed(32.0))
                .padding(2);
                button = if date == self.calendar.selected {
                    button.class(cosmic::theme::Button::Suggested)
                } else if date == self.now.date_naive() {
                    button.class(cosmic::theme::Button::Standard)
                } else {
                    button.class(cosmic::theme::Button::Text)
                };
                row = row.push(widget::tooltip::tooltip(
                    button,
                    widget::text(name).size(12),
                    widget::tooltip::Position::Top,
                ));
            }
            content = content.push(row);
        }
        content = content.push(
            widget::row([
                widget::Space::new().width(Length::Fill).into(),
                self.agenda.add_task_button(self.calendar.selected),
                action_button("Today", Message::Today, true),
            ])
            .align_y(Alignment::Center)
            .spacing(4),
        );
        content
            .push(
                self.agenda
                    .day_view(self.calendar.selected, self.preferences.twenty_four_hour),
            )
            .into()
    }

    fn weather_view(&self) -> Element<'_, Message> {
        let mut content = widget::column([]).spacing(8).width(Length::Fill).push(
            widget::row([
                widget::text::title4("Weather")
                    .size(15)
                    .width(Length::Fill)
                    .into(),
                cosmic::iced::widget::opaque(icon_button(
                    "view-refresh-symbolic",
                    "Refresh forecast (at most every 15 minutes)",
                    Message::RefreshWeather,
                    self.preferences.weather_location.is_some()
                        && self.weather_client.is_some()
                        && !self.forecast_loading
                        && self.weather_due(),
                )),
            ])
            .align_y(Alignment::Center),
        );
        if let Some(error) = &self.weather_launch_error {
            content = content.push(error_text(error));
        }
        let Some(location) = &self.preferences.weather_location else {
            return self.weather_card(content.push(widget::text("Click to choose a location in Weather, or select a city in applet Settings. No location is detected automatically.").size(13)));
        };
        content = content.push(widget::text(&location.name).size(13));
        if let Some(error) = &self.weather_client_error {
            content = content.push(error_text(error));
        }
        if self.forecast_loading {
            content = content.push(widget::text("Updating forecast…").size(12));
        }
        if let Some(error) = &self.forecast_error {
            content = content.push(error_text(error));
        }
        if let Some(forecast) = &self.forecast {
            let zone = location.timezone.parse::<Tz>().unwrap_or(chrono_tz::UTC);
            let mut hours = widget::row([]).spacing(3).width(Length::Fill);
            let now = self.now.with_timezone(&Utc);
            let mut count = 0;
            for hour in forecast
                .hours
                .iter()
                .filter(|hour| hour.time + chrono::Duration::hours(1) > now)
                .take(5)
            {
                let temperature = match self.preferences.temperature_unit {
                    TemperatureUnit::Celsius => hour.temperature_c,
                    TemperatureUnit::Fahrenheit => hour.temperature_c * 9.0 / 5.0 + 32.0,
                };
                let unit = match self.preferences.temperature_unit {
                    TemperatureUnit::Celsius => "°C",
                    TemperatureUnit::Fahrenheit => "°F",
                };
                let mut slot = widget::column([
                    widget::text(
                        hour.time
                            .with_timezone(&zone)
                            .format(if self.preferences.twenty_four_hour {
                                "%H:%M"
                            } else {
                                "%-I%p"
                            })
                            .to_string(),
                    )
                    .size(11)
                    .into(),
                    widget::tooltip::tooltip(
                        widget::icon::from_name(weather::icon_name(&hour.symbol))
                            .size(27)
                            .icon(),
                        widget::text(if hour.symbol.is_empty() {
                            "Conditions unavailable".to_owned()
                        } else {
                            hour.symbol.replace('_', " ")
                        })
                        .size(12),
                        widget::tooltip::Position::Top,
                    )
                    .into(),
                    widget::text(format!("{temperature:.0}{unit}"))
                        .size(13)
                        .into(),
                ])
                .spacing(5)
                .align_x(Alignment::Center)
                .width(Length::FillPortion(1));
                if let Some(rain) = hour.precipitation_mm {
                    slot = slot.push(widget::text(format!("{rain:.1} mm")).size(10));
                }
                hours = hours.push(slot);
                count += 1;
            }
            if count == 0 {
                content =
                    content.push(widget::text("No upcoming hourly data is available.").size(12));
            } else {
                content = content.push(hours);
            }
            let stale = forecast.stale || forecast.valid_until < now;
            content = content.push(
                widget::text(format!(
                    "{} · updated {}",
                    if stale {
                        "Stale cached forecast"
                    } else {
                        "Forecast"
                    },
                    forecast.fetched_at.with_timezone(&zone).format("%a %H:%M")
                ))
                .size(11),
            );
            if let Some(warning) = &forecast.warning {
                content = content.push(error_text(warning));
            }
        } else if !self.forecast_loading
            && self.forecast_error.is_none()
            && self.weather_client_error.is_none()
        {
            content = content.push(widget::text("Forecast has not been loaded yet.").size(12));
        }
        self.weather_card(content)
    }

    fn weather_card<'a>(
        &'a self,
        content: impl Into<Element<'a, Message>>,
    ) -> Element<'a, Message> {
        widget::button::custom(card(content))
            .name("Open Weather")
            .padding(0)
            .width(Length::Fill)
            .class(cosmic::theme::Button::Standard)
            .on_press_maybe(
                (self.config_ready && !self.config_loading && !self.weather_launch_pending)
                    .then_some(Message::OpenWeather),
            )
            .into()
    }

    fn media_view<'a>(&'a self, player: &'a media::Player) -> Element<'a, Message> {
        let artwork: Element<'a, Message> = if let Some(handle) = player
            .artwork
            .as_ref()
            .and_then(|path| self.media_artwork.get(path))
            .and_then(Option::as_ref)
        {
            widget::image(handle.clone())
                .width(Length::Fixed(52.0))
                .height(Length::Fixed(52.0))
                .into()
        } else {
            widget::icon::from_name("audio-x-generic-symbolic")
                .size(44)
                .icon()
                .into()
        };
        let ready = !self.media_pending.contains(&player.bus_name);
        let controls = widget::row([
            icon_button(
                "media-skip-backward-symbolic",
                "Previous track",
                Message::MediaControl(player.bus_name.clone(), media::Action::Previous),
                ready && player.can_previous,
            ),
            icon_button(
                if player.playing {
                    "media-playback-pause-symbolic"
                } else {
                    "media-playback-start-symbolic"
                },
                if player.playing { "Pause" } else { "Play" },
                Message::MediaControl(player.bus_name.clone(), media::Action::PlayPause),
                ready
                    && if player.playing {
                        player.can_pause
                    } else {
                        player.can_play
                    },
            ),
            icon_button(
                "media-skip-forward-symbolic",
                "Next track",
                Message::MediaControl(player.bus_name.clone(), media::Action::Next),
                ready && player.can_next,
            ),
        ])
        .spacing(3)
        .align_y(Alignment::Center);
        let mut content = widget::column([]).spacing(6).push(
            widget::row([
                artwork,
                widget::column([
                    widget::text(if player.title.is_empty() {
                        &player.identity
                    } else {
                        &player.title
                    })
                    .size(14)
                    .into(),
                    widget::text(&player.artist).size(12).into(),
                ])
                .spacing(3)
                .width(Length::Fill)
                .into(),
            ])
            .spacing(10)
            .align_y(Alignment::Center),
        );
        content = content.push(
            widget::row([
                widget::button::text(&player.identity)
                    .on_press_maybe((ready && player.can_raise).then(|| {
                        Message::MediaControl(player.bus_name.clone(), media::Action::Raise)
                    }))
                    .width(Length::Fill)
                    .into(),
                controls.into(),
            ])
            .align_y(Alignment::Center),
        );
        card(content)
    }

    fn notification_view<'a>(&'a self, entry: &'a HistoryEntry) -> Element<'a, Message> {
        let notification = &entry.notification;
        let pending = self.history.pending.contains_key(&notification.id);
        let enabled = entry.active && self.notification_client.is_some() && !pending;
        let image: Element<'a, Message> = if let Some(handle) = &notification.image {
            widget::image(handle.clone())
                .width(Length::Fixed(36.0))
                .height(Length::Fixed(36.0))
                .into()
        } else {
            widget::icon::from_name(if notification.app_icon.is_empty() {
                "dialog-information-symbolic"
            } else {
                &notification.app_icon
            })
            .size(32)
            .icon()
            .into()
        };
        let age = notification_age(self.now.with_timezone(&Utc), notification.received_at);
        let mut content = widget::column([])
            .width(Length::Fill)
            .spacing(6)
            .push(
                widget::row([
                    image,
                    widget::column([
                        widget::text(&notification.app_name).size(12).into(),
                        widget::text(if entry.active {
                            age
                        } else {
                            format!("{age} · history")
                        })
                        .size(10)
                        .into(),
                    ])
                    .width(Length::Fill)
                    .spacing(2)
                    .into(),
                    icon_button(
                        "window-close-symbolic",
                        "Dismiss notification",
                        Message::DismissNotification(notification.id),
                        !pending && (!entry.active || self.notification_client.is_some()),
                    ),
                ])
                .spacing(8)
                .align_y(Alignment::Center),
            )
            .push(widget::text(&notification.summary).size(14));
        if !notification.body.is_empty() {
            content = content.push(widget::text(&notification.body).size(13));
        }
        if entry.active {
            for action in notification
                .actions
                .iter()
                .filter(|action| action.key != "default")
            {
                content = content.push(
                    widget::button::standard(&action.label)
                        .width(Length::Fill)
                        .on_press_maybe(enabled.then(|| {
                            Message::NotificationAction(notification.id, action.key.clone())
                        })),
                );
            }
        }
        if pending {
            content = content.push(widget::text("Waiting for the notification service…").size(11));
        }
        if enabled
            && self
                .history
                .action_revision(notification.id, "default")
                .is_some()
        {
            // COSMIC buttons dispatch to children first; nested controls capture
            // their mouse, touch and keyboard activation before this card.
            widget::button::custom(card(content))
                .name(format!(
                    "{}: {}",
                    notification.app_name, notification.summary
                ))
                .padding(0)
                .width(Length::Fill)
                .class(cosmic::theme::Button::Standard)
                .on_press(Message::NotificationAction(
                    notification.id,
                    "default".into(),
                ))
                .into()
        } else {
            card(content)
        }
    }

    fn notifications_view(&self) -> Element<'_, Message> {
        let mut content = widget::column([]).width(Length::Fill).spacing(10);
        if let Some(error) = &self.notification_error {
            content = content.push(error_text(error));
        }
        if let Some(error) = &self.notification_action_error {
            content = content.push(error_text(error));
        }
        if self.history.entries.is_empty() {
            content = content.push(widget::container(widget::column([
                widget::icon::from_name("notifications-symbolic").size(40).icon().into(),
                widget::text("No notifications collected").size(15).into(),
                widget::text("New notifications appear here while this applet is running. Existing applet history is not transferred.").size(12).into(),
            ]).spacing(12).align_x(Alignment::Center)).padding(24).width(Length::Fill));
        }
        for entry in &self.history.entries {
            content = content.push(self.notification_view(entry));
        }
        let dnd = widget::toggler(self.dnd_pending.unwrap_or(self.dnd))
            .label(Some("Do Not Disturb".into()))
            .text_size(13)
            .on_toggle_maybe(
                (self.dnd_known && self.dnd_pending.is_none()).then_some(Message::DoNotDisturb),
            );
        content = content.push(
            widget::row([
                widget::button::standard("Settings")
                    .leading_icon(
                        widget::icon::from_name("emblem-system-symbolic")
                            .size(16)
                            .handle(),
                    )
                    .on_press(Message::OpenSettings)
                    .into(),
                action_button(
                    "Clear",
                    Message::ClearNotifications,
                    !self.history.entries.is_empty() && self.history.pending.is_empty(),
                ),
                dnd.width(Length::Fill).into(),
            ])
            .spacing(8)
            .align_y(Alignment::Center),
        );
        if let Some(error) = &self.dnd_error {
            content = content.push(error_text(error));
        }
        content.into()
    }

    fn section_view(&self, section: Section) -> Option<Element<'_, Message>> {
        Some(match section {
            Section::Media => {
                let mut content = widget::column([]).spacing(10).width(Length::Fill);
                let mut visible = false;
                for player in self.players.iter().filter(|player| player.playing) {
                    content = content.push(self.media_view(player));
                    visible = true;
                }
                if let Some(error) = &self.media_error {
                    content = content.push(error_text(error));
                    visible = true;
                }
                if !visible {
                    return None;
                }
                content.into()
            }
            Section::Notifications => self.notifications_view(),
            Section::Calendar => self.calendar_view(),
            Section::Weather => self.weather_view(),
            Section::WorldClocks => self.clock_view(),
        })
    }

    fn overview(&self) -> Element<'_, Message> {
        let mut left = widget::column([]).width(Length::Fill).spacing(13);
        let mut right = widget::column([]).width(Length::Fill).spacing(13);
        for placement in &self.preferences.layout {
            if let Some(section) = self.section_view(placement.section) {
                match placement.side {
                    Side::Left => left = left.push(section),
                    Side::Right => right = right.push(section),
                }
            }
        }
        if let Some(error) = &self.config_error {
            right = right.push(error_text(error));
        }
        if self.config_loading {
            right = right.push(widget::text("Loading saved preferences…").size(12));
        }
        let left_column = widget::scrollable(widget::container(left).padding([0, 8]))
            .height(Length::Shrink)
            .width(Length::FillPortion(7));
        let right_column = widget::scrollable(widget::container(right).padding([0, 6]))
            .width(Length::FillPortion(5))
            .height(Length::Shrink);
        widget::row([
            left_column.into(),
            widget::divider::vertical::default().into(),
            right_column.into(),
        ])
        .spacing(14)
        .height(Length::Shrink)
        .into()
    }

    fn settings_view(&self) -> Element<'_, Message> {
        let draft = self.draft.as_ref().unwrap_or(&self.preferences);
        let editable = self.editable();
        let toggle = |label: &str, value: bool, message: fn(bool) -> Message| {
            widget::toggler(value)
                .label(Some(label.into()))
                .text_size(14)
                .width(Length::Fill)
                .on_toggle_maybe(editable.then_some(message))
        };
        let clock_settings = card(
            widget::column([
                widget::text::title4("Clock & Calendar").into(),
                toggle(
                    "24-hour clock",
                    draft.twenty_four_hour,
                    Message::TwentyFourHour,
                )
                .into(),
                toggle("Show seconds", draft.show_seconds, Message::ShowSeconds).into(),
                toggle(
                    "Start week on Monday",
                    draft.week_start_monday,
                    Message::MondayFirst,
                )
                .into(),
                toggle(
                    "Show ISO week numbers",
                    draft.show_week_numbers,
                    Message::WeekNumbers,
                )
                .into(),
            ])
            .spacing(14),
        );
        let mut layout = widget::column([])
            .spacing(8)
            .width(Length::Fill)
            .push(widget::text::title4("Section layout"));
        for placement in &draft.layout {
            let section = placement.section;
            layout = layout.push(
                widget::column([
                    widget::row([
                        widget::text(section.label())
                            .size(13)
                            .width(Length::Fill)
                            .into(),
                        choice_button(
                            "Left",
                            Message::SectionSide(section, Side::Left),
                            placement.side == Side::Left,
                            editable,
                        ),
                        choice_button(
                            "Right",
                            Message::SectionSide(section, Side::Right),
                            placement.side == Side::Right,
                            editable,
                        ),
                    ])
                    .spacing(5)
                    .align_y(Alignment::Center)
                    .into(),
                    widget::row([
                        widget::text(placement.side.label())
                            .size(11)
                            .width(Length::Fill)
                            .into(),
                        action_button(
                            "Up",
                            Message::MoveSection(section, false),
                            editable && section_neighbor(&draft.layout, section, false).is_some(),
                        ),
                        action_button(
                            "Down",
                            Message::MoveSection(section, true),
                            editable && section_neighbor(&draft.layout, section, true).is_some(),
                        ),
                    ])
                    .spacing(5)
                    .align_y(Alignment::Center)
                    .into(),
                ])
                .spacing(4),
            );
        }
        let mut place_input = widget::search_input(
            "City, state or country, e.g. Dallas or Texas",
            &self.place_query,
        )
        .id(self.place_id.clone())
        .label("Geographic world clock search");
        if editable {
            place_input = place_input.on_input(Message::PlaceQuery);
        }
        let mut geographic_results = widget::column([]).spacing(6).width(Length::Fill);
        for &place in &self.place_matches {
            let label = place.label();
            let added = draft
                .clocks
                .iter()
                .any(|clock| clock.label == label && clock.timezone == place.timezone);
            geographic_results = geographic_results.push(
                widget::row([
                    widget::column([
                        widget::text(label).size(13).into(),
                        widget::text(place.timezone).size(11).into(),
                    ])
                    .spacing(2)
                    .width(Length::Fill)
                    .into(),
                    action_button(
                        if added { "Added" } else { "Add" },
                        Message::AddPlace(place),
                        editable && !added,
                    ),
                ])
                .spacing(6)
                .align_y(Alignment::Center),
            );
        }
        if self.place_query.trim().is_empty() {
            geographic_results = geographic_results.push(widget::text("Search the bundled geographic city index. Multiple cities can share a timezone.").size(12));
        } else if self.place_matches.is_empty() {
            geographic_results = geographic_results.push(
                widget::text(
                    "No matching indexed cities. Try a nearby city or the IANA search below.",
                )
                .size(12),
            );
        } else if self.place_matches.len() == 50 {
            geographic_results = geographic_results.push(
                widget::text("First 50 cities. Refine the search for more specific results.")
                    .size(11),
            );
        }
        // Keep these input nodes in fixed positions. Async/loading messages are
        // below the inputs, never inserted ahead of them during clock ticks.
        let mut timezone_input = widget::search_input(
            "Search IANA timezone, e.g. Europe/Paris",
            &self.timezone_query,
        )
        .id(self.timezone_id.clone())
        .label("Timezone");
        let mut label_input = widget::text_input("Optional clock label", &self.clock_label)
            .id(self.clock_label_id.clone())
            .label("Clock name");
        if editable {
            timezone_input = timezone_input.on_input(Message::TimezoneQuery);
            label_input = label_input.on_input(Message::ClockLabel);
        }
        let mut results = widget::column([]).spacing(4).width(Length::Fill);
        for &zone in &self.timezone_matches {
            let label = if self.clock_label.trim().is_empty() {
                zone.name().replace('_', " ")
            } else {
                self.clock_label.trim().to_owned()
            };
            let already_added = draft
                .clocks
                .iter()
                .any(|clock| clock.timezone == zone.name() && clock.label == label);
            results = results.push(
                widget::row([
                    widget::text(zone.name())
                        .size(12)
                        .width(Length::Fill)
                        .into(),
                    action_button(
                        if already_added { "Added" } else { "Add" },
                        Message::AddClock(zone),
                        editable && !already_added,
                    ),
                ])
                .spacing(8)
                .align_y(Alignment::Center),
            );
        }
        if self.timezone_query.trim().is_empty() {
            results = results.push(
                widget::text("Search the IANA timezone database to add a world clock.").size(12),
            );
        } else if self.timezone_matches.is_empty() {
            results = results.push(widget::text("No matching IANA timezones.").size(12));
        } else if self.timezone_matches.len() == 50 {
            results = results.push(
                widget::text("First 50 matches. Refine the search for another timezone.").size(11),
            );
        }
        let mut world = widget::column([])
            .width(Length::Fill)
            .spacing(10)
            .push(widget::text::title4("World Clocks"))
            .push(place_input)
            .push(widget::scrollable(geographic_results).height(Length::Fixed(170.0)))
            .push(widget::text("Raw IANA timezone search").size(13))
            .push(timezone_input)
            .push(label_input)
            .push(widget::scrollable(results).height(Length::Fixed(130.0)));
        let mut configured = widget::column([]).spacing(6).width(Length::Fill);
        for (index, clock) in draft.clocks.iter().enumerate() {
            configured = configured.push(
                widget::row([
                    widget::column([
                        widget::text(&clock.label).size(13).into(),
                        widget::text(&clock.timezone).size(11).into(),
                    ])
                    .spacing(2)
                    .width(Length::Fill)
                    .into(),
                    action_button(
                        "Up",
                        Message::MoveClock(index, false),
                        editable && index > 0,
                    ),
                    action_button(
                        "Down",
                        Message::MoveClock(index, true),
                        editable && index + 1 < draft.clocks.len(),
                    ),
                    action_button("Remove", Message::RemoveClock(index), editable),
                ])
                .spacing(8)
                .align_y(Alignment::Center),
            );
        }
        if draft.clocks.is_empty() {
            configured = configured.push(widget::text("No world clocks configured.").size(12));
        }
        world = world.push(configured);
        let mut city_input = widget::search_input("Search city name", &self.city_query)
            .id(self.city_id.clone())
            .label("Weather city");
        if editable {
            city_input = city_input
                .on_input(Message::CityQuery)
                .on_submit(|_| Message::SearchCities);
        }
        let mut weather_settings = widget::column([]).width(Length::Fill).spacing(10)
            .push(widget::text::title4("Weather"))
            .push(widget::row([
                widget::text("Temperature").size(14).width(Length::Fill).into(),
                choice_button("°C", Message::Unit(TemperatureUnit::Celsius), draft.temperature_unit == TemperatureUnit::Celsius, editable),
                choice_button("°F", Message::Unit(TemperatureUnit::Fahrenheit), draft.temperature_unit == TemperatureUnit::Fahrenheit, editable),
            ]).spacing(6).align_y(Alignment::Center))
            .push(city_input)
            .push(action_button("Search cities", Message::SearchCities, editable && !self.city_loading && self.weather_client.is_some()))
            .push(widget::text("Searching sends only your typed city name to Open-Meteo geocoding (GeoNames). Choosing a city enables coordinate requests to MET Norway after Save.").size(12));
        let mut cities = widget::column([]).spacing(6).width(Length::Fill);
        if self.city_loading {
            cities = cities.push(widget::text("Searching cities…").size(12));
        }
        if let Some(error) = &self.city_error {
            cities = cities.push(error_text(error));
        }
        if let Some(error) = &self.weather_client_error {
            cities = cities.push(error_text(error));
        }
        if self.city_searched
            && !self.city_loading
            && self.city_error.is_none()
            && self.cities.is_empty()
        {
            cities = cities.push(widget::text("No cities found. Try another name.").size(12));
        }
        for (index, location) in self.cities.iter().enumerate() {
            cities = cities.push(
                widget::row([
                    widget::column([
                        widget::text(&location.name).size(13).into(),
                        widget::text(format!(
                            "{} · {:.4}, {:.4}",
                            location.timezone, location.latitude, location.longitude
                        ))
                        .size(11)
                        .into(),
                    ])
                    .spacing(2)
                    .width(Length::Fill)
                    .into(),
                    action_button("Select", Message::SelectCity(index), editable),
                ])
                .spacing(6)
                .align_y(Alignment::Center),
            );
        }
        weather_settings =
            weather_settings.push(widget::scrollable(cities).height(Length::Fixed(160.0)));
        let location: Element<'_, Message> = match &draft.weather_location {
            Some(location) => widget::row([
                widget::column([
                    widget::text(format!("Selected: {}", location.name))
                        .size(13)
                        .into(),
                    widget::text(&location.timezone).size(11).into(),
                ])
                .width(Length::Fill)
                .spacing(3)
                .into(),
                action_button("Remove city", Message::RemoveCity, editable),
            ])
            .spacing(8)
            .align_y(Alignment::Center)
            .into(),
            None => widget::text("Weather is disabled until you choose a city and Save.")
                .size(12)
                .into(),
        };
        weather_settings = weather_settings.push(location)
            .push(widget::text("Forecast data comes from MET Norway, the meteorological provider behind Yr. This applet is not branded or operated by Yr.").size(12))
            .push(widget::button::link("MET Norway · CC BY 4.0")
                .on_press(Message::OpenMetCredit).name("Open MET Norway weather data attribution"))
            .push(widget::text("Forecasts are adapted into time slots, condition icons and rounded Celsius/Fahrenheit temperatures.").size(12))
            .push(widget::divider::horizontal::default())
            .push(widget::text("Offline world-clock locations: GeoNames cities5000. City aliases are normalized for search and long labels shortened. Smaller settlements may be absent.").size(12))
            .push(widget::button::link("GeoNames · CC BY 4.0")
                .on_press(Message::OpenGeoCredit).name("Open GeoNames location data attribution"));
        if let Some(error) = &self.credit_error {
            weather_settings = weather_settings.push(error_text(error));
        }
        let body = widget::row([
            widget::scrollable(
                widget::column([clock_settings, card(layout), card(world)]).spacing(12),
            )
            .width(Length::FillPortion(1))
            .height(Length::Fill)
            .into(),
            widget::scrollable(card(weather_settings))
                .width(Length::FillPortion(1))
                .height(Length::Fill)
                .into(),
        ])
        .spacing(16)
        .height(Length::Fill);
        let mut status = widget::column([]).spacing(4).width(Length::Fill);
        if self.config_loading {
            status = status.push(widget::text("Loading saved preferences…").size(12));
        }
        if self.pending_save.is_some() {
            status = status.push(
                widget::text("Saving… Previous preferences remain active until saving succeeds.")
                    .size(12),
            );
        }
        if let Some(error) = &self.config_error {
            status = status.push(error_text(error));
        }
        if !self.config_ready && !self.config_loading {
            status = status.push(widget::text("Saving is blocked because existing settings could not be read. The file will not be overwritten.").size(12))
                .push(action_button("Retry loading settings", Message::RetryConfig, true));
        }
        widget::column([
            widget::row([
                action_button("Back", Message::Back, self.pending_save.is_none()),
                widget::text::title3("Datetime Plus Settings")
                    .width(Length::Fill)
                    .into(),
            ])
            .spacing(12)
            .align_y(Alignment::Center)
            .into(),
            body.into(),
            widget::divider::horizontal::default().into(),
            status.into(),
            widget::row([
                widget::Space::new().width(Length::Fill).into(),
                action_button(
                    "Cancel",
                    Message::CancelSettings,
                    self.pending_save.is_none(),
                ),
                widget::button::suggested("Save")
                    .on_press_maybe(editable.then_some(Message::SaveSettings))
                    .into(),
            ])
            .spacing(8)
            .align_y(Alignment::Center)
            .into(),
        ])
        .spacing(12)
        .height(Length::Fill)
        .into()
    }
}

impl cosmic::Application for AppModel {
    type Executor = cosmic::executor::Default;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = config::APP_ID;

    fn core(&self) -> &cosmic::Core {
        &self.core
    }
    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    fn init(core: cosmic::Core, _flags: ()) -> (Self, Task<cosmic::Action<Message>>) {
        let mut app = Self::new(core);
        let task = Task::batch([app.load_config(), app.query_screen_size()]);
        (app, task)
    }

    fn view(&self) -> Element<'_, Message> {
        let label = format!(
            "{}  {}",
            self.now.format("%a %-d %b"),
            self.now.format(clock_format(&self.preferences))
        );
        let suggested = self.core.applet.suggested_size(true);
        let (major, minor) = self.core.applet.suggested_padding(true);
        let (horizontal, vertical) = if self.core.applet.is_horizontal() {
            (major, minor)
        } else {
            (minor, major)
        };
        let mut bubble = cosmic::iced::widget::Stack::new().push(
            widget::icon::from_name("cosmic-applet-notification-symbolic")
                .size(16)
                .icon(),
        );
        if self.history.unread() {
            let dot = widget::container(widget::Space::new())
                .width(Length::Fixed(5.0))
                .height(Length::Fixed(5.0))
                .style(|theme: &cosmic::Theme| widget::container::Style {
                    background: Some(cosmic::iced::Background::Color(
                        theme.cosmic().background(false).on.into(),
                    )),
                    border: cosmic::iced::Border {
                        radius: 2.5.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                });
            bubble = bubble.push(
                widget::container(dot)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_x(Alignment::End),
            );
        }
        let button = widget::button::custom(
            widget::layer_container(
                widget::row([self.core.applet.text(label).into(), bubble.into()])
                    .spacing(6)
                    .align_y(Alignment::Center)
                    .width(Length::Shrink),
            )
            .center_y(Length::Fixed(f32::from(suggested.1 + 2 * vertical))),
        )
        .name(if self.history.unread() {
            "Open Datetime Plus calendar and notifications; unread notifications"
        } else {
            "Open Datetime Plus calendar and notifications"
        })
        .on_press_down(Message::TogglePopup)
        .padding([0, horizontal])
        .width(Length::Shrink)
        .class(cosmic::theme::Button::AppletIcon);
        self.core.applet.autosize_window(button).into()
    }

    fn view_window(&self, _id: Id) -> Element<'_, Message> {
        let (width, height) = self.popup_size();
        // Fill-height forms need a definite viewport; only the overview sizes to content.
        let (content, content_height) = if self.agenda.editing() {
            (self.agenda.editor_view(), Length::Fixed(height))
        } else if self.draft.is_some() {
            (self.settings_view(), Length::Fixed(height))
        } else {
            (self.overview(), Length::Shrink)
        };
        self.core
            .applet
            .popup_container(
                widget::container(content)
                    .padding(16)
                    .width(Length::Fill)
                    .height(content_height)
                    .style(|theme| {
                        let background = theme.cosmic().background(false);
                        let mut color: cosmic::iced::Color = background.base.into();
                        color.a = 1.0;
                        widget::container::Style {
                            background: Some(color.into()),
                            text_color: Some(background.on.into()),
                            icon_color: Some(background.on.into()),
                            border: cosmic::iced::Border {
                                radius: theme.cosmic().corner_radii.radius_m.into(),
                                width: 1.0,
                                color: background.divider.into(),
                            },
                            ..Default::default()
                        }
                    }),
            )
            .limits(
                Limits::NONE
                    .min_width(width)
                    .max_width(width)
                    .max_height(height),
            )
            .into()
    }

    fn on_close_requested(&self, id: Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    fn on_escape(&mut self) -> Task<cosmic::Action<Message>> {
        if self.agenda.editing() {
            return self.agenda.update(agenda_ui::Message::Cancel);
        }
        if self.draft.is_some() {
            if self.pending_save.is_none() {
                Task::done(cosmic::Action::App(Message::CancelSettings))
            } else {
                Task::none()
            }
        } else if self.popup.is_some() {
            Task::done(cosmic::Action::App(Message::TogglePopup))
        } else {
            Task::none()
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![
            Subscription::run_with(self.preferences.show_seconds, |seconds| {
                futures_util::stream::unfold(*seconds, |seconds| async move {
                    tokio::time::sleep(clock_tick_delay(Utc::now(), seconds)).await;
                    Some((Message::Tick, seconds))
                })
            }),
            notifications::subscription(&self.core).map(Message::NotificationEvent),
            media::subscription().map(Message::MediaEvent),
            agenda::subscription().map(|event| Message::Agenda(agenda_ui::Message::Event(event))),
            crate::display::changes().map(|_| Message::ScreenChanged),
        ];
        if let Some(popup) = self.popup
            && self.history.unread()
            && self.draft.is_none()
            && !self.agenda.editing()
        {
            // Listen only until the overview is drawn: ordinary event listeners
            // exclude redraws, and an unconditional raw listener would redraw forever.
            subscriptions.push(
                cosmic::iced::event::listen_raw(|event, _, id| {
                    matches!(
                        event,
                        cosmic::iced::Event::Window(cosmic::iced::window::Event::RedrawRequested(
                            _
                        ))
                    )
                    .then_some(id)
                })
                .with(popup)
                .filter_map(|(popup, id)| (popup == id).then_some(Message::OverviewViewed(id))),
            );
        }
        if self.popup.is_some() && self.preferences.weather_location.is_some() {
            subscriptions
                .push(cosmic::iced::time::every(WEATHER_INTERVAL).map(|_| Message::WeatherTick));
        }
        Subscription::batch(subscriptions)
    }

    fn update(&mut self, message: Message) -> Task<cosmic::Action<Message>> {
        match message {
            Message::ScreenChanged => return self.query_screen_size(),
            Message::ScreenSize(generation, size) => {
                if generation != self.screen_generation {
                    return Task::none();
                }
                let size = size.filter(|(width, height)| {
                    width.is_finite() && height.is_finite() && *width > 0.0 && *height > 0.0
                });
                if self.screen_size != size {
                    self.screen_size = size;
                }
                if std::mem::take(&mut self.popup_pending) {
                    return self.open_popup();
                }
            }
            Message::OverviewViewed(id) => self.overview_viewed(id),
            Message::Agenda(message) => return self.agenda.update(message),
            Message::TogglePopup => {
                if let Some(id) = self.popup.take() {
                    if self.pending_save.is_none() {
                        self.leave_settings();
                    }
                    return destroy_popup(id);
                }
                if self.popup_pending {
                    self.popup_pending = false;
                    return Task::none();
                }
                self.popup_pending = true;
                return self.query_screen_size();
            }
            Message::PopupClosed(id) => {
                if self.popup == Some(id) {
                    self.popup = None;
                    if self.pending_save.is_none() {
                        self.leave_settings();
                    }
                }
            }
            Message::Tick => {
                let previous = self.now.date_naive();
                self.refresh_now();
                if previous != self.now.date_naive()
                    && self.calendar.selected == previous
                    && self.calendar.month.month() == previous.month()
                    && self.calendar.month.year() == previous.year()
                {
                    self.calendar.select(self.now.date_naive());
                }
            }
            Message::WeatherTick | Message::RefreshWeather => return self.fetch_weather(),
            Message::OpenWeather => {
                if !self.config_ready || self.config_loading || self.weather_launch_pending {
                    return Task::none();
                }
                self.weather_launch_pending = true;
                self.weather_launch_error = None;
                let popup = self.popup;
                return Task::perform(
                    crate::launcher::open_weather(
                        self.preferences.weather_location.clone(),
                        self.preferences.temperature_unit,
                    ),
                    move |result| cosmic::Action::App(Message::WeatherOpened(popup, result)),
                );
            }
            Message::WeatherOpened(popup, result) => {
                self.weather_launch_pending = false;
                match result {
                    Ok(()) => {
                        if self.popup == popup
                            && self.draft.is_none()
                            && !self.agenda.editing()
                            && let Some(id) = self.popup.take()
                        {
                            return destroy_popup(id);
                        }
                    }
                    Err(error) => self.weather_launch_error = Some(error),
                }
            }
            Message::PreviousMonth => self.calendar.navigate(false),
            Message::NextMonth => self.calendar.navigate(true),
            Message::SelectDate(date) => {
                self.calendar.select(date);
                self.agenda.select_date();
            }
            Message::Today => {
                self.refresh_now();
                self.calendar.select(self.now.date_naive());
                self.agenda.select_date();
            }
            Message::OpenSettings => {
                if self.draft.is_none() {
                    self.draft = Some(self.preferences.clone());
                }
            }
            Message::Back | Message::CancelSettings => {
                if self.pending_save.is_none() {
                    self.leave_settings();
                }
            }
            Message::RetryConfig => return self.load_config(),
            Message::ConfigLoaded(generation, result) => {
                if generation != self.config_generation || !self.config_loading {
                    return Task::none();
                }
                self.config_loading = false;
                match result {
                    Ok(preferences) => {
                        self.preferences = preferences;
                        self.config_ready = true;
                        self.config_error = None;
                        if self.draft.is_some() {
                            self.draft = Some(self.preferences.clone());
                        }
                        return self.fetch_weather();
                    }
                    Err(error) => {
                        self.config_ready = false;
                        self.config_error = Some(error);
                    }
                }
            }
            Message::SaveSettings => {
                if let Some((generation, preferences)) = self.begin_save() {
                    return Task::perform(
                        async move { config::save(&preferences).await },
                        move |result| cosmic::Action::App(Message::ConfigSaved(generation, result)),
                    );
                }
            }
            Message::ConfigSaved(generation, result) => {
                if self.finish_save(generation, result) {
                    return self.fetch_weather();
                }
            }
            Message::TwentyFourHour(value) => {
                if self.editable()
                    && let Some(draft) = &mut self.draft
                {
                    draft.twenty_four_hour = value;
                }
            }
            Message::ShowSeconds(value) => {
                if self.editable()
                    && let Some(draft) = &mut self.draft
                {
                    draft.show_seconds = value;
                }
            }
            Message::MondayFirst(value) => {
                if self.editable()
                    && let Some(draft) = &mut self.draft
                {
                    draft.week_start_monday = value;
                }
            }
            Message::WeekNumbers(value) => {
                if self.editable()
                    && let Some(draft) = &mut self.draft
                {
                    draft.show_week_numbers = value;
                }
            }
            Message::Unit(value) => {
                if self.editable()
                    && let Some(draft) = &mut self.draft
                {
                    draft.temperature_unit = value;
                }
            }
            Message::TimezoneQuery(query) => {
                if self.editable() {
                    self.timezone_query = query;
                    let needle = self.timezone_query.trim().to_ascii_lowercase();
                    self.timezone_matches = if needle.is_empty() {
                        Vec::new()
                    } else {
                        chrono_tz::TZ_VARIANTS
                            .iter()
                            .copied()
                            .filter(|zone| zone.name().to_ascii_lowercase().contains(&needle))
                            .take(50)
                            .collect()
                    };
                }
            }
            Message::ClockLabel(label) => {
                if self.editable() {
                    self.clock_label = label;
                }
            }
            Message::AddClock(zone) => {
                let label = if self.clock_label.trim().is_empty() {
                    zone.name().replace('_', " ")
                } else {
                    self.clock_label.trim().to_owned()
                };
                self.add_clock(WorldClock {
                    label,
                    timezone: zone.name().into(),
                });
            }
            Message::RemoveClock(index) => {
                if self.editable()
                    && let Some(draft) = &mut self.draft
                    && index < draft.clocks.len()
                {
                    draft.clocks.remove(index);
                }
            }
            Message::PlaceQuery(query) => {
                if self.editable() {
                    self.place_query = query;
                    self.place_matches = places::search(&self.place_query);
                }
            }
            Message::AddPlace(place) => {
                self.add_clock(WorldClock {
                    label: place.label(),
                    timezone: place.timezone.into(),
                });
            }
            Message::MoveClock(index, forward) => {
                if self.editable()
                    && let Some(draft) = &mut self.draft
                {
                    let neighbor = if forward {
                        index.checked_add(1)
                    } else {
                        index.checked_sub(1)
                    };
                    if let Some(neighbor) = neighbor
                        && index < draft.clocks.len()
                        && neighbor < draft.clocks.len()
                    {
                        draft.clocks.swap(index, neighbor);
                    }
                }
            }
            Message::MoveSection(section, forward) => {
                if self.editable()
                    && let Some(draft) = &mut self.draft
                    && let Some((index, neighbor)) =
                        section_neighbor(&draft.layout, section, forward)
                {
                    draft.layout.swap(index, neighbor);
                }
            }
            Message::SectionSide(section, side) => {
                if self.editable()
                    && let Some(draft) = &mut self.draft
                    && let Some(placement) = draft
                        .layout
                        .iter_mut()
                        .find(|placement| placement.section == section)
                {
                    placement.side = side;
                }
            }
            Message::CityQuery(query) => {
                if self.editable() {
                    self.city_query = query;
                    self.invalidate_city_search();
                }
            }
            Message::SearchCities => return self.search_cities(),
            Message::CitiesLoaded(generation, result) => self.finish_cities(generation, result),
            Message::SelectCity(index) => {
                if self.editable()
                    && let Some(location) = self.cities.get(index).cloned()
                {
                    match location.validate() {
                        Ok(()) => {
                            if let Some(draft) = &mut self.draft {
                                draft.weather_location = Some(location);
                            }
                            self.invalidate_city_search();
                        }
                        Err(error) => self.city_error = Some(error),
                    }
                }
            }
            Message::RemoveCity => {
                if self.editable() {
                    if let Some(draft) = &mut self.draft {
                        draft.weather_location = None;
                    }
                    self.invalidate_city_search();
                }
            }
            Message::ForecastLoaded(generation, location, result) => {
                self.finish_forecast(generation, location, result)
            }
            Message::NotificationEvent(event) => match event {
                notifications::Event::Connected(client) => {
                    self.notification_client = Some(client);
                    self.notification_error = None;
                }
                notifications::Event::Received(notification) => self.history.receive(notification),
                notifications::Event::Closed { id, reason } => self.history.close(id, reason),
                notifications::Event::DndChanged(value) => {
                    self.dnd = value;
                    self.dnd_known = true;
                    self.dnd_revision = self.dnd_revision.wrapping_add(1);
                    self.dnd_error = None;
                }
                notifications::Event::DndUnavailable(error) => {
                    self.dnd_error = Some(error);
                    self.dnd_known = false;
                }
                notifications::Event::Unavailable(error) => {
                    self.notification_client = None;
                    self.notification_error = Some(error);
                    self.history.disconnect();
                }
            },
            Message::DismissNotification(id) => return self.notification_operation(id, None),
            Message::NotificationAction(id, action) => {
                return self.notification_operation(id, Some(action));
            }
            Message::NotificationFinished(id, revision, dismiss, result) => {
                if self.history.finish(id, revision, dismiss, &result)
                    && let Err(error) = result
                {
                    self.notification_action_error = Some(error);
                }
            }
            Message::ClearNotifications => {
                let ids: Vec<_> = self
                    .history
                    .entries
                    .iter()
                    .map(|entry| entry.notification.id)
                    .collect();
                return Task::batch(
                    ids.into_iter()
                        .map(|id| self.notification_operation(id, None)),
                );
            }
            Message::DoNotDisturb(value) => {
                if !self.dnd_known || self.dnd_pending.is_some() {
                    return Task::none();
                }
                self.dnd_pending = Some(value);
                self.dnd_requested_revision = self.dnd_revision;
                self.dnd_error = None;
                return Task::perform(notifications::set_do_not_disturb(value), move |result| {
                    cosmic::Action::App(Message::DndFinished(value, result))
                });
            }
            Message::DndFinished(value, result) => {
                if self.dnd_pending != Some(value) {
                    return Task::none();
                }
                self.dnd_pending = None;
                match result {
                    Ok(()) => {
                        if self.dnd_revision == self.dnd_requested_revision {
                            self.dnd = value;
                        }
                        self.dnd_error = None;
                    }
                    Err(error) => self.dnd_error = Some(error),
                }
            }
            Message::MediaEvent(event) => match event {
                media::Event::Players(players) => {
                    self.players = players;
                    self.media_error = None;
                    return self.refresh_artwork();
                }
                media::Event::Unavailable(error) => {
                    self.media_error = Some(error);
                }
            },
            Message::MediaControl(bus_name, action) => {
                if self.media_pending.contains(&bus_name) {
                    return Task::none();
                }
                let Some(player) = self
                    .players
                    .iter()
                    .find(|player| player.bus_name == bus_name)
                else {
                    return Task::none();
                };
                let allowed = match action {
                    media::Action::PlayPause => {
                        if player.playing {
                            player.can_pause
                        } else {
                            player.can_play
                        }
                    }
                    media::Action::Next => player.can_next,
                    media::Action::Previous => player.can_previous,
                    media::Action::Raise => player.can_raise,
                };
                if !allowed {
                    return Task::none();
                }
                self.media_pending.insert(bus_name.clone());
                self.media_error = None;
                let finished_name = bus_name.clone();
                return Task::perform(media::control(bus_name, action), move |result| {
                    cosmic::Action::App(Message::MediaFinished(finished_name.clone(), result))
                });
            }
            Message::MediaFinished(bus_name, result) => {
                self.media_pending.remove(&bus_name);
                if let Err(error) = result {
                    self.media_error = Some(error);
                }
            }
            Message::ArtworkLoaded(path, revision, image) => {
                if self.artwork_requests.get(&path) == Some(&revision) {
                    self.artwork_requests.remove(&path);
                    self.media_artwork.insert(path, image);
                }
            }
            Message::OpenMetCredit | Message::OpenGeoCredit => {
                self.credit_error = None;
                let url = if matches!(message, Message::OpenGeoCredit) {
                    "https://www.geonames.org/about.html"
                } else {
                    "https://www.met.no/en/free-meteorological-data/Licensing-and-crediting"
                };
                // Fixed argv only; never execute data from notifications or remote services.
                return Task::perform(crate::launcher::open_uri(url), |result| {
                    cosmic::Action::App(Message::CreditOpened(result))
                });
            }
            Message::CreditOpened(result) => self.credit_error = result.err(),
        }
        Task::none()
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

fn section_neighbor(
    layout: &[config::SectionPlacement],
    section: Section,
    forward: bool,
) -> Option<(usize, usize)> {
    let index = layout
        .iter()
        .position(|placement| placement.section == section)?;
    let side = layout[index].side;
    let neighbor = if forward {
        (index + 1..layout.len()).find(|&candidate| layout[candidate].side == side)
    } else {
        (0..index)
            .rev()
            .find(|&candidate| layout[candidate].side == side)
    }?;
    Some((index, neighbor))
}

struct ClockReading {
    time: String,
    abbreviation: String,
    day_difference: i64,
    offset_difference: i32,
}

impl ClockReading {
    fn indicator(&self, local_abbreviation: &str) -> String {
        let seconds = self.offset_difference;
        let sign = if seconds < 0 { '−' } else { '+' };
        let hours = seconds.abs() / 3600;
        let minutes = seconds.abs() % 3600 / 60;
        if minutes == 0 {
            format!("{}/{local_abbreviation}{sign}{hours}", self.abbreviation)
        } else {
            format!(
                "{}/{local_abbreviation}{sign}{hours}:{minutes:02}",
                self.abbreviation
            )
        }
    }
}

fn system_timezone() -> Option<Tz> {
    std::env::var("TZ")
        .ok()
        .and_then(|name| {
            name.trim_start_matches(':')
                .strip_prefix("/usr/share/zoneinfo/")
                .unwrap_or(name.trim_start_matches(':'))
                .parse()
                .ok()
        })
        .or_else(|| iana_time_zone::get_timezone().ok()?.parse().ok())
}

fn local_clock_abbreviation(now: DateTime<FixedOffset>, zone: Option<Tz>) -> String {
    if let Some(zone) = zone {
        let zoned = now.with_timezone(&zone);
        if zoned.offset().fix() == *now.offset() {
            return zoned.format("%Z").to_string();
        }
    }
    format!("UTC{}", now.format("%:z"))
}

fn world_clock_reading(
    now: DateTime<FixedOffset>,
    zone: Tz,
    preferences: &Preferences,
) -> ClockReading {
    let time = now.with_timezone(&zone);
    ClockReading {
        time: time.format(clock_format(preferences)).to_string(),
        abbreviation: time.format("%Z").to_string(),
        day_difference: time
            .date_naive()
            .signed_duration_since(now.date_naive())
            .num_days(),
        offset_difference: time.offset().fix().local_minus_utc() - now.offset().local_minus_utc(),
    }
}

async fn load_artwork(path: PathBuf) -> Option<widget::image::Handle> {
    tokio::task::spawn_blocking(move || decode_artwork(&path))
        .await
        .ok()
        .flatten()
}

fn decode_artwork(path: &std::path::Path) -> Option<widget::image::Handle> {
    use std::{io::Read, os::unix::fs::OpenOptionsExt};
    // NONBLOCK prevents a path swapped to a FIFO from blocking the worker.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    const MAX_BYTES: u64 = 8 * 1024 * 1024;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_BYTES {
        return None;
    }
    decode_artwork_bytes(&bytes)
}

fn decode_artwork_bytes(bytes: &[u8]) -> Option<widget::image::Handle> {
    use std::io::Cursor;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(32 * 1024 * 1024);
    let mut header = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    header.limits(limits.clone());
    let dimensions = header.into_dimensions().ok()?;
    if dimensions.0 == 0
        || dimensions.1 == 0
        || u64::from(dimensions.0) * u64::from(dimensions.1) > 4 * 1024 * 1024
    {
        return None;
    }
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.limits(limits);
    let image = reader
        .decode()
        .ok()?
        .resize(128, 128, image::imageops::FilterType::Triangle)
        .into_rgba8();
    Some(widget::image::Handle::from_rgba(
        image.width(),
        image.height(),
        image.into_raw(),
    ))
}

fn clock_tick_delay(now: DateTime<Utc>, seconds: bool) -> Duration {
    let period = if seconds { 1_000 } else { 60_000 };
    Duration::from_millis((period - now.timestamp_millis().rem_euclid(period)) as u64)
}

fn clock_format(preferences: &Preferences) -> &'static str {
    match (preferences.twenty_four_hour, preferences.show_seconds) {
        (true, false) => "%H:%M",
        (true, true) => "%H:%M:%S",
        (false, false) => "%-I:%M %p",
        (false, true) => "%-I:%M:%S %p",
    }
}

fn notification_age(now: DateTime<Utc>, received: DateTime<Utc>) -> String {
    let seconds = (now - received).num_seconds().max(0);
    if seconds < 60 {
        "Just now".into()
    } else if seconds < 3600 {
        format!("{} min ago", seconds / 60)
    } else if seconds < 86400 {
        format!("{} h ago", seconds / 3600)
    } else {
        received
            .with_timezone(&Local)
            .format("%a %-d %b")
            .to_string()
    }
}

fn card<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    widget::container(content)
        .padding(10)
        .width(Length::Fill)
        .class(cosmic::theme::Container::Card)
        .into()
}

fn action_button<'a>(label: &'a str, message: Message, enabled: bool) -> Element<'a, Message> {
    widget::button::standard(label)
        .on_press_maybe(enabled.then_some(message))
        .into()
}

fn choice_button<'a>(
    label: &'a str,
    message: Message,
    selected: bool,
    enabled: bool,
) -> Element<'a, Message> {
    widget::button::standard(label)
        .class(if selected {
            cosmic::theme::Button::Suggested
        } else {
            cosmic::theme::Button::Standard
        })
        .on_press_maybe(enabled.then_some(message))
        .into()
}

fn icon_button<'a>(
    icon: &'a str,
    name: &'a str,
    message: Message,
    enabled: bool,
) -> Element<'a, Message> {
    widget::button::icon(widget::icon::from_name(icon).size(16).handle())
        .name(name)
        .tooltip(name)
        .padding(5)
        .on_press_maybe(enabled.then_some(message))
        .into()
}

fn error_text<'a>(message: &'a str) -> Element<'a, Message> {
    widget::text(message)
        .size(12)
        .class(cosmic::theme::Text::Color(
            cosmic::theme::active().cosmic().destructive_color().into(),
        ))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn unread_clears_only_after_notification_overview_frame() {
        let mut app = AppModel::new(cosmic::Core::default());
        let popup = Id::unique();
        let _ = cosmic::Application::update(
            &mut app,
            Message::NotificationEvent(notifications::Event::Received(notification(1, false))),
        );
        assert!(app.history.unread());
        let _ = cosmic::Application::update(&mut app, Message::OverviewViewed(popup));
        assert!(
            app.history.unread(),
            "An unopened popup must not mark history viewed"
        );
        app.popup = Some(popup);
        let _ = cosmic::Application::update(&mut app, Message::OpenSettings);
        let _ = cosmic::Application::update(&mut app, Message::OverviewViewed(popup));
        assert!(
            app.history.unread(),
            "Settings do not display notification history"
        );
        let _ = cosmic::Application::update(&mut app, Message::CancelSettings);
        let date = app.calendar.selected;
        let _ = cosmic::Application::update(
            &mut app,
            Message::Agenda(agenda_ui::Message::Event(agenda::Event::Loaded(
                agenda::Snapshot::default(),
            ))),
        );
        let _ = cosmic::Application::update(
            &mut app,
            Message::Agenda(agenda_ui::Message::New(agenda::Kind::Task, date)),
        );
        let _ = cosmic::Application::update(&mut app, Message::OverviewViewed(popup));
        assert!(
            app.history.unread(),
            "Task editor does not display notification history"
        );
        let _ = cosmic::Application::update(&mut app, Message::Agenda(agenda_ui::Message::Cancel));
        let _ = cosmic::Application::update(&mut app, Message::OverviewViewed(Id::unique()));
        assert!(
            app.history.unread(),
            "Other windows cannot mark history viewed"
        );
        let _ = cosmic::Application::update(&mut app, Message::OverviewViewed(popup));
        assert!(!app.history.unread());
    }

    #[test]
    fn replacements_and_failed_dismissals_preserve_unread_revision() {
        let mut history = History::default();
        history.receive(notification(1, false));
        history.mark_viewed();
        history.receive(notification(1, false));
        assert!(history.unread());
        let revision = history.entries[0].revision;
        history.pending.insert(1, revision);
        assert!(history.finish(1, revision, true, &Err("offline".into())));
        assert!(history.unread());
        history.pending.insert(1, revision);
        history.receive(notification(1, false));
        assert!(!history.finish(1, revision, true, &Ok(())));
        assert!(history.unread());
        history.close(1, 2);
        assert!(!history.unread());
    }

    #[test]
    fn expiration_keeps_unseen_history_but_successful_dismissal_clears_dot() {
        let mut history = History::default();
        history.receive(notification(1, false));
        history.receive(notification(2, true));
        history.close(1, 1);
        history.close(2, 1);
        assert!(history.unread());
        let mut app = AppModel::new(cosmic::Core::default());
        app.history = history;
        let _ = cosmic::Application::update(&mut app, Message::DismissNotification(1));
        assert!(!app.history.unread());
    }

    #[test]
    fn action_routing_requires_supplied_active_nonpending_action() {
        let mut history = History::default();
        history.receive(notification(1, false));
        assert_eq!(history.action_revision(1, "default"), None);
        let mut supplied = notification(1, false);
        supplied.actions = vec![
            notifications::NotificationAction {
                key: "default".into(),
                label: "Open".into(),
            },
            notifications::NotificationAction {
                key: "reply".into(),
                label: "Reply".into(),
            },
        ];
        history.receive(supplied);
        let revision = history.entries[0].revision;
        assert_eq!(history.action_revision(1, "default"), Some(revision));
        assert_eq!(history.action_revision(1, "reply"), Some(revision));
        assert_eq!(history.action_revision(1, "invented"), None);
        history.pending.insert(1, revision);
        assert_eq!(history.action_revision(1, "default"), None);
        history.pending.clear();
        history.close(1, 1);
        assert_eq!(history.action_revision(1, "default"), None);
        assert_eq!(history.action_revision(1, "reply"), None);
    }

    #[test]
    fn popup_limits_follow_logical_monitor_height_and_reject_stale_queries() {
        let mut app = AppModel::new(cosmic::Core::default());
        app.screen_generation = 2;
        let _ = cosmic::Application::update(&mut app, Message::ScreenSize(2, Some((720.0, 960.0))));
        assert_eq!(app.popup_size(), (720.0, 720.0));
        let _ =
            cosmic::Application::update(&mut app, Message::ScreenSize(1, Some((1920.0, 1080.0))));
        assert_eq!(app.popup_size(), (720.0, 720.0));
        let _ =
            cosmic::Application::update(&mut app, Message::ScreenSize(2, Some((1536.0, 864.0))));
        assert_eq!(app.popup_size(), (820.0, 648.0));
    }

    #[test]
    fn world_clock_local_difference_tracks_both_seasonal_offsets() {
        let preferences = Preferences::default();
        for (month, day, expected) in [
            (1, 15, "CET/PST+9"),
            (3, 15, "CET/PDT+8"),
            (7, 15, "CEST/PDT+9"),
            (10, 28, "CET/PDT+8"),
        ] {
            let now = Utc
                .with_ymd_and_hms(2026, month, day, 12, 0, 0)
                .unwrap()
                .with_timezone(&chrono_tz::America::Los_Angeles)
                .fixed_offset();
            let reading = world_clock_reading(now, chrono_tz::Europe::Paris, &preferences);
            let local = local_clock_abbreviation(now, Some(chrono_tz::America::Los_Angeles));
            assert_eq!(reading.indicator(&local), expected);
        }
    }

    #[test]
    fn world_clock_offset_supports_half_quarter_hours_and_negative_day_rollover() {
        let preferences = Preferences::default();
        let instant = Utc.with_ymd_and_hms(2026, 1, 1, 0, 15, 0).unwrap();
        let utc = instant.fixed_offset();
        let india = world_clock_reading(utc, chrono_tz::Asia::Kolkata, &preferences);
        let nepal = world_clock_reading(utc, chrono_tz::Asia::Kathmandu, &preferences);
        assert_eq!(india.offset_difference, 19_800);
        assert!(india.indicator("UTC").ends_with("+5:30"));
        assert_eq!(nepal.offset_difference, 20_700);
        assert!(nepal.indicator("UTC").ends_with("+5:45"));
        let local = instant
            .with_timezone(&chrono_tz::Asia::Kathmandu)
            .fixed_offset();
        let india = world_clock_reading(local, chrono_tz::Asia::Kolkata, &preferences);
        assert!(india.indicator("+0545").ends_with("−0:15"));
        let los_angeles = world_clock_reading(local, chrono_tz::America::Los_Angeles, &preferences);
        assert_eq!(
            (los_angeles.day_difference, los_angeles.offset_difference),
            (-1, -49_500)
        );
    }

    #[test]
    fn backend_snapshot_does_not_reset_settings_search_or_layout_draft() {
        let mut app = AppModel::new(cosmic::Core::default());
        app.config_ready = true;
        app.draft = Some(app.preferences.clone());
        app.place_query = "Texas".into();
        app.timezone_query = "America".into();
        app.clock_label = "Personal clock".into();
        app.city_query = "Austin".into();
        let _ = cosmic::Application::update(
            &mut app,
            Message::SectionSide(Section::Weather, Side::Left),
        );
        let draft = app.draft.clone();
        let _ = cosmic::Application::update(
            &mut app,
            Message::Agenda(agenda_ui::Message::Event(agenda::Event::Loaded(
                agenda::Snapshot {
                    revision: 4,
                    entries: Vec::new(),
                },
            ))),
        );
        assert_eq!(
            (
                &app.place_query,
                &app.timezone_query,
                &app.clock_label,
                &app.city_query
            ),
            (
                &"Texas".to_owned(),
                &"America".to_owned(),
                &"Personal clock".to_owned(),
                &"Austin".to_owned()
            )
        );
        assert_eq!(app.draft, draft);
    }

    #[test]
    fn layout_moves_within_side_and_settings_cancel_reverts_clocks_and_layout() {
        let mut app = AppModel::new(cosmic::Core::default());
        app.config_ready = true;
        app.draft = Some(app.preferences.clone());
        let original = app.preferences.clone();
        let _ = cosmic::Application::update(&mut app, Message::MoveSection(Section::Weather, true));
        let _ = cosmic::Application::update(
            &mut app,
            Message::SectionSide(Section::Calendar, Side::Left),
        );
        app.add_clock(WorldClock {
            label: "Dallas".into(),
            timezone: "America/Chicago".into(),
        });
        app.add_clock(WorldClock {
            label: "Austin".into(),
            timezone: "America/Chicago".into(),
        });
        let _ = cosmic::Application::update(&mut app, Message::MoveClock(2, false));
        let draft = app.draft.as_ref().unwrap();
        assert_eq!(draft.clocks[1].label, "Austin");
        let right: Vec<_> = draft
            .layout
            .iter()
            .filter(|placement| placement.side == Side::Right)
            .map(|placement| placement.section)
            .collect();
        assert_eq!(right, [Section::WorldClocks, Section::Weather]);
        assert!(draft.validate().is_ok());
        let _ = cosmic::Application::update(&mut app, Message::CancelSettings);
        assert_eq!(app.preferences, original);
    }

    #[test]
    fn saved_layout_and_clock_order_commit_only_after_success() {
        let mut app = AppModel::new(cosmic::Core::default());
        app.config_ready = true;
        app.draft = Some(app.preferences.clone());
        let _ = cosmic::Application::update(&mut app, Message::MoveSection(Section::Weather, true));
        let _ = cosmic::Application::update(
            &mut app,
            Message::SectionSide(Section::Weather, Side::Left),
        );
        app.add_clock(WorldClock {
            label: "Dallas".into(),
            timezone: "America/Chicago".into(),
        });
        let expected = app.draft.as_ref().unwrap().clone();
        let (generation, _) = app.begin_save().unwrap();
        assert_ne!(app.preferences, expected);
        assert!(app.finish_save(generation, Ok(())));
        assert_eq!(app.preferences, expected);
    }

    #[test]
    fn clock_ticks_align_to_wall_clock_boundaries_without_busy_looping() {
        let between = DateTime::parse_from_rfc3339("2026-10-13T12:34:56.250Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            clock_tick_delay(between, false),
            Duration::from_millis(3_750)
        );
        assert_eq!(clock_tick_delay(between, true), Duration::from_millis(750));
        let boundary = Utc.with_ymd_and_hms(2026, 10, 13, 12, 35, 0).unwrap();
        assert_eq!(clock_tick_delay(boundary, false), Duration::from_secs(60));
        assert_eq!(clock_tick_delay(boundary, true), Duration::from_secs(1));
    }

    fn notification(id: u32, transient: bool) -> notifications::Notification {
        notifications::Notification {
            id,
            app_name: String::new(),
            app_icon: String::new(),
            summary: String::new(),
            body: String::new(),
            received_at: Utc.timestamp_opt(0, 0).unwrap(),
            actions: Vec::new(),
            transient,
            image: None,
        }
    }

    fn location(name: &str) -> WeatherLocation {
        WeatherLocation {
            name: name.into(),
            latitude: 59.9139,
            longitude: 10.7522,
            timezone: "Europe/Oslo".into(),
        }
    }

    #[test]
    fn spring_dst_skips_missing_hour_and_autumn_repeats_with_correct_offset() {
        let zone = chrono_tz::Europe::Paris;
        let preferences = Preferences::default();
        let before = world_clock_reading(
            Utc.with_ymd_and_hms(2026, 3, 29, 0, 30, 0)
                .unwrap()
                .fixed_offset(),
            zone,
            &preferences,
        );
        let after = world_clock_reading(
            Utc.with_ymd_and_hms(2026, 3, 29, 1, 30, 0)
                .unwrap()
                .fixed_offset(),
            zone,
            &preferences,
        );
        assert_eq!(
            (before.time.as_str(), before.abbreviation.as_str()),
            ("01:30", "CET")
        );
        assert_eq!(
            (after.time.as_str(), after.abbreviation.as_str()),
            ("03:30", "CEST")
        );
        let first = world_clock_reading(
            Utc.with_ymd_and_hms(2026, 10, 25, 0, 30, 0)
                .unwrap()
                .fixed_offset(),
            zone,
            &preferences,
        );
        let second = world_clock_reading(
            Utc.with_ymd_and_hms(2026, 10, 25, 1, 30, 0)
                .unwrap()
                .fixed_offset(),
            zone,
            &preferences,
        );
        assert_eq!(
            (first.time.as_str(), first.abbreviation.as_str()),
            ("02:30", "CEST")
        );
        assert_eq!(
            (second.time.as_str(), second.abbreviation.as_str()),
            ("02:30", "CET")
        );
    }

    #[test]
    fn world_clock_indicator_uses_local_abbreviation_and_relative_offset() {
        let now = Utc
            .with_ymd_and_hms(2026, 7, 15, 12, 0, 0)
            .unwrap()
            .with_timezone(&chrono_tz::Europe::Paris)
            .fixed_offset();
        let local = local_clock_abbreviation(now, Some(chrono_tz::Europe::Paris));
        let reading = world_clock_reading(
            now,
            chrono_tz::America::Los_Angeles,
            &Preferences::default(),
        );
        assert_eq!(reading.indicator(&local), "PDT/CEST−9");
        assert_eq!(reading.time, "05:00");
        let same = world_clock_reading(now, chrono_tz::Europe::Paris, &Preferences::default());
        assert_eq!(same.indicator(&local), "CEST/CEST+0");
    }

    #[test]
    fn local_abbreviation_falls_back_to_actual_offset_when_zone_is_unknown_or_stale() {
        let now = Utc
            .with_ymd_and_hms(2026, 7, 15, 12, 0, 0)
            .unwrap()
            .with_timezone(&chrono_tz::Europe::Paris)
            .fixed_offset();
        assert_eq!(local_clock_abbreviation(now, None), "UTC+02:00");
        assert_eq!(
            local_clock_abbreviation(now, Some(chrono_tz::America::Los_Angeles)),
            "UTC+02:00",
        );
    }

    #[test]
    fn half_hour_dst_updates_the_relative_offset() {
        let preferences = Preferences::default();
        let summer = Utc
            .with_ymd_and_hms(2026, 1, 15, 12, 0, 0)
            .unwrap()
            .fixed_offset();
        let winter = Utc
            .with_ymd_and_hms(2026, 7, 15, 12, 0, 0)
            .unwrap()
            .fixed_offset();
        let summer = world_clock_reading(summer, chrono_tz::Australia::Lord_Howe, &preferences);
        let winter = world_clock_reading(winter, chrono_tz::Australia::Lord_Howe, &preferences);
        assert_eq!(summer.offset_difference - winter.offset_difference, 1800);
    }

    #[test]
    fn irish_clock_abbreviation_and_offset_follow_the_season() {
        let preferences = Preferences::default();
        let winter = Utc
            .with_ymd_and_hms(2026, 1, 15, 12, 0, 0)
            .unwrap()
            .fixed_offset();
        let summer = Utc
            .with_ymd_and_hms(2026, 7, 15, 12, 0, 0)
            .unwrap()
            .fixed_offset();
        let winter = world_clock_reading(winter, chrono_tz::Europe::Dublin, &preferences);
        let summer = world_clock_reading(summer, chrono_tz::Europe::Dublin, &preferences);
        assert_eq!(winter.indicator("UTC"), "GMT/UTC+0");
        assert_eq!(summer.indicator("UTC"), "IST/UTC+1");
    }

    #[test]
    fn world_clock_day_difference_handles_international_date_line() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 11, 30, 0).unwrap();
        let local = now
            .with_timezone(&chrono_tz::Pacific::Honolulu)
            .fixed_offset();
        let reading = world_clock_reading(
            local,
            chrono_tz::Pacific::Kiritimati,
            &Preferences::default(),
        );
        assert_eq!(reading.day_difference, 1);
    }

    #[test]
    fn expiration_retains_only_nontransient_nonactionable_history() {
        let mut history = History::default();
        history.receive(notification(1, false));
        history.receive(notification(2, true));
        history.close(1, 1);
        history.close(2, 1);
        assert_eq!(history.entries.len(), 1);
        assert!(!history.entries[0].active);
        history.close(1, 2);
        assert!(history.entries.is_empty());
    }

    #[test]
    fn history_is_bounded_and_old_dismissal_cannot_remove_replacement() {
        let mut history = History::default();
        for id in 0..150 {
            history.receive(notification(id, false));
        }
        assert_eq!(history.entries.len(), HISTORY_LIMIT);
        let revision = history.entries[0].revision;
        history.pending.insert(149, revision);
        history.receive(notification(149, false));
        assert!(!history.finish(149, revision, true, &Ok(())));
        assert_eq!(history.entries[0].notification.id, 149);
    }

    #[test]
    fn failed_save_keeps_preferences_and_serializes_transactions() {
        let mut app = AppModel::new(cosmic::Core::default());
        app.config_ready = true;
        app.draft = Some(app.preferences.clone());
        app.draft.as_mut().unwrap().twenty_four_hour = false;
        let (generation, _) = app.begin_save().unwrap();
        assert!(app.begin_save().is_none());
        assert!(!app.finish_save(generation + 1, Ok(())));
        assert!(app.pending_save.is_some());
        assert!(!app.finish_save(generation, Err("disk full".into())));
        assert!(app.preferences.twenty_four_hour);
        assert!(!app.draft.as_ref().unwrap().twenty_four_hour);
    }

    #[test]
    fn invalid_existing_config_and_invalid_drafts_cannot_be_saved() {
        let mut app = AppModel::new(cosmic::Core::default());
        app.draft = Some(app.preferences.clone());
        assert!(app.begin_save().is_none());
        app.config_ready = true;
        app.draft.as_mut().unwrap().clocks[0].timezone = "Not/AZone".into();
        assert!(app.begin_save().is_none());
        assert!(app.config_error.is_some());
    }

    #[test]
    fn stale_city_search_cannot_restore_old_query_or_cancelled_settings() {
        let mut app = AppModel::new(cosmic::Core::default());
        app.config_ready = true;
        app.draft = Some(app.preferences.clone());
        app.city_generation = 3;
        app.city_loading = true;
        let _ = cosmic::Application::update(&mut app, Message::CityQuery("Paris".into()));
        app.finish_cities(3, Ok(vec![location("Oslo")]));
        assert!(app.cities.is_empty());
        let generation = app.city_generation;
        app.leave_settings();
        app.finish_cities(generation, Ok(vec![location("Paris")]));
        assert!(app.cities.is_empty());
    }

    #[test]
    fn location_change_rejects_late_forecast_and_preserves_current_loading_state() {
        let mut app = AppModel::new(cosmic::Core::default());
        app.preferences.weather_location = Some(location("Paris"));
        app.forecast_generation = 4;
        app.forecast_loading = true;
        app.finish_forecast(3, location("Oslo"), Err("old request".into()));
        assert!(app.forecast_loading);
        assert!(app.forecast_error.is_none());
        app.finish_forecast(4, location("Oslo"), Err("wrong location".into()));
        assert!(app.forecast_loading);
        assert!(app.forecast_error.is_none());
    }

    #[test]
    fn cancelling_settings_discards_draft_without_changing_clock() {
        let mut app = AppModel::new(cosmic::Core::default());
        app.config_ready = true;
        app.draft = Some(app.preferences.clone());
        app.draft.as_mut().unwrap().show_seconds = true;
        let _ = cosmic::Application::update(&mut app, Message::CancelSettings);
        assert!(app.draft.is_none());
        assert!(!app.preferences.show_seconds);
    }

    #[test]
    fn dnd_completion_does_not_overwrite_newer_daemon_state() {
        let mut app = AppModel::new(cosmic::Core::default());
        app.dnd_pending = Some(true);
        app.dnd_requested_revision = 0;
        let _ = cosmic::Application::update(
            &mut app,
            Message::NotificationEvent(notifications::Event::DndChanged(false)),
        );
        let _ = cosmic::Application::update(&mut app, Message::DndFinished(true, Ok(())));
        assert!(!app.dnd);
        assert!(app.dnd_pending.is_none());
    }

    #[test]
    fn dnd_read_failure_preserves_live_notifications() {
        let mut app = AppModel::new(cosmic::Core::default());
        app.history.receive(notification(42, false));
        let _ = cosmic::Application::update(
            &mut app,
            Message::NotificationEvent(notifications::Event::DndUnavailable(
                "unreadable preference".into(),
            )),
        );
        assert!(app.history.entries[0].active);
        assert!(app.dnd_error.is_some());
    }

    #[test]
    fn unusable_artwork_uses_fallback_and_removed_requests_cannot_return() {
        assert!(decode_artwork_bytes(b"not an image").is_none());
        let mut app = AppModel::new(cosmic::Core::default());
        let path = PathBuf::from("/nonexistent/artwork.png");
        app.artwork_requests.insert(path.clone(), 2);
        let _ = app.refresh_artwork();
        let _ = cosmic::Application::update(&mut app, Message::ArtworkLoaded(path, 2, None));
        assert!(app.media_artwork.is_empty());
    }

    #[test]
    fn successful_save_commits_location_and_invalidates_old_forecast_scope() {
        let mut app = AppModel::new(cosmic::Core::default());
        app.config_ready = true;
        app.draft = Some(app.preferences.clone());
        app.draft.as_mut().unwrap().weather_location = Some(location("Oslo"));
        app.forecast_generation = 10;
        app.forecast_loading = true;
        let (generation, _) = app.begin_save().unwrap();
        assert!(app.finish_save(generation, Ok(())));
        assert_eq!(app.preferences.weather_location, Some(location("Oslo")));
        assert!(app.draft.is_none());
        assert!(!app.forecast_loading);
        assert_eq!(app.forecast_generation, 11);
    }

    #[test]
    fn selecting_weather_city_invalidates_old_results_without_changing_saved_location() {
        let mut app = AppModel::new(cosmic::Core::default());
        app.config_ready = true;
        app.draft = Some(app.preferences.clone());
        app.cities = vec![location("Oslo")];
        let generation = app.city_generation;
        let _ = cosmic::Application::update(&mut app, Message::SelectCity(0));
        app.finish_cities(generation, Ok(vec![location("Paris")]));
        assert!(app.cities.is_empty());
        assert_eq!(
            app.draft.as_ref().unwrap().weather_location,
            Some(location("Oslo"))
        );
        assert!(app.preferences.weather_location.is_none());
    }

    #[test]
    fn artwork_decoder_rejects_oversized_dimensions() {
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(4097, 1)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        assert!(decode_artwork_bytes(encoded.get_ref()).is_none());
    }

    #[test]
    fn met_symbols_handle_day_night_precipitation_and_unknown_codes() {
        assert_eq!(
            weather::icon_name("clearsky_night"),
            "weather-clear-night-symbolic"
        );
        assert_eq!(
            weather::icon_name("partlycloudy_day"),
            "weather-few-clouds-symbolic"
        );
        assert_eq!(
            weather::icon_name("heavyrainandthunder"),
            "weather-storm-symbolic"
        );
        assert_eq!(
            weather::icon_name("lightsleetshowers_day"),
            "weather-snow-symbolic"
        );
        assert_eq!(weather::icon_name("rain"), "weather-showers-symbolic");
        assert_eq!(weather::icon_name(""), "dialog-question-symbolic");
    }
}
