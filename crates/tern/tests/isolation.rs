//! End-to-end checks: metrics flow through every plugin world and the built-in
//! connector, and a misbehaving plugin is contained: it is interrupted,
//! capped, or stopped while every other component keeps running.
//!
//! The tests build the plugins in `tests/plugins` for wasm32-wasip2, so they
//! need that Rust target (`rustup target add wasm32-wasip2`). Without it the
//! tests are skipped.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Build a test plugin and stage it, with its manifest, in a temp directory.
fn build(name: &str) -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/plugins").join(name);
    let status = Command::new(env!("CARGO"))
        .args(["build", "--quiet", "--release", "--target", "wasm32-wasip2"])
        .current_dir(&dir)
        .status()
        .ok()?;
    if !status.success() {
        eprintln!("skipping: could not build tests/plugins/{name} for wasm32-wasip2");
        return None;
    }
    // Tests run in parallel, so each build gets its own staging directory.
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let staged = std::env::temp_dir().join(format!("tern-plugins-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&staged).unwrap();
    let wasm = staged.join(format!("{name}.wasm"));
    let built = dir.join(format!("target/wasm32-wasip2/release/tern_test_{name}.wasm"));
    std::fs::copy(built, &wasm).unwrap();
    if dir.join("tern-plugin.toml").exists() {
        std::fs::copy(
            dir.join("tern-plugin.toml"),
            staged.join(format!("{name}.tern-plugin.toml")),
        )
        .unwrap();
    }
    Some(wasm)
}

fn run_once(config: &str) -> (bool, String, String) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("tern-test-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tern.toml");
    std::fs::write(&path, config).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tern"))
        .args(["--log", "warn", "once", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn misbehaving_plugins_are_isolated() {
    let Some(wasm) = build("chaos") else { return };
    let mut config = format!(
        r#"
        [plugins.chaos]
        source = "file://{}"
        memory_limit_mib = 16

        [exporters.out]
        plugin = "debug"

        [pipelines.metrics.all]
        receivers = ["chaos_*"]
        exporters = ["out"]
        "#,
        wasm.display()
    );
    for mode in ["ok", "spin", "trap", "oom", "error", "fatal", "leak", "bad_index"] {
        config.push_str(&format!(
            "\n[receivers.chaos_{mode}]\nplugin = \"chaos\"\nmode = \"{mode}\"\ntimeout = \"300ms\"\n"
        ));
    }
    // Bad settings fail at start, before any scrape.
    config.push_str("\n[receivers.chaos_noconfig]\nplugin = \"chaos\"\n");

    let (_, stdout, stderr) = run_once(&config);
    assert!(
        stdout.contains("chaos.calls value=1i"),
        "the healthy scraper still delivers\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let expect = [
        ("spin", "overran its 300ms deadline"),
        ("trap", "chaos: trap"),
        ("oom", "receivers.chaos_oom"),
        ("error", "chaos: transient"),
        ("fatal", "chaos: fatal"),
        ("leak", "more than 1024 batches"),
        ("bad_index", "references resource 7"),
        ("noconfig", "missing field `mode`"),
    ];
    for (mode, needle) in expect {
        assert!(
            stderr.contains(needle),
            "{mode}: expected {needle:?} in stderr:\n{stderr}"
        );
    }
    assert_eq!(stdout.lines().count(), 1, "only the healthy scraper writes:\n{stdout}");
}

#[test]
fn config_check_reports_plugin_validation() {
    let Some(wasm) = build("chaos") else { return };
    let dir = std::env::temp_dir().join(format!("tern-check-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tern.toml");
    std::fs::write(
        &path,
        format!(
            r#"
            [plugins.chaos]
            source = "file://{}"
            [receivers.bad]
            plugin = "chaos"
            unknown = 1
            [exporters.out]
            plugin = "debug"
            [pipelines.metrics.p]
            receivers = ["bad"]
            exporters = ["out"]
            "#,
            wasm.display()
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tern"))
        .args(["--log", "error", "config", "check", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stderr.contains("[receivers.bad]") && stderr.contains("missing field `mode`"),
        "{stderr}"
    );
}

#[test]
fn metrics_flow_through_every_world() {
    let (Some(chaos), Some(rename), Some(sink)) = (build("chaos"), build("rename"), build("sink")) else {
        return;
    };
    let config = format!(
        r#"
        [plugins.chaos]
        source = "file://{}"
        [plugins.rename]
        source = "file://{}"
        [plugins.sink]
        source = "file://{}"

        [receivers.src]
        plugin = "chaos"
        mode = "ok"

        [connectors.fwd]
        plugin = "forward"

        [processors.prefix]
        plugin = "rename"
        prefix = "x."

        [exporters.out]
        plugin = "debug"

        [exporters.flaky]
        plugin = "sink"
        mode = "flaky"
        retry_on_failure = {{ initial_interval = "10ms" }}

        [exporters.partial]
        plugin = "sink"
        mode = "partial"

        [pipelines.metrics.ingest]
        receivers = ["src"]
        exporters = ["fwd"]

        [pipelines.metrics.publish]
        receivers = ["fwd"]
        processors = ["prefix"]
        exporters = ["out", "flaky", "partial"]
        "#,
        chaos.display(),
        rename.display(),
        sink.display()
    );
    let (ok, stdout, stderr) = run_once(&config);
    assert!(ok, "stderr:\n{stderr}");
    // Scraped, forwarded across pipelines, renamed by the processor.
    assert!(
        stdout.contains("x.chaos.calls value=1i"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // The processor's flush runs as the pipeline drains.
    assert!(stdout.contains("rename.seen value=1i"), "stdout:\n{stdout}");
    // The flaky exporter asked for a retry, then succeeded, encoding with the core.
    assert!(
        stderr.contains("line=x.chaos.calls value=1i") && stderr.contains("attempt=2"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("partial export: sink: one bad point"),
        "stderr:\n{stderr}"
    );
}
