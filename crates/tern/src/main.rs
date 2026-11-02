//! Tern: a pluggable, standards-based metrics agent.

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
#[command(name = "tern", version, about = "A pluggable, standards-based metrics agent")]
struct Cli {
    /// Log filter, such as `info` or `tern=debug` (also `TERN_LOG`).
    #[arg(long, global = true, env = "TERN_LOG")]
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    let filter = cli.log.clone().unwrap_or_else(|| "info".into());
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_new(&filter).unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
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
            println!("world:        tern:plugin/{}", p.world.as_str());
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
