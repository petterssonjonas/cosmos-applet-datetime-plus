// SPDX-License-Identifier: GPL-3.0-only

use serde::Serialize;
use std::{
    env,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
};

const MAX_BYTES: usize = 1024 * 1024;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

pub(crate) fn data_path(filename: &str) -> Result<PathBuf, String> {
    let mut components = Path::new(filename).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err("Invalid applet data filename.".into());
    }
    let root = match env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        Some(value) => PathBuf::from(value),
        None => PathBuf::from(
            env::var_os("HOME").ok_or("HOME is unavailable; cannot locate applet data.")?,
        )
        .join(".config"),
    };
    if !root.is_absolute() {
        return Err("The configuration directory must be an absolute path.".into());
    }
    Ok(root.join("cosmos-applet-datetime-plus").join(filename))
}

pub(crate) async fn read(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let file = match fs::OpenOptions::new()
        .read(true)
        // Opening a mistakenly supplied FIFO must not block the async I/O pool.
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .await
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Could not open applet data: {error}")),
    };
    let metadata = file
        .metadata()
        .await
        .map_err(|error| format!("Could not inspect applet data: {error}"))?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES as u64 {
        return Err("Applet data must be a regular JSON file no larger than 1 MiB.".into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| format!("Could not read applet data: {error}"))?;
    if bytes.len() > MAX_BYTES {
        return Err("Applet data exceeds the 1 MiB limit.".into());
    }
    Ok(Some(bytes))
}

pub(crate) async fn prepare_parent(path: &Path) -> Result<&Path, String> {
    use std::os::unix::fs::PermissionsExt;

    let parent = path.parent().ok_or("Invalid applet data path.")?;
    fs::create_dir_all(parent)
        .await
        .map_err(|error| format!("Could not create applet data directory: {error}"))?;
    fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
        .await
        .map_err(|error| format!("Could not secure applet data directory: {error}"))?;
    Ok(parent)
}

pub(crate) async fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("Could not encode applet data: {error}"))?;
    if bytes.len() > MAX_BYTES {
        return Err("Applet data exceeds the 1 MiB limit.".into());
    }
    let parent = prepare_parent(path).await?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "System clock precedes the Unix epoch.")?
        .as_nanos();
    let temporary = parent.join(format!(
        ".data-{}-{stamp}-{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let result = async {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .await
            .map_err(|error| format!("Could not create temporary applet data: {error}"))?;
        file.write_all(&bytes)
            .await
            .map_err(|error| format!("Could not write applet data: {error}"))?;
        file.sync_all()
            .await
            .map_err(|error| format!("Could not persist applet data: {error}"))?;
        fs::rename(&temporary, path)
            .await
            .map_err(|error| format!("Could not replace applet data: {error}"))?;
        fs::File::open(parent)
            .await
            .map_err(|error| format!("Could not open applet data directory: {error}"))?
            .sync_all()
            .await
            .map_err(|error| {
                format!("Applet data was written, but directory synchronization failed: {error}")
            })
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&temporary).await;
    }
    result
}
