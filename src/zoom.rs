//! `tc zoom`: L0 repo overview, L1 directory skeleton, L1.5 file symbol table, L2 foveal symbol view.
use crate::dep::{self, slash, Origin};
use crate::lang::{
    clip, fold, has_word, rs_in_lib_dir, rs_lib_rel, rs_owners, sig_clip, walk_files, word_at,
    Import, Lang, Parsed, Symbol,
};
use crate::Out;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_BUDGET: usize = 8000;

/// `filter`: case-insensitive (Unicode case-folded) substring of a symbol's name or signature. It
/// narrows L1 and L1.5 only; blank means no filter (as an empty MCP string does).
pub fn run(
    cwd: &Path,
    target: &str,
    budget: usize,
    json_mode: bool,
    filter: Option<&str>,
) -> Result<Out> {
    let q = filter.map(|f| fold(f.trim())).filter(|f| !f.is_empty());
    let q = q.as_deref();
    let t = target.trim();
    if t.is_empty() || t == "." || t == "./" || t == ".\\" {
        if q.is_some() {
            bail!("--filter narrows a directory or a file: tc zoom <dir|file> --filter <text>. `.` is the repo overview and has no symbols to filter")
        }
        return Ok(overview(cwd, budget, json_mode));
    }
    let p = cwd.join(t);
    if p.is_dir() {
        return Ok(skeleton(cwd, &p, budget, json_mode, q));
    }
    if p.is_file() {
        return file_table(cwd, &p, budget, json_mode, q);
    }
    // `path:Symbol`: split at the first ':' whose prefix is an existing file (Windows drive letters survive).
    for (i, _) in t.match_indices(':') {
        let f = cwd.join(&t[..i]);
        if f.is_file() {
            if q.is_some() {
                bail!("--filter narrows a directory or a file: tc zoom <dir|file> --filter <text>. `{t}` already names one symbol")
            }
            return symbol_view(cwd, &f, &t[i + 1..], budget, json_mode);
        }
    }
    bail!("no such file or directory: {t} (from {})", cwd.display())
}

/// Does `s` match filter `q` (already `fold`ed)? A data declaration's value
/// (`const names = new Set([...])`, `Gs="Terminal input"`) isn't its signature: only `filter_sig`,
/// the part before the parser's initializer node, counts.
fn matches(s: &Symbol, q: &str) -> bool {
    let sig = s.filter_sig.as_deref().unwrap_or(&s.sig);
    fold(&s.name).contains(q) || fold(sig).contains(q)
}

/// The symbols matching `q`, plus every container above a kept one so a method keeps its
/// class/trait/impl header. The container is the innermost same-named symbol enclosing the member;
/// Rust `impl` methods sit outside their type's span, so there the type is resolved by path and
/// `#[cfg]` (`rs_owners`). A Rust item's `mod` isn't its `parent` (that names an owner type), so
/// the innermost `mod` enclosing the item itself is kept too: an `impl Error for Box` inside
/// `mod error` keeps `mod error`.
fn filter_syms(syms: Vec<Symbol>, q: &str, lang: Lang, file: Option<&Path>) -> Vec<Symbol> {
    let mut keep: Vec<bool> = syms.iter().map(|s| matches(s, q)).collect();
    let inside = |c: &Symbol, m: &Symbol| {
        c.bytes != m.bytes && c.bytes.0 <= m.bytes.0 && m.bytes.1 <= c.bytes.1
    };
    let innermost = |m: &Symbol, ok: &dyn Fn(&Symbol) -> bool| {
        syms.iter()
            .enumerate()
            .filter(|(_, c)| ok(c) && inside(c, m))
            .min_by_key(|(_, c)| c.bytes.1 - c.bytes.0)
            .map(|(j, _)| j)
    };
    // Every owner: `#[cfg]` twins (or twin re-exports) sharing one impl all keep their header.
    let parents_of = |m: &Symbol| -> Vec<usize> {
        let mut out = match m.impl_ty {
            Some(_) => rs_owners(&syms, m, file),
            None => m
                .parent
                .as_deref()
                .and_then(|par| innermost(m, &|c| c.name == par))
                .into_iter()
                .collect(),
        };
        out.extend(
            (lang == Lang::Rs)
                .then(|| innermost(m, &|c| c.kind == "mod"))
                .flatten(),
        );
        out
    };
    // Walk up from each match; nesting is shallow, so this stops after a few steps.
    let mut todo: Vec<usize> = (0..syms.len()).filter(|i| keep[*i]).collect();
    while let Some(i) = todo.pop() {
        for j in parents_of(&syms[i]) {
            if !keep[j] {
                keep[j] = true;
                todo.push(j);
            }
        }
    }
    syms.into_iter()
        .zip(keep)
        .filter_map(|(s, k)| k.then_some(s))
        .collect()
}

/// Filtered views: `  (in Owner)` after a member whose container isn't shown (a Rust `impl` of a
/// type declared in another file), so the line still says what `file:Owner.member` to zoom into.
fn owner_note(s: &Symbol, shown: &[Symbol], q: Option<&str>, file: &Path) -> String {
    let owner_shown = |p: &String| match s.impl_ty {
        Some(_) => !rs_owners(shown, s, Some(file)).is_empty(),
        None => shown.iter().any(|c| &c.name == p),
    };
    match (&s.parent, q) {
        (Some(p), Some(_)) if !owner_shown(p) => format!("  (in {p})"),
        _ => String::new(),
    }
}

fn rel(cwd: &Path, p: &Path) -> String {
    slash(p.strip_prefix(cwd).unwrap_or(p))
}

struct Sec {
    title: String,
    lines: Vec<String>,
    empty: String,
}

/// Characters allowed for a token budget (`0` = unlimited). Tokens are chars / 4
/// (`stats::tokens`), so every budget check measures `chars()`, never UTF-8 bytes.
fn char_cap(budget: usize) -> usize {
    if budget == 0 {
        usize::MAX
    } else {
        budget.saturating_mul(4)
    }
}

/// Length as the budget counts it: characters.
fn clen(s: &str) -> usize {
    s.chars().count()
}

/// Join head, sections (in priority order) and tail within `budget`, headers and cut markers
/// included. Once a section is cut, every later one is cut too, each leaving a marker. Returns
/// the text and how many lines of each section it shows.
fn fit(head: &[String], secs: &[Sec], tail: &str, budget: usize) -> (String, Vec<usize>) {
    let limit = char_cap(budget);
    let marker = |n: usize| format!("  … {n} more lines cut by --budget {budget}\n");
    let title = |s: &Sec| {
        if s.title.is_empty() {
            String::new()
        } else {
            format!("{}\n", s.title)
        }
    };
    let empty = |s: &Sec| {
        if s.empty.is_empty() {
            String::new()
        } else {
            format!("{}\n", s.empty)
        }
    };
    // Everything but the section lines, with room for a marker wherever lines could be cut.
    let mut frame: Vec<String> = head.iter().map(|h| format!("{h}\n")).collect();
    for s in secs {
        frame.push(title(s));
        frame.push(if s.lines.is_empty() {
            empty(s)
        } else {
            marker(s.lines.len())
        });
    }
    frame.push(tail.to_string());
    // Everything fits: no room needed for markers that won't be printed.
    let mut whole: String = head.iter().map(|h| format!("{h}\n")).collect();
    for s in secs {
        whole.push_str(&title(s));
        if s.lines.is_empty() {
            whole.push_str(&empty(s));
        }
        for l in &s.lines {
            whole.push_str(l);
            whole.push('\n');
        }
    }
    whole.push_str(tail);
    if clen(&whole) <= limit {
        return (whole, secs.iter().map(|s| s.lines.len()).collect());
    }
    let fixed: usize = frame.iter().map(|f| clen(f)).sum();
    if fixed > limit {
        // Not even the frame fits: whole lines of it while they fit, at worst one cut line.
        let mut out = String::new();
        for f in frame.iter().filter(|f| !f.is_empty()) {
            if clen(&out) + clen(f) > limit {
                if out.is_empty() {
                    out.extend(f.chars().take(limit));
                }
                break;
            }
            out.push_str(f);
        }
        return (out, vec![0; secs.len()]);
    }
    let mut room = limit - fixed;
    let mut out: String = head.iter().map(|h| format!("{h}\n")).collect();
    let mut shown = Vec::new();
    let mut cut = false;
    for s in secs {
        out.push_str(&title(s));
        if s.lines.is_empty() {
            out.push_str(&empty(s));
            shown.push(0);
            continue;
        }
        let mut k = 0;
        while !cut && k < s.lines.len() && clen(&s.lines[k]) < room {
            room -= clen(&s.lines[k]) + 1;
            out.push_str(&s.lines[k]);
            out.push('\n');
            k += 1;
        }
        if k < s.lines.len() {
            cut = true;
            out.push_str(&marker(s.lines.len() - k));
        } else {
            room += clen(&marker(s.lines.len())); // reserved marker not needed
        }
        shown.push(k);
    }
    out.push_str(tail);
    (out, shown)
}

/// Did `fit` cut anything?
fn was_cut(secs: &[Sec], shown: &[usize]) -> bool {
    secs.iter().zip(shown).any(|(s, k)| *k < s.lines.len())
}

/// Does `v` printed as `--json` prints it (pretty + newline) fit `budget`?
fn json_fits(v: &Value, budget: usize) -> bool {
    serde_json::to_string_pretty(v).is_ok_and(|s| clen(&s) < char_cap(budget))
}

/// A filtered view under `--budget`, where a match outranks the container headers listed above
/// it: the view with headers if it fits whole, else the matches alone if they do, else the cut
/// view with headers if it still shows a match, else the cut matches. `view(headers)` returns
/// (output, fits whole, shows a match).
fn prefer_hits<T>(filtered: bool, view: impl Fn(bool) -> (T, bool, bool)) -> T {
    let (full, whole, hit) = view(true);
    if !filtered || whole {
        return full;
    }
    let (bare, bare_whole, _) = view(false);
    if bare_whole || !hit {
        bare
    } else {
        full
    }
}

/// Shrink `v` until its printed form (`--json`: pretty + newline) fits `budget`: trim the arrays
/// under `keys`, lowest priority first, to the longest prefix that fits. If the fixed fields alone
/// are too big, only `level` and `truncated` remain, and below that `{}`.
fn cap_json(mut v: Value, keys: &[&str], budget: usize) -> Value {
    let limit = char_cap(budget);
    let size = |v: &Value| serde_json::to_string_pretty(v).map_or(usize::MAX, |s| clen(&s) + 1);
    if size(&v) <= limit {
        return v;
    }
    v["truncated"] = json!(true);
    for k in keys {
        let Some(all) = v.get(*k).and_then(Value::as_array).cloned() else {
            continue;
        };
        let (mut lo, mut hi) = (0, all.len());
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            v[*k] = Value::Array(all[..mid].to_vec());
            if size(&v) <= limit {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        v[*k] = Value::Array(all[..lo].to_vec());
        if size(&v) <= limit {
            return v;
        }
    }
    let small = json!({"level": v["level"], "truncated": true});
    if size(&small) <= limit {
        small
    } else {
        json!({})
    }
}

// ---------------------------------------------------------------- L0

const MANIFESTS: &[&str] = &[
    "package.json",
    "pyproject.toml",
    "requirements.txt",
    "setup.py",
    "Pipfile",
    "Cargo.toml",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "go.mod",
    "tsconfig.json",
    "docker-compose.yml",
    "Dockerfile",
    "Makefile",
    "tauri.conf.json",
];

const FRAMEWORKS: &[&str] = &[
    "react",
    "vue",
    "svelte",
    "next",
    "nuxt",
    "vite",
    "express",
    "fastify",
    "koa",
    "@nestjs/core",
    "electron",
    "@tauri-apps/api",
    "@angular/core",
    "solid-js",
    "astro",
    "tailwindcss",
    "jest",
    "vitest",
    "@playwright/test",
    "prisma",
    "@xterm/xterm",
    "django",
    "flask",
    "fastapi",
    "starlette",
    "mcp",
    "torch",
    "tensorflow",
    "numpy",
    "pandas",
    "pytest",
    "pydantic",
    "sqlalchemy",
    "transformers",
    "librosa",
    "tokio",
    "axum",
    "actix-web",
    "tauri",
    "bevy",
    "clap",
    "serde",
    "rocket",
    "warp",
    "tree-sitter",
    "spring-boot",
    "lombok",
    "hibernate",
    "junit",
    "quarkus",
    "micronaut",
    "spring-boot-starter-web",
    "spring-boot-starter-data-jpa",
    "spring-boot-starter-security",
];

/// (source files in a top-level dir, source files per second-level subdir)
type DirCount = (usize, BTreeMap<String, usize>);

fn overview(cwd: &Path, budget: usize, json_mode: bool) -> Out {
    let files = walk_files(cwd, false);
    let mut langs: BTreeMap<&str, usize> = BTreeMap::new();
    let mut dirs: BTreeMap<String, DirCount> = BTreeMap::new();
    let mut manifests: Vec<PathBuf> = Vec::new();
    let mut entries: Vec<String> = Vec::new();
    let mut nsrc = 0;
    for f in &files {
        let r = f.strip_prefix(cwd).unwrap_or(f);
        let name = r.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if MANIFESTS.contains(&name) && r.components().count() <= 4 {
            manifests.push(f.clone());
        }
        if Lang::of(f).is_none() {
            continue;
        }
        nsrc += 1;
        *langs.entry(Lang::label(f)).or_default() += 1;
        let comps: Vec<String> = r
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();
        if comps.len() > 1 {
            let d = dirs.entry(comps[0].clone()).or_default();
            d.0 += 1;
            if comps.len() > 2 {
                *d.1.entry(format!("{}/{}", comps[0], comps[1])).or_default() += 1;
            }
        } else {
            dirs.entry(".".into()).or_default().0 += 1;
        }
        let stem = name.split('.').next().unwrap_or("");
        if (matches!(
            stem,
            "main"
                | "index"
                | "app"
                | "App"
                | "server"
                | "cli"
                | "__main__"
                | "lib"
                | "manage"
                | "wsgi"
                | "asgi"
        ) || name.ends_with("Application.java"))
            && !name.contains(".test.")
            && !name.contains(".d.ts")
        {
            entries.push(slash(r));
        }
    }
    entries.sort_by_key(|e| (e.matches('/').count(), e.clone()));
    manifests.sort_by_key(|m| (m.components().count(), m.clone()));
    let mut frameworks: Vec<String> = Vec::new();
    let mut scripts: Vec<String> = Vec::new();
    let mut baseline = 0;
    let mut manifest_sizes: HashMap<String, usize> = HashMap::new();
    for m in &manifests {
        let Ok(txt) = fs::read_to_string(m) else {
            continue;
        };
        baseline += txt.len();
        manifest_sizes.insert(rel(cwd, m), txt.len());
        let name = m.file_name().unwrap().to_string_lossy();
        let deps: Vec<String> = if name == "package.json" {
            let v: Value = serde_json::from_str(&txt).unwrap_or(Value::Null);
            if let Some(s) = v["scripts"].as_object() {
                let prefix = rel(cwd, m.parent().unwrap());
                let prefix = if prefix.is_empty() {
                    String::new()
                } else {
                    format!("{prefix}: ")
                };
                for (k, c) in s.iter().take(12) {
                    scripts.push(format!(
                        "{prefix}{k} = {}",
                        clip(c.as_str().unwrap_or(""), 60)
                    ));
                }
            }
            for k in ["main", "bin"] {
                if let Some(e) = v[k].as_str() {
                    entries.insert(
                        0,
                        format!("{} ({k})", rel(cwd, &m.parent().unwrap().join(e))),
                    );
                }
            }
            ["dependencies", "devDependencies", "peerDependencies"]
                .iter()
                .flat_map(|k| {
                    v[*k]
                        .as_object()
                        .map(|o| o.keys().cloned().collect::<Vec<_>>())
                        .unwrap_or_default()
                })
                .collect()
        } else {
            let low = txt.to_lowercase();
            FRAMEWORKS
                .iter()
                .filter(|f| has_word(&low, f))
                .map(|f| f.to_string())
                .collect()
        };
        for d in deps {
            if FRAMEWORKS.contains(&d.as_str()) && !frameworks.contains(&d) {
                frameworks.push(d);
            }
        }
        if name == "Makefile" {
            for l in txt.lines() {
                if let Some((t, _)) = l.split_once(':') {
                    if !t.is_empty()
                        && !t.starts_with(['\t', ' ', '.', '#'])
                        && t.chars()
                            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
                    {
                        scripts.push(format!("make {t}"));
                    }
                }
            }
        }
    }
    let readme = ["README.md", "readme.md", "README.rst", "README"]
        .iter()
        .map(|r| cwd.join(r))
        .find(|p| p.is_file());
    let about = readme
        .as_ref()
        .and_then(|r| fs::read_to_string(r).ok())
        .map(|t| {
            baseline += t.len();
            // First prose paragraph (skip headings, badges, html).
            let para: Vec<&str> = t
                .lines()
                .map(str::trim)
                .skip_while(|l| {
                    l.is_empty()
                        || l.starts_with('#')
                        || l.starts_with('[')
                        || l.starts_with('!')
                        || l.starts_with('<')
                })
                .take_while(|l| !l.is_empty())
                .collect();
            clip(&para.join(" "), 220)
        });
    let repo = cwd
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut langs: Vec<(&str, usize)> = langs.into_iter().collect();
    langs.sort_by_key(|l| std::cmp::Reverse(l.1));
    let mut dirs: Vec<(String, DirCount)> = dirs.into_iter().collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.1 .0));
    let mut head = vec![format!(
        "REPO {repo}  ({nsrc} source files, {} files total)",
        files.len()
    )];
    head.push(format!(
        "languages: {}",
        langs
            .iter()
            .map(|(l, n)| format!("{l} {n}"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    if !frameworks.is_empty() {
        head.push(format!("frameworks/libs: {}", frameworks.join(", ")));
    }
    if let Some(a) = about.filter(|a| !a.is_empty()) {
        head.push(format!("about: {a}"));
    }
    head.push(format!(
        "manifests: {}",
        manifests
            .iter()
            .take(12)
            .map(|m| rel(cwd, m))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    let dir_lines: Vec<String> = dirs
        .iter()
        .take(14)
        .map(|(d, (n, subs))| {
            let mut subs: Vec<(&String, &usize)> = subs.iter().collect();
            subs.sort_by_key(|s| std::cmp::Reverse(*s.1));
            let s: Vec<String> = subs
                .iter()
                .take(3)
                .map(|(k, v)| format!("{k} {v}"))
                .collect();
            let s = if s.is_empty() {
                String::new()
            } else {
                format!("  ({})", s.join(", "))
            };
            format!("  {d}/ {n}{s}")
        })
        .collect();
    entries.dedup();
    let secs = vec![
        Sec { title: "dirs (source files):".into(), lines: dir_lines, empty: "  (none)".into() },
        Sec { title: format!("entrypoints: {}", entries.iter().take(10).cloned().collect::<Vec<_>>().join(", ")), lines: vec![], empty: String::new() },
        Sec { title: "scripts:".into(), lines: scripts.iter().take(14).map(|s| format!("  {s}")).collect(), empty: "  (none)".into() },
        Sec { title: "next: tc zoom <dir> | tc zoom <file> | tc zoom <file>:<symbol> | tc dep <pkg> [symbol]".into(), lines: vec![], empty: String::new() },
    ];
    let (text, shown) = fit(&head, &secs, "", budget);
    let json = json!({
        "level": "L0", "repo": repo, "source_files": nsrc, "languages": langs, "frameworks": frameworks,
        "manifests": manifests.iter().map(|m| rel(cwd, m)).collect::<Vec<_>>(),
        "dirs": dirs.iter().map(|(d, (n, _))| json!({"dir": d, "files": n})).collect::<Vec<_>>(),
        "entrypoints": entries, "scripts": scripts, "truncated": was_cut(&secs, &shown),
    });
    let keys = [
        "scripts",
        "entrypoints",
        "dirs",
        "manifests",
        "frameworks",
        "languages",
    ];
    let capped = cap_json(json, &keys, budget);
    let baseline = if json_mode {
        capped
            .get("manifests")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .filter_map(|m| manifest_sizes.get(m))
                    .sum()
            })
            .unwrap_or(0)
    } else {
        baseline
    };
    Out {
        json: capped,
        text,
        baseline_bytes: baseline,
        dep_lookups: 0,
        ok: true,
    }
}

// ---------------------------------------------------------------- L1

/// Shown at L1: exported declarations (fields and variables only as top-level constants), or with
/// a filter every declaration: a filtered view is a search, and an app's code is mostly private
/// (`mod commands { pub fn pty_resize }`).
fn l1_wanted(s: &Symbol, filtered: bool, lang: Lang) -> bool {
    filtered
        || s.exported
            && match s.kind {
                "property" | "var" => {
                    s.name
                        .chars()
                        .all(|c| c.is_uppercase() || c == '_' || c.is_ascii_digit())
                        && s.parent.is_none()
                }
                "const" | "static" => s.parent.is_none(),
                "field" => lang != Lang::Rs, // a Rust struct's fields: L1.5 and filters only
                _ => true,
            }
}

fn strip_export(sig: &str) -> &str {
    sig.strip_prefix("export default ")
        .or_else(|| sig.strip_prefix("export "))
        .unwrap_or(sig)
}

fn skeleton(cwd: &Path, dir: &Path, budget: usize, json_mode: bool, q: Option<&str>) -> Out {
    let files = walk_files(dir, true);
    // (path, line count, bytes, symbols) of each listed file.
    let mut entries: Vec<(String, usize, usize, Vec<Symbol>, &Path)> = Vec::new();
    for f in &files {
        let Some(p) = Parsed::read(f) else { continue };
        let mut syms: Vec<Symbol> = p
            .symbols()
            .into_iter()
            .filter(|s| l1_wanted(s, q.is_some(), p.lang))
            .collect();
        if let Some(q) = q {
            syms = filter_syms(syms, q, p.lang, Some(f));
            if syms.is_empty() {
                continue; // a filtered view lists only the files that match
            }
        }
        entries.push((rel(cwd, f), p.src.lines().count(), p.src.len(), syms, f));
    }
    let hit = |s: &Symbol| q.is_none_or(|q| matches(s, q));
    // Per file, the symbols listed: all of them, or (`headers == false`) the matches alone.
    let lists = |headers: bool| -> Vec<Vec<Symbol>> {
        entries
            .iter()
            .map(|e| e.3.iter().filter(|s| headers || hit(s)).cloned().collect())
            .collect()
    };
    let filtered = entries.iter().any(|e| !e.3.iter().all(hit));
    let (what, empty) = match q {
        Some(q) => (
            format!(
                "public and private declarations matching \"{q}\" in {} files",
                entries.len()
            ),
            format!("(no declaration matches \"{q}\")"),
        ),
        None => ("exported/public API".into(), "(no source files)".into()),
    };
    let head = vec![format!(
        "DIR {}  ({} source files) — {what}, no bodies",
        rel(cwd, dir),
        files.len()
    )];
    let tail = "next: tc zoom <file> | tc zoom <file>:<symbol>\n";
    // Text, and the files it shows lines of (the baseline counts those only).
    let (text, shown_files) = prefer_hits(filtered, |headers| {
        let mut lines = Vec::new();
        let mut of = Vec::new(); // per line: (file index, is a match)
        for (fi, list) in lists(headers).iter().enumerate() {
            lines.push(format!("{}  ({} lines)", entries[fi].0, entries[fi].1));
            of.push((fi, false));
            for s in list {
                let ind = if s.parent.is_some() { "    " } else { "  " };
                let d = s.short_doc();
                let d = if d.is_empty() {
                    String::new()
                } else {
                    format!("  // {d}")
                };
                lines.push(format!(
                    "{ind}{}: {}{}{d}",
                    s.line,
                    sig_clip(strip_export(&s.sig), 180),
                    owner_note(s, list, q, entries[fi].4)
                ));
                of.push((fi, hit(s)));
            }
        }
        let secs = vec![Sec {
            title: String::new(),
            lines,
            empty: empty.clone(),
        }];
        let (text, shown) = fit(&head, &secs, tail, budget);
        let seen = &of[..shown[0]];
        let files: HashSet<usize> = seen.iter().map(|(fi, _)| *fi).collect();
        let any = seen.iter().any(|(_, h)| *h);
        ((text, files), seen.len() == of.len(), any)
    });
    let capped = prefer_hits(filtered, |headers| {
        let lists = lists(headers);
        let jfiles: Vec<Value> = entries
            .iter()
            .zip(&lists)
            .map(|(e, l)| json!({"file": e.0, "symbols": l}))
            .collect();
        let mut json =
            json!({"level": "L1", "dir": rel(cwd, dir), "files": jfiles, "truncated": !headers});
        if let Some(q) = q {
            json["filter"] = json!(q);
        }
        let whole = json_fits(&json, budget);
        let mut capped = cap_json(json, &["files"], budget);
        // `cap_json` keeps whole file entries; the first one left out may still fit in part, if
        // that part reaches a match (not just the container headers above one).
        let kept = capped.get("files").and_then(Value::as_array).map(Vec::len);
        if let Some(k) = kept.filter(|k| *k < lists.len()) {
            let syms = &lists[k];
            let to_hit = syms.iter().position(hit).map_or(1, |i| i + 1);
            let with = |n: usize| {
                let mut v = capped.clone();
                if let Some(a) = v["files"].as_array_mut() {
                    a.push(json!({"file": entries[k].0, "symbols": syms[..n]}));
                }
                v
            };
            let (mut lo, mut hi) = (0, syms.len());
            while lo < hi {
                let mid = (lo + hi).div_ceil(2);
                if json_fits(&with(mid), budget) {
                    lo = mid;
                } else {
                    hi = mid - 1;
                }
            }
            if lo >= to_hit {
                capped = with(lo);
            }
        }
        // Every entry left holds a match: a whole file has one, a partial one reaches one.
        let any = capped
            .get("files")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty());
        (capped, whole, any)
    });
    // Baseline: the files shown, each once (JSON entries are a prefix of `entries`).
    let baseline = if json_mode {
        let n = capped
            .get("files")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        entries.iter().take(n).map(|e| e.2).sum()
    } else {
        shown_files.iter().map(|fi| entries[*fi].2).sum()
    };
    Out {
        json: capped,
        text,
        baseline_bytes: baseline,
        dep_lookups: 0,
        ok: true,
    }
}

// ---------------------------------------------------------------- L1.5

fn import_lines(cwd: &Path, file: &Path, p: &Parsed, imports: &[Import]) -> (Vec<String>, usize) {
    let mut cache: HashMap<String, Option<dep::Pkg>> = HashMap::new();
    let mut lookups = 0;
    let lines = imports
        .iter()
        .map(|imp| {
            let names: Vec<String> = imp
                .names
                .iter()
                .map(|(o, l)| {
                    if o == l {
                        o.clone()
                    } else {
                        format!("{o} as {l}")
                    }
                })
                .collect();
            let mut what = names.join(", ");
            if let Some(a) = &imp.alias {
                what = if what.is_empty() {
                    format!("as {a}")
                } else {
                    format!("{what}, as {a}")
                };
            }
            if imp.wildcard {
                what = if what.is_empty() {
                    "*".into()
                } else {
                    format!("{what}, *")
                };
            }
            let what = clip(&what, 120);
            let tag = match dep::origin(p.lang, &imp.module, file, cwd) {
                Origin::Internal => match internal_file(cwd, file, p.lang, &imp.module) {
                    Some(f) => format!("internal {}", rel(cwd, &f)),
                    None => "internal".into(),
                },
                Origin::Std => "stdlib".into(),
                Origin::External(spec) => {
                    let pkg = cache.entry(spec.clone()).or_insert_with(|| {
                        lookups += 1;
                        if p.lang == Lang::Java {
                            None
                        } else {
                            dep::resolve(
                                file.parent().unwrap_or(cwd),
                                &spec,
                                Some(dep::eco_of(p.lang)),
                            )
                        }
                    });
                    match pkg {
                        Some(pk) => format!("{} {}", pk.name, pk.version),
                        None if p.lang == Lang::Java => {
                            "third-party (version: Maven/Gradle not resolved)".into()
                        }
                        None => "third-party, NOT INSTALLED".into(),
                    }
                }
            };
            format!("  {}: {} → {what}  [{tag}]", imp.line, imp.module)
        })
        .collect();
    (lines, lookups)
}

/// Project file an internal import points at (relative TS/JS paths, Python modules, Java classes).
fn internal_file(cwd: &Path, file: &Path, lang: Lang, module: &str) -> Option<PathBuf> {
    let dir = file.parent()?;
    match lang {
        Lang::Ts | Lang::Tsx if module.starts_with('.') => dep::resolve_rel(dir, module, false),
        Lang::Py => {
            let dots = module.chars().take_while(|c| *c == '.').count();
            let rest = module[dots..].replace('.', "/");
            let bases: Vec<PathBuf> = if dots > 0 {
                let mut b = dir.to_path_buf();
                for _ in 1..dots {
                    b = b.parent()?.to_path_buf();
                }
                vec![b]
            } else {
                dir.ancestors()
                    .take_while(|d| d.starts_with(cwd))
                    .map(Path::to_path_buf)
                    .collect()
            };
            bases.iter().find_map(|b| {
                [
                    b.join(format!("{rest}.py")),
                    b.join(&rest).join("__init__.py"),
                ]
                .into_iter()
                .find(|p| !rest.is_empty() && p.is_file())
            })
        }
        Lang::Java => dep::java_file(file, module),
        _ => None,
    }
}

fn file_table(
    cwd: &Path,
    file: &Path,
    budget: usize,
    json_mode: bool,
    q: Option<&str>,
) -> Result<Out> {
    let Some(p) = Parsed::read(file) else {
        bail!("unsupported or unreadable file: {} (supported: ts tsx js jsx mjs cjs d.ts py pyi rs java)", file.display())
    };
    let all = p.symbols();
    let total = all.len();
    let mut imps = p.imports();
    let syms = match q {
        Some(q) => {
            // Only imports that name the pattern (module, names or alias), resolved after
            // filtering so 50 unrelated imports cost neither output nor lookups.
            imps.retain(|i| {
                std::iter::once(&i.module)
                    .chain(i.alias.iter())
                    .chain(i.names.iter().flat_map(|(o, l)| [o, l]))
                    .any(|n| fold(n).contains(q))
            });
            filter_syms(all, q, p.lang, Some(file))
        }
        None => all,
    };
    let (imports, lookups) = import_lines(cwd, file, &p, &imps);
    // Skip the kind label when the signature already says it (`const X`, `def f`, `pub fn g`, `@Get ...`).
    const SAYS: &[&str] = &[
        "function",
        "def",
        "async",
        "fn",
        "class",
        "interface",
        "type",
        "const",
        "let",
        "var",
        "enum",
        "struct",
        "trait",
        "pub",
        "public",
        "private",
        "protected",
        "static",
        "abstract",
        "declare",
        "mod",
        "macro_rules!",
        "namespace",
        "union",
        "final",
    ];
    let render = |list: &[Symbol]| -> Vec<String> {
        list.iter()
            .map(|s| {
                let ind = if s.parent.is_some() { "    " } else { "  " };
                let d = s.short_doc();
                let d = if d.is_empty() {
                    String::new()
                } else {
                    format!("  // {d}")
                };
                let first = strip_export(&s.sig).split_whitespace().next().unwrap_or("");
                let kind = if SAYS.contains(&first) || first.starts_with('@') {
                    String::new()
                } else {
                    format!("{} ", s.kind)
                };
                format!(
                    "{ind}{}-{} {kind}{}{}{d}",
                    s.start,
                    s.end,
                    sig_clip(&s.sig, 200),
                    owner_note(s, list, q, file)
                )
            })
            .collect()
    };
    // Filtered: the symbols that match themselves, not the container headers listed above them.
    let direct: Vec<Symbol> = match q {
        Some(q) => syms.iter().filter(|s| matches(s, q)).cloned().collect(),
        None => vec![],
    };
    let count = match q {
        Some(q) => format!("{total} symbols, {} matching \"{q}\"", direct.len()),
        None => format!("{total} symbols"),
    };
    let head = vec![format!(
        "FILE {}  ({}, {} lines, {count})",
        rel(cwd, file),
        Lang::label(file),
        p.src.lines().count(),
    )];
    let tail = format!("next: tc zoom {}:<symbol>\n", rel(cwd, file));
    let filtered = q.is_some() && direct.len() < syms.len();
    let list = |headers: bool| if headers { &syms } else { &direct };
    let is_hit = |s: &Symbol| q.is_some_and(|q| matches(s, q));
    let (text, shown) = prefer_hits(filtered, |headers| {
        let mut secs = Vec::new();
        // Filtered: the imports section only when an import matches.
        if q.is_none() || !imports.is_empty() {
            secs.push(Sec {
                title: "imports:".into(),
                lines: imports.clone(),
                empty: "  (none)".into(),
            });
        }
        secs.push(Sec {
            title: "symbols (lines kind signature):".into(),
            lines: render(list(headers)),
            empty: match q {
                Some(q) => format!("  (no symbol matches \"{q}\")"),
                None => "  (none)".into(),
            },
        });
        let (text, shown) = fit(&head, &secs, &tail, budget);
        let whole = !was_cut(&secs, &shown);
        let any = list(headers)[..*shown.last().unwrap_or(&0)]
            .iter()
            .any(is_hit);
        ((text, shown), whole, any)
    });
    let capped = prefer_hits(filtered, |headers| {
        let mut json = json!({"level": "L1.5", "file": rel(cwd, file), "imports": imps, "symbols": list(headers), "truncated": !headers});
        if let Some(q) = q {
            json["filter"] = json!(q);
        }
        let whole = json_fits(&json, budget);
        let capped = cap_json(json, &["symbols", "imports"], budget);
        let n = capped
            .get("symbols")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let any = list(headers)[..n].iter().any(is_hit);
        (capped, whole, any)
    });
    // An empty file counts as shown whole; a filter that matched nothing showed nothing.
    let empty_file = q.is_none() && syms.is_empty() && imps.is_empty();
    let baseline = if json_mode {
        let has_content = capped.get("file").is_some()
            && (capped
                .get("symbols")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty())
                || capped
                    .get("imports")
                    .and_then(Value::as_array)
                    .is_some_and(|a| !a.is_empty())
                || empty_file);
        if has_content {
            p.src.len()
        } else {
            0
        }
    } else if shown.iter().any(|k| *k > 0) || empty_file {
        p.src.len()
    } else {
        0
    };
    Ok(Out {
        json: capped,
        text,
        baseline_bytes: baseline,
        dep_lookups: lookups,
        ok: true,
    })
}

// ---------------------------------------------------------------- L2

fn pick<'a>(syms: &'a [Symbol], query: &str) -> Option<&'a Symbol> {
    let q = query.replace("::", ".");
    let parts: Vec<&str> = q.split('.').collect();
    let name = *parts.last()?;
    let owner = (parts.len() > 1).then(|| parts[parts.len() - 2]);
    let mut m: Vec<&Symbol> = syms
        .iter()
        .filter(|s| s.name == name && (owner.is_none() || s.parent.as_deref() == owner))
        .collect();
    m.sort_by_key(|s| {
        (
            s.parent.is_some(),
            matches!(s.kind, "property" | "var" | "const" | "field"),
            s.line,
        )
    });
    m.first().copied()
}

/// A call line or a definition? Definitions: `fn x(`, `def x`, `Type x(...) {` (no `;`), `x = ` at start, etc.
fn is_def_line(line: &str, name: &str) -> bool {
    line.match_indices(name).any(|(i, _)| {
        if !word_at(line, i, name.len()) {
            return false;
        }
        let before = line[..i].trim_end();
        let prev_tok = before
            .rsplit(|c: char| c.is_whitespace() || c == '(')
            .next()
            .unwrap_or("");
        if matches!(
            prev_tok,
            "def"
                | "fn"
                | "function"
                | "class"
                | "interface"
                | "struct"
                | "enum"
                | "trait"
                | "type"
                | "async"
                | "const"
                | "let"
                | "var"
        ) {
            return true;
        }
        let after = line[i + name.len()..].trim_start();
        let t = line.trim_end();
        let typed = before
            .chars()
            .last()
            .is_some_and(|c| c.is_alphanumeric() || c == '>' || c == ']' || c == '_')
            && !matches!(
                prev_tok,
                "return"
                    | "new"
                    | "await"
                    | "throw"
                    | "yield"
                    | "else"
                    | "case"
                    | "in"
                    | "of"
                    | "typeof"
                    | "void"
            );
        typed
            && after.starts_with('(')
            && !t.ends_with(';')
            && (t.ends_with('{') || t.ends_with(')') || t.ends_with(','))
            || (before.is_empty() && after.starts_with('(') && t.ends_with('{'))
    })
}

#[derive(serde::Serialize)]
struct Callee {
    name: String,
    sig: String,
    file: String,
    line: usize,
    doc: String,
    /// Size of the callee's file when it isn't the target's own file.
    #[serde(skip)]
    bytes: usize,
}

/// Bytes shown as the L2 target: the symbol with its leading doc comments, widened to whole
/// lines only where nothing else shares them (two functions on one line stay apart).
fn target_span(src: &str, s: &Symbol) -> (usize, usize) {
    let (mut a, mut b) = (s.doc_start.min(s.bytes.0), s.bytes.1.min(src.len()));
    let ls = src[..a].rfind('\n').map_or(0, |i| i + 1);
    if src[ls..a].trim().is_empty() {
        a = ls;
    }
    let le = src[b..].find('\n').map_or(src.len(), |i| b + i);
    if src[b..le].trim().is_empty() {
        b = le;
    }
    (a, b)
}

fn symbol_view(
    cwd: &Path,
    file: &Path,
    query: &str,
    budget: usize,
    json_mode: bool,
) -> Result<Out> {
    let Some(p) = Parsed::read(file) else {
        bail!("unsupported or unreadable file: {}", file.display())
    };
    let syms = p.symbols();
    let Some(sym) = pick(&syms, query) else {
        let names: Vec<String> = syms.iter().map(|s| s.qual()).collect();
        let name = query
            .replace("::", ".")
            .rsplit('.')
            .next()
            .unwrap_or(query)
            .to_string();
        let flat: Vec<String> = syms.iter().map(|s| s.name.clone()).collect();
        let sugg = dep::suggest(&name, &flat, &[], 6);
        let sugg: Vec<String> = sugg
            .iter()
            .filter_map(|s| {
                names
                    .iter()
                    .find(|n| n.rsplit('.').next() == Some(s.as_str()))
                    .cloned()
            })
            .collect();
        bail!(
            "symbol `{query}` not found in {}. did you mean: {}",
            rel(cwd, file),
            if sugg.is_empty() {
                "(no close names)".into()
            } else {
                sugg.join(", ")
            }
        );
    };
    let frel = rel(cwd, file);
    let (a, b) = target_span(&p.src, sym);
    let first = p.src[..a].matches('\n').count() + 1;
    let src_lines: Vec<&str> = p.src[a..b].lines().collect();
    let width = (first + src_lines.len().saturating_sub(1))
        .to_string()
        .len();
    let body: Vec<String> = src_lines
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{:>width$}  {}", first + i, l.trim_end()))
        .collect();

    // INTERNAL CALLEES
    let callees = resolve_callees(cwd, file, &p, &syms, sym);
    let callee_lines: Vec<String> = callees
        .iter()
        .map(|c| {
            let d = if c.doc.is_empty() {
                String::new()
            } else {
                format!("  // {}", c.doc)
            };
            format!("  {}  [{}:{}]{d}", sig_clip(&c.sig, 220), c.file, c.line)
        })
        .collect();

    // INSTALLED DEPS USED HERE
    let (deps, _) = dep::deps_used(&p, file, cwd, Some(sym.bytes), 8);
    let (dep_lines, dep_files) = dep::render_deps(&deps);
    let lookups = deps
        .iter()
        .map(|d| d.items.len() + d.missing.len())
        .sum::<usize>();

    // CALLERS / BLAST RADIUS
    let callers = find_callers(cwd, file, sym);
    let nfiles = callers
        .iter()
        .map(|c| c.0.clone())
        .collect::<HashSet<_>>()
        .len();
    let caller_lines: Vec<String> = callers
        .iter()
        .map(|(f, l, t)| format!("  {f}:{l}  {}", clip(t, 150)))
        .collect();

    let head = vec![format!(
        "{frel}:{}  ({} {}, lines {}-{})",
        sym.qual(),
        Lang::label(file),
        sym.kind,
        sym.start,
        sym.end
    )];
    let secs = vec![
        Sec {
            title: "## TARGET".into(),
            lines: body.clone(),
            empty: String::new(),
        },
        Sec {
            title: format!("## INTERNAL CALLEES ({})", callees.len()),
            lines: callee_lines,
            empty: "  (none resolved in this project)".into(),
        },
        Sec {
            title: "## INSTALLED DEPS USED HERE".into(),
            lines: dep_lines,
            empty: "  none (no third-party imports used in this symbol)".into(),
        },
        Sec {
            title: format!(
                "## CALLERS / BLAST RADIUS ({} sites in {nfiles} files)",
                callers.len()
            ),
            lines: caller_lines,
            empty: "  (no references found outside the definition)".into(),
        },
    ];
    let (text, shown) = fit(&head, &secs, "", budget);
    // Baseline: each file that contributed shown lines, once.
    let mut seen: HashSet<&str> = HashSet::new();
    let mut baseline = if shown[0] > 0 { p.src.len() } else { 0 };
    for c in &callees[..shown[1]] {
        if c.bytes > 0 && seen.insert(&c.file) {
            baseline += c.bytes;
        }
    }
    for (f, n) in dep_files[..shown[2]].iter().flatten() {
        if seen.insert(f) {
            baseline += n;
        }
    }
    let json = json!({
        "level": "L2", "file": frel, "symbol": sym, "body": body, "callees": callees, "deps": deps,
        "callers": callers.iter().map(|(f, l, t)| json!({"file": f, "line": l, "text": t})).collect::<Vec<_>>(),
        "truncated": was_cut(&secs, &shown),
    });
    let capped = cap_json(json, &["callers", "deps", "callees", "body"], budget);
    let baseline = if json_mode {
        let mut jseen: HashSet<String> = HashSet::new();
        let mut jb = if capped
            .get("body")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty())
        {
            p.src.len()
        } else {
            0
        };
        let n_callees = capped
            .get("callees")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        for c in callees.iter().take(n_callees) {
            if c.bytes > 0 && jseen.insert(c.file.clone()) {
                jb += c.bytes;
            }
        }
        let n_deps = capped
            .get("deps")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        for d in deps.iter().take(n_deps) {
            for item in &d.items {
                if item.file_bytes > 0 && jseen.insert(item.file.clone()) {
                    jb += item.file_bytes;
                }
            }
        }
        jb
    } else {
        baseline
    };
    Ok(Out {
        json: capped,
        text,
        baseline_bytes: baseline,
        dep_lookups: lookups,
        ok: true,
    })
}

fn resolve_callees(
    cwd: &Path,
    file: &Path,
    p: &Parsed,
    syms: &[Symbol],
    sym: &Symbol,
) -> Vec<Callee> {
    // Distinct (name, receiver) pairs: `a.run()` and `b.run()` can be different methods.
    let mut calls = p.calls(sym.bytes.0, sym.bytes.1);
    let mut seen = HashSet::new();
    calls.retain(|c| seen.insert((c.name.clone(), c.recv.clone())));
    if calls.is_empty() {
        return vec![];
    }
    // Candidate files: internal imports (+ same-package Java files, + same-crate Rust files).
    let mut imported: HashMap<String, (PathBuf, String)> = HashMap::new();
    let mut cand_files: Vec<PathBuf> = Vec::new();
    for imp in p.imports() {
        if !matches!(
            dep::origin(p.lang, &imp.module, file, cwd),
            Origin::Internal
        ) {
            continue;
        }
        if let Some(f) = internal_file(cwd, file, p.lang, &imp.module) {
            for (o, l) in &imp.names {
                imported.insert(l.clone(), (f.clone(), o.clone()));
            }
            cand_files.push(f);
        }
    }
    let dir = file.parent().unwrap_or(cwd);
    if matches!(p.lang, Lang::Java | Lang::Rs) {
        // Rust: the crate dir, and the library root's directory (`rs_lib_rel`) for a file in it
        // (`binding_rust/`; the crate dir itself for `[lib] path = "lib.rs"`), else `src/`.
        let krate = dir.ancestors().find(|d| d.join("Cargo.toml").is_file());
        let comps = |d: &Path, f: &Path| -> Vec<String> {
            f.strip_prefix(d).map_or_else(
                |_| vec![],
                |r| {
                    r.components()
                        .filter_map(|c| c.as_os_str().to_str().map(String::from))
                        .collect()
                },
            )
        };
        let root = match krate {
            Some(d) if p.lang == Lang::Rs => {
                let lib = rs_lib_rel(d);
                let lib_dir = &lib[..lib.len() - 1];
                if rs_in_lib_dir(lib_dir, &comps(d, file)) {
                    d.join(lib_dir.iter().collect::<PathBuf>())
                } else {
                    d.join("src")
                }
            }
            _ => dir.to_path_buf(),
        };
        let ext = if p.lang == Lang::Rs { "rs" } else { "java" };
        let sibs = if p.lang == Lang::Rs {
            let mut s = walk_files(&root, true);
            // A library root in the crate dir: its `tests/`, `build.rs`, .. are other targets.
            if let Some(d) = krate.filter(|d| root == *d) {
                s.retain(|f| rs_in_lib_dir(&[], &comps(d, f)));
            }
            s
        } else {
            fs::read_dir(dir)
                .map(|rd| rd.flatten().map(|e| e.path()).collect())
                .unwrap_or_default()
        };
        cand_files.extend(
            sibs.into_iter()
                .filter(|f| f.extension().is_some_and(|e| e == ext) && f != file),
        );
    }
    cand_files.sort();
    cand_files.dedup();
    let own_parent = sym.parent.clone();
    let target_text = &p.src;
    // Is `class` the declared type of receiver `recv` in the target file?
    // Matches `Class recv` (Java/C-style, incl. `for (Class recv :`), `recv: Class`, `recv = new Class(` / `recv = Class(`.
    let recv_is = |recv: &str, class: &str| -> bool {
        let r = recv.rsplit(['.', ':']).next().unwrap_or(recv);
        if r == class {
            return true; // static call / `Type::method`
        }
        target_text.match_indices(r).any(|(i, _)| {
            if r.is_empty() || !word_at(target_text, i, r.len()) {
                return false;
            }
            let before = target_text[..i].trim_end();
            let before = match before.strip_suffix('>') {
                Some(b) => b.rfind('<').map(|j| &b[..j]).unwrap_or(b),
                None => before,
            };
            let ends_with_class =
                before.ends_with(class) && word_at(before, before.len() - class.len(), class.len());
            let after = target_text[i + r.len()..].trim_start();
            let typed = after
                .strip_prefix(':')
                .is_some_and(|a| a.trim_start().starts_with(class));
            let assigned = after.strip_prefix('=').is_some_and(|a| {
                let a = a.trim_start();
                a.strip_prefix("new ").unwrap_or(a).starts_with(class)
            });
            ends_with_class || typed || assigned
        })
    };
    // Per candidate file: its text, and its symbols once parsed.
    let mut texts: HashMap<PathBuf, (String, Option<Vec<Symbol>>)> = HashMap::new();
    let mut out = Vec::new();
    let mut found: HashSet<(PathBuf, usize, String)> = HashSet::new();
    for c in calls.iter().take(60) {
        let recv = c.recv.as_deref();
        let self_call = matches!(recv, Some("this" | "self" | "Self" | "super"));
        let eligible = |s: &Symbol, f: &Path| -> bool {
            if matches!(
                s.kind,
                "property" | "var" | "const" | "static" | "route" | "field"
            ) {
                return false;
            }
            match recv {
                None => s.parent.is_none() || s.parent == own_parent,
                Some("new") => matches!(s.kind, "class" | "struct" | "record" | "constructor"),
                Some(_) if self_call => s.parent == own_parent || own_parent.is_none(),
                Some(r) => match s.parent.as_deref() {
                    Some(par) => recv_is(r, par),
                    // `module.func()` / `module::func()` / `ns.func()`: receiver names the defining file
                    None => f.file_stem().and_then(|x| x.to_str()).is_some_and(|stem| {
                        stem.split('.').next() == Some(r.rsplit(['.', ':']).next().unwrap_or(r))
                    }),
                },
            }
        };
        // Search order: this file, then the file the name was imported from, then other candidates.
        let mut sources: Vec<(PathBuf, String, i32)> =
            vec![(file.to_path_buf(), c.name.clone(), 4)];
        if let Some((f, o)) = imported.get(&c.name) {
            sources.push((f.clone(), o.clone(), 3));
        }
        sources.extend(cand_files.iter().map(|f| (f.clone(), c.name.clone(), 1)));
        let mut best: Option<(i32, PathBuf, Symbol)> = None;
        for (f, name, base) in sources {
            let candidates: Vec<Symbol> = if f == file {
                syms.iter().filter(|s| s.name == name).cloned().collect()
            } else {
                let entry = texts
                    .entry(f.clone())
                    .or_insert_with(|| (fs::read_to_string(&f).unwrap_or_default(), None));
                if !has_word(&entry.0, &name) {
                    continue;
                }
                if entry.1.is_none() {
                    entry.1 = Some(
                        Parsed::of(&f, entry.0.clone())
                            .map(|p| p.symbols())
                            .unwrap_or_default(),
                    );
                }
                entry
                    .1
                    .as_ref()
                    .unwrap()
                    .iter()
                    .filter(|s| s.name == name)
                    .cloned()
                    .collect()
            };
            for s in candidates.into_iter().filter(|s| eligible(s, &f)) {
                let score = base
                    + if s
                        .parent
                        .as_deref()
                        .is_some_and(|par| has_word(target_text, par))
                    {
                        1
                    } else {
                        0
                    };
                if best.as_ref().is_none_or(|b| score > b.0) {
                    best = Some((score, f.clone(), s));
                }
            }
        }
        if let Some((_, f, s)) = best {
            // Recursion into the target isn't a callee; one definition reached twice is listed once.
            if (f == file && s.bytes == sym.bytes) || !found.insert((f.clone(), s.line, s.qual())) {
                continue;
            }
            let bytes = if f == file {
                0
            } else {
                texts.get(&f).map_or(0, |t| t.0.len())
            };
            out.push(Callee {
                name: s.qual(),
                sig: s.sig.clone(),
                file: rel(cwd, &f),
                line: s.line,
                doc: s.short_doc(),
                bytes,
            });
        }
    }
    out
}

fn find_callers(cwd: &Path, file: &Path, sym: &Symbol) -> Vec<(String, usize, String)> {
    let name = &sym.name;
    let mut out = Vec::new();
    for f in walk_files(cwd, true) {
        let Ok(text) = fs::read_to_string(&f) else {
            continue;
        };
        if !has_word(&text, name) {
            continue;
        }
        let same = f == file;
        for (i, l) in text.lines().enumerate() {
            let n = i + 1;
            if same && n >= sym.start && n <= sym.end {
                continue;
            }
            if !has_word(l, name) {
                continue;
            }
            let t = l.trim();
            if t.starts_with("//")
                || t.starts_with('#')
                || t.starts_with('*')
                || t.starts_with("/*")
                || t.starts_with("import ")
                || t.starts_with("from ")
                || t.starts_with("use ")
                || t.starts_with("export {")
                || t.starts_with("export *")
            {
                continue;
            }
            if is_def_line(l, name) {
                continue;
            }
            out.push((rel(cwd, &f), n, t.to_string()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn def_line_detection() {
        assert!(is_def_line(
            "    public List<Instant> getFreeSlots(Provider provider, LocalDate date) {",
            "getFreeSlots"
        ));
        assert!(!is_def_line("        return getFreeSlots(providerRepository.findById(providerId).orElseThrow(), date);", "getFreeSlots"));
        assert!(is_def_line(
            "export function bufferLines(buf: Rows, n: number): string[] {",
            "bufferLines"
        ));
        assert!(!is_def_line(
            "  const ash = ashSeen.fresh(p.id, bufferLines(p.term, 5));",
            "bufferLines"
        ));
        assert!(is_def_line(
            "def transcribe(path: str) -> dict:",
            "transcribe"
        ));
    }

    #[test]
    fn budget_cuts_low_priority_first() {
        let secs = vec![
            Sec {
                title: "A".into(),
                lines: vec!["a".repeat(30); 3],
                empty: String::new(),
            },
            Sec {
                title: "B".into(),
                lines: vec!["b".repeat(30); 50],
                empty: String::new(),
            },
        ];
        let (t, shown) = fit(&["h".into()], &secs, "", 60);
        assert!(was_cut(&secs, &shown));
        assert_eq!(t.matches(&"a".repeat(30)).count(), 3);
        assert!(t.contains("more lines cut by --budget 60"));
        assert!(t.len() <= 240, "{}", t.len());
    }

    #[test]
    fn budget_fits_complete_output_without_marker_room() {
        // 40 bytes is budget 10 exactly: nothing is cut, so no room is kept for a marker.
        let secs = vec![Sec {
            title: "T".into(),
            lines: vec!["x".repeat(35)],
            empty: String::new(),
        }];
        let (t, shown) = fit(&["h".into()], &secs, "", 10);
        assert_eq!((t.len(), shown), (40, vec![1]));
        let (t, shown) = fit(&["h".into()], &secs, "", 9);
        assert!(was_cut(&secs, &shown) && t.len() <= 36, "{t}");
    }

    #[test]
    fn budget_counts_chars_not_utf8_bytes() {
        // 40 chars (75 bytes) is budget 10 exactly: shown whole.
        let secs = vec![Sec {
            title: "T".into(),
            lines: vec!["é".repeat(35)],
            empty: String::new(),
        }];
        let (t, shown) = fit(&["h".into()], &secs, "", 10);
        assert_eq!((clen(&t), t.len(), shown), (40, 75, vec![1]));
        // A frame too big is cut on whole characters, up to the cap.
        let (t, _) = fit(&["é".repeat(50)], &secs, "", 2);
        assert_eq!(t, "é".repeat(8));
        // JSON: the printed form (pretty + newline) in chars.
        let v = json!({"s": "é".repeat(30)});
        let n = clen(&serde_json::to_string_pretty(&v).unwrap()) + 1;
        let b = n.div_ceil(4);
        assert!(n + 30 > b * 4, "the bytes must not fit");
        assert!(json_fits(&v, b) && !json_fits(&v, b - 1));
        assert_eq!(cap_json(v.clone(), &["s"], b), v);
    }

    #[test]
    fn filter_skips_initializers_keeps_rust_modules_and_fields() {
        // A compact initializer is a value, not a signature; a constant's name and type still match.
        assert!(kept(
            Lang::Tsx,
            "var Gs=\"Terminal input\",mi=1;\n",
            "terminal input"
        )
        .is_empty());
        let ts = "export const foo: ResizeMode = \"x\";\n";
        assert_eq!(kept(Lang::Ts, ts, "RESIZEMODE"), ["foo@1"]);
        assert_eq!(kept(Lang::Ts, ts, "foo"), ["foo@1"]);
        // Every enclosing `mod`, innermost first up to the top.
        let rs = "pub mod a {\n    mod foo {\n        pub fn bar() {}\n        pub fn baz() {}\n    }\n}\nfn bar_top() {}\n";
        assert_eq!(
            kept(Lang::Rs, rs, "bar"),
            ["a@1", "foo@2", "bar@3", "bar_top@7"]
        );
        let rs = "mod m {\n    pub struct P;\n    impl P {\n        pub fn grow(&self) {}\n    }\n    impl Ext for Q {\n        fn ext(&self) {}\n    }\n}\n";
        assert_eq!(kept(Lang::Rs, rs, "grow"), ["m@1", "P@2", "P.grow@4"]);
        assert_eq!(kept(Lang::Rs, rs, "ext"), ["m@1", "Q.ext@7"]);
        // A struct's named fields, under the struct.
        let rs = "struct ReportState { closed: bool, emitted: u64 }\n";
        assert_eq!(
            kept(Lang::Rs, rs, "emitted"),
            ["ReportState@1", "ReportState.emitted@1"]
        );
        // Unicode case folding on both sides: ΟΣ matches ς.
        let greek = "export const \u{39F}\u{3A3} = 1;\n";
        assert_eq!(kept(Lang::Ts, greek, "\u{3C2}"), ["\u{39F}\u{3A3}@1"]);
    }

    fn kept(lang: Lang, src: &str, q: &str) -> Vec<String> {
        let p = Parsed::new(lang, src.into()).unwrap();
        filter_syms(p.symbols(), &fold(q), lang, None)
            .iter()
            .map(|s| format!("{}@{}", s.qual(), s.line))
            .collect()
    }

    #[test]
    fn filter_keeps_the_enclosing_container_only() {
        // `C.resize` lives in `N.C`, not in the top-level `C`: containment picks the right header.
        let ts = "export namespace N {\n  export class C {\n    resize(): void {}\n    draw(): void {}\n  }\n}\nexport class C {}\n";
        assert_eq!(kept(Lang::Ts, ts, "resize"), ["N@1", "N.C@2", "C.resize@3"]);
        // Rust `impl` methods sit outside the struct's span: the struct is found by name.
        let rs =
            "pub struct P;\nimpl P {\n    pub fn grow(&self) {}\n    pub fn shrink(&self) {}\n}\n";
        assert_eq!(kept(Lang::Rs, rs, "grow"), ["P@1", "P.grow@3"]);
        // ...and the `mod` the impl sits in is kept as well, found from the method's own span.
        let rs = "pub struct Box;\nmod error {\n    impl Error for Box {\n        fn source(&self) {}\n    }\n}\n";
        assert_eq!(
            kept(Lang::Rs, rs, "fn source("),
            ["Box@1", "error@2", "Box.source@4"]
        );
        // Two `#[cfg]` structs of one name: a field's header is the struct around it.
        let rs =
            "#[cfg(a)]\nstruct S {\n    alpha: u8,\n}\n#[cfg(b)]\nstruct S {\n    beta: u8,\n}\n";
        assert_eq!(kept(Lang::Rs, rs, "beta"), ["S@6", "S.beta@7"]);
        assert_eq!(kept(Lang::Rs, rs, "alpha"), ["S@2", "S.alpha@3"]);
        // ...and an impl's header is the twin with the impl's `#[cfg]`.
        let rs = "#[cfg(a)]\nstruct S;\n#[cfg(a)]\nimpl S {\n    fn x(&self) {}\n}\n#[cfg(b)]\nstruct S;\n#[cfg(b)]\nimpl S {\n    fn y(&self) {}\n}\n";
        assert_eq!(kept(Lang::Rs, rs, "fn y"), ["S@8", "S.y@11"]);
        assert_eq!(kept(Lang::Rs, rs, "fn x"), ["S@2", "S.x@5"]);
        // A qualified impl type resolves against the `mod`s around it, never to another `T`.
        let rs = "pub struct CookieStore;\nmod serde_legacy {\n    impl Serialize for super::CookieStore {\n        fn serialize<S>(&self) {}\n    }\n}\n";
        assert_eq!(
            kept(Lang::Rs, rs, "fn serialize<S>"),
            ["CookieStore@1", "serde_legacy@2", "CookieStore.serialize@4"]
        );
        let rs = "pub struct T;\nmod a {\n    mod b {\n        impl X for super::other::T {\n            fn nowhere(&self) {}\n        }\n    }\n}\n";
        assert_eq!(kept(Lang::Rs, rs, "nowhere"), ["a@2", "b@3", "T.nowhere@5"]);
        // Multi-declarators: a name doesn't match through its sibling.
        let java = "class K {\n  final int port, tailnet;\n}\n";
        assert_eq!(kept(Lang::Java, java, "tailnet"), ["K@1", "K.tailnet@2"]);
        let ts = "const port = 1, tailnet = 2;\n";
        assert_eq!(kept(Lang::Ts, ts, "tailnet"), ["tailnet@1"]);
        // A constant's value isn't its signature; its name and type are.
        let data = "export const names = [\"gate\"];\nexport const gateKeeper: number = 1;\n";
        assert_eq!(kept(Lang::Ts, data, "gate"), ["gateKeeper@2"]);
        assert!(kept(Lang::Py, "def a():\n    pass\n", "zz").is_empty());
    }
}
