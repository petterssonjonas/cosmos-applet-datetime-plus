// SPDX-License-Identifier: GPL-3.0-only
//! A private per-entry socket acknowledges a created normal window, not process spawn.

use std::{
    fs::{File, OpenOptions},
    hash::{Hash, Hasher},
    io,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::UnixListener,
        },
    },
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, LazyLock},
    time::Duration,
};

use cosmic::iced::{Subscription, futures::channel::mpsc, stream};
use futures_util::SinkExt;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::{broadcast, watch},
    time::{Instant, sleep, timeout},
};

use super::Message;
use crate::{launcher, storage};

const REQUEST: u8 = b'S';
const READY: u8 = b'R';
const FAILED: u8 = b'F';
const START_TIMEOUT: Duration = Duration::from_secs(12);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
static EXITS: LazyLock<broadcast::Sender<u64>> = LazyLock::new(|| broadcast::channel(32).0);

pub(crate) fn exits() -> broadcast::Receiver<u64> {
    EXITS.subscribe()
}

fn socket_path(agenda: &Path, id: u64) -> PathBuf {
    agenda.with_file_name(format!("reminder-{id}.sock"))
}

pub(super) struct Server {
    _lock: File,
    path: PathBuf,
    listener: UnixListener,
    pub(super) opened: watch::Sender<bool>,
}

impl Hash for Server {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.path.hash(state);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // The lock is still owned here; another notifier cannot bind this pathname yet.
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!("Could not remove the reminder socket: {error}");
        }
    }
}

pub(super) async fn claim(agenda: &Path, id: u64) -> Result<Option<Arc<Server>>, String> {
    storage::prepare_parent(agenda).await?;
    let path = socket_path(agenda, id);
    let lock_path = path.with_extension("window-lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(lock_path)
        .map_err(|error| format!("Could not open the reminder window lock: {error}"))?;
    let metadata = lock
        .metadata()
        .map_err(|error| format!("Could not inspect the reminder window lock: {error}"))?;
    // SAFETY: geteuid has no memory access requirements.
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err("The reminder window lock must be a private, owned regular file.".into());
    }
    lock.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("Could not secure the reminder window lock: {error}"))?;
    // SAFETY: this independently owned descriptor is open; the operation never blocks.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(format!("Could not lock the reminder window: {error}"));
    }
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            // Never remove arbitrary data or follow a symlink when recovering a stale socket.
            if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
                return Err("The reminder socket must be an owned Unix socket.".into());
            }
            std::fs::remove_file(&path)
                .map_err(|error| format!("Could not remove the stale reminder socket: {error}"))?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("Could not inspect the reminder socket: {error}")),
    }
    let listener = UnixListener::bind(&path)
        .map_err(|error| format!("Could not bind the reminder socket: {error}"))?;
    let server = Arc::new(Server {
        _lock: lock,
        path,
        listener,
        opened: watch::channel(false).0,
    });
    std::fs::set_permissions(&server.path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("Could not secure the reminder socket: {error}"))?;
    server
        .listener
        .set_nonblocking(true)
        .map_err(|error| format!("Could not monitor the reminder socket: {error}"))?;
    Ok(Some(server))
}

async fn request(stream: &mut UnixStream) -> Result<(), String> {
    // Private directory permissions also protect the pathname. Check the connected peer.
    let credential = stream
        .peer_cred()
        .map_err(|error| format!("Could not inspect the reminder peer: {error}"))?;
    // SAFETY: geteuid has no memory access requirements.
    if credential.uid() != unsafe { libc::geteuid() } {
        return Err("The reminder socket belongs to another user.".into());
    }
    stream
        .write_u8(REQUEST)
        .await
        .map_err(|error| format!("Could not request the reminder window: {error}"))?;
    match stream.read_u8().await {
        Ok(READY) => Ok(()),
        Ok(FAILED) => Err("The reminder window could not become ready. It will be retried.".into()),
        Ok(_) => Err("The reminder window returned an invalid acknowledgement.".into()),
        Err(error) => Err(format!(
            "The reminder window closed before acknowledgement: {error}"
        )),
    }
}

fn observe(mut stream: UnixStream, id: u64) {
    let events = EXITS.clone();
    tokio::spawn(async move {
        // Holding this lease detects window death even after its original applet restarts.
        let _ = stream.read_u8().await;
        let _ = events.send(id);
    });
}

pub(crate) async fn present(id: u64) -> Result<(), String> {
    let agenda = storage::data_path("agenda.json")?;
    let path = socket_path(&agenda, id);
    present_at(&path, id).await
}

pub(super) async fn activate_existing(agenda: &Path, id: u64) -> Result<(), String> {
    ready_connection(&socket_path(agenda, id), Instant::now() + START_TIMEOUT).await?;
    Ok(())
}

async fn present_at(path: &Path, id: u64) -> Result<(), String> {
    match UnixStream::connect(path).await {
        Ok(mut stream) => {
            timeout(START_TIMEOUT, request(&mut stream)).await
                .map_err(|_| "The existing reminder window did not acknowledge readiness. It will be retried.".to_owned())??;
            observe(stream, id);
            return Ok(());
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) => {}
        Err(error) => return Err(format!("Could not connect to the reminder window: {error}")),
    }
    let executable = std::env::current_exe()
        .map_err(|error| format!("Could not locate the reminder application: {error}"))?;
    let mut child = launcher::desktop_command(executable)
        .arg("--task-notifier")
        .arg(id.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|error| format!("Could not start the reminder window: {error}"))?;
    let events = EXITS.clone();
    tokio::spawn(async move {
        if let Err(error) = child.wait().await {
            eprintln!("Could not reap the reminder window process: {error}");
        }
        let _ = events.send(id);
    });
    let stream = ready_connection(path, Instant::now() + START_TIMEOUT).await?;
    observe(stream, id);
    Ok(())
}

async fn ready_connection(path: &Path, until: Instant) -> Result<UnixStream, String> {
    loop {
        match UnixStream::connect(path).await {
            Ok(mut stream) => {
                timeout(until.saturating_duration_since(Instant::now()), request(&mut stream)).await
                    .map_err(|_| "The reminder application started but no native window acknowledged readiness. It will be retried.".to_owned())??;
                return Ok(stream);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) => {}
            Err(error) => {
                return Err(format!(
                    "Could not connect to the new reminder window: {error}"
                ));
            }
        }
        if Instant::now() >= until {
            return Err("The reminder application did not create its window in time. The reminder remains due.".into());
        }
        // Bounded process-start handshake only, never a GUI or agenda polling loop.
        sleep(Duration::from_millis(40)).await;
    }
}

pub(super) fn subscription(server: Arc<Server>) -> Subscription<Message> {
    Subscription::run_with(server, |server| {
        let server = server.clone();
        stream::channel(16, |mut output: mpsc::Sender<Message>| async move {
            let listener = match server
                .listener
                .try_clone()
                .and_then(tokio::net::UnixListener::from_std)
            {
                Ok(listener) => listener,
                Err(error) => {
                    let _ = output
                        .send(Message::TransportError(format!(
                            "Could not listen for reminder acknowledgement: {error}"
                        )))
                        .await;
                    std::future::pending::<()>().await;
                    return;
                }
            };
            let mut leases = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    Some(_) = leases.join_next(), if !leases.is_empty() => {}
                    accepted = listener.accept(), if leases.len() < 64 => {
                        let (mut connection, _) = match accepted {
                            Ok(connection) => connection,
                            Err(error) => {
                                let _ = output.send(Message::TransportError(format!("The reminder acknowledgement listener failed: {error}"))).await;
                                std::future::pending::<()>().await;
                                return;
                            }
                        };
                        let mut opened = server.opened.subscribe();
                        let mut output = output.clone();
                        leases.spawn(async move {
                            let result = timeout(REQUEST_TIMEOUT, async {
                                let credential = connection.peer_cred()?;
                                // SAFETY: geteuid has no memory access requirements.
                                if credential.uid() != unsafe { libc::geteuid() } || connection.read_u8().await? != REQUEST {
                                    return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Invalid reminder request"));
                                }
                                output.send(Message::Activate).await.map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "Reminder window stopped"))?;
                                opened.wait_for(|ready| *ready).await.map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "Reminder window did not open"))?;
                                connection.write_u8(READY).await
                            }).await;
                            if matches!(result, Ok(Ok(()))) {
                                // EOF ends the lease; no heartbeat or deadline polling is needed.
                                let _ = connection.read_u8().await;
                            } else {
                                let _ = timeout(REQUEST_TIMEOUT, connection.write_u8(FAILED)).await;
                            }
                        });
                    }
                }
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agenda::tests::Directory;

    #[tokio::test]
    async fn independent_window_claims_deduplicate_and_release_after_owner_drops() {
        let directory = Directory::new();
        let path = directory.agenda();
        let first = claim(&path, 7).await.unwrap().unwrap();
        assert!(claim(&path, 7).await.unwrap().is_none());
        drop(first);
        assert!(claim(&path, 7).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_stale_socket_is_replaced_only_after_acquiring_its_window_lock() {
        let directory = Directory::new();
        let path = directory.agenda();
        storage::prepare_parent(&path).await.unwrap();
        let stale = UnixListener::bind(socket_path(&path, 3)).unwrap();
        drop(stale);
        assert!(claim(&path, 3).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn socket_recovery_preserves_an_unexpected_regular_file() {
        let directory = Directory::new();
        let path = directory.agenda();
        storage::prepare_parent(&path).await.unwrap();
        let socket = socket_path(&path, 3);
        tokio::fs::write(&socket, b"user data").await.unwrap();
        assert!(claim(&path, 3).await.is_err());
        assert_eq!(tokio::fs::read(socket).await.unwrap(), b"user data");
    }

    #[tokio::test]
    async fn acknowledgement_rejects_invalid_replies_without_launching_any_application() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let reply = async {
            assert_eq!(server.read_u8().await.unwrap(), REQUEST);
            server.write_u8(b'?').await.unwrap();
        };
        let (result, ()) = tokio::join!(request(&mut client), reply);
        assert!(result.unwrap_err().contains("invalid acknowledgement"));
    }

    #[tokio::test]
    async fn acknowledgement_requires_ready_reply_not_connected_socket() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let (received, wait_received) = tokio::sync::oneshot::channel();
        let (release, wait_release) = tokio::sync::oneshot::channel();
        let job = tokio::spawn(async move { request(&mut client).await });
        assert_eq!(server.read_u8().await.unwrap(), REQUEST);
        let responder = tokio::spawn(async move {
            received.send(()).unwrap();
            wait_release.await.unwrap();
            server.write_u8(READY).await.unwrap();
        });
        wait_received.await.unwrap();
        assert!(!job.is_finished());
        release.send(()).unwrap();
        responder.await.unwrap();
        assert!(job.await.unwrap().is_ok());
    }
}
