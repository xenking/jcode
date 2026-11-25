use super::*;
use crate::tui::CommandSuggestionInfo;
use ratatui::backend::TestBackend;
use ratatui::{Terminal, layout::Rect};
use unicode_width::UnicodeWidthStr;

fn info(description: &str, mode: bool, active: bool) -> CommandSuggestionInfo {
    CommandSuggestionInfo {
        description: description.into(),
        mode,
        active,
    }
}

fn palette(selected: usize) -> (TestState, Vec<(String, String)>) {
    let long = "poteto's agent style for concise, detailed responses, deliberate subagents, \
                unslopped prose, simple code, and verified work. Use for poteto or requests \
                to work in this style.";
    let rows = [
        ("poteto-mode", long, true, true),
        ("selfdev", "Develop the fork", true, false),
        ("swarm", "Coordinate\nworkers", true, false),
        (
            "clickhouse-architecture-advisor",
            "Design ClickHouse schemas",
            false,
            false,
        ),
        ("go-review", long, false, false),
    ];
    let mut state = TestState {
        input: "/mode ".into(),
        palette_selected: selected,
        ..Default::default()
    };
    let mut described = Vec::new();
    for (name, description, mode, active) in rows {
        let command = format!("/mode toggle {name}");
        state
            .palette_rows
            .push((command.clone(), "Toggle session mode"));
        state
            .palette_info
            .insert(command, info(description, mode, active));
        described.push((name.to_string(), description.to_string()));
    }
    state
        .palette_rows
        .push(("/mode off".into(), "Clear all active modes"));
    (state, described)
}

fn render_palette(state: &TestState, width: u16) -> Vec<String> {
    let height = 12;
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            let input = Rect::new(0, 1, width, 1);
            crate::tui::ui::input_ui::draw_command_suggestions_overlay(frame, state, input);
        })
        .unwrap();
    let buf = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .filter(|line| !line.is_empty())
        .collect()
}

/// Display cell where `needle` starts in `line`.
fn cell_of(line: &str, needle: &str) -> Option<usize> {
    line.find(needle).map(|byte| line[..byte].width())
}

#[test]
fn skill_rows_form_one_aligned_grid_at_every_width() {
    // The first word of each row's description, after its mode tag.
    let first_words = ["active mode", "mode", "mode", "Design", "poteto's"];
    for width in [48u16, 80, 120] {
        for selected in [0, 3] {
            let (state, _) = palette(selected);
            let lines = render_palette(&state, width);
            assert_eq!(lines.len(), 6, "one visual row per suggestion: {lines:#?}");
            for line in &lines {
                assert!(line.width() <= width as usize, "{width}: {line:?}");
            }
            let starts: Vec<usize> = first_words
                .iter()
                .enumerate()
                .map(|(row, word)| {
                    cell_of(&lines[row], word)
                        .unwrap_or_else(|| panic!("{word:?} missing in {:?}", lines[row]))
                })
                .collect();
            assert!(
                starts.windows(2).all(|pair| pair[0] == pair[1]),
                "descriptions start at different cells at width {width} (selected {selected}): {starts:?}\n{lines:#?}"
            );
        }
    }
}

#[test]
fn long_descriptions_end_in_an_ellipsis_inside_the_row() {
    let (state, _) = palette(0);
    let lines = render_palette(&state, 60);
    let poteto = &lines[0];
    assert!(poteto.ends_with('…'), "{poteto:?}");
    assert!(poteto.width() <= 60, "{poteto:?}");
    assert!(poteto.contains("active mode · poteto's"), "{poteto:?}");
}

#[test]
fn modes_are_marked_and_the_active_one_is_distinct() {
    let (state, _) = palette(1);
    let lines = render_palette(&state, 100);
    assert!(lines[0].starts_with("● poteto-mode"), "{:?}", lines[0]);
    assert!(lines[1].starts_with("○ selfdev"), "{:?}", lines[1]);
    assert!(
        lines[1].contains("mode · Develop the fork"),
        "{:?}",
        lines[1]
    );
    // A plain skill has no state marker or mode tag.
    assert!(
        lines[3].starts_with("  clickhouse-architecture-advisor"),
        "{:?}",
        lines[3]
    );
    assert!(!lines[3].contains("mode ·"), "{:?}", lines[3]);
    // Multi-line descriptions collapse onto the row.
    assert!(lines[2].contains("Coordinate workers"), "{:?}", lines[2]);
}

#[test]
fn a_wide_name_never_pushes_the_description_off_a_narrow_row() {
    let (state, _) = palette(3);
    let lines = render_palette(&state, 40);
    let wide = &lines[3];
    assert!(wide.width() <= 40, "{wide:?}");
    assert!(
        wide.contains("Design"),
        "description kept visible: {wide:?}"
    );
}
