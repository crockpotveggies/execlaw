//! `execlaw` CLI.
//!
//! Bare-metal lifecycle (Phase 14 — replaces the Phase 0 docker-compose
//! wrappers):
//!
//! - `execlaw install`           one-shot: migrate + register + start
//! - `execlaw service install`   register with systemd / launchd / SCM
//! - `execlaw service start`     start the service
//! - `execlaw service stop`      stop it
//! - `execlaw service restart`   stop + start
//! - `execlaw service status`    print install state + per-OS log commands
//! - `execlaw service uninstall` deregister
//!
//! Other:
//!
//! - `execlaw doctor`            checks vault + db + (optional) Docker for
//!   managed-mode backends
//! - `execlaw db migrate`        run pending migrations
//! - `execlaw hw rescan`         (stub — §Phase 2)
//! - `execlaw serve`             run the server in foreground (dev / debug)
//!
//! Docker is only relevant for managed-mode backends now (Phase 12);
//! the control plane itself runs as a host service.

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::OnceLock;

mod service;

/// Holds the python-sandbox plugin's runtime service for the
/// lifetime of the server process. Set exactly once during boot
/// (the wiring in cmd_serve calls `wire_python_sandbox`); accessed
/// implicitly via the four registered ToolImpls. The OnceLock
/// keeps the Arc alive so the OutputWatcher's notify-thread + tokio
/// timer task aren't dropped while there are still tools that
/// reference them.
static PYTHON_SANDBOX_SERVICE: OnceLock<
    std::sync::Arc<execlaw_server::python_sandbox::PythonSandboxService>,
> = OnceLock::new();

#[derive(Debug, Parser)]
#[command(name = "execlaw", version, about = "execlaw control plane CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// First-run bare-metal install: db migrate + register + start the
    /// service. The service uses systemd (Linux), launchd (macOS), or
    /// Windows Service Control Manager — see `execlaw service --help`.
    Install {
        /// Open the local DB plaintext during migrate (dev only).
        #[arg(long, default_value_t = false)]
        no_encrypt: bool,
        /// Install at system level (root / Administrator) instead of
        /// per-user. Required on Windows for SCM access; optional on
        /// Linux + macOS.
        #[arg(long, default_value_t = false)]
        system: bool,
        /// Skip db migrate (e.g. operator already ran it).
        #[arg(long, default_value_t = false)]
        skip_migrate: bool,
        /// Override the default bind address (loopback:3031).
        #[arg(long)]
        bind: Option<String>,
        /// Override the default DB path (`~/.execlaw/execlaw.db`).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Manage the long-running control-plane service.
    Service {
        #[command(subcommand)]
        op: ServiceOp,
    },
    /// Run preflight environment checks (DB, vault, optional Docker).
    Doctor,
    /// Database operations.
    Db {
        #[command(subcommand)]
        op: DbOp,
    },
    /// Hardware detection (stub for Phase 2).
    Hw {
        #[command(subcommand)]
        op: HwOp,
    },
    /// Run the HTTP server directly — for local dev / tests.
    ///
    /// Production uses `execlaw up` which spawns the container.
    Serve {
        /// Override the configured bind address for this run. When
        /// omitted, the value falls back to
        /// `config_general.bind_address` (set in Settings → General),
        /// then to the hardcoded `127.0.0.1:3031`. Passing `--bind`
        /// is intended for one-off dev runs; persistent changes
        /// should go through the SPA.
        #[arg(long)]
        bind: Option<String>,
        /// Database file. Defaults to `~/.execlaw/execlaw.db`.
        #[arg(long)]
        db: Option<PathBuf>,
        /// If set, open the DB plaintext (dev only).
        #[arg(long, default_value_t = false)]
        no_encrypt: bool,
    },
    /// Replay a turn — reconstructs the exact prompt the model saw,
    /// the policy decision (capabilities, planner_executor, etc.),
    /// and the events `commit_turn` produced for that turn.
    ///
    /// Used to debug "why did the model do that on turn 47?" without
    /// re-running inference.
    Replay {
        /// Conversation id.
        conversation_id: String,
        /// Inclusive upper-bound seq. Replay reconstructs state up
        /// to and including this seq.
        #[arg(long)]
        at: i64,
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long, default_value_t = false)]
        no_encrypt: bool,
    },
    /// Eval-flag operations — tag event ranges as regression
    /// targets for the LLM-judge harness.
    Eval {
        #[command(subcommand)]
        op: EvalOp,
    },
    /// Phase-7 hardening: scan `state_events` for rows with NULL
    /// `tag` and sign them under the current HMAC key. Idempotent.
    /// Run once per fleet before flipping the column to NOT NULL.
    BackfillEvents {
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long, default_value_t = false)]
        no_encrypt: bool,
    },
    /// Recovery hatch: re-sign every `state_events` row under the
    /// current HMAC key, OVERWRITING the existing tags. Use when the
    /// keyring lost the original key and the operator has accepted
    /// that history is now signed under a new key (tamper-evidence
    /// for already-stored rows is destroyed).
    ResignEvents {
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long, default_value_t = false)]
        no_encrypt: bool,
        /// Required: confirms the operator understands this destroys
        /// the tamper-evidence guarantee on existing rows.
        #[arg(long, default_value_t = false)]
        i_understand_history_will_be_resigned: bool,
    },
    /// Phase-7 hardening: snapshot the SQLCipher DB to a destination
    /// path using `VACUUM INTO`. The destination is a self-contained
    /// SQLite file with the same encryption posture as the source.
    Backup {
        /// Output path. Parent directory must exist.
        #[arg(long)]
        to: PathBuf,
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long, default_value_t = false)]
        no_encrypt: bool,
    },
    /// Phase-7 hardening: validate a snapshot file (must be openable
    /// with the same key + carry the migrations table) and atomically
    /// swap it into place. Refuses to overwrite a live DB without
    /// `--force`.
    Restore {
        /// Snapshot path produced by `execlaw backup`.
        #[arg(long)]
        from: PathBuf,
        /// Live DB path to replace.
        #[arg(long)]
        db: Option<PathBuf>,
        /// Allow overwriting a non-empty target file.
        #[arg(long, default_value_t = false)]
        force: bool,
        #[arg(long, default_value_t = false)]
        no_encrypt: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ServiceOp {
    /// Register the service with the host's service manager.
    Install {
        #[arg(long, default_value_t = false)]
        system: bool,
        #[arg(long)]
        bind: Option<String>,
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Start the service.
    Start {
        #[arg(long, default_value_t = false)]
        system: bool,
    },
    /// Stop the service.
    Stop {
        #[arg(long, default_value_t = false)]
        system: bool,
    },
    /// Stop, then start.
    Restart {
        #[arg(long, default_value_t = false)]
        system: bool,
    },
    /// Print install state + per-OS commands for live status / logs.
    Status {
        #[arg(long, default_value_t = false)]
        system: bool,
    },
    /// Deregister the service.
    Uninstall {
        #[arg(long, default_value_t = false)]
        system: bool,
    },
    /// Hidden — invoked by the service unit / SCM. Operators don't
    /// run this directly; `service install` registers it as the
    /// program path. Bind address is read from
    /// `config_general.bind_address` so SPA edits take effect on the
    /// next service restart without needing to rewrite the unit.
    #[command(hide = true)]
    Run {
        /// Hidden override for testing — production service units
        /// don't pass this; the binary reads bind from the DB.
        #[arg(long)]
        bind: Option<String>,
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long, default_value_t = false)]
        no_encrypt: bool,
    },
}

#[derive(Debug, Subcommand)]
enum DbOp {
    /// Apply pending migrations.
    Migrate {
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long, default_value_t = false)]
        no_encrypt: bool,
    },
    /// Print the current schema version.
    Status {
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long, default_value_t = false)]
        no_encrypt: bool,
    },
    /// Re-stamp the stored checksum for an already-applied migration
    /// to match the embedded SQL. Use ONLY when the runner refuses
    /// with "migration id N already applied but with a different
    /// checksum" because of a benign byte-level edit (line endings,
    /// whitespace). Does not re-run the migration body — columns and
    /// tables stay put.
    RepairChecksum {
        /// Migration id to repair (e.g. `35`).
        #[arg(long)]
        id: u32,
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long, default_value_t = false)]
        no_encrypt: bool,
    },
}

#[derive(Debug, Subcommand)]
enum HwOp {
    /// Re-run tier-1 sysfs detection.
    Rescan,
}

#[derive(Debug, Subcommand)]
enum EvalOp {
    /// Tag a range of events on a conversation as a regression target.
    Flag {
        /// Conversation id.
        conversation_id: String,
        /// Inclusive event seq range, e.g. `12..48`.
        #[arg(long)]
        range: String,
        /// Short human-readable label for the flag.
        #[arg(long)]
        label: String,
        /// Optional comma-separated tags (`trust-class,rule-of-two`).
        #[arg(long)]
        tags: Option<String>,
        /// Optional notes.
        #[arg(long)]
        notes: Option<String>,
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long, default_value_t = false)]
        no_encrypt: bool,
    },
    /// List eval flags. Filter by label if provided.
    List {
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long, default_value_t = false)]
        no_encrypt: bool,
    },
}

/// Tracing subscriber init — stdout (JSON or human-readable) plus
/// a daily-rotated JSONL file under `~/.execlaw/logs/` per §14.
///
/// File path is `<data_dir>/logs/execlaw.jsonl.YYYY-MM-DD`. The
/// returned `WorkerGuard` must be held for the lifetime of the
/// process — when it drops, the appender's background flush thread
/// shuts down and any unflushed lines are lost.
///
/// Set `EXECLAW_LOG_FORMAT=json` to get JSON on stdout too;
/// `EXECLAW_LOG_DIR` overrides the file directory; `EXECLAW_NO_FILE_LOG=1`
/// disables the file appender (useful for tests + ephemeral CLI
/// invocations like `execlaw doctor`).
/// Replace the default `eprintln!`-based panic hook with one that
/// emits a structured tracing event AND aborts the process. See
/// the call site comment for the rationale; mirrors the runner-
/// binary's `install_panic_hook` for journal-grep parity (target
/// `server::panic` on this side).
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let payload = info.payload();
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("<non-string panic payload>");
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_owned());
        tracing::error!(
            target: "server::panic",
            message,
            location,
            backtrace = %backtrace,
            "SERVER_PANIC — process aborting (core dump if ulimit -c allows)"
        );
        std::process::abort();
    }));
}

fn init_tracing() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    // Default filter: `info` for everything else, plus a per-crate
    // override that silences html5ever's noisy WARN spam. The
    // `html5ever::serialize` module fires `warn!("node with weird
    // namespace ...")` for every element with a non-html/mathml/svg
    // namespace — which fires constantly when dom_smoothie
    // serializes the cleaned DOM during deep-research gather (one
    // line per element per page; thousands per job). It's a known
    // upstream issue (servo/html5ever#122) that they've left as a
    // FIXME for years; safe to ignore at our level.
    //
    // Operators can still see those messages by setting
    // RUST_LOG="html5ever=warn,..." explicitly; the default just
    // gets them out of the way.
    let default_filter_directive =
        "info,html5ever=error,markup5ever=error,html5ever::serialize=error";
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter_directive));
    let want_json = std::env::var("EXECLAW_LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    // Stdout layer.
    let stdout_layer = if want_json {
        tracing_subscriber::fmt::layer()
            .json()
            .with_writer(std::io::stdout)
            .boxed()
    } else {
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stdout)
            .boxed()
    };

    // File layer — daily-rotated JSONL.
    let (file_layer, guard) = match resolve_log_dir() {
        Some(log_dir) => {
            if let Err(e) = std::fs::create_dir_all(&log_dir) {
                eprintln!("execlaw: failed to create log dir {log_dir:?}: {e}");
                (None, None)
            } else {
                let file_appender = tracing_appender::rolling::daily(&log_dir, "execlaw.jsonl");
                let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
                let layer = tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(non_blocking)
                    .with_ansi(false)
                    .boxed();
                (Some(layer), Some(guard))
            }
        }
        None => (None, None),
    };

    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(stdout_layer);
    let _ = match file_layer {
        Some(fl) => registry.with(fl).try_init(),
        None => registry.try_init(),
    };

    guard
}

/// Returns the directory the tracing file appender writes
/// `execlaw.jsonl.<DATE>` files into, or `None` if the operator has
/// disabled file logging (`EXECLAW_NO_FILE_LOG=1`). Same resolution
/// rules as `init_tracing` so both sides agree on which dir the log
/// viewer reads from.
pub(crate) fn resolve_log_dir() -> Option<PathBuf> {
    let want_file = std::env::var("EXECLAW_NO_FILE_LOG")
        .map(|v| !matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(true);
    if !want_file {
        return None;
    }
    let dir = std::env::var("EXECLAW_LOG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_data_dir().join("logs"));
    Some(dir)
}

fn default_data_dir() -> PathBuf {
    // directories::ProjectDirs picks the right per-OS path. On Linux this
    // resolves to ~/.local/share/execlaw — but we document ~/.execlaw as
    // the conventional location, so prefer that.
    if let Some(home) = dirs_home() {
        home.join(".execlaw")
    } else {
        PathBuf::from(".execlaw")
    }
}

fn dirs_home() -> Option<PathBuf> {
    directories::UserDirs::new().map(|d| d.home_dir().to_path_buf())
}

fn default_db_path() -> PathBuf {
    default_data_dir().join("execlaw.db")
}

pub(crate) fn open_db(db_path: &Path, no_encrypt: bool) -> anyhow::Result<execlaw_core::Database> {
    let (db, _cfg) = open_db_with_config(db_path, no_encrypt)?;
    Ok(db)
}

/// Open the DB and ALSO return the `DbConfig` used to open it.
/// Production callers that need to do file-level lifecycle ops on
/// the DB (factory reset — close the connection, delete the file,
/// re-open) stash the config in `AppState::db_config` so they can
/// re-open at the same path with the same encryption posture
/// without re-querying the OS keyring.
pub(crate) fn open_db_with_config(
    db_path: &Path,
    no_encrypt: bool,
) -> anyhow::Result<(execlaw_core::Database, execlaw_core::DbConfig)> {
    use execlaw_core::db::SqlCipherKey;

    let key = if no_encrypt {
        None
    } else {
        let key_bytes = execlaw_vault::load_or_create_master_key()
            .map_err(|e| anyhow::anyhow!("could not load master key from keyring: {e}"))?;
        Some(SqlCipherKey::RawBytes(key_bytes.to_vec()))
    };
    let cfg = execlaw_core::DbConfig {
        path: db_path.to_path_buf(),
        key,
    };
    let db = execlaw_core::Database::open(&cfg)?;
    Ok((db, cfg))
}

/// Build the runner image when it's missing OR older than the
/// running control-plane binary. Operator workflow: bump source,
/// `cargo build`, restart the control plane — the next supervisor
/// boot rebuilds the runner image automatically without a manual
/// `docker build` step.
///
/// Locates the workspace by walking up from the current
/// executable looking for `Dockerfile.runner`. Production builds
/// that don't ship the source tree fall through silently — the
/// existing "image not present → warn + disable" path covers the
/// no-build-context case.
async fn ensure_runner_image_fresh(image: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let exe = std::env::current_exe().context("locate current_exe")?;
    let exe_mtime = std::fs::metadata(&exe)
        .and_then(|m| m.modified())
        .context("stat current_exe")?;

    let workspace = match find_workspace_with_dockerfile(&exe) {
        Some(p) => p,
        None => {
            tracing::debug!(
                "runner image autobuild skipped: no Dockerfile.runner found \
                 walking up from {}",
                exe.display(),
            );
            return Ok(());
        }
    };

    // Inspect the existing image. `docker image inspect <image>
    // --format {{.Created}}` returns ISO-8601 on hit, non-zero
    // exit on miss. We don't go through bollard here because the
    // path also needs `docker build` and that's only via the CLI.
    let inspect = std::process::Command::new("docker")
        .args(["image", "inspect", "--format", "{{.Created}}", image])
        .output();
    let needs_build = match inspect {
        Ok(out) if out.status.success() => {
            let created_str = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            match chrono::DateTime::parse_from_rfc3339(&created_str) {
                Ok(image_dt) => {
                    let exe_dt: chrono::DateTime<chrono::Utc> = exe_mtime.into();
                    let stale = image_dt.with_timezone(&chrono::Utc) < exe_dt;
                    if stale {
                        tracing::info!(
                            image,
                            image_created = %image_dt,
                            exe_modified = %exe_dt,
                            "runner image is older than the control-plane binary; \
                             rebuilding",
                        );
                    } else {
                        tracing::debug!(
                            image,
                            image_created = %image_dt,
                            "runner image is up-to-date with the control-plane binary",
                        );
                    }
                    stale
                }
                Err(e) => {
                    tracing::warn!(
                        image,
                        raw = %created_str,
                        error = %e,
                        "couldn't parse `docker image inspect` Created field; rebuilding to be safe",
                    );
                    true
                }
            }
        }
        Ok(_) => {
            tracing::info!(
                image,
                "runner image not present locally; building from {}",
                workspace.display(),
            );
            true
        }
        Err(e) => {
            tracing::warn!(
                image,
                error = %e,
                "could not run `docker image inspect`; assuming Docker is not \
                 available and skipping autobuild",
            );
            return Ok(());
        }
    };

    if !needs_build {
        return Ok(());
    }

    tracing::info!(
        image,
        workspace = %workspace.display(),
        "running `docker build -f Dockerfile.runner -t {image} .` (this may take a few minutes on first run)",
    );
    let status = std::process::Command::new("docker")
        .arg("build")
        .arg("-f")
        .arg("Dockerfile.runner")
        .arg("-t")
        .arg(image)
        .arg(".")
        .current_dir(&workspace)
        .status();
    match status {
        Ok(s) if s.success() => {
            tracing::info!(image, "runner image build succeeded");
        }
        Ok(s) => {
            tracing::warn!(
                image,
                exit = ?s.code(),
                "runner image build returned non-zero exit; supervisor may use \
                 the previous image. Run the docker build manually to see the \
                 full output.",
            );
        }
        Err(e) => {
            tracing::warn!(
                image,
                error = %e,
                "could not invoke `docker build`; supervisor will fall back \
                 to the previous image (if any)",
            );
        }
    }
    Ok(())
}

/// Walk up from the executable looking for `Dockerfile.runner`.
/// Returns the workspace root (containing the Dockerfile) on hit.
/// `None` for production builds that ship without source.
fn find_workspace_with_dockerfile(exe: &Path) -> Option<PathBuf> {
    let mut cur: Option<&Path> = exe.parent();
    while let Some(dir) = cur {
        if dir.join("Dockerfile.runner").is_file() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

fn cmd_install(
    no_encrypt: bool,
    system: bool,
    skip_migrate: bool,
    bind: Option<String>,
    db: Option<PathBuf>,
) -> anyhow::Result<()> {
    println!("==> execlaw install (bare-metal)");

    // 1. Make sure the data dir exists. The vault + keyring + DB
    //    paths all live under it; the service unit also points its
    //    working_directory at it.
    let data_dir = default_data_dir();
    if !data_dir.exists() {
        std::fs::create_dir_all(&data_dir)?;
        println!("--> created {}", data_dir.display());
    }

    // 2. Migrate the local SQLite (encrypted by default; --no-encrypt
    //    for dev plaintext mode).
    if !skip_migrate {
        let db_path = db.clone().unwrap_or_else(default_db_path);
        println!("--> db migrate ({})", db_path.display());
        cmd_db_migrate(db_path, no_encrypt)?;
    } else {
        println!("--  skipping db migrate (--skip-migrate)");
    }

    // 3. Register the service with systemd / launchd / Windows SCM.
    println!(
        "--> service install ({} level)",
        if system { "system" } else { "user" }
    );
    service::install(system, bind, db)?;

    // 4. Start it.
    println!("--> service start");
    service::start(system)?;

    println!(
        "==> install complete — verify with `curl http://{}/api/health`",
        service::SERVICE_BIND
    );
    println!("    Use `execlaw service status` for live state + log paths.");
    Ok(())
}

fn cmd_doctor() -> anyhow::Result<()> {
    let mut ok = true;
    let mut report = String::new();

    // 1. Docker — optional now (Phase 14). The control plane runs as
    //    a host service; Docker is only needed for managed-mode
    //    backends (Phase 12) where the supervisor spawns container
    //    sidecars. A missing Docker downgrades to a NOTE not a
    //    failure.
    match std::process::Command::new("docker")
        .arg("--version")
        .output()
    {
        Ok(out) if out.status.success() => {
            report.push_str(&format!(
                "OK   docker:   {} (managed-mode backends available)",
                String::from_utf8_lossy(&out.stdout).trim()
            ));
            report.push('\n');
        }
        _ => {
            report.push_str(
                "NOTE docker:   not found — managed-mode backends disabled. \
                 External backends (operator-supplied URLs) still work.\n",
            );
        }
    }

    // 2. Data dir.
    let data_dir = default_data_dir();
    match std::fs::create_dir_all(&data_dir) {
        Ok(_) => {
            report.push_str(&format!("OK  data dir: {}\n", data_dir.display()));
        }
        Err(e) => {
            ok = false;
            report.push_str(&format!(
                "MISS data dir: can't create {}: {e}\n",
                data_dir.display()
            ));
        }
    }

    // 3. SQLCipher sanity — open a throwaway encrypted DB in a temp
    //    location. If SQLCipher isn't bundled correctly this fails.
    let tmp = std::env::temp_dir().join("execlaw-doctor-sqlcipher-check.db");
    let _ = std::fs::remove_file(&tmp);
    let cfg = execlaw_core::DbConfig {
        path: tmp.clone(),
        key: Some(execlaw_core::db::SqlCipherKey::Passphrase(
            "doctor-preflight".into(),
        )),
    };
    match execlaw_core::Database::open(&cfg) {
        Ok(db) => {
            let res = db.with_conn(|c| {
                c.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1);")?;
                Ok(())
            });
            match res {
                Ok(_) => report.push_str("OK  sqlcipher: bundled SQLCipher works\n"),
                Err(e) => {
                    ok = false;
                    report.push_str(&format!("MISS sqlcipher: {e}\n"));
                }
            }
        }
        Err(e) => {
            ok = false;
            report.push_str(&format!("MISS sqlcipher: {e}\n"));
        }
    }
    let _ = std::fs::remove_file(&tmp);

    // 4. Keyring — try to create/read a test entry.
    match keyring::Entry::new("execlaw", "doctor_probe") {
        Ok(entry) => {
            let _ = entry.set_password("ok");
            match entry.get_password() {
                Ok(_) => {
                    let _ = entry.delete_credential();
                    report.push_str("OK  keyring:  OS keyring reachable\n");
                }
                Err(e) => {
                    // This is only a warning — headless hosts fall back
                    // to a passphrase file.
                    report.push_str(&format!(
                        "WARN keyring: OS keyring not usable ({e}); passphrase fallback required\n"
                    ));
                }
            }
        }
        Err(e) => {
            report.push_str(&format!("WARN keyring: {e}\n"));
        }
    }

    println!("execlaw doctor\n--------------\n{report}");
    if ok {
        println!("verdict: OK");
        Ok(())
    } else {
        anyhow::bail!("doctor found blocking issues");
    }
}

fn cmd_db_migrate(db_path: PathBuf, no_encrypt: bool) -> anyhow::Result<()> {
    let db = open_db(&db_path, no_encrypt)?;
    let applied = execlaw_core::MigrationRunner::new(&db).apply_all()?;
    if applied.is_empty() {
        println!("nothing to apply; schema is up to date");
    } else {
        println!("applied migrations: {applied:?}");
    }
    Ok(())
}

fn cmd_db_status(db_path: PathBuf, no_encrypt: bool) -> anyhow::Result<()> {
    let db = open_db(&db_path, no_encrypt)?;
    let count = execlaw_core::MigrationRunner::new(&db).applied_count()?;
    println!("applied migrations: {count}");
    Ok(())
}

fn cmd_db_repair_checksum(id: u32, db_path: PathBuf, no_encrypt: bool) -> anyhow::Result<()> {
    let db = open_db(&db_path, no_encrypt)?;
    let runner = execlaw_core::MigrationRunner::new(&db);
    let patched = runner.repair_checksum(id)?;
    if patched {
        println!(
            "repaired stored checksum for migration id {id} \
             to match embedded SQL on disk"
        );
    } else {
        println!(
            "no schema_version row for migration id {id}; nothing to repair \
             (run `execlaw db migrate` first if this is a fresh DB)"
        );
    }
    Ok(())
}

fn cmd_hw_rescan() -> anyhow::Result<()> {
    let profile = execlaw_container_manager::detect_sysfs(Path::new("/sys"));
    println!("{}", serde_json::to_string_pretty(&profile)?);
    Ok(())
}

/// `execlaw replay <conv_id> --at <seq>` — reconstruct the prompt
/// the model saw, the policy decision, and the events that turn
/// committed. Pure read-only operation against the SQLite log.
fn cmd_replay(
    conversation_id: String,
    at: i64,
    db_path: PathBuf,
    no_encrypt: bool,
) -> anyhow::Result<()> {
    use execlaw_core::events::{EventKind, EventLog};
    use execlaw_core::ids::{ConversationId, EventSeq};
    use execlaw_core::principal::PrincipalStore;
    use execlaw_core::principal::TrustLevel as CoreTrust;
    use execlaw_policy::trust::{TrustLevel, TurnPolicyInput, evaluate_turn};

    let db = open_db(&db_path, no_encrypt)?;
    let cid = ConversationId::from(conversation_id.as_str());
    let log = EventLog::new(&db);

    let all_events = log
        .replay_since(&cid, EventSeq(0))
        .map_err(|e| anyhow::anyhow!("replay: {e}"))?;
    if all_events.is_empty() {
        anyhow::bail!("no events for conversation {conversation_id}");
    }
    let target_seq = at;
    let target_idx = all_events
        .iter()
        .position(|e| e.seq.0 == target_seq)
        .ok_or_else(|| anyhow::anyhow!("seq {target_seq} not in conversation {conversation_id}"))?;

    // Walk backwards from target_seq to find the user_msg that
    // started this turn — replay reconstructs the turn that
    // CONTAINS the target seq.
    let mut user_msg_idx = target_idx;
    while user_msg_idx > 0 && all_events[user_msg_idx].kind != EventKind::UserMsg {
        user_msg_idx -= 1;
    }

    // Resolve sender trust at replay time. Prefer the persisted
    // PrincipalStore row (post-trust-changes); fall back to the
    // event's actor field for ephemeral senders.
    let actor = all_events[user_msg_idx]
        .actor
        .as_deref()
        .unwrap_or("controller");
    let sender_trust = if actor == "controller" {
        TrustLevel::Controller
    } else {
        let store = PrincipalStore::new(&db);
        match store.get(&execlaw_core::ids::PrincipalId::from(actor)) {
            Ok(Some(p)) => {
                TrustLevel::parse(p.trust_level.class_tag()).unwrap_or(TrustLevel::UnknownPending)
            }
            _ => {
                // Stamp at replay time as if we were resolving fresh.
                let _ = CoreTrust::Controller;
                TrustLevel::UnknownPending
            }
        }
    };

    let policy = evaluate_turn(TurnPolicyInput {
        effective_trust: sender_trust,
        sender_trust,
        voice: false,
        accesses_sensitive_data: false,
        produces_external_effect: false,
    });

    // Print the reconstructed turn.
    println!("=== Replay {conversation_id} @ seq {target_seq} ===");
    println!();
    println!("Sender trust:        {:?}", sender_trust);
    println!("Policy decision:");
    println!("  drop_turn:         {}", policy.drop_turn);
    println!("  require_approval:  {}", policy.require_approval);
    println!("  planner_executor:  {}", policy.planner_executor);
    println!("  spotlighting:      {}", policy.spotlighting);
    println!("  latency_band:      {:?}", policy.latency_band);
    println!("  capability_set:    {:?}", policy.capability_set);
    println!();
    println!("Reconstructed prompt history:");
    for ev in &all_events[..=user_msg_idx] {
        match ev.kind {
            EventKind::UserMsg => {
                let text = ev
                    .decode_payload::<serde_json::Value>()
                    .ok()
                    .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(|s| s.to_owned()))
                    .unwrap_or_else(|| "<unparseable>".into());
                println!("  user[{}]: {text}", ev.seq.0);
            }
            EventKind::ModelTurn => {
                let text = ev
                    .decode_payload::<serde_json::Value>()
                    .ok()
                    .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(|s| s.to_owned()))
                    .unwrap_or_else(|| "<unparseable>".into());
                println!("  assistant[{}]: {text}", ev.seq.0);
            }
            _ => {}
        }
    }
    println!();
    println!(
        "Events committed by/around the target turn (seq {} → {}):",
        all_events[user_msg_idx].seq.0, target_seq,
    );
    for ev in &all_events[user_msg_idx..=target_idx] {
        println!(
            "  seq={:>4}  kind={:<22}  actor={:?}",
            ev.seq.0,
            ev.kind.as_str(),
            ev.actor
        );
    }
    Ok(())
}

/// `execlaw eval flag <conv_id> --range a..b --label X` — record an
/// eval-flag row.
fn cmd_eval_flag(
    conversation_id: String,
    range: String,
    label: String,
    tags: Option<String>,
    notes: Option<String>,
    db_path: PathBuf,
    no_encrypt: bool,
) -> anyhow::Result<()> {
    use execlaw_core::eval::{EvalFlagRow, EvalFlaggedStore};
    use execlaw_core::ids::ConversationId;

    let (from, to) = parse_range(&range)?;
    let tags_vec: Vec<String> = tags
        .map(|s| s.split(',').map(|t| t.trim().to_owned()).collect())
        .unwrap_or_default();

    let db = open_db(&db_path, no_encrypt)?;
    let store = EvalFlaggedStore::new(&db);
    let id = store
        .insert(&EvalFlagRow {
            id: None,
            conversation_id: ConversationId::from(conversation_id.as_str()),
            from_seq: from,
            to_seq: to,
            label: label.clone(),
            tags: tags_vec,
            flagged_by: "controller".into(),
            flagged_at: chrono::Utc::now().timestamp(),
            notes,
        })
        .map_err(|e| anyhow::anyhow!("insert: {e}"))?;
    println!("flagged: id={id} conversation={conversation_id} range={from}..{to} label={label}");
    Ok(())
}

/// `execlaw eval list [--label X]` — print every eval flag.
fn cmd_eval_list(label: Option<String>, db_path: PathBuf, no_encrypt: bool) -> anyhow::Result<()> {
    use execlaw_core::eval::EvalFlaggedStore;

    let db = open_db(&db_path, no_encrypt)?;
    let store = EvalFlaggedStore::new(&db);
    let rows = match label.as_deref() {
        Some(l) => store
            .list_by_label(l)
            .map_err(|e| anyhow::anyhow!("list: {e}"))?,
        None => store.list_all().map_err(|e| anyhow::anyhow!("list: {e}"))?,
    };
    if rows.is_empty() {
        println!("(no flags)");
        return Ok(());
    }
    for r in rows {
        println!(
            "id={:<4} conv={:<24} range={:>4}..{:<4} label={:<24} tags={:?} flagged_at={}",
            r.id.unwrap_or_default(),
            r.conversation_id.as_str(),
            r.from_seq,
            r.to_seq,
            r.label,
            r.tags,
            r.flagged_at,
        );
        if let Some(n) = r.notes {
            println!("       notes: {n}");
        }
    }
    Ok(())
}

// ----- Phase 7 hardening commands -------------------------------------

fn cmd_backfill_events(db_path: PathBuf, no_encrypt: bool) -> anyhow::Result<()> {
    use execlaw_core::events::{EventLog, KeyRing};

    let db = open_db(&db_path, no_encrypt)?;
    // Use the operator's keyring-backed master key so back-fill
    // produces tags that match what `serve` would have produced
    // had a key been attached at append time.
    let key = execlaw_vault::load_or_create_master_key()
        .map_err(|e| anyhow::anyhow!("master key: {e}"))?;
    let log = EventLog::new(&db).with_key_ring(KeyRing::single(0, key.to_vec()));
    let report = log
        .backfill_null_tags()
        .map_err(|e| anyhow::anyhow!("back-fill: {e}"))?;
    println!(
        "backfill: signed={} skipped={} null_remaining={}",
        report.signed, report.skipped, report.null_remaining,
    );
    Ok(())
}

/// Recovery: re-sign every state_events row under the current HMAC
/// key, overwriting the existing tag. Use only when the original
/// signing key was lost (e.g. OS keyring lost the entry) and the
/// operator accepts that the existing log is now signed under a new
/// key — the tamper-evidence guarantee for already-stored rows is
/// gone.
fn cmd_resign_events(db_path: PathBuf, no_encrypt: bool, confirmed: bool) -> anyhow::Result<()> {
    use execlaw_core::events::{EventLog, KeyRing};

    if !confirmed {
        anyhow::bail!(
            "resign-events destroys the tamper-evidence guarantee for existing rows. \
             Re-run with --i-understand-history-will-be-resigned to proceed."
        );
    }

    let db = open_db(&db_path, no_encrypt)?;
    let key = execlaw_vault::load_or_create_master_key()
        .map_err(|e| anyhow::anyhow!("master key: {e}"))?;
    let log = EventLog::new(&db).with_key_ring(KeyRing::single(0, key.to_vec()));
    let report = log
        .resign_all_with_current_key()
        .map_err(|e| anyhow::anyhow!("resign: {e}"))?;
    println!("resign: signed={} (overwrote existing tags)", report.signed);
    Ok(())
}

fn cmd_backup(to: PathBuf, db_path: PathBuf, no_encrypt: bool) -> anyhow::Result<()> {
    if !db_path.exists() {
        anyhow::bail!("source db not found: {}", db_path.display());
    }
    if let Some(parent) = to.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            anyhow::bail!(
                "parent directory of --to does not exist: {}",
                parent.display()
            );
        }
    }
    if to.exists() {
        anyhow::bail!(
            "--to path already exists; remove it first: {}",
            to.display()
        );
    }

    let db = open_db(&db_path, no_encrypt)?;
    let to_str = to
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-utf8 path: {}", to.display()))?
        .to_owned();

    // VACUUM INTO writes a fresh, fully-defragmented copy at the
    // target path. With SQLCipher in play it inherits the same
    // encryption posture by default, so a snapshot can be restored
    // by any process holding the master key.
    db.with_conn(|c| {
        c.execute_batch(&format!("VACUUM INTO '{}'", to_str.replace('\'', "''")))?;
        Ok(())
    })
    .map_err(|e| anyhow::anyhow!("VACUUM INTO: {e}"))?;

    println!(
        "backup: {} -> {} ({} bytes)",
        db_path.display(),
        to.display(),
        std::fs::metadata(&to).map(|m| m.len()).unwrap_or_default()
    );
    Ok(())
}

fn cmd_restore(
    from: PathBuf,
    db_path: PathBuf,
    force: bool,
    no_encrypt: bool,
) -> anyhow::Result<()> {
    if !from.exists() {
        anyhow::bail!("snapshot file not found: {}", from.display());
    }

    // Validate the snapshot first: it must open with the operator's
    // master key AND carry the schema_version table. Otherwise
    // restoring would silently swap in a useless DB.
    {
        let snap = open_db(&from, no_encrypt)?;
        let has_version: bool = snap
            .with_conn(|c| {
                let n: i64 = c
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master \
                         WHERE type='table' AND name='schema_version'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                Ok(n > 0)
            })
            .unwrap_or(false);
        if !has_version {
            anyhow::bail!(
                "snapshot at {} doesn't look like an execlaw DB (missing schema_version table)",
                from.display()
            );
        }
    }

    if db_path.exists() && !force {
        let size = std::fs::metadata(&db_path)
            .map(|m| m.len())
            .unwrap_or_default();
        if size > 0 {
            anyhow::bail!(
                "target {} is non-empty ({} bytes); pass --force to overwrite",
                db_path.display(),
                size,
            );
        }
    }

    if let Some(parent) = db_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    // Atomic-ish: write to a sibling tempfile, then rename. Rename
    // on the same filesystem is atomic on every supported OS.
    let tmp = db_path.with_extension("restore.tmp");
    if tmp.exists() {
        std::fs::remove_file(&tmp)?;
    }
    std::fs::copy(&from, &tmp)?;
    if db_path.exists() {
        std::fs::remove_file(&db_path)?;
    }
    std::fs::rename(&tmp, &db_path)?;

    println!(
        "restore: {} -> {} ({} bytes)",
        from.display(),
        db_path.display(),
        std::fs::metadata(&db_path)
            .map(|m| m.len())
            .unwrap_or_default()
    );
    Ok(())
}

/// Build a WebAuthn relying-party from environment variables. Returns
/// `None` (so login falls back to password-only) on any error so an
/// operator who hasn't yet configured WebAuthn isn't locked out.
///
/// `EXECLAW_WEBAUTHN_RP_ID` is the effective domain (hostname only —
/// no scheme, no port). Defaults to `"localhost"`.
/// `EXECLAW_WEBAUTHN_ORIGIN` is the full origin used to build the URL
/// passed to webauthn-rs. Defaults to `http://<bind_addr>` so a
/// fresh-from-clone install Just Works for local-dev.
fn build_webauthn_from_env(
    bind_addr: &std::net::SocketAddr,
) -> Option<execlaw_server::webauthn::WebauthnSvc> {
    let rp_id = std::env::var("EXECLAW_WEBAUTHN_RP_ID").unwrap_or_else(|_| "localhost".to_owned());
    let origin =
        std::env::var("EXECLAW_WEBAUTHN_ORIGIN").unwrap_or_else(|_| format!("http://{bind_addr}"));
    match execlaw_server::webauthn::WebauthnSvc::new(&rp_id, &origin, "execlaw") {
        Ok(svc) => Some(svc),
        Err(e) => {
            tracing::warn!(
                rp_id,
                origin,
                error = %e,
                "webauthn relying-party build failed; falling back to password-only login"
            );
            None
        }
    }
}

/// Parse `12..48` (inclusive on both ends).
fn parse_range(s: &str) -> anyhow::Result<(i64, i64)> {
    let mut parts = s.splitn(2, "..");
    let from: i64 = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("range '{s}' missing 'from'"))?
        .trim()
        .parse()
        .map_err(|e| anyhow::anyhow!("bad from in '{s}': {e}"))?;
    let to: i64 = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("range '{s}' missing 'to' (use a..b)"))?
        .trim()
        .parse()
        .map_err(|e| anyhow::anyhow!("bad to in '{s}': {e}"))?;
    Ok((from, to))
}

/// Pick the bind address the listener will use. Precedence:
///
///   1. The `--bind` CLI flag, if passed (one-off overrides for dev).
///   2. `config_general.bind_address` from the DB, if a row exists
///      (the SPA's Settings → General writes here).
///   3. `127.0.0.1:3031` — the install-time hardcoded default.
///
/// Returns the resolved value plus a short source label suitable for
/// the boot log, so an operator chasing "why am I bound to X" has a
/// breadcrumb.
fn resolve_bind(cli: Option<String>, db: Option<String>) -> (String, &'static str) {
    if let Some(s) = cli {
        return (s, "cli");
    }
    if let Some(s) = db.filter(|s| !s.trim().is_empty()) {
        return (s, "config_general");
    }
    ("127.0.0.1:3031".to_string(), "default")
}

async fn cmd_serve(bind: Option<String>, db_path: PathBuf, no_encrypt: bool) -> anyhow::Result<()> {
    let (db, db_config) = open_db_with_config(&db_path, no_encrypt)?;
    execlaw_core::MigrationRunner::new(&db).apply_all()?;

    // Bind address resolution (precedence: CLI flag > DB > default).
    // The DB-stored value comes from Settings → General; making it
    // authoritative here is what allows the SPA's "takes effect on
    // next restart" hint to be true.
    let db_bind = execlaw_core::general_settings::GeneralSettingsStore::new(&db)
        .get()
        .ok()
        .flatten()
        .map(|s| s.bind_address);
    let (bind, bind_source) = resolve_bind(bind, db_bind);
    tracing::info!(addr = %bind, source = bind_source, "resolved bind address");

    // 2026-04-28 — derive the JWT signing key from the vault's
    // master key. Pre-fix this was `JwtSigner::generate(...)` which
    // minted a fresh keypair on every boot, silently invalidating
    // every previously-issued access_token whenever cargo-watch
    // rebuilt. Now: same master across boots → same JWT signing
    // key → tokens survive the rebuild. Fall back to the random
    // generator only when the keyring isn't reachable AT ALL
    // (rare; we'd already be running with a degraded vault).
    let signer = match execlaw_vault::load_or_create_master_key() {
        Ok(master) => std::sync::Arc::new(execlaw_server::auth::JwtSigner::from_master_key(
            &master,
            "execlaw".into(),
        )),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "could not load vault master key; JWT signing key will be ephemeral. \
                 Operators will be signed out on every restart."
            );
            std::sync::Arc::new(execlaw_server::auth::JwtSigner::generate("execlaw".into()))
        }
    };
    // Phase-7 hardening: refresh tokens persist in SQLite so a
    // server restart no longer signs every operator out.
    let refresh_store = std::sync::Arc::new(execlaw_server::auth::RefreshStore::new(db.clone()));

    // EXECLAW_INFERENCE_URL lets operators point dev servers at a local
    // vLLM / Ollama / OpenArc without editing code. Production boots
    // will read the active Standard deployment from
    // `config_runner_deployments` once the registry API lands.
    let inference_base_url = std::env::var("EXECLAW_INFERENCE_URL").ok();

    let config = std::sync::Arc::new(execlaw_server::ServerConfig {
        bind_addr: bind.parse()?,
        log_dir: resolve_log_dir(),
        ..Default::default()
    });

    // Phase 12.E — bootstrap is the boot-time global URL; per-turn
    // resolution may override it via config_backends rows.
    let bootstrap_inference = inference_base_url
        .map(|url| std::sync::Arc::new(execlaw_inference_api::InferenceClient::new(url)));
    let inference = std::sync::Arc::new(
        execlaw_server::inference_resolver::InferenceResolver::new(bootstrap_inference),
    );

    // Load-or-create the event-log HMAC key. Phase 1 derives it from
    // the same OS keyring entry as the SQLCipher master; a future
    // migration adds a dedicated `event_log_hmac_key` vault row with
    // key_id rotation. For now, reuse the keyring-backed bytes.
    let hmac_key = execlaw_vault::load_or_create_master_key()
        .map(|bytes| std::sync::Arc::new(bytes.to_vec()))
        .ok();

    // Stage root for installed plugins — defaults to
    // `<db_parent>/plugins/`. Each install lands under
    // `<stage_root>/<plugin_id>-<version>/`.
    let stage_root = db_path
        .parent()
        .map(|p| p.join("plugins"))
        .unwrap_or_else(|| PathBuf::from("./plugins"));
    if let Err(e) = std::fs::create_dir_all(&stage_root) {
        tracing::warn!(path = ?stage_root, error = %e, "failed to ensure plugin stage root");
    }
    let plugin_host = execlaw_plugin_host::PluginHost::new(
        db.clone(),
        execlaw_plugin_host::HookRegistry::new(),
        stage_root,
    );
    // Re-hydrate installed plugins from the DB so they survive restart.
    plugin_host
        .hydrate()
        .await
        .map_err(|e| anyhow::anyhow!("plugin hydrate: {e}"))?;

    // 2026-04-29 — register the core trait-based built-in tools
    // (read_memory, write_memory, list_memory, set_thread_name,
    // get_thread) into the host's HookRegistry and seed their
    // `config_tool_access` rows from each descriptor's
    // `default_allowed_classes`. Must run BEFORE sync_tool_access so
    // the access sync sees them in `registry.all_builtins()`.
    {
        let now = chrono::Utc::now().timestamp();
        match execlaw_plugin_host::register_core_builtins(plugin_host.registry(), &db, now) {
            Ok(landed) => tracing::info!(count = landed.len(), "core built-in tools registered"),
            Err(e) => {
                // Conflict here means an operator-installed plugin is
                // claiming a tool name that overlaps with a core
                // built-in — the plugin install should have rejected
                // that, but if it slipped through we can't proceed
                // safely with the overlap.
                return Err(anyhow::anyhow!("register_core_builtins failed: {e}"));
            }
        }
    }

    // 2026-05-04 — Phase 3 (signal sidecar): register the two
    // host-implemented Signal tools as builtins so they can reach
    // `ctx.transport`. The plugin manifest declares them with
    // `host_implemented = true` so the rhai tier doesn't try to
    // (Phase B removed: signal_tools host registration. The signal
    // plugin v0.4.0+ ships every tool in main.rhai — dispatch hits
    // the script tier through the standard plugin-tool path. No
    // host-side wiring needed here anymore.)

    // 2026-05-03 — Phase A: register the skill subsystem's tool
    // surface (skills.list/view/resource/search + admin-gated
    // create/update/promote/archive). Uses the same
    // `register_builtins` helper as the core tools so each skill
    // tool also gets a `config_tool_access` seed row from its
    // descriptor's `default_allowed_classes`. The store is shared
    // across all eight tools via Arc; it holds only a Database
    // handle so the clone is cheap.
    let skill_store = std::sync::Arc::new(execlaw_skills::SkillStore::new(db.clone()));
    {
        let now = chrono::Utc::now().timestamp();
        let tools = execlaw_skills::skill_tools(skill_store.clone());
        match execlaw_plugin_host::register_builtins(plugin_host.registry(), &db, now, tools) {
            Ok(landed) => tracing::info!(count = landed.len(), "skill tools registered"),
            Err(e) => return Err(anyhow::anyhow!("register skill tools failed: {e}")),
        }
    }

    // 2026-05-03 — Phase B: attach the same shared SkillStore to
    // the plugin host so `install` imports plugin-shipped skills
    // (with `<plugin_id>/` namespace prepending) and `uninstall`
    // archives them. `attach_skill_store` is `OnceLock`-backed and
    // composes after `hydrate()` without disturbing already-loaded
    // subprocesses or script plugins.
    plugin_host.attach_skill_store(skill_store.clone());

    // Phase 8a: reflect every built-in + persisted plugin tool into
    // `config_tool_access` so the per-tool trust-class allowlist gate
    // has a row for everything. Idempotent — operator policy from
    // previous boots is preserved; only first-sight tools get the
    // open default.
    {
        let now = chrono::Utc::now().timestamp();
        match execlaw_server::tool_sync::sync_tool_access(&db, &plugin_host, now) {
            Ok(n) => tracing::info!(rows_synced = n, "tool_access sync complete"),
            Err(e) => {
                tracing::warn!(error = %e, "tool_access sync failed; dispatch gate will fall back to allow until next sync")
            }
        }
    }

    // Phase 7e: build the WebAuthn relying-party from EXECLAW_WEBAUTHN_*
    // env vars. Falling back to localhost:3031 keeps local-dev working
    // out of the box; production must set these to the real public
    // origin (HTTPS only — webauthn-rs rejects http origins outside
    // of `localhost`).
    let webauthn = build_webauthn_from_env(&config.bind_addr).map(std::sync::Arc::new);

    // Phase 8c: MCP connection manager. `reconcile()` spins up one
    // tokio actor per `enabled = true, transport = stdio` row in
    // `config_mcp_servers`, opens the connection, runs the
    // initialise handshake, and reflects every discovered tool
    // into `config_tool_access`.
    let mcp_host = execlaw_server::mcp_host::McpHost::new(db.clone());
    {
        let mh = mcp_host.clone();
        tokio::spawn(async move { mh.reconcile().await });
    }

    let events = execlaw_server::EventBus::new();

    // Phase 12.C — supervisor for managed inference backends. Best-
    // effort connect to the local Docker daemon; if it fails (no
    // Docker, e.g. dev on a host without Docker installed) we fall
    // through to `None` and managed-mode rows just sit `Stopped`
    // until Docker is available. The actual `run()` task is spawned
    // below alongside the other sweepers so it shares `sweep_stop`.
    //
    // Phase 14.C — when the supervisor IS wired, we also stand up
    // a host-side HuggingFace downloader pointed at
    // `~/.execlaw/hf-cache`. The supervisor blocks every managed
    // row's spawn behind a cache check + (if missing) a download,
    // surfacing real progress in the SPA pill and avoiding the
    // "container redownloads 18 GB on every CrashLoop" failure
    // mode that filled the user's disk on first run.
    // Connect to Docker once + share the controller across every
    // supervisor that needs it (backend + sidecar today; future
    // ones land here too). `None` when Docker is unreachable —
    // each supervisor below independently checks + degrades to
    // disabled mode rather than failing the whole boot.
    let docker_ctrl: Option<std::sync::Arc<dyn execlaw_container_manager::ServiceController>> =
        match execlaw_container_manager::BollardServiceController::connect() {
            Ok(ctrl) => Some(std::sync::Arc::new(ctrl)),
            Err(e) => {
                tracing::warn!("container supervisors disabled — Docker daemon unreachable: {e}");
                None
            }
        };

    // Phase 14.G (Apple Silicon plan) — the backend supervisor needs
    // a controller that can dispatch to either Docker (vLLM, Whisper,
    // Kokoro — every existing managed preset) OR a native subprocess
    // (Ollama on Apple Silicon, where Metal has no container
    // passthrough). When Docker is reachable, we wrap both behind
    // `MultiplexedServiceController` so the supervisor's existing
    // `Arc<dyn ServiceController>` slot keeps working unchanged.
    // When Docker is unreachable but we're on a Mac (or any host with
    // Ollama installed), we still expose the native path so the
    // wizard's Apple preset can spawn — Docker rows on the same host
    // will surface a clear "BollardServiceController cannot spawn..."
    // error rather than silently disappearing.
    let native_ctrl: std::sync::Arc<dyn execlaw_container_manager::ServiceController> =
        std::sync::Arc::new(execlaw_container_manager::NativeServiceController::new());
    let backend_ctrl: Option<std::sync::Arc<dyn execlaw_container_manager::ServiceController>> =
        match &docker_ctrl {
            Some(d) => Some(std::sync::Arc::new(
                execlaw_container_manager::MultiplexedServiceController::new(
                    d.clone(),
                    native_ctrl.clone(),
                ),
            )),
            None => {
                // Native-only path. Useful on Macs without Docker Desktop
                // installed — the operator can still configure the
                // Apple-Silicon Ollama preset and the supervisor will
                // spawn it. Sidecar supervisor (signal-cli etc.) stays
                // gated on `docker_ctrl` below so it correctly reports
                // "Docker unreachable" without affecting the inference
                // path.
                tracing::info!(
                    "backend supervisor falling back to native-only controller — Docker is \
                 unreachable, but managed-mode Apple-Silicon Ollama presets will still spawn"
                );
                Some(native_ctrl.clone())
            }
        };

    let backend_supervisor = backend_ctrl.as_ref().map(|ctrl| {
        // Resolve the host's primary HF cache directory.
        // Operator can override with EXECLAW_HF_CACHE; default
        // is `~/.execlaw/hf-cache/`. Created on demand so a
        // fresh install Just Works without manual setup.
        let primary_cache: std::path::PathBuf = match std::env::var("EXECLAW_HF_CACHE") {
            Ok(p) => std::path::PathBuf::from(p),
            Err(_) => directories::ProjectDirs::from("", "", "execlaw")
                .map(|d| d.data_dir().join("hf-cache"))
                .unwrap_or_else(|| std::path::PathBuf::from("./.execlaw-hf-cache")),
        };
        if let Err(e) = std::fs::create_dir_all(primary_cache.join("hub")) {
            tracing::warn!(
                path = %primary_cache.display(),
                "failed to create host HF cache directory: {e}"
            );
        }
        // Operator-supplied secondary caches live in
        // `config_general.hf_secondary_caches_json`. We snapshot
        // them at boot time; changing the list requires a
        // service restart for the supervisor to pick up. (Future
        // work: dynamic reload via `BackendSupervisor::reload_hf_caches()`.)
        let secondaries = execlaw_core::general_settings::GeneralSettingsStore::new(&db)
            .read_secondary_hf_caches()
            .unwrap_or_default();
        let token = std::env::var("HF_TOKEN").ok();
        let downloader =
            execlaw_container_manager::HfDownloader::new(primary_cache.clone(), secondaries, token);
        execlaw_server::backend_supervisor::BackendSupervisor::new(db.clone(), ctrl.clone())
            .with_hf_downloader(downloader, primary_cache)
    });

    // Phase 2b — sidecar supervisor. Manages every plugin-declared
    // companion container (`[services.sidecar]`). When Docker is
    // unreachable, leave it as `None` and the `/api/admin/sidecars`
    // route returns 503 with a friendly hint.
    //
    // Construction is cheap (just an Arc + a HashMap); we wire it
    // up regardless of whether any plugin has registered a sidecar
    // yet so the supervisor's snapshot is ready the moment a
    // plugin install lands.
    let sidecar_supervisor = docker_ctrl.as_ref().map(|ctrl| {
        execlaw_server::sidecar_supervisor::SidecarSupervisor::new(
            ctrl.clone(),
            plugin_host.registry().clone(),
        )
    });

    let voice_sessions = execlaw_server::voice_session::VoiceSessionRegistry::new(events.clone());

    // Phase 13.C — voice runtime resolves Whisper / Kokoro endpoints
    // from `config_backends` and the voice id from `config_personality`
    // on every new session. A Backends or Personality save mid-
    // conversation takes effect on the next utterance (mirrors
    // InferenceResolver). All wiring lives in
    // `voice_runtime::build_with_db` so it's exercised by unit tests
    // — this cli crate has no tests of its own.
    let voice_runtime =
        execlaw_server::voice_runtime::VoiceRuntime::build_with_db(events.clone(), db.clone());

    // Phase 16 — per-principal-group runner supervisor. Default
    // ON. Operators who want the legacy in-process chat path (or
    // who run on a Docker-less host where supervised spawn would
    // fail anyway) can opt out with `EXECLAW_RUNNERS_ENABLED=0`.
    //
    // We also defensively disable when:
    //   * Docker is unreachable (operator may not have started
    //     Docker Desktop yet), OR
    //   * the runner image isn't built (first-run on a fresh
    //     checkout — operator runs `docker build -f Dockerfile.runner
    //     -t execlaw/runner:dev .` once).
    // Either case logs a warning and falls through to in-process
    // chat so the operator isn't stranded.
    let runners_enabled = std::env::var("EXECLAW_RUNNERS_ENABLED")
        .map(|v| !(v == "0" || v.eq_ignore_ascii_case("false")))
        .unwrap_or(true);
    let runner_image =
        std::env::var("EXECLAW_RUNNER_IMAGE").unwrap_or_else(|_| "execlaw/runner:dev".to_owned());
    let (runner_supervisor, runner_launcher) = if runners_enabled {
        // Pull the trait into scope so `launcher.image_present`
        // resolves; the inherent method we want lives behind the
        // trait, not on `BollardRunnerLauncher` directly.
        use execlaw_server::runner_spawn::RunnerLauncher as _;
        match execlaw_server::runner_spawn::BollardRunnerLauncher::new() {
            Ok(launcher) => {
                // 2026-05-02 — autobuild the runner image when the
                // current control-plane binary is newer than the
                // image (or the image is missing). Operators
                // restart the control plane to pick up new code;
                // the runner is part of that surface and shouldn't
                // need a separate `docker build` step they have to
                // remember. Best-effort — production deployments
                // without source on disk fall through to the old
                // "warn + disable" branch when no Dockerfile is
                // findable.
                let _ = ensure_runner_image_fresh(&runner_image).await;
                if launcher.image_present(&runner_image).await {
                    tracing::info!(
                        image = %runner_image,
                        "runner supervisor enabled"
                    );
                    // Build the spec template the supervisor will
                    // use for every lazy spawn (and for the
                    // controller prewarm). `group_id` +
                    // `spawn_secret_hex` get filled in per-spawn
                    // by `ensure_runner`; everything else is
                    // reused.
                    let rpc_url_template = std::env::var("EXECLAW_RPC_URL")
                        .unwrap_or_else(|_| "ws://host.docker.internal:3031".to_owned());
                    let runner_network = std::env::var("EXECLAW_RUNNER_NETWORK").ok();
                    let spec_template = execlaw_server::runner_spawn::RunnerSpec {
                        group_id: String::new(),
                        image: runner_image.clone(),
                        spawn_secret_hex: String::new(),
                        rpc_url: rpc_url_template,
                        // Filled per-turn by ensure_runner from
                        // the resolved inference URL. We seed a
                        // sensible default here so a runner
                        // spawned before the chat path's per-turn
                        // override can still answer health checks.
                        inference_url: "http://host.docker.internal:8101/v1".into(),
                        memory_bytes: Some(2 * 1024 * 1024 * 1024),
                        network: runner_network,
                        env: vec![("RUST_LOG".into(), "info,execlaw_runner=debug".into())],
                    };
                    let launcher_arc = std::sync::Arc::new(launcher)
                        as std::sync::Arc<dyn execlaw_server::runner_spawn::RunnerLauncher>;
                    let supervisor = execlaw_server::runner_supervisor::RunnerSupervisor::new(
                        db.clone(),
                        events.clone(),
                    )
                    .with_launcher(launcher_arc.clone(), spec_template);
                    (Some(supervisor), Some(launcher_arc))
                } else {
                    tracing::warn!(
                        image = %runner_image,
                        "runner image not found locally; supervisor disabled. \
                         Build it once with: docker build -f Dockerfile.runner \
                         -t execlaw/runner:dev . (or override via \
                         EXECLAW_RUNNER_IMAGE=...)"
                    );
                    (None, None)
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Docker unreachable; runner supervisor disabled. \
                     Falling back to in-process chat path. Set \
                     EXECLAW_RUNNERS_ENABLED=0 to silence this warning."
                );
                (None, None)
            }
        }
    } else {
        tracing::info!("runner supervisor disabled via EXECLAW_RUNNERS_ENABLED=0");
        (None, None)
    };

    // Construct the research supervisor BEFORE AppState so the
    // admin endpoints (which carry an `AppState` clone) can reach
    // its `cancel_tokens` registry. C6c — this is what closes the
    // gap where the cancel admin endpoint flipped the DB row but
    // the gather phase kept burning tokens.
    let research_workspace = execlaw_server::research::ResearchWorkspace::new(
        execlaw_server::research::ResearchWorkspace::default_root(),
    );
    // Channel-keyed transport registry. Phase B refactor: just a
    // `channel → (plugin_id, icon)` lookup. Auto-bridge sites
    // (text-reply bridge, attachment fan-out, research-PDF
    // dispatch) consult it for the channel's owning plugin id +
    // dispatch via `plugin_host.call_tool("<channel>.send_message",
    // ...)` directly. No TransportApi adapter layer.
    let host_transports = {
        let mut reg = execlaw_server::transport_registry::HostTransportRegistry::new();
        const SIGNAL_MANIFEST: &str = include_str!("../../../plugins/signal/plugin.toml");
        let signal_icon = execlaw_plugin_sdk::manifest::PluginManifest::parse(SIGNAL_MANIFEST)
            .ok()
            .and_then(|m| m.transport.and_then(|t| t.icon))
            .unwrap_or_else(|| "phone".to_owned());
        reg.register(
            "signal",
            execlaw_server::transport_registry::ChannelInfo {
                plugin_id: "signal".into(),
                icon: signal_icon,
            },
        );
        const WHATSAPP_MANIFEST: &str = include_str!("../../../plugins/whatsapp/plugin.toml");
        let whatsapp_icon = execlaw_plugin_sdk::manifest::PluginManifest::parse(WHATSAPP_MANIFEST)
            .ok()
            .and_then(|m| m.transport.and_then(|t| t.icon))
            .unwrap_or_else(|| "whatsapp".to_owned());
        reg.register(
            "whatsapp",
            execlaw_server::transport_registry::ChannelInfo {
                plugin_id: "whatsapp".into(),
                icon: whatsapp_icon,
            },
        );
        const SLACK_MANIFEST: &str = include_str!("../../../plugins/slack/plugin.toml");
        let slack_icon = execlaw_plugin_sdk::manifest::PluginManifest::parse(SLACK_MANIFEST)
            .ok()
            .and_then(|m| m.transport.and_then(|t| t.icon))
            .unwrap_or_else(|| "slack".to_owned());
        reg.register(
            "slack",
            execlaw_server::transport_registry::ChannelInfo {
                plugin_id: "slack".into(),
                icon: slack_icon,
            },
        );
        const SMS_SOCKET_MANIFEST: &str = include_str!("../../../plugins/sms-socket/plugin.toml");
        let sms_socket_icon =
            execlaw_plugin_sdk::manifest::PluginManifest::parse(SMS_SOCKET_MANIFEST)
                .ok()
                .and_then(|m| m.transport.and_then(|t| t.icon))
                .unwrap_or_else(|| "phone".to_owned());
        reg.register(
            "sms",
            execlaw_server::transport_registry::ChannelInfo {
                plugin_id: "sms-socket".into(),
                icon: sms_socket_icon,
            },
        );
        const DISCORD_MANIFEST: &str = include_str!("../../../plugins/discord/plugin.toml");
        let discord_icon = execlaw_plugin_sdk::manifest::PluginManifest::parse(DISCORD_MANIFEST)
            .ok()
            .and_then(|m| m.transport.and_then(|t| t.icon))
            .unwrap_or_else(|| "discord".to_owned());
        reg.register(
            "discord",
            execlaw_server::transport_registry::ChannelInfo {
                plugin_id: "discord".into(),
                icon: discord_icon,
            },
        );
        tracing::info!(channels = reg.len(), "host-transport registry populated");
        reg
    };

    let research_supervisor = execlaw_server::research::ResearchSupervisor::new(
        db.clone(),
        inference.clone(),
        research_workspace.clone(),
        events.clone(),
    )
    .with_host_transports(Some(host_transports.clone()))
    .with_plugin_host(Some(plugin_host.clone()));

    // Phase C (2026-05-03) — auto-capture worker. The summarizer
    // talks to `BackendPurpose::Small` so the standard turn isn't
    // contended; the worker gates internally on
    // `config_skills.auto_capture_enabled` (default OFF) so an
    // operator who hasn't opted in never burns inference cycles.
    //
    // 2026-05-13 — no model_id parameter: the worker's
    // `InferenceSummarizer` reads `resolved.model_id` from the
    // same DB row that supplied the endpoint, so caching a model
    // string at construction time is no longer a drift source.
    let (skill_capture_sink, _skill_capture_handle) =
        execlaw_server::skill_capture_runtime::spawn_capture_worker(
            db.clone(),
            skill_store.clone(),
            inference.clone(),
        );
    // Phase D.3 — reuse-update worker. Same shape; gates on
    // `config_skills.reuse_update_enabled` (default OFF).
    let (reuse_update_sink, _reuse_update_handle) =
        execlaw_server::skill_capture_runtime::spawn_reuse_update_worker(
            db.clone(),
            skill_store.clone(),
            inference.clone(),
        );

    let state = execlaw_server::AppState {
        db: db.clone(),
        // Stash the exact config we just opened with so the
        // factory-reset endpoint can close-and-rebuild at the same
        // path with the same encryption posture. See
        // `crates/core/src/db.rs::Database::rebuild_to_empty`.
        db_config: std::sync::Arc::new(db_config),
        config: config.clone(),
        signer,
        refresh_store,
        events: events.clone(),
        event_log_hmac_key: hmac_key,
        inference,
        plugin_host,
        webauthn,
        mcp_host,
        backend_supervisor,
        sidecar_supervisor: sidecar_supervisor.clone(),
        host_transports,
        voice_sessions,
        voice_runtime,
        turn_cancel: execlaw_server::turn_cancel::TurnCancellationRegistry::new(),
        runner_supervisor: runner_supervisor.clone(),
        research_supervisor: Some(research_supervisor.clone()),
        skill_capture: skill_capture_sink,
        reuse_update: reuse_update_sink,
    };

    // Phase B (channel-plugin surface): wire the host-capabilities
    // arc into the script engine NOW that AppState exists. The
    // four Rhai bindings (`sidecar_url`, `ws_subscribe`,
    // `host_route_inbound`, plus the helper plumbing) start
    // returning real results from this call onward; before this
    // they error cleanly with "host capabilities not wired."
    {
        let caps =
            execlaw_server::host_caps_impl::AppStateHostCapabilities::new(state.clone()).into_arc();
        if state.plugin_host.attach_host_capabilities(caps).is_err() {
            tracing::warn!("host_caps already attached — second wiring call ignored");
        } else {
            tracing::info!("script-tier host capabilities attached");
        }
    }

    // Phase-7 background workers — run for the lifetime of the
    // process. The sweepers carry their own intervals; the server
    // owns the stop signal so a SIGTERM can drain everything.
    let sweep_stop = std::sync::Arc::new(tokio::sync::Notify::new());
    let log_sweeper = execlaw_core::log_retention::LogRetentionSweeper::new(db.clone());
    {
        let stop = sweep_stop.clone();
        tokio::spawn(async move { log_sweeper.run(stop).await });
    }
    // 2026-04-29 — event retention: deletes `state_events` rows past
    // the operator-configured `history_retention_days` window.
    // Pinned + ephemeral conversations are exempt (the latter is
    // owned by EphemeralSweeper). Reads the policy live each tick so
    // a Settings change takes effect within one cadence.
    let event_sweeper = execlaw_core::event_retention::EventRetentionSweeper::new(db.clone());
    {
        let stop = sweep_stop.clone();
        tokio::spawn(async move { event_sweeper.run(stop).await });
    }
    let ephemeral_sweeper = execlaw_core::ephemeral_sweeper::EphemeralSweeper::new(db.clone());
    {
        let stop = sweep_stop.clone();
        tokio::spawn(async move { ephemeral_sweeper.run(stop).await });
    }
    // Phase 7 hardening — keeps `state_refresh_tokens` from growing
    // without bound. Expired rows are already rejected at consume
    // time; this just trims the table on an hourly cadence.
    let refresh_sweeper = execlaw_core::refresh_tokens::RefreshTokenSweeper::new(db.clone());
    {
        let stop = sweep_stop.clone();
        tokio::spawn(async move { refresh_sweeper.run(stop).await });
    }
    // Phase 9 — OAuth proactive token refresh + pending-CSRF GC for
    // every plugin-configured `[[oauth_accounts]]` entry. Runs every
    // 60 s; refreshes tokens within 10 min of expiry; purges
    // expired authorize-flow CSRF rows. No-op when no clients are
    // configured.
    let oauth_sweeper = execlaw_server::oauth_sweeper::OauthSweeper::new(db.clone());
    {
        let stop = sweep_stop.clone();
        tokio::spawn(async move { oauth_sweeper.run(stop).await });
    }
    // Phase 10 + 11.C — wall-clock-aligned cron tick that fires due
    // routines. Dispatch routes through chats::dispatch_routine_turn
    // so a routine fire is behaviourally identical to the controller
    // typing the prompt manually. Falls back to stub turn when no
    // inference backend is wired. See MIGRATION_PLAN §5.6.3.
    let _routine_runner = execlaw_server::routine_runner::spawn(state.clone());

    // C3 — research subsystem supervisor. Picks up `Pending` rows
    // from `state_research_jobs`, claims them atomically, and spawns
    // a per-job runner that drives plan / gather / synthesize.
    // Workspace dir defaults to `~/.execlaw/research/`. Model id
    // mirrors the chat path's default; per-purpose routing lands
    // when the runner grows modality-aware backend selection.
    {
        let stop = sweep_stop.clone();
        let supervisor = research_supervisor.clone();
        tokio::spawn(async move { supervisor.run(stop).await });

        // C6 — research-retention sweeper. Purges terminal rows
        // past the global `history_retention_days` cutoff and
        // removes their workspace dirs. Hourly tick by default.
        let retention_sweeper = execlaw_server::research::ResearchRetentionSweeper::new(
            state.db.clone(),
            research_workspace,
        );
        let stop = sweep_stop.clone();
        tokio::spawn(async move { retention_sweeper.run(stop).await });

        // 2026-05-03 (rev 7) — clarification listener. Subscribes
        // to UiEvent::ResearchAwaitingInput and wakes the agent in
        // the affected conversation so it can relay the planner's
        // question to the user. Replaces the polling-on-
        // research_start fast path; no shutdown signal needed —
        // the task exits cleanly when the event-bus subscriber
        // returns Closed at server teardown.
        let _clarification_listener =
            execlaw_server::research::clarification_listener::spawn(state.clone());
    }

    // Phase 10 closure — purge state_routine_runs rows past the
    // 90-day retention window every hour. Mirrors the existing
    // log/ephemeral/refresh sweepers. Pending rows are preserved
    // regardless of age (a crashed mid-fire row stays visible).
    {
        let stop = sweep_stop.clone();
        let routine_run_sweeper =
            execlaw_core::routine_run_retention::RoutineRunRetentionSweeper::new(db.clone());
        tokio::spawn(async move { routine_run_sweeper.run(stop).await });
    }

    // Phase 12.C — backend supervisor reconcile loop. Only spawns
    // if the Docker connect succeeded above; otherwise managed-mode
    // backends are inert and the SPA shows a "Docker unreachable"
    // notice on the Backends page status pill.
    if let Some(sup) = state.backend_supervisor.clone() {
        let stop = sweep_stop.clone();
        tokio::spawn(async move { sup.run(stop).await });
    }

    // Phase 2b — sidecar supervisor's reconcile loop. Same
    // start-only-if-Some pattern as backend_supervisor; on a
    // Docker-less host this is a no-op and the SPA's Sidecars
    // page reports the 503.
    if let Some(sup) = state.sidecar_supervisor.clone() {
        let stop = sweep_stop.clone();
        tokio::spawn(async move { sup.run(stop).await });
    }

    // Phase B lifecycle: fire each script plugin's optional
    // `on_enable()` Rhai hook. The sidecar supervisor was just
    // spawned above, so a transport plugin's WS-subscribe call
    // sees a live supervisor when it looks up `sidecar_url`.
    // Plugins whose sidecars are still spinning up handle the
    // None case gracefully (sidecar_url returns None → on_enable
    // logs + bails; the WS subscription ends up missing for that
    // boot — operator restart fixes it). A future tightening
    // would wait for sidecar healthy before firing, but that
    // adds blocking I/O to the boot path.
    {
        let plugin_host = state.plugin_host.clone();
        let state_for_wire = state.clone();
        let db_for_wire = db.clone();
        tokio::spawn(async move {
            // Small delay so the supervisor's first reconcile
            // pass has a chance to publish ports. Capped — if
            // sidecars aren't up by then we still fire on_enable
            // and let the plugin handle the None.
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            plugin_host.fire_on_enable_for_all().await;

            // 2026-05-18 — Phase 8 wiring for the python-sandbox
            // plugin. Constructs the PythonSandboxService against
            // the kernel-gateway sidecar's published port and
            // registers the four python.* tools as host-implemented
            // builtins. No-op when the plugin isn't installed or
            // the sidecar isn't healthy (warning logged from inside).
            //
            // Service is held in a `static` so its OutputWatcher
            // (notify OS thread + tokio timer) stays alive for the
            // server's lifetime. Drop happens on process exit.
            if let Some(sup) = state_for_wire.sidecar_supervisor.as_ref() {
                let now = chrono::Utc::now().timestamp();
                match execlaw_server::python_sandbox::wire_python_sandbox(
                    sup,
                    plugin_host.registry(),
                    &db_for_wire,
                    now,
                )
                .await
                {
                    Ok(Some(svc)) => {
                        // Stash so Drop doesn't run mid-server. The
                        // OnceLock ensures we only ever wire once.
                        let _ = PYTHON_SANDBOX_SERVICE.set(svc);
                    }
                    Ok(None) => {
                        // wire helper already logged the reason.
                    }
                    Err(e) => {
                        tracing::warn!(
                            ?e,
                            "python_sandbox wiring failed; python.* tools unavailable this boot"
                        );
                    }
                }
            }
        });
    }

    // Boot reconcile pass — merges any stale UnknownPending
    // principals shadowing a "My identities" mapping that was
    // added after the first cold-contact for that handle. Cheap
    // (single principals scan) and idempotent; safe to run on
    // every boot.
    match execlaw_server::principal_admit::reconcile_against_my_identities(&state.db) {
        Ok(report) if !report.merged.is_empty() => {
            tracing::info!(
                merged = report.merged.len(),
                bindings_repointed = report.bindings_repointed,
                conversations_repointed = report.conversations_repointed,
                "boot reconcile merged stale UnknownPending principals into canonical claimants",
            );
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, "boot reconcile failed; will retry on next add_my_identifier");
        }
    }

    // Phase B (signal v0.4.0+): the inbound consumer is now
    // plugin-owned. The signal plugin's `on_enable()` Rhai hook
    // fires from `PluginHost::hydrate` and calls `ws_subscribe`
    // against the supervised sidecar's `/v1/receive/<number>`
    // endpoint. The host gets out of the way — no spawn here.

    // Phase 13.D — voice-session reaper. Drops idle voice sessions
    // (operator closed the tab mid-mic) every REAP_INTERVAL so the
    // registry doesn't accumulate ghost entries. Both the
    // VoiceSessionRegistry and VoiceRuntime are passed in so future
    // versions can sweep both maps in lockstep.
    execlaw_server::voice_reaper::spawn(
        state.voice_sessions.clone(),
        state.voice_runtime.clone(),
        sweep_stop.clone(),
    );

    // Phase 16 — runner-supervisor reaper + controller prewarm.
    // Both opt-in via `runner_supervisor.is_some()`. Reaper sweeps
    // every REAP_INTERVAL (60s by default), wipes idle non-
    // controller runners' workspace volumes, and runs the per-turn
    // max-duration watchdog. Prewarm fires once on boot to spawn
    // the controller's runner so the first chat doesn't pay
    // cold-start latency.
    if let (Some(sup), Some(launcher)) = (runner_supervisor.as_ref(), runner_launcher.as_ref()) {
        let reaper_sup = sup.clone();
        let reaper_launcher = launcher.clone();
        let stop = sweep_stop.clone();
        tokio::spawn(async move {
            tracing::info!(
                interval_secs = execlaw_server::runner_supervisor::REAP_INTERVAL.as_secs(),
                ttl_secs = execlaw_server::runner_supervisor::IDLE_TTL.as_secs(),
                max_turn_secs = execlaw_server::runner_supervisor::MAX_TURN_DURATION.as_secs(),
                "runner supervisor reaper running",
            );
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(
                        execlaw_server::runner_supervisor::REAP_INTERVAL,
                    ) => {
                        let _ = reaper_sup
                            .reap_idle_with_launcher(reaper_launcher.as_ref())
                            .await;
                        reaper_sup.watchdog_pass().await;
                    }
                    _ = stop.notified() => {
                        tracing::info!("runner supervisor reaper stopping");
                        return;
                    }
                }
            }
        });

        // Boot orphan sweep: remove runner workspace volumes
        // whose principal group rows are gone (server crash mid-
        // reap, or operator deleted a group). Best-effort; logs
        // and continues on failure.
        let sweep_sup = sup.clone();
        let sweep_launcher = launcher.clone();
        tokio::spawn(async move {
            sweep_sup.boot_orphan_sweep(sweep_launcher.as_ref()).await;
        });

        // Prewarm the controller's runner. The first time anyone
        // chats with the controller we DON'T want a 1-3s cold
        // spawn delay; the supervisor blocks idle-reap on
        // controller groups by policy so this runner stays hot
        // until shutdown.
        let prewarm_sup = sup.clone();
        let prewarm_launcher = launcher.clone();
        let prewarm_db = db.clone();
        let prewarm_inference = state.inference.clone();
        tokio::spawn(async move {
            // Wait briefly so the WS endpoint is up before the
            // runner phones home. (Axum's `serve` task hasn't
            // necessarily started by the time we get here.)
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let inference_url = match prewarm_inference.resolve(
                &prewarm_db,
                execlaw_core::backends::BackendPurpose::Standard,
            ) {
                Some(c) => c.endpoint.clone(),
                None => {
                    tracing::info!(
                        "prewarm skipped: no inference backend configured (controller runner will spawn lazily on first chat)"
                    );
                    return;
                }
            };

            let image = std::env::var("EXECLAW_RUNNER_IMAGE")
                .unwrap_or_else(|_| "execlaw/runner:dev".to_owned());
            let rpc_url = std::env::var("EXECLAW_RPC_URL").unwrap_or_else(|_| {
                // Default points at the host's loopback; the
                // runner container reaches it via host-gateway.
                "ws://host.docker.internal:3031".to_owned()
            });
            let network = std::env::var("EXECLAW_RUNNER_NETWORK").ok();

            let spec = execlaw_server::runner_spawn::RunnerSpec {
                group_id: String::new(), // filled in by ensure_runner
                image,
                spawn_secret_hex: String::new(), // filled in
                rpc_url,
                inference_url,
                memory_bytes: Some(2 * 1024 * 1024 * 1024),
                network,
                env: vec![("RUST_LOG".into(), "info,execlaw_runner=debug".into())],
            };

            // The web SPA's send_message resolves an absent
            // `sender_principal_id` to the literal string
            // "controller" (see chats::resolve_sender). The chat
            // route's resolve_chat_group then hashes
            // `["controller"]` into the principal-set hash for
            // group `(web, {controller})`. Mirror that exactly so
            // the prewarmed group_id matches the one the chat
            // path will look up on the first send.
            match prewarm_sup
                .prewarm_controller(
                    prewarm_launcher.as_ref(),
                    "controller",
                    spec,
                    std::time::Duration::from_secs(30),
                )
                .await
            {
                Ok(handle) => {
                    tracing::info!(
                        group_id = %handle.group_id,
                        "controller runner prewarmed"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "controller prewarm failed (will spawn lazily on first chat)");
                }
            }
        });
    }

    let app = execlaw_server::routes::build_router(state);
    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    tracing::info!(addr = %config.bind_addr, "execlaw server listening");
    axum::serve(listener, app).await?;
    sweep_stop.notify_waiters();
    Ok(())
}

fn main() -> ExitCode {
    // Hold the tracing-appender guard for the whole process lifetime
    // so the background flush thread sees every event before exit.
    let _tracing_guard = init_tracing();
    // 2026-05-16 — install a panic hook that emits a structured
    // tracing event (full backtrace + payload + location) and
    // aborts. The abort produces a core dump if the host's
    // `ulimit -c` allows; `rust-gdb <execlaw> <core>` then
    // attaches for post-mortem analysis. Without this hook a
    // panic in a tokio worker thread silently prints to stderr
    // and the server keeps running with a corrupt runtime —
    // exactly the failure mode that's hardest to debug.
    install_panic_hook();
    let cli = Cli::parse();
    let result: anyhow::Result<()> = (|| match cli.command {
        Command::Install {
            no_encrypt,
            system,
            skip_migrate,
            bind,
            db,
        } => cmd_install(no_encrypt, system, skip_migrate, bind, db),
        Command::Service { op } => match op {
            ServiceOp::Install { system, bind, db } => service::install(system, bind, db),
            ServiceOp::Start { system } => service::start(system),
            ServiceOp::Stop { system } => service::stop(system),
            ServiceOp::Restart { system } => service::restart(system),
            ServiceOp::Status { system } => service::status(system),
            ServiceOp::Uninstall { system } => service::uninstall(system),
            ServiceOp::Run {
                bind,
                db,
                no_encrypt,
            } => {
                // The Windows path bootstraps its own tokio runtime
                // because StartServiceCtrlDispatcher returns BEFORE
                // we can establish one. The non-Windows path just
                // forwards into cmd_serve.
                #[cfg(windows)]
                {
                    service::windows_runtime_run(
                        bind,
                        db.unwrap_or_else(default_db_path),
                        no_encrypt,
                    )
                }
                #[cfg(not(windows))]
                {
                    let rt = tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()?;
                    rt.block_on(cmd_serve(
                        bind,
                        db.unwrap_or_else(default_db_path),
                        no_encrypt,
                    ))
                }
            }
        },
        Command::Doctor => cmd_doctor(),
        Command::Db { op } => match op {
            DbOp::Migrate { db, no_encrypt } => {
                cmd_db_migrate(db.unwrap_or_else(default_db_path), no_encrypt)
            }
            DbOp::Status { db, no_encrypt } => {
                cmd_db_status(db.unwrap_or_else(default_db_path), no_encrypt)
            }
            DbOp::RepairChecksum { id, db, no_encrypt } => {
                cmd_db_repair_checksum(id, db.unwrap_or_else(default_db_path), no_encrypt)
            }
        },
        Command::Hw { op } => match op {
            HwOp::Rescan => cmd_hw_rescan(),
        },
        Command::Serve {
            bind,
            db,
            no_encrypt,
        } => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(cmd_serve(
                bind,
                db.unwrap_or_else(default_db_path),
                no_encrypt,
            ))
        }
        Command::Replay {
            conversation_id,
            at,
            db,
            no_encrypt,
        } => cmd_replay(
            conversation_id,
            at,
            db.unwrap_or_else(default_db_path),
            no_encrypt,
        ),
        Command::Eval { op } => match op {
            EvalOp::Flag {
                conversation_id,
                range,
                label,
                tags,
                notes,
                db,
                no_encrypt,
            } => cmd_eval_flag(
                conversation_id,
                range,
                label,
                tags,
                notes,
                db.unwrap_or_else(default_db_path),
                no_encrypt,
            ),
            EvalOp::List {
                label,
                db,
                no_encrypt,
            } => cmd_eval_list(label, db.unwrap_or_else(default_db_path), no_encrypt),
        },
        Command::BackfillEvents { db, no_encrypt } => {
            cmd_backfill_events(db.unwrap_or_else(default_db_path), no_encrypt)
        }
        Command::ResignEvents {
            db,
            no_encrypt,
            i_understand_history_will_be_resigned,
        } => cmd_resign_events(
            db.unwrap_or_else(default_db_path),
            no_encrypt,
            i_understand_history_will_be_resigned,
        ),
        Command::Backup { to, db, no_encrypt } => {
            cmd_backup(to, db.unwrap_or_else(default_db_path), no_encrypt)
        }
        Command::Restore {
            from,
            db,
            force,
            no_encrypt,
        } => cmd_restore(from, db.unwrap_or_else(default_db_path), force, no_encrypt),
    })();

    match result {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            // {:#} prints the full anyhow chain (top context + every
            // wrapped source separated by `: `). Without it the user
            // only sees the outermost `with_context` message, which
            // for service-install hides the underlying SCM error.
            eprintln!("execlaw: error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_bind_prefers_cli_over_db() {
        let (bind, src) = resolve_bind(Some("0.0.0.0:9000".into()), Some("127.0.0.1:3031".into()));
        assert_eq!(bind, "0.0.0.0:9000");
        assert_eq!(src, "cli");
    }

    #[test]
    fn resolve_bind_falls_back_to_db_when_no_cli() {
        let (bind, src) = resolve_bind(None, Some("0.0.0.0:8080".into()));
        assert_eq!(bind, "0.0.0.0:8080");
        assert_eq!(src, "config_general");
    }

    #[test]
    fn resolve_bind_falls_back_to_default_when_neither_provided() {
        let (bind, src) = resolve_bind(None, None);
        assert_eq!(bind, "127.0.0.1:3031");
        assert_eq!(src, "default");
    }

    #[test]
    fn resolve_bind_treats_blank_db_value_as_missing() {
        // Defensive — `config_general.bind_address` is NOT NULL in
        // the schema, but a future migration / hand-edit could leave
        // it as whitespace; bind to the safe loopback default rather
        // than passing `""` to TcpListener::bind.
        let (bind, src) = resolve_bind(None, Some("   ".into()));
        assert_eq!(bind, "127.0.0.1:3031");
        assert_eq!(src, "default");
    }
}
