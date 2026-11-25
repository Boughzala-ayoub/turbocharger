//! `tc init`: wire Turbocharger into AGENTS.md / CLAUDE.md and .mcp.json, idempotently.
//! Every target is read and validated before anything is written, and each write is atomic.
use crate::Out;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const START: &str = "<!-- turbocharger:start -->";
const END: &str = "<!-- turbocharger:end -->";

pub const BLOCK: &str = "<!-- turbocharger:start -->
## Turbocharger
- Map code before reading it: `tc zoom .` (repo), `tc zoom <dir>` (API skeleton), `tc zoom <file>` (symbols + line ranges), `tc zoom <file>:<symbol>` (body, callees, installed deps, callers). Looking for one thing? `tc zoom <dir|file> --filter <text>` lists only the matching symbols; don't dump the whole skeleton.
- Before calling any third-party API, check the installed version: `tc dep <package> <Symbol>`; `tc dep --imports <file>` gives a file's whole dependency surface.
- Trust `tc dep` over memory: it reads node_modules, .venv and the Cargo registry on disk. When it answers \"did you mean\", use that name.
- CLI flags: `tc dep --bin <cmd> [--flag <flag>]`. MCP: `turbo_zoom`, `turbo_dep`, `turbo_stats` (server: `tc mcp`).
<!-- turbocharger:end -->";

/// Byte span of the managed block: start of the start-marker line to the end of the end-marker
/// line (its line break excluded). Markers count only as whole lines; anything other than zero
/// or one ordered pair is an error, so a stray marker never makes us delete user text.
fn block_span(text: &str) -> Result<Option<(usize, usize)>> {
    let (mut starts, mut ends) = (Vec::new(), Vec::new());
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        match content.trim() {
            START => starts.push(pos),
            END => ends.push(pos + content.len()),
            _ => {}
        }
        pos += line.len();
    }
    match (starts.as_slice(), ends.as_slice()) {
        ([], []) => Ok(None),
        ([a], [b]) if a < b => Ok(Some((*a, *b))),
        ([a], [b]) if b < a => bail!("the end marker comes before the start marker"),
        _ => bail!(
            "found {} start and {} end marker lines; expected exactly one `{START}` line followed by one `{END}` line",
            starts.len(),
            ends.len()
        ),
    }
}

/// Insert or replace the sentinel block, leaving every byte outside it untouched.
/// `Ok(None)` when nothing changes.
pub fn upsert_block(existing: &str) -> Result<Option<String>> {
    let crlf = existing.contains("\r\n");
    let block = if crlf {
        BLOCK.replace('\n', "\r\n")
    } else {
        BLOCK.to_string()
    };
    let nl = if crlf { "\r\n" } else { "\n" };
    let new = match block_span(existing)? {
        Some((a, b)) => format!("{}{block}{}", &existing[..a], &existing[b..]),
        None => {
            let sep = if existing.is_empty() || existing.ends_with(&format!("{nl}{nl}")) {
                ""
            } else if existing.ends_with('\n') {
                nl
            } else {
                &format!("{nl}{nl}")[..]
            };
            format!("{existing}{sep}{block}{nl}")
        }
    };
    Ok((new != existing).then_some(new))
}

/// Write `content` to `path` through a temp file in the same directory and a rename, so a crash
/// or a full disk never leaves a half-written file. Symlinks are followed (the link survives).
fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target.parent().unwrap_or(Path::new("."));
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.tc-{}.tmp", std::process::id()));
    let res = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
        drop(f);
        if let Ok(m) = fs::metadata(&target) {
            fs::set_permissions(&tmp, m.permissions())?;
        }
        // On Windows this is MoveFileEx(REPLACE_EXISTING): the old file stays intact if it fails.
        fs::rename(&tmp, &target)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res.with_context(|| format!("writing {}", path.display()))
}

struct Planned {
    path: PathBuf,
    content: String,
}

pub fn run(cwd: &Path, agents: bool, mcp: bool, dry: bool) -> Result<Out> {
    let (agents, mcp) = if !agents && !mcp {
        (true, true)
    } else {
        (agents, mcp)
    };
    let mut report: Vec<String> = Vec::new();
    let mut conflicts: Vec<String> = Vec::new();
    let mut plan: Vec<Planned> = Vec::new();
    let verb = |created: bool| match (dry, created) {
        (true, true) => "would create",
        (true, false) => "would update",
        (false, true) => "created",
        (false, false) => "updated",
    };
    // 1. Read and validate every target. Any error here leaves all files as they were.
    if agents {
        let mut seen: Vec<PathBuf> = Vec::new();
        for (name, create) in [("AGENTS.md", true), ("CLAUDE.md", false)] {
            let p = cwd.join(name);
            if fs::symlink_metadata(&p).is_ok() && !p.is_file() {
                bail!("{name} exists and is not a regular file; nothing was changed");
            }
            let exists = p.is_file();
            if !exists && !create {
                continue;
            }
            // CLAUDE.md is often a symlink to AGENTS.md: edit the real file once.
            let real = fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
            if exists && seen.contains(&real) {
                report.push(format!("unchanged {name} (same file as AGENTS.md)"));
                continue;
            }
            seen.push(real);
            let old = if exists {
                fs::read_to_string(&p).with_context(|| format!("reading {name}"))?
            } else {
                String::new()
            };
            match upsert_block(&old)
                .with_context(|| format!("{name}: Turbocharger markers are ambiguous; fix them by hand, nothing was changed"))?
            {
                None => report.push(format!("unchanged {name}")),
                Some(content) => {
                    plan.push(Planned { path: p, content });
                    report.push(format!("{} {name}", verb(!exists)));
                }
            }
        }
    }
    if mcp {
        let p = cwd.join(".mcp.json");
        if fs::symlink_metadata(&p).is_ok() && !p.is_file() {
            bail!(".mcp.json exists and is not a regular file; nothing was changed");
        }
        let exists = p.is_file();
        let mut v: Value = if exists {
            let txt = fs::read_to_string(&p).context("reading .mcp.json")?;
            match serde_json::from_str(&txt) {
                Ok(v) => v,
                Err(e) => bail!(".mcp.json is not valid JSON ({e}); fix it first, Turbocharger will not overwrite it. Nothing was changed"),
            }
        } else {
            json!({})
        };
        let Some(root) = v.as_object_mut() else {
            bail!(".mcp.json must be a JSON object. Nothing was changed")
        };
        let entry = json!({"type": "stdio", "command": "tc", "args": ["mcp"]});
        let servers = root.entry("mcpServers").or_insert_with(|| json!({}));
        let Some(servers) = servers.as_object_mut() else {
            bail!(".mcp.json: mcpServers must be an object. Nothing was changed")
        };
        match servers.get("turbocharger") {
            Some(e) if *e == entry => report.push("unchanged .mcp.json".into()),
            Some(e) => {
                let c = format!(
                    "conflict .mcp.json: mcpServers.turbocharger is customized ({e}); kept it as is. The default is {entry}"
                );
                report.push(c.clone());
                conflicts.push(c);
            }
            None => {
                servers.insert("turbocharger".into(), entry);
                plan.push(Planned {
                    path: p,
                    content: serde_json::to_string_pretty(&v)? + "\n",
                });
                report.push(format!(
                    "{} .mcp.json (mcpServers.turbocharger)",
                    verb(!exists)
                ));
            }
        }
    }
    // 2. Write.
    if !dry {
        for w in &plan {
            atomic_write(&w.path, &w.content)?;
        }
    }
    let text = report.join("\n") + "\n";
    Ok(Out {
        json: json!({"dry_run": dry, "changes": report, "conflicts": conflicts}),
        text,
        baseline_bytes: 0,
        dep_lookups: 0,
        ok: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_is_idempotent_and_replaces_in_place() {
        let once = upsert_block("# Repo\n\nnotes\n").unwrap().unwrap();
        assert!(once.starts_with("# Repo\n\nnotes\n\n<!-- turbocharger:start -->"));
        assert!(upsert_block(&once).unwrap().is_none());
        let stale = once.replace("## Turbocharger", "## Old");
        let fixed = upsert_block(&stale).unwrap().unwrap();
        assert_eq!(fixed, once);
        assert_eq!(BLOCK.lines().count(), 7);
    }

    #[test]
    fn f02_markers_must_be_one_ordered_standalone_pair() {
        let block = |inner: &str| format!("{START}\n{inner}\n{END}");
        // Bytes outside the block survive exactly, including odd whitespace.
        let before = "# Title  \n\n\nkeep\t\n";
        let after = "\n\ntail   no newline";
        let doc = format!("{before}{}{after}", block("old"));
        let new = upsert_block(&doc).unwrap().unwrap();
        assert!(new.starts_with(before) && new.ends_with(after), "{new:?}");
        assert!(new.contains("## Turbocharger") && !new.contains("old"));
        // Appending never trims the user's text.
        assert!(upsert_block("x  ")
            .unwrap()
            .unwrap()
            .starts_with("x  \n\n<!--"));
        // CRLF files get a CRLF block and stay idempotent.
        let crlf = upsert_block("a\r\nb\r\n").unwrap().unwrap();
        assert!(crlf.starts_with("a\r\nb\r\n\r\n<!-- turbocharger:start -->\r\n"));
        assert!(upsert_block(&crlf).unwrap().is_none());
        // A marker quoted inside a line is not a marker.
        let quoted = format!("Use `{START}` and `{END}` to fence it.\n");
        assert!(upsert_block(&quoted).unwrap().unwrap().starts_with(&quoted));
        // Partial, inverted and duplicate markers are refused.
        for bad in [
            format!("{START}\nuser text\n"),
            format!("user text\n{END}\n"),
            format!("{END}\nuser\n{START}\n"),
            format!("{}\nmine\n{}\n", block("a"), block("b")),
            format!("{START}\n{START}\nmine\n{END}\n"),
        ] {
            assert!(upsert_block(&bad).is_err(), "{bad:?}");
        }
    }
}
