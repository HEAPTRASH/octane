//! A selection overlay.
//!
//! Generic rather than a `/connect` screen, because the same shape serves
//! choosing a provider, a model, an agent, or a setting — and three bespoke
//! overlays would drift apart in their keys within a week.
//!
//! Pure state: items in, a selection out. The rendering and the keys live
//! elsewhere, so every rule below is testable by calling a method.

/// What a selection is for, so the caller knows what to do with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Provider,
    Model,
    Setting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerItem {
    /// Returned on selection.
    pub key: String,
    pub label: String,
    pub detail: String,
    /// Right-hand status, e.g. "needs OPENROUTER_API_KEY".
    pub state: Option<String>,
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
            enabled: true,
        }
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
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

    pub fn selected(&self) -> usize {
        self.selected
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

    /// Why the highlighted item cannot be chosen, if it cannot.
    pub fn blocked_reason(&self) -> Option<&str> {
        let matches = self.matches();
        let item = matches.get(self.selected)?;
        (!item.enabled).then_some(item.state.as_deref()).flatten()
    }
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
        assert_eq!(picker.blocked_reason(), Some("no credits"));

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
}
