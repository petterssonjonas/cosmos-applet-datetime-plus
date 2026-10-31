//! One native text input with a fixed-width, segmented duration editing policy.

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, clipboard, input_method, layout, mouse, renderer,
    text::Paragraph,
    widget::{Operation, Tree, tree},
};
use cosmic::iced::{Event, Font, Length, Point, Rectangle, Size, keyboard, touch};
use cosmic::widget::text_input::{self, State as NativeState};
use cosmic::{Element, Renderer, Theme};

const MAX_SECONDS: u32 = 99 * 3600 + 59 * 60 + 59;

#[derive(Clone, Debug)]
pub enum Message {
    Changed(u32),
}

pub fn input(seconds: u32, enabled: bool) -> Element<'static, Message> {
    let seconds = seconds.min(MAX_SECONDS);
    Element::new(DurationInput {
        seconds,
        enabled,
        native: native_input(seconds, enabled),
    })
}

fn formatted(seconds: u32) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

fn parsed(text: &str) -> Option<u32> {
    let bytes = text.as_bytes();
    if bytes.len() != 8 || bytes[2] != b':' || bytes[5] != b':' {
        return None;
    }
    let mut groups = [0_u32; 3];
    for (group, start) in groups.iter_mut().zip([0, 3, 6]) {
        let (first, second) = (bytes[start], bytes[start + 1]);
        if !first.is_ascii_digit() || !second.is_ascii_digit() {
            return None;
        }
        *group = u32::from(first - b'0') * 10 + u32::from(second - b'0');
    }
    (groups[1] < 60 && groups[2] < 60).then_some(groups[0] * 3600 + groups[1] * 60 + groups[2])
}

fn native_input(seconds: u32, enabled: bool) -> Element<'static, Message> {
    let field = text_input::text_input("", formatted(seconds))
        .font(Font::MONOSPACE)
        .size(16)
        .padding([6, 8])
        .width(124);
    if enabled {
        // All editing events are handled by the wrapper. Keep the native input
        // enabled, and validate even an unexpected native editing path.
        field
            .on_input(move |text| Message::Changed(parsed(&text).unwrap_or(seconds)))
            .into()
    } else {
        field.into()
    }
}

#[derive(Debug)]
struct EditState {
    seconds: u32,
    group: usize,
    digits: u8,
}

impl EditState {
    fn new(seconds: u32) -> Self {
        Self {
            seconds,
            group: 0,
            digits: 0,
        }
    }

    fn synchronize(&mut self, seconds: u32) {
        if self.seconds != seconds {
            self.seconds = seconds;
            self.select(0);
        }
    }

    fn select(&mut self, group: usize) {
        self.group = group.min(2);
        self.digits = 0;
    }

    // False means Tab must leave the control, not wrap around internally.
    fn navigate(&mut self, backwards: bool) -> bool {
        let next = if backwards {
            self.group.checked_sub(1)
        } else {
            self.group.checked_add(1).filter(|group| *group < 3)
        };
        self.digits = 0;
        if let Some(group) = next {
            self.group = group;
            true
        } else {
            false
        }
    }

    fn unit(&self) -> u32 {
        [3600, 60, 1][self.group]
    }

    fn value(&self) -> u32 {
        let value = self.seconds / self.unit();
        if self.group == 0 { value } else { value % 60 }
    }

    fn set_value(&mut self, value: u32) {
        self.seconds = self.seconds - self.value() * self.unit() + value * self.unit();
    }

    fn digit(&mut self, digit: u8) {
        let value = if self.digits == 1 {
            self.value() * 10 + u32::from(digit)
        } else {
            u32::from(digit)
        };
        if value <= if self.group == 0 { 99 } else { 59 } {
            self.set_value(value);
            self.digits = if self.digits == 1 { 2 } else { 1 };
        }
    }

    fn delete(&mut self, backwards: bool) {
        if backwards && self.digits == 2 {
            self.set_value(self.value() / 10);
            self.digits = 1;
        } else {
            self.set_value(0);
            self.digits = 0;
        }
    }

    fn insert(&mut self, text: &str) {
        if let Some(seconds) = parsed(text) {
            self.seconds = seconds;
            self.digits = 0;
        } else if text.len() == 1 && text.as_bytes()[0].is_ascii_digit() {
            self.digit(text.as_bytes()[0] - b'0');
        } else if text.len() == 2 && text.bytes().all(|b| b.is_ascii_digit()) {
            // A multi-character IME/paste replaces the selected group atomically.
            let bytes = text.as_bytes();
            let value = u32::from(bytes[0] - b'0') * 10 + u32::from(bytes[1] - b'0');
            if value <= if self.group == 0 { 99 } else { 59 } {
                self.set_value(value);
                self.digits = 2;
            }
        }
    }
}

struct DurationInput {
    seconds: u32,
    enabled: bool,
    native: Element<'static, Message>,
}

impl DurationInput {
    fn highlight(&self, tree: &mut Tree) {
        let group = tree.state.downcast_ref::<EditState>().group;
        let native = tree.children[0].state.downcast_mut::<NativeState>();
        if native.is_focused() {
            native.select_range(group * 3, group * 3 + 2);
        }
    }

    fn refresh(&mut self, tree: &mut Tree, shell: &mut Shell<'_, Message>, before: u32) {
        let seconds = tree.state.downcast_ref::<EditState>().seconds;
        if seconds != before {
            let id = self.native.as_widget().id();
            self.native = native_input(seconds, self.enabled);
            if let Some(id) = id {
                self.native.as_widget_mut().set_id(id);
            }
            self.native.as_widget_mut().diff(&mut tree.children[0]);
            // Publishing immediately commits even a single units digit before a
            // following Play click; no blur message or delayed commit is needed.
            shell.publish(Message::Changed(seconds));
            shell.invalidate_layout();
        }
        self.highlight(tree);
        shell.request_redraw();
        shell.capture_event();
    }

    fn click(&mut self, tree: &mut Tree, layout: Layout<'_>, position: Point) {
        // There are no label/helper/icon nodes: these are the native field's
        // row and text nodes, not independently positioned replacement inputs.
        let text_bounds = layout
            .children()
            .next()
            .and_then(|row| row.children().next())
            .map_or(layout.bounds(), |text| text.bounds());
        let native = tree.children[0].state.downcast_mut::<NativeState>();
        let hit = native.value.raw().hit_test(Point::new(
            position.x - text_bounds.x,
            text_bounds.height / 2.0,
        ));
        let index = hit.map_or(0, |hit| hit.cursor());
        native.focus();
        tree.state
            .downcast_mut::<EditState>()
            .select((index / 3).min(2));
        self.highlight(tree);
    }
}

impl Widget<Message, Theme, Renderer> for DurationInput {
    fn size(&self) -> Size<Length> {
        self.native.as_widget().size()
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<EditState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(EditState::new(self.seconds))
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(self.native.as_widget())]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.state
            .downcast_mut::<EditState>()
            .synchronize(self.seconds);
        tree.diff_children(&mut [self.native.as_widget_mut()]);
        if !self.enabled {
            tree.state.downcast_mut::<EditState>().select(0);
        }
        self.highlight(tree);
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.native
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        // Native text_input registers a focusable even when disabled. Do not
        // expose that Tab stop while showing a running/paused countdown.
        if !self.enabled {
            return;
        }
        let was_focused = tree.children[0]
            .state
            .downcast_ref::<NativeState>()
            .is_focused();
        self.native
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
        let focused = tree.children[0]
            .state
            .downcast_ref::<NativeState>()
            .is_focused();
        if focused != was_focused {
            tree.state.downcast_mut::<EditState>().select(0);
        }
        self.highlight(tree);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if !self.enabled {
            return;
        }
        let focused = tree.children[0]
            .state
            .downcast_ref::<NativeState>()
            .is_focused();
        if !focused && matches!(event, Event::Keyboard(_) | Event::InputMethod(_)) {
            return;
        }
        let before = tree.state.downcast_ref::<EditState>().seconds;
        let click = match event {
            Event::Mouse(mouse::Event::ButtonPressed(_)) => cursor.position_over(layout.bounds()),
            Event::Touch(touch::Event::FingerPressed { position, .. })
                if layout.bounds().contains(*position) =>
            {
                Some(*position)
            }
            _ => None,
        };
        if self.enabled
            && let Some(position) = click
        {
            self.click(tree, layout, position);
            if matches!(
                event,
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Middle))
            ) && let Some(text) = clipboard.read(clipboard::Kind::Primary)
            {
                tree.state.downcast_mut::<EditState>().insert(&text);
            }
            self.refresh(tree, shell, before);
            return;
        }
        if self.enabled && focused {
            let edit = tree.state.downcast_mut::<EditState>();
            match event {
                Event::Keyboard(keyboard::Event::KeyPressed {
                    key,
                    text,
                    modifiers,
                    physical_key,
                    ..
                }) => {
                    use keyboard::key::Named;
                    match key.as_ref() {
                        keyboard::Key::Named(Named::Tab) if !modifiers.control() => {
                            if !edit.navigate(modifiers.shift()) {
                                // Leave Ignored: libcosmic keyboard_nav performs
                                // native focus-next/previous, retaining this stop.
                                self.highlight(tree);
                                shell.request_redraw();
                                return;
                            }
                        }
                        keyboard::Key::Named(Named::ArrowLeft) => {
                            edit.navigate(true);
                        }
                        keyboard::Key::Named(Named::ArrowRight) => {
                            edit.navigate(false);
                        }
                        keyboard::Key::Named(Named::Home) => edit.select(0),
                        keyboard::Key::Named(Named::End) => edit.select(2),
                        keyboard::Key::Named(Named::Backspace) => edit.delete(true),
                        keyboard::Key::Named(Named::Delete) if !modifiers.shift() => {
                            edit.delete(false)
                        }
                        keyboard::Key::Named(Named::Escape | Named::Enter) => {
                            edit.digits = 0;
                            self.native.as_widget_mut().update(
                                &mut tree.children[0],
                                event,
                                layout,
                                cursor,
                                renderer,
                                clipboard,
                                shell,
                                viewport,
                            );
                            return;
                        }
                        _ => {
                            let shortcut = if modifiers.command() || modifiers.control() {
                                key.to_latin(*physical_key)
                            } else {
                                None
                            };
                            let paste = shortcut == Some('v')
                                || matches!(key.as_ref(), keyboard::Key::Named(Named::Insert))
                                    && modifiers.shift();
                            let cut = shortcut == Some('x')
                                || matches!(key.as_ref(), keyboard::Key::Named(Named::Delete))
                                    && modifiers.shift();
                            if paste {
                                if let Some(text) = clipboard.read(clipboard::Kind::Standard) {
                                    edit.insert(&text);
                                }
                            } else if shortcut == Some('c') || cut {
                                clipboard.write(
                                    clipboard::Kind::Standard,
                                    format!("{:02}", edit.value()),
                                );
                                if cut {
                                    edit.delete(false);
                                }
                            } else if shortcut == Some('a') {
                                edit.digits = 0;
                            } else if !modifiers.command()
                                && !modifiers.control()
                                && let Some(text) = text
                            {
                                edit.insert(text.as_str());
                            }
                        }
                    }
                    self.refresh(tree, shell, before);
                    return;
                }
                Event::InputMethod(input_method::Event::Commit(text)) => {
                    edit.insert(text);
                    self.refresh(tree, shell, before);
                    return;
                }
                _ => {}
            }
        }
        // Drag/drop and native context menus cannot mutate the fixed separators.
        // Mouse press was handled locally above; the native child still owns
        // ordinary focus loss, IME preedit, cursor blinking, theme and rendering.
        if matches!(event, Event::Dnd(_)) {
            return;
        }
        self.native.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        let now_focused = tree.children[0]
            .state
            .downcast_ref::<NativeState>()
            .is_focused();
        if focused != now_focused {
            tree.state.downcast_mut::<EditState>().select(0);
        }
        self.highlight(tree);
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.native.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.native.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_digit_is_units_and_second_digit_appends() {
        let mut edit = EditState::new(23 * 3600 + 45 * 60 + 56);
        edit.insert("1");
        assert_eq!(formatted(edit.seconds), "01:45:56");
        edit.synchronize(edit.seconds);
        edit.insert("0");
        assert_eq!(formatted(edit.seconds), "10:45:56");
    }

    #[test]
    fn navigation_commits_units_and_reentry_replaces_only_selected_group() {
        let mut edit = EditState::new(0);
        edit.insert("1");
        assert!(edit.navigate(false));
        edit.insert("2");
        assert!(edit.navigate(true));
        edit.insert("3");
        assert_eq!(formatted(edit.seconds), "03:02:00");
    }

    #[test]
    fn tabs_leave_only_at_endpoints_without_wrapping() {
        let mut edit = EditState::new(0);
        assert!(!edit.navigate(true));
        assert!(edit.navigate(false));
        assert!(edit.navigate(false));
        assert!(!edit.navigate(false));
        assert_eq!(edit.group, 2);
        assert!(edit.navigate(true));
        assert!(edit.navigate(true));
        assert!(!edit.navigate(true));
    }

    #[test]
    fn invalid_second_digit_does_not_corrupt_minutes_or_seconds() {
        for group in [1, 2] {
            let mut edit = EditState::new(0);
            edit.select(group);
            edit.insert("6");
            edit.insert("0");
            assert_eq!(edit.value(), 6);
            edit.insert("x");
            assert_eq!(edit.value(), 6);
        }
    }

    #[test]
    fn full_paste_is_strict_and_segment_paste_is_atomic() {
        let mut edit = EditState::new(0);
        edit.insert("99:59:59");
        assert_eq!(edit.seconds, MAX_SECONDS);
        for invalid in [
            "100:00:00",
            "12:60:00",
            "12:00:60",
            "12-00-00",
            "１２:00:00",
            "",
            "12:00:00\n",
        ] {
            edit.insert(invalid);
            assert_eq!(edit.seconds, MAX_SECONDS, "accepted {invalid:?}");
        }
        edit.select(1);
        edit.insert("60");
        assert_eq!(edit.seconds, MAX_SECONDS);
        edit.insert("10");
        assert_eq!(formatted(edit.seconds), "99:10:59");
    }

    #[test]
    fn multi_character_commit_replaces_partial_entry_atomically() {
        let mut edit = EditState::new(0);
        edit.select(1);
        edit.insert("4");
        edit.insert("60");
        assert_eq!(formatted(edit.seconds), "00:04:00");
        edit.insert("10");
        assert_eq!(formatted(edit.seconds), "00:10:00");
    }

    #[test]
    fn deletion_never_erases_colons_or_other_segments() {
        let mut edit = EditState::new(3600 + 2 * 60 + 3);
        edit.select(1);
        edit.insert("10");
        edit.delete(true);
        assert_eq!(formatted(edit.seconds), "01:01:03");
        edit.delete(true);
        assert_eq!(formatted(edit.seconds), "01:00:03");
        edit.select(2);
        edit.delete(false);
        assert_eq!(formatted(edit.seconds), "01:00:00");
    }

    #[test]
    fn external_reset_discards_partial_digits_and_restarts_hours() {
        let mut edit = EditState::new(0);
        edit.select(1);
        edit.insert("4");
        edit.synchronize(5);
        edit.insert("2");
        assert_eq!(formatted(edit.seconds), "02:00:05");
    }

    #[test]
    fn each_segment_accepts_its_entire_range_without_overflow() {
        for group in 0..3 {
            for value in 0..=if group == 0 { 99 } else { 59 } {
                let mut edit = EditState::new(0);
                edit.select(group);
                edit.digit((value / 10) as u8);
                edit.digit((value % 10) as u8);
                assert_eq!(edit.value(), value);
                assert!(edit.seconds <= MAX_SECONDS);
            }
        }
    }
}
