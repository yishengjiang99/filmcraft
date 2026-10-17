//! Settings ▸ Playback ▸ Hardware decoding drives `filmcraft_codecs::hw`'s process-wide switch.
//! Its own test binary (process): every `Session::set_prefs` and `start_autosave` writes that
//! switch, so in the library's test run a parallel test could flip it between a write here and
//! the check after it.

use filmcraft_engine::Session;
use filmcraft_engine::settings::{self, Kind};
use serde_json::json;

fn set(s: &mut Session, key: &str, value: serde_json::Value) {
    s.execute("prefs.set", json!({"key": key, "value": value})).unwrap_or_else(|e| panic!("{key}: {e}"));
}

#[test]
fn hardware_decoding_setting_drives_the_decoder_switch() {
    let mut s = Session::default();
    assert_eq!(s.prefs.playback.hardware_decoding, "auto", "Auto by default");
    set(&mut s, "playback.hardwareDecoding", json!("off"));
    assert!(!filmcraft_codecs::hw::hardware_decoding(), "Off reaches the decoder registry");
    assert!(s.execute("prefs.set", json!({"key": "playback.hardwareDecoding", "value": "gpu"})).is_err());
    set(&mut s, "playback.hardwareDecoding", json!("auto"));
    assert!(filmcraft_codecs::hw::hardware_decoding());
    assert!(settings::field("playback.hardwareDecoding").is_some_and(|f| f.wired && matches!(f.kind, Kind::Choice(_))));
}
