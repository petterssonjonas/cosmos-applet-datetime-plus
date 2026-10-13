// SPDX-License-Identifier: GPL-3.0-only

use std::{collections::HashMap, fmt};

use chrono::Utc;
use cosmic::widget::image::Handle;
use serde::{
    Deserialize, Deserializer,
    de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor},
};
use zbus::zvariant::{Signature, Type, Value};

use super::{Notification, NotificationAction};

const MAX_ACTIONS: usize = 16;
const MAX_ACTION_KEY_BYTES: usize = 256;
const MAX_IMAGE_DIMENSION: usize = 512;
const MAX_IMAGE_BYTES: usize = 1024 * 1024;
const ICON_DIMENSION: usize = 96;
const MAX_MARKUP_BYTES: usize = 32 * 1024;

// Borrow raster bytes directly from the D-Bus message: no per-byte Value allocation.
type ImageData<'a> = (i32, i32, i32, bool, i32, i32, &'a [u8]);

type WireNotification<'a> = (
    &'a str,
    u32,
    &'a str,
    &'a str,
    &'a str,
    Actions,
    Hints<'a>,
    i32,
);

pub(super) fn notification(message: &zbus::Message) -> Result<Notification, ()> {
    let body = message.body();
    let (app_name, id, app_icon, summary, text, actions, hints, _) =
        body.deserialize::<WireNotification<'_>>().map_err(|_| ())?;
    if id == 0 {
        return Err(());
    }
    let image = hints
        .image
        .and_then(raster_icon)
        .map(|(width, height, pixels)| Handle::from_rgba(width, height, pixels));
    let app_icon = [
        app_icon,
        hints.desktop_entry.trim_end_matches(".desktop"),
        hints.image_name,
    ]
    .into_iter()
    .find(|name| is_icon_name(name))
    .unwrap_or_default()
    .to_owned();
    Ok(Notification {
        id,
        app_name: plain_text(app_name, 256),
        app_icon,
        summary: plain_text(summary, 1024),
        body: plain_text(text, 8192),
        received_at: Utc::now(),
        actions: actions.0,
        transient: hints.transient,
        image,
    })
}

fn is_icon_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && name != "."
        && name != ".."
}

struct Actions(Vec<NotificationAction>);

impl Type for Actions {
    const SIGNATURE: &'static Signature = <Vec<String>>::SIGNATURE;
}

impl<'de> Deserialize<'de> for Actions {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ActionsVisitor;
        impl<'de> Visitor<'de> for ActionsVisitor {
            type Value = Actions;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("notification action key/label pairs")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Actions, A::Error> {
                let mut actions: Vec<NotificationAction> = Vec::new();
                while let Some(key) = sequence.next_element::<&str>()? {
                    let Some(label) = sequence.next_element::<&str>()? else {
                        break;
                    };
                    if actions.len() >= MAX_ACTIONS
                        || key.is_empty()
                        || key.len() > MAX_ACTION_KEY_BYTES
                        || key.chars().any(char::is_control)
                        || actions.iter().any(|action| action.key == key)
                    {
                        continue;
                    }
                    // The key remains byte-for-byte identical to the server-supplied action.
                    actions.push(NotificationAction {
                        key: key.to_owned(),
                        label: plain_text(label, 512),
                    });
                }
                Ok(Actions(actions))
            }
        }
        deserializer.deserialize_seq(ActionsVisitor)
    }
}

#[derive(Default)]
struct Hints<'a> {
    transient: bool,
    image: Option<ImageData<'a>>,
    desktop_entry: &'a str,
    image_name: &'a str,
}

impl Type for Hints<'_> {
    const SIGNATURE: &'static Signature = <HashMap<String, Value<'static>>>::SIGNATURE;
}

impl<'de> Deserialize<'de> for Hints<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct HintsVisitor;
        impl<'de> Visitor<'de> for HintsVisitor {
            type Value = Hints<'de>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("notification hints")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Hints<'de>, A::Error> {
                let mut hints = Hints::default();
                while let Some(key) = map.next_key::<&str>()? {
                    match (key, map.next_value::<Hint<'de>>()?) {
                        ("transient", Hint::Bool(value)) => hints.transient = value,
                        ("image-data", Hint::Image(value)) => hints.image = Some(value),
                        ("image_data" | "icon_data", Hint::Image(value))
                            if hints.image.is_none() =>
                        {
                            hints.image = Some(value);
                        }
                        ("desktop-entry", Hint::Text(value)) => hints.desktop_entry = value,
                        ("image-path" | "image_path", Hint::Text(value)) => {
                            hints.image_name = value
                        }
                        _ => {}
                    }
                }
                Ok(hints)
            }
        }
        deserializer.deserialize_map(HintsVisitor)
    }
}

enum Hint<'a> {
    Bool(bool),
    Image(ImageData<'a>),
    Text(&'a str),
    Ignored,
}

impl<'de> Deserialize<'de> for Hint<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct HintVisitor;
        impl<'de> Visitor<'de> for HintVisitor {
            type Value = Hint<'de>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a notification hint variant")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Hint<'de>, A::Error> {
                let signature: &str = sequence
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let value = match signature {
                    "b" => sequence.next_element::<bool>()?.map(Hint::Bool),
                    "s" => sequence.next_element::<&str>()?.map(Hint::Text),
                    "(iiibiiay)" => sequence.next_element::<ImageData<'de>>()?.map(Hint::Image),
                    _ => sequence
                        .next_element::<IgnoredAny>()?
                        .map(|_| Hint::Ignored),
                };
                value.ok_or_else(|| de::Error::invalid_length(1, &self))
            }
        }
        deserializer.deserialize_struct("Variant", &["signature", "value"], HintVisitor)
    }
}

fn raster_icon(data: ImageData<'_>) -> Option<(u32, u32, Vec<u8>)> {
    let (width, height, rowstride, has_alpha, bits, channels, bytes) = data;
    let width = usize::try_from(width).ok()?;
    let height = usize::try_from(height).ok()?;
    let rowstride = usize::try_from(rowstride).ok()?;
    let channels = usize::try_from(channels).ok()?;
    if width == 0
        || height == 0
        || width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || bits != 8
        || channels != if has_alpha { 4 } else { 3 }
        || bytes.len() > MAX_IMAGE_BYTES
    {
        return None;
    }
    let row_bytes = width.checked_mul(channels)?;
    let required = (height - 1)
        .checked_mul(rowstride)?
        .checked_add(row_bytes)?;
    let padded = height.checked_mul(rowstride)?;
    if rowstride < row_bytes
        || required > bytes.len()
        || bytes.len() > padded
        || padded > MAX_IMAGE_BYTES
    {
        return None;
    }
    let largest = width.max(height);
    let (target_width, target_height) = if largest > ICON_DIMENSION {
        (
            (width * ICON_DIMENSION / largest).max(1),
            (height * ICON_DIMENSION / largest).max(1),
        )
    } else {
        (width, height)
    };
    // Sample directly from the validated raster, allocating only the bounded final icon.
    let mut pixels = Vec::with_capacity(target_width * target_height * 4);
    for y in 0..target_height {
        let source_y = y * height / target_height;
        for x in 0..target_width {
            let source_x = x * width / target_width;
            let offset = source_y * rowstride + source_x * channels;
            pixels.extend_from_slice(&bytes[offset..offset + 3]);
            pixels.push(if has_alpha { bytes[offset + 3] } else { 255 });
        }
    }
    Some((target_width as u32, target_height as u32, pixels))
}

fn plain_text(text: &str, max_bytes: usize) -> String {
    let mut output = String::with_capacity(text.len().min(max_bytes));
    if text.len() <= MAX_MARKUP_BYTES && (text.contains('<') || text.contains('&')) {
        let wrapped = format!("<notification>{text}</notification>");
        let options = roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 2048,
        };
        if let Ok(document) = roxmltree::Document::parse_with_options(&wrapped, options) {
            for node in document.descendants().filter(|node| node.is_text()) {
                if !append_text(&mut output, node.text().unwrap_or_default(), max_bytes) {
                    break;
                }
            }
            return output;
        }
    }
    append_text(&mut output, text, max_bytes);
    output
}

fn append_text(output: &mut String, text: &str, max_bytes: usize) -> bool {
    for character in text.chars() {
        if (character.is_control() && !matches!(character, '\n' | '\t'))
            || matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            continue;
        }
        if output.len() + character.len_utf8() > max_bytes {
            return false;
        }
        output.push(character);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(actions: &[&str], hints: HashMap<&str, Value<'_>>) -> zbus::Message {
        zbus::Message::signal(
            super::super::APPLET_PATH,
            super::super::APPLET_INTERFACE,
            "Notify",
        )
        .unwrap()
        .build(&(
            "Example",
            17_u32,
            "example-icon",
            "<b>Hello</b>",
            "Open <a href='https://invalid.example'>message</a> &amp; read",
            actions,
            hints,
            -1_i32,
        ))
        .unwrap()
    }

    #[test]
    fn wire_decoder_preserves_id_supplied_key_and_transient_hint() {
        let notification = notification(&message(
            &["custom-action", "<b>Open</b>"],
            HashMap::from([
                ("transient", Value::from(true)),
                ("urgency", Value::from(2_u8)),
            ]),
        ))
        .unwrap();
        assert_eq!(
            (
                notification.id,
                notification.actions[0].key.as_str(),
                notification.transient
            ),
            (17, "custom-action", true)
        );
    }

    #[test]
    fn markup_becomes_plain_text_without_clickable_link_or_target() {
        let notification = notification(&message(&[], HashMap::new())).unwrap();
        assert_eq!(
            (notification.summary.as_str(), notification.body.as_str()),
            ("Hello", "Open message & read")
        );
    }

    #[test]
    fn unmatched_duplicate_and_overlong_action_keys_are_not_exposed() {
        let overlong = "x".repeat(MAX_ACTION_KEY_BYTES + 1);
        let notification = notification(&message(
            &[
                "open",
                "Open",
                "open",
                "Duplicate",
                &overlong,
                "Too long",
                "orphan",
            ],
            HashMap::new(),
        ))
        .unwrap();
        assert_eq!(
            notification
                .actions
                .iter()
                .map(|action| action.key.as_str())
                .collect::<Vec<_>>(),
            vec!["open"]
        );
    }

    #[test]
    fn action_storage_is_bounded() {
        let keys: Vec<_> = (0..MAX_ACTIONS + 10)
            .map(|index| format!("action-{index}"))
            .collect();
        let actions: Vec<_> = keys.iter().flat_map(|key| [key.as_str(), "Open"]).collect();
        assert_eq!(
            notification(&message(&actions, HashMap::new()))
                .unwrap()
                .actions
                .len(),
            MAX_ACTIONS
        );
    }

    #[test]
    fn borrowed_image_variant_is_decoded_from_wire() {
        let image = (
            1_i32,
            1_i32,
            4_i32,
            true,
            8_i32,
            4_i32,
            vec![10_u8, 20, 30, 40],
        );
        let notification = notification(&message(
            &[],
            HashMap::from([("image-data", Value::new(image))]),
        ))
        .unwrap();
        assert!(notification.image.is_some());
    }

    #[test]
    fn rgb_row_padding_is_not_interpreted_as_pixels() {
        let bytes = [1, 2, 3, 99, 4, 5, 6];
        let decoded = raster_icon((1, 2, 4, false, 8, 3, &bytes)).unwrap();
        assert_eq!(decoded, (1, 2, vec![1, 2, 3, 255, 4, 5, 6, 255]));
    }

    #[test]
    fn malformed_stride_and_alpha_layout_are_rejected() {
        let bytes = [0; 16];
        assert!(
            raster_icon((2, 2, 4, true, 8, 4, &bytes)).is_none()
                && raster_icon((2, 2, 8, true, 8, 3, &bytes)).is_none()
        );
    }

    #[test]
    fn oversized_dimensions_use_application_icon_instead() {
        let image = (i32::MAX, 1_i32, i32::MAX, true, 8_i32, 4_i32, vec![0_u8; 4]);
        let notification = notification(&message(
            &[],
            HashMap::from([("image-data", Value::new(image))]),
        ))
        .unwrap();
        assert!(notification.image.is_none() && notification.app_icon == "example-icon");
    }

    #[test]
    fn large_valid_raster_is_resized_with_aspect_ratio_and_byte_limit() {
        let bytes = vec![255; 256 * 128 * 4];
        let (width, height, pixels) = raster_icon((256, 128, 1024, true, 8, 4, &bytes)).unwrap();
        assert_eq!((width, height, pixels.len()), (96, 48, 96 * 48 * 4));
    }

    #[test]
    fn byte_limit_is_enforced_at_utf8_boundary_and_controls_are_removed() {
        assert_eq!(plain_text("a\u{202e}\0ééé", 6), "aéé");
    }

    #[test]
    fn malformed_markup_is_literal_text_not_executed() {
        assert_eq!(
            plain_text("Hello <a href='javascript:alert(1)'>", 128),
            "Hello <a href='javascript:alert(1)'>"
        );
    }

    #[test]
    fn remote_and_absolute_icons_are_not_given_to_unbounded_ui_loaders() {
        assert!(
            !is_icon_name("https://example.invalid/icon.png") && !is_icon_name("/tmp/icon.png")
        );
    }
}
