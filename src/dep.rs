//! Ground-truth installed dependency lookups: node_modules, Python venvs, Cargo registry.
//! Reads files only. Never runs package code and never touches the network.
use crate::lang::{
    clip, has_word, rs_owners, short_doc, sig_clip, word_at, Import, Lang, Parsed, Symbol,
};
use crate::Out;
use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Default)]
pub struct Pkg {
    pub eco: &'static str,
    pub name: String,
    /// Node subpath (`client` of `react-dom/client`): `entry` is that subpath's export.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub: Option<String>,
    pub version: String,
    pub dir: Option<PathBuf>,
    #[serde(skip)]
    pub entry: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub types_from: Option<String>,
    /// How the version was picked when that is not clear-cut (several versions in Cargo.lock).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Python: every site-packages dir, for re-exports that cross into another package.
    #[serde(skip)]
    pub sites: Vec<PathBuf>,
    /// Node: package.json has `"type": "module"`.
    #[serde(skip)]
    pub esm: bool,
}

impl Pkg {
    pub fn label(&self) -> String {
        let t = self
            .types_from
            .as_ref()
            .map(|t| format!(", types from {t}"))
            .unwrap_or_default();
        let sub = self
            .sub
            .as_ref()
            .map(|s| format!("/{s}"))
            .unwrap_or_default();
        let note = self
            .note
            .as_ref()
            .map(|n| format!("; {n}"))
            .unwrap_or_default();
        format!(
            "{}{sub} {} ({}{t}{note})",
            self.name, self.version, self.eco
        )
    }
    /// Path of `f` shown relative to the package root, prefixed by the package name.
    pub fn rel(&self, f: &Path) -> String {
        let base = self
            .dir
            .as_deref()
            .and_then(|d| d.parent())
            .unwrap_or(Path::new(""));
        let base = if self.eco == "node" && self.name.starts_with('@') {
            base.parent().unwrap_or(base)
        } else {
            base
        };
        slash(f.strip_prefix(base).unwrap_or(f))
    }
}

pub fn slash(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// `fs::canonicalize` (symlinks and Windows junctions resolved) without the `\\?\` prefix, so
/// paths still print normally; `p` itself when it can't be resolved.
pub fn canon(p: &Path) -> PathBuf {
    let Ok(c) = fs::canonicalize(p) else {
        return p.to_path_buf();
    };
    match c.to_string_lossy().strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => c,
    }
}

pub fn home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

// ---------------------------------------------------------------- resolution

/// Find an installed package by name, trying node_modules, then Python site-packages, then Cargo.lock.
pub fn resolve(start: &Path, spec: &str, eco: Option<&str>) -> Option<Pkg> {
    let tries: &[&str] = match eco {
        Some("node") => &["node"],
        Some("python") => &["python"],
        Some("rust") => &["rust"],
        _ => &["node", "python", "rust"],
    };
    tries.iter().find_map(|e| match *e {
        "node" => node_pkg(start, spec),
        "python" => py_pkg(start, spec),
        _ => rust_pkg(start, spec),
    })
}

pub fn split_node_spec(spec: &str) -> (String, Option<String>) {
    let mut it = spec.splitn(if spec.starts_with('@') { 3 } else { 2 }, '/');
    let name = if spec.starts_with('@') {
        format!("{}/{}", it.next().unwrap_or(""), it.next().unwrap_or(""))
    } else {
        it.next().unwrap_or("").to_string()
    };
    (name, it.next().map(String::from))
}

fn node_pkg(start: &Path, spec: &str) -> Option<Pkg> {
    let (name, sub) = split_node_spec(spec);
    for dir in start.ancestors() {
        let link = dir.join("node_modules").join(&name);
        let Ok(txt) = fs::read_to_string(link.join("package.json")) else {
            continue;
        };
        // Workspaces and pnpm link packages in (symlinks, Windows junctions): work from the real
        // location so `../x` imports inside the package resolve the way Node resolves them.
        let pdir = canon(&link);
        let pj: Value = serde_json::from_str(&txt).unwrap_or(Value::Null);
        let exp_key = sub
            .as_deref()
            .map(|s| format!("./{s}"))
            .unwrap_or(".".into());
        if !pj["exports"].is_null() {
            let t = match_exports_target(&pj["exports"], &exp_key)?;
            // If the winning Node condition is `null` (e.g. `{"node": null, "default": ...}`),
            // or no Node/types condition yields a string target, the subpath is blocked.
            let runtime = eval_cond(&t, false);
            let types = eval_cond(&t, true);
            if matches!(runtime, CondMatch::Blocked)
                || (matches!(runtime, CondMatch::None) && !matches!(types, CondMatch::Found(_)))
            {
                return None;
            }
        }
        let mut pkg = Pkg {
            eco: "node",
            name: name.clone(),
            sub: sub.clone(),
            version: pj["version"].as_str().unwrap_or("?").into(),
            entry: node_entry(&pdir, &pj, sub.as_deref()),
            dir: Some(pdir),
            esm: pj["type"].as_str() == Some("module"),
            ..Pkg::default()
        };
        if !pkg.entry.as_deref().is_some_and(is_dts) {
            let tname = format!("@types/{}", name.trim_start_matches('@').replace('/', "__"));
            let tdir = canon(&dir.join("node_modules").join(&tname));
            if let Ok(t) = fs::read_to_string(tdir.join("package.json")) {
                let tj: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                if let Some(e) = node_entry(&tdir, &tj, sub.as_deref()) {
                    pkg.types_from =
                        Some(format!("{tname}@{}", tj["version"].as_str().unwrap_or("?")));
                    pkg.entry = Some(e);
                    pkg.dir = Some(tdir);
                }
            }
        }
        return Some(pkg);
    }
    None
}

fn is_dts(p: &Path) -> bool {
    let s = p.to_string_lossy();
    s.ends_with(".d.ts") || s.ends_with(".d.mts") || s.ends_with(".d.cts")
}

enum CondMatch {
    Found(String),
    Blocked,
    None,
}

/// Evaluate a Node `exports` target condition tree for one resolution mode (`"import"` or
/// `"require"`) in JSON key order per `PACKAGE_TARGET_RESOLVE`.
fn eval_cond_mode(v: &Value, want_types: bool, mode: &str) -> CondMatch {
    match v {
        Value::Null => CondMatch::Blocked,
        Value::String(s) => {
            if !want_types || is_dts(Path::new(s)) {
                CondMatch::Found(s.clone())
            } else {
                CondMatch::None
            }
        }
        Value::Object(m) => {
            for (k, val) in m {
                let active = match k.as_str() {
                    "types" => want_types,
                    "node" | "module-sync" | "default" => true,
                    other => other == mode,
                };
                if !active {
                    continue;
                }
                match eval_cond_mode(val, want_types, mode) {
                    CondMatch::Found(s) => return CondMatch::Found(s),
                    CondMatch::Blocked => return CondMatch::Blocked,
                    CondMatch::None => {}
                }
            }
            CondMatch::None
        }
        Value::Array(a) => {
            let mut saw_blocked = false;
            for x in a {
                match eval_cond_mode(x, want_types, mode) {
                    CondMatch::Found(s) => return CondMatch::Found(s),
                    CondMatch::Blocked => saw_blocked = true,
                    CondMatch::None => {}
                }
            }
            if saw_blocked {
                CondMatch::Blocked
            } else {
                CondMatch::None
            }
        }
        _ => CondMatch::None,
    }
}

/// Evaluate Node conditional exports across ESM (`import`) and CJS (`require`) separately:
/// `{"require": null, "import": "./index.mjs"}` resolves under `import`, while
/// `{"node": null, "default": "./fallback.js"}` is blocked under both modes.
fn eval_cond(v: &Value, want_types: bool) -> CondMatch {
    let esm = eval_cond_mode(v, want_types, "import");
    if let CondMatch::Found(s) = esm {
        return CondMatch::Found(s);
    }
    let cjs = eval_cond_mode(v, want_types, "require");
    if let CondMatch::Found(s) = cjs {
        return CondMatch::Found(s);
    }
    if matches!(esm, CondMatch::Blocked) || matches!(cjs, CondMatch::Blocked) {
        CondMatch::Blocked
    } else {
        CondMatch::None
    }
}

fn find_cond(v: &Value, want_types: bool) -> Option<String> {
    match eval_cond(v, want_types) {
        CondMatch::Found(s) => Some(s),
        CondMatch::Blocked | CondMatch::None => None,
    }
}

fn subst_star(v: &Value, star: &str) -> Value {
    match v {
        Value::String(s) => Value::String(s.replace('*', star)),
        Value::Array(a) => Value::Array(a.iter().map(|x| subst_star(x, star)).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), subst_star(x, star)))
                .collect(),
        ),
        _ => v.clone(),
    }
}

fn match_exports_target(exp: &Value, key: &str) -> Option<Value> {
    match exp {
        Value::Object(m) if m.keys().any(|k| k.starts_with('.')) => {
            if let Some(v) = m.get(key) {
                return (!v.is_null()).then(|| v.clone());
            }
            // Subpath pattern match (Node PATTERN_KEY_COMPARE: longest prefix, then longest suffix).
            let mut best: Option<((usize, usize), Value)> = None;
            for (pat, val) in m {
                let Some((pre, suf)) = pat.split_once('*') else {
                    continue;
                };
                // Node ESM spec: `*` must match a non-empty substring (`key.len() > pre + suf`).
                if key.len() > pre.len() + suf.len() && key.starts_with(pre) && key.ends_with(suf) {
                    let star = &key[pre.len()..key.len() - suf.len()];
                    let rank = (pre.len(), suf.len());
                    if best.as_ref().is_none_or(|(r, _)| rank > *r) {
                        best = Some((rank, subst_star(val, star)));
                    }
                }
            }
            best.and_then(|(_, v)| (!v.is_null()).then_some(v))
        }
        _ if key == "." && !exp.is_null() => Some(exp.clone()),
        _ => None,
    }
}

fn resolve_mapped_target(dir: &Path, target: &Value) -> Option<PathBuf> {
    if let Some(t) = find_cond(target, true) {
        let p = dir.join(t);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(t) = find_cond(target, false) {
        let stem = ["js", "mjs", "cjs"]
            .iter()
            .find_map(|e| t.strip_suffix(&format!(".{e}")))
            .unwrap_or(&t);
        for ext in ["d.ts", "d.mts", "d.cts"] {
            let dts = dir.join(format!("{stem}.{ext}"));
            if dts.is_file() {
                return Some(dts);
            }
        }
        let p = dir.join(&t);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn node_entry(dir: &Path, pj: &Value, sub: Option<&str>) -> Option<PathBuf> {
    let key = sub.map(|s| format!("./{s}")).unwrap_or(".".into());
    let exp = &pj["exports"];
    // When `exports` is present, Node enforces it strictly: blocked (`null`) or unexported
    // subpaths never fall through to disk, and mapped JS targets win over unmapped local stubs.
    if !exp.is_null() {
        let target = match_exports_target(exp, &key)?;
        return resolve_mapped_target(dir, &target);
    }
    let mut cands: Vec<PathBuf> = Vec::new();
    match sub {
        None => {
            for k in ["types", "typings"] {
                if let Some(t) = pj[k].as_str() {
                    cands.push(dir.join(t));
                }
            }
            for k in ["main", "module"] {
                if let Some(m) = pj[k].as_str() {
                    let base = m
                        .trim_end_matches(".js")
                        .trim_end_matches(".mjs")
                        .trim_end_matches(".cjs");
                    cands.push(dir.join(format!("{base}.d.ts")));
                }
            }
            cands.push(dir.join("index.d.ts"));
            for k in ["main", "module"] {
                if let Some(m) = pj[k].as_str() {
                    cands.push(dir.join(m));
                }
            }
            cands.push(dir.join("index.js"));
        }
        Some(s) => {
            // `pkg/lib/a.js` → `lib/a.d.ts`, then the file itself.
            let stem = ["js", "mjs", "cjs"]
                .iter()
                .find_map(|e| s.strip_suffix(&format!(".{e}")))
                .unwrap_or(s);
            for ext in ["d.ts", "d.mts", "d.cts"] {
                cands.push(dir.join(format!("{stem}.{ext}")));
            }
            cands.push(dir.join(stem).join("index.d.ts"));
            cands.push(dir.join(s));
            cands.push(dir.join(format!("{s}.js")));
            cands.push(dir.join(s).join("index.js"));
        }
    }
    cands.into_iter().find(|p| p.is_file())
}

fn parse_py_compact(s: &str) -> Option<String> {
    let rest = s
        .strip_prefix("python")
        .or_else(|| s.strip_prefix("Python"))?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let after = &rest[digits.len()..];
    let ft = after.starts_with(['t', 'T'])
        && after[1..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric());
    if digits.len() >= 2 && digits.starts_with('3') {
        return Some(format!("3.{}{}", &digits[1..], if ft { "t" } else { "" }));
    }
    None
}

fn parse_py_dotted(s: &str) -> Option<String> {
    let base = s.strip_suffix(['t', 'T']);
    let ft = base.is_some();
    let num = base.unwrap_or(s);
    let mut parts = num.split('.');
    let maj = parts.next()?;
    let min = parts.next()?;
    if parts.next().is_none()
        && !maj.is_empty()
        && !min.is_empty()
        && maj.chars().all(|c| c.is_ascii_digit())
        && min.chars().all(|c| c.is_ascii_digit())
    {
        return Some(format!("{maj}.{min}{}", if ft { "t" } else { "" }));
    }
    None
}

/// Infer `major.minor[t]` directly from an executable name like `python3.14t` or `python3.13.exe`.
fn detect_py_exe_version(exe: &Path) -> Option<String> {
    let name = exe.file_name()?.to_str()?;
    let stem = name
        .strip_suffix(".exe")
        .or_else(|| name.strip_suffix(".EXE"))
        .unwrap_or(name);
    let rest = stem
        .strip_prefix("python")
        .or_else(|| stem.strip_prefix("Python"))?;
    parse_py_dotted(rest).or_else(|| parse_py_compact(stem))
}

/// Infer `major.minor` (with trailing `t` for free-threaded builds, e.g. `3.14t`) from files on
/// disk (zero execution): Windows `python313t.dll` / `Python313t` / `pythoncore-3.13t-64`,
/// Unix/macOS `lib/python3.14t`.
fn detect_py_version(home: &Path) -> String {
    let pick_best = |mut vers: Vec<String>| -> Option<String> {
        vers.sort_by(|a, b| {
            let ak = ver_key(a.trim_end_matches('t'));
            let bk = ver_key(b.trim_end_matches('t'));
            bk.cmp(&ak)
                .then_with(|| a.ends_with('t').cmp(&b.ends_with('t')))
        });
        vers.into_iter().next()
    };
    if let Ok(rd) = fs::read_dir(home) {
        let mut dll_vers = Vec::new();
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if let Some(stem) = n.strip_suffix(".dll").or_else(|| n.strip_suffix(".DLL")) {
                if let Some(v) = parse_py_compact(stem) {
                    dll_vers.push(v);
                }
            }
        }
        if let Some(v) = pick_best(dll_vers) {
            return v;
        }
    }
    if let Some(name) = home.file_name().and_then(|n| n.to_str()) {
        if let Some(v) = parse_py_compact(name) {
            return v;
        }
        // Python Install Manager dirs: `pythoncore-3.13t-64`, `pythoncore-3.14-arm64`.
        let lower = name.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("pythoncore-") {
            let tok = rest.split('-').next().unwrap_or("");
            if let Some(v) = parse_py_dotted(tok) {
                return v;
            }
        }
    }
    for base in [Some(home), home.parent()].into_iter().flatten() {
        if let Ok(rd) = fs::read_dir(base.join("lib")) {
            let mut lib_vers = Vec::new();
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if let Some(v) = n.strip_prefix("python").and_then(parse_py_dotted) {
                    lib_vers.push(v);
                }
            }
            if let Some(v) = pick_best(lib_vers) {
                return v;
            }
        }
    }
    String::new()
}

/// Infer all distinct `major.minor[t]` versions present in a single runtime directory
/// (e.g. a shared Windows folder holding both `python313.dll` and `python313t.dll` / `python3.13t.exe`).
#[cfg(windows)]
fn detect_py_versions_all(home: &Path) -> Vec<String> {
    let mut vers: Vec<String> = Vec::new();
    let mut push = |v: String| {
        if !v.is_empty() && !vers.contains(&v) {
            vers.push(v);
        }
    };
    if let Ok(rd) = fs::read_dir(home) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_file() {
                continue;
            }
            let n = e.file_name().to_string_lossy().to_string();
            if let Some(stem) = n.strip_suffix(".dll").or_else(|| n.strip_suffix(".DLL")) {
                if let Some(v) = parse_py_compact(stem) {
                    push(v);
                }
            } else if n.to_ascii_lowercase().ends_with(".exe") {
                if let Some(v) = detect_py_exe_version(&p) {
                    push(v);
                }
            }
        }
    }
    if vers.is_empty() {
        let v = detect_py_version(home);
        if !v.is_empty() {
            vers.push(v);
        }
    }
    vers
}

/// Parse a Windows Python launcher tag (e.g. `PythonCore/3.13t-arm64`, `3.11-32`, `3.14`) into
/// `(version, free_threaded, optional_arch)`.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_py_tag(raw: &str) -> (String, bool, Option<String>) {
    let s = raw.trim();
    let s = match s.split_once('/') {
        Some((co, rest)) if co.eq_ignore_ascii_case("pythoncore") => rest,
        _ => s,
    };
    let (head, arch) = match s.rsplit_once('-') {
        Some((h, a)) if matches!(a.to_ascii_lowercase().as_str(), "32" | "64" | "arm64") => {
            (h, Some(a.to_ascii_lowercase()))
        }
        _ => (s, None),
    };
    let (ver, ft) = match head.strip_suffix(['t', 'T']) {
        Some(v) => (v, true),
        None => (head, false),
    };
    (ver.to_string(), ft, arch)
}

#[cfg(windows)]
fn pe_arch(exe: &Path) -> Option<&'static str> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = fs::File::open(exe).ok()?;
    let mut hdr = [0u8; 64];
    f.read_exact(&mut hdr).ok()?;
    if &hdr[0..2] != b"MZ" {
        return None;
    }
    let off = u32::from_le_bytes(hdr[60..64].try_into().ok()?) as u64;
    f.seek(SeekFrom::Start(off)).ok()?;
    let mut pe = [0u8; 6];
    f.read_exact(&mut pe).ok()?;
    if &pe[0..4] != b"PE\0\0" {
        return None;
    }
    match u16::from_le_bytes([pe[4], pe[5]]) {
        0x014C => Some("32"),
        0x8664 => Some("64"),
        0xAA64 => Some("arm64"),
        _ => None,
    }
}

#[cfg(windows)]
fn dir_arch(dir: &Path) -> Option<&'static str> {
    if let Some(a) = pe_arch(&dir.join("python.exe")) {
        return Some(a);
    }
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_ascii_lowercase();
            if n.starts_with("python3") && n.ends_with(".exe") {
                if let Some(a) = pe_arch(&e.path()) {
                    return Some(a);
                }
            }
        }
    }
    let n = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if n.ends_with("-32") || n.ends_with("win32") {
        Some("32")
    } else if n.ends_with("-arm64") {
        Some("arm64")
    } else if n.ends_with("-64") || n.ends_with("amd64") {
        Some("64")
    } else {
        None
    }
}

/// Read PEP 514 registered Python installations from HKCU & HKLM (both 64-bit and 32-bit views).
#[cfg(windows)]
fn win_pep514_installs() -> Vec<(String, PathBuf, Option<PathBuf>, Option<String>)> {
    use std::ffi::c_void;
    const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as isize;
    const HKEY_LOCAL_MACHINE: isize = 0x8000_0002u32 as isize;
    const KEY_READ: u32 = 0x0002_0019;
    const KEY_WOW64_64KEY: u32 = 0x0100;
    const KEY_WOW64_32KEY: u32 = 0x0200;
    const REG_SZ: u32 = 1;
    const REG_EXPAND_SZ: u32 = 2;

    #[link(name = "advapi32")]
    extern "system" {
        fn RegOpenKeyExW(
            hkey: isize,
            subkey: *const u16,
            opts: u32,
            sam: u32,
            out: *mut isize,
        ) -> i32;
        fn RegEnumKeyExW(
            hkey: isize,
            index: u32,
            name: *mut u16,
            len: *mut u32,
            res: *mut u32,
            cls: *mut u16,
            cls_len: *mut u32,
            ft: *mut c_void,
        ) -> i32;
        fn RegQueryValueExW(
            hkey: isize,
            name: *const u16,
            res: *mut u32,
            kind: *mut u32,
            data: *mut u8,
            len: *mut u32,
        ) -> i32;
        fn RegCloseKey(hkey: isize) -> i32;
    }

    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let query_str = |hkey: isize, val_name: Option<&str>| -> Option<String> {
        let wn = val_name.map(wide);
        let ptr = wn.as_ref().map_or(std::ptr::null(), |v| v.as_ptr());
        let mut kind = 0u32;
        let mut len = 0u32;
        let rc = unsafe {
            RegQueryValueExW(
                hkey,
                ptr,
                std::ptr::null_mut(),
                &mut kind,
                std::ptr::null_mut(),
                &mut len,
            )
        };
        if rc != 0 || (kind != REG_SZ && kind != REG_EXPAND_SZ) || len == 0 || len > 8192 {
            return None;
        }
        let mut buf = vec![0u16; (len as usize).div_ceil(2)];
        let rc = unsafe {
            RegQueryValueExW(
                hkey,
                ptr,
                std::ptr::null_mut(),
                &mut kind,
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if rc != 0 {
            return None;
        }
        while buf.last() == Some(&0) {
            buf.pop();
        }
        Some(String::from_utf16_lossy(&buf))
    };

    let mut out = Vec::new();
    let sub = wide("Software\\Python\\PythonCore");
    for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        for (wow, default_arch) in [(KEY_WOW64_64KEY, "64"), (KEY_WOW64_32KEY, "32")] {
            let mut core = 0isize;
            if unsafe { RegOpenKeyExW(root, sub.as_ptr(), 0, KEY_READ | wow, &mut core) } != 0 {
                continue;
            }
            let mut idx = 0u32;
            loop {
                let mut name = [0u16; 256];
                let mut len = name.len() as u32;
                let rc = unsafe {
                    RegEnumKeyExW(
                        core,
                        idx,
                        name.as_mut_ptr(),
                        &mut len,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                if rc != 0 {
                    break;
                }
                idx += 1;
                let tag = String::from_utf16_lossy(&name[..len as usize]);
                let tag_sub = wide(&tag);
                let mut tag_arch_reg = None;
                let mut tk = 0isize;
                if unsafe { RegOpenKeyExW(core, tag_sub.as_ptr(), 0, KEY_READ | wow, &mut tk) } == 0
                {
                    if let Some(sa) = query_str(tk, Some("SysArchitecture")) {
                        tag_arch_reg = match sa.to_ascii_lowercase().as_str() {
                            "32bit" => Some("32".to_string()),
                            "64bit" => Some("64".to_string()),
                            "arm64" => Some("arm64".to_string()),
                            _ => None,
                        };
                    }
                    unsafe { RegCloseKey(tk) };
                }
                let ip_sub = wide(&format!("{tag}\\InstallPath"));
                let mut ip = 0isize;
                if unsafe { RegOpenKeyExW(core, ip_sub.as_ptr(), 0, KEY_READ | wow, &mut ip) } == 0
                {
                    let exe_path = query_str(ip, Some("ExecutablePath")).map(PathBuf::from);
                    let dir = query_str(ip, None).map(PathBuf::from).or_else(|| {
                        exe_path
                            .as_deref()
                            .and_then(|e| e.parent().map(Path::to_path_buf))
                    });
                    unsafe { RegCloseKey(ip) };
                    if let Some(d) = dir.filter(|d| {
                        exe_path.as_deref().is_some_and(Path::is_file)
                            || d.join("python.exe").is_file()
                    }) {
                        let (_, _, tag_arch) = parse_py_tag(&tag);
                        let arch = tag_arch
                            .or_else(|| exe_path.as_deref().and_then(pe_arch).map(String::from))
                            .or(tag_arch_reg)
                            .or_else(|| dir_arch(&d).map(String::from))
                            .or_else(|| Some(default_arch.to_string()));
                        out.push((tag, d, exe_path, arch));
                    }
                }
            }
            unsafe { RegCloseKey(core) };
        }
    }
    out
}

#[cfg(unix)]
fn is_executable_file(p: &Path) -> bool {
    use std::ffi::{c_char, c_int, CString};
    use std::os::unix::ffi::OsStrExt;
    const X_OK: c_int = 1;
    extern "C" {
        fn access(path: *const c_char, mode: c_int) -> c_int;
    }
    if !p.is_file() {
        return false;
    }
    CString::new(p.as_os_str().as_bytes()).is_ok_and(|c| unsafe { access(c.as_ptr(), X_OK) } == 0)
}

#[cfg(not(unix))]
fn is_executable_file(p: &Path) -> bool {
    p.is_file()
}

/// Find a real Python runtime on disk (zero execution) when no venv is active:
/// walks `PATH` for `python`, `python3`, `python3t`, and versioned `python3.<minor>[t]` skipping
/// launcher shims/aliases that lack runtime DLLs or site-packages, then falls back on Windows to
/// PEP 514 registry entries and standard install roots honoring `PYTHON_MANAGER_DEFAULT`/`PY_PYTHON`.
/// Returns `(home_dir, version, optional_exe, optional_arch)`.
fn find_system_python(start: &Path) -> Option<(PathBuf, String, Option<PathBuf>, Option<String>)> {
    type SysPy = (PathBuf, String, Option<PathBuf>, Option<String>);
    let mut fallback: Option<SysPy> = None;
    let mut try_exe = |exe: &Path| -> Option<SysPy> {
        let real = canon(exe);
        let d = real.parent().or_else(|| exe.parent())?;
        let exe_ver = detect_py_exe_version(&real).or_else(|| detect_py_exe_version(exe));
        let mut dir_ver = detect_py_version(d);
        if dir_ver.is_empty() {
            if let Some(ed) = exe.parent() {
                dir_ver = detect_py_version(ed);
            }
        }
        if !dir_ver.is_empty() || !lib_sites(d).is_empty() {
            let ver = exe_ver.unwrap_or(dir_ver);
            return Some((d.to_path_buf(), ver, Some(real), None));
        }
        if fallback.is_none() {
            fallback = Some((
                d.to_path_buf(),
                exe_ver.unwrap_or(dir_ver),
                Some(real),
                None,
            ));
        }
        None
    };
    if let Some(path) = std::env::var_os("PATH") {
        let path_dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        let exts: Vec<String> = if cfg!(windows) {
            std::env::var("PATHEXT")
                .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
                .split(';')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_ascii_lowercase())
                .collect()
        } else {
            vec![String::new()]
        };
        for stem in ["python", "python3", "python3t"] {
            for dir in &path_dirs {
                for ext in &exts {
                    let p = dir.join(format!("{stem}{ext}"));
                    if is_executable_file(&p) {
                        if let Some(hit) = try_exe(&p) {
                            return Some(hit);
                        }
                    }
                }
            }
        }
        // Versioned executables across all PATH dirs (e.g. macOS installer's `python3.14t`/`python3.14`),
        // ranked highest version first, standard before free-threaded, earlier PATH dir on ties.
        let mut versioned: Vec<(Vec<u64>, bool, usize, PathBuf)> = Vec::new();
        for (dir_idx, dir) in path_dirs.iter().enumerate() {
            let Ok(rd) = fs::read_dir(dir) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                if !is_executable_file(&p) {
                    continue;
                }
                let name = e.file_name().to_string_lossy().to_string();
                let lower = name.to_ascii_lowercase();
                if cfg!(windows) && !exts.iter().any(|ext| lower.ends_with(ext)) {
                    continue;
                }
                if !lower.starts_with("python3.") {
                    continue;
                }
                if let Some(v) = detect_py_exe_version(&p) {
                    let ft = v.ends_with('t');
                    versioned.push((ver_key(v.trim_end_matches('t')), ft, dir_idx, p));
                }
            }
        }
        versioned.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.cmp(&b.2))
        });
        for (_, _, _, p) in versioned {
            if let Some(hit) = try_exe(&p) {
                return Some(hit);
            }
        }
    }
    if let Some(exe) = which(start, "python").filter(|p| is_executable_file(p)) {
        if let Some(hit) = try_exe(&exe) {
            return Some(hit);
        }
    }
    #[cfg(windows)]
    {
        struct WinPy {
            key: Vec<u64>,
            dir: PathBuf,
            exe: Option<PathBuf>,
            ver: String,
            num_ver: String,
            free_threaded: bool,
            arch: Option<String>,
        }
        let has_py_exe = |p: &Path| -> bool {
            if p.join("python.exe").is_file() {
                return true;
            }
            fs::read_dir(p).ok().is_some_and(|rd| {
                rd.flatten().any(|e| {
                    let n = e.file_name().to_string_lossy().to_ascii_lowercase();
                    n.starts_with("python3") && n.ends_with(".exe") && e.path().is_file()
                })
            })
        };
        let mut found: Vec<WinPy> = Vec::new();
        let mut seen: HashSet<(PathBuf, String, bool, Option<String>)> = HashSet::new();
        let mut add = |tag_hint: Option<&str>,
                       p: PathBuf,
                       exe_path: Option<PathBuf>,
                       arch_hint: Option<String>| {
            if !exe_path.as_deref().is_some_and(Path::is_file) && !has_py_exe(&p) {
                return;
            }
            let disk_ver = {
                let v = detect_py_version(&p);
                if !v.is_empty() {
                    v
                } else {
                    exe_path
                        .as_deref()
                        .and_then(detect_py_exe_version)
                        .or_else(|| {
                            exe_path
                                .as_deref()
                                .and_then(|e| e.parent().map(detect_py_version))
                                .filter(|v| !v.is_empty())
                        })
                        .unwrap_or_default()
                }
            };
            let (num_ver, ft, tag_arch) = match tag_hint {
                Some(t) => {
                    let (v, ft, a) = parse_py_tag(t);
                    let v = if v.is_empty() {
                        disk_ver.trim_end_matches('t').to_string()
                    } else {
                        v
                    };
                    (v, ft, a)
                }
                None => (
                    disk_ver.trim_end_matches('t').to_string(),
                    disk_ver.ends_with('t'),
                    None,
                ),
            };
            if num_ver.is_empty() {
                return;
            }
            let arch = tag_arch
                .or(arch_hint)
                .or_else(|| exe_path.as_deref().and_then(pe_arch).map(String::from))
                .or_else(|| dir_arch(&p).map(String::from));
            let c = canon(&p);
            if !seen.insert((c, num_ver.clone(), ft, arch.clone())) {
                return;
            }
            let ver = if ft {
                format!("{num_ver}t")
            } else {
                num_ver.clone()
            };
            found.push(WinPy {
                key: ver_key(&num_ver),
                dir: p,
                exe: exe_path,
                ver,
                num_ver,
                free_threaded: ft,
                arch,
            });
        };
        // 1. Registered PEP 514 installs (finds C:\Python311 and any custom install/exe path).
        for (tag, dir, exe_path, arch) in win_pep514_installs() {
            add(Some(&tag), dir, exe_path, arch);
        }
        // 2. Standard filesystem install roots (handles shared dirs with both standard & `t` builds).
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Some(la) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
            let la = PathBuf::from(la);
            roots.push(la.join("Python"));
            roots.push(la.join("Programs").join("Python"));
        }
        for env_root in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(pf) = std::env::var_os(env_root).filter(|v| !v.is_empty()) {
                roots.push(PathBuf::from(pf));
            }
        }
        if let Some(sd) = std::env::var_os("SystemDrive").filter(|v| !v.is_empty()) {
            let mut s = sd.to_string_lossy().into_owned();
            if !s.ends_with('\\') {
                s.push('\\');
            }
            roots.push(PathBuf::from(s));
        }
        for root in roots {
            let Ok(rd) = fs::read_dir(&root) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    let vers = detect_py_versions_all(&p);
                    if vers.is_empty() {
                        add(None, p, None, None);
                    } else {
                        for v in vers {
                            let exe_hint = if v.ends_with('t') {
                                let ft_exe = p.join(format!("python{v}.exe"));
                                ft_exe.is_file().then_some(ft_exe)
                            } else {
                                let std_exe = p.join("python.exe");
                                std_exe.is_file().then_some(std_exe)
                            };
                            add(Some(&v), p.clone(), exe_hint, None);
                        }
                    }
                }
            }
        }
        // Highest version first; prefer standard (non-free-threaded) and 64-bit on ties.
        found.sort_by(|a, b| {
            b.key
                .cmp(&a.key)
                .then_with(|| a.free_threaded.cmp(&b.free_threaded))
                .then_with(|| {
                    (b.arch.as_deref() == Some("64")).cmp(&(a.arch.as_deref() == Some("64")))
                })
        });
        // Honor Windows Python Install Manager / `py` launcher default version/tag env vars.
        let want_raw = ["PYTHON_MANAGER_DEFAULT", "PY_PYTHON", "PY_PYTHON3"]
            .into_iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()));
        if let Some(raw) = want_raw {
            let (want_ver, want_ft, want_arch) = parse_py_tag(&raw);
            if let Some(hit) = found.iter().find(|c| {
                let ver_ok =
                    c.num_ver == want_ver || c.num_ver.starts_with(&format!("{want_ver}."));
                let ft_ok = c.free_threaded == want_ft;
                let arch_ok = want_arch
                    .as_deref()
                    .is_none_or(|wa| c.arch.as_deref() == Some(wa));
                ver_ok && ft_ok && arch_ok
            }) {
                return Some((
                    hit.dir.clone(),
                    hit.ver.clone(),
                    hit.exe.clone(),
                    hit.arch.clone(),
                ));
            }
            return None;
        }
        if let Some(hit) = found.into_iter().next() {
            return Some((hit.dir, hit.ver, hit.exe, hit.arch));
        }
    }
    fallback
}

/// Site-packages dirs for the nearest venv (plus system/user sites when the venv includes them).
pub fn py_sites(start: &Path) -> Vec<PathBuf> {
    let mut venvs: Vec<PathBuf> = Vec::new();
    'up: for dir in start.ancestors() {
        for n in [".venv", "venv", "env", ".env"] {
            let v = dir.join(n);
            if v.join("pyvenv.cfg").is_file() {
                venvs.push(v);
                break 'up;
            }
        }
    }
    if let Some(v) = std::env::var_os("VIRTUAL_ENV") {
        venvs.push(PathBuf::from(v));
    }
    let mut sites = Vec::new();
    let mut system: Option<(PathBuf, String, Option<PathBuf>, Option<String>)> = None;
    for v in &venvs {
        sites.extend(lib_sites(v));
        let cfg = fs::read_to_string(v.join("pyvenv.cfg")).unwrap_or_default();
        let get = |k: &str| {
            cfg.lines().find_map(|l| {
                let (a, b) = l.split_once('=')?;
                (a.trim() == k).then(|| b.trim().to_string())
            })
        };
        if get("include-system-site-packages").as_deref() == Some("true") {
            if let Some(h) = get("home") {
                let hp = PathBuf::from(h);
                let exe = get("executable").map(PathBuf::from);
                let detected = exe
                    .as_deref()
                    .and_then(detect_py_exe_version)
                    .unwrap_or_else(|| detect_py_version(&hp));
                let mut ver = get("version")
                    .or_else(|| get("version_info"))
                    .unwrap_or_else(|| detected.clone());
                if !ver.ends_with('t') && detected.ends_with('t') {
                    let parts: Vec<&str> = ver.split('.').take(2).collect();
                    if parts.len() == 2 {
                        ver = format!("{}.{}t", parts[0], parts[1]);
                    }
                }
                system = Some((hp, ver, exe, None));
            }
        }
    }
    if venvs.is_empty() {
        system = find_system_python(start);
    }
    if let Some((home, ver, exe, arch)) = system {
        // CPython `preconfig.c`: empty keeps user site enabled; non-empty strips ONLY ASCII
        // leading whitespace (C `isspace`) and requires an exact integer 0 ("0", " 0", "+0", "-0").
        let no_user = std::env::var("PYTHONNOUSERSITE").ok().is_some_and(|v| {
            !v.is_empty()
                && v.trim_start_matches(|c: char| {
                    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0B' | '\x0C')
                })
                .parse::<i64>()
                    != Ok(0)
        });
        let mm: Vec<&str> = ver.split('.').take(2).collect();
        if !no_user && mm.len() == 2 {
            let user_base = std::env::var_os("PYTHONUSERBASE").filter(|v| !v.is_empty());
            // macOS: detect framework name (e.g. `Python` or free-threaded `PythonT`) from
            // `home` or `executable` (canonicalized so Homebrew symlinks resolve into `.framework/`).
            #[cfg(target_os = "macos")]
            let fw_name: Option<String> = {
                let fw_of = |p: &Path| -> Option<String> {
                    for c in p.components() {
                        let s = c.as_os_str().to_string_lossy();
                        if let Some(name) = s.strip_suffix(".framework") {
                            return Some(name.to_string());
                        }
                    }
                    None
                };
                fw_of(&home)
                    .or_else(|| fw_of(&canon(&home)))
                    .or_else(|| exe.as_deref().and_then(fw_of))
                    .or_else(|| exe.as_deref().map(canon).as_deref().and_then(fw_of))
            };
            #[cfg(not(target_os = "macos"))]
            let _ = &exe;
            #[cfg(not(windows))]
            let _ = &arch;
            #[cfg(windows)]
            let win_plat = match arch
                .as_deref()
                .or_else(|| exe.as_deref().and_then(pe_arch))
                .or_else(|| dir_arch(&home))
            {
                Some("32") => "-32",
                Some("arm64") => "-arm64",
                _ => "",
            };
            if let Some(base) = user_base {
                let b = PathBuf::from(base);
                #[cfg(windows)]
                sites.push(
                    b.join(format!("Python{}{}{win_plat}", mm[0], mm[1]))
                        .join("site-packages"),
                );
                #[cfg(target_os = "macos")]
                if fw_name.is_some() {
                    sites.push(b.join("lib").join("python").join("site-packages"));
                } else {
                    sites.push(
                        b.join("lib")
                            .join(format!("python{}.{}", mm[0], mm[1]))
                            .join("site-packages"),
                    );
                }
                #[cfg(all(not(windows), not(target_os = "macos")))]
                sites.push(
                    b.join("lib")
                        .join(format!("python{}.{}", mm[0], mm[1]))
                        .join("site-packages"),
                );
            } else {
                #[cfg(windows)]
                if let Some(base) = std::env::var_os("APPDATA")
                    .filter(|v| !v.is_empty())
                    .or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()))
                    .or_else(|| std::env::var_os("HOME").filter(|v| !v.is_empty()))
                    .or_else(|| {
                        let hp = std::env::var_os("HOMEPATH").filter(|v| !v.is_empty())?;
                        let mut full = std::env::var_os("HOMEDRIVE").unwrap_or_default();
                        full.push(hp);
                        Some(full)
                    })
                {
                    sites.push(
                        PathBuf::from(base)
                            .join("Python")
                            .join(format!("Python{}{}{win_plat}", mm[0], mm[1]))
                            .join("site-packages"),
                    );
                }
                #[cfg(target_os = "macos")]
                if let Some(h) = home_dir_unix() {
                    if let Some(fw) = &fw_name {
                        sites.push(
                            h.join("Library")
                                .join(fw)
                                .join(format!("{}.{}", mm[0], mm[1].trim_end_matches('t')))
                                .join("lib/python/site-packages"),
                        );
                    } else {
                        sites.push(
                            h.join(".local/lib")
                                .join(format!("python{}.{}", mm[0], mm[1]))
                                .join("site-packages"),
                        );
                    }
                }
                #[cfg(all(not(windows), not(target_os = "macos")))]
                if let Some(h) = home_dir_unix() {
                    sites.push(
                        h.join(".local/lib")
                            .join(format!("python{}.{}", mm[0], mm[1]))
                            .join("site-packages"),
                    );
                }
            }
        }
        let ver_opt = (!ver.is_empty()).then_some(ver.as_str());
        sites.extend(lib_sites_for(&home, ver_opt));
        if let Some(parent) = home.parent() {
            sites.extend(lib_sites_for(parent, ver_opt)); // unix: home is <prefix>/bin
        }
    }
    let mut seen = HashSet::new();
    sites.retain(|s| s.is_dir() && seen.insert(s.clone()));
    sites
}

#[cfg(not(windows))]
fn home_dir_unix() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn lib_sites(prefix: &Path) -> Vec<PathBuf> {
    lib_sites_for(prefix, None)
}

fn lib_sites_for(prefix: &Path, ver: Option<&str>) -> Vec<PathBuf> {
    let mut v = vec![prefix.join("Lib").join("site-packages")];
    let want_dir = ver.and_then(|ver| {
        let parts: Vec<&str> = ver.split('.').take(2).collect();
        (parts.len() == 2).then(|| format!("python{}.{}", parts[0], parts[1]))
    });
    if let Ok(rd) = fs::read_dir(prefix.join("lib")) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let matched = match &want_dir {
                Some(want) => name == *want,
                None => name.starts_with("python"),
            };
            if matched {
                v.push(e.path().join("site-packages"));
            }
        }
    }
    v.into_iter().filter(|p| p.is_dir()).collect()
}

fn norm_py(s: &str) -> String {
    s.to_lowercase().replace(['-', '.'], "_")
}

fn dist_infos(site: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(site)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    v.retain(|p| {
        let n = p.to_string_lossy();
        n.ends_with(".dist-info") || n.ends_with(".egg-info")
    });
    v
}

fn dist_meta(d: &Path) -> (String, String) {
    let txt = fs::read_to_string(d.join("METADATA"))
        .or_else(|_| fs::read_to_string(d.join("PKG-INFO")))
        .unwrap_or_default();
    let field = |k: &str| {
        txt.lines()
            .take_while(|l| !l.is_empty())
            .find_map(|l| l.strip_prefix(k))
            .map(|v| v.trim().to_string())
    };
    (
        field("Name:").unwrap_or_default(),
        field("Version:").unwrap_or("?".into()),
    )
}

/// The dist that installed top-level module `module` (name match, then top_level.txt, then RECORD).
fn py_dist(site: &Path, module: &str) -> Option<(String, String)> {
    let ds = dist_infos(site);
    let m = norm_py(module);
    let stem = |d: &Path| {
        d.file_name()
            .unwrap()
            .to_string_lossy()
            .split('-')
            .next()
            .unwrap_or("")
            .to_string()
    };
    if let Some(d) = ds.iter().find(|d| norm_py(&stem(d)) == m) {
        return Some(dist_meta(d));
    }
    for d in &ds {
        let tl = fs::read_to_string(d.join("top_level.txt")).unwrap_or_default();
        if tl.lines().any(|l| l.trim() == module) {
            return Some(dist_meta(d));
        }
    }
    let (dir_pre, file_pre) = (format!("{module}/"), format!("{module}.py"));
    for d in &ds {
        let rec = fs::read_to_string(d.join("RECORD")).unwrap_or_default();
        if rec
            .lines()
            .any(|l| l.starts_with(&dir_pre) || l.starts_with(&file_pre))
        {
            return Some(dist_meta(d));
        }
    }
    None
}

fn py_pkg(start: &Path, spec: &str) -> Option<Pkg> {
    let sites = py_sites(start);
    let top = spec.split('.').next().unwrap_or(spec);
    let mut module = top.replace('-', "_");
    // `tc dep scikit-learn` names a dist, not a module: map it through top_level.txt.
    if !sites.iter().any(|s| {
        s.join(&module).is_dir()
            || s.join(format!("{module}.py")).is_file()
            || s.join(format!("{module}.pyi")).is_file()
    }) {
        let found = sites.iter().find_map(|s| {
            dist_infos(s).into_iter().find(|d| {
                norm_py(
                    d.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .split('-')
                        .next()
                        .unwrap_or(""),
                ) == norm_py(top)
            })
        })?;
        let tl = fs::read_to_string(found.join("top_level.txt")).unwrap_or_default();
        module = tl
            .lines()
            .next()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())?;
    }
    // PEP 420: a regular package (`__init__.pyi|.py`) or regular module file (`.pyi|.py`) in any
    // site always takes precedence over a namespace directory (no `__init__`) in an earlier site.
    for pass in 0..2 {
        for site in &sites {
            let dir = site.join(&module);
            let (entry, d) = if pass == 0 {
                let init = ["__init__.pyi", "__init__.py"]
                    .iter()
                    .map(|f| dir.join(f))
                    .find(|p| p.is_file());
                if let Some(e) = init {
                    (Some(e), Some(dir))
                } else if let Some(e) = [format!("{module}.pyi"), format!("{module}.py")]
                    .iter()
                    .map(|f| site.join(f))
                    .find(|p| p.is_file())
                {
                    (Some(e.clone()), Some(e))
                } else {
                    continue;
                }
            } else if dir.is_dir() {
                (None, Some(dir))
            } else {
                continue;
            };
            let (name, version) =
                py_dist(site, &module).unwrap_or((module.clone(), "? (no dist-info)".into()));
            return Some(Pkg {
                eco: "python",
                name: if name.is_empty() { module } else { name },
                version,
                dir: d,
                entry,
                sites: sites.clone(),
                ..Pkg::default()
            });
        }
    }
    None
}

/// One `[[package]]` of Cargo.lock. `deps` entries are `name`, `name version` or
/// `name version (source)`; a version is only written when the name is ambiguous.
#[derive(Default)]
struct LockPkg {
    name: String,
    version: String,
    local: bool,
    deps: Vec<String>,
}

fn parse_lock(txt: &str) -> Vec<LockPkg> {
    let mut out: Vec<LockPkg> = Vec::new();
    let (mut in_pkg, mut in_deps) = (false, false);
    let push_dep = |p: &mut LockPkg, d: &str| {
        let d = d.trim().trim_end_matches(',').trim().trim_matches('"');
        if !d.is_empty() {
            p.deps.push(d.to_string());
        }
    };
    for l in txt.lines() {
        let t = l.trim();
        if t.starts_with('[') && !in_deps {
            in_pkg = t == "[[package]]";
            if in_pkg {
                out.push(LockPkg {
                    local: true,
                    ..LockPkg::default()
                });
            }
            continue;
        }
        let Some(p) = out.last_mut().filter(|_| in_pkg) else {
            continue;
        };
        if in_deps {
            if t.starts_with(']') {
                in_deps = false;
            } else {
                push_dep(p, t);
            }
        } else if let Some(v) = t.strip_prefix("name = ") {
            p.name = v.trim_matches('"').to_string();
        } else if let Some(v) = t.strip_prefix("version = ") {
            p.version = v.trim_matches('"').to_string();
        } else if t.starts_with("source = ") {
            p.local = false;
        } else if let Some(v) = t.strip_prefix("dependencies = [") {
            match v.strip_suffix(']') {
                Some(inline) => inline.split(',').for_each(|d| push_dep(p, d)),
                None => in_deps = true,
            }
        }
    }
    out
}

/// `[package] name` of the nearest Cargo.toml (None at a virtual workspace root).
fn cargo_member(start: &Path) -> Option<String> {
    let toml = start
        .ancestors()
        .map(|d| d.join("Cargo.toml"))
        .find(|p| p.is_file())?;
    let mut in_pkg = false;
    for l in fs::read_to_string(toml).ok()?.lines() {
        let t = l.trim();
        if t.starts_with('[') {
            in_pkg = t == "[package]";
        } else if let Some(v) = t.strip_prefix("name").filter(|_| in_pkg) {
            if let Some(v) = v.trim_start().strip_prefix('=') {
                return Some(v.trim().trim_matches('"').to_string());
            }
        }
    }
    None
}

/// The locked version of crate `want` for the workspace member around `start`: its direct
/// dependency edge, else the only version reachable from it, else the only version locked.
/// Several candidates left → the highest, with a note naming all of them.
fn locked_version(start: &Path, want: &str) -> Option<(String, String, Option<String>)> {
    let lock = start
        .ancestors()
        .map(|d| d.join("Cargo.lock"))
        .find(|p| p.is_file())?;
    let lock = parse_lock(&fs::read_to_string(lock).ok()?);
    let same = |n: &str| n.replace('-', "_") == want;
    let mut versions: Vec<&LockPkg> = lock.iter().filter(|p| same(&p.name)).collect();
    versions.sort_by_key(|p| std::cmp::Reverse(ver_key(&p.version)));
    versions.dedup_by(|a, b| a.version == b.version);
    let first = versions.first()?;
    let pick = |v: &str| (first.name.clone(), v.to_string(), None);
    if versions.len() == 1 {
        return Some(pick(&first.version));
    }
    let member_name = cargo_member(start);
    let member = member_name
        .as_deref()
        .and_then(|m| lock.iter().find(|p| p.name == m && p.local));
    // Dependency edge → the packages it names (by version when the edge carries one).
    let targets = |d: &str| -> Vec<&LockPkg> {
        let mut it = d.split(' ');
        let (n, v) = (it.next().unwrap_or(""), it.next());
        lock.iter()
            .filter(|p| p.name == n && v.is_none_or(|v| p.version == v))
            .collect()
    };
    if let Some(m) = member {
        let mut direct: Vec<&str> = Vec::new();
        for d in &m.deps {
            for t in targets(d) {
                if same(&t.name) && !direct.contains(&t.version.as_str()) {
                    direct.push(&t.version);
                }
            }
        }
        if direct.len() == 1 {
            return Some(pick(direct[0]));
        }
        if direct.len() > 1 {
            direct.sort_by_key(|v| std::cmp::Reverse(ver_key(v)));
            let chosen = direct[0].to_string();
            return Some((
                first.name.clone(),
                chosen.clone(),
                Some(format!(
                    "ambiguous: `{}` depends directly on {} {}; showing {chosen}",
                    m.name,
                    first.name,
                    direct.join(", ")
                )),
            ));
        }
        let mut reach: Vec<&str> = Vec::new();
        let mut seen: HashSet<(&str, &str)> = HashSet::new();
        let mut queue = vec![m];
        while let Some(p) = queue.pop() {
            for d in &p.deps {
                for t in targets(d) {
                    if same(&t.name) && !reach.contains(&t.version.as_str()) {
                        reach.push(&t.version);
                    }
                    if seen.insert((&t.name, &t.version)) {
                        queue.push(t);
                    }
                }
            }
        }
        if reach.len() == 1 {
            return Some(pick(reach[0]));
        }
    }
    let all: Vec<&str> = versions.iter().map(|p| p.version.as_str()).collect();
    let why = match member_name {
        Some(m) => format!("`{m}` does not depend on it directly"),
        None => "no workspace member here to pick one".into(),
    };
    Some((
        first.name.clone(),
        first.version.clone(),
        Some(format!(
            "ambiguous: Cargo.lock has {} {}; {why}; showing {}",
            first.name,
            all.join(", "),
            first.version
        )),
    ))
}

fn ver_key(v: &str) -> Vec<u64> {
    v.split(|c: char| !c.is_ascii_digit())
        .filter_map(|x| x.parse().ok())
        .collect()
}

fn rust_pkg(start: &Path, spec: &str) -> Option<Pkg> {
    let (name, version, note) = locked_version(start, &spec.replace('-', "_"))?;
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".cargo")))?;
    let dir = fs::read_dir(cargo_home.join("registry").join("src"))
        .ok()
        .and_then(|rd| {
            rd.flatten()
                .map(|e| e.path().join(format!("{name}-{version}")))
                .find(|p| p.is_dir())
        });
    // `[lib] path` (tree-sitter: `binding_rust/lib.rs`), else `src/lib.rs`.
    let entry = dir
        .as_ref()
        .map(|d| d.join(crate::lang::rs_lib_rel(d).iter().collect::<PathBuf>()))
        .filter(|p| p.is_file());
    Some(Pkg {
        eco: "rust",
        name,
        version,
        dir,
        entry,
        note,
        ..Pkg::default()
    })
}

/// `cmd` on PATH; a path (`./tool`, `bin\x.exe`) is taken relative to `cwd`.
/// Always returns an absolute path so `Command::new(&exe).current_dir(cwd)` launches the exact
/// file that was checked even when `PATH` or `cwd` is relative.
pub fn which(cwd: &Path, cmd: &str) -> Option<PathBuf> {
    let abs = |p: PathBuf| std::path::absolute(&p).unwrap_or(p);
    if cmd.contains('/') || cmd.contains('\\') {
        let p = abs(cwd.join(cmd));
        return is_executable_file(&p).then_some(p);
    }
    let exts: Vec<String> = if cfg!(windows) {
        let pe = std::env::var("PATHEXT").unwrap_or(".COM;.EXE;.BAT;.CMD".into());
        let mut v: Vec<String> = pe.split(';').map(|e| e.to_ascii_lowercase()).collect();
        if Path::new(cmd).extension().is_some() {
            v.insert(0, String::new());
        }
        v
    } else {
        vec![String::new()]
    };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|d| {
        exts.iter()
            .map(|e| abs(d.join(format!("{cmd}{e}"))))
            .find(|p| is_executable_file(p))
    })
}

// ---------------------------------------------------------------- package files & lookup

/// Files that make up the package's API surface.
pub fn pkg_files(pkg: &Pkg) -> Vec<PathBuf> {
    match pkg.eco {
        "node" => {
            let Some(entry) = pkg.entry.clone() else {
                return vec![];
            };
            let mut out = vec![entry.clone()];
            let mut seen: HashSet<PathBuf> = HashSet::from([canon(&entry)]);
            let mut i = 0;
            while i < out.len() && out.len() < 400 {
                if let Some(p) = Parsed::read(&out[i]) {
                    for imp in p.imports() {
                        if !imp.module.starts_with('.') {
                            continue;
                        }
                        if let Some(f) = resolve_rel(out[i].parent().unwrap(), &imp.module, true) {
                            // Canonical: one file reached through two link paths is one file.
                            if seen.insert(canon(&f)) {
                                out.push(f);
                            }
                        }
                    }
                }
                i += 1;
            }
            out
        }
        "python" | "rust" => {
            let Some(dir) = &pkg.dir else { return vec![] };
            if dir.is_file() {
                return vec![dir.clone()];
            }
            let root = if pkg.eco == "rust" {
                // The library root's directory: `src/`, or `binding_rust/` for a custom `[lib] path`.
                pkg.entry
                    .as_deref()
                    .and_then(Path::parent)
                    .map_or_else(|| dir.join("src"), Path::to_path_buf)
            } else {
                dir.clone()
            };
            let mut files = Vec::new();
            collect_files(&root, &mut files, 0);
            if pkg.eco == "rust" && root == *dir {
                // `[lib] path = "lib.rs"`: the crate dir's `build.rs`, `src/`, .. are other targets.
                files.retain(|f| {
                    let rel: Vec<String> = f
                        .strip_prefix(dir)
                        .into_iter()
                        .flat_map(Path::components)
                        .filter_map(|c| c.as_os_str().to_str().map(String::from))
                        .collect();
                    crate::lang::rs_in_lib_dir(&[], &rel)
                });
            }
            sort_shallow_first(&root, &mut files);
            files
        }
        _ => vec![],
    }
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 12 || out.len() > 6000 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        // file_type() comes with the directory entry: no extra stat per file (torch has ~14k).
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            let name = e.file_name();
            let skip = [
                "__pycache__",
                "tests",
                "test",
                "node_modules",
                "benches",
                "examples",
                "include",
                "share",
                "bin",
            ];
            if !skip.iter().any(|s| name == *s) {
                collect_files(&p, out, depth + 1);
            }
        } else if Lang::of(&p).is_some() {
            out.push(p);
        }
    }
}

/// Resolve a relative module specifier (`./x`, `./x.js`, `./x.ts`) to a file.
pub fn resolve_rel(dir: &Path, spec: &str, dts: bool) -> Option<PathBuf> {
    let base = dir.join(spec);
    let s = base.to_string_lossy().to_string();
    let stem = ["js", "mjs", "cjs", "jsx"]
        .iter()
        .find_map(|e| s.strip_suffix(&format!(".{e}")))
        .unwrap_or(&s)
        .to_string();
    let mut cands: Vec<String> = Vec::new();
    if dts {
        cands.extend([
            format!("{stem}.d.ts"),
            format!("{stem}.d.mts"),
            format!("{stem}.d.cts"),
            format!("{stem}/index.d.ts"),
        ]);
    }
    cands.push(s.clone());
    for e in ["ts", "tsx", "js", "jsx", "mjs", "py", "rs"] {
        cands.push(format!("{stem}.{e}"));
    }
    for e in ["ts", "tsx", "js"] {
        cands.push(format!("{stem}/index.{e}"));
    }
    cands
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .map(|p| lexical_norm(&p))
}

/// Drop `.` and fold `..` without touching the filesystem (keeps `src/./x.ts` out of output).
pub fn lexical_norm(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub file: String,
    pub line: usize,
    pub kind: &'static str,
    pub name: String,
    pub sig: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub doc: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<Symbol>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub import_from: Option<String>,
    /// Set when `name` is an alias: the name at the declaration (`export { IBuf as IBufferLine }`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declared_as: Option<String>,
    /// TS/JS: line of the same-file `export { ... }` / `export default x` statement that makes
    /// a declaration without an inline `export` public (`declare class Channel` … `export { Channel }`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub export_line: Option<usize>,
    #[serde(skip)]
    pub file_bytes: usize,
    #[serde(skip)]
    pub rank: (u8, u8, u8, usize),
}

const DEF_WORDS: &[&str] = &[
    "def",
    "class",
    "fn",
    "struct",
    "enum",
    "trait",
    "type",
    "const",
    "static",
    "mod",
    "union",
    "macro_rules!",
    "interface",
    "function",
    "let",
    "var",
    "namespace",
];

/// Cheap text test: does `name` look defined (not just used) somewhere in `text`?
fn looks_defined(text: &str, name: &str) -> bool {
    text.lines().any(|l| {
        l.match_indices(name).any(|(i, _)| {
            if !word_at(l, i, name.len()) {
                return false;
            }
            let before = l[..i].trim_end();
            let last = before
                .rsplit(|c: char| c.is_whitespace() || c == '(')
                .next()
                .unwrap_or("");
            if DEF_WORDS.contains(&last) {
                return true;
            }
            let after = l[i + name.len()..].trim_start();
            before.is_empty()
                && (after.starts_with('=') && !after.starts_with("==")
                    || after.starts_with(':')
                    || after.starts_with('('))
        })
    })
}

/// (top-level names, imports) of an `__init__`/entry file.
type Head = (Vec<String>, Vec<Import>);

/// Lazily built view of a package: the full file list is only walked when a targeted search fails.
pub struct Index {
    all: Option<Vec<PathBuf>>,
    texts: HashMap<PathBuf, String>,
    /// (top-level names, imports) of `__init__`/entry files, parsed once.
    heads: HashMap<PathBuf, Option<Head>>,
    /// Node: the entry's public names.
    exports: Option<Vec<Export>>,
}

impl Index {
    pub fn new() -> Index {
        Index {
            all: None,
            texts: HashMap::new(),
            heads: HashMap::new(),
            exports: None,
        }
    }
    fn node_exports(&mut self, pkg: &Pkg) -> Vec<Export> {
        self.exports
            .get_or_insert_with(|| match &pkg.entry {
                Some(e) => exports_of(e, &mut HashMap::new(), &mut HashSet::new()),
                None => vec![],
            })
            .clone()
    }
    pub fn files(&mut self, pkg: &Pkg) -> Vec<PathBuf> {
        self.all.get_or_insert_with(|| pkg_files(pkg)).clone()
    }
    fn text(&mut self, f: &Path) -> &str {
        self.texts
            .entry(f.to_path_buf())
            .or_insert_with(|| fs::read_to_string(f).unwrap_or_default())
    }
    fn head(&mut self, f: &Path) -> Option<&Head> {
        self.heads
            .entry(f.to_path_buf())
            .or_insert_with(|| {
                let p = Parsed::read(f)?;
                // Rust: an inline `mod`'s items by path (`__private::format_err`), not as root names.
                let names = p
                    .symbols()
                    .into_iter()
                    .filter(|s| s.parent.is_none())
                    .map(|s| match s.module.as_str() {
                        "" => s.name,
                        m => format!("{m}::{}", s.name),
                    })
                    .collect();
                Some((names, p.imports()))
            })
            .as_ref()
    }
    /// Files under module path `prefix` (relative to the package dir) without walking the whole package.
    fn subtree(&mut self, pkg: &Pkg, prefix: &str) -> Vec<PathBuf> {
        let Some(dir) = pkg.dir.clone().filter(|d| d.is_dir()) else {
            return if prefix.is_empty() {
                self.files(pkg)
            } else {
                vec![]
            };
        };
        if pkg.eco == "node" || self.all.is_some() {
            // `prefix` itself, `prefix.<ext>` or anything under `prefix/` (not `prefixes.py`).
            let inside = |r: &str| {
                r.strip_prefix(prefix).is_some_and(|x| {
                    x.is_empty() || x.starts_with('/') || (x.starts_with('.') && !x.contains('/'))
                })
            };
            return self
                .files(pkg)
                .into_iter()
                .filter(|f| inside(&slash(f.strip_prefix(&dir).unwrap_or(f))))
                .collect();
        }
        let mut out = Vec::new();
        for ext in ["pyi", "py", "rs"] {
            let f = dir.join(format!("{prefix}.{ext}"));
            if f.is_file() {
                out.push(f);
            }
        }
        let sub = dir.join(prefix);
        if sub.is_dir() {
            collect_files(&sub, &mut out, 0);
        } else if sub.is_file() {
            out.push(sub);
        }
        sort_shallow_first(&dir, &mut out);
        out
    }
}

fn sort_shallow_first(root: &Path, files: &mut [PathBuf]) {
    files.sort_by_key(|f| {
        let depth = f.strip_prefix(root).unwrap_or(f).components().count();
        let pyi = f.extension().is_some_and(|e| e == "pyi");
        (depth, !pyi, f.clone())
    });
}

/// Look `query` up (`Name`, `Owner.member`, `Owner::member`) in the installed package.
/// `hint` is a module path the caller named (`tc dep torch.nn X`, `from a.b import X`); Python
/// lookups then stay inside that module.
pub fn lookup(pkg: &Pkg, idx: &mut Index, query: &str, hint: Option<&str>) -> Vec<Hit> {
    let hint = hint.filter(|h| !h.is_empty());
    match pkg.eco {
        "node" => {
            if let Some(h) = node_lookup(pkg, idx, query) {
                return h;
            }
        }
        "python" => {
            if let Some(h) = py_lookup(pkg, idx, query, hint) {
                return h;
            }
            if let Some(h) = hint {
                // Keep explicit dotted module lookups strictly in that module file (never walk
                // unimported descendants or fall back to a standalone top-level `.pyi`).
                return py_scan_module(pkg, query, h);
            }
        }
        _ => {}
    }
    scan(pkg, idx, query, hint, false, None)
}

/// A hit for symbol `s` of `syms` (parsed from `f`), with its public members when it's a container.
fn make_hit(
    pkg: &Pkg,
    f: &Path,
    syms: &[Symbol],
    s: &Symbol,
    file_bytes: usize,
    rank: (u8, u8, u8, usize),
) -> Hit {
    let container = matches!(
        s.kind,
        "class" | "interface" | "trait" | "struct" | "enum" | "namespace"
    );
    // A `.d.ts` class is published as declared: its `private x` / `#private` fields are part of
    // the type shape (they block structural assignment), so list them too.
    let all = s.kind == "class" && is_dts(f);
    // Members sit inside the declaration, except Rust `impl` items: those live in separate
    // `impl Type` blocks, matched to `s` by path and `#[cfg]` (`rs_owners`), so each `#[cfg]`
    // twin of a struct keeps its own fields and impls.
    let members: Vec<Symbol> = if container {
        syms.iter()
            .filter(|m| m.exported || all)
            .filter(|m| {
                let named = m.parent.as_deref() == Some(&s.name);
                (named && m.bytes.0 >= s.bytes.0 && m.bytes.1 <= s.bytes.1)
                    // `impl crate::Alias` may name `s` through a renaming re-export.
                    || (pkg.eco == "rust"
                        && m.impl_ty.as_deref().is_some_and(|t| named || t.starts_with("crate::"))
                        && rs_owners(syms, m, Some(f)).iter().any(|&o| syms[o].bytes == s.bytes))
            })
            .cloned()
            .collect()
    } else {
        vec![]
    };
    Hit {
        file: pkg.rel(f),
        line: s.line,
        kind: s.kind,
        name: s.qual(),
        sig: s.sig.clone(),
        doc: s.doc.clone(),
        members,
        import_from: None,
        declared_as: None,
        export_line: None,
        file_bytes,
        rank,
    }
}

/// Symbols of `syms` named `name` (top level, or members of `owner`).
fn named<'s>(syms: &'s [Symbol], name: &str, owner: Option<&str>) -> Vec<&'s Symbol> {
    syms.iter()
        .filter(|s| s.name == name && s.parent.as_deref() == owner)
        .collect()
}

/// Split `rest` (the parts after the resolved first name `orig`) into (name, owner).
fn name_owner<'q>(orig: &'q str, rest: &[&'q str]) -> (&'q str, Option<&'q str>) {
    match rest {
        [] => (orig, None),
        [m] => (*m, Some(orig)),
        [.., o, m] => (*m, Some(*o)),
    }
}

/// One public name of a JS/TS module and where it is declared (`orig` in `file`).
/// `orig == "*"` is a namespace re-export (`export * as ns from './x'`).
#[derive(Debug, Clone)]
pub struct Export {
    pub public: String,
    pub file: PathBuf,
    pub orig: String,
    pub kind: &'static str,
    /// Line of the `export { orig as x }` / `export default orig` statement in `file` that this
    /// name goes back to, kept through renaming re-exports (`Hit::export_line`).
    pub line: Option<usize>,
}

/// The public names of module `file`, following export edges only: `export` declarations,
/// `export { a as b }` (of a local or an imported binding), `export ... from` and `export * from`.
/// An `import` alone exports nothing. Files are keyed by canonical path, so a cycle or a file
/// reached through a symlink or junction is visited once.
fn exports_of(
    file: &Path,
    memo: &mut HashMap<PathBuf, Vec<Export>>,
    busy: &mut HashSet<PathBuf>,
) -> Vec<Export> {
    let key = canon(file);
    if let Some(v) = memo.get(&key) {
        return v.clone();
    }
    if busy.len() > 64 || !busy.insert(key.clone()) {
        return vec![];
    }
    let mut out: Vec<Export> = Vec::new();
    fn add(out: &mut Vec<Export>, e: Export) {
        if !out.iter().any(|x| x.public == e.public) {
            out.push(e);
        }
    }
    if let Some(p) = Parsed::read(&key) {
        let syms = p.symbols();
        let imports = p.imports();
        let dir = key.parent().unwrap_or(Path::new("."));
        let target = |m: &str| {
            m.starts_with('.')
                .then(|| resolve_rel(dir, m, true))
                .flatten()
        };
        // Public: a top-level `export` declaration (checked on the AST so multi-declarator
        // `export const a = 1, b = 2` and `export default class Foo` are exact), and in an ambient
        // `.d.ts` with no module syntax every top-level decl (including `declare module "pkg"`).
        // Module augmentations inside an external module (`has_export_declarations() == true`)
        // augment another package and are NOT root exports of this module.
        let context = is_dts(&key) && !p.has_export_declarations();
        for s in syms.iter().filter(|s| s.parent.is_none()) {
            let ek = p.export_kind(s);
            let public = match ek {
                Some(true) => "default".into(),
                Some(false) => s.name.clone(),
                None if context => s.name.clone(),
                None => continue,
            };
            add(
                &mut out,
                Export {
                    public,
                    file: key.clone(),
                    orig: s.name.clone(),
                    kind: s.kind,
                    line: None,
                },
            );
        }
        for (local, public, line) in p.local_exports() {
            if public == "=" {
                // `export = Lib`: the default export, and (as TypeScript allows
                // `import { make } from 'pkg'`) every member of `namespace Lib` by name.
                let kind = syms
                    .iter()
                    .find(|s| s.parent.is_none() && s.name == local)
                    .map_or("namespace", |s| s.kind);
                let e = Export {
                    public: "default".into(),
                    file: key.clone(),
                    orig: local.clone(),
                    kind,
                    line: Some(line),
                };
                add(&mut out, e);
                // Members of `namespace Lib` blocks only, not of a merged `class Lib`.
                let blocks: Vec<(usize, usize)> = syms
                    .iter()
                    .filter(|s| s.parent.is_none() && s.name == local && s.kind == "namespace")
                    .map(|s| s.bytes)
                    .collect();
                for m in syms.iter().filter(|m| {
                    m.parent.as_deref() == Some(local.as_str())
                        && blocks
                            .iter()
                            .any(|(a, b)| m.bytes.0 >= *a && m.bytes.1 <= *b)
                }) {
                    let e = Export {
                        public: m.name.clone(),
                        file: key.clone(),
                        orig: format!("{local}.{}", m.name),
                        // the collector calls a function inside a namespace a "method"
                        kind: if m.kind == "method" {
                            "function"
                        } else {
                            m.kind
                        },
                        line: None,
                    };
                    add(&mut out, e);
                }
                continue;
            }
            if let Some(s) = syms.iter().find(|s| s.parent.is_none() && s.name == local) {
                let (file, kind) = (key.clone(), s.kind);
                add(
                    &mut out,
                    Export {
                        public,
                        file,
                        orig: local,
                        kind,
                        line: Some(line),
                    },
                );
                continue;
            }
            // `import { x as local } from './y'; export { local as public }`
            for imp in imports.iter().filter(|i| !i.reexport) {
                let Some(t) = target(&imp.module) else {
                    continue;
                };
                if imp.alias.as_deref() == Some(local.as_str()) {
                    let e = Export {
                        public: public.clone(),
                        file: t,
                        orig: "*".into(),
                        kind: "namespace",
                        line: None,
                    };
                    add(&mut out, e);
                    break;
                }
                if let Some((orig, _)) = imp.names.iter().find(|(_, l)| *l == local) {
                    if let Some(e) = exports_of(&t, memo, busy)
                        .into_iter()
                        .find(|e| e.public == *orig)
                    {
                        add(
                            &mut out,
                            Export {
                                public: public.clone(),
                                ..e
                            },
                        );
                    }
                    break;
                }
            }
        }
        // Named re-exports win over `export *` (ES module rules), so they go first.
        let edges: Vec<&Import> = imports.iter().filter(|i| i.reexport).collect();
        for imp in edges.iter().filter(|i| !i.wildcard) {
            let Some(t) = target(&imp.module) else {
                continue;
            };
            if let Some(ns) = &imp.alias {
                let e = Export {
                    public: ns.clone(),
                    file: t,
                    orig: "*".into(),
                    kind: "namespace",
                    line: None,
                };
                add(&mut out, e);
                continue;
            }
            let theirs = exports_of(&t, memo, busy);
            for (orig, public) in &imp.names {
                if let Some(e) = theirs.iter().find(|e| e.public == *orig) {
                    add(
                        &mut out,
                        Export {
                            public: public.clone(),
                            ..e.clone()
                        },
                    );
                }
            }
        }
        for imp in edges.iter().filter(|i| i.wildcard) {
            let Some(t) = target(&imp.module) else {
                continue;
            };
            for e in exports_of(&t, memo, busy) {
                if e.public != "default" {
                    add(&mut out, e);
                }
            }
        }
    }
    busy.remove(&key);
    memo.insert(key, out.clone());
    out
}

/// Nearest ancestor `package.json` `"type"` for a `.js` file (stops at `pkg.dir`), per Node's
/// package scope rules: nested `package.json` overrides the package root.
fn nearest_pkg_type_is_esm(pkg: &Pkg, entry: &Path) -> bool {
    let stop = pkg.dir.as_deref();
    for dir in entry.parent().into_iter().flat_map(Path::ancestors) {
        if let Ok(txt) = fs::read_to_string(dir.join("package.json")) {
            if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                return v["type"].as_str() == Some("module");
            }
            return false;
        }
        if stop.is_some_and(|s| dir == s) {
            break;
        }
    }
    pkg.esm
}

fn is_esm_entry(pkg: &Pkg, entry: &Path) -> bool {
    let s = entry.to_string_lossy();
    if is_dts(entry) || s.ends_with(".mjs") || s.ends_with(".mts") {
        return true;
    }
    if s.ends_with(".cjs") || s.ends_with(".cts") {
        return false;
    }
    if nearest_pkg_type_is_esm(pkg, entry) {
        return true;
    }
    let Some(p) = Parsed::read(entry) else {
        return false;
    };
    !p.imports().is_empty() || p.has_export_declarations() || !p.local_exports().is_empty()
}

/// Node: resolve `query` through the entry's export edges. `None` when the entry exports nothing
/// ES-module style (CommonJS, plain scripts): the caller then scans the package files instead.
fn node_lookup(pkg: &Pkg, idx: &mut Index, query: &str) -> Option<Vec<Hit>> {
    let exports = idx.node_exports(pkg);
    if exports.is_empty() {
        if pkg.entry.as_deref().is_some_and(|e| is_esm_entry(pkg, e)) {
            return Some(vec![]);
        }
        return None;
    }
    let q = query.replace("::", ".");
    let parts: Vec<&str> = q.split('.').filter(|s| !s.is_empty()).collect();
    let Some((&first, mut rest)) = parts.split_first() else {
        return Some(vec![]);
    };
    let Some(mut e) = exports.iter().find(|e| e.public == first).cloned() else {
        return Some(vec![]);
    };
    // `export * as ns from './x'`: the next part is one of x's exports.
    while e.orig == "*" {
        let Some((&next, r)) = rest.split_first() else {
            break;
        };
        let inner = exports_of(&e.file, &mut HashMap::new(), &mut HashSet::new());
        let Some(x) = inner.into_iter().find(|x| x.public == next) else {
            return Some(vec![]);
        };
        (e, rest) = (x, r);
    }
    let text = fs::read_to_string(&e.file).unwrap_or_default();
    let file_bytes = text.len();
    if e.orig == "*" {
        let sig = format!("export * as {} from {}", e.public, pkg.rel(&e.file));
        return Some(vec![Hit {
            file: pkg.rel(&e.file),
            line: 1,
            kind: "namespace",
            name: e.public.clone(),
            sig,
            doc: String::new(),
            members: vec![],
            import_from: None,
            declared_as: None,
            export_line: None,
            file_bytes,
            rank: (0, 0, 0, 0),
        }]);
    }
    let Some(p) = Parsed::of(&e.file, text) else {
        return Some(vec![]);
    };
    let syms = p.symbols();
    // `orig` can be qualified (`Lib.make`, a member of an `export =` namespace).
    let (orig_owner, orig_name) = match e.orig.rsplit_once('.') {
        Some((o, n)) => (o.rsplit('.').next(), n),
        None => (None, e.orig.as_str()),
    };
    let (name, owner) = match rest {
        [] => (orig_name, orig_owner),
        _ => name_owner(orig_name, rest),
    };
    let public = std::iter::once(e.public.as_str())
        .chain(rest.iter().copied())
        .collect::<Vec<_>>()
        .join(".");
    let local = p.local_exports();
    let hits = named(&syms, name, owner)
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            let mut h = make_hit(pkg, &e.file, &syms, s, file_bytes, (0, 0, 0, i));
            if e.public != e.orig {
                h.declared_as = Some(std::mem::replace(&mut h.name, public.clone()));
            }
            if s.parent.is_none() && p.export_kind(s).is_none() {
                // The statement the export edge came from (a barrel may rename it again: `export
                // { Selected as Public }` still points at `export { C as Selected }`), else the one
                // exporting it under the looked-up name, else any that exports it.
                let by = |public: bool| {
                    local
                        .iter()
                        .find(|(l, pb, _)| *l == s.name && (!public || *pb == e.public))
                };
                h.export_line = e
                    .line
                    .or_else(|| by(true).or_else(|| by(false)).map(|x| x.2));
            }
            h
        })
        .collect();
    Some(hits)
}

/// Top-level module name of a Python package (`numpy` for `.../site-packages/numpy`).
fn py_top(pkg: &Pkg) -> Option<String> {
    let d = pkg.dir.as_ref()?;
    let n = if d.is_dir() {
        d.file_name()?
    } else {
        d.file_stem()?
    };
    Some(n.to_string_lossy().into())
}

/// Walk parent package segments of `module` across `sites` following Python's real `importlib`
/// rules:
/// - The first regular package (`<part>/__init__.pyi|.py`) locks `__path__` to that single dir
///   (shadowed regular packages in later sites cannot supply children).
/// - A non-package `.py`/`.pyi` file before any regular package rejects all submodules (`None`).
/// - PEP 420 namespace directories (no `__init__`) accumulate across search dirs.
fn py_parent_search_dirs(sites: &[PathBuf], module: &str) -> Option<Vec<PathBuf>> {
    let parts: Vec<&str> = module.split('.').collect();
    let mut dirs: Vec<PathBuf> = sites.to_vec();
    for part in &parts[..parts.len().saturating_sub(1)] {
        let mut next_ns = Vec::new();
        let mut locked: Option<PathBuf> = None;
        for d in &dirs {
            let pkg_dir = d.join(part);
            if pkg_dir.join("__init__.pyi").is_file() || pkg_dir.join("__init__.py").is_file() {
                locked = Some(pkg_dir);
                break;
            }
            if d.join(format!("{part}.pyi")).is_file() || d.join(format!("{part}.py")).is_file() {
                return None;
            }
            if pkg_dir.is_dir() {
                next_ns.push(pkg_dir);
            }
        }
        dirs = match locked {
            Some(p) => vec![p],
            None if !next_ns.is_empty() => next_ns,
            None => return None,
        };
    }
    Some(dirs)
}

/// `a.b` → `<dir>/b/__init__.pyi|.py` or `<dir>/b.pyi|.py` within the exact parent `__path__`.
fn py_module_file(sites: &[PathBuf], module: &str) -> Option<PathBuf> {
    let leaf = module.rsplit('.').next()?;
    let dirs = py_parent_search_dirs(sites, module)?;
    let names = [
        format!("{leaf}/__init__.pyi"),
        format!("{leaf}/__init__.py"),
        format!("{leaf}.pyi"),
        format!("{leaf}.py"),
    ];
    dirs.iter()
        .find_map(|d| names.iter().map(|f| d.join(f)).find(|p| p.is_file()))
}

/// Does `module` resolve as a Python submodule (file, regular package, or namespace package dir)
/// respecting shadowed regular packages and non-package parent files?
fn py_is_submodule(sites: &[PathBuf], module: &str) -> bool {
    if py_module_file(sites, module).is_some() {
        return true;
    }
    let Some(leaf) = module.rsplit('.').next() else {
        return false;
    };
    py_parent_search_dirs(sites, module)
        .is_some_and(|dirs| dirs.iter().any(|d| d.join(leaf).is_dir()))
}

/// Follow `from x import a as name` (and `from x import *`) from `module` to the module that
/// defines `name`, across packages: (defining file, name there). `seen` holds (module, name)
/// pairs, so re-export cycles end.
fn py_follow(
    sites: &[PathBuf],
    module: &str,
    name: &str,
    idx: &mut Index,
    seen: &mut HashSet<(String, String)>,
) -> Option<(PathBuf, String)> {
    if seen.len() > 64 || !seen.insert((module.to_string(), name.to_string())) {
        return None;
    }
    let file = py_module_file(sites, module)?;
    let (names, imports) = idx.head(&file)?.clone();
    if names.iter().any(|n| n == name) {
        return Some((file, name.to_string()));
    }
    let is_pkg = file.file_stem().is_some_and(|s| s == "__init__");
    let mut base: Vec<&str> = module.split('.').collect();
    if !is_pkg {
        base.pop();
    }
    let abs = |m: &str| -> Option<String> {
        let dots = m.chars().take_while(|c| *c == '.').count();
        if dots == 0 {
            return Some(m.to_string());
        }
        let mut parts = base[..base.len().checked_sub(dots - 1)?].to_vec();
        if dots < m.len() {
            parts.push(&m[dots..]);
        }
        Some(parts.join("."))
    };
    for imp in &imports {
        if let Some((orig, _)) = imp.names.iter().find(|(_, l)| l == name) {
            let Some(m) = abs(&imp.module) else { continue };
            if let Some(r) = py_follow(sites, &m, orig, idx, seen) {
                return Some(r);
            }
        }
    }
    for imp in imports.iter().filter(|i| i.wildcard) {
        let Some(m) = abs(&imp.module) else { continue };
        if let Some(r) = py_follow(sites, &m, name, idx, seen) {
            return Some(r);
        }
    }
    None
}

/// Python: resolve `query` from the module the caller named (or the package) by following its
/// imports. `None` when the chain doesn't resolve (namespaces built at runtime).
fn py_lookup(pkg: &Pkg, idx: &mut Index, query: &str, hint: Option<&str>) -> Option<Vec<Hit>> {
    let top = py_top(pkg)?;
    let module = match hint {
        Some(h) => format!("{top}.{}", h.replace('/', ".")),
        None => top.clone(),
    };
    let q = query.replace("::", ".");
    let parts: Vec<&str> = q.split('.').filter(|s| !s.is_empty()).collect();
    let (&first, rest) = parts.split_first()?;
    let (file, orig) = py_follow(&pkg.sites, &module, first, idx, &mut HashSet::new())?;
    let text = fs::read_to_string(&file).ok()?;
    let file_bytes = text.len();
    let p = Parsed::new(Lang::Py, text)?;
    let syms = p.symbols();
    let (name, owner) = name_owner(&orig, rest);
    let mut found = named(&syms, name, owner);
    // Stubs alias in place (`max = amax`): show the definition it names, in the same file.
    if let [s] = found[..] {
        let target = s.sig.strip_prefix(&format!("{name} = ")).unwrap_or("");
        if s.kind == "var"
            && !target.is_empty()
            && target.chars().all(|c| c.is_alphanumeric() || c == '_')
        {
            let defs = named(&syms, target, owner);
            if !defs.is_empty() {
                found = defs;
            }
        }
    }
    // `from <module> import X`, or a shorter path when a parent package (of the same top-level
    // package, under the same name) imports it too.
    let rel = pkg.rel(&file);
    let same_top = rel
        .split('/')
        .next()
        .map(|t| t.trim_end_matches(".pyi").trim_end_matches(".py"))
        == Some(top.as_str());
    let import_from = match (same_top && orig == first && rest.is_empty())
        .then(|| py_import_path(pkg, idx, &rel, &orig))
        .flatten()
    {
        Some(short) if short.len() < module.len() => short,
        _ => module.clone(),
    };
    let public = parts.join(".");
    let hits = found
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            let mut h = make_hit(pkg, &file, &syms, s, file_bytes, (0, 0, 0, i));
            h.import_from = Some(import_from.clone());
            if h.name != public {
                h.declared_as = Some(std::mem::replace(&mut h.name, public.clone()));
            }
            h
        })
        .collect();
    Some(hits)
}

/// Scan only the exact module file for an explicit dotted Python lookup (e.g. `alpha.real`),
/// returning empty if that module does not exist or does not define `query`.
fn py_scan_module(pkg: &Pkg, query: &str, hint: &str) -> Vec<Hit> {
    let Some(top) = py_top(pkg) else {
        return vec![];
    };
    let module = format!("{top}.{}", hint.replace('/', "."));
    let Some(file) = py_module_file(&pkg.sites, &module) else {
        return vec![];
    };
    let Ok(text) = fs::read_to_string(&file) else {
        return vec![];
    };
    let file_bytes = text.len();
    let Some(p) = Parsed::new(Lang::Py, text) else {
        return vec![];
    };
    let q = query.replace("::", ".");
    let parts: Vec<&str> = q.split('.').filter(|s| !s.is_empty()).collect();
    let Some(&name) = parts.last() else {
        return vec![];
    };
    let owner = (parts.len() > 1).then(|| parts[parts.len() - 2]);
    let syms = p.symbols();
    named(&syms, name, owner)
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            let mut h = make_hit(pkg, &file, &syms, s, file_bytes, (0, 0, 0, i));
            h.import_from = Some(module.clone());
            h
        })
        .collect()
}

/// Text scan for `query` over the package files (Rust, CommonJS, and Python names the import
/// chain can't reach). `strict` keeps it to the `hint` module. `at`: Rust, the module path of the
/// query's first part (`["dir"]` for `create`, `["linked_list"]` for `Entry.or_insert`); only
/// that module's item, or that type's members, count.
fn scan(
    pkg: &Pkg,
    idx: &mut Index,
    query: &str,
    hint: Option<&str>,
    strict: bool,
    at: Option<&[String]>,
) -> Vec<Hit> {
    let q = query.replace("::", ".");
    let mut decl = String::new();
    let mut parts: Vec<&str> = q.split('.').filter(|s| !s.is_empty()).collect();
    let public = parts.join(".");
    let first = parts.first().copied().unwrap_or("");
    // The entry's hint for a bare name, or for a Rust `Owner.member`'s owner; a Rust `pub use
    // builder::ConnectorBuilder as HttpsConnectorBuilder;` also gives the declared name to look for.
    let ty = match parts.len() {
        1 => Some(0),
        2 if pkg.eco == "rust" => Some(0),
        _ => None,
    };
    let prefer: Option<String> = match (hint, ty) {
        (Some(h), _) => Some(h.to_string()),
        (None, Some(t)) => entry_hint(pkg, idx, parts[t]).and_then(|(h, d)| {
            let renamed = d != parts[t];
            decl = d;
            (parts.len() == 1 || renamed).then_some(h)
        }),
        (None, None) => None,
    };
    if let (Some(t), false) = (ty, decl.is_empty()) {
        parts[t] = &decl;
    }
    // An alias's declared name is only looked for where the alias leads: elsewhere it may be an
    // unrelated item (the caller still tries other crates under the public name). An alias's
    // member (`Error.new` → `Report.new`) is looked for there first, then in the whole crate,
    // as `Report.new` is: its `impl` may sit in another file (eyre's `src/error.rs`).
    let strict = strict || (parts.len() == 1 && parts.join(".") != public);
    let Some(&name) = parts.last() else {
        return vec![];
    };
    let owner = (parts.len() > 1).then(|| parts[parts.len() - 2]);
    // An alias's member (`LinkedEntry.or_insert`, `LinkedEntry` being `linked_list::Entry`) is
    // only that type's, never a same-named `hash_map::Entry`'s.
    // ponytail: only where that module declares the type; one reached through a glob (`pub use
    // crate::imp::*;`) isn't followed, so it keeps the unfiltered scan.
    let entry = pkg.entry.as_deref().filter(|_| pkg.eco == "rust");
    let aliased = (parts.len() == 2 && !decl.is_empty() && decl != first)
        .then(|| crate::lang::rs_use_path(entry?, first))
        .flatten()
        .filter(|p| match (p.split_last(), entry) {
            (Some((ty, m)), Some(e)) => crate::lang::rs_binds(e, m, ty, true),
            _ => false,
        });
    let at = at.or(aliased.as_deref().and_then(|p| Some(p.split_last()?.1)));
    // The crate path the hit must have: (module, name) of a top-level item, or its owner type's.
    let want: Option<(String, Vec<String>)> = at.filter(|_| parts.len() <= 2).map(|m| {
        let ty = [m, &[owner.unwrap_or(name).to_string()]].concat();
        (m.join("::"), ty)
    });
    let root = pkg.dir.clone().unwrap_or_default();
    let mut hits = Vec::new();
    let depth_of = |f: &Path| {
        f.strip_prefix(&root)
            .map(|r| r.components().count())
            .unwrap_or(0)
    };
    // Phase 0 scans only the preferred module path. Phase 1 (if that found nothing) scans the rest
    // shallow-first and stops after the shallowest depth that produced a hit: big packages
    // (torch, transformers) have thousands of files and most lookups end in the first few.
    let mut scanned: HashSet<PathBuf> = HashSet::new();
    let node = pkg.eco == "node";
    for phase in 0..2 {
        if !hits.is_empty() || (phase == 1 && strict) {
            break;
        }
        let list = match (phase, &prefer) {
            (0, Some(h)) => idx.subtree(pkg, h),
            (0, None) => continue,
            _ => idx.files(pkg),
        };
        // Files come shallow-first; scan one depth level at a time (in parallel) and stop at the
        // first level with a hit. Node lists are already in entry-first BFS order: one group.
        let mut groups: Vec<(usize, Vec<(usize, PathBuf)>)> = Vec::new();
        for (order, f) in list.into_iter().enumerate() {
            if !scanned.insert(f.clone()) {
                continue;
            }
            let d = if node { 0 } else { depth_of(&f) };
            if groups.last().map(|g| g.0) != Some(d) {
                groups.push((d, Vec::new()));
            }
            groups.last_mut().unwrap().1.push((order, f));
        }
        for (_, group) in groups {
            let matched = par_map(&group, |(order, f)| {
                let text = fs::read_to_string(f).ok()?;
                let keep = has_word(&text, name) && (node || looks_defined(&text, name));
                keep.then(|| (*order, f.clone(), text))
            });
            for (order, f, text) in matched {
                let file_bytes = text.len();
                let Some(p) = Parsed::of(&f, text) else {
                    continue;
                };
                let syms = p.symbols();
                for s in syms.iter().filter(|s| s.name == name) {
                    if owner.is_some_and(|o| s.parent.as_deref() != Some(o)) {
                        continue;
                    }
                    if let (Some((m, ty)), Some(e)) = (&want, entry) {
                        // An owner path that ends where nothing declares the type went through a
                        // glob: not known to be another type.
                        let other = |p: &Vec<String>| {
                            p != ty
                                && p.split_last()
                                    .is_some_and(|(t, m)| crate::lang::rs_binds(e, m, t, true))
                        };
                        let fits = match owner {
                            None => s.parent.is_none() && s.module == *m,
                            Some(_) => {
                                let paths = crate::lang::rs_owner_paths(e, &f, s);
                                !paths.is_empty() && !paths.iter().all(other)
                            }
                        };
                        if !fits {
                            continue;
                        }
                    }
                    // `impl Visitor for X { type Value = ..; }` is `X::Value`, never the crate's `Value`.
                    if owner.is_none()
                        && pkg.eco == "rust"
                        && s.kind == "type"
                        && s.parent.is_some()
                    {
                        continue;
                    }
                    let rel_in = slash(f.strip_prefix(&root).unwrap_or(&f));
                    let hint_rank = match &prefer {
                        Some(h) if rel_in.starts_with(h.as_str()) => 0,
                        Some(_) => 1,
                        None => 0,
                    };
                    let kind_rank = match s.kind {
                        "class" | "interface" | "function" | "fn" | "struct" | "trait" | "enum"
                        | "type" | "macro" => 0,
                        "method" | "property" if owner.is_some() => 0,
                        "const" | "var" | "static" | "namespace" | "mod" => 1,
                        _ => 2,
                    };
                    let private_path = rel_in
                        .split('/')
                        .any(|seg| seg.starts_with('_') && !seg.starts_with("__init__"))
                        as u8;
                    let rank = (
                        hint_rank + (!s.exported) as u8,
                        kind_rank,
                        private_path,
                        order,
                    );
                    hits.push(make_hit(pkg, &f, &syms, s, file_bytes, rank));
                }
            }
            if !hits.is_empty() && !node {
                break;
            }
        }
    }
    hits.sort_by_key(|h| h.rank);
    // Found under its declared name: shown under the public one, as an alias.
    let declared = parts.join(".");
    if declared != public {
        for h in hits.iter_mut().filter(|h| h.name == declared) {
            h.declared_as = Some(std::mem::replace(&mut h.name, public.clone()));
        }
    }
    if pkg.eco == "python" {
        if let Some(h) = hits.first_mut() {
            // `from X import Owner`, never `import member`: only top-level names are importable.
            let top = h.name.split('.').next().unwrap_or(&h.name).to_string();
            h.import_from = py_import_path(pkg, idx, &h.file, &top);
        }
    }
    hits
}

/// Where the package entry says `name` comes from: its own file if defined there, else the module
/// it is imported from (`from numpy._core.multiarray import array` → `_core/multiarray`). Also
/// the name it is declared under there: Rust's `pub use builder::ConnectorBuilder as
/// HttpsConnectorBuilder;` → (`src/builder`, `ConnectorBuilder`); else `name` itself.
fn entry_hint(pkg: &Pkg, idx: &mut Index, name: &str) -> Option<(String, String)> {
    let entry = pkg.entry.as_ref()?;
    let dir = pkg.dir.as_ref()?;
    if pkg.eco == "node" {
        return None; // node files are already searched in entry-first BFS order
    }
    let top = dir.file_name()?.to_string_lossy().to_string();
    let rel_entry = slash(entry.strip_prefix(dir).unwrap_or(entry));
    let (names, imports) = idx.head(entry)?;
    if names.iter().any(|n| n == name) {
        return Some((rel_entry, name.to_string()));
    }
    if pkg.eco == "rust" {
        // `pub use crate::value::Value;` in `src/lib.rs` → `src/value` (`value.rs`, `value/..`);
        // `a::b::T` with `b` inline in `a.rs` → `src/a`. A root `use` wins over a same-named
        // item of an inline `mod` (`pub use crate::anyhow as format_err;` beside
        // `mod __private { pub fn format_err() }`), which is only the fallback.
        let nested = names.iter().any(|n| n.ends_with(&format!("::{name}")));
        let Some(mut path) = crate::lang::rs_use_path(entry, name) else {
            return nested.then(|| (rel_entry, name.to_string()));
        };
        let decl = path.pop()?;
        // `pub use crate::Direction as EdgeDirection;`: declared in the entry itself, or a
        // `#[macro_export] macro_rules!` of any file (`crate::anyhow` from `src/macros.rs`).
        if path.is_empty() {
            if names.contains(&decl) {
                return Some((rel_entry, decl));
            }
            let file = idx.files(pkg).into_iter().find(|f| {
                f.extension().is_some_and(|e| e == "rs")
                    && fs::read_to_string(f)
                        .is_ok_and(|t| t.contains("macro_rules!") && has_word(&t, &decl))
                    && Parsed::read(f).is_some_and(|p| {
                        p.symbols()
                            .iter()
                            .any(|s| s.kind == "macro" && s.name == decl)
                    })
            })?;
            return Some((slash(file.strip_prefix(dir).unwrap_or(&file)), decl));
        }
        // No module file of the crate holds it (`std::io::Error as IoError`): keep the name.
        return rs_mod_rel(pkg, &path).map(|h| (h, decl));
    }
    for imp in imports {
        if !imp.names.iter().any(|(_, l)| l == name) {
            continue;
        }
        let m = imp.module.as_str();
        let path = if let Some(rel) = m.strip_prefix('.') {
            rel.trim_start_matches('.').replace('.', "/")
        } else if let Some(rest) = m.strip_prefix(&format!("{top}.")) {
            rest.replace('.', "/")
        } else {
            continue;
        };
        return Some((path, name.to_string()));
    }
    None
}

/// Rust: where module `path` of `pkg`'s crate lives, relative to the package dir: the deepest
/// `src/a/b` (`b.rs`, `b/..`) on its way that exists (`a::b` inline in `a.rs` → `src/a`).
fn rs_mod_rel(pkg: &Pkg, mut path: &[String]) -> Option<String> {
    let (entry, dir) = (pkg.entry.as_ref()?, pkg.dir.as_ref()?);
    let src = entry.strip_prefix(dir).unwrap_or(entry).parent()?;
    while let Some((_, up)) = path.split_last() {
        let rel = src.join(path.join("/"));
        if dir.join(&rel).is_dir() || dir.join(&rel).with_extension("rs").is_file() {
            return Some(slash(&rel));
        }
        path = up;
    }
    None
}

/// Shortest `from X import name` path: the highest package whose `__init__` imports `name`.
fn py_import_path(pkg: &Pkg, idx: &mut Index, rel_file: &str, name: &str) -> Option<String> {
    let dir = pkg.dir.as_ref()?;
    let base = dir.parent()?;
    let mut module_parts: Vec<String> = rel_file
        .trim_end_matches(".pyi")
        .trim_end_matches(".py")
        .split('/')
        .map(String::from)
        .collect();
    if module_parts.last().is_some_and(|l| l == "__init__") {
        module_parts.pop();
    }
    for i in 1..module_parts.len() {
        let pkg_dir = base.join(module_parts[..i].join("/"));
        for init in ["__init__.pyi", "__init__.py"] {
            let f = pkg_dir.join(init);
            if !f.is_file() {
                continue;
            }
            let text = idx.text(&f);
            let in_all = text.contains("__all__") && text.contains(&format!("\"{name}\""));
            let mentioned = has_word(text, name);
            let imported = mentioned
                && idx.head(&f).is_some_and(|(_, imps)| {
                    imps.iter()
                        .any(|imp| imp.names.iter().any(|(_, l)| l == name))
                });
            if in_all || imported {
                return Some(module_parts[..i].join("."));
            }
        }
    }
    Some(module_parts.join("."))
}

/// For Rust crates that re-export another crate's API (`pub use clap_builder::*`). Python
/// re-exports, across packages too, are followed by `py_follow`.
fn lookup_reexport(start: &Path, pkg: &Pkg, query: &str) -> Option<(Pkg, Vec<Hit>)> {
    if pkg.eco != "rust" {
        return None;
    }
    let own = pkg.name.replace('-', "_");
    // The name the entry binds is the query's first part: `MMap` of `MMap.len`.
    let public = query.replace("::", ".");
    let (name, rest) = public.split_once('.').unwrap_or((&public, ""));
    let declared = |orig: &str| match rest {
        "" => orig.to_string(),
        _ => format!("{orig}.{rest}"),
    };
    // Only the crate's public API: a plain `pub use`, never a private or `pub(crate)` one.
    for (local, path) in crate::lang::rs_pub_uses(pkg.entry.as_ref()?) {
        let segs: Vec<&str> = path.trim_start_matches("::").split("::").collect();
        let top = segs[0];
        if top.is_empty()
            || matches!(top, "crate" | "self" | "super" | "std" | "core" | "alloc")
            || norm_py(top) == norm_py(&own)
        {
            continue;
        }
        // `pub use memmap2::Mmap as MMap;`: looked up in `memmap2` as `Mmap`; `pub use
        // gix_fs::dir::create as create_dir;` as `create` of module `dir`.
        // ponytail: a glob's module path isn't kept (`other::prelude::*` searches all of `other`).
        let (sub, orig) = match &segs[1..] {
            [sub @ .., orig] if local == name => (sub, *orig),
            _ if local == "*" => (&[][..], name),
            _ => continue,
        };
        let Some(other) = resolve(start, top, Some(pkg.eco)) else {
            continue;
        };
        if other.name == pkg.name {
            continue;
        }
        let (declared, mut hits) = match (sub, other.entry.as_ref()) {
            ([], _) | (_, None) => {
                let d = declared(orig);
                let hits = lookup(&other, &mut Index::new(), &d, None);
                (d, hits)
            }
            (sub, Some(e)) => {
                // Only in that module (its `use`s followed): never `symlink::create`.
                let path = sub.iter().chain([&orig]).map(|s| s.to_string()).collect();
                let path = crate::lang::rs_follow(e, path, false);
                let Some((n, m)) = path.split_last() else {
                    continue;
                };
                let d = declared(n);
                let hits = if m.is_empty() {
                    lookup(&other, &mut Index::new(), &d, None)
                } else {
                    // No module file: an inline `mod` of the root file.
                    let h = rs_mod_rel(&other, m).unwrap_or_else(|| {
                        let dir = other.dir.as_deref().unwrap_or(Path::new(""));
                        slash(e.strip_prefix(dir).unwrap_or(e))
                    });
                    let hits = scan(&other, &mut Index::new(), &d, Some(&h), true, Some(m));
                    // ponytail: a glob `use` in that module isn't followed: the whole crate then.
                    if hits.is_empty() && crate::lang::rs_binds(e, m, "*", false) {
                        lookup(&other, &mut Index::new(), &d, None)
                    } else {
                        hits
                    }
                };
                (d, hits)
            }
        };
        if hits.is_empty() {
            continue;
        }
        if declared != public {
            for h in hits.iter_mut().filter(|h| h.name == declared) {
                let d = std::mem::replace(&mut h.name, public.clone());
                h.declared_as.get_or_insert(d);
            }
        }
        return Some((other, hits));
    }
    None
}

/// All names worth suggesting from the package.
fn candidates(pkg: &Pkg, idx: &mut Index) -> Vec<String> {
    if pkg.eco == "node" {
        let mut out: Vec<String> = idx
            .node_exports(pkg)
            .into_iter()
            .map(|e| e.public)
            .filter(|n| n != "default")
            .collect();
        if !out.is_empty() || pkg.entry.as_deref().is_some_and(|e| is_esm_entry(pkg, e)) {
            out.sort();
            out.dedup();
            return out;
        }
    }
    let files = idx.files(pkg);
    let files = &files[..files.len().min(4000)];
    let eco = pkg.eco;
    let mut out: Vec<String> = par_map(files, |f| {
        let text = fs::read_to_string(f).ok()?;
        if eco == "node" {
            let p = Parsed::of(f, text)?;
            return Some(
                p.symbols()
                    .into_iter()
                    .filter(|s| s.parent.is_none())
                    .map(|s| s.name)
                    .collect::<Vec<_>>(),
            );
        }
        let mut names = Vec::new();
        for l in text.lines() {
            let indented = l.starts_with(' ') || l.starts_with('\t');
            let t = l.trim_start();
            let rest = if eco == "python" {
                if indented {
                    continue;
                }
                t.strip_prefix("class ")
                    .or_else(|| t.strip_prefix("def "))
                    .or_else(|| t.strip_prefix("async def "))
            } else {
                let t = t.strip_prefix("pub ").unwrap_or("");
                [
                    "fn ",
                    "struct ",
                    "enum ",
                    "trait ",
                    "type ",
                    "const ",
                    "static ",
                    "mod ",
                    "union ",
                    "async fn ",
                ]
                .iter()
                .find_map(|k| t.strip_prefix(k))
            };
            if let Some(r) = rest {
                let id: String = r
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !id.is_empty() && !id.starts_with('_') {
                    names.push(id);
                }
            }
        }
        Some(names)
    })
    .into_iter()
    .flatten()
    .collect();
    out.sort();
    out.dedup();
    out
}

/// Map over `items` on a few threads, preserving order. Package scans are dominated by file opens
/// (thousands for torch/transformers), which parallelise well.
fn par_map<I: Sync, T: Send>(items: &[I], f: impl Fn(&I) -> Option<T> + Sync) -> Vec<T> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8);
    if items.len() < 32 || threads < 2 {
        return items.iter().filter_map(&f).collect();
    }
    let chunk = items.len().div_ceil(threads);
    let f = &f;
    std::thread::scope(|s| {
        let hs: Vec<_> = items
            .chunks(chunk)
            .map(|c| s.spawn(move || c.iter().filter_map(f).collect::<Vec<T>>()))
            .collect();
        hs.into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    })
}

/// Up to `n` lines in the installed source that mention `name` (docs, deprecation notes,
/// strings), and the total size of the files those lines came from.
fn mentions(pkg: &Pkg, idx: &mut Index, name: &str, n: usize) -> (Vec<String>, usize) {
    let files = idx.files(pkg);
    let per_file = par_map(&files, |f| {
        let text = fs::read_to_string(f).ok()?;
        if !has_word(&text, name) {
            return None;
        }
        let lines: Vec<String> = text
            .lines()
            .enumerate()
            .filter(|(_, l)| has_word(l, name))
            .take(n)
            .map(|(i, l)| format!("{}:{}  {}", pkg.rel(f), i + 1, clip(l.trim(), 160)))
            .collect();
        Some((lines, text.len()))
    });
    let (mut out, mut bytes) = (Vec::new(), 0);
    for (lines, len) in per_file {
        if out.len() >= n {
            break;
        }
        bytes += len;
        out.extend(lines.into_iter().take(n - out.len()));
    }
    (out, bytes)
}

fn camel_tokens(s: &str) -> Vec<String> {
    let cs: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    for i in 0..cs.len() {
        let c = cs[i];
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        let prev = if i > 0 { Some(cs[i - 1]) } else { None };
        let next = cs.get(i + 1).copied();
        let boundary = c.is_uppercase()
            && prev.is_some_and(|p| {
                p.is_lowercase()
                    || p.is_ascii_digit()
                    || (p.is_uppercase() && next.is_some_and(|n| n.is_lowercase()))
            });
        if boundary && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out.into_iter().map(|t| t.to_lowercase()).collect()
}

pub fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for j in 0..b.len() {
            let cur = row[j + 1];
            row[j + 1] = (prev + (ca != b[j]) as usize).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// Closest names to `query`: edit distance, substring and camelCase-token overlap, plus a boost
/// for names that appear next to the query in the package's own text (e.g. "X was renamed to Y").
pub fn suggest(query: &str, cands: &[String], context: &[String], n: usize) -> Vec<String> {
    let q = query.to_lowercase();
    let qt = camel_tokens(query);
    let mut scored: Vec<(i64, &String)> = Vec::new();
    for c in cands {
        let cl = c.to_lowercase();
        if cl == q && c != query {
            scored.push((-10_000, c));
            continue;
        }
        if c == query {
            continue;
        }
        let d = levenshtein(&q, &cl) as i64;
        let shared = camel_tokens(c)
            .iter()
            .filter(|t| t.len() > 1 && qt.contains(t))
            .count() as i64;
        let sub = (q.len() >= 3 && cl.contains(&q)) || (cl.len() >= 3 && q.contains(&cl));
        let near = context.iter().any(|m| has_word(m, c));
        if !(d <= (q.len() as i64 / 3).max(2) || shared > 0 || sub || near) {
            continue;
        }
        let score = d * 10 - shared * 25 - if sub { 30 } else { 0 } - if near { 200 } else { 0 }
            + c.len() as i64 / 4;
        scored.push((score, c));
    }
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(b.1)));
    scored.into_iter().take(n).map(|(_, c)| c.clone()).collect()
}

// ---------------------------------------------------------------- `tc dep`

pub struct DepArgs {
    pub package: Option<String>,
    pub symbol: Option<String>,
    pub imports: Option<PathBuf>,
    /// `--bin <cmd>`: CLI only, it runs `<cmd> --help`.
    pub bin: Option<String>,
    pub flag: Option<String>,
}

pub fn run(cwd: &Path, a: &DepArgs) -> Result<Out> {
    // Checked before anything runs: a stray `flag` must never become the program to execute.
    if a.flag.is_some() && a.bin.is_none() {
        bail!("a flag needs --bin: tc dep --bin <cmd> [flag]")
    }
    if let Some(cmd) = &a.bin {
        if a.package.is_some() || a.symbol.is_some() || a.imports.is_some() {
            bail!("--bin takes <cmd> [flag] and can't be combined with a package, symbol or --imports")
        }
        return bin_help(cwd, cmd, a.flag.as_deref());
    }
    if let Some(f) = &a.imports {
        return imports_sheet(cwd, &cwd.join(f));
    }
    let Some(spec) = a.package.as_deref() else {
        bail!("usage: tc dep <package> [symbol] | --imports <file> | --bin <cmd> [flag]")
    };
    let Some(mut pkg) = resolve(cwd, spec, None) else {
        bail!(
            "'{spec}' is not installed here: no node_modules/{spec}, no site-packages entry in {}, no Cargo.lock entry. Searched from {}",
            if py_sites(cwd).is_empty() { "a venv (none found)".to_string() } else { "the venv".to_string() },
            cwd.display()
        )
    };
    let mut idx = Index::new();
    let loc = pkg
        .dir
        .as_ref()
        .map(|d| d.display().to_string())
        .unwrap_or("(source not on disk)".into());
    let mut text = format!("{}  {loc}\n", pkg.label());
    // `tc dep torch.nn Linear` or `tc dep torch.nn`: resolve the requested submodule.
    let hint = match pkg.eco {
        "python" => spec
            .split_once('.')
            .map(|(_, m)| m.replace('.', "/"))
            .unwrap_or_default(),
        _ => String::new(),
    };
    if pkg.eco == "python" && !hint.is_empty() {
        let top = py_top(&pkg).unwrap_or_default();
        let full_mod = format!("{top}.{}", hint.replace('/', "."));
        if let Some(f) = py_module_file(&pkg.sites, &full_mod) {
            pkg.entry = Some(f);
        } else if a.symbol.is_none() {
            bail!(
                "module '{spec}' not found in installed {} {}",
                pkg.name,
                pkg.version
            );
        }
    }
    let Some(sym) = a.symbol.as_deref() else {
        return Ok(listing(pkg, &mut idx, text));
    };
    if pkg.eco == "python" && !sym.contains('.') {
        let top = py_top(&pkg).unwrap_or_default();
        let cand_mod = if hint.is_empty() {
            format!("{top}.{sym}")
        } else {
            format!("{top}.{}.{sym}", hint.replace('/', "."))
        };
        if py_is_submodule(&pkg.sites, &cand_mod) {
            let module = format!("{spec}.{sym}");
            text.push_str(&format!(
                "`{sym}` is a submodule ({module}). Look inside it: tc dep {module} <Symbol>\n"
            ));
            return Ok(Out {
                json: json!({"package": pkg, "symbol": sym, "submodule": module}),
                text,
                baseline_bytes: 0,
                dep_lookups: 1,
                ok: true,
            });
        }
    }
    let mut hits = lookup(&pkg, &mut idx, sym, Some(&hint));
    let mut via = None;
    if hits.is_empty() {
        if let Some((other, h)) = lookup_reexport(cwd, &pkg, sym) {
            via = Some(other);
            hits = h;
        }
    } else if pkg.eco == "python" {
        // Followed into another installed package (`from other_pkg import X` in the entry).
        let top = hits[0].file.split('/').next().unwrap_or("");
        let top = top.trim_end_matches(".pyi").trim_end_matches(".py");
        if py_top(&pkg).is_some_and(|t| t != top) {
            via = py_pkg(cwd, top);
        }
    }
    if let Some(o) = &via {
        text.push_str(&format!("re-exported from {}\n", o.label()));
    }
    if hits.is_empty() {
        let name = sym
            .replace("::", ".")
            .rsplit('.')
            .next()
            .unwrap_or(sym)
            .to_string();
        let (ment, bytes) = mentions(&pkg, &mut idx, &name, 4);
        let sugg = suggest(&name, &candidates(&pkg, &mut idx), &ment, 6);
        let scope = if hint.is_empty() {
            String::new()
        } else {
            format!(" (module {spec})")
        };
        text.push_str(&format!(
            "NOT FOUND: `{sym}` is not {} in the installed {} {}{scope}.\n",
            if pkg.eco == "node" {
                "exported"
            } else {
                "defined"
            },
            pkg.name,
            pkg.version
        ));
        if !sugg.is_empty() {
            text.push_str(&format!("did you mean: {}\n", sugg.join(", ")));
        }
        if !ment.is_empty() {
            text.push_str("mentions in installed source:\n");
            for m in &ment {
                text.push_str(&format!("  {m}\n"));
            }
        }
        return Ok(Out {
            json: json!({"package": pkg, "symbol": sym, "found": false, "suggestions": sugg, "mentions": ment}),
            text,
            baseline_bytes: bytes,
            dep_lookups: 1,
            ok: false,
        });
    }
    let shown = &hits[0];
    text.push_str(&render_hit(shown, true));
    if hits.len() > 1 {
        let others: Vec<String> = hits[1..]
            .iter()
            .take(4)
            .map(|h| format!("{}:{} ({})", h.file, h.line, h.kind))
            .collect();
        text.push_str(&format!("also defined: {}\n", others.join(", ")));
    }
    Ok(Out {
        json: json!({"package": pkg, "via": via, "symbol": sym, "found": true, "hits": hits.iter().take(5).collect::<Vec<_>>()}),
        text,
        // Only the shown answer's file: the other locations are just `file:line`.
        baseline_bytes: shown.file_bytes,
        dep_lookups: 1,
        ok: true,
    })
}

pub fn render_hit(h: &Hit, full: bool) -> String {
    let mut s = String::new();
    if let Some(f) = &h.import_from {
        // Only a top-level name is importable: `Owner.member` → `import Owner`.
        s.push_str(&format!(
            "from {f} import {}\n",
            h.name.split('.').next().unwrap_or(&h.name)
        ));
    }
    let exp = h
        .export_line
        .map(|l| format!(", export :{l}"))
        .unwrap_or_default();
    s.push_str(&format!("{}  [{}:{}{exp}]\n", h.sig, h.file, h.line));
    if let Some(d) = &h.declared_as {
        s.push_str(&format!(
            "  (exported as `{}`, declared as `{d}`)\n",
            h.name
        ));
    }
    if !h.doc.is_empty() {
        let doc: Vec<&str> = h.doc.lines().collect();
        let max = if full { 12 } else { 1 };
        for l in doc.iter().take(max) {
            s.push_str(&format!("  # {l}\n"));
        }
        if doc.len() > max {
            s.push_str(&format!("  # … {} more doc lines\n", doc.len() - max));
        }
    }
    let mut members = h.members.clone();
    members.sort_by_key(|m| (m.name != "__init__" && m.name != "constructor", m.line));
    for m in members.iter().take(60) {
        let d = m.short_doc();
        let d = if d.is_empty() {
            String::new()
        } else {
            format!("  // {d}")
        };
        // Each member's own line (same file as the header): no follow-up lookup to find it.
        s.push_str(&format!("  [:{}] {}{d}\n", m.line, clip(&m.sig, 400)));
    }
    if members.len() > 60 {
        s.push_str(&format!("  … {} more members\n", members.len() - 60));
    }
    s
}

fn listing(pkg: Pkg, idx: &mut Index, mut text: String) -> Out {
    let mut names: Vec<(String, &'static str)> = Vec::new();
    let mut bytes = 0;
    let entry = pkg.entry.clone();
    if let Some(e) = &entry {
        text.push_str(&format!("entry: {}\n", pkg.rel(e)));
    }
    let exports = if pkg.eco == "node" {
        idx.node_exports(&pkg)
    } else {
        vec![]
    };
    if !exports.is_empty() {
        // Public names only, under their exported (possibly aliased) name.
        let mut files: Vec<&PathBuf> = exports.iter().map(|e| &e.file).collect();
        files.sort();
        files.dedup();
        bytes = files
            .iter()
            .filter_map(|f| fs::metadata(f).ok())
            .map(|m| m.len() as usize)
            .sum();
        names = exports.iter().map(|e| (e.public.clone(), e.kind)).collect();
    } else if pkg.eco == "node" && pkg.entry.as_deref().is_some_and(|e| is_esm_entry(&pkg, e)) {
        // Explicit ES module / `.d.ts` with zero exports: never fall back to scanning internal files.
    } else {
        let files: Vec<PathBuf> = if pkg.eco == "node" {
            idx.files(&pkg)
        } else {
            entry.into_iter().collect()
        };
        for f in &files {
            let Some(p) = Parsed::read(f) else { continue };
            bytes += p.src.len();
            names.extend(
                p.symbols()
                    .into_iter()
                    .filter(|s| s.parent.is_none() && s.exported)
                    .map(|s| (s.name, s.kind)),
            );
            if pkg.eco != "node" {
                for imp in p.imports() {
                    names.extend(
                        imp.names
                            .into_iter()
                            .filter(|(_, l)| !l.starts_with('_'))
                            .map(|(_, l)| (l, "re-export")),
                    );
                }
            }
        }
    }
    let mut seen = HashSet::new();
    names.retain(|(n, _)| seen.insert(n.clone()));
    let mut by_kind: Vec<(&str, Vec<String>)> = Vec::new();
    for (n, k) in &names {
        match by_kind.iter_mut().find(|(kk, _)| kk == k) {
            Some((_, v)) => v.push(n.clone()),
            None => by_kind.push((k, vec![n.clone()])),
        }
    }
    text.push_str(&format!("exports ({}):\n", names.len()));
    for (k, v) in &by_kind {
        let shown: Vec<&String> = v.iter().take(80).collect();
        let more = if v.len() > 80 {
            format!(" … +{}", v.len() - 80)
        } else {
            String::new()
        };
        text.push_str(&format!(
            "  {k}: {}{more}\n",
            shown
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if pkg.eco == "python" {
        if let Some(d) = pkg.dir.as_ref().filter(|d| d.is_dir()) {
            let mut subs: Vec<String> = fs::read_dir(d)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect()
                })
                .unwrap_or_default();
            subs.retain(|s| {
                !s.starts_with('_')
                    && (s.ends_with(".py")
                        || d.join(s).join("__init__.py").is_file()
                        || d.join(s).join("__init__.pyi").is_file())
            });
            let mut subs: Vec<String> = subs
                .into_iter()
                .map(|s| s.trim_end_matches(".py").to_string())
                .collect();
            subs.sort();
            text.push_str(&format!("submodules: {}\n", subs.join(", ")));
        }
    }
    text.push_str("next: tc dep <package> <Symbol> for a signature\n");
    Out {
        json: json!({"package": pkg, "exports": names.iter().map(|(n, k)| json!({"name": n, "kind": k})).collect::<Vec<_>>()}),
        text,
        baseline_bytes: bytes,
        dep_lookups: 1,
        ok: true,
    }
}

// ---------------------------------------------------------------- imports used by a file / symbol

pub enum Origin {
    Internal,
    Std,
    External(String),
}

const PY_STD: &[&str] = &[
    "__future__",
    "abc",
    "argparse",
    "array",
    "ast",
    "asyncio",
    "base64",
    "binascii",
    "bisect",
    "builtins",
    "bz2",
    "calendar",
    "cmath",
    "codecs",
    "collections",
    "colorsys",
    "concurrent",
    "configparser",
    "contextlib",
    "contextvars",
    "copy",
    "csv",
    "ctypes",
    "dataclasses",
    "datetime",
    "decimal",
    "difflib",
    "dis",
    "email",
    "enum",
    "errno",
    "faulthandler",
    "fnmatch",
    "fractions",
    "functools",
    "gc",
    "getpass",
    "gettext",
    "glob",
    "gzip",
    "hashlib",
    "heapq",
    "hmac",
    "html",
    "http",
    "importlib",
    "inspect",
    "io",
    "ipaddress",
    "itertools",
    "json",
    "keyword",
    "linecache",
    "locale",
    "logging",
    "lzma",
    "math",
    "mimetypes",
    "multiprocessing",
    "numbers",
    "operator",
    "os",
    "pathlib",
    "pickle",
    "platform",
    "pprint",
    "queue",
    "random",
    "re",
    "secrets",
    "select",
    "selectors",
    "shlex",
    "shutil",
    "signal",
    "site",
    "socket",
    "socketserver",
    "sqlite3",
    "ssl",
    "stat",
    "statistics",
    "string",
    "struct",
    "subprocess",
    "sys",
    "sysconfig",
    "tarfile",
    "tempfile",
    "textwrap",
    "threading",
    "time",
    "timeit",
    "tkinter",
    "token",
    "tokenize",
    "tomllib",
    "traceback",
    "types",
    "typing",
    "unicodedata",
    "unittest",
    "urllib",
    "uuid",
    "venv",
    "warnings",
    "wave",
    "weakref",
    "webbrowser",
    "winreg",
    "xml",
    "zipfile",
    "zlib",
    "zoneinfo",
    "typing_extensions",
];

const NODE_STD: &[&str] = &[
    "assert",
    "buffer",
    "child_process",
    "cluster",
    "crypto",
    "dgram",
    "dns",
    "events",
    "fs",
    "fs/promises",
    "http",
    "http2",
    "https",
    "module",
    "net",
    "os",
    "path",
    "perf_hooks",
    "process",
    "querystring",
    "readline",
    "stream",
    "string_decoder",
    "timers",
    "tls",
    "tty",
    "url",
    "util",
    "v8",
    "vm",
    "worker_threads",
    "zlib",
    "test",
];

pub fn origin(lang: Lang, module: &str, file: &Path, cwd: &Path) -> Origin {
    match lang {
        Lang::Ts | Lang::Tsx => {
            if module.starts_with('.')
                || module.starts_with('/')
                || module.starts_with("@/")
                || module.starts_with("~/")
                || module.starts_with('#')
            {
                Origin::Internal
            } else if module.starts_with("node:")
                || NODE_STD.contains(&module)
                || NODE_STD.contains(&module.split('/').next().unwrap_or(""))
            {
                Origin::Std
            } else {
                // The whole specifier: `pkg/sub` resolves through its own `exports` entry.
                Origin::External(module.to_string())
            }
        }
        Lang::Py => {
            if module.starts_with('.') {
                return Origin::Internal;
            }
            let top = module.split('.').next().unwrap_or(module);
            let local = file
                .ancestors()
                .skip(1)
                .take_while(|d| d.starts_with(cwd))
                .chain([cwd])
                .any(|d| d.join(format!("{top}.py")).is_file() || d.join(top).is_dir());
            if local {
                Origin::Internal
            } else if PY_STD.contains(&top) {
                Origin::Std
            } else {
                Origin::External(top.to_string())
            }
        }
        Lang::Rs => match module {
            "crate" | "self" | "super" => Origin::Internal,
            "std" | "core" | "alloc" | "proc_macro" | "test" => Origin::Std,
            m => Origin::External(m.to_string()),
        },
        Lang::Java => {
            let first = module.split('.').next().unwrap_or("");
            if matches!(first, "java" | "javax" | "jdk" | "sun") {
                return Origin::Std;
            }
            if java_file(file, module).is_some() {
                return Origin::Internal;
            }
            let own = std::fs::read_to_string(file).ok().and_then(|s| {
                s.lines().find_map(|l| {
                    l.trim()
                        .strip_prefix("package ")
                        .map(|p| p.trim_end_matches(';').trim().to_string())
                })
            });
            let prefix2 = |s: &str| s.split('.').take(2).collect::<Vec<_>>().join(".");
            if own.is_some_and(|o| prefix2(&o) == prefix2(module)) {
                Origin::Internal
            } else {
                Origin::External(module.to_string())
            }
        }
    }
}

/// `tn.x.Foo` imported from `file` → the project file that defines it, using `file`'s own package path.
pub fn java_file(file: &Path, fqcn: &str) -> Option<PathBuf> {
    let src = std::fs::read_to_string(file).ok()?;
    let pkg = src.lines().find_map(|l| {
        l.trim()
            .strip_prefix("package ")
            .map(|p| p.trim_end_matches(';').trim().to_string())
    })?;
    let mut root = file.parent()?.to_path_buf();
    for _ in pkg.split('.') {
        root = root.parent()?.to_path_buf();
    }
    Some(root.join(format!("{}.java", fqcn.replace('.', "/")))).filter(|p| p.is_file())
}

#[derive(Debug, Clone, Serialize)]
pub struct DepUse {
    pub module: String,
    pub package: Option<Pkg>,
    pub status: String,
    pub items: Vec<Hit>,
    pub missing: Vec<String>,
}

/// Third-party imports of `p` that are used in `scope` (byte range), with installed signatures.
pub fn deps_used(
    p: &Parsed,
    file: &Path,
    cwd: &Path,
    scope: Option<(usize, usize)>,
    per_pkg: usize,
) -> (Vec<DepUse>, usize) {
    let text = match scope {
        Some((a, b)) => &p.src[a..b],
        None => &p.src[..],
    };
    let mut out: Vec<DepUse> = Vec::new();
    let mut bytes = 0;
    let mut counted: HashSet<String> = HashSet::new(); // each source file counts once
    let mut cache: HashMap<String, (Option<Pkg>, Option<Index>)> = HashMap::new();
    let start = file.parent().unwrap_or(cwd);
    for imp in p.imports() {
        let Origin::External(spec) = origin(p.lang, &imp.module, file, cwd) else {
            continue;
        };
        // `import 'pkg/styles.css'`: an asset, not an API.
        let asset = split_node_spec(&spec).1.is_some_and(|sub| {
            Path::new(&sub).extension().is_some_and(|e| {
                !["js", "mjs", "cjs", "jsx", "ts", "mts", "cts", "tsx"]
                    .contains(&e.to_string_lossy().as_ref())
            })
        });
        if asset && matches!(p.lang, Lang::Ts | Lang::Tsx) {
            continue;
        }
        let names = used_names(p.lang, &imp, text, scope.is_none());
        if names.is_empty() && scope.is_some() {
            continue;
        }
        let (pkg, idx) = cache.entry(spec.clone()).or_insert_with(|| {
            let pkg = if p.lang == Lang::Java {
                None
            } else {
                resolve(start, &spec, Some(eco_of(p.lang)))
            };
            let idx = pkg.as_ref().map(|_| Index::new());
            (pkg, idx)
        });
        let hint = match p.lang {
            Lang::Py => imp.module.split('.').skip(1).collect::<Vec<_>>().join("/"),
            _ => String::new(),
        };
        let du = match out.iter_mut().position(|d| d.module == spec) {
            Some(i) => &mut out[i],
            None => {
                let status = match (&*pkg, p.lang) {
                    // Installed, but no `exports` entry or file matches this subpath.
                    (Some(k), _) if k.eco == "node" && k.entry.is_none() => format!(
                        "installed, but `{spec}` resolves to no file (no matching `exports` entry)"
                    ),
                    (Some(_), _) => "installed".to_string(),
                    (None, Lang::Java) => {
                        "not resolved (Maven/Gradle resolution is not in v0.1)".to_string()
                    }
                    (None, _) => "NOT INSTALLED (no node_modules / venv / Cargo.lock entry found)"
                        .to_string(),
                };
                out.push(DepUse {
                    module: spec.clone(),
                    package: pkg.clone(),
                    status,
                    items: vec![],
                    missing: vec![],
                });
                out.last_mut().unwrap()
            }
        };
        let (Some(pkg), Some(idx)) = (pkg.as_ref(), idx.as_mut()) else {
            continue;
        };
        if pkg.eco == "node" && pkg.entry.is_none() {
            continue; // nothing to look names up in; the status line says so
        }
        let pdir = pkg.dir.clone().filter(|d| d.is_dir());
        let is_submodule = |n: &str| {
            pdir.as_ref().is_some_and(|d| {
                d.join(n).is_dir()
                    || d.join(format!("{n}.py")).is_file()
                    || d.join(format!("{n}.pyi")).is_file()
            })
        };
        for n in names.into_iter().filter(|n| !is_submodule(n)).take(per_pkg) {
            if du
                .items
                .iter()
                .any(|h| h.name == n || h.declared_as.as_deref() == Some(&n))
                || du.missing.contains(&n)
            {
                continue;
            }
            let mut hits = lookup(pkg, idx, &n, Some(&hint));
            if hits.is_empty() {
                hits = lookup_reexport(start, pkg, &n)
                    .map(|(_, h)| h)
                    .unwrap_or_default();
            }
            match hits.into_iter().next() {
                Some(mut h) => {
                    if counted.insert(h.file.clone()) {
                        bytes += h.file_bytes;
                    }
                    h.members.clear();
                    du.items.push(h);
                }
                None => du.missing.push(n),
            }
        }
    }
    (out, bytes)
}

pub fn eco_of(lang: Lang) -> &'static str {
    match lang {
        Lang::Ts | Lang::Tsx => "node",
        Lang::Py => "python",
        Lang::Rs => "rust",
        Lang::Java => "java",
    }
}

/// `alias.x` / `alias::x` accesses in `text` → the names `x` (`np.linalg.norm` → `norm`).
fn member_uses(text: &str, alias: &str, sep: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let pat = format!("{alias}{sep}");
    let ident = |t: &str| -> String {
        t.chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect()
    };
    for (i, _) in text.match_indices(&pat) {
        if !word_at(text, i, alias.len()) {
            continue;
        }
        let rest = &text[i + pat.len()..];
        // `np.linalg.norm`: the submodule hop isn't a symbol; take the last name in the chain.
        let mut chain = vec![ident(rest)];
        let mut tail = &rest[chain[0].len()..];
        while let Some(t) = tail.strip_prefix(sep) {
            let next = ident(t);
            if next.is_empty() {
                break;
            }
            tail = &t[next.len()..];
            chain.push(next);
        }
        let pick = if chain.len() > 1 && chain[0].chars().next().is_some_and(|c| c.is_lowercase()) {
            chain[1].clone()
        } else {
            chain[0].clone()
        };
        if !pick.is_empty() && !out.contains(&pick) {
            out.push(pick);
        }
    }
    out
}

/// Imported names (and `alias.member` accesses) that appear in `text`. A default import
/// (`import Foo from 'pkg'`) asks for the package's `default` export, or `default.member`.
fn used_names(lang: Lang, imp: &Import, text: &str, all: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (orig, local) in &imp.names {
        if orig == "*" {
            continue;
        }
        if orig == "default" {
            let members = member_uses(text, local, ".");
            if !members.is_empty() {
                out.extend(members.into_iter().map(|m| format!("default.{m}")));
            } else if all || has_word(text, local) {
                out.push("default".into());
            }
            continue;
        }
        if all || has_word(text, local) {
            out.push(orig.clone());
        }
    }
    if let Some(a) = &imp.alias {
        let sep = if lang == Lang::Rs { "::" } else { "." };
        for m in member_uses(text, a, sep) {
            if !out.contains(&m) {
                out.push(m);
            }
        }
    }
    out.dedup();
    out
}

/// Rendered lines, and for each line the (file, bytes) of the installed file it came from.
pub fn render_deps(deps: &[DepUse]) -> (Vec<String>, Vec<Option<(String, usize)>>) {
    let mut lines = Vec::new();
    let mut files = Vec::new();
    for d in deps {
        match &d.package {
            Some(p) => lines.push(format!("{}  {}", p.label(), d.status)),
            None => lines.push(format!("{}  {}", d.module, d.status)),
        }
        files.push(None);
        for h in &d.items {
            files.push(Some((h.file.clone(), h.file_bytes)));
            let doc = short_doc(&h.doc);
            let doc = if doc.is_empty() {
                String::new()
            } else {
                format!("  // {doc}")
            };
            let imp = h
                .import_from
                .as_ref()
                .map(|f| format!("  (from {f})"))
                .unwrap_or_default();
            lines.push(format!(
                "  {}  [{}:{}]{imp}{doc}",
                sig_clip(&h.sig, 260),
                h.file,
                h.line
            ));
        }
        if !d.missing.is_empty() && d.package.is_some() {
            lines.push(format!("  ! not defined in installed version: {}  (run `tc dep {} <name>` for suggestions)", d.missing.join(", "), d.module));
            files.push(None);
        }
    }
    (lines, files)
}

fn imports_sheet(cwd: &Path, file: &Path) -> Result<Out> {
    let Some(p) = Parsed::read(file) else {
        bail!("cannot read/parse {}", file.display())
    };
    let (deps, bytes) = deps_used(&p, file, cwd, None, 25);
    let mut text = format!(
        "third-party API used by {}\n",
        slash(file.strip_prefix(cwd).unwrap_or(file))
    );
    if deps.is_empty() {
        text.push_str("(no third-party imports)\n");
    }
    for l in render_deps(&deps).0 {
        text.push_str(&l);
        text.push('\n');
    }
    let lookups = deps
        .iter()
        .map(|d| d.items.len() + d.missing.len())
        .sum::<usize>()
        .max(deps.len());
    Ok(Out {
        json: json!({"file": slash(file), "deps": deps}),
        text,
        baseline_bytes: bytes,
        dep_lookups: lookups,
        ok: true,
    })
}

// ---------------------------------------------------------------- `tc dep --bin`

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            if it.peek() == Some(&'[') {
                it.next();
                for d in it.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        if c == '\x08' {
            out.pop(); // man-style overstrike
            continue;
        }
        out.push(c);
    }
    out
}

/// Drain `r` to EOF, keeping at most `cap` bytes (the rest is read and dropped so the writer
/// never blocks on a full pipe). Returns (kept bytes, whether anything was dropped).
fn drain(mut r: impl std::io::Read, cap: usize) -> (Vec<u8>, bool) {
    let (mut kept, mut cut) = (Vec::new(), false);
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = cap.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..n.min(room)]);
                cut |= n > room;
            }
        }
    }
    (kept, cut)
}

/// The process tree of a spawned child, so a timeout kills grandchildren too. Windows: spawn
/// suspended, attach to a Job Object with KILL_ON_JOB_CLOSE, then resume so zero descendants can
/// escape. Unix: the child leads its own process group.
#[cfg(windows)]
mod tree {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command};

    const CREATE_SUSPENDED: u32 = 0x0000_0004;
    const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;
    const THREAD_SUSPEND_RESUME: u32 = 0x0002;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: u32 = 9;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;

    #[repr(C)]
    struct ThreadEntry32 {
        dw_size: u32,
        cnt_usage: u32,
        th32_thread_id: u32,
        th32_owner_process_id: u32,
        tp_base_pri: i32,
        tp_delta_pri: i32,
        dw_flags: u32,
    }

    #[repr(C)]
    struct JobObjectBasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    struct JobObjectExtendedLimitInformation {
        basic_limit_information: JobObjectBasicLimitInformation,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(attrs: *mut c_void, name: *const u16) -> *mut c_void;
        fn SetInformationJobObject(
            job: *mut c_void,
            class: u32,
            info: *const c_void,
            len: u32,
        ) -> i32;
        fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
        fn TerminateJobObject(job: *mut c_void, code: u32) -> i32;
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> *mut c_void;
        fn Thread32First(snapshot: *mut c_void, entry: *mut ThreadEntry32) -> i32;
        fn Thread32Next(snapshot: *mut c_void, entry: *mut ThreadEntry32) -> i32;
        fn OpenThread(access: u32, inherit: i32, tid: u32) -> *mut c_void;
        fn ResumeThread(thread: *mut c_void) -> u32;
        fn CloseHandle(h: *mut c_void) -> i32;
    }

    pub fn prepare(c: &mut Command) {
        c.creation_flags(CREATE_SUSPENDED);
    }

    fn resume_process(pid: u32) {
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snap.is_null() || snap == (-1isize as *mut c_void) {
                return;
            }
            let mut te = ThreadEntry32 {
                dw_size: std::mem::size_of::<ThreadEntry32>() as u32,
                cnt_usage: 0,
                th32_thread_id: 0,
                th32_owner_process_id: 0,
                tp_base_pri: 0,
                tp_delta_pri: 0,
                dw_flags: 0,
            };
            if Thread32First(snap, &mut te) != 0 {
                loop {
                    if te.th32_owner_process_id == pid {
                        let th = OpenThread(THREAD_SUSPEND_RESUME, 0, te.th32_thread_id);
                        if !th.is_null() {
                            ResumeThread(th);
                            CloseHandle(th);
                        }
                    }
                    if Thread32Next(snap, &mut te) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snap);
        }
    }

    pub struct Tree(*mut c_void);

    impl Tree {
        pub fn new(child: &mut Child) -> anyhow::Result<Tree> {
            let job = unsafe {
                let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
                if job.is_null() {
                    std::ptr::null_mut()
                } else {
                    let mut info: JobObjectExtendedLimitInformation = std::mem::zeroed();
                    info.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                    let ok_info = SetInformationJobObject(
                        job,
                        JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                        (&info as *const JobObjectExtendedLimitInformation).cast(),
                        std::mem::size_of::<JobObjectExtendedLimitInformation>() as u32,
                    );
                    if ok_info == 0 || AssignProcessToJobObject(job, child.as_raw_handle()) == 0 {
                        CloseHandle(job);
                        std::ptr::null_mut()
                    } else {
                        job
                    }
                }
            };
            if job.is_null() {
                // Never resume an uncontained child: terminate while still suspended.
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("could not attach child process to a Windows Job Object");
            }
            resume_process(child.id());
            Ok(Tree(job))
        }
        pub fn kill(&self, child: &mut Child) {
            if !self.0.is_null() {
                unsafe {
                    TerminateJobObject(self.0, 1);
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CloseHandle(self.0) };
            }
        }
    }
}

#[cfg(unix)]
mod tree {
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    pub fn prepare(c: &mut Command) {
        c.process_group(0);
    }

    pub struct Tree(i32);

    impl Tree {
        pub fn new(child: &mut Child) -> anyhow::Result<Tree> {
            Ok(Tree(child.id() as i32))
        }
        pub fn kill(&self, child: &mut Child) {
            unsafe { kill(-self.0, 9) }; // SIGKILL the whole group
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Run `<cmd> --help` from `cwd` and return its help text (stdout, or stderr when stdout is
/// empty). One deadline covers the exit and both pipes; on expiry the whole tree is killed. Each
/// stream keeps at most 512 KiB. A nonzero exit is an error.
pub fn run_help(cwd: &Path, cmd: &str, timeout: Duration) -> Result<String> {
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    const CAP: usize = 512 << 10;
    let Some(exe) = which(cwd, cmd) else {
        bail!(
            "`{cmd}` not found on PATH (or relative to {})",
            cwd.display()
        )
    };
    let mut c = Command::new(&exe);
    c.arg("--help")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("PAGER", "cat")
        .env("GIT_PAGER", "cat");
    tree::prepare(&mut c);
    let mut child = c.spawn()?;
    let tree = tree::Tree::new(&mut child)?;
    let deadline = Instant::now() + timeout;
    let (tx, rx) = mpsc::channel();
    let pipes: [Option<Box<dyn std::io::Read + Send>>; 2] = [
        child.stdout.take().map(|p| Box::new(p) as _),
        child.stderr.take().map(|p| Box::new(p) as _),
    ];
    for (i, pipe) in pipes.into_iter().enumerate() {
        let tx = tx.clone();
        // Never joined: a grandchild holding the pipe open must not hold us past the deadline.
        std::thread::spawn(move || {
            let got = pipe.map(|p| drain(p, CAP)).unwrap_or_default();
            let _ = tx.send((i, got));
        });
    }
    drop(tx);
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if Instant::now() >= deadline {
            tree.kill(&mut child);
            bail!("`{cmd} --help` timed out after {timeout:?}; killed it and its child processes");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut got: [(Vec<u8>, bool); 2] = Default::default();
    for _ in 0..2 {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((i, g)) => got[i] = g,
            Err(_) => {
                tree.kill(&mut child);
                bail!("`{cmd} --help` exited but a child process kept its output open past {timeout:?}; killed them");
            }
        }
    }
    let [(out, out_cut), (err, err_cut)] = got;
    if !status.success() {
        let why = String::from_utf8_lossy(&err);
        let why = why.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        bail!(
            "`{cmd} --help` failed ({status}){}",
            if why.is_empty() {
                String::new()
            } else {
                format!(": {}", clip(why.trim(), 200))
            }
        );
    }
    let (out, cut) = if out.iter().all(|b| b.is_ascii_whitespace()) {
        (err, err_cut)
    } else {
        (out, out_cut)
    };
    let mut text = strip_ansi(&String::from_utf8_lossy(&out));
    if cut {
        text.push_str("\n… help output cut at 512 KiB\n");
    }
    Ok(text)
}

fn bin_help(cwd: &Path, cmd: &str, flag: Option<&str>) -> Result<Out> {
    let help = run_help(cwd, cmd, Duration::from_secs(3))?;
    let full = help.len();
    let Some(flag) = flag else {
        return Ok(Out {
            json: json!({"cmd": cmd, "help": help}),
            text: help,
            baseline_bytes: full,
            dep_lookups: 1,
            ok: true,
        });
    };
    let lines: Vec<&str> = help.lines().collect();
    let indent = |l: &str| l.len() - l.trim_start().len();
    let matches_tok = |l: &str| {
        l.split(|c: char| c.is_whitespace() || c == ',' || c == '|' || c == '[' || c == ']')
            .map(|t| t.split(['=', '<']).next().unwrap_or(""))
            .filter(|t| !t.is_empty())
            // `--flag` may sit after its short form; a subcommand must lead its line.
            .take(if flag.starts_with('-') { 6 } else { 1 })
            .any(|t| t == flag)
    };
    let mut blocks: Vec<String> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if !matches_tok(l) {
            continue;
        }
        let ind = indent(l);
        let mut block = vec![l.trim_end().to_string()];
        for next in &lines[i + 1..] {
            if next.trim().is_empty() || indent(next) <= ind {
                break;
            }
            block.push(next.trim_end().to_string());
        }
        blocks.push(block.join("\n"));
        if blocks.len() >= 3 {
            break;
        }
    }
    if blocks.is_empty() && !flag.starts_with('-') {
        // Subcommand listed inline (`npm --help`: "access, adduser, audit, ..."): show those lines.
        blocks = lines
            .iter()
            .filter(|l| has_word(l, flag))
            .take(3)
            .map(|l| l.trim_end().to_string())
            .collect();
    }
    let ok = !blocks.is_empty();
    let text = if ok {
        format!("{cmd} --help → {flag}\n{}\n", blocks.join("\n"))
    } else {
        let mut opts: Vec<String> = help
            .split(|c: char| c.is_whitespace() || c == ',' || c == '[' || c == ']')
            .map(|t| t.split(['=', '<']).next().unwrap_or("").to_string())
            .filter(|t| t.starts_with('-') && t.len() > 1)
            .collect();
        opts.sort();
        opts.dedup();
        let sugg = suggest(
            flag.trim_start_matches('-'),
            &opts
                .iter()
                .map(|o| o.trim_start_matches('-').to_string())
                .collect::<Vec<_>>(),
            &[],
            5,
        );
        let sugg: Vec<String> = sugg
            .into_iter()
            .filter_map(|s| {
                opts.iter()
                    .find(|o| o.trim_start_matches('-') == s)
                    .cloned()
            })
            .collect();
        format!(
            "NOT FOUND: `{flag}` does not appear in `{cmd} --help`.\ndid you mean: {}\n",
            sugg.join(", ")
        )
    };
    Ok(Out {
        json: json!({"cmd": cmd, "flag": flag, "found": ok, "text": text}),
        text,
        baseline_bytes: full,
        dep_lookups: 1,
        ok,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggest_prefers_documented_rename_and_tokens() {
        let cands: Vec<String> = [
            "MCPServer",
            "MCPError",
            "Server",
            "ClientSession",
            "FastAPI",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let ctx = vec!["`FastMCP` is now `mcp.server.mcpserver.MCPServer`".to_string()];
        assert_eq!(suggest("FastMCP", &cands, &ctx, 3)[0], "MCPServer");
        assert!(suggest("FastMCP", &cands, &[], 5).contains(&"MCPServer".to_string()));
        assert_eq!(
            suggest("getcell", &["getCell".into(), "getLine".into()], &[], 2)[0],
            "getCell"
        );
    }

    #[test]
    fn tokens_and_distance() {
        assert_eq!(camel_tokens("MCPServer"), vec!["mcp", "server"]);
        assert_eq!(camel_tokens("FastMCP"), vec!["fast", "mcp"]);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }

    #[test]
    fn node_spec_split() {
        assert_eq!(
            split_node_spec("@xterm/xterm"),
            ("@xterm/xterm".into(), None)
        );
        assert_eq!(
            split_node_spec("react-dom/client"),
            ("react-dom".into(), Some("client".into()))
        );
    }

    #[test]
    fn f04_help_capture_is_capped_but_drained() {
        let big = vec![b'x'; 600 << 10];
        let (kept, cut) = drain(&big[..], 512 << 10);
        assert_eq!((kept.len(), cut), (512 << 10, true));
        let (kept, cut) = drain(&b"usage"[..], 512 << 10);
        assert_eq!((kept.as_slice(), cut), (&b"usage"[..], false));
    }

    #[test]
    fn f09_lock_edges_parse() {
        let lock = parse_lock(
            "version = 4

[[package]]
name = \"app\"
version = \"0.1.0\"
dependencies = [
 \"a 1.0.0 (registry+x)\",
 \"b\",
]

[[package]]
name = \"a\"
version = \"1.0.0\"
source = \"registry+x\"

[metadata]
name = \"not a package\"
",
        );
        assert_eq!(lock.len(), 2);
        assert_eq!(lock[0].deps, ["a 1.0.0 (registry+x)", "b"]);
        assert!(lock[0].local && !lock[1].local);
        assert_eq!(lock[1].name, "a");
    }

    #[test]
    fn defined_heuristic() {
        assert!(looks_defined("class MCPServer(Generic[T]):", "MCPServer"));
        assert!(looks_defined("pub fn new() -> Self", "new"));
        assert!(!looks_defined("x = MCPServer()", "MCPServer"));
    }

    #[test]
    fn py_tag_and_free_threaded_version_detection() {
        assert_eq!(parse_py_tag("3.13t"), ("3.13".into(), true, None));
        assert_eq!(
            parse_py_tag("PythonCore/3.13T-ARM64"),
            ("3.13".into(), true, Some("arm64".into()))
        );
        assert_eq!(
            parse_py_tag("3.11-32"),
            ("3.11".into(), false, Some("32".into()))
        );
        assert_eq!(
            parse_py_tag("PythonCoreé/3.11"),
            ("PythonCoreé/3.11".into(), false, None)
        );
        assert_eq!(
            detect_py_exe_version(Path::new("/usr/bin/python3.14t")).as_deref(),
            Some("3.14t")
        );
        let base = std::env::temp_dir().join(format!("tc-pyver-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let mac_fw = base.join("PythonT.framework/Versions/3.14/bin");
        fs::create_dir_all(base.join("PythonT.framework/Versions/3.14/lib/python3.14t")).unwrap();
        fs::create_dir_all(&mac_fw).unwrap();
        assert_eq!(detect_py_version(&mac_fw), "3.14t");

        let win_ft = base.join("pythoncore-3.13t-64");
        fs::create_dir_all(&win_ft).unwrap();
        fs::write(win_ft.join("python313t.dll"), b"").unwrap();
        assert_eq!(detect_py_version(&win_ft), "3.13t");

        // Dual install dir (standard + optional free-threaded DLLs): default prefers non-t.
        let win_dual = base.join("Python313");
        fs::create_dir_all(&win_dual).unwrap();
        fs::write(win_dual.join("python313.dll"), b"").unwrap();
        fs::write(win_dual.join("python313t.dll"), b"").unwrap();
        assert_eq!(detect_py_version(&win_dual), "3.13");

        // `lib_sites_for` isolates standard vs free-threaded builds under the same Unix prefix.
        let unix_pfx = base.join("unix_pfx");
        let s_std = unix_pfx.join("lib/python3.14/site-packages");
        let s_ft = unix_pfx.join("lib/python3.14t/site-packages");
        fs::create_dir_all(&s_std).unwrap();
        fs::create_dir_all(&s_ft).unwrap();
        assert_eq!(lib_sites_for(&unix_pfx, Some("3.14")), vec![s_std]);
        assert_eq!(lib_sites_for(&unix_pfx, Some("3.14t")), vec![s_ft]);
        let _ = fs::remove_dir_all(&base);
    }
}
