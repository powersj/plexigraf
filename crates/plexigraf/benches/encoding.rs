//! Encoder throughput. Run with `make bench`; `cargo bench -p plexigraf -- influx`
//! filters by name. Encoding is the agent's hot path, so check these before
//! and after changing `src/encoding/`.

use divan::counter::ItemsCount;
use divan::{Bencher, black_box};
use plexigraf::harness;

fn main() {
    divan::main();
}

const SIZES: &[usize] = &[10, 1_000, 10_000];

#[divan::bench(args = SIZES)]
fn encode_influx(bencher: Bencher, n: usize) {
    let batch = harness::sample_batch(n);
    bencher
        .counter(ItemsCount::new(n))
        .bench(|| harness::encode("influx", black_box(&batch)));
}

#[divan::bench(args = SIZES)]
fn encode_json(bencher: Bencher, n: usize) {
    let batch = harness::sample_batch(n);
    bencher
        .counter(ItemsCount::new(n))
        .bench(|| harness::encode("json", black_box(&batch)));
}

#[divan::bench(args = SIZES)]
fn decode_influx(bencher: Bencher, n: usize) {
    let lines = harness::encode("influx", &harness::sample_batch(n)).unwrap();
    bencher
        .counter(ItemsCount::new(n))
        .bench(|| harness::decode("influx", black_box(&lines)));
}
