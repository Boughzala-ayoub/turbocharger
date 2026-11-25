//! MCP server over stdio: newline-delimited JSON-RPC 2.0. stdout carries protocol only; logs go to stderr.
use crate::{dep, run_recorded, stats, zoom};
use serde_json::{json, Map, Value};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};

/// The one protocol version served. Batching (2025-03-26) is not supported.
pub const PROTOCOL: &str = "2025-06-18";
/// Longest accepted frame (one message per line, newline excluded).
pub const MAX_FRAME: usize = 4 << 20;

fn tools() -> Value {
    json!([
        {
            "name": "turbo_zoom",
            "description": "Foveal code view. target='.' → repo overview; a dir → exported API skeleton (no bodies); a file → symbol table with line ranges + resolved import versions; 'file:Symbol' (or 'file:Class.method') → full body, internal callee signatures, installed third-party signatures used, and call sites across the repo. Looking for one thing in a dir or file? Pass `filter` instead of reading the whole skeleton. Use instead of reading whole files.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description": "'.', a directory, a file, or 'file:Symbol'"},
                    "cwd": {"type": "string", "description": "Project root (default: the server's --cwd)"},
                    "budget": {"type": "integer", "minimum": 0, "description": "Max output tokens (default 8000, 0 = unlimited)"},
                    "filter": {"type": "string", "description": "Dir or file targets only: keep symbols whose name or signature contains this text (Unicode case-insensitive), with their enclosing class/impl/mod header"}
                },
                "required": ["target"],
                "additionalProperties": false
            }
        },
        {
            "name": "turbo_dep",
            "description": "Ground-truth API of the INSTALLED version of a dependency, read from node_modules (.d.ts), the Python venv (.pyi/.py + dist-info) or the Cargo registry. Reads files only: no network, no package code is executed. Gives exact signatures and docs, and 'did you mean' suggestions when a symbol doesn't exist in the installed version. imports_file → cheat sheet for every third-party import of a file.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "package": {"type": "string", "description": "Package name, e.g. '@xterm/xterm', 'mcp', 'serde'"},
                    "symbol": {"type": "string", "description": "Symbol, e.g. 'Terminal', 'IBufferLine.getCell', 'MCPServer'"},
                    "imports_file": {"type": "string", "description": "File whose third-party imports to summarize"},
                    "cwd": {"type": "string", "description": "Project root (default: the server's --cwd)"}
                },
                "additionalProperties": false
            }
        },
        {
            "name": "turbo_stats",
            "description": "Tokens and quota saved by Turbocharger in this repo: calls, latency, emitted vs. estimated baseline tokens, multiplier.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "cwd": {"type": "string"},
                    "reset": {"type": "boolean"}
                },
                "additionalProperties": false
            }
        }
    ])
}

/// Run one tool. `Err` = invalid params (JSON-RPC -32602); `Ok((text, is_error))` otherwise.
fn call(
    root: &Path,
    name: &str,
    args: &Map<String, Value>,
    no_stats: bool,
) -> Result<(String, bool), String> {
    let allowed: &[&str] = match name {
        "turbo_zoom" => &["target", "cwd", "budget", "filter"],
        "turbo_dep" => &["package", "symbol", "imports_file", "cwd"],
        "turbo_stats" => &["cwd", "reset"],
        _ => return Err(format!("unknown tool: {name}")),
    };
    if name == "turbo_dep" && (args.contains_key("bin") || args.contains_key("flag")) {
        return Err("`bin`/`flag` are not available over MCP: they run an executable. Use the CLI: tc dep --bin <cmd> [flag]".into());
    }
    if let Some(k) = args.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(format!(
            "unknown argument `{k}` for {name} (allowed: {})",
            allowed.join(", ")
        ));
    }
    // Empty strings count as absent; explicit `null` is rejected for typed fields.
    let s = |k: &str| -> Result<Option<String>, String> {
        match args.get(k) {
            None => Ok(None),
            Some(Value::String(v)) => Ok(Some(v.clone()).filter(|v| !v.is_empty())),
            Some(_) => Err(format!("argument `{k}` must be a string")),
        }
    };
    let cwd = match s("cwd")? {
        Some(c) => dep::canon(&root.join(c)),
        None => root.to_path_buf(),
    };
    let res = match name {
        "turbo_zoom" => {
            let target = s("target")?.ok_or("missing required argument: target")?;
            let budget = match args.get("budget") {
                None => zoom::DEFAULT_BUDGET,
                Some(v) => v
                    .as_u64()
                    .map(|b| usize::try_from(b).unwrap_or(usize::MAX))
                    .ok_or("argument `budget` must be a non-negative integer")?,
            };
            let filter = s("filter")?;
            run_recorded(&cwd, "zoom", &target, no_stats, false, || {
                zoom::run(&cwd, &target, budget, false, filter.as_deref())
            })
        }
        "turbo_dep" => {
            let a = dep::DepArgs {
                package: s("package")?,
                symbol: s("symbol")?,
                imports: s("imports_file")?.map(PathBuf::from),
                bin: None,
                flag: None,
            };
            if a.package.is_none() && a.imports.is_none() {
                return Err(
                    "turbo_dep needs `package` (optionally with `symbol`) or `imports_file`".into(),
                );
            }
            if a.symbol.is_some() && a.package.is_none() {
                return Err("`symbol` needs `package`".into());
            }
            let target = [s("package")?, s("symbol")?, s("imports_file")?]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            run_recorded(&cwd, "dep", &target, no_stats, false, || dep::run(&cwd, &a))
        }
        _ => {
            let reset = match args.get("reset") {
                None => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err("argument `reset` must be a boolean".into()),
            };
            stats::run(&cwd, reset)
        }
    };
    Ok(match res {
        Ok(o) => (o.text, !o.ok),
        Err(e) => (format!("error: {e:#}"), true),
    })
}

fn error(id: Value, code: i64, message: impl Into<String>) -> Option<Value> {
    Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}}))
}

/// Answer one parsed message. `None` = nothing to send (notifications, client responses).
pub fn handle(msg: &Value, root: &Path, no_stats: bool) -> Option<Value> {
    let Some(obj) = msg.as_object() else {
        return error(
            Value::Null,
            -32600,
            "invalid request: expected one JSON object (batches are not supported)",
        );
    };
    // A response to us (we never send requests): nothing to answer.
    if !obj.contains_key("method") && (obj.contains_key("result") || obj.contains_key("error")) {
        return None;
    }
    let id = obj.get("id");
    let id_ok = matches!(id, Some(Value::String(_)))
        || matches!(id, Some(Value::Number(n)) if n.is_i64() || n.is_u64());
    let reply_id = if id_ok {
        id.cloned().unwrap_or_default()
    } else {
        Value::Null
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return error(
            reply_id,
            -32600,
            "invalid request: \"jsonrpc\" must be \"2.0\"",
        );
    }
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        return error(
            reply_id,
            -32600,
            "invalid request: \"method\" must be a string",
        );
    };
    match id {
        None => return None, // a notification never gets a reply
        Some(_) if !id_ok => {
            return error(
                Value::Null,
                -32600,
                "invalid request: \"id\" must be a string or an integer",
            );
        }
        _ => {}
    }
    let params = match obj.get("params") {
        None => None,
        Some(p) if p.is_object() => Some(p),
        Some(_) => {
            return error(
                reply_id,
                -32602,
                "invalid params: \"params\" must be an object",
            );
        }
    };
    let result = match method {
        "initialize" => json!({
            "protocolVersion": PROTOCOL,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "turbocharger", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Use turbo_zoom to look at code at the right zoom level instead of reading whole files, and turbo_dep to check the installed version of any third-party API before using it."
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            let Some(name) = params.and_then(|p| p.get("name")).and_then(Value::as_str) else {
                return error(
                    reply_id,
                    -32602,
                    "invalid params: tools/call needs a string \"name\"",
                );
            };
            let empty = Map::new();
            let args = match params.and_then(|p| p.get("arguments")) {
                None => &empty,
                Some(Value::Object(a)) => a,
                Some(_) => {
                    return error(
                        reply_id,
                        -32602,
                        "invalid params: \"arguments\" must be an object",
                    )
                }
            };
            match call(root, name, args, no_stats) {
                Ok((text, is_error)) => {
                    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
                }
                Err(m) => return error(reply_id, -32602, format!("invalid params: {m}")),
            }
        }
        _ => return error(reply_id, -32601, format!("method not found: {method}")),
    };
    Some(json!({"jsonrpc": "2.0", "id": reply_id, "result": result}))
}

/// Read one `\n`-terminated line of at most `max` bytes into `buf` (terminator kept).
/// `None` at EOF; `Some(false)` when the line was longer, in which case it is discarded through
/// its newline in bounded chunks and `buf` holds only its first `max + 1` bytes.
pub fn read_line_bounded(
    r: &mut impl BufRead,
    max: usize,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<bool>> {
    buf.clear();
    if r.by_ref().take(max as u64 + 1).read_until(b'\n', buf)? == 0 {
        return Ok(None);
    }
    if buf.len() <= max || buf.last() == Some(&b'\n') {
        return Ok(Some(true));
    }
    loop {
        let avail = r.fill_buf()?;
        if avail.is_empty() {
            break;
        }
        match avail.iter().position(|b| *b == b'\n') {
            Some(i) => {
                r.consume(i + 1);
                break;
            }
            None => {
                let n = avail.len();
                r.consume(n);
            }
        }
    }
    Ok(Some(false))
}

/// The reply to one raw frame (`None` for blank lines and notifications).
fn frame_reply(frame: &[u8], root: &Path, no_stats: bool) -> Option<Value> {
    let frame = frame.trim_ascii();
    if frame.is_empty() {
        return None;
    }
    match serde_json::from_slice::<Value>(frame) {
        Ok(msg) => handle(&msg, root, no_stats),
        Err(e) => error(Value::Null, -32700, format!("parse error: {e}")),
    }
}

/// Serve frames from `input` until EOF. A bad frame gets an error reply; the session goes on.
pub fn serve_io(
    mut input: impl BufRead,
    mut out: impl Write,
    root: &Path,
    no_stats: bool,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    while let Some(fits) = read_line_bounded(&mut input, MAX_FRAME, &mut buf)? {
        let reply = if fits {
            frame_reply(&buf, root, no_stats)
        } else {
            error(
                Value::Null,
                -32700,
                format!("parse error: frame exceeds {} MiB", MAX_FRAME >> 20),
            )
        };
        if let Some(r) = reply {
            writeln!(out, "{r}")?;
            out.flush()?;
        }
    }
    Ok(())
}

/// `root` (from `--cwd`) is the project root for tool calls that don't pass `cwd`.
pub fn serve(root: &Path, no_stats: bool) -> anyhow::Result<()> {
    eprintln!(
        "turbocharger MCP server {} on stdio (root {})",
        env!("CARGO_PKG_VERSION"),
        root.display()
    );
    serve_io(
        std::io::stdin().lock(),
        std::io::stdout().lock(),
        root,
        no_stats,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(input: &[u8]) -> Vec<Value> {
        let mut out = Vec::new();
        serve_io(input, &mut out, Path::new("."), true).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn bad_frames_get_errors_and_the_session_continues() {
        let ping = |id: u32| format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"ping\"}}\n");
        let mut input =
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"x\":\"\xff\"}\n".to_vec();
        input.extend(ping(2).as_bytes());
        input.extend(vec![b'a'; MAX_FRAME + 10]);
        input.push(b'\n');
        input.extend(ping(3).as_bytes());
        input.extend(b"\n   \n{not json\n");
        input.extend(ping(4).trim_end().as_bytes()); // last frame, no newline
        let r = session(&input);
        let codes: Vec<Value> = r.iter().map(|v| v["error"]["code"].clone()).collect();
        assert_eq!(
            codes,
            [
                json!(-32700),
                Value::Null,
                json!(-32700),
                Value::Null,
                json!(-32700),
                Value::Null
            ]
        );
        assert_eq!(
            (r[1]["id"].clone(), r[3]["id"].clone(), r[5]["id"].clone()),
            (json!(2), json!(3), json!(4))
        );
        assert!(r[2]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("exceeds 4 MiB"));
    }

    #[test]
    fn envelope_validation() {
        let h = |v: Value| handle(&v, Path::new("."), true);
        let code = |v: Value| h(v).unwrap()["error"]["code"].clone();
        assert_eq!(code(json!({})), -32600);
        assert_eq!(code(json!([])), -32600);
        assert_eq!(code(json!({"id": 1, "method": "ping"})), -32600);
        assert_eq!(
            code(json!({"jsonrpc": "1.0", "id": 1, "method": "ping"})),
            -32600
        );
        assert_eq!(
            code(json!({"jsonrpc": "2.0", "id": null, "method": "ping"})),
            -32600
        );
        assert_eq!(
            code(json!({"jsonrpc": "2.0", "id": [1], "method": "ping"})),
            -32600
        );
        assert_eq!(
            code(json!({"jsonrpc": "2.0", "id": 1, "method": 5})),
            -32600
        );
        assert_eq!(
            h(json!({"jsonrpc": "2.0", "id": 7, "method": "nope"})).unwrap()["id"],
            7
        );
        assert!(h(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none());
        assert!(h(json!({"jsonrpc": "2.0", "id": 1, "result": {}})).is_none());
        let init = h(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}})).unwrap();
        assert_eq!(init["result"]["protocolVersion"], PROTOCOL);
    }
}
