// SPDX-License-Identifier: GPL-3.0-only

use crate::storage;
use cosmic_applet_datetime_plus::weather::WeatherLocation;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const APP_ID: &str = "io.github.cosmic-applet-datetime-plus";
pub const NOTIFICATION_PREVIEW_MIN: u32 = 25;
pub const NOTIFICATION_PREVIEW_MAX: u32 = 2000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldClock {
    pub label: String,
    pub timezone: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TemperatureUnit {
    #[default]
    Celsius,
    Fahrenheit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Section {
    Media,
    Notifications,
    Calendar,
    Weather,
    WorldClocks,
}

impl Section {
    pub const ALL: [Self; 5] = [
        Self::Media,
        Self::Notifications,
        Self::Calendar,
        Self::Weather,
        Self::WorldClocks,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Media => "Media",
            Self::Notifications => "Notifications",
            Self::Calendar => "Calendar and agenda",
            Self::Weather => "Weather",
            Self::WorldClocks => "World clocks",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
}

impl Side {
    pub fn label(self) -> &'static str {
        match self {
            Self::Left => "Left",
            Self::Right => "Right",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SectionPlacement {
    pub section: Section,
    pub side: Side,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Preferences {
    pub twenty_four_hour: bool,
    pub show_seconds: bool,
    pub week_start_monday: bool,
    pub show_week_numbers: bool,
    pub notification_preview_limit: u32,
    pub temperature_unit: TemperatureUnit,
    pub weather_location: Option<WeatherLocation>,
    pub clocks: Vec<WorldClock>,
    pub layout: Vec<SectionPlacement>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            twenty_four_hour: true,
            show_seconds: false,
            week_start_monday: true,
            show_week_numbers: true,
            notification_preview_limit: 100,
            temperature_unit: TemperatureUnit::Celsius,
            weather_location: None,
            clocks: vec![WorldClock {
                label: "Coordinated Universal Time".into(),
                timezone: "UTC".into(),
            }],
            layout: Section::ALL
                .into_iter()
                .map(|section| SectionPlacement {
                    side: match section {
                        Section::Media | Section::Notifications => Side::Left,
                        _ => Side::Right,
                    },
                    section,
                })
                .collect(),
        }
    }
}

impl Preferences {
    pub fn validate(&self) -> Result<(), String> {
        if !(NOTIFICATION_PREVIEW_MIN..=NOTIFICATION_PREVIEW_MAX)
            .contains(&self.notification_preview_limit)
        {
            return Err(format!(
                "Notification preview limit must be {NOTIFICATION_PREVIEW_MIN}–{NOTIFICATION_PREVIEW_MAX} characters."
            ));
        }
        if self.layout.len() != Section::ALL.len()
            || Section::ALL.iter().any(|section| {
                self.layout
                    .iter()
                    .filter(|placement| placement.section == *section)
                    .count()
                    != 1
            })
        {
            return Err("The layout must include each section exactly once.".into());
        }
        if let Some(location) = &self.weather_location {
            location.validate()?;
        }
        for clock in &self.clocks {
            validate_label(&clock.label)?;
            clock
                .timezone
                .parse::<chrono_tz::Tz>()
                .map_err(|_| format!("Unknown IANA timezone: {}", clock.timezone))?;
        }
        Ok(())
    }
}

fn validate_label(label: &str) -> Result<(), String> {
    if label.trim().is_empty() || label.len() > 160 || label.chars().any(char::is_control) {
        return Err(
            "Location and clock names must contain 1–160 bytes without control characters.".into(),
        );
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsFile<T> {
    version: u8,
    preferences: T,
}

fn decode(bytes: &[u8]) -> Result<Preferences, String> {
    let file: SettingsFile<Preferences> = serde_json::from_slice(bytes)
        .map_err(|error| format!("Saved settings contain invalid JSON: {error}"))?;
    if file.version != 1 {
        return Err(
            "Saved settings use an unsupported version; the file has not been changed.".into(),
        );
    }
    file.preferences.validate()?;
    Ok(file.preferences)
}

pub async fn load() -> Result<Preferences, String> {
    load_from(&storage::data_path("settings.json")?).await
}

async fn load_from(path: &Path) -> Result<Preferences, String> {
    match storage::read(path).await? {
        Some(bytes) => decode(&bytes),
        None => Ok(Preferences::default()),
    }
}

pub async fn save(preferences: &Preferences) -> Result<(), String> {
    save_to(&storage::data_path("settings.json")?, preferences).await
}

async fn save_to(path: &Path, preferences: &Preferences) -> Result<(), String> {
    preferences.validate()?;
    storage::write_json(
        path,
        &SettingsFile {
            version: 1,
            preferences,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        env,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };
    use tokio::fs;

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn legacy_preferences_gain_layout_without_losing_user_choices() {
        let preferences = decode(
            br#"{"version":1,"preferences":{"twenty_four_hour":false,"clocks":[{"label":"Dallas","timezone":"America/Chicago"}]}}"#,
        )
        .unwrap();
        assert!(!preferences.twenty_four_hour);
        assert_eq!(preferences.clocks[0].label, "Dallas");
        assert_eq!(preferences.clocks[0].timezone, "America/Chicago");
        assert_eq!(preferences.layout, Preferences::default().layout);
        assert_eq!(preferences.notification_preview_limit, 100);
    }

    #[test]
    fn notification_preview_bounds_accept_endpoints_and_reject_invalid_saved_values() {
        for limit in [NOTIFICATION_PREVIEW_MIN, NOTIFICATION_PREVIEW_MAX] {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "version": 1, "preferences": { "notification_preview_limit": limit }
            }))
            .unwrap();
            assert_eq!(decode(&bytes).unwrap().notification_preview_limit, limit);
        }
        for limit in [
            0,
            NOTIFICATION_PREVIEW_MIN - 1,
            NOTIFICATION_PREVIEW_MAX + 1,
            u32::MAX,
        ] {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "version": 1, "preferences": { "notification_preview_limit": limit }
            }))
            .unwrap();
            assert!(decode(&bytes).is_err(), "Accepted invalid limit {limit}");
        }
    }

    #[test]
    fn incomplete_or_duplicate_layouts_are_rejected() {
        let mut preferences = Preferences::default();
        preferences.layout.pop();
        assert!(preferences.validate().is_err());
        preferences.layout.push(preferences.layout[0].clone());
        assert!(preferences.validate().is_err());
    }
    #[test]
    fn invalid_coordinates_and_timezones_do_not_become_preferences() {
        let mut location = WeatherLocation {
            name: "Oslo".into(),
            latitude: 59.9139,
            longitude: 10.7522,
            timezone: "Europe/Oslo".into(),
        };
        location.latitude = f64::NAN;
        assert!(location.validate().is_err());
        location.latitude = 91.0;
        assert!(location.validate().is_err());
        location.latitude = 59.9139;
        location.timezone = "Not/AZone".into();
        assert!(location.validate().is_err());
    }
    #[test]
    fn corrupted_and_future_settings_are_not_reset() {
        assert!(decode(b"{broken").is_err());
        assert!(decode(br#"{"version":2,"preferences":{}}"#).is_err());
        assert!(decode(br#"{"version":1,"preferences":{"clocks":[{"label":"Clock","timezone":"Not/AZone"}]}}"#).is_err());
    }

    #[tokio::test]
    async fn saved_settings_round_trip_privately_and_invalid_edits_preserve_them() {
        use std::os::unix::fs::PermissionsExt;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = env::temp_dir().join(format!(
            "datetime-config-{}-{stamp}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let path = directory.join("settings.json");
        let mut layout = Preferences::default().layout;
        layout.reverse();
        layout[0].side = Side::Left;
        let preferences = Preferences {
            twenty_four_hour: false,
            layout,
            notification_preview_limit: 375,
            weather_location: Some(WeatherLocation {
                name: "Oslo".into(),
                latitude: 59.9139,
                longitude: 10.7522,
                timezone: "Europe/Oslo".into(),
            }),
            clocks: vec![WorldClock {
                label: "New York".into(),
                timezone: "America/New_York".into(),
            }],
            ..Preferences::default()
        };
        save_to(&path, &preferences).await.unwrap();
        assert_eq!(load_from(&path).await.unwrap(), preferences);
        assert_eq!(
            fs::metadata(&path).await.unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&directory).await.unwrap().permissions().mode() & 0o777,
            0o700
        );
        let mut invalid = preferences.clone();
        invalid.clocks[0].timezone = "Not/AZone".into();
        assert!(save_to(&path, &invalid).await.is_err());
        assert_eq!(load_from(&path).await.unwrap(), preferences);
        invalid = preferences.clone();
        invalid.notification_preview_limit = NOTIFICATION_PREVIEW_MIN - 1;
        assert!(save_to(&path, &invalid).await.is_err());
        assert_eq!(load_from(&path).await.unwrap(), preferences);
        fs::write(&path, b"{broken").await.unwrap();
        assert!(load_from(&path).await.is_err());
        assert_eq!(fs::read(&path).await.unwrap(), b"{broken");
        fs::remove_dir_all(directory).await.unwrap();
    }
}
