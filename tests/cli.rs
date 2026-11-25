//! End-to-end tests: run the real `tc` binary against a throwaway fixture repo
//! (TS + Python + Rust + Java, a fake node_modules package, a fake .venv).
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// `tc` with the user's home and stats ledger swapped for a scratch dir, so no test can touch
/// the real `~/.turbocharger/stats.jsonl`. Tests that record stats set their own `TC_STATS_FILE`.
fn tc() -> Command {
    let home = std::env::temp_dir().join(format!("tc-test-home-{}", std::process::id()));
    let mut c = Command::new(env!("CARGO_BIN_EXE_tc"));
    c.env("USERPROFILE", &home)
        .env("HOME", &home)
        .env_remove("VIRTUAL_ENV")
        .env_remove("PYTHON_MANAGER_DEFAULT")
        .env_remove("PY_PYTHON")
        .env_remove("PY_PYTHON3")
        .env("TC_STATS_FILE", home.join("stats.jsonl"));
    c
}

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

/// A temp fixture dir, deleted when the test ends.
struct Fixture(PathBuf);

impl std::ops::Deref for Fixture {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<std::ffi::OsStr> for Fixture {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.0.as_os_str()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fresh(name: &str) -> Fixture {
    // Tests run in parallel: every fixture gets its own directory.
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("tc-test-{name}-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    Fixture(d)
}

/// Run `tc --no-stats --cwd <root> <args>` → (stdout, stderr, exit code).
fn run(root: &Path, args: &[&str]) -> (String, String, i32) {
    let o = tc()
        .arg("--no-stats")
        .arg("--cwd")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&o.stdout).into(),
        String::from_utf8_lossy(&o.stderr).into(),
        o.status.code().unwrap_or(-1),
    )
}

fn ok(root: &Path, args: &[&str]) -> String {
    let (out, err, code) = run(root, args);
    assert_eq!(
        code, 0,
        "tc {args:?} failed\nstdout:\n{out}\nstderr:\n{err}"
    );
    out
}

fn project() -> Fixture {
    let r = fresh("proj");
    write(&r, ".gitignore", "ignored/\n");
    write(
        &r,
        "package.json",
        r#"{"name":"proj","scripts":{"dev":"vite"},"dependencies":{"@xterm/xterm":"^9"}}"#,
    );
    // TypeScript
    write(&r, "src/util.ts", "/** Doubles a number. */\nexport function helper(x: number): number {\n  return x * 2;\n}\nfunction hidden() {}\n");
    write(
        &r,
        "src/main.ts",
        "import { helper } from \"./util\";\nimport { Terminal } from \"@xterm/xterm\";\n\n/** Runs it. */\nexport function run(t: Terminal): number {\n  t.write(\"hi\");\n  return helper(2);\n}\n",
    );
    write(
        &r,
        "src/other.ts",
        "import { run } from \"./main\";\nconst n = run(null as any);\n",
    );
    write(&r, "ignored/skip.ts", "run(1);\n");
    // node_modules package with a barrel re-export and an aliased re-export
    write(
        &r,
        "node_modules/@xterm/xterm/package.json",
        r#"{"name":"@xterm/xterm","version":"9.9.9","types":"typings/index.d.ts"}"#,
    );
    write(
        &r,
        "node_modules/@xterm/xterm/typings/index.d.ts",
        "export * from './terminal';\nexport { IBuf as IBufferLine } from './buf';\n",
    );
    write(
        &r,
        "node_modules/@xterm/xterm/typings/terminal.d.ts",
        "/** The terminal. */\nexport declare class Terminal {\n  /** Writes data. */\n  write(data: string): void;\n}\n",
    );
    write(&r, "node_modules/@xterm/xterm/typings/buf.d.ts", "/** A buffer line. */\nexport interface IBuf {\n  /** Wrapped? */\n  readonly isWrapped: boolean;\n}\n");
    // Python app + venv (Windows layout)
    write(
        &r,
        "app/server.py",
        "\"\"\"App.\"\"\"\nfrom fastlib.core import Engine\nimport fastlib\n\n\ndef compute(n: int) -> int:\n    \"\"\"Compute things.\"\"\"\n    e = Engine(n)\n    return helper_py(n) + fastlib.speed(n)\n\n\ndef helper_py(n):\n    return n\n",
    );
    write(
        &r,
        "caller.py",
        "from app.server import compute\nprint(compute(3))\n",
    );
    write(
        &r,
        ".venv/pyvenv.cfg",
        "home = C:\\nowhere\ninclude-system-site-packages = false\nversion = 3.11.0\n",
    );
    write(
        &r,
        ".venv/Lib/site-packages/fastlib/__init__.py",
        "from .core import Engine, speed\n",
    );
    write(
        &r,
        ".venv/Lib/site-packages/fastlib/core.py",
        "class Engine:\n    \"\"\"Fast engine.\"\"\"\n\n    def __init__(self, n: int): ...\n\n    def run(self) -> int: ...\n\n\ndef speed(n: int) -> float:\n    \"\"\"How fast.\"\"\"\n    return 1.0\n\n# FastEngine was renamed to Engine in 2.0\n",
    );
    write(
        &r,
        ".venv/Lib/site-packages/fastlib-2.0.1.dist-info/METADATA",
        "Metadata-Version: 2.1\nName: fastlib\nVersion: 2.0.1\n\nlong description\n",
    );
    // Rust
    write(
        &r,
        "rs/Cargo.toml",
        "[package]\nname = \"rs\"\nversion = \"0.1.0\"\n",
    );
    write(
        &r,
        "rs/src/lib.rs",
        "/// Adds one.\npub fn add_one(x: i32) -> i32 {\n    inner(x) + 1\n}\n\nfn inner(x: i32) -> i32 {\n    x\n}\n\npub struct Point {\n    pub x: i32,\n}\n\nimpl Point {\n    pub fn norm(&self) -> i32 {\n        add_one(self.x)\n    }\n}\n",
    );
    // Java
    write(
        &r,
        "java/src/main/java/com/acme/Svc.java",
        "package com.acme;\n\nimport com.acme.repo.Repo;\n\npublic class Svc {\n    private final Repo repo;\n\n    /** Finds one. */\n    @GetMapping(\"/find\")\n    public int find(int id) {\n        return repo.load(id);\n    }\n}\n",
    );
    write(&r, "java/src/main/java/com/acme/repo/Repo.java", "package com.acme.repo;\n\npublic interface Repo {\n    /** Loads. */\n    int load(int id);\n}\n");
    write(&r, "java/src/main/java/com/acme/Use.java", "package com.acme;\n\npublic class Use {\n    int go(Svc s) {\n        return s.find(1);\n    }\n}\n");
    r
}

#[test]
fn zoom_l0_overview() {
    let r = project();
    let out = ok(&r, &["zoom", "."]);
    for want in [
        "languages:",
        "TypeScript",
        "Python",
        "Rust",
        "Java",
        "manifests:",
        "package.json",
        "dev = vite",
        "@xterm/xterm",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
    assert!(!out.contains("node_modules/"), "{out}");
}

#[test]
fn zoom_l1_skeleton_has_no_bodies() {
    let r = project();
    let out = ok(&r, &["zoom", "src"]);
    assert!(out.contains("src/util.ts"), "{out}");
    assert!(
        out.contains("function helper(x: number): number") && out.contains("Doubles a number."),
        "{out}"
    );
    assert!(!out.contains("return x * 2"), "body leaked:\n{out}");
    assert!(!out.contains("hidden"), "private symbol listed:\n{out}");
}

#[test]
fn zoom_l15_file_table_resolves_imports() {
    let r = project();
    let out = ok(&r, &["zoom", "src/main.ts"]);
    assert!(out.contains("@xterm/xterm 9.9.9"), "{out}");
    assert!(out.contains("internal src/util.ts"), "{out}");
    assert!(
        out.contains("4-8 export function run(t: Terminal): number  // Runs it."),
        "{out}"
    );
}

#[test]
fn zoom_l2_typescript() {
    let r = project();
    let out = ok(&r, &["zoom", "src/main.ts:run"]);
    assert!(
        out.contains("## TARGET") && out.contains("return helper(2);"),
        "{out}"
    );
    assert!(
        out.contains("export function helper(x: number): number  [src/util.ts:2]"),
        "{out}"
    );
    assert!(
        out.contains("@xterm/xterm 9.9.9") && out.contains("export declare class Terminal"),
        "{out}"
    );
    assert!(out.contains("src/other.ts:2"), "{out}");
    assert!(!out.contains("ignored/"), ".gitignore not honoured:\n{out}");
}

#[test]
fn zoom_l2_python() {
    let r = project();
    let out = ok(&r, &["zoom", "app/server.py:compute"]);
    assert!(
        out.contains("def helper_py(n)  [app/server.py:12]"),
        "{out}"
    );
    assert!(
        out.contains("fastlib 2.0.1")
            && out.contains("class Engine")
            && out.contains("def speed(n: int) -> float"),
        "{out}"
    );
    assert!(out.contains("caller.py:2"), "{out}");
}

#[test]
fn zoom_l2_rust_and_java() {
    let r = project();
    let out = ok(&r, &["zoom", "rs/src/lib.rs:Point.norm"]);
    assert!(
        out.contains("pub fn add_one(x: i32) -> i32  [rs/src/lib.rs:2]  // Adds one."),
        "{out}"
    );
    let out = ok(&r, &["zoom", "java/src/main/java/com/acme/Svc.java:find"]);
    assert!(out.contains("@GetMapping(\"/find\")"), "{out}");
    assert!(
        out.contains("int load(int id);  [java/src/main/java/com/acme/repo/Repo.java:5]"),
        "{out}"
    );
    assert!(
        out.contains("java/src/main/java/com/acme/Use.java:5"),
        "{out}"
    );
}

#[test]
fn zoom_budget_truncates_never_fails() {
    let r = project();
    let out = ok(&r, &["zoom", "src/main.ts:run", "--budget", "30"]);
    assert!(out.contains("cut by --budget 30"), "{out}");
    assert!(out.contains("## TARGET"), "{out}");
}

#[test]
fn zoom_symbol_miss_suggests() {
    let r = project();
    let (_, err, code) = run(&r, &["zoom", "src/util.ts:helpr"]);
    assert_eq!(code, 2);
    assert!(err.contains("did you mean: helper"), "{err}");
}

#[test]
fn dep_node_follows_barrels_and_aliases() {
    let r = project();
    let out = ok(&r, &["dep", "@xterm/xterm", "Terminal"]);
    assert!(
        out.contains("@xterm/xterm 9.9.9 (node)") && out.contains("The terminal."),
        "{out}"
    );
    assert!(
        out.contains("write(data: string): void  // Writes data."),
        "{out}"
    );
    let out = ok(&r, &["dep", "@xterm/xterm", "IBufferLine"]);
    assert!(out.contains("readonly isWrapped: boolean"), "{out}");
    let out = ok(&r, &["dep", "@xterm/xterm"]);
    assert!(out.contains("Terminal") && out.contains("IBuf"), "{out}");
}

#[test]
fn dep_python_venv_windows_layout() {
    let r = project();
    let out = ok(&r, &["dep", "fastlib", "Engine"]);
    assert!(out.contains("fastlib 2.0.1 (python)"), "{out}");
    assert!(out.contains("from fastlib import Engine"), "{out}");
    assert!(
        out.contains("class Engine") && out.contains("def __init__(self, n: int)"),
        "{out}"
    );
}

#[test]
fn dep_python_venv_unix_layout() {
    let r = fresh("unix");
    write(
        &r,
        ".venv/pyvenv.cfg",
        "home = /usr/bin\ninclude-system-site-packages = false\n",
    );
    write(
        &r,
        ".venv/lib/python3.12/site-packages/unixlib/__init__.pyi",
        "def thing(a: str) -> bool:\n    \"\"\"Is it a thing.\"\"\"\n",
    );
    write(
        &r,
        ".venv/lib/python3.12/site-packages/unixlib-0.3.0.dist-info/METADATA",
        "Name: unixlib\nVersion: 0.3.0\n",
    );
    let out = ok(&r, &["dep", "unixlib", "thing"]);
    assert!(
        out.contains("unixlib 0.3.0")
            && out.contains("def thing(a: str) -> bool")
            && out.contains("Is it a thing."),
        "{out}"
    );
}

#[test]
fn dep_fuzzy_miss_suggests_and_mentions() {
    let r = project();
    let (out, _, code) = run(&r, &["dep", "fastlib", "FastEngine"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("NOT FOUND"), "{out}");
    assert!(out.contains("did you mean: Engine"), "{out}");
    assert!(out.contains("renamed to Engine"), "{out}");
}

#[test]
fn dep_imports_cheat_sheet() {
    let r = project();
    let out = ok(&r, &["dep", "--imports", "app/server.py"]);
    assert!(
        out.contains("fastlib 2.0.1")
            && out.contains("class Engine")
            && out.contains("def speed(n: int) -> float"),
        "{out}"
    );
    let out = ok(&r, &["dep", "--imports", "src/main.ts"]);
    assert!(
        out.contains("@xterm/xterm 9.9.9") && out.contains("Terminal"),
        "{out}"
    );
}

#[test]
fn dep_bin_extracts_flag() {
    let r = project();
    let exe = env!("CARGO_BIN_EXE_tc");
    let out = ok(&r, &["dep", "--bin", exe, "--flag", "--no-stats"]);
    assert!(
        out.contains("--no-stats") && out.contains("record"),
        "{out}"
    );
    let (out, _, code) = run(&r, &["dep", "--bin", exe, "--flag", "--no-statz"]);
    assert_eq!(code, 1);
    assert!(out.contains("did you mean: --no-stats"), "{out}");
}

#[test]
fn json_output_is_valid() {
    let r = project();
    let v: Value = serde_json::from_str(&ok(&r, &["--json", "zoom", "src/main.ts:run"])).unwrap();
    assert_eq!(v["level"], "L2");
    assert_eq!(v["callees"][0]["file"], "src/util.ts");
}

#[test]
fn mcp_jsonrpc_roundtrip() {
    let r = project();
    let mut child = tc()
        .args(["mcp", "--no-stats"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut send = |v: Value| writeln!(stdin, "{v}").unwrap();
    send(
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}),
    );
    let init: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["serverInfo"]["name"], "turbocharger");
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    send(json!({"jsonrpc":"2.0","id":"two","method":"tools/list"}));
    let list: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(list["id"], "two", "notification must not get a reply");
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["turbo_zoom", "turbo_dep", "turbo_stats"]);
    send(
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"turbo_zoom","arguments":{"target":"src/main.ts:run","cwd":r.to_str().unwrap()}}}),
    );
    let call: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(call["result"]["isError"], false);
    assert!(call["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("return helper(2);"));
    send(
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"turbo_dep","arguments":{"package":"fastlib","symbol":"Engine","cwd":r.to_str().unwrap()}}}),
    );
    let dep: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert!(dep["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("fastlib 2.0.1"));
    send(
        json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"turbo_zoom","arguments":{"target":"nope.ts","cwd":r.to_str().unwrap()}}}),
    );
    let bad: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(bad["result"]["isError"], true);
    send(json!({"jsonrpc":"2.0","id":6,"method":"bogus/method"}));
    let unk: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(unk["error"]["code"], -32601);
    writeln!(stdin, "{{not json").unwrap();
    let parse: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(parse["error"]["code"], -32700);
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

#[test]
fn init_is_idempotent_and_preserves_mcp_entries() {
    let r = fresh("init");
    write(&r, "CLAUDE.md", "# Notes\n\nkeep me\n");
    write(
        &r,
        ".mcp.json",
        r#"{"mcpServers":{"other":{"command":"x"}}}"#,
    );
    let dry = ok(&r, &["init", "--dry-run"]);
    assert!(
        dry.contains("would create AGENTS.md") && !r.join("AGENTS.md").exists(),
        "{dry}"
    );
    let first = ok(&r, &["init"]);
    assert!(
        first.contains("created AGENTS.md")
            && first.contains("updated CLAUDE.md")
            && first.contains("updated .mcp.json"),
        "{first}"
    );
    let snapshot = |f: &str| fs::read_to_string(r.join(f)).unwrap();
    let (agents, claude, mcp) = (
        snapshot("AGENTS.md"),
        snapshot("CLAUDE.md"),
        snapshot(".mcp.json"),
    );
    assert!(
        claude.starts_with("# Notes\n\nkeep me\n")
            && claude.contains("<!-- turbocharger:start -->")
    );
    let v: Value = serde_json::from_str(&mcp).unwrap();
    assert_eq!(v["mcpServers"]["other"]["command"], "x");
    assert_eq!(v["mcpServers"]["turbocharger"]["args"][0], "mcp");
    let second = ok(&r, &["init"]);
    assert_eq!(second.matches("unchanged").count(), 3, "{second}");
    assert_eq!(
        (
            snapshot("AGENTS.md"),
            snapshot("CLAUDE.md"),
            snapshot(".mcp.json")
        ),
        (agents, claude, mcp)
    );
    let (_, err, code) = {
        write(&r, ".mcp.json", "{broken");
        run(&r, &["init", "--mcp"])
    };
    assert_eq!(code, 2);
    assert!(err.contains("not valid JSON"), "{err}");
    assert_eq!(fs::read_to_string(r.join(".mcp.json")).unwrap(), "{broken");
}

#[test]
fn stats_record_summarise_reset() {
    let r = project();
    let with_stats = |args: &[&str]| {
        tc().env_remove("TC_STATS_FILE")
            .arg("--cwd")
            .arg(&r)
            .args(args)
            .output()
            .unwrap()
    };
    assert!(with_stats(&["zoom", "src/main.ts:run"]).status.success());
    assert!(with_stats(&["dep", "fastlib", "Engine"]).status.success());
    assert_eq!(
        fs::read_to_string(r.join(".turbocharger/.gitignore"))
            .unwrap()
            .lines()
            .last(),
        Some("*")
    );
    let v: Value = serde_json::from_slice(&with_stats(&["--json", "stats"]).stdout).unwrap();
    assert_eq!(v["calls"], 2);
    assert!(v["dep_lookups"].as_u64().unwrap() >= 2, "{v}");
    assert!(v["baseline_tokens"].as_u64().unwrap() > 0, "{v}");
    let text = String::from_utf8_lossy(&with_stats(&["stats"]).stdout).to_string();
    assert!(
        text.contains("quota") && text.contains("× stretch") && text.contains("zoom.L2"),
        "{text}"
    );
    // --no-stats really writes nothing
    assert!(tc()
        .env_remove("TC_STATS_FILE")
        .arg("--cwd")
        .arg(&r)
        .args(["--no-stats", "zoom", "."])
        .status()
        .unwrap()
        .success());
    let v: Value = serde_json::from_slice(&with_stats(&["--json", "stats"]).stdout).unwrap();
    assert_eq!(v["calls"], 2);
    assert!(
        String::from_utf8_lossy(&with_stats(&["stats", "--reset"]).stdout).contains("stats reset")
    );
    assert!(String::from_utf8_lossy(&with_stats(&["stats"]).stdout).contains("no stats yet"));
}

// ---------------------------------------------------------------- regression tests, one per finding

/// Token estimate used by `tc` (`stats::tokens`): chars / 4, rounded up.
fn tokens(s: &str) -> usize {
    s.chars().count().div_ceil(4)
}

/// An executable script `name` in `root` (`.cmd` on Windows, `.sh` elsewhere); returns its file name.
fn script(root: &Path, name: &str, win: &str, unix: &str) -> String {
    #[cfg(windows)]
    {
        let _ = unix;
        let f = format!("{name}.cmd");
        write(
            root,
            &f,
            &format!("@echo off\r\n{}\r\n", win.replace('\n', "\r\n")),
        );
        f
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = win;
        let f = format!("{name}.sh");
        write(root, &f, &format!("#!/bin/sh\n{unix}\n"));
        fs::set_permissions(root.join(&f), fs::Permissions::from_mode(0o755)).unwrap();
        f
    }
}

/// A script that leaves `marker.txt` next to itself when it runs.
fn marker_script(root: &Path) -> PathBuf {
    let f = script(
        root,
        "mark",
        "echo ran> \"%~dp0marker.txt\"\necho usage: mark --x",
        "echo ran > \"$(dirname \"$0\")/marker.txt\"\necho 'usage: mark --x'",
    );
    root.join(f)
}

/// Start `tc [pre...] mcp` with `cwd` as the process directory.
fn mcp_child(cwd: &Path, pre: &[&str]) -> std::process::Child {
    tc().current_dir(cwd)
        .args(pre)
        .args(["--no-stats", "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// Feed `input` to a fresh MCP server and return every reply line.
fn mcp_session(cwd: &Path, pre: &[&str], input: &[u8]) -> Vec<Value> {
    let mut child = mcp_child(cwd, pre);
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    let w = std::thread::spawn(move || {
        stdin.write_all(&input).unwrap();
    });
    let out = child.wait_with_output().unwrap();
    w.join().unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn frame(v: Value) -> Vec<u8> {
    format!("{v}\n").into_bytes()
}

#[test]
fn f01_mcp_never_launches_programs() {
    let r = project();
    let mark = marker_script(&r);
    let mark = mark.to_str().unwrap();
    let mut input = frame(json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}));
    for (id, args) in [
        (2, json!({"bin": mark})),
        (3, json!({"flag": mark})),
        (4, json!({"package": "fastlib", "flag": mark})),
    ] {
        input.extend(frame(
            json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"turbo_dep","arguments":args}}),
        ));
    }
    let r2 = mcp_session(&r, &["--cwd", r.to_str().unwrap()], &input);
    let dep_props = &r2[0]["result"]["tools"][1]["inputSchema"]["properties"];
    assert!(
        dep_props.get("bin").is_none() && dep_props.get("flag").is_none(),
        "{dep_props}"
    );
    for reply in &r2[1..] {
        assert_eq!(reply["error"]["code"], -32602, "{reply}");
    }
    // CLI: `--bin` only takes <cmd> [flag]; a package next to it is refused, not run as a flag.
    let (_, err, code) = run(&r, &["dep", "fastlib", "--bin", mark]);
    assert_eq!(code, 2, "{err}");
    assert!(!r.join("marker.txt").exists(), "a program was executed");
}

#[test]
fn f02_init_refuses_ambiguous_markers_and_changes_nothing() {
    let r = fresh("f02");
    let claude = "# Mine\n<!-- turbocharger:start -->\nold\n<!-- turbocharger:end -->\nkeep this\n<!-- turbocharger:start -->\nnot ours\n<!-- turbocharger:end -->\n";
    write(&r, "CLAUDE.md", claude);
    let (_, err, code) = run(&r, &["init"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("markers"), "{err}");
    assert_eq!(fs::read_to_string(r.join("CLAUDE.md")).unwrap(), claude);
    assert!(!r.join("AGENTS.md").exists() && !r.join(".mcp.json").exists());
    // An inverted pair is refused too; a single ordered pair is replaced, bytes around it kept.
    write(
        &r,
        "CLAUDE.md",
        "a\n<!-- turbocharger:end -->\nb\n<!-- turbocharger:start -->\n",
    );
    assert_eq!(run(&r, &["init", "--agents"]).2, 2);
    let good = "top  \n\n<!-- turbocharger:start -->\nold\n<!-- turbocharger:end -->\n\ntail";
    write(&r, "CLAUDE.md", good);
    ok(&r, &["init", "--agents"]);
    let now = fs::read_to_string(r.join("CLAUDE.md")).unwrap();
    assert!(
        now.starts_with("top  \n\n<!-- turbocharger:start -->\n## Turbocharger"),
        "{now:?}"
    );
    assert!(
        now.ends_with("<!-- turbocharger:end -->\n\ntail") && !now.contains("old"),
        "{now:?}"
    );
}

#[test]
fn f03_init_validates_everything_first_and_keeps_custom_entries() {
    let r = fresh("f03");
    write(&r, "CLAUDE.md", "# Notes\n");
    write(&r, ".mcp.json", "{broken");
    let (_, err, code) = run(&r, &["init"]);
    assert_eq!(code, 2, "{err}");
    assert!(
        !r.join("AGENTS.md").exists(),
        "AGENTS.md written before .mcp.json was validated"
    );
    assert_eq!(
        fs::read_to_string(r.join("CLAUDE.md")).unwrap(),
        "# Notes\n"
    );
    let custom = "{\n  \"mcpServers\": {\n    \"turbocharger\": {\"command\": \"tc-dev\", \"args\": [\"mcp\", \"--no-stats\"]}\n  }\n}\n";
    write(&r, ".mcp.json", custom);
    let out = ok(&r, &["init"]);
    assert!(
        out.contains("conflict .mcp.json") && out.contains("customized"),
        "{out}"
    );
    assert_eq!(fs::read_to_string(r.join(".mcp.json")).unwrap(), custom);
    assert!(fs::read_to_string(r.join("AGENTS.md"))
        .unwrap()
        .contains("## Turbocharger"));
    let leftovers: Vec<String> = fs::read_dir(&*r)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn f04_bin_deadline_kills_the_tree_and_failures_are_errors() {
    let r = fresh("f04");
    // Exits at once, but a grandchild keeps stdout open for ~6 s and would then write late.txt.
    let slow = script(
        &r,
        "slow",
        "echo usage: slow --x\nstart \"\" /b cmd /c \"ping -n 7 127.0.0.1 >nul & echo late> late.txt\"",
        "echo 'usage: slow --x'\n(sleep 6; echo late > late.txt) &",
    );
    let t0 = std::time::Instant::now();
    let slow = r.join(slow).to_string_lossy().into_owned(); // absolute: f16 covers relative
    let (_, err, code) = run(&r, &["dep", "--bin", &slow, "--flag", "--x"]);
    let took = t0.elapsed();
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("output open") || err.contains("timed out"),
        "{err}"
    );
    assert!(
        took < std::time::Duration::from_millis(5500),
        "took {took:?}"
    );
    std::thread::sleep(std::time::Duration::from_secs(8).saturating_sub(t0.elapsed()));
    assert!(
        !r.join("late.txt").exists(),
        "grandchild survived the timeout"
    );
    // A nonzero exit is a failure, not a help text.
    let fail = script(
        &r,
        "fail",
        "echo usage: fail --x\nexit /b 3",
        "echo 'usage: fail --x'\nexit 3",
    );
    let fail = r.join(fail).to_string_lossy().into_owned();
    let (out, err, code) = run(&r, &["dep", "--bin", &fail, "--flag", "--x"]);
    assert_eq!(code, 2, "{out}{err}");
    assert!(err.contains("failed"), "{err}");
}

#[test]
fn f05_deep_nesting_does_not_overflow() {
    let r = fresh("f05");
    let n = 3000;
    let mods =
        "pub mod m {\n".repeat(n) + "pub fn leaf() {}\n" + &"}\n".repeat(n) + "pub fn top() {}\n";
    write(&r, "src/mods.rs", &mods);
    let uses = format!(
        "use a::{}c{};\npub fn u() {{}}\n",
        "{b::".repeat(n),
        "}".repeat(n)
    );
    write(&r, "src/uses.rs", &uses);
    let ts = "export namespace A {\n".repeat(n)
        + "export function z() {}\n"
        + &"}\n".repeat(n)
        + "export function top() {}\n";
    write(&r, "src/deep.ts", &ts);
    let py = (0..400)
        .map(|i| format!("{}if x:\n", "    ".repeat(i)))
        .collect::<String>()
        + &"    ".repeat(400)
        + "pass\ndef top():\n    return ((((((((1))))))))\n";
    write(&r, "src/deep.py", &py);
    let java = "public class Deep {\n".to_string()
        + &"public static class C {\n".repeat(n)
        + "public void z() {}\n"
        + &"}\n".repeat(n)
        + "public int top() { return 1; }\n}\n";
    write(&r, "src/Deep.java", &java);
    for t in [
        "src",
        "src/mods.rs",
        "src/mods.rs:top",
        "src/uses.rs",
        "src/deep.ts",
        "src/deep.ts:top",
        "src/deep.py:top",
        "src/Deep.java",
        "src/Deep.java:top",
    ] {
        ok(&r, &["zoom", t, "--budget", "300"]);
    }
}

#[test]
fn f06_node_subpath_imports_use_their_own_exports() {
    let r = fresh("f06");
    write(
        &r,
        "node_modules/sub/package.json",
        r#"{"name":"sub","version":"1.2.3","exports":{".":{"types":"./index.d.ts"},"./sample":{"types":"./sample.d.ts"}}}"#,
    );
    write(
        &r,
        "node_modules/sub/index.d.ts",
        "export declare function make(a: string): number;\n",
    );
    write(
        &r,
        "node_modules/sub/sample.d.ts",
        "export declare function make(b: boolean): string;\n",
    );
    write(
        &r,
        "src/use.ts",
        "import { make } from 'sub';\nimport { make as make2 } from 'sub/sample';\nexport function go() {\n  return make('x') + make2(true);\n}\nexport function only() {\n  return make2(false);\n}\n",
    );
    let out = ok(&r, &["zoom", "src/use.ts:go"]);
    assert!(
        out.contains("make(a: string): number") && out.contains("make(b: boolean): string"),
        "{out}"
    );
    let out = ok(&r, &["zoom", "src/use.ts:only"]);
    assert!(
        out.contains("sub/sample 1.2.3") && out.contains("make(b: boolean): string"),
        "{out}"
    );
    assert!(!out.contains("make(a: string)"), "{out}");
    let out = ok(&r, &["dep", "sub/sample", "make"]);
    assert!(out.contains("make(b: boolean): string"), "{out}");
}

#[test]
fn f07_node_exports_follow_edges_aliases_and_junctions() {
    let r = fresh("f07");
    write(
        &r,
        "node_modules/edges/package.json",
        r#"{"name":"edges","version":"1.0.0","types":"index.d.ts"}"#,
    );
    write(
        &r,
        "node_modules/edges/index.d.ts",
        "import { Internal } from './internal';\nexport declare function api(x: Internal): void;\nexport { Source as Public } from './source';\n",
    );
    write(
        &r,
        "node_modules/edges/internal.d.ts",
        "export interface Internal { secret: number }\n",
    );
    write(
        &r,
        "node_modules/edges/source.d.ts",
        "/** The source. */\nexport interface Source {\n  /** A member. */\n  member(): void;\n}\n",
    );
    let (out, _, code) = run(&r, &["dep", "edges", "Internal"]);
    assert_eq!(code, 1, "internal import reported as public API:\n{out}");
    let out = ok(&r, &["dep", "edges", "Public"]);
    assert!(
        out.contains("export interface Source") && out.contains("exported as `Public`"),
        "{out}"
    );
    let out = ok(&r, &["dep", "edges", "Public.member"]);
    assert!(out.contains("member(): void"), "{out}");
    let out = ok(&r, &["dep", "edges"]);
    assert!(out.contains("Public") && out.contains("api"), "{out}");
    assert!(
        !out.contains("Internal") && !out.contains("Source"),
        "{out}"
    );
    // A workspace package linked into node_modules: `../shared` resolves from the real location.
    write(
        &r,
        "packages/linked/package.json",
        r#"{"name":"linked","version":"0.0.1","types":"index.d.ts"}"#,
    );
    write(
        &r,
        "packages/linked/index.d.ts",
        "export * from '../shared/types';\n",
    );
    write(
        &r,
        "packages/shared/types.d.ts",
        "export interface Shared { x: number }\n",
    );
    let (link, real) = (
        r.join("node_modules").join("linked"),
        r.join("packages").join("linked"),
    );
    #[cfg(windows)]
    {
        let o = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&real)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let out = ok(&r, &["dep", "linked", "Shared"]);
    assert!(out.contains("export interface Shared"), "{out}");
}

#[test]
fn f07_export_assignment_same_file_aliases_and_subpath_files() {
    let r = fresh("f07b");
    let pkg = |name: &str, files: &[(&str, &str)]| {
        let pj = format!(r#"{{"name":"{name}","version":"1.0.0","types":"index.d.ts"}}"#);
        write(&r, &format!("node_modules/{name}/package.json"), &pj);
        for (f, body) in files {
            write(&r, &format!("node_modules/{name}/{f}"), body);
        }
    };
    // `export = Lib`: the namespace's members are importable by name.
    pkg(
        "eq",
        &[(
            "index.d.ts",
            "export = Lib;
declare namespace Lib {
  function make(n: number): string;
}
",
        )],
    );
    let out = ok(&r, &["dep", "eq", "make"]);
    assert!(out.contains("function make(n: number): string;"), "{out}");
    // Same-file alias: only the alias is public in a module declaration file.
    pkg(
        "same",
        &[(
            "index.d.ts",
            "interface Source {
  member(): void;
}
declare class Hidden {}
export { Source as Public };
",
        )],
    );
    let out = ok(&r, &["dep", "same"]);
    assert!(
        out.contains("Public") && !out.contains("Source") && !out.contains("Hidden"),
        "{out}"
    );
    assert_eq!(run(&r, &["dep", "same", "Source"]).2, 1);
    assert_eq!(run(&r, &["dep", "same", "Hidden"]).2, 1);
    assert!(ok(&r, &["dep", "same", "Public.member"]).contains("member(): void"));
    // A deep import of a file path, no `exports` map: `lib/a.js` answers from `lib/a.d.ts`.
    pkg(
        "sub",
        &[
            (
                "index.d.ts",
                "export declare function root(): void;
",
            ),
            (
                "lib/a.d.ts",
                "export declare function a(x: number): number;
",
            ),
        ],
    );
    write(
        &r,
        "src/use.ts",
        "import { make } from 'eq';
import { a } from 'sub/lib/a.js';
import { b } from 'sub/nope';
export function go() {
  return make(1) + a(2) + b;
}
",
    );
    let out = ok(&r, &["zoom", "src/use.ts:go"]);
    assert!(
        out.contains("make(n: number): string") && out.contains("a(x: number): number"),
        "{out}"
    );
    assert!(
        out.contains("`sub/nope` resolves to no file")
            && !out.contains("not defined in installed version"),
        "{out}"
    );
}

/// A venv (Windows layout) holding the Python packages the f08 test needs.
fn py_fixture() -> Fixture {
    let r = fresh("f08");
    write(
        &r,
        ".venv/pyvenv.cfg",
        "home = C:\\nowhere\ninclude-system-site-packages = false\n",
    );
    let site = ".venv/Lib/site-packages";
    let w = |f: &str, body: &str| write(&r, &format!("{site}/{f}"), body);
    w("wrong/__init__.py", "");
    w("wrong/real.py", "class Thing:\n    def other(self): ...\n");
    w("wrong/other.py", "class Thing:\n    def wrong(self): ...\n");
    w(
        "alias_pkg/__init__.py",
        "from ._impl import Source as Public\n",
    );
    w(
        "alias_pkg/_impl.py",
        "class Source:\n    \"\"\"Impl.\"\"\"\n\n    def go(self) -> int: ...\n",
    );
    w("hop_a/__init__.py", "from hop_b import Widget\n");
    w(
        "hop_b/__init__.py",
        "from hop_c.core import Gadget as Widget\n",
    );
    w("hop_c/__init__.py", "");
    w(
        "hop_c/core.py",
        "class Gadget:\n    \"\"\"Three hops away.\"\"\"\n",
    );
    w("cyc_a/__init__.py", "from cyc_b import Loop\n");
    w("cyc_b/__init__.py", "from cyc_a import Loop\n");
    w("stubonly.pyi", "def thing(a: int) -> str: ...\n");
    w(
        "stubonly-1.0.0.dist-info/METADATA",
        "Name: stubonly\nVersion: 1.0.0\n",
    );
    r
}

#[test]
fn f08_python_lookups_stay_in_scope_and_follow_reexports() {
    let r = py_fixture();
    let (out, _, code) = run(&r, &["dep", "wrong.real", "Thing.wrong"]);
    assert_eq!(code, 1, "{out}");
    assert!(!out.contains("import wrong"), "invented an import:\n{out}");
    let out = ok(&r, &["dep", "wrong.other", "Thing.wrong"]);
    assert!(
        out.contains("import Thing\n") && out.contains("def wrong(self)"),
        "{out}"
    );
    let out = ok(&r, &["dep", "alias_pkg", "Public"]);
    assert!(
        out.contains("from alias_pkg import Public") && out.contains("class Source"),
        "{out}"
    );
    let out = ok(&r, &["dep", "alias_pkg", "Public.go"]);
    assert!(
        out.contains("from alias_pkg import Public") && out.contains("def go(self) -> int"),
        "{out}"
    );
    let out = ok(&r, &["dep", "hop_a", "Widget"]);
    assert!(
        out.contains("from hop_a import Widget") && out.contains("Three hops away."),
        "{out}"
    );
    let (_, _, code) = run(&r, &["dep", "cyc_a", "Loop"]);
    assert_eq!(code, 1);
    let out = ok(&r, &["dep", "stubonly", "thing"]);
    assert!(
        out.contains("stubonly 1.0.0") && out.contains("def thing(a: int) -> str"),
        "{out}"
    );
}

#[test]
fn f09_rust_version_comes_from_the_members_lock_edge() {
    let r = fresh("f09");
    write(
        &r,
        "ws/Cargo.toml",
        "[workspace]\nmembers = [\"app\", \"other\"]\n",
    );
    let src = "source = \"registry+https://github.com/rust-lang/crates.io-index\"";
    write(
        &r,
        "ws/Cargo.lock",
        &format!("version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"dupe 0.1.0\",\n]\n\n[[package]]\nname = \"dupe\"\nversion = \"0.1.0\"\n{src}\n\n[[package]]\nname = \"dupe\"\nversion = \"0.2.0\"\n{src}\n\n[[package]]\nname = \"other\"\nversion = \"0.1.0\"\ndependencies = [\n \"dupe 0.2.0\",\n]\n"),
    );
    write(
        &r,
        "ws/app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    );
    write(
        &r,
        "ws/other/Cargo.toml",
        "[package]\nname = \"other\"\nversion = \"0.1.0\"\n",
    );
    let reg = "cargo/registry/src/index.crates.io-0000";
    write(
        &r,
        &format!("{reg}/dupe-0.1.0/src/lib.rs"),
        "/// Old.\npub fn old_api() -> u8 { 1 }\n",
    );
    write(
        &r,
        &format!("{reg}/dupe-0.2.0/src/lib.rs"),
        "/// New.\npub fn new_api() -> u8 { 2 }\n",
    );
    let dep = |dir: &str, args: &[&str]| {
        let o = tc()
            .env("CARGO_HOME", r.join("cargo"))
            .args(["--no-stats", "--cwd"])
            .arg(r.join(dir))
            .arg("dep")
            .args(args)
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&o.stdout).to_string(),
            o.status.code(),
        )
    };
    let (out, code) = dep("ws/app", &["dupe", "old_api"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("dupe 0.1.0"), "{out}");
    let (out, code) = dep("ws/other", &["dupe", "new_api"]);
    assert_eq!(code, Some(0), "{out}");
    let (out, _) = dep("ws", &["dupe"]);
    assert!(
        out.contains("ambiguous") && out.contains("0.1.0") && out.contains("0.2.0"),
        "{out}"
    );
}

#[test]
fn f10_f11_f16_mcp_frames_envelopes_and_root() {
    let r = project();
    let ping = |id: u32| frame(json!({"jsonrpc":"2.0","id":id,"method":"ping"}));
    let call = |id: u32, name: &str, args: Value| {
        frame(
            json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}}),
        )
    };
    let mut input = Vec::new();
    input.extend(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"x\":\"\xff\"}\n"); // invalid UTF-8
    input.extend(ping(2));
    input.extend(vec![b' '; 4 << 20]); // oversized frame
    input.extend(b"x\n");
    input.extend(ping(3));
    input.extend(b"{}\n");
    input.extend(b"{\"id\":4,\"method\":\"ping\"}\n");
    input.extend(b"{\"jsonrpc\":\"2.0\",\"id\":null,\"method\":\"ping\"}\n");
    input.extend(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n");
    input.extend(frame(json!({"jsonrpc":"2.0","id":5,"method":"initialize","params":{"protocolVersion":"2025-03-26"}})));
    input.extend(call(6, "turbo_nope", json!({})));
    input.extend(call(
        7,
        "turbo_zoom",
        json!({"target": "src/main.ts", "budget": "big"}),
    ));
    input.extend(call(
        8,
        "turbo_dep",
        json!({"package": "fastlib", "symbol": "FastEngine"}),
    ));
    input.extend(call(9, "turbo_zoom", json!({"target": "src/main.ts:run"}))); // no cwd: --cwd root
    input.extend(ping(10).trim_ascii_end()); // last frame without a newline
    let elsewhere = fresh("f10-elsewhere");
    let out = mcp_session(&elsewhere, &["--cwd", r.to_str().unwrap()], &input);
    let by_id = |id: Value| {
        out.iter()
            .find(|v| v["id"] == id)
            .cloned()
            .unwrap_or(Value::Null)
    };
    let nulls: Vec<Value> = out
        .iter()
        .filter(|v| v["id"].is_null())
        .map(|v| v["error"]["code"].clone())
        .collect();
    assert_eq!(nulls, [-32700, -32700, -32600, -32600], "{out:#?}");
    assert_eq!(by_id(json!(4))["error"]["code"], -32600, "missing jsonrpc");
    for id in [2, 3, 10] {
        assert_eq!(
            by_id(json!(id))["result"],
            json!({}),
            "ping {id} lost: {out:#?}"
        );
    }
    assert_eq!(by_id(json!(5))["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(by_id(json!(6))["error"]["code"], -32602);
    assert_eq!(by_id(json!(7))["error"]["code"], -32602);
    assert_eq!(
        by_id(json!(8))["result"]["isError"],
        true,
        "a miss is a tool error"
    );
    let zoom = by_id(json!(9));
    assert_eq!(zoom["result"]["isError"], false, "{zoom}");
    assert!(zoom["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("return helper(2);"));
    assert_eq!(
        out.len(),
        13,
        "the notification must not be answered: {out:#?}"
    );
}

#[test]
fn f12_budget_bounds_text_and_json_at_every_level() {
    let r = project();
    for target in [
        ".",
        "src",
        "java",
        "src/main.ts",
        "src/main.ts:run",
        "app/server.py:compute",
    ] {
        for budget in [1usize, 5, 30, 100, 400] {
            for json in [false, true] {
                let b = budget.to_string();
                let mut args = vec!["zoom", target, "--budget", &b];
                if json {
                    args.insert(0, "--json");
                }
                let out = ok(&r, &args);
                assert!(
                    tokens(&out) <= budget,
                    "{args:?}: {} tokens\n{out}",
                    tokens(&out)
                );
                if json {
                    serde_json::from_str::<Value>(&out)
                        .unwrap_or_else(|e| panic!("{args:?}: {e}\n{out}"));
                }
            }
        }
    }
    let out = ok(&r, &["zoom", "src/main.ts:run", "--budget", "40"]);
    assert!(
        out.contains("## TARGET") && out.contains("cut by --budget 40"),
        "{out}"
    );
    ok(
        &r,
        &["zoom", "src/main.ts:run", "--budget", &u64::MAX.to_string()],
    );
}

#[test]
fn f13_stats_survive_concurrent_writers_and_corrupt_lines() {
    let r = project();
    let ledger = r.join("ledger/stats.jsonl");
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let (r, ledger) = (r.to_path_buf(), ledger.clone());
            std::thread::spawn(move || {
                for _ in 0..6 {
                    let o = tc()
                        .env("TC_STATS_FILE", &ledger)
                        .arg("--cwd")
                        .arg(&r)
                        .args(["zoom", "src/util.ts"])
                        .output()
                        .unwrap();
                    assert!(o.status.success());
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let stats = |extra: &[u8]| {
        let mut f = fs::OpenOptions::new().append(true).open(&ledger).unwrap();
        f.write_all(extra).unwrap();
        let o = tc()
            .env("TC_STATS_FILE", &ledger)
            .args(["--json", "stats"])
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        serde_json::from_slice::<Value>(&o.stdout).unwrap()
    };
    let v = stats(b"");
    assert_eq!(
        (v["calls"].clone(), v["skipped_lines"].clone()),
        (json!(48), json!(0)),
        "{v}"
    );
    let big = r#"{"ts":1,"tool":"zoom.L1","target":"x","ms":1.0,"actual_tokens":18446744073709551615,"baseline_tokens":18446744073709551615,"dep_lookups":18446744073709551615,"ok":true}"#;
    let mut junk = b"not json\n{\"ts\":\xff\xfe}\n{\"ts\":1e30}\n".to_vec();
    junk.extend(format!("{big}\n{big}\n").as_bytes());
    let v = stats(&junk);
    assert_eq!(
        (v["calls"].clone(), v["skipped_lines"].clone()),
        (json!(50), json!(3)),
        "{v}"
    );
    assert_eq!(v["actual_tokens"], json!(u64::MAX), "sums saturate");
}

#[test]
fn f14_stats_reset_only_touches_its_own_ledger() {
    let r = fresh("f14");
    let home = r.join("home");
    write(&home, ".turbocharger/stats.jsonl", "{\"keep\":true}\n");
    let reset = |stats_file: &Path| {
        let o = tc()
            .env("USERPROFILE", &home)
            .env("HOME", &home)
            .env("TC_STATS_FILE", stats_file)
            .args(["stats", "--reset"])
            .output()
            .unwrap();
        let text =
            String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
        (text, o.status.code())
    };
    let (out, code) = reset(&r.join("missing/stats.jsonl"));
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("no stats to reset"), "{out}");
    assert_eq!(
        fs::read_to_string(home.join(".turbocharger/stats.jsonl")).unwrap(),
        "{\"keep\":true}\n"
    );
    // An I/O error is reported, not mistaken for "nothing to reset".
    fs::create_dir_all(r.join("adir")).unwrap();
    let (out, code) = reset(&r.join("adir"));
    assert_eq!(code, Some(2), "{out}");
    assert!(out.contains("resetting"), "{out}");
}

#[test]
fn f15_stats_measure_what_was_printed_and_shown() {
    let r = project();
    write(&r, ".venv/Lib/site-packages/multi/__init__.py", "");
    write(
        &r,
        ".venv/Lib/site-packages/multi/a.py",
        "class Thing:\n    \"\"\"A.\"\"\"\n",
    );
    write(
        &r,
        ".venv/Lib/site-packages/multi/b.py",
        &format!(
            "class Thing:\n    \"\"\"B.\"\"\"\n{}",
            "# pad\n".repeat(500)
        ),
    );
    let ledger = r.join("ledger/stats.jsonl");
    let rec = |args: &[&str]| {
        let o = tc()
            .env("TC_STATS_FILE", &ledger)
            .arg("--cwd")
            .arg(&r)
            .args(args)
            .output()
            .unwrap();
        let last = fs::read_to_string(&ledger)
            .unwrap()
            .lines()
            .last()
            .unwrap()
            .to_string();
        (
            String::from_utf8_lossy(&o.stdout).to_string(),
            serde_json::from_str::<Value>(&last).unwrap(),
        )
    };
    let (out, e) = rec(&["--json", "zoom", "src/main.ts:run"]);
    assert_eq!(
        e["actual_tokens"],
        json!(tokens(&out)),
        "JSON output measured as text"
    );
    let (out, e) = rec(&["dep", "multi", "Thing"]);
    assert!(out.contains("also defined"), "{out}");
    let shown = fs::metadata(r.join(".venv/Lib/site-packages/multi/a.py"))
        .unwrap()
        .len();
    assert_eq!(e["baseline_tokens"], json!(shown.div_ceil(4)), "{out}");
    let exe = env!("CARGO_BIN_EXE_tc");
    rec(&["dep", "--bin", exe, "--flag", "--no-stats"]);
    let o = tc()
        .env("TC_STATS_FILE", &ledger)
        .arg("stats")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(
        !text.contains("0 code executed") && text.contains("--bin calls   1"),
        "{text}"
    );
    assert!(text.contains("estimate"), "{text}");
}

#[test]
fn f16_bin_runs_relative_to_cwd() {
    let r = fresh("f16");
    let w = script(&r, "where", "echo cwd=%CD%", "echo \"cwd=$(pwd)\"");
    let out = ok(&r, &["dep", "--bin", &format!("./{w}")]);
    let name = r.file_name().unwrap().to_string_lossy().to_string();
    assert!(out.contains("cwd=") && out.contains(&name), "{out}");
}

#[test]
fn f17_callees_by_definition_exact_target_unicode_words() {
    let r = fresh("f17");
    write(
        &r,
        "src/two.ts",
        "class A {\n  run(): number { return 1 }\n}\nclass B {\n  run(): string { return 'b' }\n}\nexport function go(a: A, b: B) {\n  a.run();\n  b.run();\n}\n",
    );
    let out = ok(&r, &["zoom", "src/two.ts:go"]);
    assert!(
        out.contains("run(): number") && out.contains("run(): string"),
        "{out}"
    );
    write(
        &r,
        "src/line.ts",
        "export function a() { return 1 } export function b() { return 2 }\n",
    );
    let out = ok(&r, &["zoom", "src/line.ts:b"]);
    assert!(
        out.contains("function b()") && !out.contains("function a()"),
        "{out}"
    );
    write(
        &r,
        "src/uni.ts",
        "export function f() {}\nconst αf = 1;\nαf;\nf();\n",
    );
    let out = ok(&r, &["zoom", "src/uni.ts:f"]);
    assert!(out.contains("src/uni.ts:4"), "{out}");
    assert!(
        !out.contains("src/uni.ts:2") && !out.contains("src/uni.ts:3"),
        "{out}"
    );
}

#[test]
fn f18_public_api_views_see_real_declarations() {
    let r = fresh("f18");
    write(
        &r,
        "src/api.ts",
        "function named(): number { return 1 }\nexport { named };\nexport default function () { return 2 }\nfunction hidden2() {}\n",
    );
    write(&r, "src/build/gen.ts", "export function built(): void {}\n");
    write(&r, "src/env/cfg.py", "def configured():\n    pass\n");
    let out = ok(&r, &["zoom", "src"]);
    for want in [
        "function named(): number",
        "3: function ()",
        "built()",
        "configured()",
    ] {
        assert!(out.contains(want), "missing {want}:\n{out}");
    }
    assert!(!out.contains("hidden2"), "{out}");
    write(
        &r,
        "node_modules/defpkg/package.json",
        r#"{"name":"defpkg","version":"3.0.0","types":"index.d.ts"}"#,
    );
    write(
        &r,
        "node_modules/defpkg/index.d.ts",
        "export default function makeThing(n: number): string;\n",
    );
    write(
        &r,
        "src/def.ts",
        "import makeThing from 'defpkg';\nexport function use() {\n  return makeThing(1);\n}\n",
    );
    let out = ok(&r, &["zoom", "src/def.ts:use"]);
    assert!(
        out.contains("defpkg 3.0.0") && out.contains("makeThing(n: number): string"),
        "{out}"
    );
    write(
        &r,
        "java/Consts.java",
        "public class Consts {\n    public static final int MAX = 5;\n    private int hidden;\n}\n",
    );
    let out = ok(&r, &["zoom", "java"]);
    assert!(
        out.contains("public static final int MAX = 5;") && !out.contains("hidden"),
        "{out}"
    );
    write(
        &r,
        "rs/src/vis.rs",
        "pub(crate) fn crate_only() {}\nmod private {\n    pub fn inner_pub() {}\n}\npub mod api {\n    pub fn api_fn() {}\n}\npub fn real() {}\n",
    );
    let out = ok(&r, &["zoom", "rs/src"]);
    assert!(out.contains("api_fn") && out.contains("fn real()"), "{out}");
    assert!(
        !out.contains("crate_only") && !out.contains("inner_pub"),
        "{out}"
    );
}

#[test]
fn recheck_01_init_rejects_dir_without_partial_writes() {
    let r = fresh("rc01");
    fs::create_dir_all(r.join(".mcp.json")).unwrap();
    let (_, err, code) = run(&r, &["init", "--all"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains(".mcp.json"), "{err}");
    assert!(!r.join("AGENTS.md").exists(), "partial write occurred");
}

#[test]
fn recheck_02_bin_kills_grandchild_after_parent_exits() {
    let r = fresh("rc02");
    // Parent spawns a grandchild that holds stdout open for 20s, then parent exits immediately.
    let w = script(
        &r,
        "orphan",
        "@echo off\r\nstart /b ping -n 20 127.0.0.1\r\necho ready\r\nexit /b 0",
        "sleep 20 &\necho ready\nexit 0",
    );
    let t0 = std::time::Instant::now();
    let (_, err, code) = run(&r, &["dep", "--bin", &format!("./{w}")]);
    let elapsed = t0.elapsed();
    assert_eq!(code, 2, "{err}");
    assert!(
        elapsed.as_secs_f64() < 8.0,
        "hung past deadline ({elapsed:?}): {err}"
    );
}

#[test]
fn recheck_03_bin_allows_trailing_global_cwd_and_no_stats() {
    let r = fresh("rc03");
    let w = script(&r, "here", "echo cwd=%CD%", "echo \"cwd=$(pwd)\"");
    let stats_file = r.join("no_stats.jsonl");
    let o = tc()
        .env("TC_STATS_FILE", &stats_file)
        .args([
            "dep",
            "--bin",
            &format!("./{w}"),
            "--no-stats",
            "--cwd",
            r.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "{out}{err}");
    let name = r.file_name().unwrap().to_string_lossy().to_string();
    assert!(out.contains(&name), "{out}");
    assert!(
        !stats_file.exists(),
        "--no-stats after --bin was swallowed and recorded stats"
    );
    // `--flag` without `--bin` is rejected before dispatch.
    let (_, err, code) = run(&r, &["dep", "--flag", "--locked"]);
    assert_eq!(code, 2, "{err}");
}

#[test]
fn recheck_04_node_exports_are_enforced() {
    let r = fresh("rc04");
    write(
        &r,
        "node_modules/exp/package.json",
        r#"{"name":"exp","version":"1.0.0","exports":{"./mapped":"./dist/real.js","./blocked":null,"./condnull":{"import":null,"default":null},"./nodenull":{"node":null,"default":"./dist/real.js"},"./syncnull":{"module-sync":null,"default":"./dist/real.js"},"./esmonly":{"require":null,"import":"./dist/real.js"},"./*":"./dist/*.js","./*.private":null}}"#,
    );
    write(
        &r,
        "node_modules/@types/exp/nodenull.d.ts",
        "export declare function leakedNodeNull(): void;\n",
    );
    write(
        &r,
        "node_modules/exp/dist/.private.d.ts",
        "export declare function dotPrivate(): boolean;\n",
    );
    write(
        &r,
        "node_modules/@types/exp/condnull.d.ts",
        "export declare function leakedCond(): void;\n",
    );
    write(
        &r,
        "node_modules/exp/mapped.d.ts",
        "export declare function f(): number;\n",
    );
    write(
        &r,
        "node_modules/exp/dist/real.js",
        "export function f() { return 'ok'; }\n",
    );
    write(
        &r,
        "node_modules/exp/dist/real.d.ts",
        "export declare function f(): string;\n",
    );
    write(
        &r,
        "node_modules/exp/dist/foo.private.d.ts",
        "export declare function secret(): void;\n",
    );
    write(
        &r,
        "node_modules/exp/blocked.d.ts",
        "export declare function secret(): void;\n",
    );
    // Even with @types/exp present, a null-blocked subpath must not resolve through @types.
    write(
        &r,
        "node_modules/@types/exp/package.json",
        r#"{"name":"@types/exp","version":"1.0.0"}"#,
    );
    write(
        &r,
        "node_modules/@types/exp/blocked.d.ts",
        "export declare function secret(): void;\n",
    );
    let out = ok(&r, &["dep", "exp/mapped", "f"]);
    assert!(
        out.contains("(): string") && !out.contains("(): number"),
        "{out}"
    );
    let (_, _, code) = run(&r, &["dep", "exp/blocked", "secret"]);
    assert_ne!(code, 0);
    let (_, _, code) = run(&r, &["dep", "exp/condnull", "leakedCond"]);
    assert_ne!(code, 0);
    let (_, _, code) = run(&r, &["dep", "exp/nodenull", "leakedNodeNull"]);
    assert_ne!(code, 0);
    let (_, _, code) = run(&r, &["dep", "exp/syncnull", "f"]);
    assert_ne!(code, 0);
    // `{"require": null, "import": "./dist/real.js"}` must still resolve under ESM import!
    let out = ok(&r, &["dep", "exp/esmonly", "f"]);
    assert!(out.contains("(): string"), "{out}");
    let (_, _, code) = run(&r, &["dep", "exp/foo.private", "secret"]);
    assert_ne!(code, 0);
    // Node resolves `./.private` through `./*` because `*` in `./*.private` cannot match empty.
    let out = ok(&r, &["dep", "exp/.private", "dotPrivate"]);
    assert!(out.contains("(): boolean"), "{out}");
}

#[test]
fn recheck_05_node_unexported_names_stay_private() {
    let r = fresh("rc05");
    write(
        &r,
        "node_modules/emptyre/package.json",
        r#"{"name":"emptyre","version":"1.0.0","types":"index.d.ts"}"#,
    );
    write(
        &r,
        "node_modules/emptyre/lib.d.ts",
        "export declare function leaked(): void;\n",
    );
    write(
        &r,
        "node_modules/emptyre/index.d.ts",
        "export {} from './lib';\n",
    );
    let (out, _, code) = run(&r, &["dep", "emptyre", "leaked"]);
    assert_eq!(code, 1, "{out}");
    let list_out = ok(&r, &["dep", "emptyre"]);
    assert!(!list_out.contains("leaked"), "{list_out}");

    write(
        &r,
        "node_modules/emptyexp/package.json",
        r#"{"name":"emptyexp","version":"1.0.0","types":"index.d.ts"}"#,
    );
    write(
        &r,
        "node_modules/emptyexp/lib.d.ts",
        "export declare function secret(): void;\n",
    );
    write(
        &r,
        "node_modules/emptyexp/index.d.ts",
        "import { secret } from './lib';\nexport {};\n",
    );
    let (out, _, code) = run(&r, &["dep", "emptyexp", "secret"]);
    assert_eq!(code, 1, "{out}");

    write(
        &r,
        "node_modules/defcls/package.json",
        r#"{"name":"defcls","version":"1.0.0","types":"index.d.ts"}"#,
    );
    write(
        &r,
        "node_modules/defcls/index.d.ts",
        "interface PrivateHelper { x: number; }\nexport default class Widget { run(): void; }\n",
    );
    let (out, _, code) = run(&r, &["dep", "defcls", "Widget"]);
    assert_eq!(code, 1, "{out}");
    let (out, _, code) = run(&r, &["dep", "defcls", "PrivateHelper"]);
    assert_eq!(code, 1, "{out}");
    let out = ok(&r, &["dep", "defcls", "default"]);
    assert!(out.contains("class Widget"), "{out}");

    // Multi-declarator `export const first = ..., second = ...` stays exported!
    write(
        &r,
        "node_modules/multiexp/package.json",
        r#"{"name":"multiexp","version":"1.0.0","main":"index.js"}"#,
    );
    write(
        &r,
        "node_modules/multiexp/index.js",
        "export const first = () => 1, second = () => 2;\nexport function other() { return 3; }\n",
    );
    let out = ok(&r, &["dep", "multiexp", "first"]);
    assert!(out.contains("first = () =>"), "{out}");
    let out = ok(&r, &["dep", "multiexp", "second"]);
    assert!(out.contains("second = () =>"), "{out}");

    // Module augmentation inside an external `.d.ts` must NOT leak as root exports.
    write(
        &r,
        "node_modules/augpkg/package.json",
        r#"{"name":"augpkg","version":"1.0.0","types":"index.d.ts"}"#,
    );
    write(
        &r,
        "node_modules/augpkg/index.d.ts",
        "export declare function real(): void;\ndeclare module \"other\" { interface AugOnly { x: number; } }\n",
    );
    let list_out = ok(&r, &["dep", "augpkg"]);
    assert!(
        list_out.contains("real") && !list_out.contains("AugOnly"),
        "{list_out}"
    );

    // `"type": "module"` entry with zero exports must not leak private functions OR suggest them.
    write(
        &r,
        "node_modules/esmzero/package.json",
        r#"{"name":"esmzero","version":"1.0.0","type":"module","main":"index.js"}"#,
    );
    write(
        &r,
        "node_modules/esmzero/index.js",
        "function privateOnly() { return 1; }\n",
    );
    let (out, _, code) = run(&r, &["dep", "esmzero", "privateOnly"]);
    assert_eq!(code, 1, "{out}");
    assert!(!out.contains("did you mean"), "{out}");

    // Nested `package.json` overrides root `"type"` in BOTH directions (CJS->ESM and ESM->CJS).
    write(
        &r,
        "node_modules/nested/package.json",
        r#"{"name":"nested","version":"1.0.0","exports":{"./esm":"./esm/index.js"}}"#,
    );
    write(
        &r,
        "node_modules/nested/esm/package.json",
        r#"{"type":"module"}"#,
    );
    write(
        &r,
        "node_modules/nested/esm/index.js",
        "function hiddenNested() { return 1; }\n",
    );
    let (out, _, code) = run(&r, &["dep", "nested/esm", "hiddenNested"]);
    assert_eq!(code, 1, "{out}");
    assert!(!out.contains("did you mean"), "{out}");

    write(
        &r,
        "node_modules/nested2/package.json",
        r#"{"name":"nested2","version":"1.0.0","type":"module","exports":{"./cjs":"./cjs/index.js"}}"#,
    );
    write(
        &r,
        "node_modules/nested2/cjs/package.json",
        r#"{"type":"commonjs"}"#,
    );
    write(
        &r,
        "node_modules/nested2/cjs/index.js",
        "function cjsPublic() { return 2; }\nmodule.exports = { cjsPublic };\n",
    );
    let out = ok(&r, &["dep", "nested2/cjs", "cjsPublic"]);
    assert!(out.contains("cjsPublic"), "{out}");
}

#[test]
fn recheck_06_py_dotted_lookup_stays_in_module() {
    let r = fresh("rc06");
    write(&r, ".venv/pyvenv.cfg", "version = 3.11.0\n");
    write(
        &r,
        ".venv/Lib/site-packages/alpha/__init__.py",
        "def root_only(): ...\n",
    );
    write(
        &r,
        ".venv/Lib/site-packages/alpha/real/__init__.py",
        "def ok(): ...\n",
    );
    write(
        &r,
        ".venv/Lib/site-packages/alpha/real/unimported.py",
        "class DraggedIn:\n    def method(self): ...\n",
    );
    // alpha/solo.py is a non-package file; alpha/solo/ghost.py must NOT be importable.
    write(
        &r,
        ".venv/Lib/site-packages/alpha/solo.py",
        "def solo_fn(): ...\n",
    );
    write(
        &r,
        ".venv/Lib/site-packages/alpha/solo/ghost.py",
        "def ghost_fn(): ...\n",
    );
    write(
        &r,
        ".venv/Lib/site-packages/solo.pyi",
        "def exported() -> int: ...\n",
    );
    let list_out = ok(&r, &["dep", "alpha.real"]);
    assert!(
        list_out.contains("ok") && !list_out.contains("root_only"),
        "{list_out}"
    );
    let (out, _, code) = run(&r, &["dep", "alpha.real", "DraggedIn.method"]);
    assert_eq!(code, 1, "{out}");
    let (out, _, code) = run(&r, &["dep", "alpha.solo.ghost", "ghost_fn"]);
    assert_eq!(code, 1, "{out}");
    // `alpha.solo ghost` must NOT report `ghost` as a submodule when `alpha/solo.py` blocks it.
    let (out, _, code) = run(&r, &["dep", "alpha.solo", "ghost"]);
    assert_eq!(code, 1, "{out}");
    let (out, _, code) = run(&r, &["dep", "solo.fake", "exported"]);
    assert_eq!(code, 1, "{out}");

    // Shadowed regular package: venv defines `shadow/__init__.py` (no `sys_only`), system site
    // also has `shadow/__init__.py` + `shadow/sys_only.py`. Python raises ModuleNotFoundError.
    let sys_home = r.join("sys_py");
    write(
        &r,
        ".venv/pyvenv.cfg",
        &format!(
            "home = {}\ninclude-system-site-packages = true\nversion = 3.11.0\n",
            sys_home.display()
        ),
    );
    write(
        &r,
        ".venv/Lib/site-packages/shadow/__init__.py",
        "def venv_fn(): ...\n",
    );
    write(
        &sys_home,
        "Lib/site-packages/shadow/__init__.py",
        "def sys_fn(): ...\n",
    );
    write(
        &sys_home,
        "Lib/site-packages/shadow/sys_only.py",
        "def leaked(): ...\n",
    );
    let (out, _, code) = run(&r, &["dep", "shadow.sys_only", "leaked"]);
    assert_eq!(code, 1, "{out}");
    let (out, _, code) = run(&r, &["dep", "shadow", "sys_only"]);
    assert_eq!(code, 1, "{out}");

    // PEP 420: venv has namespace dir `demo/ghost.py` (no __init__.py), system site has regular
    // `demo/__init__.py`. `tc dep demo` must select the regular system package, NOT list `ghost`.
    write(
        &r,
        ".venv/Lib/site-packages/demo/ghost.py",
        "def ghost_only(): ...\n",
    );
    write(
        &sys_home,
        "Lib/site-packages/demo/__init__.py",
        "def real_sys(): ...\n",
    );
    let list_demo = ok(&r, &["dep", "demo"]);
    assert!(
        list_demo.contains("real_sys") && !list_demo.contains("ghost"),
        "{list_demo}"
    );

    // PYTHONNOUSERSITE parsing + platform-specific PYTHONUSERBASE + APPDATA fallback + no-venv PATH.
    let user_base = r.join("custom_user_base");
    #[cfg(windows)]
    let (native_ub, other_ub) = (
        "Python311/site-packages/ubpkg/__init__.py",
        "lib/python3.11/site-packages/otheronly/__init__.py",
    );
    #[cfg(not(windows))]
    let (native_ub, other_ub) = (
        "lib/python3.11/site-packages/ubpkg/__init__.py",
        "Python311/site-packages/otheronly/__init__.py",
    );
    write(&user_base, native_ub, "def from_userbase() -> int: ...\n");
    write(&user_base, other_ub, "def from_other() -> int: ...\n");
    for zero in ["0", " 0", "00", "+0", "-0"] {
        let o = tc()
            .env("PYTHONNOUSERSITE", zero)
            .env("PYTHONUSERBASE", &user_base)
            .args([
                "--no-stats",
                "--cwd",
                r.to_str().unwrap(),
                "dep",
                "ubpkg",
                "from_userbase",
            ])
            .output()
            .unwrap();
        let out = String::from_utf8_lossy(&o.stdout);
        assert!(
            o.status.success() && out.contains("from_userbase"),
            "zero={zero:?}: {out}"
        );
    }
    // Wrong-platform layout inside PYTHONUSERBASE must not be picked up.
    let o_other = tc()
        .env("PYTHONNOUSERSITE", "0")
        .env("PYTHONUSERBASE", &user_base)
        .args([
            "--no-stats",
            "--cwd",
            r.to_str().unwrap(),
            "dep",
            "otheronly",
            "from_other",
        ])
        .output()
        .unwrap();
    assert!(!o_other.status.success());

    // Nonzero, trailing whitespace ("0 "), whitespace-only (" "), or Unicode space ("\u{2028}0")
    // disables user site (CPython uses ASCII isspace only).
    for disable in ["1", "0 ", " ", "\u{2028}0"] {
        let o2 = tc()
            .env("PYTHONNOUSERSITE", disable)
            .env("PYTHONUSERBASE", &user_base)
            .args([
                "--no-stats",
                "--cwd",
                r.to_str().unwrap(),
                "dep",
                "ubpkg",
                "from_userbase",
            ])
            .output()
            .unwrap();
        assert!(!o2.status.success(), "disable={disable:?} should fail");
    }

    // Windows: APPDATA="" falls back to USERPROFILE, then HOME, then HOMEDRIVE+HOMEPATH.
    #[cfg(windows)]
    {
        let prof = r.join("fake_profile");
        write(
            &prof,
            "Python/Python311/site-packages/homepkg/__init__.py",
            "def from_home() -> int: ...\n",
        );
        let o_home = tc()
            .env("APPDATA", "")
            .env("USERPROFILE", &prof)
            .env_remove("PYTHONUSERBASE")
            .env_remove("PYTHONNOUSERSITE")
            .args([
                "--no-stats",
                "--cwd",
                r.to_str().unwrap(),
                "dep",
                "homepkg",
                "from_home",
            ])
            .output()
            .unwrap();
        let out_h = String::from_utf8_lossy(&o_home.stdout);
        assert!(
            o_home.status.success() && out_h.contains("from_home"),
            "{out_h}"
        );
        let prof_s = prof.to_string_lossy();
        if let Some((drive, rest)) = prof_s.split_once(':') {
            let o_hd = tc()
                .env("APPDATA", "")
                .env_remove("USERPROFILE")
                .env_remove("HOME")
                .env("HOMEDRIVE", format!("{drive}:"))
                .env("HOMEPATH", rest)
                .env_remove("PYTHONUSERBASE")
                .env_remove("PYTHONNOUSERSITE")
                .args([
                    "--no-stats",
                    "--cwd",
                    r.to_str().unwrap(),
                    "dep",
                    "homepkg",
                    "from_home",
                ])
                .output()
                .unwrap();
            let out_hd = String::from_utf8_lossy(&o_hd.stdout);
            assert!(
                o_hd.status.success() && out_hd.contains("from_home"),
                "{out_hd}"
            );
        }
    }

    // Outside any venv: portable fake Python 3.11 on PATH (even behind a launcher shim) resolves.
    let novenv = fresh("rc06_novenv");
    let fake_py = novenv.join("fake_py");
    #[cfg(windows)]
    let fake_path_dir = {
        let shim = novenv.join("shim");
        write(&shim, "python.exe", "");
        write(&fake_py, "python.exe", "");
        write(&fake_py, "python311.dll", "");
        std::env::join_paths([&shim, &fake_py]).unwrap()
    };
    #[cfg(not(windows))]
    let fake_path_dir = {
        use std::os::unix::fs::PermissionsExt;
        write(&fake_py, "bin/python", "#!/bin/sh\n");
        fs::set_permissions(
            fake_py.join("bin/python"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::create_dir_all(fake_py.join("lib/python3.11")).unwrap();
        std::ffi::OsString::from(fake_py.join("bin"))
    };
    let mut cmd3 = tc();
    cmd3.env_remove("VIRTUAL_ENV")
        .env("PATH", &fake_path_dir)
        .env("PYTHONNOUSERSITE", "0")
        .env("PYTHONUSERBASE", &user_base);
    #[cfg(windows)]
    cmd3.env("PATHEXT", ".COM;.EXE;.BAT;.CMD");
    let o3 = cmd3
        .args([
            "--no-stats",
            "--cwd",
            novenv.to_str().unwrap(),
            "dep",
            "ubpkg",
            "from_userbase",
        ])
        .output()
        .unwrap();
    let out3 = String::from_utf8_lossy(&o3.stdout);
    assert!(
        o3.status.success() && out3.contains("from_userbase"),
        "{out3}"
    );
}

#[test]
fn recheck_07_rust_dual_direct_versions_flagged_ambiguous() {
    let r = fresh("rc07");
    let home = r.join("cargo_home");
    write(
        &r,
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\ndup1 = { package = \"dup\", version = \"1\" }\ndup2 = { package = \"dup\", version = \"2\" }\n",
    );
    write(
        &r,
        "Cargo.lock",
        "version = 3\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"dup 1.0.0\",\n \"dup 2.0.0\",\n]\n\n[[package]]\nname = \"dup\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n[[package]]\nname = \"dup\"\nversion = \"2.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
    );
    write(
        &home,
        "registry/src/index/dup-1.0.0/src/lib.rs",
        "pub fn ping() -> u8 { 1 }\n",
    );
    write(
        &home,
        "registry/src/index/dup-2.0.0/src/lib.rs",
        "pub fn ping() -> u16 { 2 }\n",
    );
    let o = tc()
        .env("CARGO_HOME", &home)
        .args([
            "--no-stats",
            "--cwd",
            r.to_str().unwrap(),
            "dep",
            "dup",
            "ping",
        ])
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success(), "{out}");
    assert!(
        out.contains("ambiguous") && out.contains("2.0.0") && out.contains("1.0.0"),
        "{out}"
    );
}

#[test]
fn recheck_08_mcp_rejects_fractional_id_scalar_params_and_null_reset() {
    let r = fresh("rc08");
    let mut child = tc()
        .args(["--no-stats", "--cwd", r.to_str().unwrap(), "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let reqs = concat!(
        r#"{"jsonrpc":"2.0","id":1.5,"method":"ping"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"ping","params":"bad"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"turbo_stats","arguments":{"reset":null}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"turbo_stats","arguments":null}}"#,
        "\n",
    );
    stdin.write_all(reqs.as_bytes()).unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert_eq!(lines[0]["error"]["code"], -32600);
    assert_eq!(lines[1]["error"]["code"], -32602);
    assert_eq!(lines[2]["error"]["code"], -32602);
    assert_eq!(lines[3]["error"]["code"], -32602);
}

#[test]
fn recheck_09_json_budget_does_not_count_cut_files() {
    let r = fresh("rc09");
    write(
        &r,
        "package.json",
        &format!(r#"{{"name":"demo","description":"{}"}}"#, "m".repeat(500)),
    );
    write(
        &r,
        "src/big.ts",
        &format!("export function f() {{ return '{}'; }}\n", "x".repeat(400)),
    );
    let stats_file = r.join("s.jsonl");
    let o = tc()
        .env("TC_STATS_FILE", &stats_file)
        .args([
            "--json",
            "--cwd",
            r.to_str().unwrap(),
            "zoom",
            "src",
            "--budget",
            "20",
        ])
        .output()
        .unwrap();
    assert!(o.status.success());
    let j: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let rec: serde_json::Value =
        serde_json::from_str(fs::read_to_string(&stats_file).unwrap().trim()).unwrap();
    if j.get("files")
        .and_then(|f| f.as_array())
        .is_none_or(|a| a.is_empty())
    {
        assert_eq!(rec["baseline_tokens"], 0, "json={j}\nrec={rec}");
    }
    // L0 overview capped so `manifests` is empty must also log 0 baseline_tokens.
    let s0 = r.join("s0.jsonl");
    let o0 = tc()
        .env("TC_STATS_FILE", &s0)
        .args([
            "--json",
            "--cwd",
            r.to_str().unwrap(),
            "zoom",
            ".",
            "--budget",
            "35",
        ])
        .output()
        .unwrap();
    assert!(o0.status.success());
    let j0: serde_json::Value = serde_json::from_slice(&o0.stdout).unwrap();
    let rec0: serde_json::Value =
        serde_json::from_str(fs::read_to_string(&s0).unwrap().trim()).unwrap();
    if j0
        .get("manifests")
        .and_then(|m| m.as_array())
        .is_none_or(|a| a.is_empty())
    {
        assert_eq!(rec0["baseline_tokens"], 0, "json={j0}\nrec={rec0}");
    }
}

#[test]
fn recheck_10_multi_arrow_and_combining_mark() {
    let r = fresh("rc10");
    write(
        &r,
        "src/multi.ts",
        "export const first = () => 1, second = () => 2;\n",
    );
    write(
        &r,
        "src/caller.ts",
        "const f\u{0301} = 1;\nconst か\u{3099} = 2;\nconst か\u{309B} = 3;\nexport const ok = first();\n",
    );
    let out = ok(&r, &["zoom", "src/multi.ts:first"]);
    assert!(
        out.contains("first = () => 1") && !out.contains("second = () => 2"),
        "{out}"
    );
    write(
        &r,
        "src/fn.ts",
        "export function f() { return 1; }\nexport function か() { return 2; }\n",
    );
    let out = ok(&r, &["zoom", "src/fn.ts:f"]);
    assert!(!out.contains("src/caller.ts:1"), "{out}");
    let out = ok(&r, &["zoom", "src/fn.ts:か"]);
    assert!(
        !out.contains("src/caller.ts:2") && !out.contains("src/caller.ts:3"),
        "{out}"
    );
}

#[cfg(windows)]
#[test]
fn windows_launcher_tags_and_user_sites() {
    let r = fresh("win_pytags");
    let shim = r.join("shim");
    let local = r.join("local");
    let appdata = r.join("appdata");
    write(&shim, "python.exe", "");
    // 3.14-64 (highest version), 3.13t-64 (free-threaded), 3.13-32 (32-bit), 3.13-64 (64-bit).
    write(&local, "Python/pythoncore-3.14-64/python.exe", "");
    write(&local, "Python/pythoncore-3.14-64/python314.dll", "");
    write(
        &local,
        "Python/pythoncore-3.14-64/Lib/site-packages/pkg314/__init__.py",
        "def from_314() -> int: ...\n",
    );
    write(&local, "Python/pythoncore-3.13t-64/python3.13t.exe", "");
    write(&local, "Python/pythoncore-3.13t-64/python313t.dll", "");
    write(
        &local,
        "Python/pythoncore-3.13t-64/Lib/site-packages/pkg313t/__init__.py",
        "def from_313t() -> int: ...\n",
    );
    write(
        &appdata,
        "Python/Python313t/site-packages/user313t/__init__.py",
        "def user_ft() -> int: ...\n",
    );
    write(&local, "Python/pythoncore-3.13-32/python.exe", "");
    write(&local, "Python/pythoncore-3.13-32/python313.dll", "");
    write(
        &local,
        "Python/pythoncore-3.13-32/Lib/site-packages/pkg313_32/__init__.py",
        "def from_32() -> int: ...\n",
    );
    write(
        &appdata,
        "Python/Python313-32/site-packages/user313_32/__init__.py",
        "def user_32() -> int: ...\n",
    );
    write(&local, "Python/pythoncore-3.13-64/python.exe", "");
    write(&local, "Python/pythoncore-3.13-64/python313.dll", "");
    write(
        &local,
        "Python/pythoncore-3.13-64/Lib/site-packages/pkg313_64/__init__.py",
        "def from_64() -> int: ...\n",
    );

    let probe = |env_k: &str, env_v: &str, pkg: &str| {
        tc().env("PATH", &shim)
            .env("LOCALAPPDATA", &local)
            .env("APPDATA", &appdata)
            .env("ProgramFiles", r.join("empty_pf"))
            .env("ProgramFiles(x86)", r.join("empty_pf86"))
            .env("SystemDrive", r.join("empty_sd"))
            .env("TC_NO_WIN_REGISTRY", "1")
            .env_remove("PYTHONNOUSERSITE")
            .env_remove("PYTHONUSERBASE")
            .env(env_k, env_v)
            .args(["--no-stats", "--cwd", r.to_str().unwrap(), "dep", pkg])
            .output()
            .unwrap()
    };

    // 3.13t selects free-threaded runtime + Python313t user site, never falls through to 3.14.
    let o = probe("PYTHON_MANAGER_DEFAULT", "3.13t", "pkg313t");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let o = probe("PYTHON_MANAGER_DEFAULT", "3.13t", "user313t");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let o = probe("PYTHON_MANAGER_DEFAULT", "3.13t", "pkg314");
    assert_eq!(o.status.code(), Some(2));

    // 3.13-32 selects 32-bit runtime + Python313-32 user site, not 64-bit 3.13.
    let o = probe("PYTHON_MANAGER_DEFAULT", "3.13-32", "pkg313_32");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let o = probe("PYTHON_MANAGER_DEFAULT", "3.13-32", "user313_32");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let o = probe("PYTHON_MANAGER_DEFAULT", "3.13-32", "pkg313_64");
    assert_eq!(o.status.code(), Some(2));

    // PY_PYTHON=3.13-64 selects 64-bit 3.13; unmatched/non-ASCII tags return exit 2 (never panic).
    let o = probe("PY_PYTHON", "3.13-64", "pkg313_64");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let o = probe("PYTHON_MANAGER_DEFAULT", "3.99", "pkg314");
    assert_eq!(o.status.code(), Some(2));
    let o = probe("PY_PYTHON", "PythonCoreé/3.11", "pkg314");
    assert_eq!(o.status.code(), Some(2));

    // Shared folder with multiple versions + free-threaded builds (3.12, 3.12t, 3.15t).
    write(&local, "Programs/Python/Python312/python.exe", "");
    write(&local, "Programs/Python/Python312/python3.12t.exe", "");
    write(&local, "Programs/Python/Python312/python3.15t.exe", "");
    write(&local, "Programs/Python/Python312/python312.dll", "");
    write(&local, "Programs/Python/Python312/python312t.dll", "");
    write(&local, "Programs/Python/Python312/python315t.dll", "");
    write(
        &appdata,
        "Python/Python312t/site-packages/user312t/__init__.py",
        "def dual_ft() -> int: ...\n",
    );
    write(
        &appdata,
        "Python/Python315t/site-packages/user315t_shared/__init__.py",
        "def shared_315t() -> int: ...\n",
    );
    let o = probe("PYTHON_MANAGER_DEFAULT", "3.12t", "user312t");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let o = probe("PYTHON_MANAGER_DEFAULT", "3.15t", "user315t_shared");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

#[test]
fn dep_bin_relative_path_with_different_cwd() {
    let r = fresh("rel_path_bin");
    let work = r.join("work");
    fs::create_dir_all(&work).unwrap();
    #[cfg(windows)]
    let (script_rel, cmd_name, body) = (
        "relbin/mytool.cmd",
        "mytool",
        "@echo off\r\necho Usage: mytool --port NUM\r\n",
    );
    #[cfg(not(windows))]
    let (script_rel, cmd_name, body) = (
        "relbin/mytool",
        "mytool",
        "#!/bin/sh\necho 'Usage: mytool --port NUM'\n",
    );
    write(&r, script_rel, body);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(r.join(script_rel), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let sep = if cfg!(windows) { ";" } else { ":" };
    let path_val = format!("relbin{sep}{}", std::env::var("PATH").unwrap_or_default());
    let o = tc()
        .current_dir(&*r)
        .env("PATH", path_val)
        .args([
            "--no-stats",
            "--cwd",
            work.to_str().unwrap(),
            "dep",
            "--bin",
            cmd_name,
            "--flag",
            "--port",
        ])
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(String::from_utf8_lossy(&o.stdout).contains("--port"));
}

// ---------------------------------------------------------------- member lines, .d.ts shape, zoom --filter

#[test]
fn dep_member_lines_in_text_and_json_ts_python() {
    let r = project();
    // TS: each member carries its own line; the dotted lookup's header is the member's line.
    let out = ok(&r, &["dep", "@xterm/xterm", "Terminal"]);
    assert!(
        out.contains("  [:4] write(data: string): void  // Writes data."),
        "{out}"
    );
    let out = ok(&r, &["dep", "@xterm/xterm", "Terminal.write"]);
    assert!(out.contains("typings/terminal.d.ts:4]"), "{out}");
    let v: Value =
        serde_json::from_str(&ok(&r, &["--json", "dep", "@xterm/xterm", "Terminal"])).unwrap();
    assert_eq!(v["hits"][0]["line"], 2);
    assert_eq!(v["hits"][0]["members"][0]["name"], "write");
    assert_eq!(v["hits"][0]["members"][0]["line"], 4);
    // Python: `__init__` first, then by line.
    let out = ok(&r, &["dep", "fastlib", "Engine"]);
    assert!(
        out.contains("  [:4] def __init__(self, n: int)")
            && out.contains("  [:6] def run(self) -> int"),
        "{out}"
    );
    let v: Value = serde_json::from_str(&ok(&r, &["--json", "dep", "fastlib", "Engine"])).unwrap();
    let lines: Vec<&Value> = v["hits"][0]["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| &m["line"])
        .collect();
    assert_eq!(lines, [&json!(4), &json!(6)], "{v}");
}

#[test]
fn dep_member_lines_rust_trait() {
    let r = fresh("rsmembers");
    write(
        &r,
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    );
    write(
        &r,
        "Cargo.lock",
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"ptyx\",\n]\n\n[[package]]\nname = \"ptyx\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
    );
    write(
        &r,
        "cargo/registry/src/index.crates.io-0000/ptyx-0.1.0/src/lib.rs",
        "/// The master end.\npub trait MasterPty: Send {\n    /// Resize it.\n    fn resize(&self, rows: u16) -> Result<(), String>;\n    fn get_size(&self) -> u16;\n}\n",
    );
    let dep = |args: &[&str]| {
        let o = tc()
            .env("CARGO_HOME", r.join("cargo"))
            .args(["--no-stats", "--cwd"])
            .arg(&*r)
            .args(args)
            .output()
            .unwrap();
        assert!(o.status.success(), "{args:?}: {o:?}");
        String::from_utf8_lossy(&o.stdout).to_string()
    };
    let out = dep(&["dep", "ptyx", "MasterPty"]);
    assert!(
        out.contains("pub trait MasterPty: Send  [ptyx-0.1.0/src/lib.rs:2]"),
        "{out}"
    );
    assert!(
        out.contains("  [:4] fn resize(&self, rows: u16) -> Result<(), String>;  // Resize it.")
            && out.contains("  [:5] fn get_size(&self) -> u16;"),
        "{out}"
    );
    let out = dep(&["dep", "ptyx", "MasterPty.resize"]);
    assert!(out.contains("ptyx-0.1.0/src/lib.rs:4]"), "{out}");
    let v: Value = serde_json::from_str(&dep(&["--json", "dep", "ptyx", "MasterPty"])).unwrap();
    assert_eq!(v["hits"][0]["members"][1]["line"], 5, "{v}");
}

#[test]
fn dep_dts_class_keeps_private_fields_and_names_its_export_statement() {
    let r = fresh("dtsshape");
    write(
        &r,
        "node_modules/chan/package.json",
        r#"{"name":"chan","version":"2.0.0","types":"core.d.ts"}"#,
    );
    write(
        &r,
        "node_modules/chan/core.d.ts",
        "declare class Channel<T = unknown> {\n    #private;\n    id: number;\n    private cleanupCallback;\n    protected guard: number;\n    toJSON(): string;\n}\ndeclare function invoke(cmd: string): Promise<void>;\nexport declare function inline(): void;\nexport { Channel, invoke };\nexport { invoke as call };\n",
    );
    let out = ok(&r, &["dep", "chan", "Channel"]);
    assert!(
        out.contains("declare class Channel<T = unknown>  [chan/core.d.ts:1, export :10]"),
        "{out}"
    );
    for want in [
        "  [:2] #private",
        "  [:3] id: number",
        "  [:4] private cleanupCallback",
        "  [:5] protected guard: number",
        "  [:6] toJSON(): string",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
    assert!(
        ok(&r, &["dep", "chan", "invoke"]).contains("[chan/core.d.ts:8, export :10]"),
        "named export of a function"
    );
    // An alias points at the statement that exports that alias.
    let out = ok(&r, &["dep", "chan", "call"]);
    assert!(out.contains("[chan/core.d.ts:8, export :11]"), "{out}");
    // An inline `export` needs no second location.
    let out = ok(&r, &["dep", "chan", "inline"]);
    assert!(out.contains("[chan/core.d.ts:9]"), "{out}");
    let v: Value = serde_json::from_str(&ok(&r, &["--json", "dep", "chan", "Channel"])).unwrap();
    assert_eq!(v["hits"][0]["export_line"], 10, "{v}");
    let names: Vec<&str> = v["hits"][0]["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"#private") && names.contains(&"cleanupCallback"),
        "{v}"
    );
    let v: Value = serde_json::from_str(&ok(&r, &["--json", "dep", "chan", "inline"])).unwrap();
    assert!(v["hits"][0].get("export_line").is_none(), "{v}");
    // Runtime source (not a declaration file) still hides private members.
    write(
        &r,
        "node_modules/rt/package.json",
        r#"{"name":"rt","version":"1.0.0","types":"index.ts"}"#,
    );
    write(
        &r,
        "node_modules/rt/index.ts",
        "export class K {\n  private secret = 1;\n  #hidden = 2;\n  open(): void {}\n}\n",
    );
    let out = ok(&r, &["dep", "rt", "K"]);
    assert!(out.contains("  [:4] open(): void"), "{out}");
    assert!(!out.contains("secret") && !out.contains("#hidden"), "{out}");
}

/// `project()` plus a TS class with a nested match and a Rust `impl` of a type from another file.
fn filter_project() -> Fixture {
    let r = project();
    write(
        &r,
        "src/shapes.ts",
        "export class Shape {\n  resize(n: number): void {}\n  draw(): void {}\n}\nexport function resizeAll(): void {}\nexport function other(): void {}\nfunction privateResize(): void {}\n",
    );
    write(
        &r,
        "rs/src/ext.rs",
        "use crate::Point;\n\nimpl Point {\n    pub fn scale(&self) -> i32 {\n        self.x\n    }\n}\n",
    );
    r
}

#[test]
fn zoom_filter_l1_shows_matching_files_and_container_headers() {
    let r = filter_project();
    let out = ok(&r, &["zoom", "src", "--filter", "RESIZE"]);
    assert!(
        out.contains("matching \"resize\" in 1 files")
            && out.contains("src/shapes.ts")
            && out.contains("  1: class Shape")
            && out.contains("    2: resize(n: number): void")
            && out.contains("  5: function resizeAll(): void")
            && out.contains("  7: function privateResize(): void"),
        "{out}"
    );
    assert!(
        !out.contains("draw") && !out.contains("other") && !out.contains("src/util.ts"),
        "{out}"
    );
    let v: Value =
        serde_json::from_str(&ok(&r, &["--json", "zoom", "src", "--filter", "resize"])).unwrap();
    assert_eq!(v["filter"], "resize");
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "{v}");
    let names: Vec<&str> = files[0]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Shape", "resize", "resizeAll", "privateResize"]);
    // A miss is a clean, empty answer.
    let out = ok(&r, &["zoom", "src", "--filter", "zzqq"]);
    assert!(out.contains("(no declaration matches \"zzqq\")"), "{out}");
    // A blank pattern is no filter (as an empty MCP string is).
    assert_eq!(
        ok(&r, &["zoom", "src", "--filter", "  "]),
        ok(&r, &["zoom", "src"])
    );
    // A filtered L1 searches every declaration, class fields included, not just the exported API.
    write(&r, "src/cfg.ts", "class Cfg {\n  gridLimit = 3;\n}\n");
    let out = ok(&r, &["zoom", "src", "--filter", "gridlimit"]);
    assert!(
        out.contains("src/cfg.ts")
            && out.contains("  1: class Cfg")
            && out.contains("    2: gridLimit = 3"),
        "{out}"
    );
    assert!(!ok(&r, &["zoom", "src"]).contains("gridLimit"));
}

#[test]
fn zoom_filter_l15_keeps_matching_symbols_and_imports() {
    let r = filter_project();
    let out = ok(&r, &["zoom", "src/main.ts", "--filter", "terminal"]);
    assert!(
        out.contains("8 lines, 1 symbols, 1 matching \"terminal\"")
            && out.contains("@xterm/xterm → Terminal  [@xterm/xterm 9.9.9]")
            && out.contains("4-8 export function run(t: Terminal): number"),
        "{out}"
    );
    assert!(!out.contains("./util"), "unrelated import listed:\n{out}");
    // No matching import: no imports section at all.
    let out = ok(&r, &["zoom", "src/shapes.ts", "--filter", "draw"]);
    assert!(!out.contains("imports:"), "{out}");
    assert!(
        out.contains("  1-4 export class Shape") && out.contains("    3-3 method draw(): void"),
        "{out}"
    );
    assert!(!out.contains("resize"), "{out}");
    // Rust: an impl method keeps its struct header; one whose type lives elsewhere says its owner.
    let out = ok(&r, &["zoom", "rs/src/lib.rs", "--filter", "norm"]);
    assert!(
        out.contains("pub struct Point") && out.contains("pub fn norm(&self) -> i32"),
        "{out}"
    );
    assert!(!out.contains("add_one") && !out.contains("inner"), "{out}");
    let out = ok(&r, &["zoom", "rs/src/ext.rs", "--filter", "scale"]);
    assert!(
        out.contains("pub fn scale(&self) -> i32  (in Point)"),
        "{out}"
    );
    let v: Value = serde_json::from_str(&ok(
        &r,
        &["--json", "zoom", "src/main.ts", "--filter", "helper"],
    ))
    .unwrap();
    assert_eq!(v["filter"], "helper");
    assert_eq!(v["imports"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["imports"][0]["module"], "./util");
    assert!(v["symbols"].as_array().unwrap().is_empty(), "{v}");
}

#[test]
fn zoom_filter_is_rejected_on_overview_and_symbol_views() {
    let r = filter_project();
    for target in [".", "src/main.ts:run"] {
        let (out, err, code) = run(&r, &["zoom", target, "--filter", "run"]);
        assert_eq!(code, 2, "{target}: {out}");
        assert!(
            err.contains("--filter narrows a directory or a file"),
            "{err}"
        );
    }
}

#[test]
fn zoom_filter_budget_and_baseline_count_only_shown_files() {
    let r = filter_project();
    for target in ["src", "src/main.ts", "rs/src/lib.rs"] {
        for budget in [1usize, 5, 30, 100] {
            for json in [false, true] {
                let b = budget.to_string();
                let mut args = vec!["zoom", target, "--filter", "r", "--budget", &b];
                if json {
                    args.insert(0, "--json");
                }
                let out = ok(&r, &args);
                assert!(
                    tokens(&out) <= budget,
                    "{args:?}: {} tokens\n{out}",
                    tokens(&out)
                );
                if json {
                    serde_json::from_str::<Value>(&out)
                        .unwrap_or_else(|e| panic!("{args:?}: {e}\n{out}"));
                }
            }
        }
    }
    let ledger = r.join("ledger/stats.jsonl");
    let baseline = |args: &[&str]| {
        let o = tc()
            .env("TC_STATS_FILE", &ledger)
            .arg("--cwd")
            .arg(&*r)
            .args(args)
            .output()
            .unwrap();
        assert!(o.status.success(), "{args:?}");
        let last = fs::read_to_string(&ledger).unwrap();
        let e: Value = serde_json::from_str(last.lines().last().unwrap()).unwrap();
        e["baseline_tokens"].clone()
    };
    let shapes = fs::metadata(r.join("src/shapes.ts")).unwrap().len();
    for json in [false, true] {
        let pre: &[&'static str] = if json { &["--json"] } else { &[] };
        let with = |rest: &[&'static str]| [pre, rest].concat();
        assert_eq!(
            baseline(&with(&["zoom", "src", "--filter", "resize"])),
            json!(shapes.div_ceil(4)),
            "json={json}: only the matching file counts"
        );
        assert_eq!(
            baseline(&with(&["zoom", "src", "--filter", "zzqq"])),
            json!(0)
        );
        assert_eq!(
            baseline(&with(&["zoom", "src/shapes.ts", "--filter", "zzqq"])),
            json!(0),
            "json={json}: nothing shown"
        );
        assert_eq!(
            baseline(&with(&["zoom", "src/shapes.ts", "--filter", "draw"])),
            json!(shapes.div_ceil(4))
        );
    }
}

#[test]
fn zoom_filter_over_mcp_and_in_init_guidance() {
    let r = filter_project();
    let root = r.to_str().unwrap();
    let call = |id: u32, args: Value| {
        frame(
            json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"turbo_zoom","arguments":args}}),
        )
    };
    let mut input = frame(json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}));
    input.extend(call(
        2,
        json!({"target": "src", "filter": "resize", "cwd": root}),
    ));
    input.extend(call(3, json!({"target": "src", "filter": 5, "cwd": root})));
    input.extend(call(4, json!({"target": ".", "filter": "x", "cwd": root})));
    input.extend(call(5, json!({"target": "src", "filter": "", "cwd": root})));
    input.extend(call(6, json!({"target": "src", "cwd": root})));
    input.extend(call(
        7,
        json!({"target": "src/main.ts", "filter": "terminal", "cwd": root}),
    ));
    let out = mcp_session(&r, &[], &input);
    let by_id = |id: u32| out.iter().find(|v| v["id"] == id).cloned().unwrap();
    let zoom_tool = by_id(1)["result"]["tools"][0].clone();
    assert_eq!(
        zoom_tool["inputSchema"]["properties"]["filter"]["type"],
        "string"
    );
    let text = |id: u32| {
        by_id(id)["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(by_id(2)["result"]["isError"], false);
    assert!(
        text(2).contains("resizeAll") && !text(2).contains("src/util.ts"),
        "{}",
        text(2)
    );
    assert_eq!(by_id(3)["error"]["code"], -32602, "non-string filter");
    assert_eq!(by_id(4)["result"]["isError"], true);
    assert!(text(4).contains("--filter narrows"), "{}", text(4));
    assert_eq!(text(5), text(6), "empty filter = no filter");
    assert_eq!(by_id(7)["result"]["isError"], false);
    assert!(
        text(7).contains("@xterm/xterm → Terminal")
            && text(7).contains("export function run(t: Terminal)")
            && !text(7).contains("./util"),
        "{}",
        text(7)
    );
    // `tc init` tells agents about it.
    let i = fresh("initfilter");
    ok(&i, &["init", "--agents"]);
    let agents = fs::read_to_string(i.join("AGENTS.md")).unwrap();
    assert!(
        agents.contains("tc zoom <dir|file> --filter <text>"),
        "{agents}"
    );
    assert!(ok(&i, &["init", "--agents"]).contains("unchanged AGENTS.md"));
}

#[test]
fn zoom_filter_skips_initializers_and_folds_unicode_case() {
    let r = fresh("filterinit");
    write(
        &r,
        "lib/xterm.mjs",
        "var Gs=\"Terminal input\",mi={get:()=>Gs};\nexport function other() {}\n",
    );
    write(
        &r,
        "src/mode.ts",
        "export const foo: ResizeMode = \"x\";\nexport const bar = \"foo\";\n",
    );
    // `Gs="Terminal input"` has no spaces around `=`: still a value, not a signature.
    let out = ok(&r, &["zoom", "lib/xterm.mjs", "--filter", "Terminal input"]);
    assert!(
        out.contains("(no symbol matches \"terminal input\")"),
        "{out}"
    );
    let out = ok(&r, &["zoom", "lib", "--filter", "Terminal input"]);
    assert!(out.contains("(no declaration matches"), "{out}");
    for q in ["foo", "ResizeMode"] {
        let out = ok(&r, &["zoom", "src/mode.ts", "--filter", q]);
        assert!(
            out.contains("export const foo: ResizeMode = \"x\"") && !out.contains("bar"),
            "{q}: {out}"
        );
    }
    // `--filter=-x`: the equals form takes a pattern that starts with `-`.
    let out = ok(&r, &["zoom", "src/mode.ts", "--filter=-x"]);
    assert!(out.contains("(no symbol matches \"-x\")"), "{out}");
    // Full Unicode case folding on the pattern, names, signatures and imports.
    write(
        &r,
        "src/greek.ts",
        "import { \u{3A9}mega } from \"./mode\";\nexport const \u{39F}\u{3A3} = 1;\nexport function stra\u{DF}e(): void {}\nexport function \u{130}stanbul(): void {}\n",
    );
    let sigma = "export const \u{39F}\u{3A3} = 1";
    let strasse = "export function stra\u{DF}e(): void";
    let istanbul = "export function \u{130}stanbul(): void";
    for (q, want) in [
        ("\u{3A3}", sigma),
        ("\u{3C3}", sigma),
        ("\u{3C2}", sigma),
        ("STRASSE", strasse),
        ("\u{1E9E}", strasse),
        ("istanbul", istanbul),
        ("\u{131}STANBUL", istanbul),
    ] {
        let out = ok(&r, &["zoom", "src/greek.ts", "--filter", q]);
        assert!(
            out.contains(want) && out.contains("1 matching"),
            "{q}: {out}"
        );
        assert!(!out.contains("./mode"), "{q}: {out}");
    }
    let out = ok(&r, &["zoom", "src/greek.ts", "--filter", "\u{3C9}MEGA"]);
    assert!(out.contains("./mode → \u{3A9}mega"), "{out}");
}

#[test]
fn zoom_filter_rust_keeps_modules_and_finds_struct_fields() {
    let r = fresh("rsmods");
    write(
        &r,
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    );
    write(
        &r,
        "src/lib.rs",
        "/// Reports.\npub struct ReportState {\n    closed: bool,\n    pub emitted: u64,\n}\n\npub mod outer {\n    /// The commands that wait on a process or the disk run on the blocking pool under the same names and arguments as before.\n    mod commands {\n        use super::*;\n\n        #[tauri::command]\n        pub async fn pty_resize(id: String, cols: u16) -> Result<(), String> {\n            Ok(())\n        }\n    }\n}\n",
    );
    let full = ok(&r, &["zoom", "src/lib.rs", "--filter", "pty_resize"]);
    let doc = "The commands that wait on a process or the disk run on the blocking pool under the same names and arguments as before.";
    let hit = "  12-15 pub async fn pty_resize(id: String, cols: u16) -> Result<(), String>\n";
    let outer = "  7-17 pub mod outer\n";
    let commands = format!("  8-16 mod commands  // {doc}\n");
    assert!(full.contains("1 matching"), "{full}");
    assert!(
        full.contains(&format!("{outer}{commands}{hit}")),
        "module chain lost:\n{full}"
    );
    let full_l1 = ok(&r, &["zoom", "src", "--filter", "pty_resize"]);
    let outer_l1 = "  7: pub mod outer\n";
    let commands_l1 = format!("  9: mod commands  // {doc}\n");
    assert!(
        full_l1.contains(&format!(
            "{outer_l1}{commands_l1}  13: pub async fn pty_resize("
        )),
        "{full_l1}"
    );
    // Hit-only views: the text without the header lines, the JSON with the match alone.
    let hit_only = |args: &[&str], key: &dyn Fn(&mut Value) -> &mut Value| {
        let mut v: Value = serde_json::from_str(&ok(&r, args)).unwrap();
        let syms = key(&mut v);
        let only: Vec<Value> = syms
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["name"] == "pty_resize")
            .cloned()
            .collect();
        *syms = json!(only);
        v["truncated"] = json!(true);
        tokens(&(serde_json::to_string_pretty(&v).unwrap() + "\n"))
    };
    let t15 = ["zoom", "src/lib.rs", "--filter", "pty_resize"];
    let t1 = ["zoom", "src", "--filter", "pty_resize"];
    let j15 = ["--json", "zoom", "src/lib.rs", "--filter", "pty_resize"];
    let j1 = ["--json", "zoom", "src", "--filter", "pty_resize"];
    let cases: [(&[&str], usize, usize); 4] = [
        (
            &t15,
            tokens(&full.replace(outer, "").replace(&commands, "")),
            tokens(&full),
        ),
        (
            &t1,
            tokens(&full_l1.replace(outer_l1, "").replace(&commands_l1, "")),
            tokens(&full_l1),
        ),
        (
            &j15,
            hit_only(&j15, &|v| &mut v["symbols"]),
            tokens(&ok(&r, &j15)),
        ),
        (
            &j1,
            hit_only(&j1, &|v| &mut v["files"][0]["symbols"]),
            tokens(&ok(&r, &j1)),
        ),
    ];
    // At every budget a hit-only view fits, the hit shows: a match outranks its headers. From
    // the full view's own size up, all of it shows, with no room held back for a cut marker.
    for (args, bare, whole) in cases {
        assert!(bare < whole, "{args:?}: {bare} {whole}");
        let unbudgeted = ok(&r, args);
        for n in bare..=whole + 1 {
            let b = n.to_string();
            let mut a = args.to_vec();
            a.extend(["--budget", &b]);
            let out = ok(&r, &a);
            assert!(tokens(&out) <= n, "{a:?}: {} tokens", tokens(&out));
            assert!(
                out.contains("fn pty_resize") || out.contains("\"name\": \"pty_resize\""),
                "hit dropped at {a:?}:\n{out}"
            );
            if n >= whole {
                assert_eq!(out, unbudgeted, "{a:?}");
            }
        }
    }
    // Struct fields: found under their struct; at L1.5 all of them, at L1 only through a filter.
    let out = ok(&r, &["zoom", "src/lib.rs", "--filter", "emitted"]);
    assert!(
        out.contains("  1-5 pub struct ReportState  // Reports.\n    4-4 pub emitted: u64\n"),
        "{out}"
    );
    assert!(!out.contains("closed"), "{out}");
    let out = ok(&r, &["zoom", "src/lib.rs"]);
    assert!(
        out.contains("    3-3 field closed: bool") && out.contains("    4-4 pub emitted: u64"),
        "{out}"
    );
    assert!(!ok(&r, &["zoom", "src"]).contains("emitted"));
    assert!(ok(&r, &["zoom", "src", "--filter", "EMITTED"]).contains("4: pub emitted: u64"));
    // `tc dep`: a struct lists its plain-`pub` fields beside its methods, nothing private.
    write(
        &r,
        "Cargo.lock",
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"ptyx\",\n]\n\n[[package]]\nname = \"ptyx\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
    );
    write(
        &r,
        "cargo/registry/src/index.crates.io-0000/ptyx-0.1.0/src/lib.rs",
        "/// A size.\npub struct PtySize {\n    /// Rows.\n    pub rows: u16,\n    cols: u16,\n    pub(crate) px: u16,\n}\nimpl PtySize {\n    pub fn area(&self) -> u16 { 0 }\n}\n",
    );
    let o = tc()
        .env("CARGO_HOME", r.join("cargo"))
        .args(["--no-stats", "--cwd"])
        .arg(&*r)
        .args(["dep", "ptyx", "PtySize"])
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success(), "{o:?}");
    assert!(
        out.contains("pub struct PtySize  [ptyx-0.1.0/src/lib.rs:2]")
            && out.contains("  [:4] pub rows: u16  // Rows.")
            && out.contains("  [:9] pub fn area(&self) -> u16"),
        "{out}"
    );
    assert!(!out.contains("cols") && !out.contains("px"), "{out}");
}

#[test]
fn zoom_filter_dir_json_trims_symbols_and_keeps_hits_that_fit() {
    let r = fresh("fithits");
    let mut grid = format!("/** {} */\nexport class Grid {{\n", "Grid doc. ".repeat(30));
    for i in 0..30 {
        grid += &format!("  resize{i}(rows: number, cols: number): void {{}}\n");
    }
    grid += "}\n";
    write(&r, "src/a_grid.ts", &grid);
    write(
        &r,
        "src/b_more.ts",
        "export function resizeMore(): void {}\n",
    );
    let mut partial = None;
    for budget in (10..=700).step_by(5) {
        let b = budget.to_string();
        let args = [
            "--json", "zoom", "src", "--filter", "resize", "--budget", &b,
        ];
        let out = ok(&r, &args);
        assert!(tokens(&out) <= budget, "{args:?}: {} tokens", tokens(&out));
        let v: Value = serde_json::from_str(&out).unwrap();
        let Some(files) = v["files"].as_array() else {
            continue;
        };
        // An entry cut down to its `class Grid` header alone would show no hit.
        for f in files {
            let syms = f["symbols"].as_array().unwrap();
            assert!(
                syms.iter()
                    .any(|s| s["name"].as_str().unwrap().contains("resize")),
                "header-only entry at --budget {budget}: {v}"
            );
        }
        if let [f] = &files[..] {
            if (2..31).contains(&f["symbols"].as_array().unwrap().len()) {
                assert_eq!(v["truncated"], true, "{v}");
                partial.get_or_insert(budget);
            }
        }
    }
    let budget = partial
        .expect("no budget kept part of a file entry")
        .to_string();
    // The partly shown file counts toward the baseline, the left-out one doesn't.
    let ledger = r.join("ledger/stats.jsonl");
    let o = tc()
        .env("TC_STATS_FILE", &ledger)
        .arg("--cwd")
        .arg(&*r)
        .args([
            "--json", "zoom", "src", "--filter", "resize", "--budget", &budget,
        ])
        .output()
        .unwrap();
    assert!(o.status.success());
    let e: Value =
        serde_json::from_str(fs::read_to_string(&ledger).unwrap().lines().last().unwrap()).unwrap();
    let grid_bytes = fs::metadata(r.join("src/a_grid.ts")).unwrap().len();
    assert_eq!(e["baseline_tokens"], json!(grid_bytes.div_ceil(4)));
}

#[test]
fn dep_renamed_reexport_names_the_declaration_files_export_line() {
    let r = fresh("rexline");
    write(
        &r,
        "node_modules/rex/package.json",
        r#"{"name":"rex","version":"1.0.0","types":"index.d.ts"}"#,
    );
    write(
        &r,
        "node_modules/rex/leaf.d.ts",
        "declare class C {}\nexport { C as Other };\nexport { C as Selected };\n",
    );
    write(
        &r,
        "node_modules/rex/index.d.ts",
        "export { Selected as Public } from './leaf';\nexport { Other } from './leaf';\n",
    );
    let out = ok(&r, &["dep", "rex", "Public"]);
    assert!(
        out.contains("declare class C  [rex/leaf.d.ts:1, export :3]")
            && out.contains("(exported as `Public`, declared as `C`)"),
        "{out}"
    );
    let out = ok(&r, &["dep", "rex", "Other"]);
    assert!(out.contains("[rex/leaf.d.ts:1, export :2]"), "{out}");
    let v: Value = serde_json::from_str(&ok(&r, &["--json", "dep", "rex", "Public"])).unwrap();
    assert_eq!(v["hits"][0]["export_line"], 3, "{v}");
}

// ---------------------------------------------------------------- review 2 (P2) regressions

#[test]
fn budget_exact_fit_counts_chars_not_utf8_bytes() {
    let r = fresh("utf8budget");
    write(
        &r,
        "package.json",
        r#"{"name":"demo","scripts":{"dev":"echo café — été — déjà — très — prêt — ça"}}"#,
    );
    write(
        &r,
        "README.md",
        "# Demo\n\nÇa marche — déjà — très léger — été — prêt — à côté.\n",
    );
    write(
        &r,
        "src/pane.ts",
        "/** Pane — résumé — déjà vu — été. */\nexport class Pane {\n  /** Resize — coalesced — one fit per frame — été — déjà. */\n  resize(): void { this.draw(); }\n  /** Draw — é. */\n  draw(): void {}\n}\nexport function run(p: Pane): void { p.resize(); }\n",
    );
    let cases: [&[&str]; 6] = [
        &["zoom", "src/pane.ts", "--filter", "resize"],
        &["zoom", "src", "--filter", "resize"],
        &["zoom", "src/pane.ts"],
        &["zoom", "src"],
        &["zoom", "src/pane.ts:Pane.resize"],
        &["zoom", "."],
    ];
    for case in cases {
        for json in [false, true] {
            let at = |b: usize| {
                let b = b.to_string();
                let mut a = case.to_vec();
                if json {
                    a.insert(0, "--json");
                }
                a.extend(["--budget", &b]);
                ok(&r, &a)
            };
            let full = at(0);
            let b = tokens(&full);
            // The case only means something if the output's bytes overflow its own budget.
            assert!(
                full.len() > b * 4,
                "{case:?} json={json}: too little non-ASCII\n{full}"
            );
            assert_eq!(at(b), full, "{case:?} json={json}: exact fit was cut");
            let cut = at(b - 1);
            assert!(
                tokens(&cut) < b && cut != full,
                "{case:?} json={json}: {cut}"
            );
        }
    }
    // `tc stats` measures the same output the same way.
    let stats_file = r.join("s.jsonl");
    let args = ["zoom", "src/pane.ts", "--filter", "resize"];
    let b = tokens(&ok(&r, &args)).to_string();
    let o = tc()
        .env("TC_STATS_FILE", &stats_file)
        .arg("--cwd")
        .arg(&*r)
        .args(args)
        .args(["--budget", &b])
        .output()
        .unwrap();
    assert!(o.status.success());
    let rec: Value = serde_json::from_str(fs::read_to_string(&stats_file).unwrap().trim()).unwrap();
    assert_eq!(rec["actual_tokens"].to_string(), b, "{rec}");
}

#[test]
fn zoom_filter_rust_impl_in_a_mod_keeps_the_mod_and_its_type() {
    let r = fresh("rsimplmod");
    write(
        &r,
        "src/lib.rs",
        "pub struct Box<T>(T);\n\nmod error {\n    use super::Box;\n\n    impl<T: core::error::Error> core::error::Error for Box<T> {\n        fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {\n            None\n        }\n    }\n}\n",
    );
    let names = |args: &[&str], dir: bool| -> Vec<(String, u64)> {
        let v: Value = serde_json::from_str(&ok(&r, args)).unwrap();
        let syms = if dir {
            &v["files"][0]["symbols"]
        } else {
            &v["symbols"]
        };
        syms.as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s["name"].as_str().unwrap().into(),
                    s["line"].as_u64().unwrap(),
                )
            })
            .collect()
    };
    let want = vec![
        ("Box".to_string(), 1),
        ("error".to_string(), 3),
        ("source".to_string(), 7),
    ];
    let q = "fn source(";
    assert_eq!(
        names(&["--json", "zoom", "src/lib.rs", "--filter", q], false),
        want
    );
    assert_eq!(names(&["--json", "zoom", "src", "--filter", q], true), want);
    let out = ok(&r, &["zoom", "src/lib.rs", "--filter", q]);
    assert!(
        out.contains("  3-11 mod error\n    7-9 fn source(&self)"),
        "{out}"
    );
}

#[test]
fn cfg_twin_structs_keep_their_own_fields_in_dep_and_zoom() {
    let r = fresh("cfgtwins");
    write(
        &r,
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    );
    write(
        &r,
        "Cargo.lock",
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"cfgx\",\n]\n\n[[package]]\nname = \"cfgx\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
    );
    let crate_dir = r.join("cargo/registry/src/index.crates.io-0000/cfgx-0.1.0");
    write(
        &crate_dir,
        "src/lib.rs",
        "#[repr(C, packed(1))]\n#[cfg(target_arch = \"x86\")]\n#[derive(Clone, Copy)]\npub struct INFO {\n    pub Alpha: u16,\n    pub Beta: u16,\n}\n#[cfg(target_arch = \"x86\")]\nimpl Default for INFO {\n    fn default() -> Self { unsafe { core::mem::zeroed() } }\n}\n#[repr(C)]\n#[cfg(not(target_arch = \"x86\"))]\n#[derive(Clone, Copy)]\npub struct INFO {\n    pub Gamma: u16,\n    pub Delta: u16,\n}\n",
    );
    let dep = |args: &[&str]| {
        let o = tc()
            .env("CARGO_HOME", r.join("cargo"))
            .args(["--no-stats", "--cwd"])
            .arg(&*r)
            .args(args)
            .output()
            .unwrap();
        assert!(o.status.success(), "{args:?}: {o:?}");
        String::from_utf8_lossy(&o.stdout).to_string()
    };
    let out = dep(&["dep", "cfgx", "INFO"]);
    assert!(
        out.contains("pub struct INFO  [cfgx-0.1.0/src/lib.rs:4]")
            && out.contains(
                "  [:5] pub Alpha: u16\n  [:6] pub Beta: u16\n  [:10] fn default() -> Self"
            )
            && out.contains("also defined: cfgx-0.1.0/src/lib.rs:15 (struct)"),
        "{out}"
    );
    assert!(!out.contains("Gamma") && !out.contains("Delta"), "{out}");
    let v: Value = serde_json::from_str(&dep(&["--json", "dep", "cfgx", "INFO"])).unwrap();
    let members: Vec<&str> = v["hits"][0]["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(members, ["Alpha", "Beta", "default"], "{v}");
    // `zoom --filter`: a field's header is the struct around it, not the first of that name.
    for (q, want) in [("gamma", [15, 16]), ("alpha", [4, 5])] {
        let args = ["--json", "zoom", "src/lib.rs", "--filter", q];
        let v: Value = serde_json::from_str(&ok(&crate_dir, &args)).unwrap();
        let lines: Vec<u64> = v["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["line"].as_u64().unwrap())
            .collect();
        assert_eq!(lines, want, "{q}: {v}");
    }
}

#[test]
fn rust_dep_bare_name_is_the_reexported_item_not_an_associated_type() {
    // serde_json: `impl Visitor for X { type Value = ..; }` in shallow files must not stand in
    // for `pub enum Value` in `src/value/mod.rs`, re-exported by `pub use crate::value::Value;`.
    let r = fresh("rsassoctype");
    write(
        &r,
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    );
    write(
        &r,
        "Cargo.lock",
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"valx\",\n]\n\n[[package]]\nname = \"valx\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
    );
    let crate_dir = r.join("cargo/registry/src/index.crates.io-0000/valx-0.1.0");
    for (rel, body) in [
        (
            "src/lib.rs",
            "mod decoy;\nmod raw;\npub mod value;\npub use crate::value::{Map, Value};\n",
        ),
        (
            "src/raw.rs",
            "pub struct Raw;\npub trait Visitor {\n    type Value;\n}\nimpl Visitor for Raw {\n    type Value = Raw;\n}\n",
        ),
        // A private top-level `Value` shallower than the real one: only the re-export finds it.
        ("src/decoy.rs", "struct Value;\n"),
        (
            "src/value/de.rs",
            "pub struct ValueVisitor;\nimpl crate::raw::Visitor for ValueVisitor {\n    type Value = super::Value;\n}\n",
        ),
        (
            "src/value/mod.rs",
            "mod de;\npub struct Map;\n/// A JSON value.\npub enum Value {\n    Null,\n    Bool(bool),\n}\n",
        ),
    ] {
        write(&crate_dir, rel, body);
    }
    let o = tc()
        .env("CARGO_HOME", r.join("cargo"))
        .args(["--no-stats", "--cwd"])
        .arg(&*r)
        .args(["dep", "valx", "Value"])
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success(), "{o:?}");
    assert!(
        out.contains("pub enum Value  [valx-0.1.0/src/value/mod.rs:4]"),
        "{out}"
    );
    assert!(
        !out.contains("type Value") && !out.contains("raw.rs") && !out.contains("decoy.rs"),
        "{out}"
    );
}

#[test]
fn rust_impls_attach_by_cfg_and_module_path_in_dep_and_zoom() {
    let r = fresh("rsimplowners");
    write(
        &r,
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    );
    write(
        &r,
        "Cargo.lock",
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"owny\",\n]\n\n[[package]]\nname = \"owny\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
    );
    let crate_dir = r.join("cargo/registry/src/index.crates.io-0000/owny-0.1.0");
    write(
        &crate_dir,
        "src/lib.rs",
        r#"#[repr(C, packed(1))]
#[cfg(target_arch = "x86")]
#[derive(Clone, Copy)]
pub struct INFO {
    pub Alpha: u16,
}
#[cfg(target_arch = "x86")]
impl Default for INFO {
    fn default() -> Self { unsafe { core::mem::zeroed() } }
}
#[repr(C)]
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[derive(Clone, Copy)]
pub struct INFO {
    pub Gamma: u16,
}
#[cfg(any(target_arch="aarch64", target_arch="x86_64"))]
impl Default for INFO {
    fn default() -> Self { unsafe { core::mem::zeroed() } }
}
impl INFO {
    pub fn shared(&self) {}
}
pub struct Lone;
#[cfg(unix)]
impl Lone {
    pub fn only_unix() {}
}
pub struct Store;
pub mod legacy {
    pub struct Store;
    impl super::Store {
        pub fn up(&self) {}
    }
    impl self::Store {
        pub fn here(&self) {}
    }
    pub mod inner {
        impl super::super::Store {
            pub fn up2(&self) {}
        }
        impl super::other::Store {
            pub fn nowhere(&self) {}
        }
    }
}
"#,
    );
    // (name, line) of each hit's members.
    let members = |query: &str| -> Vec<Vec<(String, u64)>> {
        let o = tc()
            .env("CARGO_HOME", r.join("cargo"))
            .args(["--no-stats", "--json", "--cwd"])
            .arg(&*r)
            .args(["dep", "owny", query])
            .output()
            .unwrap();
        assert!(o.status.success(), "{query}: {o:?}");
        let v: Value = serde_json::from_slice(&o.stdout).unwrap();
        v["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| {
                h["members"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|m| {
                        (
                            m["name"].as_str().unwrap().into(),
                            m["line"].as_u64().unwrap(),
                        )
                    })
                    .collect()
            })
            .collect()
    };
    let m = |v: &[(&str, u64)]| -> Vec<(String, u64)> {
        v.iter().map(|(n, l)| (n.to_string(), *l)).collect()
    };
    // Each `#[cfg]` twin lists its own `default`; the cfg-less impl is shared.
    assert_eq!(
        members("INFO"),
        [
            m(&[("Alpha", 5), ("default", 9), ("shared", 22)]),
            m(&[("Gamma", 15), ("default", 19), ("shared", 22)]),
        ]
    );
    // A lone struct keeps its `#[cfg(unix)]` impl.
    assert_eq!(members("Lone"), [m(&[("only_unix", 27)])]);
    // `super::Store` / `super::super::Store` are the top one, `self::Store` the one in `legacy`;
    // `super::other::Store` is neither.
    assert_eq!(
        members("Store"),
        [m(&[("up", 33), ("up2", 40)]), m(&[("here", 36)])]
    );
    let names = |args: &[&str], dir: bool| -> Vec<(String, u64)> {
        let v: Value = serde_json::from_str(&ok(&crate_dir, args)).unwrap();
        let syms = if dir {
            &v["files"][0]["symbols"]
        } else {
            &v["symbols"]
        };
        syms.as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s["name"].as_str().unwrap().into(),
                    s["line"].as_u64().unwrap(),
                )
            })
            .collect()
    };
    let zoom = |q: &str| names(&["--json", "zoom", "src/lib.rs", "--filter", q], false);
    assert_eq!(
        zoom("fn default"),
        m(&[("INFO", 4), ("default", 9), ("INFO", 14), ("default", 19)])
    );
    // `zoom --filter` keeps the owner of a `super::` impl, in the file and the directory view.
    let want = m(&[("Store", 29), ("legacy", 30), ("up", 33)]);
    assert_eq!(zoom("fn up("), want);
    let dir = ["--json", "zoom", "src", "--filter", "fn up("];
    assert_eq!(names(&dir, true), want);
    let out = ok(&crate_dir, &["zoom", "src/lib.rs", "--filter", "fn up("]);
    assert!(
        out.contains("  29-29 pub struct Store;\n")
            && out.contains("    33-33 pub fn up(&self)\n")
            && !out.contains("(in "),
        "{out}"
    );
    assert_eq!(
        zoom("nowhere"),
        m(&[("legacy", 30), ("inner", 38), ("nowhere", 43)])
    );
    let out = ok(&crate_dir, &["zoom", "src/lib.rs", "--filter", "nowhere"]);
    assert!(out.contains("pub fn nowhere(&self)  (in Store)"), "{out}");
}

#[test]
fn zoom_filter_multi_declarator_fields_match_only_their_own_name() {
    let r = fresh("multidecl");
    write(
        &r,
        "android/src/com/local/loom/Link.java",
        "package com.local.loom;\n\npublic final class Link {\n  public static final class App {\n    final String name;\n    final int port, tailnet;\n    int a = 1, b = 2;\n  }\n}\n",
    );
    write(&r, "src/net.ts", "export const port = 1, tailnet = 2;\n");
    let names = |file: &str, q: &str| -> Vec<String> {
        let v: Value =
            serde_json::from_str(&ok(&r, &["--json", "zoom", file, "--filter", q])).unwrap();
        v["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap().into())
            .collect()
    };
    let java = "android/src/com/local/loom/Link.java";
    assert_eq!(names(java, "tailnet"), ["Link", "App", "tailnet"]);
    assert_eq!(names(java, "port"), ["Link", "App", "port"]);
    assert!(ok(&r, &["zoom", java, "--filter", "tailnet"]).contains("1 matching \"tailnet\""));
    assert_eq!(names("src/net.ts", "tailnet"), ["tailnet"]);
    assert_eq!(names("src/net.ts", "port"), ["port"]);
}

#[test]
fn rust_crate_paths_follow_the_file_module_and_cfg_all_conjuncts() {
    let r = fresh("rsfilemod");
    write(
        &r,
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    );
    write(
        &r,
        "Cargo.lock",
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"modz\",\n]\n\n[[package]]\nname = \"modz\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
    );
    let c = r.join("cargo/registry/src/index.crates.io-0000/modz-0.1.0");
    write(
        &c,
        "Cargo.toml",
        "[package]\nname = \"modz\"\nversion = \"0.1.0\"\n",
    );
    write(
        &c,
        "src/lib.rs",
        r#"pub struct Set;
pub mod foo;
pub mod operation;
mod platform;
#[cfg(feature = "union")]
union Data {
    a: u8,
}
#[cfg(all(feature = "union", feature = "const_new"))]
impl Data {
    const fn from_const(inline: u8) -> Self { todo!() }
}
#[cfg(not(feature = "union"))]
enum Data {
    A,
}
#[cfg(all(not(feature = "union"), feature = "const_new"))]
impl Data {
    const fn from_const(inline: u8) -> Self { todo!() }
}
impl crate::Set {
    pub fn at_root(&self) {}
}
"#,
    );
    write(
        &c,
        "src/platform/windows.rs",
        "pub(crate) struct Set {\n    exclude_from_monitoring: bool,\n}\npub trait SetExt {\n    fn exclude_from_monitoring(self) -> Self;\n}\nimpl SetExt for crate::Set {\n    fn exclude_from_monitoring(self) -> Self {\n        self\n    }\n}\nimpl crate::platform::windows::Set {\n    pub fn own(&self) {}\n}\nimpl super::Set {\n    pub fn sibling(&self) {}\n}\n",
    );
    write(
        &c,
        "src/operation/create_token.rs",
        "pub enum CreateTokenError {\n    Unhandled,\n}\nimpl RequestId for crate::operation::create_token::CreateTokenError {\n    fn request_id(&self) -> Option<&str> {\n        None\n    }\n}\n",
    );
    write(
        &c,
        "src/foo/mod.rs",
        "pub struct F;\nimpl crate::foo::F {\n    pub fn in_mod(&self) {}\n}\nimpl crate::F {\n    pub fn elsewhere(&self) {}\n}\n",
    );
    // (name, line) of the symbols `zoom <file> --filter <q>` keeps.
    let zoom = |file: &str, q: &str| -> Vec<(String, u64)> {
        let v: Value =
            serde_json::from_str(&ok(&c, &["--json", "zoom", file, "--filter", q])).unwrap();
        v["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s["name"].as_str().unwrap().into(),
                    s["line"].as_u64().unwrap(),
                )
            })
            .collect()
    };
    let m = |v: &[(&str, u64)]| -> Vec<(String, u64)> {
        v.iter().map(|(n, l)| (n.to_string(), *l)).collect()
    };
    // `src/platform/windows.rs`: `crate::Set` is `lib.rs`'s, `super::Set` is `platform`'s.
    let win = "src/platform/windows.rs";
    assert_eq!(
        zoom(win, "fn exclude_from_monitoring(self) -> Self"),
        m(&[
            ("SetExt", 4),
            ("exclude_from_monitoring", 5),
            ("exclude_from_monitoring", 8)
        ])
    );
    let out = ok(&c, &["zoom", win, "--filter", "exclude_from_monitoring"]);
    assert!(
        out.contains("fn exclude_from_monitoring(self) -> Self  (in Set)"),
        "{out}"
    );
    assert_eq!(zoom(win, "fn own"), m(&[("Set", 1), ("own", 13)]));
    assert_eq!(zoom(win, "fn sibling"), m(&[("sibling", 16)]));
    // `src/lib.rs` is the crate root; `src/foo/mod.rs` is `crate::foo`.
    assert_eq!(
        zoom("src/lib.rs", "fn at_root"),
        m(&[("Set", 1), ("at_root", 22)])
    );
    assert_eq!(
        zoom("src/foo/mod.rs", "fn in_mod"),
        m(&[("F", 1), ("in_mod", 3)])
    );
    assert_eq!(
        zoom("src/foo/mod.rs", "fn elsewhere"),
        m(&[("elsewhere", 6)])
    );
    // `src/operation/create_token.rs` is `crate::operation::create_token`: zoom and dep keep it.
    let op = "src/operation/create_token.rs";
    assert_eq!(
        zoom(op, "request_id"),
        m(&[("CreateTokenError", 1), ("request_id", 5)])
    );
    let o = tc()
        .env("CARGO_HOME", r.join("cargo"))
        .args(["--no-stats", "--cwd"])
        .arg(&*r)
        .args(["dep", "modz", "CreateTokenError"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{o:?}");
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(
        out.contains("  [:5] fn request_id(&self) -> Option<&str>"),
        "{out}"
    );
    // `all(not(union), const_new)` entails `not(union)`: the enum's, never the union's.
    assert_eq!(
        zoom("src/lib.rs", "fn from_const(inline:"),
        m(&[
            ("Data", 6),
            ("from_const", 11),
            ("Data", 14),
            ("from_const", 19)
        ])
    );
}

#[test]
fn rust_crate_paths_follow_root_reexports_into_the_declaring_file() {
    let r = fresh("rsreexport");
    write(
        &r,
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    );
    write(
        &r,
        "Cargo.lock",
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"reex\",\n]\n\n[[package]]\nname = \"reex\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
    );
    let c = r.join("cargo/registry/src/index.crates.io-0000/reex-0.1.0");
    write(
        &c,
        "Cargo.toml",
        "[package]\nname = \"reex\"\nversion = \"0.1.0\"\n",
    );
    // muda's `pub use icon::{BadIcon, Icon, NativeIcon};`, a renamed and a glob re-export, and
    // arboard's own root `Set` beside a glob of the module that declares another `Set`.
    write(
        &c,
        "src/lib.rs",
        "mod glob;\nmod icon;\nmod platform;\nmod renamed;\npub use glob::*;\npub use icon::{BadIcon, Icon, NativeIcon};\npub use self::renamed::RenamedIcon as RootIcon;\npub use platform::windows::*;\npub struct Set<'c> {\n    c: &'c (),\n}\n",
    );
    write(
        &c,
        "src/icon.rs",
        "pub enum NativeIcon {\n    Add,\n}\nimpl crate::NativeIcon {\n    pub fn freedesktop_name(&self) -> &str {\n        \"list-add\"\n    }\n}\n",
    );
    write(
        &c,
        "src/renamed.rs",
        "pub enum RenamedIcon {\n    Add,\n}\nimpl crate::RootIcon {\n    pub fn renamed_name(&self) {}\n}\n",
    );
    write(
        &c,
        "src/glob.rs",
        "pub struct GlobIcon;\nimpl crate::GlobIcon {\n    pub fn glob_name(&self) {}\n}\n",
    );
    write(
        &c,
        "src/platform/windows.rs",
        "pub struct Set;\nimpl crate::Set<'_> {\n    pub fn exclude_from_monitoring(mut self) -> Self {\n        self\n    }\n}\n",
    );
    let zoom = |file: &str, q: &str| -> Vec<(String, u64)> {
        let v: Value =
            serde_json::from_str(&ok(&c, &["--json", "zoom", file, "--filter", q])).unwrap();
        v["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s["name"].as_str().unwrap().into(),
                    s["line"].as_u64().unwrap(),
                )
            })
            .collect()
    };
    let m = |v: &[(&str, u64)]| -> Vec<(String, u64)> {
        v.iter().map(|(n, l)| (n.to_string(), *l)).collect()
    };
    for (file, q, owner, name) in [
        (
            "src/icon.rs",
            "freedesktop_name",
            "NativeIcon",
            "NativeIcon",
        ),
        ("src/renamed.rs", "renamed_name", "RenamedIcon", "RootIcon"),
        ("src/glob.rs", "glob_name", "GlobIcon", "GlobIcon"),
    ] {
        let fns = zoom(file, q);
        assert_eq!(fns[0], (owner.to_string(), 1), "{file}: {fns:?}");
        assert_eq!(fns.len(), 2, "{file}: {fns:?}");
        let out = ok(&c, &["zoom", file, "--filter", q]);
        assert!(!out.contains(&format!("(in {name})")), "{out}");
    }
    // The root declares its own `Set`: the glob of `platform::windows` doesn't reach it.
    let win = "src/platform/windows.rs";
    assert_eq!(
        zoom(win, "fn exclude_from_monitoring(mut self"),
        m(&[("exclude_from_monitoring", 3)])
    );
    let out = ok(&c, &["zoom", win, "--filter", "exclude_from_monitoring"]);
    assert!(out.contains("-> Self  (in Set)"), "{out}");
    // `dep` lists the impl under the re-exported declaration, renamed or not.
    let dep = |q: &str| {
        let o = tc()
            .env("CARGO_HOME", r.join("cargo"))
            .args(["--no-stats", "--cwd"])
            .arg(&*r)
            .args(["dep", "reex", q])
            .output()
            .unwrap();
        assert!(o.status.success(), "{o:?}");
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    let out = dep("NativeIcon");
    assert!(
        out.contains("  [:5] pub fn freedesktop_name(&self) -> &str"),
        "{out}"
    );
    let out = dep("RenamedIcon");
    assert!(out.contains("  [:5] pub fn renamed_name(&self)"), "{out}");
    let out = dep("GlobIcon");
    assert!(out.contains("  [:3] pub fn glob_name(&self)"), "{out}");
    // Neither `Set` gets `windows.rs`'s `impl crate::Set` (that's the root's `Set`).
    let out = dep("Set");
    assert!(
        out.contains("pub struct Set") && !out.contains("exclude_from_monitoring"),
        "{out}"
    );
}

#[test]
fn rust_crate_paths_follow_cfg_grouped_and_child_reexports() {
    let r = fresh("rsreexport2");
    write(
        &r,
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    );
    write(
        &r,
        "Cargo.lock",
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"reex\",\n]\n\n[[package]]\nname = \"reex\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
    );
    let c = r.join("cargo/registry/src/index.crates.io-0000/reex-0.1.0");
    write(
        &c,
        "Cargo.toml",
        "[package]\nname = \"reex\"\nversion = \"0.1.0\"\n",
    );
    // `#[cfg]` twin re-exports (and an own `#[cfg]` declaration), a grouped and a nested glob, and
    // `crate::` paths from the root and from `sub/mod.rs` into their out-of-line child modules.
    write(
        &c,
        "src/lib.rs",
        "mod alt;\nmod bridge;\nmod group;\nmod nest;\nmod own;\nmod plain;\nmod sub;\n#[cfg(not(feature = \"alt\"))]\npub use plain::AltTy;\n#[cfg(feature = \"alt\")]\npub use alt::AltTy;\n#[cfg(feature = \"alt\")]\npub struct OwnTy;\n#[cfg(not(feature = \"alt\"))]\npub use own::OwnTy;\npub use group::{Other, *};\npub use nest::{inner::{*}};\npub struct BridgeTy;\nimpl crate::bridge::BridgeTy {\n    pub fn bridged(&self) {}\n}\n",
    );
    let twin = |ty: &str, pre: &str| {
        format!("pub struct {ty};\n#[cfg(feature = \"alt\")]\nimpl crate::{ty} {{\n    pub fn {pre}_alt(&self) {{}}\n}}\n#[cfg(not(feature = \"alt\"))]\nimpl crate::{ty} {{\n    pub fn {pre}_default(&self) {{}}\n}}\n")
    };
    write(&c, "src/plain.rs", &twin("AltTy", "twin"));
    write(&c, "src/alt.rs", "pub struct AltTy;\n");
    write(&c, "src/own.rs", &twin("OwnTy", "own"));
    write(&c, "src/bridge.rs", "pub use crate::BridgeTy;\n");
    write(
        &c,
        "src/group.rs",
        "pub struct Other;\npub enum GroupIcon {\n    Add,\n}\nimpl crate::GroupIcon {\n    pub fn group_name(&self) {}\n}\n",
    );
    write(&c, "src/nest/mod.rs", "pub mod inner;\n");
    write(
        &c,
        "src/nest/inner.rs",
        "pub struct NestIcon;\nimpl crate::NestIcon {\n    pub fn nest_name(&self) {}\n}\n",
    );
    write(
        &c,
        "src/sub/mod.rs",
        "mod child;\npub struct SubTy;\nimpl crate::sub::child::SubTy {\n    pub fn via_child(&self) {}\n}\n",
    );
    write(&c, "src/sub/child.rs", "pub use super::SubTy;\n");
    let zoom = |file: &str, q: &str| -> Vec<(String, u64)> {
        let v: Value =
            serde_json::from_str(&ok(&c, &["--json", "zoom", file, "--filter", q])).unwrap();
        v["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s["name"].as_str().unwrap().into(),
                    s["line"].as_u64().unwrap(),
                )
            })
            .collect()
    };
    let m = |v: &[(&str, u64)]| -> Vec<(String, u64)> {
        v.iter().map(|(n, l)| (n.to_string(), *l)).collect()
    };
    // Attached: the owner is shown above the member, with no `(in Owner)` note.
    for (file, q, owner, line, member) in [
        ("src/plain.rs", "twin_default", "AltTy", 1, 8),
        ("src/own.rs", "own_default", "OwnTy", 1, 8),
        ("src/group.rs", "group_name", "GroupIcon", 2, 6),
        ("src/nest/inner.rs", "nest_name", "NestIcon", 1, 3),
        ("src/lib.rs", "bridged", "BridgeTy", 18, 20),
        ("src/sub/mod.rs", "via_child", "SubTy", 2, 4),
    ] {
        assert_eq!(zoom(file, q), m(&[(owner, line), (q, member)]), "{file}");
        let out = ok(&c, &["zoom", file, "--filter", q]);
        assert!(!out.contains("(in "), "{file}: {out}");
    }
    // Under `alt`, `crate::AltTy` is `alt.rs`'s and `crate::OwnTy` the root's own.
    for (file, q, owner) in [
        ("src/plain.rs", "twin_alt", "AltTy"),
        ("src/own.rs", "own_alt", "OwnTy"),
    ] {
        assert_eq!(zoom(file, q), m(&[(q, 4)]), "{file}");
        let out = ok(&c, &["zoom", file, "--filter", q]);
        assert!(out.contains(&format!("(in {owner})")), "{file}: {out}");
    }
    // `dep` lists the same members. Not `OwnTy`: `dep` shows the root's own one, with no impls.
    let dep = |q: &str| {
        let o = tc()
            .env("CARGO_HOME", r.join("cargo"))
            .args(["--no-stats", "--cwd"])
            .arg(&*r)
            .args(["dep", "reex", q])
            .output()
            .unwrap();
        assert!(o.status.success(), "{o:?}");
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    for (ty, has, lacks) in [
        ("AltTy", "pub fn twin_default(&self)", Some("twin_alt")),
        ("GroupIcon", "pub fn group_name(&self)", None),
        ("NestIcon", "pub fn nest_name(&self)", None),
        ("BridgeTy", "pub fn bridged(&self)", None),
        ("SubTy", "pub fn via_child(&self)", None),
    ] {
        let out = dep(ty);
        assert!(out.contains(has), "{ty}: {out}");
        assert!(lacks.is_none_or(|l| !out.contains(l)), "{ty}: {out}");
    }
}

#[test]
fn rust_crate_paths_follow_exclusive_targets_conditional_shadows_mod_cfgs_and_spaced_globs() {
    let r = fresh("rsreexport3");
    write(
        &r,
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    );
    write(
        &r,
        "Cargo.lock",
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"reex\",\n]\n\n[[package]]\nname = \"reex\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
    );
    let c = r.join("cargo/registry/src/index.crates.io-0000/reex-0.1.0");
    write(
        &c,
        "Cargo.toml",
        "[package]\nname = \"reex\"\nversion = \"0.1.0\"\n",
    );
    // `target_os` twin re-exports; a glob beside a `#[cfg(feature = "alt")]` named `use` and
    // beside an unconditional one; a `#[cfg]` inline `mod bridge` beside `mod bridge;`; globs
    // with spaces and comments after `::`; and a `main.rs` whose `mod helper;` isn't `lib.rs`'s
    // inline `mod helper`.
    write(
        &c,
        "src/lib.rs",
        "mod fallback;\nmod linux;\nmod loose;\nmod outer;\nmod preferred;\nmod win;\n#[cfg(feature = \"inline\")]\nmod bridge {\n    pub struct Other;\n}\n#[cfg(not(feature = \"inline\"))]\nmod bridge;\nmod helper {\n    pub struct Unrelated;\n}\n#[cfg(target_os = \"linux\")]\npub use linux::OsTy;\n#[cfg(target_os = \"windows\")]\npub use win::OsTy;\npub use fallback::*;\n#[cfg(feature = \"alt\")]\npub use preferred::FbTy;\npub use preferred::Shadowed;\npub use outer::{nested:: *};\npub use outer::{other:: /* comment */ *};\npub use loose:: /* c */ *;\npub struct BridgeTy;\nimpl crate::bridge::BridgeTy {\n    pub fn bridged(&self) {}\n}\n",
    );
    write(
        &c,
        "src/linux.rs",
        "pub struct OsTy;\n#[cfg(target_os = \"linux\")]\nimpl crate::OsTy {\n    pub fn on_linux(&self) {}\n}\n#[cfg(windows)]\nimpl crate::OsTy {\n    pub fn on_windows(&self) {}\n}\n",
    );
    write(&c, "src/win.rs", "pub struct OsTy;\n");
    write(
        &c,
        "src/fallback.rs",
        "pub struct FbTy;\nimpl crate::FbTy {\n    pub fn fb_plain(&self) {}\n}\n#[cfg(feature = \"alt\")]\nimpl crate::FbTy {\n    pub fn fb_alt(&self) {}\n}\npub struct Shadowed;\nimpl crate::Shadowed {\n    pub fn shadowed(&self) {}\n}\n",
    );
    write(
        &c,
        "src/preferred.rs",
        "pub struct FbTy;\npub struct Shadowed;\n",
    );
    write(&c, "src/bridge.rs", "pub use crate::BridgeTy;\n");
    write(&c, "src/outer/mod.rs", "pub mod nested;\npub mod other;\n");
    write(
        &c,
        "src/outer/nested.rs",
        "pub struct NestedTy;\nimpl crate::NestedTy {\n    pub fn nested_name(&self) {}\n}\n",
    );
    write(
        &c,
        "src/outer/other.rs",
        "pub struct OtherTy;\nimpl crate::OtherTy {\n    pub fn other_name(&self) {}\n}\n",
    );
    write(
        &c,
        "src/loose.rs",
        "pub struct LooseTy;\nimpl crate::LooseTy {\n    pub fn loose_name(&self) {}\n}\n",
    );
    write(
        &c,
        "src/main.rs",
        "mod helper;\npub struct MainTy;\nimpl crate::helper::MainTy {\n    pub fn via_main(&self) {}\n}\nfn main() {}\n",
    );
    write(&c, "src/helper.rs", "pub use crate::MainTy;\n");
    let zoom = |file: &str, q: &str| -> Vec<(String, u64)> {
        let v: Value =
            serde_json::from_str(&ok(&c, &["--json", "zoom", file, "--filter", q])).unwrap();
        v["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s["name"].as_str().unwrap().into(),
                    s["line"].as_u64().unwrap(),
                )
            })
            .collect()
    };
    let m = |v: &[(&str, u64)]| -> Vec<(String, u64)> {
        v.iter().map(|(n, l)| (n.to_string(), *l)).collect()
    };
    // Attached: the owner is shown above the member, with no `(in Owner)` note.
    for (file, q, owner, line, member) in [
        ("src/linux.rs", "on_linux", "OsTy", 1, 4),
        ("src/fallback.rs", "fb_plain", "FbTy", 1, 3),
        ("src/lib.rs", "bridged", "BridgeTy", 27, 29),
        ("src/outer/nested.rs", "nested_name", "NestedTy", 1, 3),
        ("src/outer/other.rs", "other_name", "OtherTy", 1, 3),
        ("src/loose.rs", "loose_name", "LooseTy", 1, 3),
        ("src/main.rs", "via_main", "MainTy", 2, 4),
    ] {
        assert_eq!(zoom(file, q), m(&[(owner, line), (q, member)]), "{file}");
        let out = ok(&c, &["zoom", file, "--filter", q]);
        assert!(!out.contains("(in "), "{file}: {out}");
    }
    // Not attached: on `windows`, `crate::OsTy` is `win.rs`'s; under `alt`, `crate::FbTy` is
    // `preferred.rs`'s; and the unconditional `pub use preferred::Shadowed` always wins the glob.
    for (file, q, owner, member) in [
        ("src/linux.rs", "on_windows", "OsTy", 8),
        ("src/fallback.rs", "fb_alt", "FbTy", 7),
        ("src/fallback.rs", "fn shadowed", "Shadowed", 11),
    ] {
        let name = q.trim_start_matches("fn ");
        assert_eq!(zoom(file, q), m(&[(name, member)]), "{file}: {q}");
        let out = ok(&c, &["zoom", file, "--filter", q]);
        assert!(out.contains(&format!("(in {owner})")), "{file}: {out}");
    }
    // `dep` lists the same members. Not `FbTy` or `Shadowed`: `dep` shows the declaration the
    // root's named `use` points at (`preferred.rs`), whatever its cfg.
    let dep = |q: &str| {
        let o = tc()
            .env("CARGO_HOME", r.join("cargo"))
            .args(["--no-stats", "--cwd"])
            .arg(&*r)
            .args(["dep", "reex", q])
            .output()
            .unwrap();
        assert!(o.status.success(), "{o:?}");
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    for (ty, has, lacks) in [
        ("OsTy", "pub fn on_linux(&self)", Some("on_windows")),
        ("BridgeTy", "pub fn bridged(&self)", None),
        ("NestedTy", "pub fn nested_name(&self)", None),
        ("OtherTy", "pub fn other_name(&self)", None),
        ("LooseTy", "pub fn loose_name(&self)", None),
    ] {
        let out = dep(ty);
        assert!(out.contains(has), "{ty}: {out}");
        assert!(lacks.is_none_or(|l| !out.contains(l)), "{ty}: {out}");
    }
}

/// Rust `dep`/`zoom` on a crate with twin aliases, renaming re-exports (direct, chained and
/// dotted), a trait's associated type, and a crate whose `[lib] path` is `binding_rust/lib.rs`.
#[test]
fn rust_aliases_twin_aliases_associated_types_and_custom_lib_paths() {
    let r = fresh("rsalias");
    write(
        &r,
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    );
    let pkg = |n: &str| {
        format!("\n[[package]]\nname = \"{n}\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n")
    };
    write(
        &r,
        "Cargo.lock",
        &format!(
            "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"aliasx\",\n \"custlib\",\n]\n{}{}",
            pkg("aliasx"),
            pkg("custlib")
        ),
    );
    let reg = r.join("cargo/registry/src/index.crates.io-0000");
    let c = reg.join("aliasx-0.1.0");
    write(
        &c,
        "Cargo.toml",
        "[package]\nname = \"aliasx\"\nversion = \"0.1.0\"\n",
    );
    write(
        &c,
        "src/lib.rs",
        "mod builder;\nmod chain;\nmod platform;\npub mod de;\npub use builder::ConnectorBuilder as HttpsConnectorBuilder;\npub use chain::Middle as Outer;\n#[cfg(feature = \"alt\")]\npub use platform::A as T;\n#[cfg(not(feature = \"alt\"))]\npub use platform::B as T;\npub use de::{Deserializer, Error};\n",
    );
    write(
        &c,
        "src/builder.rs",
        "pub struct ConnectorBuilder<State>(State);\nimpl<State> ConnectorBuilder<State> {\n    pub fn with_native_roots(self) -> Self {\n        self\n    }\n}\n",
    );
    write(
        &c,
        "src/chain.rs",
        "mod inner;\npub use inner::Deep as Middle;\n",
    );
    write(
        &c,
        "src/chain/inner.rs",
        "pub struct Deep;\nimpl Deep {\n    pub fn deep_fn(&self) {}\n}\n",
    );
    write(
        &c,
        "src/platform.rs",
        "pub struct A;\npub struct B;\nimpl crate::T {\n    pub fn shared(&self) {}\n}\n",
    );
    write(
        &c,
        "src/de.rs",
        "pub trait Error: Sized {\n    fn custom(msg: &str) -> Self;\n}\npub trait Deserializer {\n    /// The error type.\n    type Error: Error;\n    fn deserialize_any(self);\n}\n",
    );
    let l = reg.join("custlib-0.1.0");
    write(
        &l,
        "Cargo.toml",
        "[package]\nname = \"custlib\"\nversion = \"0.1.0\"\npath = \"no/pkg.rs\"\n\n[lib]\nname = \"custlib\" # the library\npath = \"binding_rust/lib.rs\"\n\n[[bin]]\nname = \"x\"\npath = \"src/lib.rs\"\n\n[dependencies.y]\npath = \"../y\"\n",
    );
    write(
        &l,
        "binding_rust/lib.rs",
        "mod cursor;\npub use cursor::TreeCursor;\n",
    );
    write(
        &l,
        "binding_rust/cursor.rs",
        "pub struct TreeCursor;\nimpl crate::TreeCursor {\n    pub fn goto_first_child(&mut self) -> bool {\n        true\n    }\n}\n",
    );
    write(&l, "src/lib.rs", "pub struct Decoy;\n");
    let dep = |args: &[&str]| {
        let o = tc()
            .env("CARGO_HOME", r.join("cargo"))
            .args(["--no-stats", "--cwd"])
            .arg(&*r)
            .arg("dep")
            .args(args)
            .output()
            .unwrap();
        assert!(o.status.success(), "{args:?}: {o:?}");
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    // Twin aliases: the unconditional `impl crate::T` is `A`'s and `B`'s.
    for ty in ["A", "B", "T"] {
        let out = dep(&["aliasx", ty]);
        assert!(out.contains("pub fn shared(&self)"), "{ty}: {out}");
    }
    let v: Value = serde_json::from_str(&ok(
        &c,
        &["--json", "zoom", "src/platform.rs", "--filter", "shared"],
    ))
    .unwrap();
    let shown: Vec<(&str, u64)> = v["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["name"].as_str().unwrap(), s["line"].as_u64().unwrap()))
        .collect();
    assert_eq!(shown, [("A", 1), ("B", 2), ("shared", 4)]);
    // Renaming re-exports, chained and dotted: found under the declared name, shown as the alias.
    for (q, has, public, declared) in [
        (
            "HttpsConnectorBuilder",
            "pub struct ConnectorBuilder<State>  [aliasx-0.1.0/src/builder.rs:1]",
            "HttpsConnectorBuilder",
            "ConnectorBuilder",
        ),
        (
            "HttpsConnectorBuilder.with_native_roots",
            "pub fn with_native_roots(self) -> Self",
            "HttpsConnectorBuilder.with_native_roots",
            "ConnectorBuilder.with_native_roots",
        ),
        (
            "HttpsConnectorBuilder::with_native_roots",
            "pub fn with_native_roots(self) -> Self",
            "HttpsConnectorBuilder.with_native_roots",
            "ConnectorBuilder.with_native_roots",
        ),
        ("Outer", "pub struct Deep;", "Outer", "Deep"),
    ] {
        let out = dep(&["aliasx", q]);
        assert!(out.contains(has), "{q}: {out}");
        let note = format!("(exported as `{public}`, declared as `{declared}`)");
        assert!(out.contains(&note), "{q}: {out}");
        let v: Value = serde_json::from_str(&dep(&["--json", "aliasx", q])).unwrap();
        assert_eq!(v["hits"][0]["name"], public, "{q}");
        assert_eq!(v["hits"][0]["declared_as"], declared, "{q}");
    }
    assert!(dep(&["aliasx", "HttpsConnectorBuilder"]).contains("pub fn with_native_roots"));
    assert!(dep(&["aliasx", "Outer"]).contains("pub fn deep_fn(&self)"));
    // The package overview still lists the public names.
    let out = dep(&["aliasx"]);
    for n in ["HttpsConnectorBuilder", "Outer", "Deserializer", "T"] {
        assert!(out.contains(n), "{n}: {out}");
    }
    // A trait's associated type is its member, found as `Owner.Type`, never as a bare name.
    let out = dep(&["aliasx", "Deserializer"]);
    assert!(out.contains("] type Error: Error;"), "{out}");
    let out = dep(&["aliasx", "Deserializer.Error"]);
    assert!(
        out.starts_with("aliasx") && out.contains("type Error: Error;"),
        "{out}"
    );
    assert!(!out.contains("NOT FOUND"), "{out}");
    let out = dep(&["aliasx", "Error"]);
    assert!(out.contains("pub trait Error: Sized"), "{out}");
    assert!(!out.contains("type Error: Error;"), "{out}");
    // `[lib] path`: the entry, the scan root and `crate::` paths are under `binding_rust/`.
    let out = dep(&["custlib", "TreeCursor"]);
    assert!(out.contains("pub struct TreeCursor;"), "{out}");
    assert!(
        out.contains("custlib-0.1.0/binding_rust/cursor.rs:1"),
        "{out}"
    );
    assert!(
        out.contains("pub fn goto_first_child(&mut self) -> bool"),
        "{out}"
    );
    let out = dep(&["custlib"]);
    assert!(
        out.contains("entry: custlib-0.1.0/binding_rust/lib.rs"),
        "{out}"
    );
    assert!(
        out.contains("TreeCursor") && !out.contains("Decoy"),
        "{out}"
    );
}

/// Rust `dep` on crate-root aliases (petgraph's `pub use crate::Direction as EdgeDirection;`),
/// and `dep`/`zoom` on a crate whose `[lib] path` is `lib.rs` in the crate dir (selectors).
#[test]
fn rust_crate_root_aliases_and_lib_at_the_crate_root() {
    let r = fresh("rsroot");
    write(
        &r,
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    );
    let pkg = |n: &str| {
        format!("\n[[package]]\nname = \"{n}\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n")
    };
    write(
        &r,
        "Cargo.lock",
        &format!(
            "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"rootalias\",\n \"rootlib\",\n]\n{}{}",
            pkg("rootalias"),
            pkg("rootlib")
        ),
    );
    let reg = r.join("cargo/registry/src/index.crates.io-0000");
    let a = reg.join("rootalias-0.1.0");
    write(
        &a,
        "Cargo.toml",
        "[package]\nname = \"rootalias\"\nversion = \"0.1.0\"\n",
    );
    write(
        &a,
        "src/lib.rs",
        "mod other;\npub enum Direction {\n    Outgoing,\n    Incoming,\n}\nimpl Direction {\n    pub fn opposite(self) -> Direction {\n        self\n    }\n}\npub use crate::Direction as EdgeDirection;\npub use self::Direction as SelfDir;\npub use Direction as BareDir;\n",
    );
    // Another `opposite`: never an alias's.
    write(
        &a,
        "src/other.rs",
        "pub struct Other;\nimpl Other {\n    pub fn opposite(self) {}\n}\n",
    );
    let l = reg.join("rootlib-0.1.0");
    write(
        &l,
        "Cargo.toml",
        "[package]\nname = \"rootlib\"\nversion = \"0.1.0\"\nbuild = \"build.rs\"\n\n[lib]\nname = \"rootlib\"\npath = \"lib.rs\"\n",
    );
    write(&l, "lib.rs", "mod parser;\npub use parser::Selector;\n");
    write(
        &l,
        "parser.rs",
        "pub struct Selector;\nimpl crate::Selector {\n    pub fn matches_host(&self) -> bool {\n        true\n    }\n}\n",
    );
    write(&l, "build.rs", "pub struct BuildOnly;\nfn main() {}\n");
    write(&l, "src/main.rs", "pub struct BinOnly;\nfn main() {}\n");
    let dep = |args: &[&str]| {
        let o = tc()
            .env("CARGO_HOME", r.join("cargo"))
            .args(["--no-stats", "--cwd"])
            .arg(&*r)
            .arg("dep")
            .args(args)
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&o.stdout).into_owned(),
            o.status.code(),
        )
    };
    // Crate-root aliases, bare and dotted: found in the entry under the declared name.
    for alias in ["EdgeDirection", "SelfDir", "BareDir"] {
        for (q, has, declared) in [
            (
                alias.to_string(),
                "pub enum Direction  [rootalias-0.1.0/src/lib.rs:2]",
                "Direction",
            ),
            (
                format!("{alias}.opposite"),
                "pub fn opposite(self) -> Direction",
                "Direction.opposite",
            ),
        ] {
            let (out, code) = dep(&["rootalias", &q]);
            assert_eq!(code, Some(0), "{q}: {out}");
            assert!(out.contains(has), "{q}: {out}");
            let note = format!("(exported as `{q}`, declared as `{declared}`)");
            assert!(out.contains(&note), "{q}: {out}");
            assert!(!out.contains("other.rs"), "{q}: {out}");
        }
    }
    // `[lib] path = "lib.rs"`: the entry, the scan root and `crate::` paths are the crate dir's.
    let (out, code) = dep(&["rootlib", "Selector"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("rootlib-0.1.0/parser.rs:1"), "{out}");
    assert!(out.contains("pub fn matches_host(&self) -> bool"), "{out}");
    let (out, _) = dep(&["rootlib"]);
    assert!(out.contains("entry: rootlib-0.1.0/lib.rs"), "{out}");
    // `build.rs` and `src/main.rs` are other targets.
    for q in ["BuildOnly", "BinOnly"] {
        let (out, code) = dep(&["rootlib", q]);
        assert_eq!(code, Some(1), "{q}: {out}");
        assert!(out.contains("NOT FOUND"), "{q}: {out}");
    }
    // zoom: `parser.rs`'s `crate::matching::..` callee is the crate dir's `matching.rs`, never
    // `benches/`, `examples/` or `tests/` (which sort first), and `crate::Selector` is its owner.
    let z = fresh("rszoomroot");
    write(
        &z,
        "Cargo.toml",
        "[package]\nname = \"sel\"\nversion = \"0.1.0\"\n\n[lib]\npath = \"lib.rs\"\n",
    );
    write(
        &z,
        "lib.rs",
        "mod matching;\nmod parser;\npub use parser::Selector;\n",
    );
    write(
        &z,
        "parser.rs",
        "pub struct Selector;\nimpl crate::Selector {\n    pub fn matches_host(&self) -> bool {\n        crate::matching::compound_matches(self)\n    }\n}\n",
    );
    let callee = "pub fn compound_matches(_s: &crate::parser::Selector) -> bool {\n    true\n}\n";
    for f in [
        "matching.rs",
        "benches/matching.rs",
        "examples/matching.rs",
        "tests/matching.rs",
    ] {
        write(&z, f, callee);
    }
    write(&z, "build.rs", "fn main() {}\n");
    let v: Value = serde_json::from_str(&ok(
        &z,
        &["--json", "zoom", "parser.rs:Selector.matches_host"],
    ))
    .unwrap();
    let callees = v["callees"].as_array().unwrap();
    assert_eq!(callees.len(), 1, "{v}");
    assert_eq!(callees[0]["name"], "compound_matches", "{v}");
    assert_eq!(callees[0]["file"], "matching.rs", "{v}");
    let v: Value = serde_json::from_str(&ok(
        &z,
        &["--json", "zoom", "parser.rs", "--filter", "matches_host"],
    ))
    .unwrap();
    let shown: Vec<(&str, u64)> = v["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["name"].as_str().unwrap(), s["line"].as_u64().unwrap()))
        .collect();
    assert_eq!(shown, [("Selector", 1), ("matches_host", 3)]);
}

/// Rust `dep`/`zoom`: `#[cfg(feature = "alt")] struct T;` beside `#[cfg(not(feature = "alt"))]
/// pub use ..::B as T;` (same file, and through another module): an unconditional
/// `impl crate::T` is `T`'s and `B`'s, an `alt` one `T`'s alone.
#[test]
fn rust_conditional_declarations_keep_complementary_reexports() {
    let r = fresh("rscond");
    write(
        &r,
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    );
    write(
        &r,
        "Cargo.lock",
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"condx\",\n]\n\n[[package]]\nname = \"condx\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
    );
    let c = r.join("cargo/registry/src/index.crates.io-0000/condx-0.1.0");
    write(
        &c,
        "Cargo.toml",
        "[package]\nname = \"condx\"\nversion = \"0.1.0\"\n",
    );
    write(
        &c,
        "src/lib.rs",
        "mod mid;\n#[cfg(feature = \"alt\")]\npub struct T;\n#[cfg(feature = \"alt\")]\npub struct U;\npub mod sub {\n    pub struct B;\n    pub struct C;\n}\n#[cfg(not(feature = \"alt\"))]\npub use sub::B as T;\n#[cfg(not(feature = \"alt\"))]\npub use mid::U;\nimpl crate::T {\n    pub fn shared(&self) {}\n}\n#[cfg(feature = \"alt\")]\nimpl crate::T {\n    pub fn alt_only(&self) {}\n}\nimpl crate::U {\n    pub fn via_mid(&self) {}\n}\npub struct V;\npub mod sub2 {\n    pub struct D;\n}\n#[cfg(not(feature = \"alt\"))]\npub use sub2::D as V;\nimpl crate::V {\n    pub fn only_v(&self) {}\n}\n",
    );
    write(&c, "src/mid.rs", "pub use crate::sub::C as U;\n");
    let shown = |filter: &str| -> Vec<(String, u64)> {
        let v: Value = serde_json::from_str(&ok(
            &c,
            &["--json", "zoom", "src/lib.rs", "--filter", filter],
        ))
        .unwrap();
        v["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                let n = s["name"].as_str().unwrap();
                (
                    n.rsplit("::").next().unwrap().to_string(),
                    s["line"].as_u64().unwrap(),
                )
            })
            .collect()
    };
    let o = |v: &[(&str, u64)]| -> Vec<(String, u64)> {
        v.iter().map(|(n, l)| (n.to_string(), *l)).collect()
    };
    assert_eq!(
        shown("shared"),
        o(&[("T", 3), ("sub", 6), ("B", 7), ("shared", 15)])
    );
    assert_eq!(shown("alt_only"), o(&[("T", 3), ("alt_only", 19)]));
    assert_eq!(
        shown("via_mid"),
        o(&[("U", 5), ("sub", 6), ("C", 8), ("via_mid", 22)])
    );
    // An unconditional `V` is the only `V`: never `D`.
    assert_eq!(shown("only_v"), o(&[("V", 24), ("only_v", 31)]));
    let dep = |q: &str| {
        let o = tc()
            .env("CARGO_HOME", r.join("cargo"))
            .args(["--no-stats", "--cwd"])
            .arg(&*r)
            .args(["dep", "condx", q])
            .output()
            .unwrap();
        assert!(o.status.success(), "{q}: {o:?}");
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    let out = dep("B");
    assert!(out.contains("pub fn shared(&self)"), "{out}");
    assert!(!out.contains("alt_only"), "{out}");
    let out = dep("C");
    assert!(out.contains("pub fn via_mid(&self)"), "{out}");
    let out = dep("D");
    assert!(out.contains("pub struct D;"), "{out}");
    assert!(!out.contains("only_v"), "{out}");
}

/// Rust `dep`: an alias's member is found in whichever file has the declared type's `impl`
/// (eyre's `Error.new`); a crate-root `pub use crate::anyhow as format_err;` is the
/// `#[macro_export]` macro in `src/macros.rs`, never `__private::format_err`; a renamed
/// cross-crate re-export (gix-pack's `pub use memmap2::Mmap as MMap;`) is looked up under the
/// other crate's name; a `#[cfg(not(any(unix, windows)))]` fallback never gets a Windows impl.
#[test]
fn rust_alias_members_macro_aliases_renamed_crate_reexports_and_negated_anys() {
    let r = fresh("rsalias4");
    write(
        &r,
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    );
    let names = ["aliasy", "fallb", "inner", "mac", "outer"];
    let mut lock = format!(
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n{}]\n",
        names.map(|n| format!(" \"{n}\",\n")).concat()
    );
    for n in names {
        lock.push_str(&format!("\n[[package]]\nname = \"{n}\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n"));
    }
    write(&r, "Cargo.lock", &lock);
    let reg = r.join("cargo/registry/src/index.crates.io-0000");
    let krate = |n: &str, files: &[(&str, &str)]| {
        let d = reg.join(format!("{n}-0.1.0"));
        write(
            &d,
            "Cargo.toml",
            &format!("[package]\nname = \"{n}\"\nversion = \"0.1.0\"\n"),
        );
        for (f, body) in files {
            write(&d, f, body);
        }
    };
    krate(
        "aliasy",
        &[
            (
                "src/lib.rs",
                "mod error;\nmod io;\npub struct Report;\npub use crate::Report as Error;\npub use Report as BareError;\n",
            ),
            (
                "src/error.rs",
                "use crate::Report;\nimpl Report {\n    pub fn new() -> Report {\n        Report\n    }\n}\n",
            ),
            // Another `Error` with a `new`: never the alias's.
            (
                "src/io.rs",
                "pub struct Error;\nimpl Error {\n    pub fn new() -> Error {\n        Error\n    }\n}\n",
            ),
        ],
    );
    krate(
        "mac",
        &[
            (
                "src/lib.rs",
                "#[macro_use]\nmod macros;\npub use crate::anyhow as format_err;\npub use self::anyhow as self_err;\npub use anyhow as bare_err;\npub mod __private {\n    pub fn format_err() {}\n    pub fn self_err() {}\n    pub fn bare_err() {}\n    pub fn only_private() {}\n}\n",
            ),
            (
                "src/macros.rs",
                "/// Construct an ad-hoc error.\n#[macro_export]\nmacro_rules! anyhow {\n    ($msg:literal) => {};\n}\n",
            ),
        ],
    );
    krate(
        "outer",
        &[(
            "src/lib.rs",
            "pub use inner::Mmap as MMap;\npub use inner::Plain;\n",
        )],
    );
    krate(
        "inner",
        &[(
            "src/lib.rs",
            "pub struct Mmap;\nimpl Mmap {\n    pub fn len(&self) -> usize {\n        0\n    }\n}\npub struct Plain;\nimpl Plain {\n    pub fn go(&self) {}\n}\n",
        )],
    );
    krate(
        "fallb",
        &[(
            "src/lib.rs",
            "#[cfg(unix)]\npub struct Handle {\n    pub fd: i32,\n}\n#[cfg(not(any(unix, windows)))]\npub struct Handle {\n    pub id: u32,\n}\n#[cfg(windows)]\nimpl Handle {\n    pub fn raw_handle(&self) {}\n}\nimpl Handle {\n    pub fn shared(&self) {}\n}\n",
        )],
    );
    let dep = |args: &[&str]| {
        let o = tc()
            .env("CARGO_HOME", r.join("cargo"))
            .args(["--no-stats", "--cwd"])
            .arg(&*r)
            .args(args)
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&o.stdout).into_owned(),
            o.status.code(),
        )
    };
    let found = |args: &[&str], has: &[&str], hasnt: &[&str]| {
        let (out, code) = dep(args);
        assert_eq!(code, Some(0), "{args:?}: {out}");
        for h in has {
            assert!(out.contains(h), "{args:?} lacks {h}: {out}");
        }
        for h in hasnt {
            assert!(!out.contains(h), "{args:?} has {h}: {out}");
        }
    };
    // 1. An alias's member: the `impl` in `src/error.rs`, not `io::Error::new`.
    for alias in ["Error", "BareError"] {
        found(
            &["dep", "aliasy", &format!("{alias}.new")],
            &[
                "pub fn new() -> Report  [aliasy-0.1.0/src/error.rs:3]",
                &format!("(exported as `{alias}.new`, declared as `Report.new`)"),
            ],
            &["io.rs"],
        );
        found(
            &["dep", "aliasy", alias],
            &[
                "pub struct Report;  [aliasy-0.1.0/src/lib.rs:3]",
                &format!("(exported as `{alias}`, declared as `Report`)"),
            ],
            &["io.rs"],
        );
    }
    // 4. A crate-root macro alias beats `__private`'s same-named fn; `__private` alone still counts.
    for alias in ["format_err", "self_err", "bare_err"] {
        found(
            &["dep", "mac", alias],
            &[
                "macro_rules! anyhow  [mac-0.1.0/src/macros.rs:3]",
                &format!("(exported as `{alias}`, declared as `anyhow`)"),
            ],
            &["lib.rs"],
        );
    }
    found(
        &["dep", "mac", "only_private"],
        &["pub fn only_private()  [mac-0.1.0/src/lib.rs:10]"],
        &[],
    );
    // 3. A renamed cross-crate re-export, bare and dotted, in text and JSON.
    found(
        &["dep", "outer", "MMap"],
        &[
            "re-exported from inner 0.1.0",
            "pub struct Mmap;  [inner-0.1.0/src/lib.rs:1]",
            "(exported as `MMap`, declared as `Mmap`)",
        ],
        &[],
    );
    for q in ["MMap.len", "MMap::len"] {
        found(
            &["dep", "outer", q],
            &[
                "re-exported from inner 0.1.0",
                "pub fn len(&self) -> usize  [inner-0.1.0/src/lib.rs:3]",
                "(exported as `MMap.len`, declared as `Mmap.len`)",
            ],
            &[],
        );
    }
    found(
        &["dep", "outer", "Plain.go"],
        &["pub fn go(&self)  [inner-0.1.0/src/lib.rs:9]"],
        &["exported as"],
    );
    for (q, declared) in [("MMap", "Mmap"), ("MMap.len", "Mmap.len")] {
        let (out, _) = dep(&["--json", "dep", "outer", q]);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["via"]["name"], "inner", "{v}");
        assert_eq!(v["hits"][0]["name"], q, "{v}");
        assert_eq!(v["hits"][0]["declared_as"], declared, "{v}");
    }
    // 2. Each `Handle` twin keeps `shared`; neither gets the `windows` impl.
    let (out, code) = dep(&["--json", "dep", "fallb", "Handle"]);
    assert_eq!(code, Some(0), "{out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    let twins: Vec<(u64, Vec<&str>)> = v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            let m = h["members"].as_array().unwrap();
            let names = m.iter().map(|m| m["name"].as_str().unwrap()).collect();
            (h["line"].as_u64().unwrap(), names)
        })
        .collect();
    assert_eq!(
        twins,
        [(2, vec!["fd", "shared"]), (6, vec!["id", "shared"])],
        "{v}"
    );
}

/// Rust `dep`: an alias's member is only its own type's (scc's `LinkedEntry` is
/// `linked_list::Entry`, never `hash_map::Entry`); a cross-crate re-export keeps its module path
/// (gix-tempfile's `pub use gix_fs::dir::create as create_dir;`, never `symlink::create`); a
/// private or `pub(crate)` root `use` is no public name (ratatui-termwiz's `TermwizPosition`);
/// a trailing comma inside `not(..)` still negates (`#[cfg(not(any(unix, windows),))]`).
#[test]
fn rust_alias_owners_reexport_paths_private_uses_and_trailing_comma_cfgs() {
    let r = fresh("rsalias5");
    write(
        &r,
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    );
    let names = [
        "cfgc", "fsg", "fsm", "fsx", "globby", "llist", "posx", "tmpf", "twz",
    ];
    let mut lock = format!(
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n{}]\n",
        names.map(|n| format!(" \"{n}\",\n")).concat()
    );
    for n in names {
        lock.push_str(&format!("\n[[package]]\nname = \"{n}\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n"));
    }
    write(&r, "Cargo.lock", &lock);
    let reg = r.join("cargo/registry/src/index.crates.io-0000");
    let krate = |n: &str, files: &[(&str, &str)]| {
        let d = reg.join(format!("{n}-0.1.0"));
        write(
            &d,
            "Cargo.toml",
            &format!("[package]\nname = \"{n}\"\nversion = \"0.1.0\"\n"),
        );
        for (f, body) in files {
            write(&d, f, body);
        }
    };
    krate(
        "llist",
        &[
            (
                "src/lib.rs",
                "pub mod hash_map;\nmod hash_table;\nmod linked_list;\nmod prelude;\nmod user;\nmod user2;\npub use linked_list::Entry as LinkedEntry;\n",
            ),
            ("src/prelude.rs", "pub use crate::linked_list::*;\n"),
            // Named through a glob re-export: not known to be another `Entry`.
            (
                "src/user2.rs",
                "use crate::prelude::Entry;\nimpl Entry {\n    pub fn via_glob(&self) {}\n}\n",
            ),
            (
                "src/hash_map.rs",
                "pub enum Entry {\n    A,\n}\nimpl Entry {\n    pub fn or_insert(self) {}\n}\n",
            ),
            (
                "src/hash_table.rs",
                "pub use self::inner::Entry;\nmod inner {\n    pub struct Entry;\n    impl Entry {\n        pub fn or_insert(self) {}\n    }\n}\n",
            ),
            (
                "src/linked_list.rs",
                "mod sub;\npub struct Entry {\n    pub next: u8,\n}\nimpl Entry {\n    pub fn take(&self) {}\n}\n",
            ),
            (
                "src/linked_list/sub.rs",
                "impl super::Entry {\n    pub fn via_super(&self) {}\n}\n",
            ),
            (
                "src/user.rs",
                "use crate::linked_list::Entry;\nimpl Entry {\n    pub fn via_use(&self) {}\n}\n",
            ),
        ],
    );
    // An alias of a type a glob re-exports (`prelude` has `pub use crate::imp::*;`).
    krate(
        "globby",
        &[
            (
                "src/lib.rs",
                "mod imp;\nmod prelude;\npub use prelude::Entry as LinkedEntry;\n",
            ),
            ("src/prelude.rs", "pub use crate::imp::*;\n"),
            (
                "src/imp.rs",
                "pub struct Entry;\nimpl Entry {\n    pub fn take(&self) {}\n}\n",
            ),
        ],
    );
    krate(
        "tmpf",
        &[(
            "src/lib.rs",
            "pub use fsx::dir::create as create_dir;\npub use fsm::dir::{create as make_dir};\npub use fsx::dir::Thing as T;\npub use fsg::hash_map::Thing as G;\n",
        )],
    );
    krate(
        "fsg",
        &[
            (
                "src/lib.rs",
                "mod map;\npub mod hash_map {\n    pub use crate::map::*;\n}\n",
            ),
            ("src/map.rs", "pub struct Thing;\n"),
        ],
    );
    // `dir::create` is a fn of `dir/mod.rs` in `fsx`, a module in `fsm`; `symlink.rs` (shallower,
    // so a whole-crate scan meets it first) has another `create`. `dir::Thing.go` has decoys in
    // `dir/create.rs` (same depth, scanned first) and `symlink.rs`.
    let thing = "pub struct Thing;\nimpl Thing {\n    pub fn go(&self) {}\n}\n";
    for (n, dir) in [
        (
            "fsx",
            format!("/// Create a directory.\npub fn create() {{}}\n{thing}"),
        ),
        ("fsm", "pub mod create;\n".to_string()),
    ] {
        krate(
            n,
            &[
                ("src/lib.rs", "pub mod dir;\npub mod symlink;\n"),
                ("src/dir/mod.rs", &dir),
                (
                    "src/dir/create.rs",
                    &format!("pub struct Retries;\npub fn create() {{}}\n{thing}"),
                ),
                (
                    "src/symlink.rs",
                    &format!("/// Create a symlink.\npub fn create() {{}}\n{thing}"),
                ),
            ],
        );
    }
    krate(
        "twz",
        &[(
            "src/lib.rs",
            "mod m {\n    pub struct Inner;\n}\nuse m::Inner as PrivInner;\nuse posx::Position as TermwizPosition;\npub(crate) use posx::Position as CratePosition;\npub(super) use posx::Position as SuperPosition;\npub use posx::Position as PubPosition;\npub fn f(_: TermwizPosition, _: CratePosition, _: SuperPosition, _: PrivInner) {}\n",
        )],
    );
    krate(
        "posx",
        &[("src/lib.rs", "pub enum Position {\n    Absolute,\n}\n")],
    );
    krate(
        "cfgc",
        &[(
            "src/lib.rs",
            "#[cfg(unix)]\npub struct Handle {\n    pub fd: i32,\n}\n#[cfg(not(any(unix, windows),))]\npub struct Handle {\n    pub id: u32,\n}\n#[cfg(windows)]\nimpl Handle {\n    pub fn raw_handle(&self) {}\n}\nimpl Handle {\n    pub fn shared(&self) {}\n}\n#[cfg(not(all(a, b,),))]\npub struct Pair;\n#[cfg(all(a, b))]\nimpl Pair {\n    pub fn both(&self) {}\n}\nimpl Pair {\n    pub fn either(&self) {}\n}\n",
        )],
    );
    let dep = |args: &[&str]| {
        let o = tc()
            .env("CARGO_HOME", r.join("cargo"))
            .args(["--no-stats", "--cwd"])
            .arg(&*r)
            .args(args)
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&o.stdout).into_owned(),
            o.status.code(),
        )
    };
    let found = |args: &[&str], has: &[&str], hasnt: &[&str]| {
        let (out, code) = dep(args);
        assert_eq!(code, Some(0), "{args:?}: {out}");
        for h in has {
            assert!(out.contains(h), "{args:?} lacks {h}: {out}");
        }
        for h in hasnt {
            assert!(!out.contains(h), "{args:?} has {h}: {out}");
        }
    };
    let missing = |args: &[&str]| {
        let (out, code) = dep(args);
        assert_eq!(code, Some(1), "{args:?}: {out}");
        assert!(out.contains("NOT FOUND"), "{args:?}: {out}");
        assert!(!out.contains("exported as"), "{args:?}: {out}");
    };
    // 1. `linked_list::Entry` has no `or_insert`: neither `hash_map`'s nor `hash_table`'s counts.
    for q in ["LinkedEntry.or_insert", "LinkedEntry::or_insert"] {
        missing(&["dep", "llist", q]);
    }
    for (q, at) in [
        ("take", "src/linked_list.rs:6"),
        ("via_super", "src/linked_list/sub.rs:2"),
        ("via_use", "src/user.rs:3"),
        ("via_glob", "src/user2.rs:3"),
    ] {
        found(
            &["dep", "llist", &format!("LinkedEntry.{q}")],
            &[
                &format!("[llist-0.1.0/{at}]"),
                &format!("(exported as `LinkedEntry.{q}`, declared as `Entry.{q}`)"),
            ],
            &[],
        );
    }
    found(
        &["dep", "globby", "LinkedEntry.take"],
        &[
            "pub fn take(&self)  [globby-0.1.0/src/imp.rs:3]",
            "(exported as `LinkedEntry.take`, declared as `Entry.take`)",
        ],
        &[],
    );
    // 2. The re-export's module path: `dir`'s `create` (a fn, or the module), never `symlink`'s.
    found(
        &["dep", "tmpf", "create_dir"],
        &[
            "re-exported from fsx 0.1.0",
            "pub fn create()  [fsx-0.1.0/src/dir/mod.rs:2]",
            "(exported as `create_dir`, declared as `create`)",
        ],
        &["symlink", "create.rs"],
    );
    found(
        &["dep", "tmpf", "make_dir"],
        &[
            "re-exported from fsm 0.1.0",
            "pub mod create;  [fsm-0.1.0/src/dir/mod.rs:1]",
            "(exported as `make_dir`, declared as `create`)",
        ],
        &["symlink", "create.rs"],
    );
    // A member of a type of that module, not of a same-named one beside or under it.
    found(
        &["dep", "tmpf", "T.go"],
        &[
            "re-exported from fsx 0.1.0",
            "pub fn go(&self)  [fsx-0.1.0/src/dir/mod.rs:5]",
            "(exported as `T.go`, declared as `Thing.go`)",
        ],
        &["symlink", "create.rs"],
    );
    // A glob `use` in that module: found where it leads.
    found(
        &["dep", "tmpf", "G"],
        &[
            "re-exported from fsg 0.1.0",
            "pub struct Thing;  [fsg-0.1.0/src/map.rs:1]",
            "(exported as `G`, declared as `Thing`)",
        ],
        &[],
    );
    // 3. Private, `pub(crate)` and `pub(super)` `use`s export nothing; a `pub use` does.
    for q in [
        "TermwizPosition",
        "CratePosition",
        "SuperPosition",
        "PrivInner",
    ] {
        missing(&["dep", "twz", q]);
    }
    found(
        &["dep", "twz", "PubPosition"],
        &[
            "re-exported from posx 0.1.0",
            "pub enum Position  [posx-0.1.0/src/lib.rs:1]",
            "(exported as `PubPosition`, declared as `Position`)",
        ],
        &[],
    );
    // 4. `not(any(unix, windows),)` contradicts `windows`; `not(all(a, b,),)` contradicts `all(a, b)`.
    let (out, code) = dep(&["--json", "dep", "cfgc", "Handle"]);
    assert_eq!(code, Some(0), "{out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    let members = |h: &Value| -> Vec<String> {
        h["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["name"].as_str().unwrap().to_string())
            .collect()
    };
    let twins: Vec<(u64, Vec<String>)> = v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| (h["line"].as_u64().unwrap(), members(h)))
        .collect();
    let s = |v: &[&str]| v.iter().map(|m| m.to_string()).collect::<Vec<_>>();
    assert_eq!(
        twins,
        [(2, s(&["fd", "shared"])), (6, s(&["id", "shared"]))],
        "{v}"
    );
    let (out, code) = dep(&["--json", "dep", "cfgc", "Pair"]);
    assert_eq!(code, Some(0), "{out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(members(&v["hits"][0]), s(&["either"]), "{v}");
}
