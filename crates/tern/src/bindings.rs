//! Host bindings for the `tern:plugin` worlds. The scraper world generates the
//! shared interfaces; the other worlds reuse them so there is one set of types.

pub mod scraper {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "tern:plugin/scraper",
        imports: { default: trappable },
        with: { "tern:metrics/metrics.batch": crate::model::BatchData },
    });
}

pub mod processor {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "tern:plugin/processor",
        imports: { default: trappable },
        with: {
            "tern:metrics/metrics": crate::bindings::scraper::tern::metrics::metrics,
            "tern:plugin/types": crate::bindings::scraper::tern::plugin::types,
            "tern:plugin/host": crate::bindings::scraper::tern::plugin::host,
            "tern:plugin/encoding": crate::bindings::scraper::tern::plugin::encoding,
        },
    });
}

pub mod exporter {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "tern:plugin/exporter",
        imports: { default: trappable },
        with: {
            "tern:metrics/metrics": crate::bindings::scraper::tern::metrics::metrics,
            "tern:plugin/types": crate::bindings::scraper::tern::plugin::types,
            "tern:plugin/host": crate::bindings::scraper::tern::plugin::host,
            "tern:plugin/encoding": crate::bindings::scraper::tern::plugin::encoding,
        },
    });
}

pub use scraper::tern::metrics::metrics;
pub use scraper::tern::plugin::{encoding, host, types};
