//! Line protocol arrives from the network and from plugins, so it is
//! untrusted. Decoding must never panic, anything that decodes must encode
//! in every format, and its line protocol must decode again.
//!
//! Run with `make fuzz`; crashes land in `fuzz/artifacts/influx_decode/`.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tern::harness;

fuzz_target!(|data: &[u8]| {
    let Ok(batch) = harness::decode("influx", data) else {
        return;
    };
    harness::encode("json", &batch).expect("a decoded batch encodes as JSON");
    let encoded = harness::encode("influx", &batch).expect("a decoded batch encodes");
    if let Err(e) = harness::decode("influx", &encoded) {
        panic!(
            "encoder output does not decode: {e}\n{}",
            String::from_utf8_lossy(&encoded)
        );
    }
});
