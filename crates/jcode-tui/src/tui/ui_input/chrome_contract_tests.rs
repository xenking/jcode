use super::*;
use unicode_width::UnicodeWidthStr;

#[test]
fn unicode_branch_is_cell_bounded_and_single_line() {
    use crate::tui::info_widget::{GitInfo, InfoWidgetData};
    let data = InfoWidgetData {
        git_info: Some(GitInfo {
            branch: "工作😀\n\t".repeat(20),
            ..Default::default()
        }),
        ..Default::default()
    };
    let branch = overscroll_git_branch(&data).unwrap();
    assert!(
        branch.width() <= 24,
        "use display-cell truncation: {branch:?}"
    );
    assert!(branch.ends_with('…'), "use the shared ellipsis: {branch:?}");
    assert!(
        !branch.chars().any(char::is_control),
        "sanitize chrome: {branch:?}"
    );
}

#[test]
fn status_rows_fit_every_narrow_cell_budget() {
    for label in ["工作😀", "👩‍💻 e\u{301}", "branch\nwith\ttabs"] {
        for width in 0..80 {
            let fitted = overscroll_truncate_label(&label.repeat(30), width);
            assert!(fitted.width() <= width, "width={width}: {fitted:?}");
            assert!(
                !fitted.chars().any(char::is_control),
                "sanitize chrome: {fitted:?}"
            );
        }
    }
}
