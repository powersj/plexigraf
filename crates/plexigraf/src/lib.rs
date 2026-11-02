//! Plexigraf: a pluggable, standards-based metrics agent.
//!
//! The binary is a thin wrapper around [`main`]. The library exists so fuzz
//! targets and benchmarks can reach the encoders; it has no stable API.

mod bindings;
mod config;
mod encoding;
mod health;
mod model;
mod plugin;
mod runtime;
mod telemetry;
mod util;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::config::Kind;
use crate::plugin::{HostEngine, Plugin, World};

#[derive(Parser)]
#[command(name = "plexigraf", version, about = "A pluggable, standards-based metrics agent")]
struct Cli {
    /// Log filter, such as `info` or `plexigraf=debug` (also `PLEXIGRAF_LOG`).
    #[arg(long, global = true, env = "PLEXIGRAF_LOG")]
    log: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct ConfigArgs {
    /// Config file; may be repeated.
    #[arg(short, long = "config")]
    config: Vec<PathBuf>,
    /// Directory whose *.toml files are merged into one config.
    #[arg(long = "config-dir")]
    config_dir: Vec<PathBuf>,
}

impl ConfigArgs {
    fn load(&self) -> Result<config::Config> {
        config::load(&config::collect_files(&self.config, &self.config_dir)?)
    }
}

#[derive(Subcommand)]
enum Command {
    /// Load plugins and run every pipeline until interrupted.
    Run {
        #[command(flatten)]
        config: ConfigArgs,
        /// Fail if any component fails to start, instead of isolating it.
        #[arg(long)]
        strict: bool,
    },
    /// Run every scraper once, deliver the results, and exit.
    Once {
        #[command(flatten)]
        config: ConfigArgs,
    },
    /// Work with configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Work with plugins.
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Validate config and plugin settings without running anything.
    Check {
        #[command(flatten)]
        config: ConfigArgs,
    },
}

#[derive(Subcommand)]
enum PluginCommand {
    /// Print a plugin's manifest, world, and requested capabilities.
    Inspect { path: PathBuf },
}

/// Run the `plexigraf` command line: parse arguments, set up logging, and run the
/// chosen command.
#[must_use]
pub fn main() -> ExitCode {
    let cli = Cli::parse();
    let filter = cli.log.clone().unwrap_or_else(|| "info".into());
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_new(&filter).unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    install_panic_hook();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Send panics to the log with their thread and location, instead of the
/// default hook's bare stderr line. Panics still unwind: a panic in a pipeline
/// task ends that task, not the agent. Set `RUST_BACKTRACE=1` for a backtrace.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        let location = info.location().map(ToString::to_string).unwrap_or_default();
        let message = info.payload_as_str().unwrap_or("non-string panic payload");
        let backtrace = std::backtrace::Backtrace::capture();
        tracing::error!(thread = thread.name().unwrap_or("unnamed"), %location, "panic: {message}\n{backtrace}");
    }));
}

fn run(cli: Cli) -> Result<()> {
    let rt = || tokio::runtime::Builder::new_multi_thread().enable_all().build();
    match cli.command {
        Command::Run { config, strict } => {
            let cfg = config.load()?;
            log_flow(&cfg);
            rt()?.block_on(runtime::run(cfg, runtime::Mode::Run, strict))
        }
        Command::Once { config } => {
            let cfg = config.load()?;
            rt()?.block_on(runtime::run(cfg, runtime::Mode::Once, false))
        }
        Command::Config {
            command: ConfigCommand::Check { config },
        } => check(&config.load()?),
        Command::Plugin {
            command: PluginCommand::Inspect { path },
        } => inspect(&path),
    }
}

fn log_flow(cfg: &config::Config) {
    for line in cfg.describe().lines() {
        tracing::info!("{line}");
    }
}

fn check(cfg: &config::Config) -> Result<()> {
    print!("{}", cfg.describe());
    let host = HostEngine::new()?;
    let (plugins, failed) = plugin::load_plugins(&host, cfg);
    let mut problems: Vec<String> = failed.into_iter().map(|(n, e)| format!("plugin {n}: {e}")).collect();
    for c in cfg.active_components() {
        let Some(p) = plugins.get(&c.plugin) else { continue };
        let want = match c.kind {
            Kind::Receiver => World::Scraper,
            Kind::Processor => World::Processor,
            Kind::Exporter => World::Exporter,
            _ => continue,
        };
        if p.world != want {
            problems.push(format!(
                "[{}]: plugin {} is a {}, not a {}",
                c.label(),
                c.plugin,
                p.world.as_str(),
                want.as_str()
            ));
            continue;
        }
        let toml = toml::to_string(&c.settings)?;
        if let Err(e) = plugin::validate_config(Arc::clone(p), &c.label(), toml) {
            problems.push(format!("[{}]: {e}", c.label()));
        }
    }
    if problems.is_empty() {
        println!("config ok");
        Ok(())
    } else {
        bail!("{} problem(s):\n  {}", problems.len(), problems.join("\n  "))
    }
}

fn inspect(path: &std::path::Path) -> Result<()> {
    let host = HostEngine::new()?;
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("plugin");
    let (manifest, manifest_path) = plugin::Manifest::find(path)?;
    let requested = manifest.requested_capabilities();
    println!("file:         {}", path.display());
    println!(
        "manifest:     {}",
        manifest_path.map_or("(none)".into(), |p| p.display().to_string())
    );
    println!("name:         {}", manifest.name.as_deref().unwrap_or(stem));
    println!("version:      {}", manifest.version.as_deref().unwrap_or("(unset)"));
    if let Some(d) = &manifest.description {
        println!("description:  {d}");
    }
    println!(
        "capabilities: {}",
        if requested.is_empty() {
            "none (clocks and randomness only)".to_string()
        } else {
            requested.join(", ")
        }
    );
    match Plugin::load(&host, stem, path, None) {
        Ok(p) => {
            println!("world:        plexigraf:plugin/{}", p.world.as_str());
            println!("memory limit: {} MiB", p.memory_limit >> 20);
            if p.manifest.flush {
                println!("flush:        yes");
            }
        }
        Err(e) => println!("load error:   {e:#}"),
    }
    if let Some(sample) = &manifest.sample_config {
        println!("\nsample config:\n{sample}");
    }
    Ok(())
}

/// Entry points for the fuzz targets in `fuzz/` and the benchmarks in
/// `benches/`. Not a stable API: it changes whenever they need it to.
#[doc(hidden)]
pub mod harness {
    use crate::bindings::metrics::{
        Data, DataPointFlags, Gauge, InstrumentationScope, Metric, Number, NumberDataPoint,
    };
    use crate::model::{BatchData, empty_resource, kv_str};

    /// A batch of metrics, opaque outside the crate.
    pub struct Batch(BatchData);

    impl Batch {
        /// The number of metrics in the batch.
        #[must_use]
        pub fn len(&self) -> usize {
            self.0.metrics.len()
        }

        /// Whether the batch has no metrics.
        #[must_use]
        pub fn is_empty(&self) -> bool {
            self.0.metrics.is_empty()
        }
    }

    /// Decode bytes with a built-in format and default options.
    pub fn decode(format: &str, data: &[u8]) -> Result<Batch, String> {
        crate::encoding::decode(format, &toml::Table::new(), data).map(Batch)
    }

    /// Encode a batch with a built-in format and default options.
    pub fn encode(format: &str, batch: &Batch) -> Result<Vec<u8>, String> {
        crate::encoding::encode(format, &toml::Table::new(), &batch.0)
    }

    /// A batch of `n` gauges with two point attributes each, shaped like a
    /// typical host-metrics scrape.
    #[must_use]
    pub fn sample_batch(n: usize) -> Batch {
        let metrics = (0..n)
            .map(|i| Metric {
                resource: 0,
                scope: 0,
                name: format!("system.cpu.time.{i}"),
                description: String::new(),
                unit: "s".into(),
                #[expect(clippy::cast_precision_loss, reason = "sample values only")]
                data: Data::Gauge(Gauge {
                    data_points: vec![NumberDataPoint {
                        attributes: vec![kv_str("cpu", &format!("cpu{}", i % 8)), kv_str("state", "user")],
                        start_time_unix_nano: 0,
                        time_unix_nano: 1_761_955_200_000_000_000,
                        value: Number::Double(i as f64 * 0.5),
                        exemplars: vec![],
                        flags: DataPointFlags::empty(),
                    }],
                }),
                metadata: vec![],
            })
            .collect();
        let scope = InstrumentationScope {
            name: "plexigraf.harness".into(),
            version: String::new(),
            attributes: vec![],
            dropped_attributes_count: 0,
            schema_url: String::new(),
        };
        Batch(BatchData {
            resources: vec![empty_resource()],
            scopes: vec![scope],
            metrics,
        })
    }
}
