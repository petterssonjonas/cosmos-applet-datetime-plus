// SPDX-License-Identifier: GPL-3.0-only

use std::borrow::Cow;

fn normalized_chars(text: &str) -> impl Iterator<Item = char> + Clone + '_ {
    text.split_whitespace()
        .enumerate()
        .flat_map(|(index, word)| (index > 0).then_some(' ').into_iter().chain(word.chars()))
}

fn preview_text(text: &str, limit: usize, following_text: bool) -> (Cow<'_, str>, usize) {
    if limit == 0 {
        return (Cow::Borrowed(""), 0);
    }
    let text = text.trim();
    let chars = normalized_chars(text);
    let length = chars.clone().take(limit.saturating_add(1)).count();
    let shortened = length > limit || (following_text && length == limit);
    if !shortened && text.chars().eq(chars.clone()) {
        return (Cow::Borrowed(text), length);
    }
    let mut preview: String = chars
        .take(if shortened { limit - 1 } else { length })
        .collect();
    if shortened {
        preview.push('…');
    }
    (Cow::Owned(preview), length.min(limit))
}

pub(crate) fn notification_preview<'a>(
    summary: &'a str,
    body: &'a str,
    limit: usize,
) -> (Cow<'a, str>, Cow<'a, str>) {
    let (summary, used) = preview_text(summary, limit, !body.trim().is_empty());
    let (body, _) = preview_text(body, limit - used, false);
    (summary, body)
}
