// SPDX-License-Identifier: GPL-3.0-only

use cosmic::iced::{self, Event, Subscription, Task, event, window};

/// Query the logical monitor dimensions for the applet's actual panel window.
pub fn size(core: &cosmic::Core) -> Task<Option<(f32, f32)>> {
    let Some(id) = core.main_window_id() else {
        return Task::done(None);
    };
    window::monitor_size(id).map(|size| {
        size.filter(|size| {
            size.width.is_finite()
                && size.height.is_finite()
                && size.width > 0.0
                && size.height > 0.0
        })
        .map(|size| (size.width, size.height))
    })
}

/// Re-query only on native monitor/window geometry changes, never on a timer.
pub fn changes() -> Subscription<()> {
    event::listen_with(|event, _, _| match event {
        Event::Window(
            window::Event::Opened { .. }
            | window::Event::Moved(_)
            | window::Event::Resized(_)
            | window::Event::Rescaled(_),
        )
        | Event::PlatformSpecific(iced::core::event::PlatformSpecific::Wayland(
            iced::core::event::wayland::Event::Output(_, _),
        )) => Some(()),
        _ => None,
    })
}
