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

/// Track the panel's named Wayland output; native applet surfaces cannot use
/// `window::monitor_size`, whose Wayland `current_monitor` is unimplemented.
pub fn panel_size(output_name: String) -> Subscription<(f32, f32)> {
    use iced::core::event::wayland::{Event as WaylandEvent, OutputEvent};

    event::listen_with(|event, _, _| match event {
        Event::PlatformSpecific(event::PlatformSpecific::Wayland(WaylandEvent::Output(
            OutputEvent::Created(Some(info)) | OutputEvent::InfoUpdate(info),
            _,
        ))) => Some((info.name, info.logical_size)),
        _ => None,
    })
    .with(output_name)
    .filter_map(|(expected, (name, size))| panel_dimensions(&expected, name.as_deref(), size))
}

fn panel_dimensions(
    expected: &str,
    name: Option<&str>,
    size: Option<(i32, i32)>,
) -> Option<(f32, f32)> {
    let (width, height) = size?;
    (name == Some(expected) && !expected.is_empty() && width > 0 && height > 0)
        .then_some((width as f32, height as f32))
}

#[cfg(test)]
mod tests {
    use super::panel_dimensions;

    #[test]
    fn panel_uses_only_its_named_outputs_logical_dimensions() {
        assert_eq!(
            panel_dimensions("DP-1", Some("DP-1"), Some((1536, 864))),
            Some((1536.0, 864.0))
        );
        assert_eq!(
            panel_dimensions("DP-1", Some("HDMI-A-1"), Some((3840, 2160))),
            None
        );
    }

    #[test]
    fn incomplete_or_invalid_output_geometry_is_ignored() {
        for (name, size) in [
            (None, Some((1920, 1080))),
            (Some("DP-1"), None),
            (Some("DP-1"), Some((0, 1080))),
            (Some("DP-1"), Some((1920, -1))),
        ] {
            assert_eq!(panel_dimensions("DP-1", name, size), None);
        }
        assert_eq!(panel_dimensions("", Some(""), Some((1920, 1080))), None);
    }
}
