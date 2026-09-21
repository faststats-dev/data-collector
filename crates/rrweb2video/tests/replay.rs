use rrweb2video::{FramePlan, Replay};

mod common;

#[test]
fn synthetic_metadata_and_frame_budget() {
    let replay = Replay::from_slice(&common::recording()).unwrap();
    assert_eq!(replay.event_count(), 3);
    assert_eq!((replay.width, replay.height), (320, 240));
    assert_eq!(replay.duration_ms, 3200);
    let plan = FramePlan::new(replay.duration_ms, 5, 8.0).unwrap();
    assert_eq!(plan.frame_count, 3);
    assert_eq!(plan.replay_time_ms(2), 3200.0);
}

#[test]
fn owned_raw_events_use_the_same_validation() {
    let bytes = common::recording();
    let raw = serde_json::from_slice(&bytes).unwrap();
    let replay = Replay::from_events(raw).unwrap();
    assert_eq!(replay.event_count(), 3);
    assert_eq!(
        (replay.width, replay.height, replay.duration_ms),
        (320, 240, 3200)
    );
    assert!(Replay::from_events(Vec::new()).is_err());
    let mut raw: Vec<Box<serde_json::value::RawValue>> = serde_json::from_slice(&bytes).unwrap();
    raw.reverse();
    assert!(Replay::from_events(raw).is_err());
}

#[test]
fn invalid_input_and_settings_are_rejected() {
    for bytes in [b"[]".as_slice(), b"{}", br#"[{"type":4,"timestamp":2,"data":{"width":100,"height":100}},{"type":2,"timestamp":1,"data":{}}]"#, br#"[{"type":4,"timestamp":0,"data":{"width":0,"height":100}},{"type":2,"timestamp":1,"data":{}}]"#] {
        assert!(Replay::from_slice(bytes).is_err());
    }
    assert!(FramePlan::new(10, 0, 1.0).is_err());
    assert!(FramePlan::new(10, 30, f64::NAN).is_err());
    assert!(FramePlan::new(10, 30, 0.0).is_err());
    assert!(FramePlan::new(86_400_001, 30, 1.0).is_err());
}

#[test]
fn frame_schedule_includes_terminal_state_without_drift() {
    let plan = FramePlan::new(1000, 30, 1.0).unwrap();
    assert_eq!(plan.frame_count, 31);
    assert_eq!(plan.replay_time_ms(30), 1000.0);
    assert_eq!(FramePlan::new(0, 10, 8.0).unwrap().frame_count, 1);
}
