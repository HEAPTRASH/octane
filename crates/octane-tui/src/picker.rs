//! A selection overlay.
//!
//! Generic rather than a `/connect` screen, because the same shape serves
//! choosing a provider, a model, an agent, or a setting — and three bespoke
//! overlays would drift apart in their keys within a week.
//!
//! Pure state: items in, a selection out. The rendering and the keys live
//! elsewhere, so every rule below is testable by calling a method.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

/// What a selection is for, so the caller knows what to do with it.
///
/// `SettingValue` carries the setting being edited rather than leaving the
/// caller to remember it between two picks. A second picker opened from the
/// first is the one case where the selection alone is ambiguous — `"true"`
/// means nothing without knowing which switch it was for — and ambient state
/// that has to stay in step with a modal overlay is state that will not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerKind {
    Provider,
    Model,
    /// Which recorded session to resume.
    Session,
    /// Choosing which setting to change.
    Setting,
    /// Choosing the value for the named setting.
    SettingValue(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerItem {
    /// Returned on selection.
    pub key: String,
    pub label: String,
    pub detail: String,
    /// Right-hand status, e.g. "needs OPENROUTER_API_KEY".
    pub state: Option<String>,
    /// Marks the value currently in effect, for a list of alternatives.
    ///
    /// A glyph rather than the word "current" in the state column: which one is
    /// set should be legible with every colour off and without reading prose,
    /// and a radio column answers "what are my options" at a glance.
    pub selected_value: Option<bool>,
    /// A disabled item is shown but cannot be chosen — it is usually the one
    /// the user most needs to see, with the reason next to it.
    pub enabled: bool,
}

impl PickerItem {
    pub fn new(key: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            detail: String::new(),
            state: None,
            selected_value: None,
            enabled: true,
        }
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    /// Mark this row as the value in effect, or as an alternative to it.
    pub fn radio(mut self, chosen: bool) -> Self {
        self.selected_value = Some(chosen);
        self
    }

    pub fn state(mut self, state: impl Into<String>) -> Self {
        self.state = Some(state.into());
        self
    }
}

/// Rows shown at once.
pub const MAX_VISIBLE: usize = 10;

#[derive(Debug, Clone)]
pub struct Picker {
    pub kind: PickerKind,
    pub title: String,
    items: Vec<PickerItem>,
    /// Typed filter.
    filter: String,
    /// Index into the *filtered* list.
    selected: usize,
}

impl Picker {
    pub fn new(kind: PickerKind, title: impl Into<String>, items: Vec<PickerItem>) -> Self {
        Self { kind, title: title.into(), items, filter: String::new(), selected: 0 }
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Items matching the current filter.
    ///
    /// Substring rather than prefix, so `router` finds `openrouter` — the same
    /// reasoning as file completion.
    pub fn matches(&self) -> Vec<&PickerItem> {
        if self.filter.is_empty() {
            return self.items.iter().collect();
        }
        let needle = self.filter.to_lowercase();
        self.items
            .iter()
            .filter(|item| {
                item.key.to_lowercase().contains(&needle)
                    || item.label.to_lowercase().contains(&needle)
            })
            .collect()
    }

    /// Where the filter matched inside a row's label, as a char range.
    ///
    /// A subsequence match looks like a bug without this: nothing explains why
    /// `router` returned `OpenRouter`. Matching is `contains`, so every hit is
    /// one contiguous range and this is recoverable in the same pass rather
    /// than needing an index set.
    ///
    /// `None` when the filter matched the key but not the visible label, since
    /// there is nothing on screen to underline.
    pub fn match_span(&self, item: &PickerItem) -> Option<(usize, usize)> {
        if self.filter.is_empty() {
            return None;
        }
        let needle = self.filter.to_lowercase();
        let haystack = item.label.to_lowercase();
        let byte_start = haystack.find(&needle)?;
        // Char offsets, because the renderer slices by character.
        let start = haystack[..byte_start].chars().count();
        Some((start, needle.chars().count()))
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Rows before filtering, for the `matched/total` ratio.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.matches().is_empty()
    }

    /// The rows to draw, and where the highlight sits among them.
    pub fn visible(&self) -> (Vec<&PickerItem>, usize) {
        let matches = self.matches();
        if matches.len() <= MAX_VISIBLE {
            return (matches, self.selected);
        }
        let start = self
            .selected
            .saturating_sub(MAX_VISIBLE - 1)
            .min(matches.len() - MAX_VISIBLE);
        (matches[start..start + MAX_VISIBLE].to_vec(), self.selected - start)
    }

    pub fn select_next(&mut self) {
        let count = self.matches().len();
        if count > 0 {
            self.selected = (self.selected + 1) % count;
        }
    }

    pub fn select_previous(&mut self) {
        let count = self.matches().len();
        if count > 0 {
            self.selected = if self.selected == 0 { count - 1 } else { self.selected - 1 };
        }
    }

    pub fn push_filter(&mut self, ch: char) {
        self.filter.push(ch);
        // The old index almost certainly points at something else now.
        self.selected = 0;
    }

    /// Drop the whole filter, restoring the full list.
    ///
    /// Distinct from repeated `pop_filter`: Esc undoes the narrowing in one
    /// step, which is what makes it safe to reach for.
    pub fn clear_filter(&mut self) {
        self.filter.clear();
        self.selected = 0;
    }

    pub fn pop_filter(&mut self) {
        self.filter.pop();
        self.selected = 0;
    }

    /// The highlighted item's key, if it can be chosen.
    ///
    /// `None` for a disabled row rather than silently selecting the next one:
    /// picking something the user did not point at is worse than doing nothing.
    pub fn choose(&self) -> Option<&str> {
        let matches = self.matches();
        let item = matches.get(self.selected)?;
        item.enabled.then_some(item.key.as_str())
    }

}

/// The picker, and the rows it needs including its border.
///
/// The height comes from the lines actually built, because a second count kept
/// in step by hand drifts: undercounting clips the hint line, which is the line
/// that says what Esc will do, and overcounting leaves a dead row in the box.
pub fn widget<'a>(
    stack: &[Picker],
    theme: &crate::theme::Theme,
    glyphs: &crate::glyphs::Glyphs,
) -> (Paragraph<'a>, u16) {
    let Some(picker) = stack.last() else { return (Paragraph::new(Vec::<Line>::new()), 0) };
    let (visible, highlight) = picker.visible();
    let total = picker.matches().len();

    // Padded past the longest label rather than to a guessed width, or a long
    // one runs straight into its state with no gap — `Ollama (local)ready`.
    let column = visible
        .iter()
        .map(|item| item.label.chars().count())
        .max()
        .unwrap_or(0)
        + 2;

    // The ratio is what makes a short list self-explaining: without a
    // denominator, "one result" and "one result out of two hundred" look the
    // same, and a filter that excluded everything looks like an empty menu.
    let ratio = format!("{total}/{}", picker.len());
    let head = if picker.filter().is_empty() {
        "type to filter".to_string()
    } else {
        format!("{} {}", glyphs.prompt, picker.filter())
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(head, theme.dim()),
        Span::styled(format!("   {ratio}"), theme.dim()),
    ])];
    lines.push(Line::default());

    if visible.is_empty() {
        // Echoes the query, because the usual cause is a typo the user cannot
        // see from the result, and names the way out.
        lines.push(Line::styled(
            format!("  nothing matches {:?}", picker.filter()),
            theme.dim(),
        ));
        lines.push(Line::styled(
            "  backspace to narrow less, esc to clear the filter",
            theme.dim(),
        ));
    }

    for (index, item) in visible.iter().enumerate() {
        let selected = index == highlight;
        let marker = if selected { glyphs.edit } else { " " };

        let label_style = if !item.enabled {
            theme.dim()
        } else if selected {
            theme.signal(theme.accent)
        } else {
            Style::default().fg(theme.assistant)
        };

        // Underlined, not bold: the selected row is already bold, so a bold
        // span inside it is invisible on exactly the row being looked at.
        let label = {
            let padded = format!("{:<column$}", item.label);
            match picker.match_span(item) {
                Some((start, len)) => {
                    let chars: Vec<char> = padded.chars().collect();
                    let cut = |a: usize, b: usize| chars[a.min(chars.len())..b.min(chars.len())]
                        .iter()
                        .collect::<String>();
                    vec![
                        Span::styled(cut(0, start), label_style),
                        Span::styled(
                            cut(start, start + len),
                            label_style.add_modifier(ratatui::style::Modifier::UNDERLINED),
                        ),
                        Span::styled(cut(start + len, chars.len()), label_style),
                    ]
                }
                None => vec![Span::styled(padded, label_style)],
            }
        };

        let marker_style =
            if selected && item.enabled { theme.signal(theme.accent) } else { theme.accent() };
        let mut spans = vec![Span::styled(format!("{marker} "), marker_style)];
        if let Some(chosen) = item.selected_value {
            let glyph = if chosen { glyphs.radio_on } else { glyphs.radio_off };
            spans.push(Span::styled(
                format!("{glyph} "),
                if selected && item.enabled {
                    theme.signal(theme.accent)
                } else if chosen {
                    theme.accent()
                } else {
                    theme.dim()
                },
            ));
        }
        spans.extend(label);
        if let Some(state) = &item.state {
            // The state is why a row is or is not usable, so it earns colour.
            let style = if selected && item.enabled {
                theme.signal(theme.accent)
            } else if item.enabled {
                theme.dim()
            } else {
                theme.label(theme.warning)
            };
            spans.push(Span::styled(state.clone(), style));
        }
        let row = Line::from(spans);
        lines.push(if selected && item.enabled {
            row.style(theme.signal(theme.accent))
        } else {
            row
        });

        // Under the highlight only. Every row would triple the box height and
        // bury the list the detail is meant to explain.
        if selected && !item.detail.is_empty() {
            lines.push(Line::styled(format!("      {}", item.detail), theme.dim()));
        }
    }

    // `visible()` windows to MAX_VISIBLE and says nothing about the remainder,
    // so a long list looked complete. The ratio above counts matches, not rows
    // on screen.
    let hidden = total.saturating_sub(visible.len());
    if hidden > 0 {
        lines.push(Line::styled(format!("  [+ {hidden} more]"), theme.dim()));
    }

    lines.push(Line::default());
    // Names what Esc will actually do, which now depends on both the filter
    // and the depth. A fixed "esc cancel" was a lie at every level but one.
    let escape = if !picker.filter().is_empty() {
        "esc clear filter".to_string()
    } else if stack.len() > 1 {
        match stack.get(stack.len() - 2) {
            Some(parent) => format!("esc back to {}", parent.title),
            None => "esc back".to_string(),
        }
    } else {
        "esc close".to_string()
    };
    lines.push(Line::styled(
        format!(
            "  {}{} move   enter choose   {escape}",
            glyphs.arrow_up, glyphs.arrow_down
        ),
        theme.dim(),
    ));

    // The trail is built from the stack itself, so it cannot drift from where
    // the user actually is. Ancestors are dimmed and only the active level is
    // emphasised, which keeps the distinction under NO_COLOR: `theme.label`
    // falls back to BOLD when there is no colour to use.
    let mut title = vec![Span::styled(" SELECT / ", theme.accent())];
    for (index, level) in stack.iter().enumerate() {
        if index > 0 {
            title.push(Span::styled(format!(" {} ", glyphs.prompt), theme.dim()));
        }
        let last = index + 1 == stack.len();
        title.push(Span::styled(
            level.title.clone(),
            if last { theme.label(theme.accent) } else { theme.dim() },
        ));
    }
    title.push(Span::raw(" "));

    let height = u16::try_from(lines.len()).unwrap_or(14) + 2; // borders
    let widget = Paragraph::new(lines).style(theme.panel()).block(
        Block::default()
            .style(theme.panel())
            .borders(Borders::ALL)
            .border_style(theme.dim())
            .title(Line::from(title)),
    );
    (widget, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker() -> Picker {
        Picker::new(
            PickerKind::Provider,
            "Connect a provider",
            vec![
                PickerItem::new("anthropic", "Anthropic").state("needs ANTHROPIC_API_KEY"),
                PickerItem::new("openrouter", "OpenRouter").state("ready"),
                PickerItem::new("ollama", "Ollama (local)").state("ready"),
                PickerItem::new("groq", "Groq").state("needs GROQ_API_KEY"),
            ],
        )
    }

    #[test]
    fn everything_is_shown_before_filtering() {
        assert_eq!(picker().matches().len(), 4);
    }

    #[test]
    fn filtering_is_substring_not_prefix() {
        // `router` should find `openrouter`, same as file completion.
        let mut picker = picker();
        for ch in "router".chars() {
            picker.push_filter(ch);
        }
        assert_eq!(picker.matches().len(), 1);
        assert_eq!(picker.choose(), Some("openrouter"));
    }

    #[test]
    fn filtering_is_case_insensitive() {
        let mut picker = picker();
        for ch in "OLLAMA".chars() {
            picker.push_filter(ch);
        }
        assert_eq!(picker.choose(), Some("ollama"));
    }

    #[test]
    fn filtering_resets_the_highlight() {
        // The old index almost certainly points at something else now.
        let mut picker = picker();
        picker.select_next();
        picker.select_next();
        assert_eq!(picker.selected(), 2);

        picker.push_filter('g');
        assert_eq!(picker.selected(), 0);
    }

    #[test]
    fn backspace_widens_the_filter_again() {
        let mut picker = picker();
        for ch in "groqx".chars() {
            picker.push_filter(ch);
        }
        assert!(picker.is_empty());

        picker.pop_filter();
        assert_eq!(picker.choose(), Some("groq"));
    }

    #[test]
    fn selection_wraps_in_both_directions() {
        let mut picker = picker();
        picker.select_previous();
        assert_eq!(picker.selected(), 3);
        picker.select_next();
        assert_eq!(picker.selected(), 0);
    }

    #[test]
    fn a_disabled_row_cannot_be_chosen() {
        // Selecting the next one instead would pick something the user did not
        // point at, which is worse than doing nothing.
        let mut picker = Picker::new(
            PickerKind::Model,
            "Pick a model",
            vec![
                PickerItem { enabled: false, ..PickerItem::new("locked", "Locked").state("no credits") },
                PickerItem::new("open", "Open"),
            ],
        );
        assert_eq!(picker.choose(), None);

        picker.select_next();
        assert_eq!(picker.choose(), Some("open"));
    }

    #[test]
    fn an_empty_result_chooses_nothing_rather_than_panicking() {
        let mut picker = picker();
        for ch in "zzzz".chars() {
            picker.push_filter(ch);
        }
        assert!(picker.is_empty());
        assert_eq!(picker.choose(), None);
        // Moving around an empty list must be harmless.
        picker.select_next();
        picker.select_previous();
        assert_eq!(picker.choose(), None);
    }

    #[test]
    fn the_visible_window_follows_the_selection() {
        let items: Vec<PickerItem> =
            (0..30).map(|i| PickerItem::new(format!("k{i}"), format!("Item {i}"))).collect();
        let mut picker = Picker::new(PickerKind::Setting, "Many", items);

        for _ in 0..15 {
            picker.select_next();
        }
        let (visible, highlight) = picker.visible();
        assert_eq!(visible.len(), MAX_VISIBLE);
        assert!(highlight < MAX_VISIBLE, "the highlight must stay in the window");
    }

    #[test]
    fn clearing_the_filter_restores_every_row() {
        // One step, not a run of backspaces: Esc undoes the whole narrowing,
        // which is what makes it safe to reach for after a mistyped query.
        let mut picker = picker();
        for ch in "groq".chars() {
            picker.push_filter(ch);
        }
        assert_eq!(picker.matches().len(), 1);

        picker.clear_filter();
        assert_eq!(picker.matches().len(), picker.len());
        assert_eq!(picker.selected(), 0);
        assert!(picker.filter().is_empty());
    }

    #[test]
    fn the_total_is_the_unfiltered_count() {
        // The ratio's denominator. Without it a one-row result and a one-row
        // menu look identical.
        let mut picker = picker();
        let total = picker.len();
        for ch in "groq".chars() {
            picker.push_filter(ch);
        }
        assert_eq!(picker.len(), total, "filtering must not change the total");
        assert_eq!(picker.matches().len(), 1);
    }

    #[test]
    fn the_match_span_explains_a_surprising_hit() {
        // `router` returning `OpenRouter` looks like a bug until the matched
        // characters are marked. The span is over the LABEL, since that is what
        // is on screen.
        let mut picker = picker();
        for ch in "router".chars() {
            picker.push_filter(ch);
        }
        let item = picker.matches()[0];
        assert_eq!(item.label, "OpenRouter");
        assert_eq!(picker.match_span(item), Some((4, 6)));
    }

    #[test]
    fn a_key_only_match_underlines_nothing() {
        // Filtering matches key or label. When only the key matched there is
        // nothing visible to mark, and inventing a span would point at the
        // wrong characters.
        let picker = Picker::new(
            PickerKind::Provider,
            "Pick",
            vec![PickerItem::new("openrouter", "A Different Label")],
        );
        let mut picker = picker;
        for ch in "router".chars() {
            picker.push_filter(ch);
        }
        assert_eq!(picker.matches().len(), 1, "it still matches on the key");
        assert_eq!(picker.match_span(picker.matches()[0]), None);
    }

    #[test]
    fn the_span_is_measured_in_characters_not_bytes() {
        // The renderer slices by character, so a byte offset would cut a
        // multi-byte label in the wrong place or panic.
        let mut picker = Picker::new(
            PickerKind::Provider,
            "Pick",
            vec![PickerItem::new("k", "ünicode match")],
        );
        for ch in "match".chars() {
            picker.push_filter(ch);
        }
        assert_eq!(picker.match_span(picker.matches()[0]), Some((8, 5)));
    }
}
