# COSMIC Datetime Plus

A standalone COSMIC panel clock with a GNOME-style, two-column dropdown. It does not rebuild COSMIC or replace its notification daemon.

- **Panel:** content-sized weekday, day, month, time and the native COSMIC chat-bubble icon; an unread dot, no hover tooltip.
- **Default left column:** currently playing MPRIS media cards with previous/play-pause/next controls; clickable notification cards, supplied secondary actions, individual dismissal, Clear, and COSMIC's shared Do Not Disturb setting. Paused/stopped players and the empty media section are hidden.
- **Default right column:** a navigable month calendar and local agenda, MET Norway weather, and world clocks.
- **Settings:** section order and left/right placement, individual clock order, geographic world-clock search, 12/24-hour time, optional seconds, Monday/Sunday week start, ISO week numbers, weather city, and Celsius/Fahrenheit.

The dropdown uses an opaque theme-colored background and grows up to **75% of the current monitor's logical height**. It follows the panel's named Wayland output, including scaled logical dimensions, rather than an unsupported native window-monitor query that previously left the height capped at 710 pixels. Each column scrolls independently only when its content exceeds that limit. Sections use a compact 9-pixel gap, including below the calendar actions. The notification footer contains **Settings**, **Clear**, then **Do Not Disturb**. Settings and the task editor use a bounded-height view with scrollable controls and visible action buttons.

## Calendar, tasks and reminders

Click a calendar date, then **Add task** beside **Today**. Enter a title, local date/time and reminder minutes: **0** means at the chosen time; **15** means fifteen minutes before it. Dates containing tasks have a dot. Entries can be edited or deleted; tasks can be marked completed to suppress their reminder. Deletion requires confirmation. Previously saved events remain editable and keep their reminders; the calendar no longer offers a separate Add event button.

Times are resolved in the system timezone when saved and stored as UTC instants. Nonexistent daylight-saving times are rejected; repeated times offer explicit first/second occurrence controls. Changing a title does not repeat an already-delivered reminder; changing its time or advance interval rearms it.

Reminders open a **persistent, opaque native reminder window while this applet is running**, even with its dropdown closed. Use **Finished**, **Snooze**, or **Delete** to resolve it. Outside clicks, Escape and ordinary window-close requests do not dismiss it; it can be minimized. Snooze moves the reminder deadline, not the original scheduled time. Finished completes a task; for a legacy event it resolves the reminder without deleting the event.

They do not wake a sleeping machine or run after logout. Due reminders and unresolved windows are restored after startup/resume. A native realtime deadline timer handles wall-clock changes, and filesystem notifications track agenda edits without a GUI refresh loop. Concurrent instances coordinate through locks and a private per-entry socket, so restarting the applet reuses an existing reminder window.

Delivery is persisted only after the native window acknowledges opening. Unresolved reminders survive restart; resolved history is not reopened. Window/process loss is retried with a 30-second backoff. A save failure remains visible and keeps the reminder open rather than silently treating it as resolved. No duplicate desktop-notification toast is sent.

The local agenda supports up to 512 entries, 160-character titles and reminders up to 365 days before an event. It does not synchronize accounts or provide recurring events. Unreadable or unsupported agenda files are preserved; the UI reports failures without discarding typed drafts or overwriting concurrent edits.

## Layout and world clocks

In Settings, use **Left/Right** and **Up/Down** for Media, Notifications, Calendar and agenda, Weather, and World clocks. Up/Down moves a section within its selected column. Each configured clock also has Up/Down and Remove controls. **Save** applies the draft; **Cancel** leaves saved preferences unchanged. Existing settings gain the default section layout without losing saved clocks or weather settings.

World-clock search is offline and includes cities, administrative regions, countries, aliases and country codes. Searches such as **Dallas**, **Austin**, or **Texas** return geographic locations, not just timezone identifiers. Different cities sharing a timezone can be added independently. Regions spanning timezones return actual city choices; Texas is not treated as a single timezone. Raw IANA timezone search and custom labels remain available.

The bundled [GeoNames cities5000](https://download.geonames.org/export/dump/) snapshot contains **69,801 cities across 245 countries/territories** (2026-10-13), covering cities above 5,000 population or administrative seats according to GeoNames. It is not every settlement; try a nearby city or a raw IANA timezone for missing places. Some aliases are historical, and administrative codes are not universally ISO state abbreviations. World clocks handle daylight saving time and relative dates.

Each clock shows its current timezone abbreviation and signed difference from the local timezone, including fractional hours (for example `CDT/CEST−7` or `+0545/CEST+3:45`). Relative dates remain visible across the date line. There are no seasonal icons or world-clock hover popups; timezone abbreviations and offsets still follow daylight-saving changes.

## Build and install

Requires a COSMIC session, a current Rust toolchain, `just`, `desktop-file-utils`, and the native Wayland/font/graphics development dependencies required by [libcosmic](https://github.com/pop-os/libcosmic). Tested on Fedora 44 with Rust 1.98.1 and COSMIC 1.10.0.

```sh
just build-release
just install
```

The recipes install the executable, desktop entry, symbolic icon and geographic-data attribution under `~/.local`. The desktop entry uses an absolute executable path, so the panel does not need `~/.local/bin` in its `PATH`. `CARGO_TARGET_DIR` is respected by both build and install.

In COSMIC Settings → Desktop → Panel → Applets, add **Date & Notifications**. It can coexist with the stock clock and Notifications Center. Those stock applets remain installed; disable their panel entries yourself if you want only the combined dropdown. No daemon replacement, desktop rebuild, or login credentials are needed.

For development, `just run` starts the applet, but **notification reception requires panel launch**: the installed desktop entry includes `X-NotificationsApplet=true`, and the panel supplies a private `COSMIC_NOTIFICATIONS` socket.

## Notifications and media

The applet uses COSMIC's private notification-applet protocol for incoming notifications/actions, and the standard session D-Bus notification interface for dismissal. Do Not Disturb updates only the existing `com.system76.CosmicNotifications` version-1 `do_not_disturb` key. Changes from other COSMIC controls remain synchronized.

COSMIC does not replay existing applet history to a new connection. This applet collects incoming notifications while running, retaining at most 100 entries in memory; it does not write their contents to disk. Expired, non-transient notifications can remain as history, but their actions are disabled once the daemon closes them. Transient notifications are not retained after closure. Replacing an existing notification ID replaces its previous card and action bindings.

Clicking a notification card invokes the sender's **default action**, when one is supplied. Secondary-action and dismissal buttons remain independent and do not also activate the card. The panel dot clears when the notification overview is actually drawn, not when settings or a reminder window opens.

Media players are discovered through MPRIS name/property signals, not a rapid polling loop. Controls respect each player's current capabilities. Only bounded local-file artwork is loaded; remote artwork URLs are not fetched. A player that disappears cannot silently redirect a pending control to a replacement process.

## Weather and privacy

Weather comes exclusively from [MET Norway Locationforecast](https://api.met.no/weatherapi/locationforecast/2.0/documentation), the meteorological service behind Yr. This is not an official Yr application and does not use Yr branding.

1. Open Settings and explicitly search for a city.
2. Select a result and save.
3. The weather card displays up to five forecast timestamps in that location's timezone.
4. Click the card to open the companion **Weather** application with the same city and temperature unit. Its Refresh button remains separate.

The companion lives in the sibling `cosmic-weather` repository. Keep the two repositories side by side when building Weather, then run its `just` and `just install` recipes. Weather reuses this repository's public `weather` library; both installed executables run independently. Existing Weather windows receive the requested location/unit instead of opening duplicates. Weather's own preferences do not silently rewrite applet settings.

There is no automatic or IP-based location detection. Explicit searches send the typed city query and your connection's IP address to [Open-Meteo geocoding](https://open-meteo.com/en/docs/geocoding-api), whose location database comes from [GeoNames](https://www.geonames.org/). Forecast requests send the selected coordinates and connection IP address to MET Norway over HTTPS. Coordinates are truncated to four decimal places. No accounts, authentication tokens, or telemetry are involved.

Forecast requests use a durable cache and a minimum **15-minute** network cadence across applet and Weather instances and locations, plus any later server `Expires` time or retry deadline. Opening either application or pressing Refresh cannot bypass those gates. The client uses exact `Last-Modified`/`If-Modified-Since` validators, handles HTTP 304, and respects rate-limit/backoff responses. Changing to a previously uncached city during a cooldown may therefore require waiting. Cached data stays visible with an explicit stale/error indication when appropriate; missing weather data is never substituted with zeroes.

Forecast data: **MET Norway, [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/)**. The app selects forecast timestamps, maps condition symbols to desktop icons, rounds displayed temperatures, and optionally converts Celsius to Fahrenheit. Provider/license links and modification descriptions are in **Settings**, not the overview. Weather geocoding attribution: **Open-Meteo / GeoNames**.

Offline world-clock data: **[GeoNames](https://www.geonames.org/), CC BY 4.0**. Search aliases are normalized, shared metadata is compacted, and long or duplicate display labels are adapted. Source URLs, download hashes, coverage and regeneration instructions are in `resources/places-source.txt` and `resources/build-places.py`. No world-clock search query leaves the machine. Attribution is also installed at `~/.local/share/doc/cosmic-applet-datetime-plus/places-source.txt`.

The genuine User-Agent is `cosmic-applet-datetime-plus/0.1.0`. Before distributing this app broadly, add a real maintainer contact or published project URL to `USER_AGENT` in `src/weather.rs`, as recommended by [MET's terms](https://api.met.no/doc/TermsOfService). No fictional contact or project URL is supplied.

## Files and behavior

- Preferences: `$XDG_CONFIG_HOME/cosmic-applet-datetime-plus/settings.json`, normally `~/.config/cosmic-applet-datetime-plus/settings.json`.
- Local events/tasks and delivery state: `$XDG_CONFIG_HOME/cosmic-applet-datetime-plus/agenda.json`; sibling `agenda.lock` coordinates instances.
- Persistent reminder windows: sibling private `reminder-<id>.sock` and `reminder-<id>.window-lock` files. The same executable's internal `--task-notifier <id>` mode opens a normal desktop window without inheriting the panel's private Wayland/notification sockets.
- Weather cache and durable request gate: `$XDG_CACHE_HOME/cosmic-applet-datetime-plus/`, normally `~/.cache/cosmic-applet-datetime-plus/`.
- Applet desktop ID: `io.github.cosmic-applet-datetime-plus`.

Settings, agenda and cache files are private to the user. Saves are validated and atomic; unreadable, malformed, or unsupported data is not silently overwritten. Preferences load at startup; use Settings, or restart the applet after manual preference edits. Agenda changes are watched while running. Back up `settings.json` and `agenda.json` together to retain preferences and scheduled entries.

The panel clock updates on wall-clock minute boundaries by default, or second boundaries when seconds are enabled. Clock ticks do not trigger weather requests or reconstruct search state. Notification and media updates are event-driven.

## Verification and removal

```sh
just check
```

The behavioral suite covers calendar/year/leap/DST boundaries, saved-preference failures and transactions, layout/order persistence, geographic search, agenda CRUD/concurrency/deadline/delivery recovery, stale asynchronous results, notification lifecycles and image bounds, MPRIS capability/ownership changes, and weather parsing/cache/cooldown behavior. Tests use isolated temporary files and do not contact real weather providers or modify the user's notification/media state.

Remove this applet's panel entry before running `just uninstall`. Uninstallation removes this applet's executable, desktop/icon registration and installed geographic attribution, not the stock applets. Preferences, agenda and cache are retained.

## License

Application code is GPL-3.0-only. Adapted System76 COSMIC applet code retains its copyright and license notices; see `LICENSE` and the source headers. Bundled GeoNames data is separately licensed CC BY 4.0, as described above. Native integration references: [COSMIC applets](https://github.com/pop-os/cosmic-applets) and [COSMIC notifications](https://github.com/pop-os/cosmic-notifications).
