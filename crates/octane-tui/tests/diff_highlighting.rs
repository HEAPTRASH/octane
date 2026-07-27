use octane_tui::theme::Theme;

/// A large diff must not pay for syntax highlighting.
///
/// `ApprovalPane` renders the diff twice per frame — once to measure, once to
/// draw — so an unbounded highlighter put a 400-line diff at roughly half the
/// frame budget. This pins the guard rather than the timing, because a wall
/// clock assertion is flaky on shared CI.
#[test]
fn a_large_diff_is_not_syntax_highlighted() {
    let theme = Theme::default();
    let header = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,400 +1,400 @@\n";

    let small: String = header.to_string()
        + &(0..20).map(|i| format!("+let x{i} = 1;\n")).collect::<String>();
    let large: String = header.to_string()
        + &(0..400).map(|i| format!("+let x{i} = 1;\n")).collect::<String>();

    let spans = |diff: &str| {
        octane_tui::render::render_diff(diff, &theme)
            .iter()
            .map(|line| line.spans.len())
            .max()
            .unwrap_or(0)
    };

    assert!(spans(&small) > 3, "a small diff is highlighted into several spans");
    // Number column plus the content, and no more: unhighlighted.
    assert!(spans(&large) <= 3, "a large diff must skip highlighting");
}
