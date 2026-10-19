// SPDX-License-Identifier: GPL-3.0-only

//! MET Norway Locationforecast client. City searches use Open-Meteo/GeoNames only.
//! Forecast requests are serialized and rate-limited across applet instances and restarts.

use std::{
    collections::HashMap,
    env,
    fs::Permissions,
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use chrono::{DateTime, NaiveDateTime, TimeDelta, Utc};
use reqwest::{
    StatusCode,
    header::{ACCEPT, EXPIRES, HeaderMap, IF_MODIFIED_SINCE, LAST_MODIFIED, RETRY_AFTER},
};
use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Mutex,
};

const USER_AGENT: &str =
    "cosmos-applet-datetime-plus/0.1.0 (https://github.com/petterssonjonas/cosmos-applet-datetime-plus)";
const FORECAST_URL: &str = "https://api.met.no/weatherapi/locationforecast/2.0/compact";
const SEARCH_URL: &str = "https://geocoding-api.open-meteo.com/v1/search";
const MIN_CADENCE: i64 = 15 * 60;
const MAX_RESPONSE: usize = 512 * 1024;
const MAX_CACHE: usize = 1024 * 1024;
const MAX_HOURS: usize = 500;
const MEMORY_LOCATIONS: usize = 16;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeatherLocation {
    pub name: String,
    pub latitude: f64,
    pub longitude: f64,
    pub timezone: String,
}

impl WeatherLocation {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty()
            || self.name.len() > 160
            || self.name.chars().any(char::is_control)
        {
            return Err(
                "Location names must contain 1–160 bytes without control characters.".into(),
            );
        }
        if !self.latitude.is_finite()
            || !(-90.0..=90.0).contains(&self.latitude)
            || !self.longitude.is_finite()
            || !(-180.0..=180.0).contains(&self.longitude)
        {
            return Err(
                "Weather coordinates are outside the valid latitude/longitude range.".into(),
            );
        }
        self.timezone
            .parse::<chrono_tz::Tz>()
            .map_err(|_| "Choose a valid IANA weather timezone.".to_owned())?;
        Ok(())
    }
}

pub fn icon_name(symbol: &str) -> &'static str {
    let code = symbol.split('_').next().unwrap_or(symbol);
    if code.contains("thunder") {
        "weather-storm-symbolic"
    } else if code.contains("snow") || code.contains("sleet") {
        "weather-snow-symbolic"
    } else if code.contains("rain") {
        "weather-showers-symbolic"
    } else {
        match code {
            "clearsky" if symbol.ends_with("_night") => "weather-clear-night-symbolic",
            "clearsky" => "weather-clear-symbolic",
            "fair" | "partlycloudy" if symbol.ends_with("_night") => {
                "weather-few-clouds-night-symbolic"
            }
            "fair" | "partlycloudy" => "weather-few-clouds-symbolic",
            "cloudy" => "weather-overcast-symbolic",
            "fog" => "weather-fog-symbolic",
            _ => "dialog-question-symbolic",
        }
    }
}

/// Human-readable MET condition, without inventing a condition for unknown codes.
pub fn description(symbol: &str) -> String {
    let code = symbol
        .strip_suffix("_day")
        .or_else(|| symbol.strip_suffix("_night"))
        .or_else(|| symbol.strip_suffix("_polartwilight"))
        .unwrap_or(symbol);
    match code {
        "" => return "Conditions unavailable".into(),
        "clearsky" => return "Clear sky".into(),
        "fair" => return "Mostly clear".into(),
        "partlycloudy" => return "Partly cloudy".into(),
        "cloudy" => return "Cloudy".into(),
        "fog" => return "Fog".into(),
        _ => {}
    }
    let (code, thunder) = code
        .strip_suffix("andthunder")
        .map_or((code, false), |code| (code, true));
    // MET also uses these doubled-s spellings for light sleet/snow thunder showers.
    let code = match code {
        "lightssleetshowers" => "lightsleetshowers",
        "lightssnowshowers" => "lightsnowshowers",
        code => code,
    };
    let (code, intensity) = if let Some(code) = code.strip_prefix("light") {
        (code, "Light ")
    } else if let Some(code) = code.strip_prefix("heavy") {
        (code, "Heavy ")
    } else {
        (code, "")
    };
    let condition = match code {
        "rain" => "rain",
        "sleet" => "sleet",
        "snow" => "snow",
        "rainshowers" => "rain showers",
        "sleetshowers" => "sleet showers",
        "snowshowers" => "snow showers",
        _ => return "Unknown conditions".into(),
    };
    let mut text = format!("{intensity}{condition}");
    // All supported condition words are ASCII.
    text[..1].make_ascii_uppercase();
    if thunder {
        text.push_str(" with thunder");
    }
    text
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Forecast {
    pub location: WeatherLocation,
    pub generated_at: DateTime<Utc>,
    pub fetched_at: DateTime<Utc>,
    pub valid_until: DateTime<Utc>,
    pub hours: Vec<ForecastHour>,
    pub stale: bool,
    pub warning: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForecastHour {
    pub time: DateTime<Utc>,
    pub temperature_c: f64,
    /// The actual MET symbol code; empty when the provider omits a summary.
    pub symbol: String,
    /// One-hour precipitation only, never a six-hour total presented as hourly rain.
    pub precipitation_mm: Option<f64>,
    /// MET wind speed in metres per second; absent or invalid measurements stay unavailable.
    #[serde(default)]
    pub wind_speed_mps: Option<f64>,
    /// MET relative humidity in percent, within 0–100 inclusive.
    #[serde(default)]
    pub relative_humidity_percent: Option<f64>,
}

impl ForecastHour {
    /// Calculated shade apparent temperature, not a provider measurement.
    ///
    /// Uses the [NWS wind chill](https://www.weather.gov/safety/cold-wind-chill-chart)
    /// at temperatures ≤10°C and wind speeds >3 mph (standard 10-metre wind).
    /// Uses the [NWS heat index](https://www.wpc.ncep.noaa.gov/html/heatindex_equation.shtml)
    /// with Rothfusz regression, humidity adjustments and the preliminary 80°F gate.
    /// Heat-index input is conservatively limited to 80–112°F to avoid extrapolating
    /// into extreme heat. These indices assume shade, not direct sun.
    /// Missing/invalid required measurements or inapplicable conditions return `None`;
    /// the actual temperature is never substituted as a calculated estimate.
    pub fn apparent_temperature_c(&self) -> Option<f64> {
        let temperature = self.temperature_c;
        if !temperature.is_finite() {
            return None;
        }
        if temperature <= 10.0 {
            let wind = self.wind_speed_mps.filter(|value| valid_wind(*value))?;
            if wind <= 3.0 * 0.44704 {
                return None;
            }
            let wind_mph = wind / 0.44704;
            let temperature_f = temperature * 1.8 + 32.0;
            let speed_factor = wind_mph.powf(0.16);
            let chill_f = 35.74 + 0.6215 * temperature_f - 35.75 * speed_factor
                + 0.4275 * temperature_f * speed_factor;
            return Some((chill_f - 32.0) / 1.8).filter(|value| value.is_finite());
        }
        let temperature_f = temperature * 1.8 + 32.0;
        if !(80.0..=112.0).contains(&temperature_f) {
            return None;
        }
        let humidity = self
            .relative_humidity_percent
            .filter(|value| valid_humidity(*value))?;
        let preliminary =
            0.5 * (temperature_f + 61.0 + (temperature_f - 68.0) * 1.2 + humidity * 0.094);
        if (preliminary + temperature_f) * 0.5 < 80.0 {
            return None;
        }
        let t = temperature_f;
        let r = humidity;
        let mut index = -42.379 + 2.04901523 * t + 10.14333127 * r
            - 0.22475541 * t * r
            - 0.00683783 * t * t
            - 0.05481717 * r * r
            + 0.00122874 * t * t * r
            + 0.00085282 * t * r * r
            - 0.00000199 * t * t * r * r;
        if r < 13.0 {
            index -= (13.0 - r) / 4.0 * ((17.0 - (t - 95.0).abs()) / 17.0).sqrt();
        } else if r > 85.0 && t <= 87.0 {
            index += (r - 85.0) / 10.0 * (87.0 - t) / 5.0;
        }
        Some((index - 32.0) / 1.8).filter(|value| value.is_finite())
    }
}

fn valid_wind(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

fn valid_humidity(value: f64) -> bool {
    value.is_finite() && (0.0..=100.0).contains(&value)
}

#[derive(Debug)]
pub struct Client {
    http: reqwest::Client,
    cache_dir: PathBuf,
    weather: Mutex<WeatherState>,
    search: Mutex<SearchState>,
}

#[derive(Debug, Default)]
struct WeatherState {
    initialized: bool,
    ledger: Ledger,
    entries: HashMap<String, Option<CacheRecord>>,
    persistence_error: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Ledger {
    version: u8,
    next_request: Option<DateTime<Utc>>,
    last_error: Option<String>,
    failures: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheRecord {
    version: u8,
    key: String,
    last_modified: Option<String>,
    next_request: DateTime<Utc>,
    forecast: Forecast,
}

#[derive(Debug, Default)]
struct SearchState {
    next_request: Option<DateTime<Utc>>,
    error: Option<String>,
    entries: HashMap<String, (DateTime<Utc>, Vec<WeatherLocation>)>,
}

impl Client {
    pub fn new() -> Result<Self, String> {
        let root = match env::var_os("XDG_CACHE_HOME").filter(|value| !value.is_empty()) {
            Some(value) => PathBuf::from(value),
            None => PathBuf::from(
                env::var_os("HOME")
                    .ok_or("HOME is unavailable; cannot locate the weather cache.")?,
            )
            .join(".cache"),
        };
        if !root.is_absolute() {
            return Err("The weather cache directory must be an absolute path.".into());
        }
        Self::with_cache(root.join("cosmos-applet-datetime-plus"), http_client()?)
    }

    fn with_cache(cache_dir: PathBuf, http: reqwest::Client) -> Result<Self, String> {
        if !cache_dir.is_absolute() {
            return Err("The weather cache directory must be an absolute path.".into());
        }
        Ok(Self {
            http,
            cache_dir,
            weather: Mutex::new(WeatherState::default()),
            search: Mutex::new(SearchState::default()),
        })
    }

    /// Explicit city lookup only. This does not request weather or perform IP geolocation.
    pub async fn search(&self, query: &str) -> Result<Vec<WeatherLocation>, String> {
        let query = query.trim();
        if query.len() > 160 || query.chars().any(char::is_control) {
            return Err(
                "City search must contain at most 160 bytes without control characters.".into(),
            );
        }
        if query.chars().count() < 2 {
            return Ok(Vec::new());
        }
        // The mutex provides single-flight requests; no spawned retry loop outlives the UI.
        let mut state = self.search.lock().await;
        let now = Utc::now();
        if let Some((expires, results)) = state.entries.get(query)
            && *expires > now
        {
            return Ok(results.clone());
        }
        if let Some(next) = state.next_request.filter(|next| *next > now) {
            return Err(format!(
                "{} Try city search after {}.",
                state
                    .error
                    .as_deref()
                    .unwrap_or("City search is rate-limited."),
                next.format("%H:%M:%S UTC")
            ));
        }
        state.next_request = Some(now + TimeDelta::seconds(1));
        let response = match self
            .http
            .get(SEARCH_URL)
            .query(&[
                ("name", query),
                ("count", "10"),
                ("language", "en"),
                ("format", "json"),
            ])
            .header(ACCEPT, "application/json")
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => {
                state.next_request = Some(Utc::now() + TimeDelta::seconds(60));
                state.error = Some("City search is offline or timed out.".into());
                return Err(
                    "City search is offline or timed out. No location was selected.".into(),
                );
            }
        };
        let status = response.status();
        if status != StatusCode::OK {
            let now = Utc::now();
            state.next_request = Some(
                retry_after(response.headers(), now)
                    .unwrap_or(now)
                    .max(now + TimeDelta::seconds(60)),
            );
            let error = format!("Open-Meteo city search returned HTTP {}.", status.as_u16());
            state.error = Some(error.clone());
            return Err(error);
        }
        let results = match bounded_body(response)
            .await
            .and_then(|bytes| decode_locations(&bytes))
        {
            Ok(results) => results,
            Err(error) => {
                state.next_request = Some(Utc::now() + TimeDelta::seconds(60));
                state.error = Some(error.clone());
                return Err(error);
            }
        };
        state.error = None;
        if state.entries.len() >= MEMORY_LOCATIONS {
            state.entries.retain(|_, (expires, _)| *expires > now);
            if state.entries.len() >= MEMORY_LOCATIONS {
                state.entries.clear();
            }
        }
        state.entries.insert(
            query.to_owned(),
            (now + TimeDelta::minutes(5), results.clone()),
        );
        Ok(results)
    }

    /// Returns this location's real cached data or an explicit error, never another city's data.
    /// Opening the popup or requesting refresh cannot override the provider cache/cooldown.
    pub async fn forecast(&self, location: &WeatherLocation) -> Result<Forecast, String> {
        location.validate()?;
        let key = location_key(location);
        // This is a request transaction, deliberately held through bounded I/O for single flight.
        let mut state = self.weather.lock().await;
        if !state.initialized {
            state.ledger = load_ledger(&self.cache_dir).await?;
            state.initialized = true;
        }
        let now = Utc::now();
        // Another panel instance can finish a request after we cached a miss or stale data.
        if state
            .entries
            .get(&key)
            .and_then(Option::as_ref)
            .is_none_or(|record| record.next_request <= now)
        {
            let cached = load_record(&self.cache_dir, &key).await?;
            if state.entries.len() >= MEMORY_LOCATIONS {
                state.entries.clear();
            }
            state.entries.insert(key.clone(), cached);
        }
        let cached = state.entries.get(&key).and_then(Option::as_ref);
        if let Some(record) = cached
            && record.next_request > now
        {
            return cached_forecast(record, location, now, state.persistence_error.as_deref());
        }
        if let Some(error) = &state.persistence_error {
            return cached_or_error(cached, location, now, error);
        }
        if let Some(next) = state.ledger.next_request.filter(|next| *next > now) {
            let warning = cooldown_warning(&state.ledger, next);
            return cached_or_error(cached, location, now, &warning);
        }

        secure_directory(&self.cache_dir).await?;
        let _lock = match request_lock(&self.cache_dir).await {
            Ok(lock) => lock,
            Err(error) => return cached_or_error(cached, location, now, &error),
        };
        // Another applet process may have updated the durable gate while this one was idle.
        state.ledger = load_ledger(&self.cache_dir).await?;
        let newest = load_record(&self.cache_dir, &key).await?;
        if let Some(record) = &newest
            && record.next_request > now
        {
            let forecast = cached_forecast(record, location, now, None);
            state.entries.insert(key, newest);
            return forecast;
        }
        state.entries.insert(key.clone(), newest);
        if let Some(next) = state.ledger.next_request.filter(|next| *next > now) {
            let warning = cooldown_warning(&state.ledger, next);
            return cached_or_error(
                state.entries.get(&key).and_then(Option::as_ref),
                location,
                now,
                &warning,
            );
        }
        state.ledger.version = 1;
        state.ledger.next_request = Some(request_floor(now));
        // Persist the attempt BEFORE sending; cancellation/crash/restart must not bypass cadence.
        if let Err(error) = save_json(&self.cache_dir, "met-state.json", &state.ledger).await {
            state.persistence_error = Some(error.clone());
            return cached_or_error(
                state.entries.get(&key).and_then(Option::as_ref),
                location,
                now,
                &error,
            );
        }
        let cached = state.entries.get(&key).and_then(Option::as_ref);
        let mut request = self
            .http
            .get(FORECAST_URL)
            .query(&[
                ("lat", coordinate(location.latitude)),
                ("lon", coordinate(location.longitude)),
            ])
            .header(ACCEPT, "application/json");
        if let Some(last_modified) = conditional_header(cached, now) {
            request = request.header(IF_MODIFIED_SINCE, last_modified);
        }
        let response = request.send().await;
        let received = Utc::now();
        let mut retry = None;
        let mut server_expiry = None;
        let mut status = None;
        let outcome = match response {
            Ok(response) => {
                status = Some(response.status());
                let headers = response.headers().clone();
                retry = retry_after(&headers, received);
                server_expiry = headers
                    .get(EXPIRES)
                    .and_then(|value| value.to_str().ok())
                    .and_then(http_date);
                if response.status() == StatusCode::NOT_MODIFIED {
                    apply_response(response.status(), &headers, &[], cached, location, received)
                } else if matches!(
                    response.status(),
                    StatusCode::OK | StatusCode::NON_AUTHORITATIVE_INFORMATION
                ) {
                    let response_status = response.status();
                    match bounded_body(response).await {
                        Ok(bytes) => apply_response(
                            response_status,
                            &headers,
                            &bytes,
                            cached,
                            location,
                            received,
                        ),
                        Err(error) => Err(error),
                    }
                } else {
                    Err(provider_error(response.status()))
                }
            }
            Err(_) => Err("MET Norway is offline or the forecast request timed out.".into()),
        };
        match outcome {
            Ok(mut record) => {
                state.ledger.failures = 0;
                state.ledger.last_error = None;
                if let Some(retry) = retry {
                    record.next_request = record.next_request.max(retry);
                    state.ledger.next_request =
                        Some(state.ledger.next_request.unwrap_or(received).max(retry));
                }
                if let Err(error) =
                    save_json(&self.cache_dir, &format!("{key}.json"), &record).await
                {
                    state.persistence_error = Some(error.clone());
                    append_warning(&mut record.forecast.warning, &error);
                }
                if let Err(error) =
                    save_json(&self.cache_dir, "met-state.json", &state.ledger).await
                {
                    state.persistence_error = Some(error.clone());
                    append_warning(&mut record.forecast.warning, &error);
                }
                let forecast = cached_forecast(&record, location, received, None);
                state.entries.insert(key, Some(record));
                forecast
            }
            Err(error) => {
                state.ledger.failures = state.ledger.failures.saturating_add(1);
                state.ledger.last_error = Some(error.clone());
                let next = failure_gate(received, status, retry, state.ledger.failures)
                    .max(server_expiry.unwrap_or(received));
                state.ledger.next_request =
                    Some(state.ledger.next_request.unwrap_or(received).max(next));
                let mut warning = error;
                if let Err(error) =
                    save_json(&self.cache_dir, "met-state.json", &state.ledger).await
                {
                    state.persistence_error = Some(error.clone());
                    append_text(&mut warning, &error);
                }
                cached_or_error(
                    state.entries.get(&key).and_then(Option::as_ref),
                    location,
                    received,
                    &warning,
                )
            }
        }
    }
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .https_only(true)
        .gzip(true)
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 3
                || attempt.url().scheme() != "https"
                || !matches!(
                    attempt.url().host_str(),
                    Some("api.met.no" | "geocoding-api.open-meteo.com")
                )
            {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|error| format!("Could not initialize the weather HTTP client: {error}"))
}

async fn bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE as u64)
    {
        return Err("The weather provider response exceeds the 512 KiB limit.".into());
    }
    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or(16 * 1024)
            .min(MAX_RESPONSE as u64) as usize,
    );
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Could not finish downloading the weather provider response.")?
    {
        if chunk.len() > MAX_RESPONSE.saturating_sub(bytes.len()) {
            return Err("The weather provider response exceeds the 512 KiB limit.".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn coordinate(value: f64) -> String {
    let scaled = (value * 10_000.0).trunc() as i32;
    let sign = if scaled < 0 { "-" } else { "" };
    let absolute = scaled.unsigned_abs();
    format!("{sign}{}.{:04}", absolute / 10_000, absolute % 10_000)
}

fn location_key(location: &WeatherLocation) -> String {
    let mut zone = String::with_capacity(location.timezone.len() * 2);
    use std::fmt::Write;
    for byte in location.timezone.bytes() {
        let _ = write!(zone, "{byte:02x}");
    }
    format!(
        "met-v1_{}_{}_{}",
        coordinate(location.latitude),
        coordinate(location.longitude),
        zone
    )
}

fn request_floor(now: DateTime<Utc>) -> DateTime<Utc> {
    // User-driven requests need no timer; jitter keeps subsequent refresh gates off hour boundaries.
    let jitter = (u64::from(now.timestamp_subsec_nanos()) ^ u64::from(std::process::id())) % 121;
    now + TimeDelta::seconds(MIN_CADENCE + jitter as i64)
}

fn failure_gate(
    now: DateTime<Utc>,
    status: Option<StatusCode>,
    retry: Option<DateTime<Utc>>,
    failures: u32,
) -> DateTime<Utc> {
    let seconds = if status
        .is_some_and(|status| status.is_client_error() && status != StatusCode::TOO_MANY_REQUESTS)
    {
        24 * 60 * 60
    } else {
        (MIN_CADENCE * (1_i64 << failures.saturating_sub(1).min(5))).min(6 * 60 * 60)
    };
    retry.unwrap_or(now).max(now + TimeDelta::seconds(seconds))
}

fn http_date(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc2822(value)
        .ok()
        .map(|date| date.with_timezone(&Utc))
        .or_else(|| {
            ["%A, %d-%b-%y %H:%M:%S GMT", "%a %b %e %H:%M:%S %Y"]
                .iter()
                .find_map(|format| {
                    NaiveDateTime::parse_from_str(value, format)
                        .ok()
                        .map(|date| date.and_utc())
                })
        })
}

fn retry_after(headers: &HeaderMap, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        let seconds = i64::try_from(seconds).ok().and_then(TimeDelta::try_seconds);
        return Some(
            seconds
                .and_then(|seconds| now.checked_add_signed(seconds))
                .unwrap_or(DateTime::<Utc>::MAX_UTC),
        );
    }
    http_date(value)
}

fn conditional_header(cached: Option<&CacheRecord>, now: DateTime<Utc>) -> Option<&str> {
    let value = cached?.last_modified.as_deref()?;
    http_date(value).filter(|date| *date <= now)?;
    Some(value)
}

fn provider_error(status: StatusCode) -> String {
    match status {
        StatusCode::FORBIDDEN => "MET Norway denied this request (HTTP 403). Application identification may need a real maintainer contact or the service may have blocked access.".into(),
        StatusCode::TOO_MANY_REQUESTS => "MET Norway rate-limited this applet (HTTP 429). Requests have been suspended until the cooldown ends.".into(),
        StatusCode::UNPROCESSABLE_ENTITY => "MET Norway has no forecast coverage for these coordinates (HTTP 422).".into(),
        _ => format!("MET Norway returned HTTP {}; no new forecast was received.", status.as_u16()),
    }
}

fn cooldown_warning(ledger: &Ledger, next: DateTime<Utc>) -> String {
    format!(
        "{} Next permitted forecast request: {}.",
        ledger
            .last_error
            .as_deref()
            .unwrap_or("Forecast refresh is waiting for the provider cache/cadence gate."),
        next.format("%Y-%m-%d %H:%M UTC")
    )
}

fn append_text(target: &mut String, text: &str) {
    if target.contains(text) {
        return;
    }
    if !target.is_empty() {
        target.push(' ');
    }
    target.push_str(text);
}

fn append_warning(warning: &mut Option<String>, text: &str) {
    match warning {
        Some(warning) => append_text(warning, text),
        None => *warning = Some(text.into()),
    }
}

fn cached_forecast(
    record: &CacheRecord,
    location: &WeatherLocation,
    now: DateTime<Utc>,
    warning: Option<&str>,
) -> Result<Forecast, String> {
    if record.key != location_key(location) {
        return Err("The cached forecast belongs to a different location.".into());
    }
    let mut forecast = record.forecast.clone();
    forecast.location = location.clone();
    forecast.stale =
        forecast.valid_until <= now || forecast.hours.last().is_none_or(|hour| hour.time <= now);
    if forecast.stale {
        append_warning(
            &mut forecast.warning,
            "Cached forecast is stale; its provider expiry has passed or its forecast times have ended.",
        );
    }
    if let Some(warning) = warning {
        append_warning(&mut forecast.warning, warning);
    }
    Ok(forecast)
}

fn cached_or_error(
    cached: Option<&CacheRecord>,
    location: &WeatherLocation,
    now: DateTime<Utc>,
    warning: &str,
) -> Result<Forecast, String> {
    match cached {
        Some(record) => cached_forecast(record, location, now, Some(warning)),
        None => Err(format!(
            "{warning} No cached forecast exists for {}.",
            location.name
        )),
    }
}

fn apply_response(
    status: StatusCode,
    headers: &HeaderMap,
    bytes: &[u8],
    cached: Option<&CacheRecord>,
    location: &WeatherLocation,
    now: DateTime<Utc>,
) -> Result<CacheRecord, String> {
    let mut forecast = match status {
        StatusCode::NOT_MODIFIED => {
            let record =
                cached.ok_or("MET Norway returned HTTP 304, but no cached forecast exists.")?;
            if record.key != location_key(location)
                || conditional_header(Some(record), now).is_none()
            {
                return Err(
                    "MET Norway returned HTTP 304 without a matching conditional forecast.".into(),
                );
            }
            record.forecast.clone()
        }
        StatusCode::OK | StatusCode::NON_AUTHORITATIVE_INFORMATION => {
            decode_forecast(bytes, location, now)?
        }
        _ => return Err(provider_error(status)),
    };
    forecast.location = location.clone();
    forecast.fetched_at = now;
    forecast.stale = false;
    forecast.valid_until = headers.get(EXPIRES).and_then(|value| value.to_str().ok()).and_then(http_date).unwrap_or_else(|| {
        append_warning(&mut forecast.warning, "MET did not provide a valid Expires header; using a conservative 15-minute cache lifetime.");
        now + TimeDelta::seconds(MIN_CADENCE)
    });
    if status == StatusCode::NON_AUTHORITATIVE_INFORMATION {
        append_warning(
            &mut forecast.warning,
            "MET Norway returned HTTP 203: this product is beta or deprecated and may stop working.",
        );
    }
    let last_modified = headers
        .get(LAST_MODIFIED)
        .and_then(|value| value.to_str().ok())
        .filter(|value| http_date(value).is_some_and(|date| date <= now))
        .map(str::to_owned)
        .or_else(|| {
            if status == StatusCode::NOT_MODIFIED {
                cached.and_then(|record| record.last_modified.clone())
            } else {
                None
            }
        });
    if last_modified.is_none() {
        append_warning(
            &mut forecast.warning,
            "MET did not provide a usable Last-Modified header; conditional revalidation is unavailable.",
        );
    }
    Ok(CacheRecord {
        version: 1,
        key: location_key(location),
        last_modified,
        next_request: request_floor(now).max(forecast.valid_until),
        forecast,
    })
}

#[derive(Deserialize)]
struct MetForecast {
    geometry: MetGeometry,
    properties: MetProperties,
}
#[derive(Deserialize)]
struct MetGeometry {
    coordinates: Vec<f64>,
}
#[derive(Deserialize)]
struct MetProperties {
    meta: MetMeta,
    timeseries: Vec<MetTime>,
}
#[derive(Deserialize)]
struct MetMeta {
    updated_at: DateTime<Utc>,
    units: MetUnits,
}
#[derive(Deserialize)]
struct MetUnits {
    air_temperature: String,
    precipitation_amount: Option<String>,
    wind_speed: Option<String>,
    relative_humidity: Option<String>,
}
#[derive(Deserialize)]
struct MetTime {
    time: DateTime<Utc>,
    data: MetData,
}
#[derive(Deserialize)]
struct MetData {
    instant: Option<MetInstant>,
    next_1_hours: Option<MetPeriod>,
    next_6_hours: Option<MetPeriod>,
    next_12_hours: Option<MetPeriod>,
}
#[derive(Deserialize)]
struct MetInstant {
    details: MetInstantDetails,
}
#[derive(Deserialize)]
struct MetInstantDetails {
    air_temperature: Option<f64>,
    wind_speed: Option<f64>,
    relative_humidity: Option<f64>,
}
#[derive(Deserialize)]
struct MetPeriod {
    summary: Option<MetSummary>,
    details: Option<MetPeriodDetails>,
}
#[derive(Deserialize)]
struct MetSummary {
    symbol_code: Option<String>,
}
#[derive(Deserialize)]
struct MetPeriodDetails {
    precipitation_amount: Option<f64>,
}

fn valid_symbol(symbol: &str) -> bool {
    symbol.len() <= 80
        && symbol
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn decode_forecast(
    bytes: &[u8],
    location: &WeatherLocation,
    now: DateTime<Utc>,
) -> Result<Forecast, String> {
    let model: MetForecast = serde_json::from_slice(bytes)
        .map_err(|error| format!("MET Norway returned an invalid forecast document: {error}"))?;
    let latitude = coordinate(location.latitude)
        .parse::<f64>()
        .map_err(|_| "Invalid forecast latitude.")?;
    let longitude = coordinate(location.longitude)
        .parse::<f64>()
        .map_err(|_| "Invalid forecast longitude.")?;
    let coordinates = &model.geometry.coordinates;
    if coordinates.len() < 2
        || (coordinates[0] - longitude).abs() > 0.00011
        || (coordinates[1] - latitude).abs() > 0.00011
        || !coordinates.iter().all(|value| value.is_finite())
    {
        return Err("MET Norway returned forecast data for different coordinates.".into());
    }
    if model.properties.meta.units.air_temperature != "celsius" {
        return Err(
            "MET Norway returned unsupported temperature units; no temperature was substituted."
                .into(),
        );
    }
    if model.properties.timeseries.is_empty() || model.properties.timeseries.len() > MAX_HOURS {
        return Err("MET Norway returned an empty or oversized forecast timeseries.".into());
    }
    let rain_is_mm = model.properties.meta.units.precipitation_amount.as_deref() == Some("mm");
    let wind_is_mps = model.properties.meta.units.wind_speed.as_deref() == Some("m/s");
    let humidity_is_percent = model.properties.meta.units.relative_humidity.as_deref() == Some("%");
    let mut hours = Vec::with_capacity(model.properties.timeseries.len());
    let mut missing_temperatures = 0;
    let mut missing_symbols = 0;
    let mut previous_time = None;
    for step in model.properties.timeseries {
        if previous_time.is_some_and(|previous| step.time <= previous) {
            return Err("MET Norway returned forecast times out of chronological order.".into());
        }
        previous_time = Some(step.time);
        let Some(temperature_c) = step
            .data
            .instant
            .as_ref()
            .and_then(|instant| instant.details.air_temperature)
            .filter(|value| value.is_finite())
        else {
            missing_temperatures += 1;
            continue;
        };
        let symbol = [
            &step.data.next_1_hours,
            &step.data.next_6_hours,
            &step.data.next_12_hours,
        ]
        .iter()
        .find_map(|period| {
            period
                .as_ref()?
                .summary
                .as_ref()?
                .symbol_code
                .as_deref()
                .filter(|symbol| !symbol.is_empty() && valid_symbol(symbol))
        })
        .unwrap_or("")
        .to_owned();
        // The final instant has no following forecast period; no symbol is expected there.
        if symbol.is_empty()
            && (step.data.next_1_hours.is_some()
                || step.data.next_6_hours.is_some()
                || step.data.next_12_hours.is_some())
        {
            missing_symbols += 1;
        }
        let precipitation_mm = if rain_is_mm {
            step.data
                .next_1_hours
                .as_ref()
                .and_then(|period| period.details.as_ref())
                .and_then(|details| details.precipitation_amount)
                .filter(|value| value.is_finite() && *value >= 0.0)
        } else {
            None
        };
        let details = step.data.instant.as_ref().map(|instant| &instant.details);
        let wind_speed_mps = details
            .and_then(|details| details.wind_speed)
            .filter(|value| wind_is_mps && valid_wind(*value));
        let relative_humidity_percent = details
            .and_then(|details| details.relative_humidity)
            .filter(|value| humidity_is_percent && valid_humidity(*value));
        hours.push(ForecastHour {
            time: step.time,
            temperature_c,
            symbol,
            precipitation_mm,
            wind_speed_mps,
            relative_humidity_percent,
        });
    }
    if hours.is_empty() {
        return Err("MET Norway returned no usable temperature data; no zero-temperature forecast was created.".into());
    }
    let mut warning = None;
    if missing_temperatures > 0 {
        append_warning(
            &mut warning,
            &format!(
                "MET omitted temperatures for {missing_temperatures} forecast timestamps; those entries are unavailable."
            ),
        );
    }
    if missing_symbols > 0 {
        append_warning(
            &mut warning,
            &format!("MET omitted weather symbols for {missing_symbols} forecast timestamps."),
        );
    }
    Ok(Forecast {
        location: location.clone(),
        generated_at: model.properties.meta.updated_at,
        fetched_at: now,
        valid_until: now,
        hours,
        stale: false,
        warning,
    })
}

#[derive(Deserialize)]
struct Geocoding {
    #[serde(default)]
    results: Vec<GeocodingLocation>,
    #[serde(default)]
    error: bool,
}
#[derive(Deserialize)]
struct GeocodingLocation {
    name: Option<String>,
    latitude: Option<f64>,
    longitude: Option<f64>,
    timezone: Option<String>,
    admin1: Option<String>,
    country: Option<String>,
}

fn decode_locations(bytes: &[u8]) -> Result<Vec<WeatherLocation>, String> {
    let response: Geocoding = serde_json::from_slice(bytes)
        .map_err(|error| format!("Open-Meteo returned invalid city search results: {error}"))?;
    if response.error {
        return Err("Open-Meteo could not complete the city search.".into());
    }
    let mut locations = Vec::with_capacity(response.results.len().min(10));
    for result in response.results.into_iter().take(10) {
        let (Some(name), Some(latitude), Some(longitude), Some(timezone)) = (
            result.name,
            result.latitude,
            result.longitude,
            result.timezone,
        ) else {
            continue;
        };
        let mut parts = vec![name];
        for part in [result.admin1, result.country].into_iter().flatten() {
            if !part.trim().is_empty() && !parts.contains(&part) {
                parts.push(part);
            }
        }
        let location = WeatherLocation {
            name: parts.join(", "),
            latitude,
            longitude,
            timezone,
        };
        if location.validate().is_ok() && !locations.contains(&location) {
            locations.push(location);
        }
    }
    Ok(locations)
}

async fn secure_directory(path: &Path) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).await.map_err(|error| {
                format!("Could not create the weather cache directory: {error}")
            })?;
            fs::symlink_metadata(path).await.map_err(|error| {
                format!("Could not inspect the new weather cache directory: {error}")
            })?
        }
        Err(error) => {
            return Err(format!(
                "Could not inspect the weather cache directory: {error}"
            ));
        }
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.uid() != effective_uid()
    {
        return Err(
            "The weather cache directory must be a real directory owned by the current user."
                .into(),
        );
    }
    fs::set_permissions(path, Permissions::from_mode(0o700))
        .await
        .map_err(|error| format!("Could not secure the weather cache directory: {error}"))
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no pointer arguments and does not alter process state.
    unsafe { libc::geteuid() }
}

async fn private_bytes(path: &Path, max: usize) -> Result<Option<Vec<u8>>, String> {
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .await
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Could not open the weather cache: {error}")),
    };
    let metadata = file
        .metadata()
        .await
        .map_err(|error| format!("Could not inspect the weather cache: {error}"))?;
    if !metadata.is_file()
        || metadata.len() > max as u64
        || metadata.uid() != effective_uid()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err("Weather cache files must be private, current-user-owned regular files within the size limit.".into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((max + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| format!("Could not read the weather cache: {error}"))?;
    if bytes.len() > max {
        return Err("The weather cache exceeds its size limit.".into());
    }
    Ok(Some(bytes))
}

async fn load_ledger(directory: &Path) -> Result<Ledger, String> {
    let Some(bytes) = private_bytes(&directory.join("met-state.json"), 16 * 1024).await? else {
        return Ok(Ledger {
            version: 1,
            ..Ledger::default()
        });
    };
    let ledger: Ledger = serde_json::from_slice(&bytes).map_err(|error| {
        format!("The weather cooldown cache is invalid; requests remain disabled: {error}")
    })?;
    if ledger.version != 1 {
        return Err(
            "The weather cooldown cache version is unsupported; requests remain disabled.".into(),
        );
    }
    Ok(ledger)
}

async fn load_record(directory: &Path, key: &str) -> Result<Option<CacheRecord>, String> {
    let Some(bytes) = private_bytes(&directory.join(format!("{key}.json")), MAX_CACHE).await?
    else {
        return Ok(None);
    };
    let record: CacheRecord = serde_json::from_slice(&bytes)
        .map_err(|error| format!("The saved MET forecast is invalid: {error}"))?;
    record.forecast.location.validate()?;
    if record.version != 1
        || record.key != key
        || location_key(&record.forecast.location) != key
        || record.forecast.hours.is_empty()
        || record.forecast.hours.len() > MAX_HOURS
        || !record.forecast.hours.iter().all(|hour| {
            hour.temperature_c.is_finite()
                && valid_symbol(&hour.symbol)
                && hour
                    .precipitation_mm
                    .is_none_or(|rain| rain.is_finite() && rain >= 0.0)
                && hour.wind_speed_mps.is_none_or(valid_wind)
                && hour.relative_humidity_percent.is_none_or(valid_humidity)
        })
        || !record
            .forecast
            .hours
            .windows(2)
            .all(|hours| hours[0].time < hours[1].time)
        || record.next_request < record.forecast.valid_until
    {
        return Err(
            "The saved MET forecast has invalid data or belongs to a different location.".into(),
        );
    }
    Ok(Some(record))
}

async fn save_json(directory: &Path, name: &str, value: &impl Serialize) -> Result<(), String> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| format!("Could not encode the weather cache: {error}"))?;
    if bytes.len() > MAX_CACHE {
        return Err("The weather cache exceeds its 1 MiB limit.".into());
    }
    secure_directory(directory).await?;
    let temporary = directory.join(format!(
        ".weather-{}-{}-{}.tmp",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let result = async {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .await
            .map_err(|error| format!("Could not create a private weather cache: {error}"))?;
        file.write_all(&bytes)
            .await
            .map_err(|error| format!("Could not write the weather cache: {error}"))?;
        file.sync_all()
            .await
            .map_err(|error| format!("Could not persist the weather cache: {error}"))?;
        fs::rename(&temporary, directory.join(name))
            .await
            .map_err(|error| format!("Could not replace the weather cache: {error}"))?;
        fs::File::open(directory)
            .await
            .map_err(|error| format!("Could not open the weather cache directory: {error}"))?
            .sync_all()
            .await
            .map_err(|error| format!("Could not synchronize the weather cache directory: {error}"))
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(temporary).await;
    }
    result
}

async fn request_lock(directory: &Path) -> Result<std::fs::File, String> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join("met-request.lock"))
        .await
        .map_err(|error| format!("Could not open the weather request lock: {error}"))?;
    let metadata = file
        .metadata()
        .await
        .map_err(|error| format!("Could not inspect the weather request lock: {error}"))?;
    if !metadata.is_file()
        || metadata.uid() != effective_uid()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(
            "The weather request lock must be a private regular file owned by the current user."
                .into(),
        );
    }
    let file = file.into_std().await;
    // SAFETY: the descriptor is owned by file; LOCK_NB never blocks the async runtime.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(
            "Another applet instance is updating MET weather; use cached data and retry later."
                .into(),
        );
    }
    // Closing this owned descriptor releases the lock, including on cancellation.
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-13T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }
    fn oslo() -> WeatherLocation {
        WeatherLocation {
            name: "Oslo, Norway".into(),
            latitude: 59.9139,
            longitude: 10.7522,
            timezone: "Europe/Oslo".into(),
        }
    }
    fn headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(EXPIRES, "Tue, 13 Oct 2026 13:00:00 GMT".parse().unwrap());
        headers.insert(
            LAST_MODIFIED,
            "Tue, 13 Oct 2026 11:30:00 GMT".parse().unwrap(),
        );
        headers
    }
    fn document() -> serde_json::Value {
        serde_json::json!({"geometry":{"type":"Point","coordinates":[10.7522,59.9139,10]},"properties":{"meta":{"updated_at":"2026-10-13T11:30:00Z","units":{"air_temperature":"celsius","precipitation_amount":"mm"}},"timeseries":[{"time":"2026-10-13T12:00:00Z","data":{"instant":{"details":{"air_temperature":8.5}},"next_1_hours":{"summary":{"symbol_code":"rain"},"details":{"precipitation_amount":0.6}}}},{"time":"2026-10-13T13:00:00Z","data":{"instant":{"details":{"air_temperature":9.1}},"next_6_hours":{"summary":{"symbol_code":"partlycloudy_day"},"details":{"precipitation_amount":3.0}}}},{"time":"2026-10-13T19:00:00Z","data":{"instant":{"details":{"air_temperature":6.0}}}}]}})
    }
    fn record() -> CacheRecord {
        apply_response(
            StatusCode::OK,
            &headers(),
            &serde_json::to_vec(&document()).unwrap(),
            None,
            &oslo(),
            now(),
        )
        .unwrap()
    }

    fn measured_hour(temperature_c: f64, wind: Option<f64>, humidity: Option<f64>) -> ForecastHour {
        ForecastHour {
            time: now(),
            temperature_c,
            symbol: String::new(),
            precipitation_mm: None,
            wind_speed_mps: wind,
            relative_humidity_percent: humidity,
        }
    }

    #[test]
    fn optional_measurements_require_provider_units_and_valid_ranges() {
        for (wind_unit, humidity_unit, wind, humidity, expected) in [
            ("m/s", "%", 4.0, 75.0, (Some(4.0), Some(75.0))),
            ("km/h", "%", 4.0, 75.0, (None, Some(75.0))),
            ("m/s", "fraction", 4.0, 0.75, (Some(4.0), None)),
            ("m/s", "%", -0.1, 100.1, (None, None)),
            ("m/s", "%", 0.0, 0.0, (Some(0.0), Some(0.0))),
            ("m/s", "%", 4.0, 100.0, (Some(4.0), Some(100.0))),
            ("m/s", "%", 4.0, -0.1, (Some(4.0), None)),
        ] {
            let mut document = document();
            document["properties"]["meta"]["units"]["wind_speed"] = wind_unit.into();
            document["properties"]["meta"]["units"]["relative_humidity"] = humidity_unit.into();
            document["properties"]["timeseries"][0]["data"]["instant"]["details"]["wind_speed"] =
                wind.into();
            document["properties"]["timeseries"][0]["data"]["instant"]["details"]["relative_humidity"] =
                humidity.into();
            let forecast =
                decode_forecast(&serde_json::to_vec(&document).unwrap(), &oslo(), now()).unwrap();
            let hour = &forecast.hours[0];
            assert_eq!(
                (hour.wind_speed_mps, hour.relative_humidity_percent),
                expected,
                "{wind_unit}, {humidity_unit}, {wind}, {humidity}"
            );
            assert_eq!(
                (hour.temperature_c, hour.precipitation_mm),
                (8.5, Some(0.6))
            );
            assert_eq!(forecast.hours.len(), 3);
        }
    }

    #[test]
    fn absent_optional_measurements_and_units_stay_unavailable() {
        let mut document = document();
        document["properties"]["timeseries"][0]["data"]["instant"]["details"]["wind_speed"] =
            4.0.into();
        document["properties"]["timeseries"][0]["data"]["instant"]["details"]["relative_humidity"] =
            75.0.into();
        let forecast =
            decode_forecast(&serde_json::to_vec(&document).unwrap(), &oslo(), now()).unwrap();
        assert!(forecast.hours.iter().all(|hour| {
            hour.wind_speed_mps.is_none() && hour.relative_humidity_percent.is_none()
        }));
    }

    #[test]
    fn old_cache_without_optional_fields_loads_through_private_disk_validation() {
        runtime().block_on(async {
            let directory = Directory::new();
            let record = record();
            let mut old = serde_json::to_value(&record).unwrap();
            for hour in old["forecast"]["hours"].as_array_mut().unwrap() {
                hour.as_object_mut().unwrap().remove("wind_speed_mps");
                hour.as_object_mut()
                    .unwrap()
                    .remove("relative_humidity_percent");
            }
            save_json(&directory.0, &format!("{}.json", record.key), &old)
                .await
                .unwrap();
            let loaded = load_record(&directory.0, &record.key)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(loaded.next_request, record.next_request);
            assert_eq!(loaded.forecast.hours.len(), record.forecast.hours.len());
            assert!(loaded.forecast.hours.iter().all(|hour| {
                hour.wind_speed_mps.is_none() && hour.relative_humidity_percent.is_none()
            }));
        });
    }

    #[test]
    fn cache_rejects_out_of_range_optional_measurements() {
        runtime().block_on(async {
            let directory = Directory::new();
            for (wind, humidity) in [(Some(-1.0), None), (None, Some(-1.0)), (None, Some(101.0))] {
                let mut record = record();
                record.forecast.hours[0].wind_speed_mps = wind;
                record.forecast.hours[0].relative_humidity_percent = humidity;
                save_json(&directory.0, &format!("{}.json", record.key), &record)
                    .await
                    .unwrap();
                assert!(load_record(&directory.0, &record.key).await.is_err());
            }
        });
    }

    #[test]
    fn optional_measurements_survive_cache_round_trip() {
        runtime().block_on(async {
            let directory = Directory::new();
            let mut record = record();
            record.forecast.hours[0].wind_speed_mps = Some(0.0);
            record.forecast.hours[0].relative_humidity_percent = Some(100.0);
            save_json(&directory.0, &format!("{}.json", record.key), &record)
                .await
                .unwrap();
            let loaded = load_record(&directory.0, &record.key)
                .await
                .unwrap()
                .unwrap();
            let hour = &loaded.forecast.hours[0];
            assert_eq!(
                (hour.wind_speed_mps, hour.relative_humidity_percent),
                (Some(0.0), Some(100.0))
            );
        });
    }

    #[test]
    fn wind_chill_matches_nws_example_without_requiring_humidity() {
        let hour = measured_hour(-32.0 / 1.8, Some(15.0 * 0.44704), None);
        let chill_f = hour.apparent_temperature_c().unwrap() * 1.8 + 32.0;
        assert!((chill_f - -19.0).abs() < 1.0);
    }

    #[test]
    fn wind_chill_respects_temperature_and_strict_wind_boundary() {
        assert!(
            measured_hour(10.0, Some(3.0 * 0.44704 + 0.001), None)
                .apparent_temperature_c()
                .is_some()
        );
        for (temperature, wind) in [
            (10.001, Some(5.0)),
            (10.0, Some(3.0 * 0.44704)),
            (0.0, Some(0.0)),
            (0.0, None),
            (0.0, Some(-1.0)),
            (0.0, Some(f64::INFINITY)),
            (f64::NAN, Some(5.0)),
        ] {
            assert!(
                measured_hour(temperature, wind, Some(75.0))
                    .apparent_temperature_c()
                    .is_none()
            );
        }
    }

    #[test]
    fn heat_index_matches_nws_regression_without_requiring_wind() {
        let hour = measured_hour((90.0 - 32.0) / 1.8, None, Some(70.0));
        let index_f = hour.apparent_temperature_c().unwrap() * 1.8 + 32.0;
        assert!((index_f - 105.922).abs() < 0.001);
    }

    #[test]
    fn heat_index_respects_input_domain_and_preliminary_gate() {
        for (temperature_f, humidity) in [
            (79.99, Some(100.0)),
            (80.0, Some(0.0)),
            (112.01, Some(50.0)),
            (90.0, None),
            (90.0, Some(-1.0)),
            (90.0, Some(101.0)),
            (90.0, Some(f64::NAN)),
            (90.0, Some(f64::INFINITY)),
        ] {
            assert!(
                measured_hour((temperature_f - 32.0) / 1.8, None, humidity)
                    .apparent_temperature_c()
                    .is_none()
            );
        }
        for (temperature_f, humidity) in [(80.0, 100.0), (112.0, 50.0)] {
            assert!(
                measured_hour((temperature_f - 32.0) / 1.8, None, Some(humidity))
                    .apparent_temperature_c()
                    .is_some()
            );
        }
    }

    #[test]
    fn heat_index_applies_low_and_high_humidity_adjustments() {
        // Independent published NWS polynomial plus adjustment values, in Fahrenheit.
        for (temperature_f, humidity, expected_f) in [(100.0, 10.0, 94.122), (85.0, 90.0, 101.780)]
        {
            let hour = measured_hour((temperature_f - 32.0) / 1.8, None, Some(humidity));
            let index_f = hour.apparent_temperature_c().unwrap() * 1.8 + 32.0;
            assert!(
                (index_f - expected_f).abs() < 0.01,
                "{index_f} != {expected_f}"
            );
        }
    }

    #[test]
    fn descriptions_handle_met_variants_and_unknown_conditions_honestly() {
        for (symbol, expected) in [
            ("clearsky_night", "Clear sky"),
            ("partlycloudy_polartwilight", "Partly cloudy"),
            (
                "heavyrainshowersandthunder_day",
                "Heavy rain showers with thunder",
            ),
            (
                "lightssleetshowersandthunder",
                "Light sleet showers with thunder",
            ),
            (
                "lightssnowshowersandthunder",
                "Light snow showers with thunder",
            ),
            ("", "Conditions unavailable"),
            ("unknown", "Unknown conditions"),
            ("clearsky_unknown", "Unknown conditions"),
        ] {
            assert_eq!(description(symbol), expected);
        }
    }

    #[test]
    fn forecast_preserves_provider_timestamps_temperatures_and_symbols() {
        let forecast = record().forecast;
        assert_eq!(
            (
                forecast.generated_at,
                forecast.hours[0].time,
                forecast.hours[0].temperature_c,
                forecast.hours[0].symbol.as_str(),
                forecast.hours[0].precipitation_mm
            ),
            (
                now() - TimeDelta::minutes(30),
                now(),
                8.5,
                "rain",
                Some(0.6)
            )
        );
    }

    #[test]
    fn six_hour_rain_is_not_reported_as_hourly_precipitation() {
        let hour = record().forecast.hours.remove(1);
        assert_eq!(
            (hour.symbol.as_str(), hour.precipitation_mm),
            ("partlycloudy_day", None)
        );
    }

    #[test]
    fn absent_temperature_is_omitted_not_zero() {
        let mut document = document();
        document["properties"]["timeseries"][0]["data"]["instant"]["details"] =
            serde_json::json!({});
        let forecast =
            decode_forecast(&serde_json::to_vec(&document).unwrap(), &oslo(), now()).unwrap();
        assert_eq!(
            (forecast.hours.len(), forecast.hours[0].temperature_c),
            (2, 9.1)
        );
    }

    #[test]
    fn entirely_missing_temperatures_return_error() {
        let mut document = document();
        for step in document["properties"]["timeseries"].as_array_mut().unwrap() {
            step["data"]["instant"]["details"] = serde_json::json!({});
        }
        assert!(decode_forecast(&serde_json::to_vec(&document).unwrap(), &oslo(), now()).is_err());
    }

    #[test]
    fn wrong_location_provider_data_is_rejected() {
        let mut document = document();
        document["geometry"]["coordinates"][0] = serde_json::json!(-74.0);
        assert!(decode_forecast(&serde_json::to_vec(&document).unwrap(), &oslo(), now()).is_err());
    }

    #[test]
    fn missing_final_period_has_no_invented_symbol() {
        let forecast = record().forecast;
        assert!(forecast.hours[2].symbol.is_empty());
        assert!(forecast.warning.is_none());
    }

    #[test]
    fn provided_forecast_period_without_conditions_remains_a_warning() {
        let mut document = document();
        document["properties"]["timeseries"][0]["data"]["next_1_hours"] =
            serde_json::json!({"details": {"precipitation_amount": 0.6}});
        let forecast =
            decode_forecast(&serde_json::to_vec(&document).unwrap(), &oslo(), now()).unwrap();
        assert!(forecast.warning.is_some());
        assert!(forecast.hours[0].symbol.is_empty());
        assert_eq!(forecast.hours[0].temperature_c, 8.5);
    }

    #[test]
    fn coordinates_are_truncated_to_at_most_four_decimals() {
        assert_eq!(
            (
                coordinate(59.9139876),
                coordinate(-10.7522987),
                coordinate(-0.00001)
            ),
            (
                "59.9139".to_owned(),
                "-10.7522".to_owned(),
                "0.0000".to_owned()
            )
        );
    }

    #[test]
    fn provider_expiry_and_minimum_cadence_both_gate_refresh() {
        let record = record();
        let mut past = headers();
        past.insert(EXPIRES, "Tue, 13 Oct 2026 11:00:00 GMT".parse().unwrap());
        let expired = apply_response(
            StatusCode::OK,
            &past,
            &serde_json::to_vec(&document()).unwrap(),
            None,
            &oslo(),
            now(),
        )
        .unwrap();
        assert!(
            record.next_request >= now() + TimeDelta::hours(1)
                && expired.next_request >= now() + TimeDelta::minutes(15)
        );
    }

    #[test]
    fn conditional_request_preserves_last_modified_header_verbatim() {
        let record = record();
        assert_eq!(
            conditional_header(Some(&record), now()),
            Some("Tue, 13 Oct 2026 11:30:00 GMT")
        );
    }

    #[test]
    fn not_modified_reuses_data_and_extends_expiry() {
        let record = record();
        let mut next_headers = headers();
        next_headers.insert(EXPIRES, "Tue, 13 Oct 2026 14:00:00 GMT".parse().unwrap());
        let next = apply_response(
            StatusCode::NOT_MODIFIED,
            &next_headers,
            b"not a body",
            Some(&record),
            &oslo(),
            now() + TimeDelta::hours(1),
        )
        .unwrap();
        assert_eq!(
            (
                next.forecast.hours[0].temperature_c,
                next.forecast.generated_at,
                next.forecast.fetched_at,
                next.forecast.valid_until
            ),
            (
                8.5,
                record.forecast.generated_at,
                now() + TimeDelta::hours(1),
                now() + TimeDelta::hours(2)
            )
        );
    }

    #[test]
    fn not_modified_without_cache_cannot_create_forecast() {
        assert!(
            apply_response(
                StatusCode::NOT_MODIFIED,
                &headers(),
                b"",
                None,
                &oslo(),
                now()
            )
            .is_err()
        );
    }

    #[test]
    fn not_modified_cannot_reuse_another_city_cache() {
        let mut cached = record();
        cached.key = "different-location".into();
        assert!(
            apply_response(
                StatusCode::NOT_MODIFIED,
                &headers(),
                b"",
                Some(&cached),
                &oslo(),
                now()
            )
            .is_err()
        );
    }

    #[test]
    fn future_last_modified_is_never_sent_as_conditional_timestamp() {
        let mut cached = record();
        cached.last_modified = Some("Tue, 13 Oct 2026 15:00:00 GMT".into());
        assert!(conditional_header(Some(&cached), now()).is_none());
    }

    #[test]
    fn provider_error_response_never_becomes_zero_temperature() {
        assert!(
            apply_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &headers(),
                b"{}",
                None,
                &oslo(),
                now()
            )
            .is_err()
        );
    }

    #[test]
    fn deprecated_response_remains_visible_as_warning() {
        let record = apply_response(
            StatusCode::NON_AUTHORITATIVE_INFORMATION,
            &headers(),
            &serde_json::to_vec(&document()).unwrap(),
            None,
            &oslo(),
            now(),
        )
        .unwrap();
        assert!(record.forecast.warning.unwrap().contains("HTTP 203"));
    }

    #[test]
    fn offline_expired_forecast_is_explicitly_stale() {
        let record = record();
        let forecast = cached_or_error(
            Some(&record),
            &oslo(),
            now() + TimeDelta::hours(2),
            "Offline.",
        )
        .unwrap();
        assert!(forecast.stale && forecast.warning.unwrap().contains("Offline."));
    }

    #[test]
    fn retry_after_seconds_and_date_are_respected() {
        let mut seconds = HeaderMap::new();
        seconds.insert(RETRY_AFTER, "7200".parse().unwrap());
        let mut date = HeaderMap::new();
        date.insert(
            RETRY_AFTER,
            "Tue, 13 Oct 2026 15:00:00 GMT".parse().unwrap(),
        );
        assert_eq!(
            (
                failure_gate(
                    now(),
                    Some(StatusCode::TOO_MANY_REQUESTS),
                    retry_after(&seconds, now()),
                    1
                ),
                failure_gate(
                    now(),
                    Some(StatusCode::SERVICE_UNAVAILABLE),
                    retry_after(&date, now()),
                    1
                )
            ),
            (now() + TimeDelta::hours(2), now() + TimeDelta::hours(3))
        );
    }

    #[test]
    fn repeated_failures_back_off_without_rapid_retry() {
        assert_eq!(
            (
                failure_gate(now(), None, None, 1),
                failure_gate(now(), None, None, 2),
                failure_gate(now(), None, None, 100)
            ),
            (
                now() + TimeDelta::minutes(15),
                now() + TimeDelta::minutes(30),
                now() + TimeDelta::hours(6)
            )
        );
    }

    #[test]
    fn forbidden_identification_error_uses_long_cooldown() {
        assert!(
            failure_gate(now(), Some(StatusCode::FORBIDDEN), None, 1)
                >= now() + TimeDelta::hours(24)
        );
    }

    #[test]
    fn geocoding_only_returns_complete_valid_locations() {
        let results = decode_locations(br#"{"results":[{"name":"Oslo","latitude":59.9139,"longitude":10.7522,"timezone":"Europe/Oslo","country":"Norway"},{"name":"Missing timezone","latitude":0,"longitude":0},{"name":"Invalid timezone","latitude":0,"longitude":0,"timezone":"Not/AZone"}]}"#).unwrap();
        assert_eq!(results, vec![oslo()]);
    }

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = env::temp_dir().join(format!(
                "datetime-weather-test-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            std::fs::set_permissions(&path, Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }
    fn offline_client(directory: &Path) -> Client {
        let http = reqwest::Client::builder()
            .https_only(true)
            .no_proxy()
            .resolve("api.met.no", "127.0.0.1:9".parse().unwrap())
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        Client::with_cache(directory.to_owned(), http).unwrap()
    }

    #[test]
    fn cache_survives_client_restart_and_repeated_open_without_network() {
        runtime().block_on(async {
            let directory = Directory::new();
            let mut record = record();
            record.forecast.valid_until = Utc::now() + TimeDelta::hours(1);
            record.next_request = record.forecast.valid_until;
            let key = record.key.clone();
            save_json(&directory.0, &format!("{key}.json"), &record)
                .await
                .unwrap();
            let ledger = Ledger {
                version: 1,
                next_request: Some(record.next_request),
                ..Ledger::default()
            };
            save_json(&directory.0, "met-state.json", &ledger)
                .await
                .unwrap();
            let first = offline_client(&directory.0);
            let one = first.forecast(&oslo()).await.unwrap();
            let two = first.forecast(&oslo()).await.unwrap();
            drop(first);
            let restarted = offline_client(&directory.0)
                .forecast(&oslo())
                .await
                .unwrap();
            assert_eq!(
                (one.fetched_at, two.fetched_at, restarted.fetched_at),
                (
                    record.forecast.fetched_at,
                    record.forecast.fetched_at,
                    record.forecast.fetched_at
                )
            );
        });
    }

    #[test]
    fn waiting_instance_reads_forecast_completed_by_another_instance() {
        runtime().block_on(async {
            let directory = Directory::new();
            let mut completed = record();
            completed.forecast.valid_until = Utc::now() + TimeDelta::hours(1);
            completed.next_request = completed.forecast.valid_until;
            save_json(
                &directory.0,
                "met-state.json",
                &Ledger {
                    version: 1,
                    next_request: Some(completed.next_request),
                    ..Ledger::default()
                },
            )
            .await
            .unwrap();
            let waiting = offline_client(&directory.0);
            assert!(waiting.forecast(&oslo()).await.is_err());
            save_json(&directory.0, &format!("{}.json", completed.key), &completed)
                .await
                .unwrap();
            let available = waiting.forecast(&oslo()).await.unwrap();
            assert_eq!(available.fetched_at, completed.forecast.fetched_at);
            assert_eq!(available.hours[0].temperature_c, 8.5);
        });
    }

    #[test]
    fn cooldown_for_new_city_never_leaks_previous_city_data() {
        runtime().block_on(async {
            let directory = Directory::new();
            let record = record();
            save_json(&directory.0, &format!("{}.json", record.key), &record)
                .await
                .unwrap();
            save_json(
                &directory.0,
                "met-state.json",
                &Ledger {
                    version: 1,
                    next_request: Some(Utc::now() + TimeDelta::hours(1)),
                    ..Ledger::default()
                },
            )
            .await
            .unwrap();
            let berlin = WeatherLocation {
                name: "Berlin".into(),
                latitude: 52.52,
                longitude: 13.405,
                timezone: "Europe/Berlin".into(),
            };
            assert!(
                offline_client(&directory.0)
                    .forecast(&berlin)
                    .await
                    .is_err()
            );
        });
    }

    #[test]
    fn cache_identity_mismatch_is_rejected_on_disk() {
        runtime().block_on(async {
            let directory = Directory::new();
            let mut record = record();
            let key = record.key.clone();
            record.forecast.location.longitude = -74.0;
            save_json(&directory.0, &format!("{key}.json"), &record)
                .await
                .unwrap();
            assert!(load_record(&directory.0, &key).await.is_err());
        });
    }

    #[test]
    fn durable_cooldown_and_cache_are_private_files() {
        runtime().block_on(async {
            let directory = Directory::new();
            save_json(
                &directory.0,
                "met-state.json",
                &Ledger {
                    version: 1,
                    ..Ledger::default()
                },
            )
            .await
            .unwrap();
            let metadata = fs::metadata(directory.0.join("met-state.json"))
                .await
                .unwrap();
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        });
    }

    #[test]
    fn native_request_lock_prevents_parallel_process_transactions() {
        runtime().block_on(async {
            let directory = Directory::new();
            let _lock = request_lock(&directory.0).await.unwrap();
            assert!(request_lock(&directory.0).await.is_err());
        });
    }

    #[test]
    fn invalid_timezone_is_rejected_before_cache_or_network() {
        runtime().block_on(async {
            let directory = Directory::new();
            let mut location = oslo();
            location.timezone = "Not/AZone".into();
            assert!(
                offline_client(&directory.0)
                    .forecast(&location)
                    .await
                    .is_err()
            );
        });
    }
}
