//! Local JSONL telemetry: what each call emitted vs. an estimate of what the model would have read.
//! Writers append whole lines under an exclusive file lock; readers hold a shared lock.
use crate::Out;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs;
use std::io::{BufReader, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone)]
pub struct Entry {
    pub ts: u64,
    pub tool: String,
    pub target: String,
    pub ms: f64,
    pub actual_tokens: u64,
    pub baseline_tokens: u64,
    pub dep_lookups: u64,
    pub ok: bool,
}

/// Longest ledger line read back; longer ones are skipped as corrupt.
const MAX_LINE: usize = 64 << 10;

/// The token estimate used everywhere: characters / 4, rounded up.
pub fn tokens(s: &str) -> usize {
    s.chars().count().div_ceil(4)
}

fn repo_root(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find(|d| d.join(".git").exists())
        .unwrap_or(cwd)
        .to_path_buf()
}

fn home_file() -> Option<PathBuf> {
    crate::dep::home().map(|h| h.join(".turbocharger").join("stats.jsonl"))
}

/// `$TC_STATS_FILE` if set (one ledger across repos), else `<repo>/.turbocharger/stats.jsonl`.
pub fn path(cwd: &Path) -> PathBuf {
    match std::env::var_os("TC_STATS_FILE") {
        Some(f) => PathBuf::from(f),
        None => repo_root(cwd).join(".turbocharger").join("stats.jsonl"),
    }
}

fn append(file: &Path, line: &str) -> std::io::Result<()> {
    let dir = file.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let gi = dir.join(".gitignore");
    // Self-ignore only our own dir: a TC_STATS_FILE elsewhere must not hide a whole repo from git.
    if dir.file_name().is_some_and(|n| n == ".turbocharger") && !gi.exists() {
        fs::write(
            gi,
            "# created by turbocharger: keeps stats out of git status\n*\n",
        )?;
    }
    let mut f = fs::OpenOptions::new()
        .read(true)
        .create(true)
        .append(true)
        .open(file)?;
    f.lock()?;
    // A line cut short by a crash must not swallow ours.
    let mut buf = Vec::with_capacity(line.len() + 2);
    if f.metadata()?.len() > 0 {
        let mut last = [0u8];
        f.seek(SeekFrom::End(-1))?;
        f.read_exact(&mut last)?;
        if last[0] != b'\n' {
            buf.push(b'\n');
        }
    }
    buf.extend_from_slice(line.as_bytes());
    buf.push(b'\n');
    f.write_all(&buf) // one write per record; the lock drops with `f`
}

/// Append one entry; repo-local first, user home if the repo isn't writable. Never fails the caller.
pub fn record(cwd: &Path, e: &Entry) {
    let line = serde_json::to_string(e).unwrap_or_default();
    if append(&path(cwd), &line).is_err() {
        if let Some(h) = home_file() {
            let _ = append(&h, &line);
        }
    }
}

/// Entries plus the number of lines skipped as corrupt (bad JSON, non-UTF-8, out of range, too long).
fn read(file: &Path) -> std::io::Result<(Vec<Entry>, usize)> {
    let f = fs::File::open(file)?;
    f.lock_shared()?;
    let mut r = BufReader::new(&f);
    let (mut es, mut bad) = (Vec::new(), 0);
    let mut buf = Vec::new();
    while let Some(fits) = crate::mcp::read_line_bounded(&mut r, MAX_LINE, &mut buf)? {
        let line = buf.trim_ascii();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<Entry>(line) {
            Ok(e) if fits => es.push(e),
            _ => bad += 1,
        }
    }
    Ok((es, bad))
}

fn k(n: u64) -> String {
    if n >= 10_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Days since 1970-01-01 → (y, m, d). (Howard Hinnant's civil_from_days.)
fn ymd(secs: u64) -> String {
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}-{m:02}-{d:02}")
}

fn sum(es: &[&Entry], f: impl Fn(&Entry) -> u64) -> u64 {
    es.iter().fold(0u64, |a, e| a.saturating_add(f(e)))
}

fn plain(json: serde_json::Value, text: String) -> Out {
    Out {
        json,
        text,
        baseline_bytes: 0,
        dep_lookups: 0,
        ok: true,
    }
}

/// Empty the ledger `path(cwd)` resolves to, under the writers' lock. Only that file: the home
/// ledger is reset by pointing `TC_STATS_FILE` at it.
fn reset(cwd: &Path) -> Result<Out> {
    let file = path(cwd);
    let res = fs::OpenOptions::new()
        .write(true)
        .open(&file)
        .and_then(|f| {
            f.lock()?;
            f.set_len(0)
        });
    match res {
        Ok(()) => Ok(plain(
            json!({"reset": true, "file": file}),
            format!("stats reset ({})\n", file.display()),
        )),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let mut text = format!("no stats to reset at {}\n", file.display());
            if let Some(h) = home_file().filter(|h| *h != file && h.is_file()) {
                text.push_str(&format!(
                    "{} was not touched; to reset it: TC_STATS_FILE={} tc stats --reset\n",
                    h.display(),
                    h.display()
                ));
            }
            Ok(plain(json!({"reset": false, "file": file}), text))
        }
        Err(e) => Err(e).with_context(|| format!("resetting {}", file.display())),
    }
}

pub fn run(cwd: &Path, reset_it: bool) -> Result<Out> {
    if reset_it {
        return reset(cwd);
    }
    let mut file = path(cwd);
    let mut note = "";
    let (es, bad) = match read(&file) {
        Ok(x) => x,
        // Records fall back to the home ledger when the repo isn't writable: show that one.
        Err(e) if e.kind() == ErrorKind::NotFound => match home_file()
            .filter(|h| std::env::var_os("TC_STATS_FILE").is_none() && *h != file && h.is_file())
        {
            Some(h) => {
                file = h;
                note = " (home ledger; this repo has none)";
                read(&file).with_context(|| format!("reading {}", file.display()))?
            }
            None => (vec![], 0),
        },
        Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
    };
    let skipped = if bad > 0 {
        format!("  skipped       {bad} corrupt ledger lines\n")
    } else {
        String::new()
    };
    if es.is_empty() {
        let text = format!(
            "no stats yet ({}{note}). Every `tc zoom` / `tc dep` call is recorded unless --no-stats.\n{skipped}",
            file.display()
        );
        return Ok(plain(
            json!({"file": file, "calls": 0, "skipped_lines": bad}),
            text,
        ));
    }
    let all: Vec<&Entry> = es.iter().collect();
    let n = es.len();
    let actual = sum(&all, |e| e.actual_tokens);
    let base = sum(&all, |e| e.baseline_tokens);
    let bin_calls = es.iter().filter(|e| e.tool == "dep.bin").count();
    let lookups = sum(&all, |e| {
        if e.tool == "dep.bin" {
            0
        } else {
            e.dep_lookups
        }
    });
    let mut ms: Vec<f64> = es.iter().map(|e| e.ms).collect();
    ms.sort_by(|a, b| a.total_cmp(b));
    let avg = ms.iter().sum::<f64>() / n as f64;
    let p50 = ms[n / 2];
    let max = ms[n - 1];
    let saved = base as i128 - actual as i128;
    let mult = if actual > 0 {
        base as f64 / actual as f64
    } else {
        0.0
    };
    let mut tools: Vec<(String, Vec<&Entry>)> = Vec::new();
    for e in &es {
        match tools.iter_mut().find(|t| t.0 == e.tool) {
            Some(t) => t.1.push(e),
            None => tools.push((e.tool.clone(), vec![e])),
        }
    }
    tools.sort_by(|a, b| a.0.cmp(&b.0));
    let mut text = format!(
        "Turbocharger stats  {}{note}  ({n} calls since {})\n",
        file.display(),
        ymd(es.iter().map(|e| e.ts).min().unwrap_or(0))
    );
    text.push_str(&format!(
        "  avg latency   {avg:.1} ms  (p50 {p50:.1}, max {max:.1})\n"
    ));
    text.push_str(&format!(
        "  tokens        emitted {}  vs est. baseline {}  → net saved {}\n",
        k(actual),
        k(base),
        if saved >= 0 {
            k(saved.min(u64::MAX as i128) as u64)
        } else {
            format!("-{}", k((-saved).min(u64::MAX as i128) as u64))
        }
    ));
    text.push_str(&format!("  quota         ~{mult:.1}× stretch\n"));
    text.push_str(
        "  baseline      estimate: whole files the shown answers came from, not measured reads\n",
    );
    text.push_str(&format!(
        "  dep lookups   {lookups} read from installed package files (no network, no package code run)\n"
    ));
    if bin_calls > 0 {
        text.push_str(&format!(
            "  --bin calls   {bin_calls} (each ran `<cmd> --help`)\n"
        ));
    }
    text.push_str(&skipped);
    text.push_str("  by tool        calls   avg ms   emitted → baseline   stretch\n");
    let mut jt = Vec::new();
    for (t, v) in &tools {
        let a = sum(v, |e| e.actual_tokens);
        let b = sum(v, |e| e.baseline_tokens);
        let m = v.iter().map(|e| e.ms).sum::<f64>() / v.len() as f64;
        let x = if a > 0 { b as f64 / a as f64 } else { 0.0 };
        text.push_str(&format!(
            "  {t:<14} {:>5}   {m:>6.1}   {:>7} → {:<8}   {x:.1}×\n",
            v.len(),
            k(a),
            k(b)
        ));
        jt.push(json!({"tool": t, "calls": v.len(), "avg_ms": m, "actual_tokens": a, "baseline_tokens": b, "multiplier": x}));
    }
    Ok(plain(
        json!({
            "file": file, "calls": n, "avg_ms": avg, "p50_ms": p50, "max_ms": max, "actual_tokens": actual,
            "baseline_tokens": base, "baseline_is_estimate": true, "saved_tokens": saved.clamp(i64::MIN as i128, i64::MAX as i128) as i64,
            "multiplier": mult, "dep_lookups": lookups, "bin_calls": bin_calls, "skipped_lines": bad, "by_tool": jt,
        }),
        text,
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn dates_and_tokens() {
        assert_eq!(super::ymd(0), "1970-01-01");
        assert_eq!(super::ymd(1_763_856_000), "2025-11-23");
        assert!(super::ymd(u64::MAX).len() >= 10);
        assert_eq!(super::tokens("abcde"), 2);
    }
}
