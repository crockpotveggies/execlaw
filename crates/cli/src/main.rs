//! `execlaw` CLI.
//!
//! Container lifecycle:
//!
//! - `execlaw build`            build the docker image (`docker build ...`)
//! - `execlaw install`          first-run install: migrate + build + start
//! - `execlaw start` / `up`     `docker compose up -d`
//! - `execlaw restart`          `docker compose restart`
//! - `execlaw stop` / `down`    `docker compose down`
//! - `execlaw status`           `docker compose ps`
//! - `execlaw logs`             `docker compose logs` (add `--follow` for -f)
//!
//! Other:
//!
//! - `execlaw doctor`           checks docker + sqlcipher + keyring + bootstrap
//! - `execlaw db migrate`       run pending migrations
//! - `execlaw hw rescan`        (stub — §Phase 2)
//! - `execlaw serve`            run the server directly (for dev; production
//!   uses the container)
//!
//! These subcommands are also exposed as cargo aliases (`.cargo/config.toml`):
//! `cargo start`, `cargo stop`, `cargo restart`, `cargo status`, `cargo logs`,
//! `cargo image` (= build), `cargo bootstrap` (= install).

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(name = "execlaw", version, about = "execlaw control plane CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Build the control-plane docker image (`docker build -f Dockerfile.control-plane ...`).
    Build {
        /// Image tag. Defaults to `execlaw-control-plane:dev`.
        #[arg(long, default_value = "execlaw-control-plane:dev")]
        tag: String,
        /// Dockerfile path. Defaults to `Dockerfile.control-plane`.
        #[arg(long)]
        dockerfile: Option<PathBuf>,
        /// Pass `--no-cache` to the docker build.
        #[arg(long, default_value_t = false)]
        no_cache: bool,
    },
    /// First-run install: run migrations locally, build the image, start the stack.
    ///
    /// This is the one-command bootstrap for a fresh checkout. Equivalent to:
    ///   execlaw db migrate && execlaw build && execlaw start
    Install {
        #[arg(long)]
        compose_file: Option<PathBuf>,
        #[arg(long)]
        dockerfile: Option<PathBuf>,
        #[arg(long, default_value_t = false)]
        no_cache: bool,
        /// Skip `db migrate` — useful if you're installing in a container
        /// where migrations run on first `serve`.
        #[arg(long, default_value_t = false)]
        skip_migrate: bool,
        /// Open the local DB plaintext during migrate (dev only).
        #[arg(long, default_value_t = false)]
        no_encrypt: bool,
    },
    /// Start the control plane (wraps `docker compose up -d`).
    Up {
        /// Path to docker-compose.yml (defaults to the repo root one).
        #[arg(long)]
        compose_file: Option<PathBuf>,
    },
    /// Alias for `up` — start the control plane.
    Start {
        #[arg(long)]
        compose_file: Option<PathBuf>,
    },
    /// Restart the control plane (`docker compose restart`).
    Restart {
        #[arg(long)]
        compose_file: Option<PathBuf>,
    },
    /// Stop the control plane (wraps `docker compose down`).
    Down {
        #[arg(long)]
        compose_file: Option<PathBuf>,
    },
    /// Alias for `down` — stop the control plane.
    Stop {
        #[arg(long)]
        compose_file: Option<PathBuf>,
    },
    /// Show container status (`docker compose ps`).
    Status {
        #[arg(long)]
        compose_file: Option<PathBuf>,
    },
    /// Show container logs (`docker compose logs`).
    Logs {
        #[arg(long)]
        compose_file: Option<PathBuf>,
        /// Follow log output (pass `-f` to docker compose).
        #[arg(long, short, default_value_t = false)]
        follow: bool,
        /// Tail N lines before following.
        #[arg(long, default_value_t = 200)]
        tail: usize,
    },
    /// Run preflight environment checks.
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
        #[arg(long, default_value = "127.0.0.1:3030")]
        bind: String,
        /// Database file. Defaults to `~/.execlaw/execlaw.db`.
        #[arg(long)]
        db: Option<PathBuf>,
        /// If set, open the DB plaintext (dev only).
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
}

#[derive(Debug, Subcommand)]
enum HwOp {
    /// Re-run tier-1 sysfs detection.
    Rescan,
}

fn init_tracing() {
    // Default to JSON logs (matching the architecture-doc §14 promise:
    // JSONL to disk + SQLite) when the EXECLAW_LOG_FORMAT env var is set to
    // "json", and to human-readable output otherwise. Either way, no OTEL,
    // no Langfuse — per the 2026-04-23 locked decision.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let want_json = std::env::var("EXECLAW_LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    let _ = if want_json {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .try_init()
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).try_init()
    };
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

fn open_db(db_path: &Path, no_encrypt: bool) -> anyhow::Result<execlaw_core::Database> {
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
    Ok(execlaw_core::Database::open(&cfg)?)
}

/// Run `docker compose -f <compose_path> <args...>` and verify it succeeded.
fn run_compose(compose_file: Option<PathBuf>, args: &[&str]) -> anyhow::Result<()> {
    let compose_path = compose_file.unwrap_or_else(|| PathBuf::from("docker-compose.yml"));
    anyhow::ensure!(
        compose_path.exists(),
        "docker-compose.yml not found at {}",
        compose_path.display()
    );
    let status = std::process::Command::new("docker")
        .args(["compose", "-f"])
        .arg(&compose_path)
        .args(args)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to invoke docker: {e}"))?;
    anyhow::ensure!(
        status.success(),
        "`docker compose {}` failed",
        args.join(" ")
    );
    Ok(())
}

fn cmd_build(tag: String, dockerfile: Option<PathBuf>, no_cache: bool) -> anyhow::Result<()> {
    let dockerfile_path = dockerfile.unwrap_or_else(|| PathBuf::from("Dockerfile.control-plane"));
    anyhow::ensure!(
        dockerfile_path.exists(),
        "Dockerfile not found at {}",
        dockerfile_path.display()
    );
    let mut cmd = std::process::Command::new("docker");
    cmd.args(["build", "-f"])
        .arg(&dockerfile_path)
        .args(["-t", &tag]);
    if no_cache {
        cmd.arg("--no-cache");
    }
    cmd.arg(".");
    let status = cmd
        .status()
        .map_err(|e| anyhow::anyhow!("failed to invoke docker: {e}"))?;
    anyhow::ensure!(status.success(), "`docker build` failed");
    println!("built image: {tag}");
    Ok(())
}

fn cmd_up(compose_file: Option<PathBuf>) -> anyhow::Result<()> {
    run_compose(compose_file, &["up", "-d"])
}

fn cmd_restart(compose_file: Option<PathBuf>) -> anyhow::Result<()> {
    run_compose(compose_file, &["restart"])
}

fn cmd_down(compose_file: Option<PathBuf>) -> anyhow::Result<()> {
    run_compose(compose_file, &["down"])
}

fn cmd_status(compose_file: Option<PathBuf>) -> anyhow::Result<()> {
    run_compose(compose_file, &["ps"])
}

fn cmd_logs(compose_file: Option<PathBuf>, follow: bool, tail: usize) -> anyhow::Result<()> {
    let tail_arg = tail.to_string();
    let mut args: Vec<&str> = vec!["logs", "--tail", &tail_arg];
    if follow {
        args.push("-f");
    }
    run_compose(compose_file, &args)
}

fn cmd_install(
    compose_file: Option<PathBuf>,
    dockerfile: Option<PathBuf>,
    no_cache: bool,
    skip_migrate: bool,
    no_encrypt: bool,
) -> anyhow::Result<()> {
    println!("==> execlaw install");

    // 1. Migrate the local SQLite (if not skipped — inside the container,
    //    migrations run on first serve).
    if !skip_migrate {
        println!("--> db migrate");
        cmd_db_migrate(default_db_path(), no_encrypt)?;
    } else {
        println!("--  skipping db migrate (--skip-migrate)");
    }

    // 2. Build the image.
    println!("--> build image");
    cmd_build(
        "execlaw-control-plane:dev".to_string(),
        dockerfile,
        no_cache,
    )?;

    // 3. Start the stack.
    println!("--> start stack");
    cmd_up(compose_file)?;

    println!(
        "==> install complete — verify with `execlaw status` or `curl http://localhost:3030/api/health`"
    );
    Ok(())
}

fn cmd_doctor() -> anyhow::Result<()> {
    let mut ok = true;
    let mut report = String::new();

    // 1. Docker.
    match std::process::Command::new("docker")
        .arg("--version")
        .output()
    {
        Ok(out) if out.status.success() => {
            report.push_str(&format!(
                "OK  docker:   {}",
                String::from_utf8_lossy(&out.stdout).trim()
            ));
            report.push('\n');
        }
        _ => {
            ok = false;
            report.push_str("MISS docker:   not found in PATH\n");
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

fn cmd_hw_rescan() -> anyhow::Result<()> {
    let profile = execlaw_container_manager::detect_sysfs(Path::new("/sys"));
    println!("{}", serde_json::to_string_pretty(&profile)?);
    Ok(())
}

async fn cmd_serve(bind: String, db_path: PathBuf, no_encrypt: bool) -> anyhow::Result<()> {
    let db = open_db(&db_path, no_encrypt)?;
    execlaw_core::MigrationRunner::new(&db).apply_all()?;

    let signer = std::sync::Arc::new(execlaw_server::auth::JwtSigner::generate("execlaw".into()));
    let refresh_store = std::sync::Arc::new(execlaw_server::auth::RefreshStore::new());

    // EXECLAW_INFERENCE_URL lets operators point dev servers at a local
    // vLLM / Ollama / OpenArc without editing code. Production boots
    // will read the active Standard deployment from
    // `config_runner_deployments` once the registry API lands.
    let inference_base_url = std::env::var("EXECLAW_INFERENCE_URL").ok();

    let config = std::sync::Arc::new(execlaw_server::ServerConfig {
        bind_addr: bind.parse()?,
        inference_base_url: inference_base_url.clone(),
        ..Default::default()
    });

    let inference = inference_base_url.map(|url| {
        std::sync::Arc::new(execlaw_inference_api::InferenceClient::new(url))
    });

    // Load-or-create the event-log HMAC key. Phase 1 derives it from
    // the same OS keyring entry as the SQLCipher master; a future
    // migration adds a dedicated `event_log_hmac_key` vault row with
    // key_id rotation. For now, reuse the keyring-backed bytes.
    let hmac_key = execlaw_vault::load_or_create_master_key()
        .map(|bytes| std::sync::Arc::new(bytes.to_vec()))
        .ok();

    let state = execlaw_server::AppState {
        db,
        config: config.clone(),
        signer,
        refresh_store,
        events: execlaw_server::EventBus::new(),
        event_log_hmac_key: hmac_key,
        inference,
    };
    let app = execlaw_server::routes::build_router(state);
    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    tracing::info!(addr = %config.bind_addr, "execlaw server listening");
    axum::serve(listener, app).await?;
    Ok(())
}

fn main() -> ExitCode {
    init_tracing();
    let cli = Cli::parse();
    let result: anyhow::Result<()> = (|| match cli.command {
        Command::Build {
            tag,
            dockerfile,
            no_cache,
        } => cmd_build(tag, dockerfile, no_cache),
        Command::Install {
            compose_file,
            dockerfile,
            no_cache,
            skip_migrate,
            no_encrypt,
        } => cmd_install(compose_file, dockerfile, no_cache, skip_migrate, no_encrypt),
        Command::Up { compose_file } | Command::Start { compose_file } => cmd_up(compose_file),
        Command::Restart { compose_file } => cmd_restart(compose_file),
        Command::Down { compose_file } | Command::Stop { compose_file } => cmd_down(compose_file),
        Command::Status { compose_file } => cmd_status(compose_file),
        Command::Logs {
            compose_file,
            follow,
            tail,
        } => cmd_logs(compose_file, follow, tail),
        Command::Doctor => cmd_doctor(),
        Command::Db { op } => match op {
            DbOp::Migrate { db, no_encrypt } => {
                cmd_db_migrate(db.unwrap_or_else(default_db_path), no_encrypt)
            }
            DbOp::Status { db, no_encrypt } => {
                cmd_db_status(db.unwrap_or_else(default_db_path), no_encrypt)
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
    })();

    match result {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("execlaw: error: {e}");
            ExitCode::FAILURE
        }
    }
}
