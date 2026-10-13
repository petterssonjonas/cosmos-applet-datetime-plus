// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only
// Subscription and local-artwork handling are adapted from cosmic-applet-audio.

use std::{
    collections::HashMap, fmt::Display, future::Future, path::PathBuf, sync::Arc, time::Duration,
};

use cosmic::iced::{Subscription, futures::channel::mpsc, stream};
use futures_util::{SinkExt, StreamExt, stream::FuturesUnordered};
use tokio::{sync::Semaphore, time::timeout};
use zbus::{
    Connection, MatchRule, MessageStream, fdo::DBusProxy, message::Sequence, names::BusName,
    zvariant::OwnedValue,
};

const PREFIX: &str = "org.mpris.MediaPlayer2.";
const ROOT_INTERFACE: &str = "org.mpris.MediaPlayer2";
const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
const CALL_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_ARTWORK_BYTES: u64 = 8 * 1024 * 1024;
type Properties = HashMap<String, OwnedValue>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Player {
    pub bus_name: String,
    pub identity: String,
    pub title: String,
    pub artist: String,
    pub artwork: Option<PathBuf>,
    pub playing: bool,
    pub can_play: bool,
    pub can_pause: bool,
    pub can_next: bool,
    pub can_previous: bool,
    pub can_raise: bool,
}

#[derive(Clone, Debug)]
pub enum Event {
    Players(Vec<Player>),
    Unavailable(String),
}

#[derive(Clone, Copy, Debug)]
pub enum Action {
    PlayPause,
    Next,
    Previous,
    Raise,
}

/// Monitors MPRIS name ownership and properties; healthy sessions never poll.
pub fn subscription() -> Subscription<Event> {
    Subscription::run(|| {
        stream::channel(16, |mut output| async move {
            let mut reconnect_delay = Duration::from_secs(5);
            loop {
                match monitor(&mut output, &mut reconnect_delay).await {
                    Ok(()) => return,
                    Err(error) => {
                        if output.send(Event::Players(Vec::new())).await.is_err()
                            || output.send(Event::Unavailable(error)).await.is_err()
                        {
                            return;
                        }
                        // Only reconnect a failed bus session, never periodically query players.
                        tokio::time::sleep(reconnect_delay).await;
                        reconnect_delay = (reconnect_delay * 2).min(Duration::from_secs(60));
                    }
                }
            }
        })
    })
}

async fn bounded<T, E: Display>(
    operation: &str,
    future: impl Future<Output = Result<T, E>>,
) -> Result<T, String> {
    timeout(CALL_TIMEOUT, future)
        .await
        .map_err(|_| format!("{operation} timed out"))?
        .map_err(|error| format!("{operation}: {error}"))
}

fn valid_player_name(name: &str) -> bool {
    name.starts_with(PREFIX)
        && name.len() > PREFIX.len()
        && matches!(BusName::try_from(name), Ok(BusName::WellKnown(_)))
}

async fn owner(connection: &Connection, name: &str) -> Result<Option<String>, String> {
    let name = BusName::try_from(name).map_err(|_| "Invalid MPRIS player name".to_owned())?;
    let bus = bounded("Connecting to the session bus", DBusProxy::new(connection)).await?;
    match timeout(CALL_TIMEOUT, bus.get_name_owner(name)).await {
        Ok(Ok(owner)) => Ok(Some(owner.to_string())),
        Ok(Err(zbus::fdo::Error::NameHasNoOwner(_))) => Ok(None),
        Ok(Err(error)) => Err(format!("Discovering the media player: {error}")),
        Err(_) => Err("Discovering the media player timed out".to_owned()),
    }
}

async fn properties(
    connection: &Connection,
    destination: &str,
    interface: &'static str,
) -> Result<(Properties, Sequence), String> {
    let reply = bounded(
        "Reading media properties",
        connection.call_method(
            Some(destination),
            OBJECT_PATH,
            Some("org.freedesktop.DBus.Properties"),
            "GetAll",
            &(interface,),
        ),
    )
    .await?;
    let values = reply
        .body()
        .deserialize::<Properties>()
        .map_err(|error| format!("Invalid media properties: {error}"))?;
    Ok((values, reply.recv_position()))
}

fn take_string(values: &mut Properties, key: &str) -> String {
    values
        .remove(key)
        .and_then(|value| String::try_from(value).ok())
        .unwrap_or_default()
}

fn take_bool(values: &mut Properties, key: &str) -> bool {
    values
        .remove(key)
        .and_then(|value| bool::try_from(value).ok())
        .unwrap_or(false)
}

fn playback_status(values: &Properties) -> Result<bool, String> {
    match values
        .get("PlaybackStatus")
        .and_then(|value| <&str>::try_from(value).ok())
    {
        Some("Playing") => Ok(true),
        Some("Paused" | "Stopped") => Ok(false),
        _ => Err("Media player returned an invalid playback status".to_owned()),
    }
}

fn local_artwork(url: &str) -> Option<PathBuf> {
    let url = url::Url::parse(url).ok()?;
    if url.scheme() != "file"
        || !matches!(url.host_str(), None | Some("") | Some("localhost"))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let path = url.to_file_path().ok()?;
    path.is_absolute().then_some(path)
}

fn parse_player(
    name: String,
    mut root: Properties,
    mut playback: Properties,
) -> Result<Player, String> {
    let playing = playback_status(&playback)?;
    let can_control = take_bool(&mut playback, "CanControl");
    let mut metadata = playback
        .remove("Metadata")
        .and_then(|value| Properties::try_from(value).ok())
        .unwrap_or_default();
    let identity = take_string(&mut root, "Identity");
    Ok(Player {
        identity: if identity.is_empty() {
            name.clone()
        } else {
            identity
        },
        bus_name: name,
        title: take_string(&mut metadata, "xesam:title"),
        artist: metadata
            .remove("xesam:artist")
            .and_then(|value| Vec::<String>::try_from(value).ok())
            .unwrap_or_default()
            .join(", "),
        artwork: local_artwork(&take_string(&mut metadata, "mpris:artUrl")),
        playing,
        can_play: can_control && take_bool(&mut playback, "CanPlay"),
        can_pause: can_control && take_bool(&mut playback, "CanPause"),
        can_next: can_control && take_bool(&mut playback, "CanGoNext"),
        can_previous: can_control && take_bool(&mut playback, "CanGoPrevious"),
        can_raise: take_bool(&mut root, "CanRaise"),
    })
}

struct Reading {
    player: Player,
    root_position: Sequence,
    player_position: Sequence,
}

async fn read_player(
    connection: &Connection,
    name: &str,
    destination: &str,
) -> Result<Reading, String> {
    // Pin both reads to the unique owner, not a well-known name that can restart mid-read.
    let ((root, root_position), (playback, player_position)) = tokio::try_join!(
        properties(connection, destination, ROOT_INTERFACE),
        properties(connection, destination, PLAYER_INTERFACE),
    )?;
    let mut player = parse_player(name.to_owned(), root, playback)?;
    if let Some(path) = &player.artwork {
        let usable = matches!(
            timeout(CALL_TIMEOUT, tokio::fs::metadata(path)).await,
            Ok(Ok(metadata)) if metadata.is_file() && metadata.len() > 0 && metadata.len() <= MAX_ARTWORK_BYTES
        );
        if !usable {
            player.artwork = None;
        }
    }
    Ok(Reading {
        player,
        root_position,
        player_position,
    })
}

#[derive(Default)]
struct Record {
    owner: Option<String>,
    player: Option<Player>,
    root_position: Sequence,
    player_position: Sequence,
    pending: bool,
    dirty: bool,
    error: Option<String>,
}

impl Record {
    fn needs_refresh(&self, sender: &str, interface: &str, position: Sequence) -> bool {
        if self.owner.as_deref() != Some(sender) {
            // Signals may arrive after the first GetAll reply but before its job completes.
            return self.pending && self.owner.is_none();
        }
        let snapshot_position = if interface == ROOT_INTERFACE {
            self.root_position
        } else {
            self.player_position
        };
        position > snapshot_position
    }
}

async fn refresh(
    connection: Connection,
    name: String,
    slots: Arc<Semaphore>,
) -> (String, Option<String>, Result<Option<Reading>, String>) {
    let mut discovered_owner = None;
    let result = async {
        let _permit = slots
            .acquire()
            .await
            .map_err(|_| "Media discovery stopped".to_owned())?;
        discovered_owner = owner(&connection, &name).await?;
        match discovered_owner.as_deref() {
            Some(destination) => read_player(&connection, &name, destination).await.map(Some),
            None => Ok(None),
        }
    }
    .await;
    (name, discovered_owner, result)
}

fn relevant_property(interface: &str, name: &str) -> bool {
    match interface {
        ROOT_INTERFACE => matches!(name, "Identity" | "CanRaise"),
        PLAYER_INTERFACE => matches!(
            name,
            "Metadata"
                | "PlaybackStatus"
                | "CanControl"
                | "CanPlay"
                | "CanPause"
                | "CanGoNext"
                | "CanGoPrevious"
        ),
        _ => false,
    }
}

fn sort_players(players: &mut [Player]) {
    players.sort_by(|a, b| {
        b.playing
            .cmp(&a.playing)
            .then_with(|| a.identity.cmp(&b.identity))
            .then_with(|| a.bus_name.cmp(&b.bus_name))
    });
}

async fn publish(output: &mut mpsc::Sender<Event>, records: &HashMap<String, Record>) -> bool {
    let mut players: Vec<_> = records
        .values()
        .filter_map(|record| record.player.clone())
        .collect();
    sort_players(&mut players);
    if output.send(Event::Players(players)).await.is_err() {
        return false;
    }
    let mut errors: Vec<_> = records
        .iter()
        .filter(|(_, record)| record.error.is_some())
        .collect();
    errors.sort_by(|a, b| a.0.cmp(b.0));
    if let Some((name, record)) = errors.first() {
        let error = format!(
            "Cannot read {name}: {}{}",
            record.error.as_deref().unwrap_or_default(),
            if errors.len() > 1 {
                format!(" ({} players unavailable)", errors.len())
            } else {
                String::new()
            }
        );
        if output.send(Event::Unavailable(error)).await.is_err() {
            return false;
        }
    }
    true
}

async fn monitor(
    output: &mut mpsc::Sender<Event>,
    reconnect_delay: &mut Duration,
) -> Result<(), String> {
    let connection = bounded("Connecting media to the session bus", Connection::session()).await?;
    let names_rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.freedesktop.DBus")
        .map_err(|error| error.to_string())?
        .path("/org/freedesktop/DBus")
        .map_err(|error| error.to_string())?
        .interface("org.freedesktop.DBus")
        .map_err(|error| error.to_string())?
        .member("NameOwnerChanged")
        .map_err(|error| error.to_string())?
        .arg0ns(ROOT_INTERFACE)
        .map_err(|error| error.to_string())?
        .build();
    let properties_rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .path(OBJECT_PATH)
        .map_err(|error| error.to_string())?
        .interface("org.freedesktop.DBus.Properties")
        .map_err(|error| error.to_string())?
        .member("PropertiesChanged")
        .map_err(|error| error.to_string())?
        .arg0ns(ROOT_INTERFACE)
        .map_err(|error| error.to_string())?
        .build();
    // Register both listeners before ListNames/GetAll so startup cannot lose a player.
    let mut names = bounded(
        "Watching media player ownership",
        MessageStream::for_match_rule(names_rule, &connection, Some(512)),
    )
    .await?;
    let mut changes = bounded(
        "Watching media properties",
        MessageStream::for_match_rule(properties_rule, &connection, Some(512)),
    )
    .await?;
    let bus = bounded("Connecting media discovery", DBusProxy::new(&connection)).await?;
    let initial_names = bounded("Listing media players", bus.list_names()).await?;
    *reconnect_delay = Duration::from_secs(5);

    let slots = Arc::new(Semaphore::new(8));
    let mut jobs = FuturesUnordered::new();
    let mut records = HashMap::<String, Record>::new();
    for name in initial_names {
        if valid_player_name(name.as_str()) {
            let name = name.to_string();
            records.insert(
                name.clone(),
                Record {
                    pending: true,
                    ..Record::default()
                },
            );
            jobs.push(refresh(connection.clone(), name, Arc::clone(&slots)));
        }
    }
    if !publish(output, &records).await {
        return Ok(());
    }

    loop {
        let mut changed = false;
        tokio::select! {
            message = names.next() => {
                let message = message.ok_or_else(|| "Media session bus disconnected".to_owned())?
                    .map_err(|error| format!("Media session bus disconnected: {error}"))?;
                // MessageStream cannot locally authenticate a well-known sender by itself.
                if message.header().sender().map(|name| name.as_str()) != Some("org.freedesktop.DBus") {
                    continue;
                }
                let (name, old_owner, new_owner) = message.body().deserialize::<(String, String, String)>()
                    .map_err(|error| format!("Invalid media ownership event: {error}"))?;
                if !valid_player_name(&name) || old_owner == new_owner {
                    continue;
                }
                let record = records.entry(name.clone()).or_default();
                if record.owner.as_deref() == Some(old_owner.as_str()) {
                    let removed_player = record.player.take().is_some();
                    let removed_error = record.error.take().is_some();
                    changed = removed_player || removed_error;
                    record.owner = None;
                }
                if record.pending {
                    record.dirty = true;
                } else {
                    record.pending = true;
                    jobs.push(refresh(connection.clone(), name, Arc::clone(&slots)));
                }
            }
            message = changes.next() => {
                let message = message.ok_or_else(|| "Media property stream disconnected".to_owned())?
                    .map_err(|error| format!("Media property stream disconnected: {error}"))?;
                let header = message.header();
                let Some(sender) = header.sender().map(|sender| sender.as_str()) else { continue; };
                let Ok((interface, values, invalidated)) = message.body().deserialize::<(String, Properties, Vec<String>)>() else { continue; };
                if !values.keys().chain(invalidated.iter()).any(|name| relevant_property(&interface, name)) {
                    continue;
                }
                for (name, record) in &mut records {
                    if record.needs_refresh(sender, &interface, message.recv_position()) {
                        if record.pending {
                            record.dirty = true;
                        } else {
                            record.pending = true;
                            jobs.push(refresh(connection.clone(), name.clone(), Arc::clone(&slots)));
                        }
                    }
                }
            }
            completed = jobs.next(), if !jobs.is_empty() => {
                let Some((name, discovered_owner, result)) = completed else { continue; };
                let Some(record) = records.get_mut(&name) else { continue; };
                if record.dirty {
                    record.dirty = false;
                    jobs.push(refresh(connection.clone(), name, Arc::clone(&slots)));
                    continue;
                }
                record.pending = false;
                if discovered_owner.is_some() {
                    // Keep failed players discoverable when they later emit properties.
                    record.owner = discovered_owner;
                }
                match result {
                    Ok(Some(reading)) => {
                        changed = record.player.as_ref() != Some(&reading.player) || record.error.is_some();
                        record.root_position = reading.root_position;
                        record.player_position = reading.player_position;
                        record.player = Some(reading.player);
                        record.error = None;
                    }
                    Ok(None) => {
                        changed = record.player.is_some() || record.error.is_some();
                        records.remove(&name);
                    }
                    Err(error) => {
                        changed = record.player.take().is_some() || record.error.as_ref() != Some(&error);
                        record.error = Some(error);
                    }
                }
            }
        }
        if changed && !publish(output, &records).await {
            return Ok(());
        }
    }
}

/// Sends only standard MPRIS methods to a freshly resolved, capability-checked owner.
pub async fn control(bus_name: String, action: Action) -> Result<(), String> {
    if !valid_player_name(&bus_name) {
        return Err("Invalid MPRIS player name".to_owned());
    }
    timeout(Duration::from_secs(15), async {
        let connection = bounded("Connecting media control", Connection::session()).await?;
        let destination = owner(&connection, &bus_name)
            .await?
            .ok_or_else(|| "Media player is no longer available".to_owned())?;
        let interface = if matches!(action, Action::Raise) {
            ROOT_INTERFACE
        } else {
            PLAYER_INTERFACE
        };
        let (values, _) = properties(&connection, &destination, interface).await?;
        let method = permitted_method(&values, action)?;
        if owner(&connection, &bus_name).await?.as_deref() != Some(destination.as_str()) {
            return Err("Media player changed while preparing control; try again".to_owned());
        }
        // Unique destinations cannot auto-start or accidentally target a replacement player.
        let reply = bounded(
            "Controlling media player",
            connection.call_method(
                Some(destination.as_str()),
                OBJECT_PATH,
                Some(interface),
                method,
                &(),
            ),
        )
        .await?;
        reply
            .body()
            .deserialize::<()>()
            .map_err(|error| format!("Invalid media control reply: {error}"))
    })
    .await
    .map_err(|_| "Media control timed out".to_owned())?
}

fn permitted_method(values: &Properties, action: Action) -> Result<&'static str, String> {
    let capability = |key: &str| {
        values
            .get(key)
            .and_then(|value| bool::try_from(value).ok())
            .unwrap_or(false)
    };
    let (method, permitted) = match action {
        Action::Raise => ("Raise", capability("CanRaise")),
        Action::PlayPause => {
            let playing = playback_status(values)?;
            (
                "PlayPause",
                capability("CanControl")
                    && capability(if playing { "CanPause" } else { "CanPlay" }),
            )
        }
        Action::Next => ("Next", capability("CanControl") && capability("CanGoNext")),
        Action::Previous => (
            "Previous",
            capability("CanControl") && capability("CanGoPrevious"),
        ),
    };
    if permitted {
        Ok(method)
    } else {
        Err(format!("Media player does not currently support {method}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{Str, Value};

    fn text(value: &str) -> OwnedValue {
        OwnedValue::from(Str::from(value))
    }

    fn playback(playing: bool, controllable: bool) -> Properties {
        HashMap::from([
            (
                "PlaybackStatus".to_owned(),
                text(if playing { "Playing" } else { "Paused" }),
            ),
            ("CanControl".to_owned(), OwnedValue::from(controllable)),
            ("CanPlay".to_owned(), OwnedValue::from(true)),
            ("CanPause".to_owned(), OwnedValue::from(false)),
            ("CanGoNext".to_owned(), OwnedValue::from(true)),
            ("CanGoPrevious".to_owned(), OwnedValue::from(true)),
        ])
    }

    #[test]
    fn destinations_must_be_mpris_well_known_names() {
        for name in [
            ":1.42",
            "org.freedesktop.DBus",
            "org.mpris.MediaPlayer2",
            "org.mpris.MediaPlayer2.",
            "org.mpris.MediaPlayer2.bad/name",
        ] {
            assert!(!valid_player_name(name), "accepted {name}");
        }
        assert!(valid_player_name("org.mpris.MediaPlayer2.vlc.instance123"));
    }

    #[test]
    fn artwork_accepts_only_local_file_urls_and_decodes_escaped_paths() {
        assert_eq!(
            local_artwork("file:///tmp/cover%20art.png"),
            Some(PathBuf::from("/tmp/cover art.png"))
        );
        for url in [
            "https://example.com/cover.png",
            "file://remote/tmp/cover.png",
            "file:///tmp/cover.png?x=1",
            "file:///tmp/cover.png#fragment",
            "/tmp/cover.png",
        ] {
            assert!(local_artwork(url).is_none(), "accepted {url}");
        }
    }

    #[test]
    fn metadata_and_capabilities_are_parsed_without_inventing_missing_fields() {
        let mut values = playback(false, true);
        let artists = OwnedValue::try_from(Value::from(vec!["First", "Second"])).unwrap();
        values.insert(
            "Metadata".to_owned(),
            OwnedValue::from(HashMap::from([
                ("xesam:title".to_owned(), text("A track")),
                ("xesam:artist".to_owned(), artists),
                (
                    "mpris:artUrl".to_owned(),
                    text("https://example.com/no-download.png"),
                ),
            ])),
        );
        let player = parse_player(
            "org.mpris.MediaPlayer2.example".to_owned(),
            HashMap::new(),
            values,
        )
        .unwrap();
        assert_eq!(
            (
                player.title.as_str(),
                player.artist.as_str(),
                player.artwork,
                player.can_play,
                player.can_pause
            ),
            ("A track", "First, Second", None, true, false)
        );
    }

    #[test]
    fn can_control_disables_all_playback_controls_but_not_raise() {
        let root = HashMap::from([("CanRaise".to_owned(), OwnedValue::from(true))]);
        let player = parse_player(
            "org.mpris.MediaPlayer2.example".to_owned(),
            root,
            playback(false, false),
        )
        .unwrap();
        assert_eq!(
            (
                player.can_play,
                player.can_pause,
                player.can_next,
                player.can_previous,
                player.can_raise
            ),
            (false, false, false, false, true)
        );
    }

    #[test]
    fn play_pause_rechecks_the_capability_for_the_current_playback_status() {
        assert!(permitted_method(&playback(true, true), Action::PlayPause).is_err());
        assert_eq!(
            permitted_method(&playback(false, true), Action::PlayPause).unwrap(),
            "PlayPause"
        );
    }

    #[test]
    fn malformed_capabilities_and_playback_status_cannot_enable_controls() {
        let mut values = playback(false, true);
        values.insert("CanGoNext".to_owned(), text("true"));
        assert!(permitted_method(&values, Action::Next).is_err());
        values.insert("PlaybackStatus".to_owned(), text("Buffering"));
        assert!(permitted_method(&values, Action::PlayPause).is_err());
    }

    #[test]
    fn property_events_from_a_replaced_owner_do_not_refresh_the_current_player() {
        let record = Record {
            owner: Some(":1.200".to_owned()),
            ..Record::default()
        };
        assert!(!record.needs_refresh(":1.100", PLAYER_INTERFACE, Sequence::default()));
    }

    #[test]
    fn first_discovery_keeps_property_changes_that_arrive_before_completion() {
        let record = Record {
            pending: true,
            ..Record::default()
        };
        assert!(record.needs_refresh(":1.200", PLAYER_INTERFACE, Sequence::default()));
    }

    #[test]
    fn seek_position_changes_do_not_trigger_refreshes() {
        assert!(!relevant_property(PLAYER_INTERFACE, "Position"));
    }

    #[test]
    fn playing_players_sort_first_with_stable_identity_and_bus_name_ties() {
        let name = |suffix: &str| format!("org.mpris.MediaPlayer2.{suffix}");
        let mut players = vec![
            parse_player(name("b"), HashMap::new(), playback(false, true)).unwrap(),
            parse_player(name("z"), HashMap::new(), playback(true, true)).unwrap(),
            parse_player(name("a"), HashMap::new(), playback(false, true)).unwrap(),
        ];
        sort_players(&mut players);
        assert_eq!(
            players
                .iter()
                .map(|player| player.bus_name.as_str())
                .collect::<Vec<_>>(),
            [
                "org.mpris.MediaPlayer2.z",
                "org.mpris.MediaPlayer2.a",
                "org.mpris.MediaPlayer2.b"
            ]
        );
    }
}
