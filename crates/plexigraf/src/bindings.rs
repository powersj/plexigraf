//! Host bindings for the `plexigraf:plugin` worlds. The scraper world generates the
//! shared interfaces; the other worlds reuse them so there is one set of types.

pub mod scraper {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "plexigraf:plugin/scraper",
        imports: { default: trappable },
        with: { "plexigraf:metrics/metrics.batch": crate::model::BatchData },
    });
}

pub mod processor {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "plexigraf:plugin/processor",
        imports: { default: trappable },
        with: {
            "plexigraf:metrics/metrics": crate::bindings::scraper::plexigraf::metrics::metrics,
            "plexigraf:plugin/types": crate::bindings::scraper::plexigraf::plugin::types,
            "plexigraf:plugin/host": crate::bindings::scraper::plexigraf::plugin::host,
            "plexigraf:plugin/encoding": crate::bindings::scraper::plexigraf::plugin::encoding,
        },
    });
}

pub mod exporter {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "plexigraf:plugin/exporter",
        imports: { default: trappable },
        with: {
            "plexigraf:metrics/metrics": crate::bindings::scraper::plexigraf::metrics::metrics,
            "plexigraf:plugin/types": crate::bindings::scraper::plexigraf::plugin::types,
            "plexigraf:plugin/host": crate::bindings::scraper::plexigraf::plugin::host,
            "plexigraf:plugin/encoding": crate::bindings::scraper::plexigraf::plugin::encoding,
        },
    });
}

pub use scraper::plexigraf::metrics::metrics;
pub use scraper::plexigraf::plugin::{encoding, host, types};
