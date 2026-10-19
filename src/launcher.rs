// SPDX-License-Identifier: GPL-3.0-only

use std::{env, ffi::OsStr, os::unix::ffi::OsStrExt, path::PathBuf, process::Stdio};
use tokio::process::Command;

use crate::config::TemperatureUnit;
use cosmos_applet_datetime_plus::weather::WeatherLocation;

/// Start a desktop application without the panel's private Wayland/notification sockets.
pub fn desktop_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    let sockets = [
        "WAYLAND_SOCKET",
        "COSMIC_NOTIFICATIONS",
        "X_PRIVILEGED_WAYLAND_SOCKET",
    ];
    let descriptors = sockets.map(|name| {
        command.env_remove(name);
        env::var(name)
            .ok()
            .and_then(|value| value.parse::<libc::c_int>().ok())
            .filter(|&descriptor| descriptor >= 3)
    });
    for (name, _) in env::vars_os() {
        if name.as_bytes().starts_with(b"COSMIC_PANEL_") {
            command.env_remove(name);
        }
    }
    // SAFETY: after fork this closure only calls async-signal-safe close with
    // captured integers. It never changes ownership of the parent's descriptors.
    unsafe {
        command.pre_exec(move || {
            for descriptor in descriptors.into_iter().flatten() {
                libc::close(descriptor);
            }
            Ok(())
        });
    }
    command
}

/// Hand a fixed URI to the desktop opener without tying browser lifetime to the applet.
pub async fn open_uri(uri: &'static str) -> Result<(), String> {
    let mut child = desktop_command("xdg-open")
        .arg(uri)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("Could not start the browser: {error}"))?;
    // Some openers wait for a newly launched browser to exit. Reap asynchronously;
    // dropping a popup or exceeding a UI timeout must never kill that browser.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

/// Open the native companion with the location and units selected in the applet.
pub async fn open_weather(
    location: Option<WeatherLocation>,
    unit: TemperatureUnit,
) -> Result<(), String> {
    let location = location
        .map(|location| {
            location.validate()?;
            serde_json::to_string(&location)
                .map_err(|error| format!("Could not pass the weather location: {error}"))
        })
        .transpose()?;
    let local =
        env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/bin/cosmos-weather"));
    let mut command = match local {
        Some(path) if path.is_file() => desktop_command(path),
        _ => desktop_command("cosmos-weather"),
    };
    if let Some(location) = location {
        command.arg("--location-json").arg(location);
    }
    command.arg("--unit").arg(match unit {
        TemperatureUnit::Celsius => "celsius",
        TemperatureUnit::Fahrenheit => "fahrenheit",
    });
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|error| {
            format!("Could not open Cosmos Weather: {error}. Install cosmos-weather and try again.")
        })?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}
