use octane_tui::theme::Theme;

/// Syntect's string scope is the same green as `theme.added`, so a removed line
/// containing a string literal used to render part of itself in the additions
/// colour — a deleted line reading as an addition at a glance.
#[test]
fn no_part_of_a_removed_line_renders_in_the_additions_colour() {
    let theme = Theme::default();
    let diff = "--- a/x.rs\n+++ b/x.rs\n@@ -1,2 +1,2 @@\n-let s = \"gone\";\n+let s = \"kept\";\n";

    for line in octane_tui::render::render_diff(diff, &theme) {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        if !text.trim_start().starts_with(|c: char| c.is_ascii_digit()) || !text.contains('-') {
            continue;
        }
        if !text.contains("gone") {
            continue;
        }
        for span in &line.spans {
            assert_ne!(
                span.style.fg,
                Some(theme.added),
                "a removed line must contain no additions colour: {text:?}",
            );
        }
    }
}
