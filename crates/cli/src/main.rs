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
fn init_tracing() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let want_json = std::env::var("EXECLAW_LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    let want_file = std::env::var("EXECLAW_NO_FILE_LOG")
        .map(|v| !matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(true);

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
    let (file_layer, guard) = if want_file {
        let log_dir = std::env::var("EXECLAW_LOG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| default_data_dir().join("logs"));
        if let Err(e) = std::fs::create_dir_all(&log_dir) {
            eprintln!("execlaw: failed to create log dir {log_dir:?}: {e}");
            (None, None)
        } else {
            let file_appender =
                tracing_appender::rolling::daily(&log_dir, "execlaw.jsonl");
            let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
            let layer = tracing_subscriber::fmt::layer()
                .json()
                .with_writer(non_blocking)
                .with_ansi(false)
                .boxed();
            (Some(layer), Some(guard))
        }
    } else {
        (None, None)
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
    while user_msg_idx > 0
        && all_events[user_msg_idx].kind != EventKind::UserMsg
    {
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
            Ok(Some(p)) => TrustLevel::parse(p.trust_level.class_tag())
                .unwrap_or(TrustLevel::UnknownPending),
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
    println!("Events committed by/around the target turn (seq {} → {}):",
        all_events[user_msg_idx].seq.0,
        target_seq,
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
fn cmd_eval_list(
    label: Option<String>,
    db_path: PathBuf,
    no_encrypt: bool,
) -> anyhow::Result<()> {
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
        c.execute_batch(&format!(
            "VACUUM INTO '{}'",
            to_str.replace('\'', "''")
        ))?;
        Ok(())
    })
    .map_err(|e| anyhow::anyhow!("VACUUM INTO: {e}"))?;

    println!(
        "backup: {} -> {} ({} bytes)",
        db_path.display(),
        to.display(),
        std::fs::metadata(&to)
            .map(|m| m.len())
            .unwrap_or_default()
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
    let rp_id = std::env::var("EXECLAW_WEBAUTHN_RP_ID")
        .unwrap_or_else(|_| "localhost".to_owned());
    let origin = std::env::var("EXECLAW_WEBAUTHN_ORIGIN")
        .unwrap_or_else(|_| format!("http://{bind_addr}"));
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

async fn cmd_serve(bind: String, db_path: PathBuf, no_encrypt: bool) -> anyhow::Result<()> {
    let db = open_db(&db_path, no_encrypt)?;
    execlaw_core::MigrationRunner::new(&db).apply_all()?;

    let signer = std::sync::Arc::new(execlaw_server::auth::JwtSigner::generate("execlaw".into()));
    // Phase-7 hardening: refresh tokens persist in SQLite so a
    // server restart no longer signs every operator out.
    let refresh_store =
        std::sync::Arc::new(execlaw_server::auth::RefreshStore::new(db.clone()));

    // EXECLAW_INFERENCE_URL lets operators point dev servers at a local
    // vLLM / Ollama / OpenArc without editing code. Production boots
    // will read the active Standard deployment from
    // `config_runner_deployments` once the registry API lands.
    let inference_base_url = std::env::var("EXECLAW_INFERENCE_URL").ok();

    let config = std::sync::Arc::new(execlaw_server::ServerConfig {
        bind_addr: bind.parse()?,
        ..Default::default()
    });

    // Phase 12.E — bootstrap is the boot-time global URL; per-turn
    // resolution may override it via config_backends rows.
    let bootstrap_inference = inference_base_url.map(|url| {
        std::sync::Arc::new(execlaw_inference_api::InferenceClient::new(url))
    });
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
    plugin_host.hydrate().await.map_err(|e| anyhow::anyhow!("plugin hydrate: {e}"))?;

    // Phase 8a: reflect every built-in + persisted plugin tool into
    // `config_tool_access` so the per-tool trust-class allowlist gate
    // has a row for everything. Idempotent — operator policy from
    // previous boots is preserved; only first-sight tools get the
    // open default.
    {
        let now = chrono::Utc::now().timestamp();
        match execlaw_server::tool_sync::sync_tool_access(&db, &plugin_host, now) {
            Ok(n) => tracing::info!(rows_synced = n, "tool_access sync complete"),
            Err(e) => tracing::warn!(error = %e, "tool_access sync failed; dispatch gate will fall back to allow until next sync"),
        }
    }

    // Phase 7e: build the WebAuthn relying-party from EXECLAW_WEBAUTHN_*
    // env vars. Falling back to localhost:3030 keeps local-dev working
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

    // Phase 8.5: in-memory runner registry. Settings → Runners
    // reads from this; per-turn lifecycle in chats.rs registers
    // start/end. The reaper drops idle non-controller entries.
    let runner_registry = execlaw_server::runner_registry::RunnerRegistry::new();
    let events = execlaw_server::EventBus::new();

    // Phase 12.C — supervisor for managed inference backends. Best-
    // effort connect to the local Docker daemon; if it fails (no
    // Docker, e.g. dev on a host without Docker installed) we fall
    // through to `None` and managed-mode rows just sit `Stopped`
    // until Docker is available. The actual `run()` task is spawned
    // below alongside the other sweepers so it shares `sweep_stop`.
    let backend_supervisor = match execlaw_container_manager::BollardServiceController::connect() {
        Ok(ctrl) => Some(execlaw_server::backend_supervisor::BackendSupervisor::new(
            db.clone(),
            std::sync::Arc::new(ctrl),
        )),
        Err(e) => {
            tracing::warn!(
                "backend supervisor disabled — Docker daemon unreachable: {e}"
            );
            None
        }
    };

    let voice_sessions =
        execlaw_server::voice_session::VoiceSessionRegistry::new(events.clone());

    // Phase 13.C — voice runtime resolves Whisper / Kokoro endpoints
    // from `config_backends` per session. The factories pull URLs +
    // voice id at construction time; a Backends save mid-conversation
    // takes effect on the next utterance (mirrors InferenceResolver).
    let voice_runtime = {
        let db_for_whisper = db.clone();
        let db_for_kokoro = db.clone();
        let db_for_voice = db.clone();
        execlaw_server::voice_runtime::VoiceRuntime::with_http_clients(
            events.clone(),
            std::sync::Arc::new(move || {
                use execlaw_core::backends::{BackendPurpose, BackendStore};
                BackendStore::new(&db_for_whisper)
                    .get(BackendPurpose::VoiceStt)
                    .ok()
                    .flatten()
                    .and_then(|r| r.endpoint)
                    .filter(|s| !s.trim().is_empty())
            }),
            std::sync::Arc::new(move || {
                use execlaw_core::backends::{BackendPurpose, BackendStore};
                BackendStore::new(&db_for_kokoro)
                    .get(BackendPurpose::VoiceTts)
                    .ok()
                    .flatten()
                    .and_then(|r| r.endpoint)
                    .filter(|s| !s.trim().is_empty())
            }),
            std::sync::Arc::new(move || {
                // Voice id from Settings → Personality (default
                // personality row). Falls back to the locked-decision
                // blend `bf_emma+am_michael` when no personality
                // row carries a `voice_id`.
                use execlaw_core::personality::PersonalityStore;
                PersonalityStore::new(&db_for_voice)
                    .get_default()
                    .ok()
                    .and_then(|p| p.voice_id)
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "bf_emma+am_michael".to_owned())
            }),
        )
    };

    let state = execlaw_server::AppState {
        db: db.clone(),
        config: config.clone(),
        signer,
        refresh_store,
        events: events.clone(),
        event_log_hmac_key: hmac_key,
        inference,
        plugin_host,
        webauthn,
        mcp_host,
        runner_registry: runner_registry.clone(),
        backend_supervisor,
        voice_sessions,
        voice_runtime,
    };

    // Phase-7 background workers — run for the lifetime of the
    // process. The sweepers carry their own intervals; the server
    // owns the stop signal so a SIGTERM can drain everything.
    let sweep_stop = std::sync::Arc::new(tokio::sync::Notify::new());
    let log_sweeper =
        execlaw_core::log_retention::LogRetentionSweeper::new(db.clone());
    {
        let stop = sweep_stop.clone();
        tokio::spawn(async move { log_sweeper.run(stop).await });
    }
    let ephemeral_sweeper =
        execlaw_core::ephemeral_sweeper::EphemeralSweeper::new(db.clone());
    {
        let stop = sweep_stop.clone();
        tokio::spawn(async move { ephemeral_sweeper.run(stop).await });
    }
    // Phase 7 hardening — keeps `state_refresh_tokens` from growing
    // without bound. Expired rows are already rejected at consume
    // time; this just trims the table on an hourly cadence.
    let refresh_sweeper =
        execlaw_core::refresh_tokens::RefreshTokenSweeper::new(db.clone());
    {
        let stop = sweep_stop.clone();
        tokio::spawn(async move { refresh_sweeper.run(stop).await });
    }
    // Phase 8.5 — drops idle non-controller runner entries from
    // the in-memory registry every 60s. Controller entries survive
    // by policy (always hot).
    {
        let stop = sweep_stop.clone();
        let reg = runner_registry.clone();
        tokio::spawn(async move { reg.run_reaper(stop).await });
    }

    // Phase 10 + 11.C — wall-clock-aligned cron tick that fires due
    // routines. Dispatch routes through chats::dispatch_routine_turn
    // so a routine fire is behaviourally identical to the controller
    // typing the prompt manually. Falls back to stub turn when no
    // inference backend is wired. See MIGRATION_PLAN §5.6.3.
    let _routine_runner = execlaw_server::routine_runner::spawn(state.clone());

    // Phase 10 closure — purge state_routine_runs rows past the
    // 90-day retention window every hour. Mirrors the existing
    // log/ephemeral/refresh sweepers. Pending rows are preserved
    // regardless of age (a crashed mid-fire row stays visible).
    {
        let stop = sweep_stop.clone();
        let routine_run_sweeper =
            execlaw_core::routine_run_retention::RoutineRunRetentionSweeper::new(
                db.clone(),
            );
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
        Command::Backup { to, db, no_encrypt } => {
            cmd_backup(to, db.unwrap_or_else(default_db_path), no_encrypt)
        }
        Command::Restore {
            from,
            db,
            force,
            no_encrypt,
        } => cmd_restore(
            from,
            db.unwrap_or_else(default_db_path),
            force,
            no_encrypt,
        ),
    })();

    match result {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("execlaw: error: {e}");
            ExitCode::FAILURE
        }
    }
}
