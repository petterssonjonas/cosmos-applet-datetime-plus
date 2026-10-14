// SPDX-License-Identifier: GPL-3.0-only

mod agenda;
mod app;
mod config;
mod display;
mod launcher;
mod media;
mod notification_preview;
mod notifications;
mod notifier;
mod places;
mod storage;

fn main() -> cosmic::iced::Result {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() == Some(std::ffi::OsStr::new("--task-notifier")) {
        let id = args
            .next()
            .and_then(|value| value.to_str()?.parse::<u64>().ok());
        let result = match (id, args.next()) {
            (Some(id), None) => notifier::run(id),
            _ => Err("Usage: cosmic-applet-datetime-plus --task-notifier <entry-id>".into()),
        };
        if let Err(error) = result {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return Ok(());
    }
    cosmic::applet::run::<app::AppModel>(())
}
