//! Streamer frames come off the network: decoding one, and its payloads,
//! must never panic, whatever arrives.
#![no_main]
use libfuzzer_sys::fuzz_target;
use rustyschwab_types::streaming::StreamMessage;

fuzz_target!(|data: &[u8]| {
    if let Ok(StreamMessage::Data(frame)) = serde_json::from_slice::<StreamMessage>(data) {
        for item in &frame.data {
            let _ = item.decode();
            let table = item
                .service()
                .map_or(&[][..], rustyschwab_types::streaming::StreamService::fields);
            for content in item.content.iter().cloned() {
                let _ = content.into_named_value(table);
            }
        }
    }
});
