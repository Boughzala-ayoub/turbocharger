mod dep;
mod init;
mod lang;
mod mcp;
mod stats;
mod zoom;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// What every command produces. Only `main` prints, so CLI, MCP and stats share one path.
pub struct Out {
    pub text: String,
    pub json: serde_json::Value,
    /// Estimated bytes of source the model would otherwise have read: the whole files that
    /// contributed visible output (target, shown callees, the .d.ts/.pyi files of shown answers).
    pub baseline_bytes: usize,
    pub dep_lookups: usize,
    /// false = a clean "not found" answer (exit code 1), not an error.
    pub ok: bool,
}

#[derive(Parser)]
#[command(
    name = "tc",
    version,
    about = "Turbocharger: foveal code views + installed-dependency ground truth for AI coding agents"
)]
struct Cli {
    /// Don't record this call in .turbocharger/stats.jsonl
    #[arg(long, global = true)]
    no_stats: bool,
    /// Print JSON instead of text
    #[arg(long, global = true)]
    json: bool,
    /// Project directory (default: current directory). Also the default root for `tc mcp` tool calls.
    #[arg(long, global = true)]
    cwd: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Zoom: `.` overview, <dir> skeleton, <file> symbol table, <file>:<symbol> foveal view
    Zoom {
        target: String,
        /// Max output tokens, text or --json (0 = unlimited)
        #[arg(long, default_value_t = zoom::DEFAULT_BUDGET)]
        budget: usize,
        /// Only symbols whose name or signature contains PATTERN (Unicode case-insensitive), with
        /// their enclosing class/impl/mod header. For a <dir> or <file>, not `.` or
        /// <file>:<symbol>. A PATTERN starting with `-` needs the equals form: --filter=-term
        #[arg(long, value_name = "PATTERN")]
        filter: Option<String>,
    },
    /// Installed dependency ground truth: signatures, versions, did-you-mean
    Dep {
        package: Option<String>,
        symbol: Option<String>,
        /// Cheat sheet for every third-party import in <file>
        #[arg(long, value_name = "FILE")]
        imports: Option<PathBuf>,
        /// Run `<cmd> --help` (CLI only: this executes <cmd>)
        #[arg(long, value_name = "CMD")]
        bin: Option<String>,
        /// Flag or subcommand to extract from `<cmd> --help` (requires `--bin`)
        #[arg(long, allow_hyphen_values = true, value_name = "FLAG")]
        flag: Option<String>,
    },
    /// Run the MCP stdio server (turbo_zoom, turbo_dep, turbo_stats)
    Mcp,
    /// Add Turbocharger to AGENTS.md/CLAUDE.md and .mcp.json (idempotent)
    Init {
        #[arg(long)]
        agents: bool,
        #[arg(long)]
        mcp: bool,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// Tokens and quota saved so far
    Stats {
        /// Empty this repo's ledger (or $TC_STATS_FILE); never touches another ledger
        #[arg(long)]
        reset: bool,
    },
}

/// The exact string printed for `o`: pretty JSON with `--json`, else the text.
pub fn render(o: &Out, json: bool) -> String {
    if json {
        serde_json::to_string_pretty(&o.json).unwrap_or_default() + "\n"
    } else {
        o.text.clone()
    }
}

/// Run a zoom/dep call, time it and append a stats entry measured on what `render` prints.
pub fn run_recorded(
    cwd: &Path,
    tool: &str,
    target: &str,
    no_stats: bool,
    json: bool,
    f: impl FnOnce() -> Result<Out>,
) -> Result<Out> {
    let t0 = Instant::now();
    let res = f();
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    if std::env::var_os("TC_TIMING").is_some() {
        eprintln!("[tc] {tool} {target}: {ms:.1} ms");
    }
    if !no_stats {
        let (actual, base, lookups, ok) = match &res {
            Ok(o) => (
                stats::tokens(&render(o, json)),
                o.baseline_bytes.div_ceil(4),
                o.dep_lookups,
                o.ok,
            ),
            Err(e) => (stats::tokens(&format!("{e:#}")), 0, 0, false),
        };
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let level = match &res {
            Ok(o) => o
                .json
                .get("level")
                .and_then(|l| l.as_str())
                .map(|l| format!("{tool}.{l}"))
                .unwrap_or(tool.into()),
            Err(_) => tool.into(),
        };
        stats::record(
            cwd,
            &stats::Entry {
                ts,
                tool: level,
                target: target.into(),
                ms,
                actual_tokens: actual as u64,
                baseline_tokens: base as u64,
                dep_lookups: lookups as u64,
                ok,
            },
        );
    }
    res
}

fn main() {
    let cli = Cli::parse();
    let cwd = cli
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().expect("cwd"));
    let cwd = dep::canon(&cwd);
    let res = match &cli.cmd {
        Cmd::Mcp => {
            if let Err(e) = mcp::serve(&cwd, cli.no_stats) {
                eprintln!("tc mcp: {e:#}");
                std::process::exit(1);
            }
            return;
        }
        Cmd::Zoom {
            target,
            budget,
            filter,
        } => run_recorded(&cwd, "zoom", target, cli.no_stats, cli.json, || {
            zoom::run(&cwd, target, *budget, cli.json, filter.as_deref())
        }),
        Cmd::Dep {
            package,
            symbol,
            imports,
            bin,
            flag,
        } => {
            let a = dep::DepArgs {
                package: package.clone(),
                symbol: symbol.clone(),
                imports: imports.clone(),
                bin: bin.clone(),
                flag: flag.clone(),
            };
            let target = [
                package.clone(),
                symbol.clone(),
                imports.as_ref().map(|p| p.display().to_string()),
                bin.clone(),
                flag.clone(),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
            // `dep.bin` even when it fails: `tc stats` counts commands that were run.
            let tool = if a.bin.is_some() { "dep.bin" } else { "dep" };
            run_recorded(&cwd, tool, &target, cli.no_stats, cli.json, || {
                dep::run(&cwd, &a)
            })
        }
        Cmd::Init {
            agents,
            mcp,
            all,
            dry_run,
        } => init::run(&cwd, *agents || *all, *mcp || *all, *dry_run),
        Cmd::Stats { reset } => stats::run(&cwd, *reset),
    };
    match res {
        Ok(o) => {
            print!("{}", render(&o, cli.json));
            if !o.ok {
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("tc: {e:#}");
            std::process::exit(2);
        }
    }
}
