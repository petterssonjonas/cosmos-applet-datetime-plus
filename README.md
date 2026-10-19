# Cosmos Date & Time

A COSMIC panel clock with notifications, media controls, a calendar, local tasks, weather and world clocks. Settings control the two-column layout, clock format, notification preview length and temperature unit. The dropdown grows to 75% of the output's logical height. This does not replace the COSMIC notification daemon.

## Requirements

Runtime: a COSMIC Wayland desktop and session D-Bus. Notifications require launch through the COSMIC panel. Weather and city lookup need HTTPS access. World-clock search works offline. No Python process is required at runtime.

Build/install: current stable Rust from [rustup](https://rustup.rs), `just`, Bash, GNU coreutils/findutils, Git and native development packages. Distribution Rust can be too old. These package examples do not mean all distributions were tested. See [libcosmic dependencies](https://github.com/pop-os/libcosmic#dependencies).

Fedora:

```sh
sudo dnf install git just gcc-c++ cmake pkgconf-pkg-config desktop-file-utils \
  expat-devel freetype-devel fontconfig-devel wayland-devel libxkbcommon-devel \
  libxkbcommon-x11-devel libinput-devel mesa-libEGL-devel libX11-devel \
  libXcursor-devel libXi-devel libXrandr-devel dbus-devel openssl-devel
```

Debian 13 or newer:

```sh
sudo apt update
sudo apt install git just build-essential cmake pkgconf desktop-file-utils \
  libexpat1-dev libfreetype-dev libfontconfig-dev libwayland-dev libxkbcommon-dev \
  libxkbcommon-x11-dev libinput-dev libegl1-mesa-dev libx11-dev libxcursor-dev \
  libxi-dev libxrandr-dev libdbus-1-dev libssl-dev
```

Arch Linux:

```sh
sudo pacman -S --needed git just base-devel cmake pkgconf desktop-file-utils \
  expat freetype2 fontconfig wayland libxkbcommon libxkbcommon-x11 libinput \
  mesa libglvnd libx11 libxcursor libxi libxrandr dbus openssl
```

These are build/install packages, not extra applet runtime programs. Rust tests use `just test`; `just check` also needs the rustup Clippy component.

## Build and install

Install rustup from its linked site first. Run as your desktop user:

```sh
git clone https://github.com/petterssonjonas/cosmos-applet-datetime-plus
cd cosmos-applet-datetime-plus
rustup toolchain install stable
rustup override set stable
just build-release
just install
```

Use `sudo` only for system package prerequisites, never for `just build-release` or `just install`. Files go to `~/.local/bin` and `~/.local/share`. Use your mapped desktop user in build containers, not root.

In **COSMIC Settings → Desktop → Panel → Applets**, add **Cosmos Date & Time**. Reopen Settings or log in again if necessary. It can coexist with the stock clock and notification applet. Remove their panel entries yourself if desired. Installation never edits panel configuration.

## Notifications, media and reminders

Panel launch supplies the private `COSMIC_NOTIFICATIONS` socket. A terminal launch with `just run` cannot receive panel notifications. The applet supports default and secondary notification actions, dismissal, Clear and COSMIC's shared Do Not Disturb setting. It does not replace the daemon or change other notification settings.

COSMIC does not replay old notification history to a new connection. The applet keeps at most 100 notifications in memory and never writes their contents to disk. Closed actions are disabled. Transient notifications disappear after closure. The preview limit changes applet text only, not the original notification. MPRIS media controls show active players; remote artwork is not fetched.

Select a calendar date and **Add task**. Set a local date, time and reminder interval. Zero minutes means the selected time. Reminders use persistent desktop windows. Complete or delete a task to suppress its reminder. Reminders do not wake the computer or run after logout. Due and unresolved reminders return after startup or resume. Delivery is saved only after a reminder window opens; unresolved reminders survive restart.

The local agenda holds up to 512 entries. It has no account synchronization or recurring tasks. Restart the applet and reopen reminder windows after an upgrade to use the new executable.

## Weather, data and privacy

Choose a city explicitly in Settings. There is no automatic location detection. City searches send the query and connection IP address to [Open-Meteo / GeoNames](https://open-meteo.com/en/docs/geocoding-api). Forecast requests send selected coordinates, truncated to four decimal places, and connection IP address to [MET Norway](https://api.met.no/weatherapi/locationforecast/2.0/documentation). No accounts, keys or telemetry are used.

The applet and companion Weather application share a durable cache and a minimum 15-minute request interval across locations and processes. Later server expiry and retry limits also apply. Refresh cannot bypass these limits; an uncached city can require a wait. Missing values remain unavailable, and stale data is identified.

Click the weather card to open [Cosmos Weather](https://github.com/petterssonjonas/cosmos-weather) with the selected city and unit. Weather is installed separately and has separate preferences. Its README gives the required adjacent clone layout.

User files, with standard XDG defaults:

- Preferences and agenda: `~/.config/cosmos-applet-datetime-plus/settings.json` and `agenda.json`. Back up both together.
- Shared forecast cache and provider request state: `~/.cache/cosmos-applet-datetime-plus/`.

Files are private to your user. Invalid or unsupported saved files are not silently overwritten. World-clock search stays on the computer.

## Migration and removal

Stop the old `cosmic-applet-datetime-plus`, old Weather and all reminder windows. Remove the old **Date & Notifications** panel entry. Keep both generations stopped during migration: their locks use separate names.

Run `just migrate` here to copy old settings, agenda and shared cache to the `cosmos-applet-datetime-plus` namespaces. Sources remain backups. Existing destinations, unsafe paths and non-regular files are refused. Close reminder windows cleanly; remove only confirmed stale sockets after all old processes stop. Directory copies are individually atomic, not one combined transaction. Resolve any conflict before retrying.

If Weather also needs migration, run `just migrate false` in `cosmos-weather` afterward. If Weather migrated the cache first, use `just migrate false` here instead. There is no automatic migration or legacy executable alias. Add the new panel entry after installation. Use the old version's uninstaller, if available, for obsolete registrations.

To remove this version, remove its panel entry and run `just uninstall`. Preferences, agenda and cache remain.

## Credits and license

Independent project; not an official System76 or Yr application. Code: GPL-3.0-only; see `LICENSE` and preserved System76 source notices. Forecast data: MET Norway, [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/); displayed samples, icons, rounding and units are adapted. City lookup: Open-Meteo / GeoNames. Offline world-clock data: [GeoNames](https://www.geonames.org/), CC BY 4.0; labels and search aliases are adapted. Source and regeneration details remain in `resources/places-source.txt` and `resources/build-places.py`. Geographic attribution installs under `~/.local/share/doc/cosmos-applet-datetime-plus/`. Provider details also appear in Settings.
