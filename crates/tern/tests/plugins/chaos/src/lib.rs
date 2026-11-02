//! A deliberately misbehaving scraper for the integration tests.

use serde::Deserialize;
use tern_sdk::{Batch, Metric, PluginError, Scraper, Timestamp, export_scraper};

#[derive(Deserialize)]
struct Chaos {
    /// `ok`, `spin`, `trap`, `oom`, `error`, `fatal`, `leak`, or `bad_index`.
    mode: String,
    #[serde(skip)]
    calls: u64,
}

impl Scraper for Chaos {
    fn scrape(&mut self, now: Timestamp) -> Result<Batch, PluginError> {
        self.calls += 1;
        match self.mode.as_str() {
            "spin" => loop {
                std::hint::black_box(self.calls);
            },
            "trap" => panic!("chaos: trap"),
            "oom" => {
                let mut v: Vec<Vec<u8>> = Vec::new();
                loop {
                    v.push(vec![1u8; 1 << 20]);
                }
            }
            "error" => Err(PluginError::Transient("chaos: transient".into())),
            "fatal" => Err(PluginError::Fatal("chaos: fatal".into())),
            "leak" => {
                // Batches never returned or dropped must hit the handle cap.
                loop {
                    std::mem::forget(Batch::new());
                }
            }
            "bad_index" => {
                let mut b = Batch::new();
                let mut m: tern_sdk::raw::Metric = Metric::gauge("chaos.bad", "1").point(now, 1, &[]).into();
                m.resource = 7;
                b.push(m);
                Ok(b)
            }
            _ => {
                let mut b = Batch::new();
                b.push(Metric::gauge("chaos.calls", "1").point(
                    now,
                    i64::try_from(self.calls).unwrap_or(i64::MAX),
                    &[],
                ));
                Ok(b)
            }
        }
    }
}

export_scraper!(Chaos);
