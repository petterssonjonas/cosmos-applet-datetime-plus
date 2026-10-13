// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only
// Native protocol adapted from cosmic-applet-notifications and cosmic-notifications.

mod decoder;

use std::{
    collections::VecDeque,
    fmt,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use cosmic::{
    cosmic_config::{
        self, Config, ConfigSet, CosmicConfigEntry, cosmic_config_derive::CosmicConfigEntry,
    },
    iced::{
        Subscription,
        futures::{self, FutureExt},
        stream,
    },
    widget::image::Handle,
};
use futures_util::{SinkExt, StreamExt};
use zbus::{Connection, MatchRule, MessageStream, Proxy, connection::Builder};

const CONFIG_ID: &str = "com.system76.CosmicNotifications";
const APPLET_PATH: &str = "/com/system76/NotificationsApplet";
const APPLET_INTERFACE: &str = "com.system76.NotificationsApplet";
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_LIVE_NOTIFICATIONS: usize = 100;

#[derive(Clone, Debug)]
pub struct NotificationAction {
    pub key: String,
    pub label: String,
}

#[derive(Clone)]
pub struct Notification {
    pub id: u32,
    pub app_name: String,
    pub app_icon: String,
    pub summary: String,
    pub body: String,
    pub received_at: DateTime<Utc>,
    pub actions: Vec<NotificationAction>,
    pub transient: bool,
    pub image: Option<Handle>,
}

// Application message debugging must not expose private notification content.
impl fmt::Debug for Notification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Notification")
            .field("id", &self.id)
            .field("received_at", &self.received_at)
            .field("action_count", &self.actions.len())
            .field("transient", &self.transient)
            .field("has_image", &self.image.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub enum Event {
    Connected(Arc<Client>),
    Received(Notification),
    Closed { id: u32, reason: u32 },
    DndChanged(bool),
    DndUnavailable(String),
    Unavailable(String),
}

pub struct Client {
    peer: Connection,
    public: Proxy<'static>,
    connected: AtomicBool,
    live: Mutex<LiveNotifications>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("connected", &self.connected.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl Client {
    pub async fn dismiss(&self, id: u32) -> Result<(), String> {
        self.require_live(id, None)?;
        let result: zbus::Result<()> = self.public.call("CloseNotification", &id).await;
        result.map_err(|_| "COSMIC could not dismiss this notification".to_owned())
    }

    pub async fn invoke(&self, id: u32, action: String) -> Result<(), String> {
        self.require_live(id, Some(&action))?;
        self.peer
            .call_method(
                None::<&str>,
                APPLET_PATH,
                Some(APPLET_INTERFACE),
                "InvokeAction",
                &(id, action),
            )
            .await
            .map(|_| ())
            .map_err(|_| "COSMIC could not invoke this notification action".to_owned())
    }

    fn require_live(&self, id: u32, action: Option<&str>) -> Result<(), String> {
        if !self.connected.load(Ordering::Acquire) {
            return Err("COSMIC notifications are disconnected".to_owned());
        }
        let live = self
            .live
            .lock()
            .map_err(|_| "Notification state is unavailable".to_owned())?;
        match live
            .0
            .iter()
            .find(|(notification_id, _)| *notification_id == id)
        {
            None => Err("This notification is no longer active".to_owned()),
            Some((_, actions))
                if action.is_some_and(|key| !actions.iter().any(|value| value == key)) =>
            {
                Err("This action was not supplied by the notification".to_owned())
            }
            Some(_) => Ok(()),
        }
    }

    fn disconnect(&self) {
        self.connected.store(false, Ordering::Release);
        if let Ok(mut live) = self.live.lock() {
            live.0.clear();
        }
    }
}

#[derive(Default)]
struct LiveNotifications(VecDeque<(u32, Vec<String>)>);

impl LiveNotifications {
    fn replace(&mut self, id: u32, actions: &[NotificationAction]) {
        self.close(id);
        if self.0.len() == MAX_LIVE_NOTIFICATIONS {
            self.0.pop_front();
        }
        self.0.push_back((
            id,
            actions.iter().map(|action| action.key.clone()).collect(),
        ));
    }

    fn close(&mut self, id: u32) {
        self.0.retain(|(notification_id, _)| *notification_id != id);
    }
}

// Only this existing COSMIC key is read or written; unrelated daemon preferences stay intact.
#[derive(Clone, Debug, Default, PartialEq, CosmicConfigEntry)]
#[version = 1]
struct DndPreference {
    do_not_disturb: bool,
}

pub fn subscription(core: &cosmic::Core) -> Subscription<Event> {
    Subscription::batch([
        Subscription::run(notification_stream),
        core.watch_config::<DndPreference>(CONFIG_ID).map(|update| {
            if update.errors.is_empty() {
                Event::DndChanged(update.config.do_not_disturb)
            } else {
                Event::DndUnavailable("Could not read COSMIC Do Not Disturb preference".to_owned())
            }
        }),
    ])
}

pub async fn set_do_not_disturb(enabled: bool) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        let config = Config::new(CONFIG_ID, DndPreference::VERSION)
            .map_err(|error| format!("Could not open COSMIC notification preferences: {error}"))?;
        config
            .set("do_not_disturb", enabled)
            .map_err(|error| format!("Could not save Do Not Disturb: {error}"))
    })
    .await
    .map_err(|_| "Could not save COSMIC notification preferences".to_owned())?
}

fn notification_stream() -> impl futures::Stream<Item = Event> {
    stream::channel(
        16,
        |mut output: futures::channel::mpsc::Sender<Event>| async move {
            let result = tokio::time::timeout(CALL_TIMEOUT, connect()).await;
            let (client, mut incoming, mut closed) = match result {
                Ok(Ok(value)) => value,
                Ok(Err(error)) => {
                    let _ = output.send(Event::Unavailable(error)).await;
                    futures::future::pending::<()>().await;
                    return;
                }
                Err(_) => {
                    let _ = output
                        .send(Event::Unavailable(
                            "COSMIC notification connection timed out".to_owned(),
                        ))
                        .await;
                    futures::future::pending::<()>().await;
                    return;
                }
            };
            if output
                .send(Event::Connected(Arc::clone(&client)))
                .await
                .is_err()
            {
                client.disconnect();
                return;
            }
            let unavailable = loop {
                let next_notification = incoming.next().fuse();
                let next_closed = closed.next().fuse();
                futures::pin_mut!(next_notification, next_closed);
                futures::select! {
                    message = next_notification => {
                        let message = match message {
                            Some(Ok(message)) => message,
                            Some(Err(_)) | None => break "COSMIC notification peer disconnected",
                        };
                        let notification = match decoder::notification(&message) {
                            Ok(notification) => notification,
                            Err(_) => continue,
                        };
                        match client.live.lock() {
                            Ok(mut live) => live.replace(notification.id, &notification.actions),
                            Err(_) => break "Notification state is unavailable",
                        }
                        if output.send(Event::Received(notification)).await.is_err() {
                            client.disconnect();
                            return;
                        }
                    }
                    message = next_closed => {
                        let Some(message) = message else {
                            break "COSMIC notification session bus disconnected";
                        };
                        let Ok((id, reason)) = message.body().deserialize::<(u32, u32)>() else {
                            continue;
                        };
                        // Even expired messages retained as UI history may no longer invoke actions.
                        match client.live.lock() {
                            Ok(mut live) => live.close(id),
                            Err(_) => break "Notification state is unavailable",
                        }
                        if output.send(Event::Closed { id, reason }).await.is_err() {
                            client.disconnect();
                            return;
                        }
                    }
                }
            };
            client.disconnect();
            let _ = output
                .send(Event::Unavailable(unavailable.to_owned()))
                .await;
            drop(incoming);
            drop(closed);
            drop(client);
            // A closed inherited peer cannot be reconnected. Keep the clock/calendar subscription alive.
            futures::future::pending::<()>().await;
        },
    )
}

async fn connect() -> Result<
    (
        Arc<Client>,
        MessageStream,
        zbus::proxy::SignalStream<'static>,
    ),
    String,
> {
    let descriptor = std::env::var("COSMIC_NOTIFICATIONS")
        .map_err(|_| {
            "Notifications unavailable: launch this applet from the COSMIC panel".to_owned()
        })?
        .parse::<RawFd>()
        .map_err(|_| "COSMIC supplied an invalid notification socket descriptor".to_owned())?;
    let socket = duplicate_socket(descriptor)?;
    socket
        .set_nonblocking(true)
        .map_err(|_| "Could not configure COSMIC notification socket".to_owned())?;
    let socket = tokio::net::UnixStream::from_std(socket)
        .map_err(|_| "Could not open COSMIC notification socket".to_owned())?;
    let peer = Builder::socket(socket)
        .p2p()
        .max_queued(16)
        .method_timeout(CALL_TIMEOUT)
        .build()
        .await
        .map_err(|_| "Could not connect to COSMIC notifications".to_owned())?;
    // This rule applies only to our private panel-supplied socket, never to arbitrary bus traffic.
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .path(APPLET_PATH)
        .map_err(|_| "Invalid COSMIC notification path".to_owned())?
        .interface(APPLET_INTERFACE)
        .map_err(|_| "Invalid COSMIC notification interface".to_owned())?
        .member("Notify")
        .map_err(|_| "Invalid COSMIC notification signal".to_owned())?
        .build();
    let incoming = MessageStream::for_match_rule(rule, &peer, Some(16))
        .await
        .map_err(|_| "Could not subscribe to COSMIC notification peer".to_owned())?;
    let session = Builder::session()
        .map_err(|_| "Session bus is unavailable".to_owned())?
        .max_queued(16)
        .method_timeout(CALL_TIMEOUT)
        .build()
        .await
        .map_err(|_| "Could not connect to notification session bus".to_owned())?;
    let public = Proxy::new(
        &session,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .await
    .map_err(|_| "Could not open public notification interface".to_owned())?;
    let closed = public
        .receive_signal("NotificationClosed")
        .await
        .map_err(|_| "Could not subscribe to notification closure signals".to_owned())?;
    Ok((
        Arc::new(Client {
            peer,
            public,
            connected: AtomicBool::new(true),
            live: Mutex::new(LiveNotifications::default()),
        }),
        incoming,
        closed,
    ))
}

fn duplicate_socket(descriptor: RawFd) -> Result<std::os::unix::net::UnixStream, String> {
    if descriptor < 0 {
        return Err("COSMIC supplied an invalid notification socket descriptor".to_owned());
    }
    // SAFETY: fcntl validates the raw descriptor; only its new descriptor is owned below.
    let duplicate = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        return Err("COSMIC notification socket descriptor is closed or invalid".to_owned());
    }
    // SAFETY: F_DUPFD_CLOEXEC returned a fresh, owned descriptor with close-on-exec set.
    let owned = unsafe { OwnedFd::from_raw_fd(duplicate) };
    let mut socket_type: libc::c_int = 0;
    let mut length = std::mem::size_of_val(&socket_type) as libc::socklen_t;
    // SAFETY: the output pointer and its length describe a valid c_int on the stack.
    let result = unsafe {
        libc::getsockopt(
            owned.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut socket_type as *mut libc::c_int).cast(),
            &mut length,
        )
    };
    if result < 0 || socket_type != libc::SOCK_STREAM {
        return Err("COSMIC notification descriptor is not a stream socket".to_owned());
    }
    let socket = std::os::unix::net::UnixStream::from(owned);
    if socket.peer_addr().is_err() {
        return Err("COSMIC notification socket is not connected".to_owned());
    }
    // The panel had to make the original descriptor inheritable for this process.
    // Protect it as well as our duplicate before launching browsers or other children.
    // SAFETY: fcntl checks this still-open inherited descriptor; ownership is unchanged.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
    {
        return Err("Could not protect the inherited COSMIC notification socket".to_owned());
    }
    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn action(key: &str) -> NotificationAction {
        NotificationAction {
            key: key.to_owned(),
            label: key.to_owned(),
        }
    }

    #[test]
    fn replacement_removes_previous_action_keys() {
        let mut live = LiveNotifications::default();
        live.replace(7, &[action("old")]);
        live.replace(7, &[action("new")]);
        assert_eq!(
            live.0.iter().collect::<Vec<_>>(),
            vec![&(7, vec!["new".to_owned()])]
        );
    }

    #[test]
    fn oldest_active_binding_is_evicted_at_capacity() {
        let mut live = LiveNotifications::default();
        for id in 0..=MAX_LIVE_NOTIFICATIONS as u32 {
            live.replace(id, &[]);
        }
        assert_eq!(
            (live.0.len(), live.0.front().map(|value| value.0)),
            (MAX_LIVE_NOTIFICATIONS, Some(1))
        );
    }

    #[test]
    fn closure_removes_actions_without_removing_other_notifications() {
        let mut live = LiveNotifications::default();
        live.replace(7, &[action("open")]);
        live.replace(8, &[]);
        live.close(7);
        assert_eq!(live.0.front().map(|value| value.0), Some(8));
    }

    #[test]
    fn duplicate_does_not_take_ownership_of_inherited_socket() {
        let (mut original, mut other) = std::os::unix::net::UnixStream::pair().unwrap();
        drop(duplicate_socket(original.as_raw_fd()).unwrap());
        other.write_all(b"x").unwrap();
        let mut byte = [0];
        original.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [b'x']);
    }

    #[test]
    fn inherited_and_duplicated_sockets_are_close_on_exec() {
        let (original, _other) = std::os::unix::net::UnixStream::pair().unwrap();
        // SAFETY: the test owns this socket and reproduces its state after panel exec.
        assert_eq!(
            unsafe { libc::fcntl(original.as_raw_fd(), libc::F_SETFD, 0) },
            0
        );
        let duplicate = duplicate_socket(original.as_raw_fd()).unwrap();
        for descriptor in [original.as_raw_fd(), duplicate.as_raw_fd()] {
            // SAFETY: both descriptors are owned and live throughout this loop.
            let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
    }

    #[test]
    fn regular_file_descriptor_is_rejected() {
        let file = std::fs::File::open("/dev/null").unwrap();
        assert!(duplicate_socket(file.as_raw_fd()).is_err());
    }
}
