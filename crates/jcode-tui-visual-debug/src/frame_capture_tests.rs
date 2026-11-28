use super::*;

struct Probe;

impl FrameProbe for Probe {
    fn state(&self) -> StateSnapshot {
        StateSnapshot {
            status: "Streaming".into(),
            ..Default::default()
        }
    }

    fn rendered(&self) -> RenderedText {
        RenderedText {
            status_line: "Streaming reply".into(),
            ..Default::default()
        }
    }
}

#[test]
fn regular_frame_keeps_probe_data_with_final_geometry_and_display_input() {
    let mut builder = FrameCaptureBuilder::new(80, 24);
    builder.record_frame(&Probe, LayoutCapture::default());
    builder.record_pane_geometry(Rect::new(0, 0, 60, 20), Some(Rect::new(60, 0, 20, 20)));
    builder.record_input("•••", Some("Queue reply"));
    let frame = serde_json::to_value(builder.build()).expect("serializable frame");

    assert_eq!(frame["state"]["status"], "Streaming");
    assert_eq!(frame["layout"]["messages_area"]["width"], 60);
    assert_eq!(frame["layout"]["diagram_area"]["x"], 60);
    assert_eq!(frame["rendered_text"]["status_line"], "Streaming reply");
    assert_eq!(frame["rendered_text"]["input_area"], "•••");
    assert_eq!(frame["rendered_text"]["input_hint"], "Queue reply");
}

#[test]
fn frozen_frame_counts_input_bytes_but_truncates_preview_by_character() {
    let input = "界".repeat(201);
    let mut builder = FrameCaptureBuilder::new(40, 10);
    builder.record_hibernation(&input, 3, 4, 5, "Sleeping");
    let frame = builder.build();

    assert_eq!(frame.state.status, "hibernated");
    assert_eq!(frame.state.input_len, 603);
    assert_eq!(frame.state.input_preview, "界".repeat(200));
    assert_eq!(frame.rendered_text.status_line, "Sleeping");
    assert_eq!(frame.render_order, ["frozen_hibernation_frame"]);
}
