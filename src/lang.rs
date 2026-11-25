//! Tree-sitter parsing: symbols, imports and calls for TS/JS, Python, Rust and Java.
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser, Tree};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Ts,
    Tsx,
    Py,
    Rs,
    Java,
}

impl Lang {
    pub fn of(path: &Path) -> Option<Lang> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "ts" | "mts" | "cts" => Lang::Ts,
            "tsx" | "js" | "jsx" | "mjs" | "cjs" => Lang::Tsx,
            "py" | "pyi" => Lang::Py,
            "rs" => Lang::Rs,
            "java" => Lang::Java,
            _ => return None,
        })
    }

    /// Human label from the extension (`.js` parses with the TSX grammar but is JavaScript).
    pub fn label(path: &Path) -> &'static str {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match ext.as_str() {
            "ts" | "mts" | "cts" | "tsx" => "TypeScript",
            "js" | "jsx" | "mjs" | "cjs" => "JavaScript",
            "py" | "pyi" => "Python",
            "rs" => "Rust",
            "java" => "Java",
            _ => "other",
        }
    }

    fn grammar(self) -> tree_sitter::Language {
        match self {
            Lang::Ts => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::Py => tree_sitter_python::LANGUAGE.into(),
            Lang::Rs => tree_sitter_rust::LANGUAGE.into(),
            Lang::Java => tree_sitter_java::LANGUAGE.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Symbol {
    pub name: String,
    pub kind: &'static str,
    pub sig: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub doc: String,
    /// Line of the declaration itself (1-based).
    pub line: usize,
    /// First line including leading doc comments / decorators.
    pub start: usize,
    pub end: usize,
    pub exported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(skip)]
    pub bytes: (usize, usize),
    /// Byte where the leading doc comments / attributes start (`bytes.0` when there are none).
    #[serde(skip)]
    pub doc_start: usize,
    /// A data declaration's signature up to its initializer (`const x: T = …` → `const x: T`),
    /// cut at the parser's value node: what `--filter` matches. `None`: all of `sig` counts.
    #[serde(skip)]
    pub filter_sig: Option<String>,
    /// Rust: the item's module path in its crate (`a::b`, empty at the root): the file's
    /// (`Parsed::rs_mod`), then the inline `mod`s around the item.
    #[serde(skip)]
    pub module: String,
    /// Rust: the `#[cfg(..)]` conditions in force, as conjuncts (`rs_cfgs`): the item's own and
    /// those of the `impl`, type or `mod`s around it.
    #[serde(skip)]
    pub cfg: Vec<String>,
    /// Rust `impl` members: the impl's type as written, generics cut (`super::Foo`). `parent` is
    /// its last segment; `rs_owners` resolves it.
    #[serde(skip)]
    pub impl_ty: Option<String>,
}

impl Symbol {
    pub fn short_doc(&self) -> String {
        short_doc(&self.doc)
    }
    pub fn qual(&self) -> String {
        match &self.parent {
            Some(p) => format!("{p}.{}", self.name),
            None => self.name.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Import {
    pub module: String,
    /// (imported name, local name)
    pub names: Vec<(String, String)>,
    /// Local binding for the whole module (`import * as x`, `import numpy as np`, `import os`).
    pub alias: Option<String>,
    pub wildcard: bool,
    pub line: usize,
    /// `export ... from` (TS/JS): an export edge, not a local binding.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub reexport: bool,
}

#[derive(Debug, Clone)]
pub struct Call {
    pub name: String,
    pub recv: Option<String>,
}

pub struct Parsed {
    pub lang: Lang,
    pub src: String,
    pub tree: Tree,
    /// Rust: the file's module path in its crate (`rs_mod_path`); empty for a crate root.
    pub rs_mod: Vec<String>,
}

impl Parsed {
    pub fn new(lang: Lang, src: String) -> Option<Parsed> {
        let mut p = Parser::new();
        p.set_language(&lang.grammar()).ok()?;
        let tree = p.parse(&src, None)?;
        Some(Parsed {
            lang,
            src,
            tree,
            rs_mod: Vec::new(),
        })
    }

    /// Parse `src`, the text of the file at `path`.
    pub fn of(path: &Path, src: String) -> Option<Parsed> {
        let mut p = Parsed::new(Lang::of(path)?, src)?;
        if p.lang == Lang::Rs {
            p.rs_mod = rs_mod_path(path);
        }
        Some(p)
    }

    pub fn read(path: &Path) -> Option<Parsed> {
        Lang::of(path)?;
        Parsed::of(path, std::fs::read_to_string(path).ok()?)
    }

    pub fn text(&self, n: Node) -> &str {
        &self.src[n.byte_range()]
    }

    pub fn symbols(&self) -> Vec<Symbol> {
        let mut c = Collector {
            p: self,
            out: Vec::new(),
            depth: 0,
            module: self.rs_mod.clone(),
            cfg: Vec::new(),
            impl_ty: None,
            // `export { a }` / `export default a` make a top-level `a` public.
            local: self
                .local_exports()
                .into_iter()
                .map(|(orig, _, _)| orig)
                .filter(|o| o != "default")
                .collect(),
        };
        // TS/JS top level is private unless exported; Python is public by name; Rust/Java read modifiers.
        let top = !matches!(self.lang, Lang::Ts | Lang::Tsx);
        c.visit(self.tree.root_node(), None, "", top, false);
        c.out
    }

    pub fn imports(&self) -> Vec<Import> {
        let mut out = Vec::new();
        let mut stack = vec![self.tree.root_node()];
        while let Some(n) = stack.pop() {
            match (self.lang, n.kind()) {
                (Lang::Ts | Lang::Tsx, "import_statement") => self.ts_import(n, &mut out),
                (Lang::Ts | Lang::Tsx, "export_statement")
                    if n.child_by_field_name("source").is_some() =>
                {
                    self.ts_import(n, &mut out)
                }
                (Lang::Py, "import_statement" | "import_from_statement") => {
                    self.py_import(n, &mut out)
                }
                (Lang::Rs, "use_declaration") => self.rs_import(n, &mut out),
                (Lang::Java, "import_declaration") => {
                    let t = self
                        .text(n)
                        .trim_start_matches("import")
                        .trim()
                        .trim_end_matches(';');
                    let t = t.trim_start_matches("static ").trim().to_string();
                    let line = n.start_position().row + 1;
                    if let Some(m) = t.strip_suffix(".*") {
                        out.push(Import {
                            module: m.into(),
                            names: vec![],
                            alias: None,
                            wildcard: true,
                            line,
                            reexport: false,
                        });
                    } else {
                        let last = t.rsplit('.').next().unwrap_or(&t).to_string();
                        out.push(Import {
                            module: t,
                            names: vec![(last.clone(), last)],
                            alias: None,
                            wildcard: false,
                            line,
                            reexport: false,
                        });
                    }
                }
                _ => {
                    for i in (0..n.named_child_count()).rev() {
                        stack.push(n.named_child(i).unwrap());
                    }
                }
            }
        }
        out.sort_by_key(|i| i.line);
        out
    }

    /// TS/JS export edges without a `from`: (local name, public name, line of the export
    /// statement). Covers `export { a as b }`, `export default a`, `export default function f`,
    /// an anonymous default (`("default", "default")`) and `export = X` (public name `=`).
    pub fn local_exports(&self) -> Vec<(String, String, usize)> {
        let mut out = Vec::new();
        if !matches!(self.lang, Lang::Ts | Lang::Tsx) {
            return out;
        }
        let root = self.tree.root_node();
        for n in children(root)
            .into_iter()
            .filter(|n| n.kind() == "export_statement")
        {
            if n.child_by_field_name("source").is_some() {
                continue;
            }
            let line = n.start_position().row + 1;
            let mut c = n.walk();
            let tokens: Vec<&str> = n
                .children(&mut c)
                .filter(|x| !x.is_named())
                .map(|x| x.kind())
                .collect();
            if let Some(d) = n.child_by_field_name("declaration") {
                if tokens.contains(&"default") {
                    if let Some(name) = d.child_by_field_name("name") {
                        out.push((self.text(name).to_string(), "default".into(), line));
                    }
                }
            } else if let Some(v) = n.child_by_field_name("value") {
                let orig = if v.kind() == "identifier" {
                    self.text(v)
                } else {
                    "default"
                };
                out.push((orig.to_string(), "default".into(), line));
            } else if tokens.contains(&"=") {
                if let Some(x) = n.named_child(0).filter(|x| x.kind() == "identifier") {
                    out.push((self.text(x).to_string(), "=".into(), line));
                }
            } else {
                for clause in children(n)
                    .into_iter()
                    .filter(|x| x.kind() == "export_clause")
                {
                    for sp in children(clause) {
                        let Some(name) = sp.child_by_field_name("name") else {
                            continue;
                        };
                        let name = self.text(name).to_string();
                        let public = sp
                            .child_by_field_name("alias")
                            .map(|a| self.text(a).to_string())
                            .unwrap_or_else(|| name.clone());
                        out.push((name, public, line));
                    }
                }
            }
        }
        out
    }

    /// TS/JS: does the file have any top-level `export` or `import` statement?
    /// TypeScript treats a declaration file with any module syntax as an external module
    /// (only explicitly exported declarations are public), and one without as an ambient script.
    pub fn has_export_declarations(&self) -> bool {
        children(self.tree.root_node())
            .into_iter()
            .any(|n| matches!(n.kind(), "export_statement" | "import_statement"))
    }

    /// TS/JS: is `s` declared inside a top-level `export` statement?
    /// Returns `Some(true)` for `export default ...`, `Some(false)` for named exports (including
    /// multi-declarator `export const a = 1, b = 2`), and `None` when not directly exported.
    pub fn export_kind(&self, s: &Symbol) -> Option<bool> {
        let mut cur = self
            .tree
            .root_node()
            .descendant_for_byte_range(s.bytes.0, s.bytes.1)?;
        while let Some(p) = cur.parent() {
            if cur.kind() == "export_statement" && p.kind() == "program" {
                let mut w = cur.walk();
                let is_default = cur.children(&mut w).any(|c| c.kind() == "default");
                return Some(is_default);
            }
            cur = p;
        }
        None
    }

    fn ts_import(&self, n: Node, out: &mut Vec<Import>) {
        let Some(src) = n.child_by_field_name("source") else {
            return;
        };
        let module = unquote(self.text(src));
        let mut imp = Import {
            module,
            names: vec![],
            alias: None,
            wildcard: false,
            line: n.start_position().row + 1,
            reexport: n.kind() == "export_statement",
        };
        let mut stack: Vec<Node> = children(n);
        while let Some(c) = stack.pop() {
            match c.kind() {
                "import_clause" | "named_imports" | "export_clause" => stack.extend(children(c)),
                "identifier" if n.kind() == "import_statement" => {
                    imp.names.push(("default".into(), self.text(c).into()))
                }
                "namespace_import" | "namespace_export" => {
                    if let Some(id) = children(c).into_iter().find(|x| x.kind() == "identifier") {
                        imp.alias = Some(self.text(id).into());
                    } else {
                        imp.wildcard = true;
                    }
                }
                "import_specifier" | "export_specifier" => {
                    let name = c
                        .child_by_field_name("name")
                        .map(|x| self.text(x).to_string())
                        .unwrap_or_default();
                    let local = c
                        .child_by_field_name("alias")
                        .map(|x| self.text(x).to_string())
                        .unwrap_or(name.clone());
                    imp.names.push((name, local));
                }
                _ => {}
            }
        }
        let mut cur = n.walk();
        let has_star = n.children(&mut cur).any(|x| x.kind() == "*");
        if n.kind() == "export_statement" && has_star && imp.alias.is_none() {
            imp.wildcard = true; // export * from './x' (not `export {} from './x'`)
        }
        imp.names.reverse();
        out.push(imp);
    }

    fn py_import(&self, n: Node, out: &mut Vec<Import>) {
        let line = n.start_position().row + 1;
        if n.kind() == "import_statement" {
            let mut c = n.walk();
            for name in n.children_by_field_name("name", &mut c) {
                let (module, alias) = if name.kind() == "aliased_import" {
                    let m = name
                        .child_by_field_name("name")
                        .map(|x| self.text(x))
                        .unwrap_or("");
                    let a = name
                        .child_by_field_name("alias")
                        .map(|x| self.text(x))
                        .unwrap_or(m);
                    (m.to_string(), a.to_string())
                } else {
                    let m = self.text(name).to_string();
                    let first = m.split('.').next().unwrap_or(&m).to_string();
                    (m, first)
                };
                out.push(Import {
                    module,
                    names: vec![],
                    alias: Some(alias),
                    wildcard: false,
                    line,
                    reexport: false,
                });
            }
            return;
        }
        let module = n
            .child_by_field_name("module_name")
            .map(|x| self.text(x).to_string())
            .unwrap_or_default();
        let mut imp = Import {
            module,
            names: vec![],
            alias: None,
            wildcard: false,
            line,
            reexport: false,
        };
        let mut c = n.walk();
        for name in n.children_by_field_name("name", &mut c) {
            if name.kind() == "aliased_import" {
                let m = name
                    .child_by_field_name("name")
                    .map(|x| self.text(x))
                    .unwrap_or("");
                let a = name
                    .child_by_field_name("alias")
                    .map(|x| self.text(x))
                    .unwrap_or(m);
                imp.names.push((m.into(), a.into()));
            } else {
                let t = self.text(name).to_string();
                imp.names.push((t.clone(), t));
            }
        }
        imp.wildcard = children(n).iter().any(|c| c.kind() == "wildcard_import");
        out.push(imp);
    }

    fn rs_import(&self, n: Node, out: &mut Vec<Import>) {
        let Some(arg) = n.child_by_field_name("argument") else {
            return;
        };
        let mut leaves: Vec<(String, String)> = Vec::new();
        self.rs_use_tree(arg, String::new(), &mut leaves, 0);
        let line = n.start_position().row + 1;
        let mut by_crate: Vec<Import> = Vec::new();
        for (path, local) in leaves {
            let krate = path.split("::").next().unwrap_or("").to_string();
            let last = path.rsplit("::").next().unwrap_or("").to_string();
            let imp = match by_crate.iter_mut().find(|i| i.module == krate) {
                Some(i) => i,
                None => {
                    by_crate.push(Import {
                        module: krate,
                        names: vec![],
                        alias: None,
                        wildcard: false,
                        line,
                        reexport: false,
                    });
                    by_crate.last_mut().unwrap()
                }
            };
            if local == "*" {
                imp.wildcard = true;
            } else if path.contains("::") {
                imp.names.push((last, local));
            } else {
                imp.alias = Some(local); // `use serde_json;`
            }
        }
        out.extend(by_crate);
    }

    fn rs_use_tree(&self, n: Node, prefix: String, out: &mut Vec<(String, String)>, depth: usize) {
        if depth > MAX_NEST {
            return; // pathological nesting: stop instead of overflowing the stack
        }
        let path = |f| n.child_by_field_name(f).map(|x| self.rs_path(x));
        match n.kind() {
            "identifier" | "crate" | "self" | "super" | "scoped_identifier" => {
                let t = self.rs_path(n);
                let local = t.rsplit("::").next().unwrap_or(&t).to_string();
                out.push((format!("{prefix}{t}"), local));
            }
            "use_as_clause" => {
                let p = path("path").unwrap_or_default();
                let a = n
                    .child_by_field_name("alias")
                    .map(|x| self.text(x))
                    .unwrap_or("");
                out.push((format!("{prefix}{p}"), a.into()));
            }
            "scoped_use_list" => {
                let p = path("path").unwrap_or_default();
                let pre = if p.is_empty() {
                    prefix
                } else {
                    format!("{prefix}{p}::")
                };
                if let Some(l) = n.child_by_field_name("list") {
                    for c in children(l) {
                        self.rs_use_tree(c, pre.clone(), out, depth + 1);
                    }
                }
            }
            "use_list" => {
                for c in children(n) {
                    self.rs_use_tree(c, prefix.clone(), out, depth + 1);
                }
            }
            "use_wildcard" => {
                // `icon::{Other, *}`: a bare `*` in a list, whose prefix already ends in `::`.
                // Its path is its one child that isn't a comment (`a:: /* c */ *`).
                let p = match children(n)
                    .into_iter()
                    .find(|c| !c.kind().ends_with("comment"))
                {
                    Some(c) => format!("{prefix}{}::", self.rs_path(c)),
                    None => prefix,
                };
                out.push((format!("{p}*"), "*".into()));
            }
            _ => {}
        }
    }

    /// Rust: a path node as `a::b::c`, from its segments, without the spaces and comments
    /// written between them (`a :: /* c */ b`).
    fn rs_path(&self, n: Node) -> String {
        let mut segs = Vec::new();
        let mut cur = Some(n);
        while let Some(c) = cur {
            if c.kind() != "scoped_identifier" {
                segs.push(self.text(c));
                break;
            }
            segs.push(c.child_by_field_name("name").map_or("", |x| self.text(x)));
            cur = c.child_by_field_name("path");
            if cur.is_none() {
                segs.push(""); // `::std`
            }
        }
        segs.reverse();
        segs.join("::")
    }

    /// Rust: what each module of this file binds at its top level (`RsItem`): the types it
    /// declares, its `use`s and its `mod`s (inline ones are walked into). `use`s inside `fn`
    /// bodies don't bind module names.
    fn rs_mod_items(&self) -> Vec<RsItem> {
        let mut out = Vec::new();
        let mut todo = vec![(self.tree.root_node(), self.rs_mod.join("::"), Vec::new())];
        while let Some((n, module, mod_cfg)) = todo.pop() {
            for c in children(n) {
                let name = c
                    .child_by_field_name("name")
                    .map(|x| self.text(x).to_string());
                let cfg = || [mod_cfg.clone(), rs_cfgs(self, c)].concat();
                let public = children(c)
                    .iter()
                    .any(|v| v.kind() == "visibility_modifier" && self.text(*v).trim() == "pub");
                let item = |name, path, mod_inline| RsItem {
                    module: module.clone(),
                    name,
                    path,
                    mod_inline,
                    cfg: cfg(),
                    public,
                };
                match (c.kind(), name) {
                    ("use_declaration", _) => {
                        let mut leaves = Vec::new();
                        if let Some(a) = c.child_by_field_name("argument") {
                            self.rs_use_tree(a, String::new(), &mut leaves, 0);
                        }
                        for (path, local) in leaves {
                            let path = path.strip_suffix("::*").unwrap_or(&path).to_string();
                            out.push(item(local, Some(path), None));
                        }
                    }
                    (
                        "struct_item" | "enum_item" | "union_item" | "type_item" | "trait_item",
                        Some(n),
                    ) => out.push(item(n, None, None)),
                    ("mod_item", Some(n)) => {
                        let body = c.child_by_field_name("body");
                        out.push(item(n.clone(), None, Some(body.is_some())));
                        if let Some(body) = body {
                            let sub = if module.is_empty() {
                                n
                            } else {
                                format!("{module}::{n}")
                            };
                            todo.push((body, sub, cfg()));
                        }
                    }
                    _ => {}
                }
            }
        }
        out
    }

    /// Calls made anywhere inside byte range `[from, to)`.
    pub fn calls(&self, from: usize, to: usize) -> Vec<Call> {
        let mut out = Vec::new();
        let mut stack = vec![self.tree.root_node()];
        while let Some(n) = stack.pop() {
            if n.end_byte() <= from || n.start_byte() >= to {
                continue;
            }
            match (self.lang, n.kind()) {
                (Lang::Ts | Lang::Tsx, "call_expression") => {
                    if let Some(f) = n.child_by_field_name("function") {
                        self.push_call(f, "member_expression", "property", "object", &mut out);
                    }
                }
                (Lang::Ts | Lang::Tsx, "new_expression") => {
                    if let Some(f) = n.child_by_field_name("constructor") {
                        if f.kind() == "identifier" {
                            out.push(Call {
                                name: self.text(f).into(),
                                recv: Some("new".into()),
                            });
                        }
                    }
                }
                (Lang::Py, "call") => {
                    if let Some(f) = n.child_by_field_name("function") {
                        self.push_call(f, "attribute", "attribute", "object", &mut out);
                    }
                }
                (Lang::Rs, "call_expression") => {
                    if let Some(mut f) = n.child_by_field_name("function") {
                        if f.kind() == "generic_function" {
                            f = f.child_by_field_name("function").unwrap_or(f);
                        }
                        if f.kind() == "scoped_identifier" {
                            self.push_call(f, "scoped_identifier", "name", "path", &mut out);
                        } else {
                            self.push_call(f, "field_expression", "field", "value", &mut out);
                        }
                    }
                }
                (Lang::Java, "method_invocation") => {
                    if let Some(name) = n.child_by_field_name("name") {
                        let recv = n
                            .child_by_field_name("object")
                            .map(|o| clip(self.text(o), 40));
                        out.push(Call {
                            name: self.text(name).into(),
                            recv,
                        });
                    }
                }
                (Lang::Java, "object_creation_expression") => {
                    if let Some(t) = n.child_by_field_name("type") {
                        let t = self.text(t);
                        out.push(Call {
                            name: t.split('<').next().unwrap_or(t).into(),
                            recv: Some("new".into()),
                        });
                    }
                }
                _ => {}
            }
            for i in 0..n.named_child_count() {
                stack.push(n.named_child(i).unwrap());
            }
        }
        out
    }

    fn push_call(
        &self,
        f: Node,
        member_kind: &str,
        name_field: &str,
        obj_field: &str,
        out: &mut Vec<Call>,
    ) {
        if f.kind() == "identifier" {
            out.push(Call {
                name: self.text(f).into(),
                recv: None,
            });
        } else if f.kind() == member_kind {
            if let Some(name) = f.child_by_field_name(name_field) {
                let recv = f
                    .child_by_field_name(obj_field)
                    .map(|o| clip(self.text(o), 40));
                out.push(Call {
                    name: self.text(name).into(),
                    recv,
                });
            }
        }
    }
}

/// Deepest nesting of declarations (modules, classes, namespaces, blocks) that is walked. Deeper
/// items are left out rather than risk a stack overflow on generated or hostile source.
pub const MAX_NEST: usize = 48;

struct Collector<'a> {
    p: &'a Parsed,
    out: Vec<Symbol>,
    depth: usize,
    local: HashSet<String>,
    /// Rust: the `mod`s, `#[cfg]`s and `impl` type around the item being visited.
    module: Vec<String>,
    cfg: Vec<String>,
    impl_ty: Option<String>,
}

const TS_ROUTE_VERBS: &[&str] = &[
    "get", "post", "put", "delete", "patch", "all", "use", "head", "options",
];

impl<'a> Collector<'a> {
    fn visit(
        &mut self,
        node: Node,
        parent: Option<&str>,
        pkind: &str,
        exported: bool,
        force: bool,
    ) {
        if self.depth >= MAX_NEST {
            return;
        }
        self.depth += 1;
        for ch in children(node) {
            self.item(ch, ch, parent, pkind, exported, force);
        }
        self.depth -= 1;
    }

    /// Top-level TS name listed in a local `export { ... }` / `export default name`.
    fn local_exp(&self, parent: Option<&str>, name: &str) -> bool {
        parent.is_none() && self.local.contains(name)
    }

    fn name_of(&self, n: Node) -> Option<String> {
        n.child_by_field_name("name")
            .map(|x| self.p.text(x).to_string())
    }

    #[allow(clippy::too_many_arguments)]
    fn item(
        &mut self,
        outer: Node,
        node: Node,
        parent: Option<&str>,
        pkind: &str,
        exp: bool,
        force: bool,
    ) {
        let p = self.p;
        let k = node.kind();
        match p.lang {
            Lang::Ts | Lang::Tsx => match k {
                "export_statement" => {
                    if let Some(d) = node.child_by_field_name("declaration") {
                        self.item(outer, d, parent, pkind, true, force);
                    } else if let Some(v) = node.child_by_field_name("value") {
                        // `export default function () {}` / `export default class {}`
                        let body = v.child_by_field_name("body");
                        match v.kind() {
                            "function_expression"
                            | "function"
                            | "generator_function"
                            | "arrow_function" => {
                                self.push(
                                    outer,
                                    v,
                                    "function",
                                    "default".into(),
                                    body,
                                    true,
                                    parent,
                                );
                            }
                            "class" => {
                                self.push(outer, v, "class", "default".into(), body, true, parent);
                                if let Some(b) = body {
                                    self.visit(b, Some("default"), "class", true, false);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                "ambient_declaration" => {
                    // `declare ...` in a .d.ts is API surface (global or module-level).
                    for c in children(node) {
                        self.item(outer, c, parent, pkind, true, force);
                    }
                }
                "expression_statement" => {
                    for c in children(node) {
                        if c.kind() == "internal_module" {
                            self.item(outer, c, parent, pkind, exp, force);
                        } else if c.kind() == "call_expression" && parent.is_none() {
                            self.ts_route(outer, c);
                        }
                    }
                }
                "function_declaration"
                | "generator_function_declaration"
                | "function_signature" => {
                    let kind = if parent.is_some() {
                        "method"
                    } else {
                        "function"
                    };
                    if let Some(n) = self.name_of(node) {
                        self.push(
                            outer,
                            node,
                            kind,
                            n,
                            node.child_by_field_name("body"),
                            exp || force,
                            parent,
                        );
                    }
                }
                "class_declaration" | "abstract_class_declaration" | "interface_declaration" => {
                    let kind = if k == "interface_declaration" {
                        "interface"
                    } else {
                        "class"
                    };
                    if let Some(n) = self.name_of(node) {
                        let body = node.child_by_field_name("body");
                        let e = exp || force || pkind == "ambient" || self.local_exp(parent, &n);
                        self.push(outer, node, kind, n.clone(), body, e, parent);
                        if let Some(b) = body {
                            self.visit(b, Some(&n), kind, e, false);
                        }
                    }
                }
                "type_alias_declaration" | "enum_declaration" => {
                    let kind = if k == "enum_declaration" {
                        "enum"
                    } else {
                        "type"
                    };
                    if let Some(n) = self.name_of(node) {
                        self.push(outer, node, kind, n, None, exp || force, parent);
                    }
                }
                "module" | "internal_module" => {
                    let Some(name) = node.child_by_field_name("name") else {
                        return;
                    };
                    let body = node.child_by_field_name("body");
                    if name.kind() == "string" {
                        // `declare module "pkg" { ... }`: everything inside is the package's API.
                        if let Some(b) = body {
                            self.visit(b, parent, "ambient", true, true);
                        }
                    } else {
                        let n = p.text(name).to_string();
                        let e = exp || force || self.local_exp(parent, &n);
                        self.push(outer, node, "namespace", n.clone(), body, e, parent);
                        if let Some(b) = body {
                            self.visit(b, Some(&n), "namespace", e, force);
                        }
                    }
                }
                "lexical_declaration" | "variable_declaration" => {
                    let decls: Vec<Node> = children(node)
                        .into_iter()
                        .filter(|c| c.kind() == "variable_declarator")
                        .collect();
                    let single = decls.len() == 1;
                    for d in decls {
                        let Some(name) = d
                            .child_by_field_name("name")
                            .filter(|n| n.kind() == "identifier")
                        else {
                            continue;
                        };
                        let value = d.child_by_field_name("value");
                        let is_fn = value.is_some_and(|v| {
                            matches!(
                                v.kind(),
                                "arrow_function"
                                    | "function_expression"
                                    | "function"
                                    | "generator_function"
                            )
                        });
                        let body = if is_fn {
                            value.and_then(|v| v.child_by_field_name("body"))
                        } else {
                            None
                        };
                        let kind = if is_fn { "function" } else { "const" };
                        let (span_outer, span_node) = if single { (outer, node) } else { (d, d) };
                        self.push(
                            span_outer,
                            span_node,
                            kind,
                            p.text(name).into(),
                            body,
                            exp || force,
                            parent,
                        );
                    }
                }
                "method_definition"
                | "method_signature"
                | "abstract_method_signature"
                | "public_field_definition"
                | "property_signature" => {
                    let Some(name) = node.child_by_field_name("name") else {
                        return;
                    };
                    let private = name.kind() == "private_property_identifier"
                        || children(node).iter().any(|c| {
                            c.kind() == "accessibility_modifier" && p.text(*c) != "public"
                        });
                    let kind = if k.contains("method") {
                        "method"
                    } else {
                        "property"
                    };
                    let body = node.child_by_field_name("body");
                    self.push(
                        outer,
                        node,
                        kind,
                        p.text(name).into(),
                        body,
                        exp && !private,
                        parent,
                    );
                }
                _ => {}
            },
            Lang::Py => match k {
                "decorated_definition" => {
                    if let Some(d) = node.child_by_field_name("definition") {
                        self.item(outer, d, parent, pkind, exp, force);
                    }
                }
                "function_definition" | "class_definition" => {
                    let Some(n) = self.name_of(node) else { return };
                    let public = !n.starts_with('_') || (n.starts_with("__") && n.ends_with("__"));
                    let e = exp && public;
                    let body = node.child_by_field_name("body");
                    if k == "class_definition" {
                        self.push(outer, node, "class", n.clone(), body, e, parent);
                        if let Some(b) = body {
                            self.visit(b, Some(&n), "class", e, false);
                        }
                    } else {
                        let kind = if pkind == "class" {
                            "method"
                        } else {
                            "function"
                        };
                        self.push(outer, node, kind, n, body, e, parent);
                    }
                }
                "expression_statement" if pkind != "function" => {
                    let Some(a) = node.named_child(0).filter(|a| a.kind() == "assignment") else {
                        return;
                    };
                    let Some(left) = a
                        .child_by_field_name("left")
                        .filter(|l| l.kind() == "identifier")
                    else {
                        return;
                    };
                    let n = p.text(left).to_string();
                    let e = exp && !n.starts_with('_');
                    self.push(outer, node, "var", n, None, e, parent);
                }
                "if_statement" | "try_statement" | "else_clause" | "elif_clause"
                | "except_clause" | "finally_clause" | "block" => {
                    self.visit(node, parent, pkind, exp, force);
                }
                _ => {}
            },
            Lang::Rs => {
                // Only plain `pub` inside a public module chain (`exp`) is public API:
                // `pub(crate)`, `pub(super)` and `pub(in ..)` are not.
                let vis = children(node)
                    .iter()
                    .any(|c| c.kind() == "visibility_modifier" && p.text(*c).trim() == "pub");
                let e = (vis && exp) || force;
                // The item's `#[cfg]`s hold for it and everything visited inside it.
                let cfg_base = self.cfg.len();
                self.cfg.extend(rs_cfgs(p, outer));
                match k {
                    "function_item" | "function_signature_item" => {
                        let kind = if parent.is_some() { "method" } else { "fn" };
                        if let Some(n) = self.name_of(node) {
                            self.push(
                                outer,
                                node,
                                kind,
                                n,
                                node.child_by_field_name("body"),
                                e,
                                parent,
                            );
                        }
                    }
                    // `associated_type`: a trait's `type Error: StdError;` (no `= ..`), public as
                    // the trait is. One arm with `type_item`: a new arm grows this recursive
                    // function's debug-build stack frame (`f05_deep_nesting_does_not_overflow`).
                    "struct_item" | "enum_item" | "union_item" | "type_item"
                    | "associated_type" | "const_item" | "static_item" | "macro_definition" => {
                        let kind = k.trim_end_matches("_item").trim_end_matches("_definition");
                        let kind = match kind {
                            "struct" => "struct",
                            "enum" => "enum",
                            "union" => "union",
                            "type" | "associated_type" => "type",
                            "const" => "const",
                            "static" => "static",
                            _ => "macro",
                        };
                        if let Some(n) = self.name_of(node) {
                            let body = node
                                .child_by_field_name("body")
                                .filter(|_| kind != "const" && kind != "static");
                            let e = e || kind == "macro";
                            self.push(outer, node, kind, n.clone(), body, e, parent);
                            // Named fields, public only as plain `pub` in a public struct.
                            if let Some(b) = body.filter(|b| b.kind() == "field_declaration_list") {
                                self.visit(b, Some(&n), kind, e, false);
                            }
                        }
                    }
                    "field_declaration" if pkind == "struct" || pkind == "union" => {
                        if let Some(n) = self.name_of(node) {
                            self.push(outer, node, "field", n, None, e, parent);
                        }
                    }
                    "trait_item" | "mod_item" => {
                        let kind = if k == "trait_item" { "trait" } else { "mod" };
                        if let Some(n) = self.name_of(node) {
                            let body = node.child_by_field_name("body");
                            self.push(outer, node, kind, n.clone(), body, e, parent);
                            if let Some(b) = body {
                                if kind == "trait" {
                                    self.visit(b, Some(&n), kind, e, e);
                                } else {
                                    self.module.push(n.clone());
                                    self.visit(b, None, kind, e, false);
                                    self.module.pop();
                                }
                            }
                        }
                    }
                    "impl_item" => {
                        let ty = node
                            .child_by_field_name("type")
                            .map(|t| p.text(t))
                            .unwrap_or("");
                        let ty = ty.split('<').next().unwrap_or(ty).trim();
                        // `impl Trait for super::Foo`: the owner is `Foo`; `rs_owners` resolves the path.
                        let name = ty.rsplit("::").next().unwrap_or(ty).trim().to_string();
                        let is_trait = node.child_by_field_name("trait").is_some();
                        if let Some(b) = node.child_by_field_name("body") {
                            let outer_ty = self.impl_ty.replace(ty.to_string());
                            self.visit(b, Some(&name), "impl", exp, is_trait && exp);
                            self.impl_ty = outer_ty;
                        }
                    }
                    _ => {}
                }
                self.cfg.truncate(cfg_base);
            }
            Lang::Java => match k {
                "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
                | "annotation_type_declaration" => {
                    let kind = match k {
                        "interface_declaration" | "annotation_type_declaration" => "interface",
                        "enum_declaration" => "enum",
                        "record_declaration" => "record",
                        _ => "class",
                    };
                    let Some(n) = self.name_of(node) else { return };
                    let e = self.java_public(node, pkind);
                    let body = node.child_by_field_name("body");
                    self.push(outer, node, kind, n.clone(), body, e, parent);
                    if let Some(b) = body {
                        self.visit(b, Some(&n), kind, e, false);
                    }
                }
                "enum_body_declarations" => self.visit(node, parent, pkind, exp, force),
                "field_declaration" | "constant_declaration" => {
                    let e = exp && self.java_public(node, pkind);
                    let decls: Vec<Node> = children(node)
                        .into_iter()
                        .filter(|c| c.kind() == "variable_declarator")
                        .collect();
                    // Shared modifiers and type: everything before the first declarator.
                    let head = decls.first().map_or("", |d| {
                        &p.src[outer.start_byte().min(d.start_byte())..d.start_byte()]
                    });
                    let head = collapse(&strip_line_comments(head, "//"));
                    for d in decls {
                        if let Some(n) = d.child_by_field_name("name") {
                            self.push(outer, node, "field", p.text(n).into(), None, e, parent);
                            // `final int port, tailnet;`: each name is searched with the shared
                            // head and its own declarator (`int a[]`), not its siblings or value.
                            let own = &p.src[d.start_byte()
                                ..d.child_by_field_name("value")
                                    .map_or(d.end_byte(), |v| v.start_byte())];
                            let own = collapse(own).trim_end_matches(['=', ' ']).to_string();
                            if let Some(s) = self.out.last_mut() {
                                s.filter_sig = Some(format!("{head} {own}").trim().to_string());
                            }
                        }
                    }
                }
                "method_declaration"
                | "constructor_declaration"
                | "compact_constructor_declaration" => {
                    let Some(n) = self.name_of(node) else { return };
                    let kind = if k == "method_declaration" {
                        "method"
                    } else {
                        "constructor"
                    };
                    let e = exp && self.java_public(node, pkind);
                    self.push(
                        outer,
                        node,
                        kind,
                        n,
                        node.child_by_field_name("body"),
                        e,
                        parent,
                    );
                }
                _ => {}
            },
        }
    }

    fn java_public(&self, node: Node, pkind: &str) -> bool {
        let mods = children(node).into_iter().find(|c| c.kind() == "modifiers");
        let words: Vec<&str> = mods
            .map(|m| self.p.text(m).split_whitespace().collect())
            .unwrap_or_default();
        words.contains(&"public") || (pkind == "interface" && !words.contains(&"private"))
    }

    fn ts_route(&mut self, outer: Node, call: Node) {
        let p = self.p;
        let Some(f) = call
            .child_by_field_name("function")
            .filter(|f| f.kind() == "member_expression")
        else {
            return;
        };
        let Some(prop) = f.child_by_field_name("property") else {
            return;
        };
        let verb = p.text(prop);
        if !TS_ROUTE_VERBS.contains(&verb) {
            return;
        }
        let Some(args) = call.child_by_field_name("arguments") else {
            return;
        };
        let Some(first) = args
            .named_child(0)
            .filter(|a| a.kind() == "string" || a.kind() == "template_string")
        else {
            return;
        };
        let path = unquote(p.text(first));
        if !path.starts_with('/') {
            return;
        }
        let name = format!("{} {}", verb.to_uppercase(), path);
        self.push(outer, call, "route", name, None, true, None);
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        outer: Node,
        node: Node,
        kind: &'static str,
        name: String,
        body: Option<Node>,
        exported: bool,
        parent: Option<&str>,
    ) {
        let p = self.p;
        let sig_end = body
            .map(|b| b.start_byte())
            .unwrap_or(node.end_byte())
            .max(outer.start_byte());
        let marker = if p.lang == Lang::Py { "#" } else { "//" };
        let mut sig = collapse(&strip_line_comments(
            &p.src[outer.start_byte()..sig_end],
            marker,
        ));
        if body.is_some() {
            sig = sig.trim_end_matches(['{', ':']).trim_end().to_string();
        }
        if kind == "macro" {
            sig = format!("macro_rules! {name}");
        }
        if sig.chars().count() > 4000 {
            sig = clip(&sig, 4000);
        }
        // The initializer: `value` (TS/JS declarator or field, Rust const/static, Java declarator)
        // or `right` (Python assignment), on the node or a direct child (the declarator of a
        // one-declarator statement, the first of a Java field's).
        let filter_sig = matches!(kind, "const" | "var" | "static" | "property" | "field")
            .then(|| {
                std::iter::once(node).chain(children(node)).find_map(|n| {
                    n.child_by_field_name("value")
                        .or_else(|| n.child_by_field_name("right"))
                })
            })
            .flatten()
            .filter(|v| v.start_byte() >= outer.start_byte())
            .map(|v| {
                let head = strip_line_comments(&p.src[outer.start_byte()..v.start_byte()], marker);
                collapse(&head).trim_end_matches(['=', ' ']).to_string()
            });
        let (doc, doc_row, doc_byte) = self.doc(outer, node, body);
        let exported = exported || self.local_exp(parent, &name);
        let line = node
            .child_by_field_name("name")
            .unwrap_or(node)
            .start_position()
            .row
            + 1;
        self.out.push(Symbol {
            name,
            kind,
            sig,
            doc,
            line,
            start: doc_row.min(outer.start_position().row) + 1,
            end: outer.end_position().row + 1,
            exported,
            parent: parent.map(String::from),
            bytes: (outer.start_byte(), outer.end_byte()),
            doc_start: doc_byte.min(outer.start_byte()),
            filter_sig,
            module: self.module.join("::"),
            cfg: self.cfg.clone(),
            impl_ty: self.impl_ty.clone(),
        });
    }

    /// (cleaned doc text, first row and first byte of leading comments/attributes)
    fn doc(&self, outer: Node, node: Node, body: Option<Node>) -> (String, usize, usize) {
        let p = self.p;
        let mut row = outer.start_position().row;
        let mut byte = outer.start_byte();
        if p.lang == Lang::Py {
            let ds = body
                .and_then(|b| b.named_child(0))
                .filter(|s| s.kind() == "expression_statement")
                .and_then(|s| s.named_child(0))
                .filter(|s| s.kind() == "string");
            return (
                ds.map(|s| clean_docstring(p.text(s))).unwrap_or_default(),
                row,
                byte,
            );
        }
        let _ = node;
        let mut parts = Vec::new();
        let mut cur = outer;
        while let Some(prev) = cur.prev_named_sibling() {
            let k = prev.kind();
            let is_comment = matches!(k, "comment" | "line_comment" | "block_comment");
            if !(is_comment || k == "attribute_item") || prev.end_position().row + 1 < row {
                break;
            }
            // A trailing comment on the previous statement's line is not ours.
            if let Some(pp) = prev.prev_sibling() {
                if is_comment
                    && pp.end_position().row == prev.start_position().row
                    && !matches!(pp.kind(), "comment" | "line_comment" | "block_comment")
                {
                    break;
                }
            }
            let t = p.text(prev);
            if t.starts_with("//!") {
                break;
            }
            if is_comment {
                parts.push(clean_comment(t));
            }
            row = prev.start_position().row;
            byte = prev.start_byte();
            cur = prev;
        }
        parts.reverse();
        (parts.join("\n").trim().to_string(), row, byte)
    }
}

pub fn children(n: Node) -> Vec<Node> {
    let mut c = n.walk();
    n.named_children(&mut c).collect()
}

/// Rust: the conditions of the `#[cfg(..)]` attributes right above `item` (among its other
/// attributes and comments), as conjuncts: `#[cfg(all(a, all(b, c)))] #[cfg(d)]` → `a b c d`.
/// Whitespace is removed so `a = "x"` and `a="x"` compare equal; negations go inward
/// (`cfg_conjuncts`).
fn rs_cfgs(p: &Parsed, item: Node) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = item.prev_named_sibling();
    while let Some(n) = cur.filter(|n| {
        matches!(
            n.kind(),
            "attribute_item" | "line_comment" | "block_comment"
        )
    }) {
        let t: String = p.text(n).split_whitespace().collect();
        if let Some(c) = t.strip_prefix("#[cfg(").and_then(|t| t.strip_suffix(")]")) {
            cfg_conjuncts(c, &mut out);
        }
        cur = n.prev_named_sibling();
    }
    out
}

/// Push the conjuncts of cfg predicate `c` (whitespace-free) onto `out`: `all(..)` is split at
/// its top-level commas, recursively; `any(..)` stays whole. Negations go inward (De Morgan):
/// `not(not(a))` is `a`, `not(any(a, b))` is `not(a)` and `not(b)`, `not(all(a, b))` is
/// `any(not(a), not(b))`. Trailing commas go (`not(any(a, b,),)` is `not(any(a,b))`), so equal
/// predicates compare equal.
fn cfg_conjuncts(mut c: &str, out: &mut Vec<String>) {
    while let Some(x) = cfg_not(c).and_then(cfg_not) {
        c = x;
    }
    if let Some(x) = cfg_not(c) {
        let negated =
            |args: Vec<&str>| -> Vec<String> { args.iter().map(|a| format!("not({a})")).collect() };
        if let Some(args) = cfg_list(x, "any") {
            return negated(args).iter().for_each(|n| cfg_conjuncts(n, out));
        }
        if let Some(args) = cfg_list(x, "all") {
            return match &negated(args)[..] {
                [one] => cfg_conjuncts(one, out),
                any => out.push(format!("any({})", any.join(","))), // `any()` never holds
            };
        }
        return out.push(format!("not({x})"));
    }
    if let Some(args) = cfg_list(c, "all") {
        return args.into_iter().for_each(|a| cfg_conjuncts(a, out));
    }
    match cfg_list(c, "any") {
        Some(args) => out.push(format!("any({})", args.join(","))),
        None if !c.is_empty() => out.push(c.to_string()),
        None => {}
    }
}

/// The non-empty arguments of cfg predicate `c` when it is `f(..)`: `any(a,b,)` → `a`, `b`.
fn cfg_list<'c>(c: &'c str, f: &str) -> Option<Vec<&'c str>> {
    let inner = c.strip_prefix(f)?.strip_prefix('(')?.strip_suffix(')')?;
    Some(
        cfg_args(inner)
            .into_iter()
            .filter(|a| !a.is_empty())
            .collect(),
    )
}

/// `x` of cfg predicate `not(x)` (or `not(x,)`).
fn cfg_not(c: &str) -> Option<&str> {
    match cfg_list(c, "not")?[..] {
        [x] => Some(x),
        _ => None,
    }
}

/// The arguments of a cfg predicate list (`a,f(b,c),k="x,y"`), split at its top-level commas.
fn cfg_args(list: &str) -> Vec<&str> {
    let (mut depth, mut quoted, mut prev, mut from, mut out) = (0i32, false, ' ', 0, vec![]);
    for (i, ch) in list.char_indices() {
        match ch {
            '"' if prev != '\\' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => depth -= 1,
            ',' if !quoted && depth == 0 => {
                out.push(&list[from..i]);
                from = i + 1;
            }
            _ => {}
        }
        prev = ch;
    }
    out.push(&list[from..]);
    out
}

/// Rust: the module path of the file at `path` in its crate, from where it sits under the
/// nearest directory holding a `Cargo.toml` (`rs_target_mod`). No `Cargo.toml`: a crate root.
/// A custom `[lib] path` (`rs_lib_rel`) is the root, and its directory stands in for `src/`.
pub fn rs_mod_path(path: &Path) -> Vec<String> {
    rs_crate_rel(path).map_or_else(Vec::new, |(root, mut rel)| {
        let lib = rs_lib_rel(&root);
        if rel == lib {
            return vec![];
        }
        let dir = &lib[..lib.len() - 1];
        if dir != ["src"] && rs_in_lib_dir(dir, &rel) {
            rel.splice(..dir.len(), ["src".to_string()]);
        }
        rs_target_mod(&rel.iter().map(String::as_str).collect::<Vec<_>>())
    })
}

/// Rust: whether the file at `rel` (components under its crate directory) is in the directory
/// `dir` of the library root (`rs_lib_rel` without its file). For a root in the crate directory
/// itself (`[lib] path = "lib.rs"`), the other targets' places aren't: `src/`, `tests/`,
/// `examples/`, `benches/`, `target/` and `build.rs`.
pub fn rs_in_lib_dir(dir: &[String], rel: &[String]) -> bool {
    rel.len() > dir.len()
        && rel.starts_with(dir)
        && (!dir.is_empty()
            || !matches!(
                rel[0].as_str(),
                "src" | "tests" | "examples" | "benches" | "target" | "build.rs"
            ))
}

/// A `Cargo.toml`'s `rs_lib_rel`, with the mtime it was read at.
type RsLib = (Option<std::time::SystemTime>, Vec<String>);

thread_local! {
    /// `rs_lib_rel` by `Cargo.toml` path, checked by mtime.
    static RS_LIB: std::cell::RefCell<std::collections::HashMap<PathBuf, RsLib>> =
        Default::default();
}

/// Rust: the library root of the crate in directory `dir`, relative to it, as path components:
/// `[lib] path` in its `Cargo.toml` (`binding_rust/lib.rs`), else `src/lib.rs`.
pub fn rs_lib_rel(dir: &Path) -> Vec<String> {
    let toml = dir.join("Cargo.toml");
    let mtime = std::fs::metadata(&toml).and_then(|m| m.modified()).ok();
    if let Some(hit) =
        RS_LIB.with_borrow(|c| c.get(&toml).filter(|e| e.0 == mtime).map(|e| e.1.clone()))
    {
        return hit;
    }
    let rel: Vec<String> = std::fs::read_to_string(&toml)
        .ok()
        .and_then(|t| cargo_lib_path(&t))
        .map(|p| {
            p.split(['/', '\\'])
                .filter(|s| !s.is_empty() && *s != ".")
                .map(String::from)
                .collect()
        })
        .filter(|r: &Vec<String>| !r.is_empty())
        .unwrap_or_else(|| vec!["src".into(), "lib.rs".into()]);
    RS_LIB.with_borrow_mut(|c| c.insert(toml, (mtime, rel.clone())));
    rel
}

/// `path = ".."` of the `[lib]` table of Cargo.toml text `t`: not `[package]`'s, `[[bin]]`'s or a
/// dependency's, nor one in a comment.
// ponytail: line-based; `lib.path = ..` or `lib = { path = .. }` inline forms aren't read.
fn cargo_lib_path(t: &str) -> Option<String> {
    let mut in_lib = false;
    for l in t.lines().map(str::trim) {
        if l.starts_with('[') {
            let head: String = l
                .split('#')
                .next()
                .unwrap_or("")
                .split_whitespace()
                .collect();
            in_lib = head == "[lib]";
        } else if let Some(v) = l
            .strip_prefix("path")
            .filter(|_| in_lib)
            .and_then(|v| v.trim_start().strip_prefix('='))
        {
            let v = v.trim_start();
            let q = v.chars().next().filter(|c| *c == '"' || *c == '\'')?;
            return v[1..].split(q).next().map(String::from);
        }
    }
    None
}

/// Rust: the nearest directory above `path` holding a `Cargo.toml`, and `path`'s components
/// relative to it.
fn rs_crate_rel(path: &Path) -> Option<(PathBuf, Vec<String>)> {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let root = abs
        .ancestors()
        .skip(1)
        .find(|d| d.join("Cargo.toml").is_file())?;
    let rel = abs
        .strip_prefix(root)
        .unwrap_or(&abs)
        .components()
        .filter_map(|c| c.as_os_str().to_str().map(String::from))
        .collect();
    Some((root.to_path_buf(), rel))
}

/// Rust: the root file (the library's, `src/lib.rs` or its `[lib] path`, or `src/main.rs`) of
/// the crate the file at `path` belongs to. A package can have both: each is its own root, and a
/// `foo.rs` or `foo/..` beside one belongs to the one that declares `mod foo;` (the library when
/// both or neither do). `None` outside their directories, and for `src/bin/` targets.
// ponytail: a `src/bin/`, `examples/` or `tests/` target's `crate::` paths aren't followed into
// its root file.
fn rs_crate_root(path: &Path) -> Option<PathBuf> {
    let (root, rel) = rs_crate_rel(path)?;
    if rel.len() > 2 && rel[..2] == ["src", "bin"] {
        return None;
    }
    let roots = [rs_lib_rel(&root), vec!["src".into(), "main.rs".into()]];
    let abs = |r: &[String]| root.join(r.iter().collect::<PathBuf>());
    if let Some(r) = roots.iter().find(|r| **r == rel) {
        return Some(abs(r));
    }
    // Each root whose directory holds `path`, and the top module `path` is in under it.
    let cands: Vec<(PathBuf, &str)> = roots
        .iter()
        .filter_map(|r| {
            let dir = &r[..r.len() - 1];
            let top = rel.get(dir.len()).filter(|_| rs_in_lib_dir(dir, &rel))?;
            Some((abs(r), top.trim_end_matches(".rs")))
        })
        .collect();
    let declares = |(r, top): &&(PathBuf, &str)| {
        rs_file_items(r).is_some_and(|it| {
            it.iter()
                .any(|i| i.mod_inline == Some(false) && i.module.is_empty() && i.name == *top)
        })
    };
    cands
        .iter()
        .find(declares)
        .or_else(|| cands.iter().find(|(r, _)| r.is_file()))
        .map(|(r, _)| r.clone())
}

/// Rust: a `use`/impl path (its segments) read in module `from`, as a module path of the crate.
/// `None` past the crate root or for `::extern_crate`.
fn rs_resolve<'s>(from: &[String], segs: impl IntoIterator<Item = &'s str>) -> Option<Vec<String>> {
    let mut out = from.to_vec();
    for seg in segs {
        match seg.trim() {
            "crate" => out.clear(),
            "self" => {}
            "super" => {
                out.pop()?;
            }
            "" => return None,
            s => out.push(s.to_string()),
        }
    }
    Some(out)
}

/// Rust: whether cfg conjuncts `a` and `b` can't both hold (`cfg_unsat` of the two together).
fn cfg_contradicts(a: &[String], b: &[String]) -> bool {
    cfg_unsat(&cfg_and(a, b))
}

/// Rust: whether cfg conjuncts `c` can't all hold: two of them clash (`cfg_clash`), or an
/// `any(..)` has no branch that can hold beside all the others (`not(a)` leaves `any(a, b)` only
/// `b`; nested `any`s split in turn).
fn cfg_unsat(c: &[String]) -> bool {
    cfg_unsat_at(c, 0)
}

fn cfg_unsat_at(c: &[String], depth: usize) -> bool {
    if c.iter()
        .enumerate()
        .any(|(i, x)| c[i + 1..].iter().any(|y| cfg_clash(x, y)))
    {
        return true;
    }
    // ponytail: a case split per `any`, so `k` of them cost branches^k; past 8 deep, assume it holds.
    if depth > 8 {
        return false;
    }
    let Some((i, inner)) = c.iter().enumerate().find_map(|(i, x)| {
        let inner = x.strip_prefix("any(")?.strip_suffix(')')?;
        Some((i, inner))
    }) else {
        return false;
    };
    let rest: Vec<String> = [&c[..i], &c[i + 1..]].concat();
    cfg_args(inner)
        .into_iter()
        .filter(|p| !p.is_empty()) // `any(a, b,)`
        .all(|p| {
            let mut branch = Vec::new();
            cfg_conjuncts(p, &mut branch);
            cfg_unsat_at(&cfg_and(&rest, &branch), depth + 1)
        })
}

/// Rust: whether cfg conjuncts `x` and `y` clash: one is `not` of the other (or of what the other
/// implies: `not(unix)` vs `target_os = "linux"`), or they pin two values of one single-valued key
/// (`target_os = "linux"` vs `"windows"`, `unix` vs `windows`).
fn cfg_clash(x: &str, y: &str) -> bool {
    let negates = |x: &str, y: &str| {
        x.strip_prefix("not(")
            .and_then(|x| x.strip_suffix(')'))
            .is_some_and(|x| {
                let fx = cfg_facts(x);
                x == y || (!fx.is_empty() && fx.iter().all(|f| cfg_facts(y).contains(f)))
            })
    };
    let fy = cfg_facts(y);
    negates(x, y)
        || negates(y, x)
        || cfg_facts(x)
            .iter()
            .any(|(k, v)| fy.iter().any(|(k2, v2)| k == k2 && v != v2))
}

/// Rust: the single-valued facts cfg atom `c` (whitespace-free) pins, as (key, value):
/// `target_os="linux"` → `target_os` is `linux` and the family `unix`; `windows` → the family
/// `windows`. Multi-valued keys (`feature`, `target_family = "wasm"`) pin nothing.
fn cfg_facts(c: &str) -> Vec<(&str, &str)> {
    // ponytail: the `unix`-family OSes that are certain; another one just never clashes with `windows`.
    const UNIX_OS: &[&str] = &[
        "linux",
        "android",
        "macos",
        "ios",
        "tvos",
        "watchos",
        "visionos",
        "freebsd",
        "openbsd",
        "netbsd",
        "dragonfly",
        "solaris",
        "illumos",
        "haiku",
        "redox",
        "aix",
        "hurd",
        "emscripten",
    ];
    const SINGLE: &[&str] = &[
        "target_os",
        "target_arch",
        "target_env",
        "target_endian",
        "target_pointer_width",
        "target_vendor",
        "target_abi",
    ];
    let Some((key, v)) = c.split_once('=').map(|(k, v)| (k, v.trim_matches('"'))) else {
        return match c {
            "unix" | "windows" => vec![("family", c)],
            _ => vec![],
        };
    };
    let family = match (key, v) {
        ("target_family", "unix" | "windows") => Some(v),
        ("target_os", "windows") => Some("windows"),
        ("target_os", os) if UNIX_OS.contains(&os) => Some("unix"),
        _ => None,
    };
    let mut out: Vec<_> = family.map(|f| ("family", f)).into_iter().collect();
    if SINGLE.contains(&key) {
        out.push((key, v));
    }
    out
}

/// Rust: a name bound at a module's top level (`Parsed::rs_mod_items`).
struct RsItem {
    /// The module's path in the crate (`a::b`, empty at the root).
    module: String,
    /// The local name; `*` for a glob.
    name: String,
    /// A `use`'s path as written (a glob's names the module); `None`: a declaration.
    path: Option<String>,
    /// A `mod name`: inline `{ .. }` (`Some(true)`) or out-of-line `mod name;` (`Some(false)`).
    mod_inline: Option<bool>,
    /// The `#[cfg]` conjuncts in force: the item's and its inline `mod`s'.
    cfg: Vec<String>,
    /// Plain `pub` (not `pub(crate)`, `pub(super)`, `pub(in ..)` or private).
    public: bool,
}

/// Rust: cfg conjuncts `a`, then those of `b` it lacks.
fn cfg_and(a: &[String], b: &[String]) -> Vec<String> {
    let mut out = a.to_vec();
    for c in b {
        if !out.contains(c) {
            out.push(c.clone());
        }
    }
    out
}

/// A module file's `rs_mod_items`, with the mtime they were read at.
type RsItems = (Option<std::time::SystemTime>, std::rc::Rc<Vec<RsItem>>);

thread_local! {
    /// The module files `rs_reexport` read, checked by mtime (`tc mcp` lives on while files change).
    static RS_ITEMS: std::cell::RefCell<std::collections::HashMap<PathBuf, RsItems>> =
        Default::default();
}

/// Rust: the `rs_mod_items` of `file`, cached by path and mtime.
fn rs_file_items(file: &Path) -> Option<std::rc::Rc<Vec<RsItem>>> {
    let mtime = std::fs::metadata(file).and_then(|m| m.modified()).ok();
    if let Some(hit) =
        RS_ITEMS.with_borrow(|c| c.get(file).filter(|e| e.0 == mtime).map(|e| e.1.clone()))
    {
        return Some(hit);
    }
    let items = std::rc::Rc::new(Parsed::read(file)?.rs_mod_items());
    RS_ITEMS.with_borrow_mut(|c| c.insert(file.to_path_buf(), (mtime, items.clone())));
    Some(items)
}

/// Rust: each file that may hold module `module` of the crate whose root file is `root`, its
/// items, and the cfgs under which it does: `cfg` and those of the `mod`s on the way. Each
/// segment follows its `mod` in the parent: an inline `mod a { .. }` stays in that file, `mod a;`
/// moves to `a.rs` or `a/mod.rs` (as does a module with no `mod` seen: `cfg_if!`, a missing file).
/// A `mod` whose cfgs contradict is skipped; one whose cfgs already hold is the only one; else
/// each is a candidate (`#[cfg(x)] mod a { .. }` beside `#[cfg(not(x))] mod a;`), and so is
/// `a.rs` when no `mod a;` was seen.
// ponytail: a file's own `#![cfg(x)]` isn't carried into its items; add it here if twins hide
// behind it.
fn rs_mod_file_items(
    root: &Path,
    module: &[String],
    cfg: &[String],
) -> Vec<(PathBuf, std::rc::Rc<Vec<RsItem>>, Vec<String>)> {
    let src = root.parent().unwrap_or(root);
    let read = |f: PathBuf| Some((rs_file_items(&f)?, f));
    let mut out = vec![(read(root.to_path_buf()), cfg.to_vec())];
    for k in 1..=module.len() {
        let (at, seg) = (module[..k - 1].join("::"), &module[k - 1]);
        let d: PathBuf = module[..k].iter().collect();
        let file = || {
            [
                src.join(&d).with_extension("rs"),
                src.join(&d).join("mod.rs"),
            ]
            .into_iter()
            .find(|f| f.is_file())
            .and_then(read)
        };
        out = out
            .into_iter()
            .flat_map(|(items, cfg)| {
                let mods: Vec<&RsItem> = items
                    .iter()
                    .flat_map(|(it, _)| it.iter())
                    .filter(|i| i.mod_inline.is_some() && i.module == at && i.name == *seg)
                    .collect();
                let mut ways: Vec<(bool, &[String])> = mods
                    .iter()
                    .filter(|i| !cfg_contradicts(&i.cfg, &cfg))
                    .map(|i| (i.mod_inline == Some(true), &i.cfg[..]))
                    .collect();
                if let Some(&w) = ways.iter().find(|w| w.1.iter().all(|c| cfg.contains(c))) {
                    ways = vec![w];
                } else if !mods.iter().any(|i| i.mod_inline == Some(false)) {
                    ways.push((false, &[]));
                }
                ways.into_iter()
                    .map(|(inline, c)| {
                        let items = if inline { items.clone() } else { file() };
                        (items, cfg_and(&cfg, c))
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
    }
    out.into_iter()
        .filter_map(|(items, cfg)| items.map(|(it, f)| (f, it, cfg)))
        .collect()
}

/// Rust: `crate::<module>::<name>`, named in an impl whose cfgs are `cfg`: follow that module's
/// items on disk (`pub use icon::Foo`, `icon::{Foo as Bar}`, `self::icon::*`, chains through
/// other modules and inline `mod`s) until `has` finds a declaration of `file` (canonical), in a
/// module the walk from the crate root lands in `file` for (`rs_mod_file_items`). Returns each
/// such module and name, with the cfgs of the impl and of every `mod` and `use` followed: every
/// viable branch, so `#[cfg(x)] pub use p::A as T;` beside `#[cfg(not(x))] pub use p::B as T;`
/// gives both `A` and `B`. Cfgs that can't all hold end the path, and items whose cfgs contradict
/// them are skipped. Named `use`s only bind the name where no declaration of it does, and a glob
/// where no declaration or named `use` does: never beside one whose cfgs already hold, else where
/// those are off (`not(x)` beside `#[cfg(x)] struct T;` or `#[cfg(x)] pub use a::T;`).
fn rs_reexport(
    root: &Path,
    file: &Path,
    module: Vec<String>,
    name: &str,
    cfg: Vec<String>,
    has: &dyn Fn(&str, &str, &[String]) -> bool,
    seen: &mut Vec<(Vec<String>, String, Vec<String>)>,
) -> Vec<(String, String, Vec<String>)> {
    let key = (module.clone(), name.to_string(), cfg.clone());
    if cfg_unsat(&cfg) || seen.len() > 64 || seen.contains(&key) {
        return vec![]; // never compiled, a `use` cycle, or a chain too long to be real
    }
    seen.push(key);
    let at = module.join("::");
    let mut out = Vec::new();
    for (f, items, cfg) in rs_mod_file_items(root, &module, &cfg) {
        // `#[cfg(x)] mod a { .. }` beside `#[cfg(not(x))] mod a;`: `a.rs` is `a` only under `not(x)`.
        if std::fs::canonicalize(&f).is_ok_and(|f| f == file) && has(&at, name, &cfg) {
            out.push((at.clone(), name.to_string(), cfg.clone()));
        }
        let mine: Vec<&RsItem> = items
            .iter()
            .filter(|i| i.module == at && !cfg_contradicts(&i.cfg, &cfg))
            .collect();
        let follow = |i: &RsItem, cfg: &[String]| {
            let to = rs_resolve(&module, i.path.as_deref()?.split("::"))?;
            Some((to, cfg_and(cfg, &i.cfg)))
        };
        let holds = |i: &&RsItem| i.cfg.iter().all(|c| cfg.contains(c));
        // `cfg` with the extra conjuncts of each of `of` negated: where none of them is on.
        let off_of = |of: &[&RsItem]| {
            let mut off = cfg.clone();
            for i in of {
                let extra: Vec<&str> = i
                    .cfg
                    .iter()
                    .filter(|c| !cfg.contains(c))
                    .map(String::as_str)
                    .collect();
                let mut not = Vec::new();
                cfg_conjuncts(&format!("not(all({}))", extra.join(",")), &mut not);
                off = cfg_and(&off, &not);
            }
            off
        };
        let named: Vec<&RsItem> = mine.iter().copied().filter(|i| i.name == name).collect();
        // A declaration binds the name alone where its cfgs hold: never beside a `use` of it.
        // `#[cfg(x)] struct T;` beside `pub use a::B as T;` leaves the `use` `not(x)`.
        let decls: Vec<&RsItem> = named
            .iter()
            .copied()
            .filter(|i| i.path.is_none() && i.mod_inline.is_none())
            .collect();
        if decls.iter().any(holds) {
            continue;
        }
        let uses_cfg = off_of(&decls);
        if !cfg_unsat(&uses_cfg) {
            // Every named `use` (`#[cfg]` twins: `pub use windows::Set;` beside `pub use unix::Set;`).
            for i in &named {
                let Some((mut to, cfg)) = follow(i, &uses_cfg) else {
                    continue;
                };
                if let Some(n) = to.pop() {
                    out.extend(rs_reexport(root, file, to, &n, cfg, has, seen));
                }
            }
        }
        if named.iter().any(holds) {
            continue;
        }
        let off = off_of(&named);
        if cfg_unsat(&off) {
            continue; // `#[cfg(x)]` and `#[cfg(not(x))]` twins leave the glob nothing
        }
        for i in mine.iter().filter(|i| i.name == "*") {
            // `#[cfg(x)] pub use a::*;` beside `#[cfg(x)] pub use b::T;`: never `a::T`.
            if cfg_contradicts(&off, &i.cfg) {
                continue;
            }
            let Some((to, cfg)) = follow(i, &off) else {
                continue;
            };
            out.extend(rs_reexport(root, file, to, name, cfg, has, seen));
        }
    }
    let mut uniq = Vec::new();
    for h in out {
        if !uniq.contains(&h) {
            uniq.push(h);
        }
    }
    uniq
}

/// Rust: the crate path a top-level `use` of crate root file `root` binds `name` to, through the
/// named `use`s of the modules it leads to: `pub use crate::value::Value;` → `value::Value`;
/// `pub use a::X as Y;` with `pub use b::Z as X;` in `a` → `a::b::Z`; a crate-root alias
/// (`pub use crate::Direction as EdgeDirection;`, `self::`, bare) → `Direction`. `None` for no
/// such `use`, or `::other::X`. Only a plain `pub use` makes a public name: a private or
/// `pub(crate)` `use` binds it inside the crate alone.
pub fn rs_use_path(root: &Path, name: &str) -> Option<Vec<String>> {
    let start = vec![name.to_string()];
    let path = rs_follow(root, start.clone(), true);
    (path != start).then_some(path)
}

/// Rust: crate path `path` (modules, then a name) of the crate whose root file is `root`, followed
/// through the named `use`s of the modules it leads to (`rs_use_path`). `public`: the first `use`
/// must be a plain `pub use`.
// ponytail: the first `use` of a name wins; `#[cfg]` twin aliases resolve to the first twin.
pub fn rs_follow(root: &Path, mut path: Vec<String>, public: bool) -> Vec<String> {
    for k in 0..16 {
        let Some((name, module)) = path.split_last() else {
            break;
        };
        let at = module.join("::");
        let next = rs_mod_file_items(root, module, &[])
            .iter()
            .find_map(|(_, items, _)| {
                let i = items.iter().find(|i| {
                    i.module == at
                        && i.name == *name
                        && i.path.is_some()
                        && (i.public || !public || k > 0)
                })?;
                rs_resolve(module, i.path.as_deref()?.split("::"))
            });
        match next {
            Some(next) if next != path => path = next,
            _ => break,
        }
    }
    path
}

/// Rust: whether module `module` of the crate whose root file is `root` has an item `name` that
/// is a declaration (`decl`: a type, trait or `mod`; fns aren't seen) or else a `use` (`*`: a glob).
pub fn rs_binds(root: &Path, module: &[String], name: &str, decl: bool) -> bool {
    let at = module.join("::");
    rs_mod_file_items(root, module, &[])
        .iter()
        .any(|(_, items, _)| {
            items
                .iter()
                .any(|i| i.module == at && i.name == name && i.path.is_none() == decl)
        })
}

/// Rust: the plain `pub use`s at the top of crate root file `root`, as (local name, path as
/// written); a glob's local name is `*` and its path the module's.
pub fn rs_pub_uses(root: &Path) -> Vec<(String, String)> {
    let Some(items) = rs_file_items(root) else {
        return vec![];
    };
    items
        .iter()
        .filter(|i| i.public && i.module.is_empty())
        .filter_map(|i| Some((i.name.clone(), i.path.clone()?)))
        .collect()
}

/// Rust: the crate path (modules, then the type) of the type member `m` of `file` belongs to, in
/// the crate whose root file is `root`, `use`s followed (`rs_follow`). Its `impl`'s type path is
/// read in the impl's module: `crate::a::T` and `super::T` directly, a bare `T` through what that
/// module binds `T` to (a declaration, a named `use`, else each glob's `T`). A field's or a trait
/// item's type is declared in its own module. Empty when unknown (`::ext::T`, nothing binds `T`).
pub fn rs_owner_paths(root: &Path, file: &Path, m: &Symbol) -> Vec<Vec<String>> {
    let Some(ty) = m.impl_ty.as_deref().or(m.parent.as_deref()) else {
        return vec![];
    };
    let module: Vec<String> = m
        .module
        .split("::")
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    let mut segs: Vec<&str> = ty.split("::").map(str::trim).collect();
    let name = segs.pop().unwrap_or("");
    let paths: Vec<Vec<String>> = if segs.is_empty() {
        let items = rs_file_items(file).unwrap_or_default();
        let here: Vec<&RsItem> = items.iter().filter(|i| i.module == m.module).collect();
        let named: Vec<&&RsItem> = here.iter().filter(|i| i.name == name).collect();
        let resolve = |i: &RsItem, last: Option<&str>| match i.path.as_deref() {
            None => Some([&module[..], &[name.to_string()]].concat()),
            Some(p) => rs_resolve(&module, p.split("::").chain(last)),
        };
        if named.is_empty() {
            here.iter()
                .filter(|i| i.name == "*")
                .filter_map(|i| resolve(i, Some(name)))
                .collect()
        } else {
            named.iter().filter_map(|i| resolve(i, None)).collect()
        }
    } else {
        rs_resolve(&module, segs.into_iter().chain([name]))
            .into_iter()
            .collect()
    };
    paths
        .into_iter()
        .map(|p| rs_follow(root, p, false))
        .collect()
}

/// Rust: the module path of a file from its path relative to the crate directory:
/// `src/lib.rs` → [], `src/a/mod.rs` → [a], `src/a/b.rs` → [a, b]. A `src/bin/`, `examples/`,
/// `tests/` or `benches/` target's own `x.rs` or `x/main.rs` is a root, `x/y.rs` is [y].
// ponytail: by Cargo's layout alone (`rs_mod_path` maps a custom `[lib] path` onto it); a
// `#[path = ..]` module or a `tests/common/mod.rs` comes out wrong (its `crate::` paths miss).
// Reading the parent's `mod` items would fix that.
fn rs_target_mod(rel: &[&str]) -> Vec<String> {
    let (rest, roots): (&[&str], &[&str]) = match rel {
        ["src", "bin", rest @ ..] | ["examples" | "tests" | "benches", rest @ ..] => match rest {
            [_, rest @ ..] if !rest.is_empty() => (rest, &["main.rs"]),
            _ => return vec![],
        },
        ["src", rest @ ..] => (rest, &["lib.rs", "main.rs"]),
        _ => return vec![], // `build.rs`, a target outside the standard layout
    };
    let Some((file, dirs)) = rest.split_last() else {
        return vec![];
    };
    let mut out: Vec<String> = dirs.iter().map(|d| d.to_string()).collect();
    if *file != "mod.rs" && !(dirs.is_empty() && roots.contains(file)) {
        out.push(file.trim_end_matches(".rs").to_string());
    }
    out
}

/// Rust: indexes of the declarations in `syms` (one file's symbols) that `impl` member `m`
/// belongs to. The impl's type path is resolved against the impl's module path in the crate
/// (`super::Foo`, `self::a::Foo`, `crate::a::Foo`); a qualified path that names no declaration of
/// this file has no owner here, unless it is a `crate::` path whose module re-exports a
/// declaration of this file (`pub use icon::Foo;` in `lib.rs`, `pub use crate::Foo;` in an
/// out-of-line child module, read from `file`'s crate on disk: `rs_reexport`; their cfgs count
/// as the impl's). A declaration there that the impl's cfgs don't imply (`#[cfg(x)] struct T;`)
/// is an owner beside those re-exports (`#[cfg(not(x))] pub use a::B as T;`). A bare `Foo` prefers the impl's own `mod`, else any `Foo` of the
/// file (`use super::Foo; impl Foo`). A declaration whose cfgs contradict the impl's (`x` vs
/// `not(x)`, `unix` vs `windows`: `cfg_contradicts`) is never its owner. Among `#[cfg]` twins of one name, an impl whose cfgs include a
/// twin's belongs to that twin; one matching none (or with no cfg) is shared by all.
pub fn rs_owners(syms: &[Symbol], m: &Symbol, file: Option<&Path>) -> Vec<usize> {
    let Some(ty) = m.impl_ty.as_deref() else {
        return vec![];
    };
    let mut segs: Vec<&str> = ty.split("::").map(str::trim).collect();
    let name = segs.pop().unwrap_or("");
    let impl_mod: Vec<String> = m
        .module
        .split("::")
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    // `None`: past the crate root (another file) or `::extern_crate::Foo`.
    let path = rs_resolve(&impl_mod, segs.iter().copied());
    let decls = |module: Option<&str>, name: &str| -> Vec<usize> {
        syms.iter()
            .enumerate()
            .filter(|(_, s)| {
                s.name == name
                    && s.parent.is_none()
                    && matches!(s.kind, "struct" | "enum" | "union" | "type" | "trait")
                    && module.is_none_or(|m| s.module == m)
            })
            .map(|(i, _)| i)
            .collect()
    };
    let mut found = match &path {
        Some(p) => decls(Some(&p.join("::")), name),
        None => vec![],
    };
    if found.is_empty() && segs.is_empty() {
        found = decls(None, name);
    }
    let fine = |i: &usize, cfg: &[String]| !cfg_contradicts(&syms[*i].cfg, cfg);
    // Re-exports add their cfgs to the impl's: `#[cfg(unix)] pub use unix::Set;` picks the unix
    // twin. Each re-export branch is its own (declarations, cfgs).
    // Disk is read only for a `crate::` path this file declares nothing under for every build
    // of the impl: `#[cfg(x)] struct T;` beside `#[cfg(not(x))] pub use a::B as T;` is `B` too.
    let holds = |i: &usize| syms[*i].cfg.iter().all(|c| m.cfg.contains(c));
    let read = !found.iter().any(|i| fine(i, &m.cfg) && holds(i));
    let mut branches = vec![(found, m.cfg.clone())];
    if read && segs.first() == Some(&"crate") {
        let canon = file.and_then(|f| std::fs::canonicalize(f).ok());
        if let (Some(p), Some(root), Some(f)) = (path, file.and_then(rs_crate_root), canon) {
            let has = |m: &str, n: &str, c: &[String]| decls(Some(m), n).iter().any(|i| fine(i, c));
            let hits = rs_reexport(&root, &f, p, name, m.cfg.clone(), &has, &mut Vec::new());
            branches.extend(hits.into_iter().map(|(m, n, c)| (decls(Some(&m), &n), c)));
        }
    }
    let mut out = Vec::new();
    for (mut found, cfg) in branches {
        found.retain(|i| fine(i, &cfg));
        let fits: Vec<usize> = found
            .iter()
            .copied()
            .filter(|&i| !syms[i].cfg.is_empty() && syms[i].cfg.iter().all(|c| cfg.contains(c)))
            .collect();
        out.extend(if fits.is_empty() { found } else { fits });
    }
    out.sort_unstable();
    out.dedup();
    out
}

pub fn unquote(s: &str) -> String {
    s.trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .to_string()
}

/// Remove `# ...` / `// ...` line comments (outside string literals) before collapsing a signature.
fn strip_line_comments(s: &str, marker: &str) -> String {
    s.lines()
        .map(|l| {
            let mut quote: Option<char> = None;
            let mut prev = ' ';
            for (i, c) in l.char_indices() {
                match quote {
                    Some(q) if c == q && prev != '\\' => quote = None,
                    Some(_) => {}
                    None if c == '"' || c == '\'' || c == '`' => quote = Some(c),
                    None if l[i..].starts_with(marker) => return &l[..i],
                    None => {}
                }
                prev = c;
            }
            l
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Clip a signature to `n` chars; when too long, drop leading `@Annotation(...)` / `@decorator` first
/// so the declaration itself survives.
pub fn sig_clip(sig: &str, n: usize) -> String {
    if sig.chars().count() <= n {
        return sig.to_string();
    }
    // Split off leading `@Annotation(...)` / `@decorator(...)` groups.
    let mut s = sig;
    let mut anns: Vec<&str> = Vec::new();
    while let Some(rest) = s.strip_prefix('@') {
        let id = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
            .unwrap_or(rest.len());
        let mut end = 1 + id;
        if rest[id..].starts_with('(') {
            let mut depth = 0;
            for (i, c) in rest[id..].char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = 1 + id + i + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
        anns.push(&s[..end.min(s.len())]);
        s = s[end.min(s.len())..].trim_start();
    }
    // Keep route-like annotations (`@GetMapping("/x")`, `@app.get("/x")`, `@mcp.tool()`); drop the rest.
    let routey = |a: &str| {
        let name = a.split('(').next().unwrap_or(a).to_ascii_lowercase();
        [
            "mapping",
            "route",
            ".get",
            ".post",
            ".put",
            ".delete",
            ".patch",
            ".tool",
            ".websocket",
        ]
        .iter()
        .any(|k| name.contains(k))
    };
    let mut kept: Vec<String> = anns
        .into_iter()
        .filter(|a| routey(a))
        .map(|a| clip(a, 60))
        .collect();
    kept.push(s.to_string());
    clip(&kept.join(" "), n)
}

pub fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
    t.push('…');
    t
}

fn clean_comment(t: &str) -> String {
    let t = t.trim();
    let t = t
        .strip_prefix("/**")
        .or_else(|| t.strip_prefix("/*"))
        .unwrap_or(t);
    let t = t.strip_suffix("*/").unwrap_or(t);
    t.lines()
        .map(|l| {
            let l = l.trim();
            let l = l
                .strip_prefix("///")
                .or_else(|| l.strip_prefix("//"))
                .unwrap_or(l);
            let l = l.strip_prefix('*').unwrap_or(l);
            l.trim()
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn clean_docstring(t: &str) -> String {
    let t = t.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    let t = t.trim_matches(|c| c == '"' || c == '\'');
    t.lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

pub fn short_doc(doc: &str) -> String {
    let para = doc.split("\n\n").next().unwrap_or("");
    let para = collapse(para);
    let cut = para.find(". ").map(|i| &para[..=i]).unwrap_or(&para);
    clip(cut, 140)
}

/// Is `s[i..i+len]` a whole identifier (not part of a longer one)? Uses full UAX #31
/// `ID_Continue` (`XID_Continue` + the 19 extra `ID_Continue` code points such as U+309B/U+309C)
/// so `αf`, `f\u{0301}`, `か\u{3099}`, and `か\u{309B}` never split identifiers.
pub fn word_at(s: &str, i: usize, len: usize) -> bool {
    let id = |c: char| {
        unicode_ident::is_xid_continue(c)
            || matches!(
                c,
                '_' | '$'
                    | '\u{200C}'
                    | '\u{200D}'
                    | '\u{037A}'
                    | '\u{309B}'
                    | '\u{309C}'
                    | '\u{FC5E}'
                    ..='\u{FC63}'
                        | '\u{FDFA}'
                        | '\u{FDFB}'
                        | '\u{FE70}'
                        | '\u{FE72}'
                        | '\u{FE74}'
                        | '\u{FE76}'
                        | '\u{FE78}'
                        | '\u{FE7A}'
                        | '\u{FE7C}'
                        | '\u{FE7E}'
            )
    };
    !s[..i].chars().next_back().is_some_and(id)
        && !s
            .get(i + len..)
            .and_then(|r| r.chars().next())
            .is_some_and(id)
}

/// Unicode full case folding (CaseFolding.txt, statuses C and F) for case-insensitive substring
/// search: `fold(a).contains(&fold(b))`. Per char, so `Σ` folds alike in any position (the str
/// `to_lowercase` final-sigma rule turns a word-final one into `ς`). `char::to_lowercase` gives
/// the C mappings except the lowercase letters folded below; the F rows expand (`ß` → `ss`).
/// One departure: `İ` and `ı` fold to plain `i` (as Turkic folding does for `İ`), so `İstanbul`
/// and `istanbul` match each other.
pub fn fold(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '\u{130}' {
            out.push('i'); // İ
            continue;
        }
        for l in c.to_lowercase() {
            fold_lower(l, &mut out);
        }
    }
    out
}

/// Fold one already-lowercased char into `out`.
fn fold_lower(c: char, out: &mut String) {
    let s: &str = match c {
        '\u{131}' => "i", // ı
        '\u{DF}' => "ss", // ß (ẞ lowercases to it)
        // C: lowercase letters that fold to another lowercase letter.
        '\u{B5}' => "\u{3BC}",
        '\u{17F}' => "s",
        '\u{345}' | '\u{1FBE}' => "\u{3B9}",
        '\u{3C2}' => "\u{3C3}", // ς → σ
        '\u{3D0}' => "\u{3B2}",
        '\u{3D1}' => "\u{3B8}",
        '\u{3D5}' => "\u{3C6}",
        '\u{3D6}' => "\u{3C0}",
        '\u{3F0}' => "\u{3BA}",
        '\u{3F1}' => "\u{3C1}",
        '\u{3F5}' => "\u{3B5}",
        '\u{1C80}' => "\u{432}",
        '\u{1C81}' => "\u{434}",
        '\u{1C82}' => "\u{43E}",
        '\u{1C83}' => "\u{441}",
        '\u{1C84}' | '\u{1C85}' => "\u{442}",
        '\u{1C86}' => "\u{44A}",
        '\u{1C87}' => "\u{463}",
        '\u{1C88}' => "\u{A64B}",
        '\u{1E9B}' => "\u{1E61}",
        // F: full foldings that expand.
        '\u{149}' => "\u{2BC}n",
        '\u{1F0}' => "j\u{30C}",
        '\u{390}' | '\u{1FD3}' => "\u{3B9}\u{308}\u{301}",
        '\u{3B0}' | '\u{1FE3}' => "\u{3C5}\u{308}\u{301}",
        '\u{587}' => "\u{565}\u{582}",
        '\u{1E96}' => "h\u{331}",
        '\u{1E97}' => "t\u{308}",
        '\u{1E98}' => "w\u{30A}",
        '\u{1E99}' => "y\u{30A}",
        '\u{1E9A}' => "a\u{2BE}",
        '\u{1F50}' => "\u{3C5}\u{313}",
        '\u{1F52}' => "\u{3C5}\u{313}\u{300}",
        '\u{1F54}' => "\u{3C5}\u{313}\u{301}",
        '\u{1F56}' => "\u{3C5}\u{313}\u{342}",
        // U+1F80-87, 1F90-97, 1FA0-A7 (their capitals lowercase to these): vowel + ι.
        '\u{1F80}'..='\u{1F87}' | '\u{1F90}'..='\u{1F97}' | '\u{1FA0}'..='\u{1FA7}' => {
            let base = match c as u32 & 0xFFF0 {
                0x1F80 => 0x1F00,
                0x1F90 => 0x1F20,
                _ => 0x1F60,
            } + (c as u32 & 7);
            out.extend([char::from_u32(base).unwrap_or(c), '\u{3B9}']);
            return;
        }
        '\u{1FB2}' => "\u{1F70}\u{3B9}",
        '\u{1FB3}' => "\u{3B1}\u{3B9}",
        '\u{1FB4}' => "\u{3AC}\u{3B9}",
        '\u{1FB6}' => "\u{3B1}\u{342}",
        '\u{1FB7}' => "\u{3B1}\u{342}\u{3B9}",
        '\u{1FC2}' => "\u{1F74}\u{3B9}",
        '\u{1FC3}' => "\u{3B7}\u{3B9}",
        '\u{1FC4}' => "\u{3AE}\u{3B9}",
        '\u{1FC6}' => "\u{3B7}\u{342}",
        '\u{1FC7}' => "\u{3B7}\u{342}\u{3B9}",
        '\u{1FD2}' => "\u{3B9}\u{308}\u{300}",
        '\u{1FD6}' => "\u{3B9}\u{342}",
        '\u{1FD7}' => "\u{3B9}\u{308}\u{342}",
        '\u{1FE2}' => "\u{3C5}\u{308}\u{300}",
        '\u{1FE4}' => "\u{3C1}\u{313}",
        '\u{1FE6}' => "\u{3C5}\u{342}",
        '\u{1FE7}' => "\u{3C5}\u{308}\u{342}",
        '\u{1FF2}' => "\u{1F7C}\u{3B9}",
        '\u{1FF3}' => "\u{3C9}\u{3B9}",
        '\u{1FF4}' => "\u{3CE}\u{3B9}",
        '\u{1FF6}' => "\u{3C9}\u{342}",
        '\u{1FF7}' => "\u{3C9}\u{342}\u{3B9}",
        '\u{FB00}' => "ff",
        '\u{FB01}' => "fi",
        '\u{FB02}' => "fl",
        '\u{FB03}' => "ffi",
        '\u{FB04}' => "ffl",
        '\u{FB05}' | '\u{FB06}' => "st",
        '\u{FB13}' => "\u{574}\u{576}",
        '\u{FB14}' => "\u{574}\u{565}",
        '\u{FB15}' => "\u{574}\u{56B}",
        '\u{FB16}' => "\u{57E}\u{576}",
        '\u{FB17}' => "\u{574}\u{56D}",
        _ => {
            out.push(c);
            return;
        }
    };
    out.push_str(s);
}

pub fn has_word(s: &str, w: &str) -> bool {
    !w.is_empty() && s.match_indices(w).any(|(i, _)| word_at(s, i, w.len()))
}

/// Source files under `root`, honouring .gitignore and skipping dependency dirs. Ambiguous names
/// (`build`, `dist`, `env`, ...) are left to .gitignore; a virtualenv is spotted by its `pyvenv.cfg`,
/// and hidden dirs (`.git`, `.venv`, `.tox`, ...) are skipped as hidden.
pub fn walk_files(root: &Path, sources_only: bool) -> Vec<PathBuf> {
    use std::sync::Mutex;
    const SKIP: &[&str] = &[
        "node_modules",
        ".git",
        "target",
        ".venv",
        "__pycache__",
        "site-packages",
        ".turbocharger",
    ];
    let out = Mutex::new(Vec::new());
    ignore::WalkBuilder::new(root)
        .hidden(true)
        .require_git(false)
        .filter_entry(|e| {
            !(e.depth() > 0
                && e.file_type().is_some_and(|t| t.is_dir())
                && (SKIP.contains(&e.file_name().to_str().unwrap_or(""))
                    || e.path().join("pyvenv.cfg").is_file()))
        })
        .build_parallel()
        .run(|| {
            Box::new(|e| {
                if let Ok(e) = e {
                    if e.file_type().is_some_and(|t| t.is_file())
                        && (!sources_only || Lang::of(e.path()).is_some())
                    {
                        out.lock().unwrap().push(e.into_path());
                    }
                }
                ignore::WalkState::Continue
            })
        });
    let mut v = out.into_inner().unwrap();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn syms(lang: Lang, src: &str) -> Vec<Symbol> {
        Parsed::new(lang, src.into()).unwrap().symbols()
    }

    #[test]
    fn ts_exports_ambient_and_arrows() {
        let s = syms(
            Lang::Ts,
            "/** Adds. */\nexport function add(a: number, b: number): number { return a + b }\nconst hidden = 1;\nexport const mul = (a: number) => a * 2;\ndeclare module 'x' {\n  interface IBufferLine {\n    readonly isWrapped: boolean;\n    getCell(x: number): string;\n  }\n}\n",
        );
        let add = s.iter().find(|s| s.name == "add").unwrap();
        assert_eq!(
            (add.kind, add.start, add.line, add.exported),
            ("function", 1, 2, true)
        );
        assert_eq!(add.doc, "Adds.");
        assert!(add.sig.ends_with("): number"), "{}", add.sig);
        assert!(!s.iter().find(|s| s.name == "hidden").unwrap().exported);
        assert_eq!(s.iter().find(|s| s.name == "mul").unwrap().kind, "function");
        let ib = s.iter().find(|s| s.name == "IBufferLine").unwrap();
        assert!(ib.exported);
        assert!(s
            .iter()
            .any(|s| s.name == "getCell" && s.parent.as_deref() == Some("IBufferLine")));
    }

    #[test]
    fn dts_private_members_and_export_statement_lines() {
        let p = Parsed::new(
            Lang::Ts,
            "declare class Channel {\n    #private;\n    private cleanupCallback;\n    id: number;\n}\ndeclare function invoke(): void;\nexport { Channel, invoke as call };\nexport default Channel;\n".into(),
        )
        .unwrap();
        let s = p.symbols();
        let m = |n: &str| s.iter().find(|s| s.name == n).unwrap();
        // Collected with their lines, flagged non-public (`tc dep` shows them for `.d.ts` classes).
        assert_eq!((m("#private").line, m("#private").exported), (2, false));
        assert_eq!(
            (m("cleanupCallback").line, m("cleanupCallback").exported),
            (3, false)
        );
        assert!(m("id").exported);
        let ex = |a: &str, b: &str, l: usize| (a.to_string(), b.to_string(), l);
        assert_eq!(
            p.local_exports(),
            [
                ex("Channel", "Channel", 7),
                ex("invoke", "call", 7),
                ex("Channel", "default", 8)
            ]
        );
        assert_eq!(p.export_kind(m("Channel")), None);
    }

    #[test]
    fn python_decorators_docstrings_privacy() {
        let s = syms(Lang::Py, "@app.get('/x')\ndef route():\n    \"\"\"Serve x.\"\"\"\n    return 1\n\ndef _private(): pass\nclass A:\n    def __init__(self, n: int): ...\n    def m(self): ...\n");
        let r = s.iter().find(|s| s.name == "route").unwrap();
        assert_eq!((r.start, r.doc.as_str()), (1, "Serve x."));
        assert!(r.sig.starts_with("@app.get('/x') def route()"), "{}", r.sig);
        assert!(!s.iter().find(|s| s.name == "_private").unwrap().exported);
        assert!(s
            .iter()
            .any(|s| s.name == "__init__" && s.kind == "method" && s.exported));
    }

    #[test]
    fn rust_pub_and_impl_methods() {
        let s = syms(Lang::Rs, "/// Doc.\n#[derive(Debug)]\npub struct S { a: u8 }\nfn private() {}\nimpl S {\n    pub fn new() -> Self { S { a: 0 } }\n    fn hidden(&self) {}\n}\n");
        let st = s.iter().find(|s| s.name == "S").unwrap();
        assert_eq!((st.start, st.doc.as_str(), st.exported), (1, "Doc.", true));
        assert!(!s.iter().find(|s| s.name == "private").unwrap().exported);
        let new = s.iter().find(|s| s.name == "new").unwrap();
        assert_eq!((new.parent.as_deref(), new.exported), (Some("S"), true));
        assert!(!s.iter().find(|s| s.name == "hidden").unwrap().exported);
    }

    #[test]
    fn rust_impl_owners_follow_cfg_and_module_paths() {
        let src = r#"#[cfg(target_arch = "x86")]
struct S { a: u8 }
#[cfg(target_arch="x86")]
impl Default for S { fn default() -> Self { todo!() } }
#[cfg(not(target_arch = "x86"))]
struct S { b: u8 }
#[cfg(not( target_arch = "x86" ))]
impl Default for S { fn default() -> Self { todo!() } }
impl S { fn shared(&self) {} }
struct Lone;
#[cfg(unix)]
impl Lone { pub fn a() {} }
pub struct T;
mod a {
    pub struct T;
    impl super::T { fn top(&self) {} }
    impl self::T { fn here(&self) {} }
    impl T { fn bare(&self) {} }
    mod b {
        impl super::super::T { fn top2(&self) {} }
        impl super::other::T { fn nowhere(&self) {} }
        impl crate::a::T { fn via_crate(&self) {} }
    }
}
impl super::T { fn outside(&self) {} }
#[cfg(unix)]
mod sys { pub struct P; impl P { fn u(&self) {} } }
#[cfg(windows)]
mod sys { pub struct P; impl P { fn w(&self) {} } }
"#;
        let s = syms(Lang::Rs, src);
        // Owner declaration lines of the impl member declared on `line`.
        let owners = |line: usize| -> Vec<usize> {
            let m = s
                .iter()
                .find(|m| m.line == line && m.impl_ty.is_some())
                .unwrap();
            rs_owners(&s, m, None)
                .into_iter()
                .map(|i| s[i].line)
                .collect()
        };
        // `#[cfg]` twins: each impl goes to its own twin (whitespace aside); a cfg-less one to both.
        assert_eq!(
            (owners(4), owners(8), owners(9)),
            (vec![2], vec![6], vec![2, 6])
        );
        // A lone struct keeps a `#[cfg]` impl.
        assert_eq!(owners(12), [10]);
        // Qualified paths resolve against the `mod`s around the impl.
        assert_eq!(
            (owners(16), owners(17), owners(18)),
            (vec![13], vec![15], vec![15])
        );
        assert_eq!((owners(20), owners(22)), (vec![13], vec![15]));
        // `super::other::T` and a `super` past the file's top name no `T` of this file.
        assert!(owners(21).is_empty() && owners(25).is_empty());
        // A `#[cfg]` on the `mod` holds for what's inside it.
        assert_eq!((owners(27), owners(29)), (vec![27], vec![29]));
        let top = s.iter().find(|m| m.name == "top").unwrap();
        assert_eq!(
            (top.qual(), top.impl_ty.as_deref(), top.module.as_str()),
            ("T.top".into(), Some("super::T"), "a")
        );
    }

    /// Owner declaration lines of each impl member of `src`, parsed as the file at module `rs_mod`.
    fn owner_lines(src: &str, rs_mod: &[&str]) -> Vec<(String, Vec<usize>)> {
        let mut p = Parsed::new(Lang::Rs, src.into()).unwrap();
        p.rs_mod = rs_mod.iter().map(|s| s.to_string()).collect();
        let s = p.symbols();
        s.iter()
            .filter(|m| m.impl_ty.is_some())
            .map(|m| {
                let o = rs_owners(&s, m, None)
                    .into_iter()
                    .map(|i| s[i].line)
                    .collect();
                (m.name.clone(), o)
            })
            .collect()
    }

    #[test]
    fn rust_file_module_path_follows_cargo_layout() {
        let m = |rel: &str| rs_target_mod(&rel.split('/').collect::<Vec<_>>());
        let none: Vec<String> = vec![];
        for root in [
            "src/lib.rs",
            "src/main.rs",
            "src/bin/x.rs",
            "src/bin/x/main.rs",
            "examples/x.rs",
            "examples/x/main.rs",
            "tests/cli.rs",
            "benches/b.rs",
            "build.rs",
            "other/x.rs",
        ] {
            assert_eq!(m(root), none, "{root}");
        }
        assert_eq!(m("src/foo/mod.rs"), ["foo"]);
        assert_eq!(m("src/platform/windows.rs"), ["platform", "windows"]);
        assert_eq!(
            m("src/operation/create_o_auth2_token.rs"),
            ["operation", "create_o_auth2_token"]
        );
        assert_eq!(m("src/a/lib.rs"), ["a", "lib"]);
        assert_eq!(m("src/bin/x/util.rs"), ["util"]);
        assert_eq!(m("examples/x/util/mod.rs"), ["util"]);
    }

    #[test]
    fn rust_crate_paths_resolve_against_the_file_module() {
        let src = "pub struct Set;\nimpl crate::Set<'_> { fn root(&self) {} }\nimpl crate::platform::windows::Set { fn full(&self) {} }\nimpl super::Set { fn parent(&self) {} }\nimpl self::Set { fn here(&self) {} }\nimpl Set { fn bare(&self) {} }\nmod inner { impl super::Set { fn up(&self) {} } impl crate::platform::windows::Set { fn deep(&self) {} } }\n";
        let o = |name: &str, n: Vec<usize>| (name.to_string(), n);
        // `src/platform/windows.rs`: `crate::Set` and `super::Set` live in other files.
        assert_eq!(
            owner_lines(src, &["platform", "windows"]),
            [
                o("root", vec![]),
                o("full", vec![1]),
                o("parent", vec![]),
                o("here", vec![1]),
                o("bare", vec![1]),
                o("up", vec![1]),
                o("deep", vec![1]),
            ]
        );
        // `src/lib.rs`: `crate::Set` is this one, `super` goes past the crate root.
        let lib = owner_lines(src, &[]);
        assert_eq!(
            (&lib[0], &lib[1], &lib[2]),
            (
                &o("root", vec![1]),
                &o("full", vec![]),
                &o("parent", vec![])
            )
        );
        // `src/foo/mod.rs` and `src/operation/x.rs`.
        let src = "pub struct F;\nimpl crate::foo::F { fn a(&self) {} }\nimpl crate::F { fn b(&self) {} }\n";
        assert_eq!(
            owner_lines(src, &["foo"]),
            [o("a", vec![1]), o("b", vec![])]
        );
        let src =
            "pub enum E { A }\nimpl R for crate::operation::x::E { fn request_id(&self) {} }\n";
        assert_eq!(
            owner_lines(src, &["operation", "x"]),
            [o("request_id", vec![1])]
        );
        assert_eq!(owner_lines(src, &[]), [o("request_id", vec![])]);
    }

    /// A throwaway crate of `files` (path, text) under the temp dir; returns its directory.
    fn temp_crate(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tc-reexport-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for (rel, body) in [("Cargo.toml", "[package]\nname = \"x\"\n")]
            .iter()
            .chain(files)
        {
            let p = d.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        d
    }

    /// (member, owner lines) of each impl member of the crate file at `path`.
    fn file_owners(path: &Path) -> Vec<(String, Vec<usize>)> {
        let s = Parsed::read(path).unwrap().symbols();
        s.iter()
            .filter(|m| m.impl_ty.is_some())
            .map(|m| {
                let o = rs_owners(&s, m, Some(path)).into_iter().map(|i| s[i].line);
                (m.name.clone(), o.collect())
            })
            .collect()
    }

    #[test]
    fn rust_crate_paths_follow_reexports_back_into_the_file() {
        let o = |name: &str, n: &[usize]| vec![(name.to_string(), n.to_vec())];
        let icon = "pub enum NativeIcon { Add }\nimpl crate::NativeIcon { fn name(&self) {} }\n";
        let renamed = "pub enum NativeIcon { Add }\nimpl crate::RootIcon { fn name(&self) {} }\n";
        // muda: grouped, renamed, `self::`/`crate::` prefixed and glob re-exports from `lib.rs`.
        for (case, lib, icon) in [
            (
                "named",
                "mod icon;\npub use icon::{BadIcon, Icon, NativeIcon};\n",
                icon,
            ),
            (
                "renamed",
                "mod icon;\npub use self::icon::NativeIcon as RootIcon;\n",
                renamed,
            ),
            ("glob", "mod icon;\npub use crate::icon::*;\n", icon),
        ] {
            let d = temp_crate(case, &[("src/lib.rs", lib), ("src/icon.rs", icon)]);
            assert_eq!(
                file_owners(&d.join("src/icon.rs")),
                o("name", &[1]),
                "{case}"
            );
            let _ = std::fs::remove_dir_all(d);
        }
        // Through an intermediate module, and `crate::platform::Set` read from its own file.
        let win = "pub struct Set;\nimpl crate::Set { fn a(&self) {} }\nimpl crate::platform::Set { fn b(&self) {} }\n";
        let d = temp_crate(
            "chain",
            &[
                ("src/lib.rs", "mod platform;\npub use platform::*;\n"),
                (
                    "src/platform/mod.rs",
                    "mod windows;\npub use windows::{Set};\n",
                ),
                ("src/platform/windows.rs", win),
            ],
        );
        let path = d.join("src/platform/windows.rs");
        assert_eq!(file_owners(&path), [o("a", &[1]), o("b", &[1])].concat());
        let _ = std::fs::remove_dir_all(d);
        // `#[cfg]` twin re-exports: each file's `Set` is the root's on its platform.
        let set = "pub struct Set;\nimpl crate::Set { fn a(&self) {} }\n";
        let d = temp_crate(
            "twins",
            &[
                ("src/lib.rs", "mod unix;\nmod windows;\n#[cfg(windows)]\npub use windows::Set;\n#[cfg(unix)]\npub use unix::Set;\n"),
                ("src/unix.rs", set),
                ("src/windows.rs", set),
            ],
        );
        for f in ["src/unix.rs", "src/windows.rs"] {
            assert_eq!(file_owners(&d.join(f)), o("a", &[1]), "{f}");
        }
        let _ = std::fs::remove_dir_all(d);
        // `#[cfg]` twin aliases of two types of one file: an unconditional `impl crate::T` is
        // `A`'s in `alt` builds and `B`'s in the others; an `alt`-only one is `A`'s alone.
        let d = temp_crate(
            "twinalias",
            &[
                ("src/lib.rs", "mod platform;\n#[cfg(feature = \"alt\")]\npub use platform::A as T;\n#[cfg(not(feature = \"alt\"))]\npub use platform::B as T;\n"),
                ("src/platform.rs", "pub struct A;\npub struct B;\nimpl crate::T { fn m(&self) {} }\n#[cfg(feature = \"alt\")]\nimpl crate::T { fn alt(&self) {} }\n#[cfg(not(feature = \"alt\"))]\nimpl crate::T { fn plain(&self) {} }\n"),
            ],
        );
        assert_eq!(
            file_owners(&d.join("src/platform.rs")),
            [o("m", &[1, 2]), o("alt", &[1]), o("plain", &[2])].concat()
        );
        let _ = std::fs::remove_dir_all(d);
        // arboard: the root's own `Set`, a named `use` of another `Set`, a `use` cycle, a glob
        // inside `mod tests`: none of them is this file's `Set`, whatever globs come along.
        let globs =
            "mod other;\nmod platform;\npub use other::Set;\npub use platform::windows::*;\n";
        for (case, lib, other) in [
            (
                "own",
                "mod platform;\npub use platform::windows::*;\npub struct Set<'c> { c: &'c () }\n",
                "",
            ),
            ("other", globs, "pub struct Set;\n"),
            ("cycle", globs, "pub use crate::Set;\n"),
            (
                "tests",
                "mod platform;\nmod tests { pub use crate::platform::windows::*; }\n",
                "",
            ),
        ] {
            let d = temp_crate(
                case,
                &[
                    ("src/lib.rs", lib),
                    ("src/other.rs", other),
                    (
                        "src/platform/windows.rs",
                        "pub(crate) struct Set;\nimpl crate::Set { fn a(&self) {} }\n",
                    ),
                ],
            );
            let path = d.join("src/platform/windows.rs");
            assert_eq!(file_owners(&path), o("a", &[]), "{case}");
            let _ = std::fs::remove_dir_all(d);
        }
        // `tc mcp` outlives edits: a changed `lib.rs` (newer mtime) is read again.
        let lib = "mod platform;\npub struct Set;\n";
        let d = temp_crate(
            "edit",
            &[
                ("src/lib.rs", lib),
                (
                    "src/platform/windows.rs",
                    "pub struct Set;\nimpl crate::Set { fn a(&self) {} }\n",
                ),
            ],
        );
        let path = d.join("src/platform/windows.rs");
        assert_eq!(file_owners(&path), o("a", &[]));
        let lib = d.join("src/lib.rs");
        std::fs::write(&lib, "mod platform;\npub use platform::windows::Set;\n").unwrap();
        let f = std::fs::File::options().write(true).open(&lib).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
        drop(f);
        assert_eq!(file_owners(&path), o("a", &[1]));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn rust_custom_lib_path_is_the_crate_root() {
        // Only `[lib]`'s own `path`: not `[package]`'s, `[[bin]]`'s, a dependency's or a comment's.
        let toml = "[package]\nname = \"x\"\npath = \"no/pkg.rs\"\n# [lib]\n[[bin]]\npath = \"no/bin.rs\"\n[ lib ] # the library\n# path = \"no/comment.rs\"\npaths = \"no/paths.rs\"\npath = 'binding_rust/lib.rs' # here\n[dependencies.y]\npath = \"../y\"\n";
        assert_eq!(cargo_lib_path(toml).as_deref(), Some("binding_rust/lib.rs"));
        let explicit = "[lib]\nname = \"h\"\npath = \"src/lib.rs\"\n\n[[example]]\npath = \"examples/client.rs\"\n";
        assert_eq!(cargo_lib_path(explicit).as_deref(), Some("src/lib.rs"));
        let none = "[package]\npath = \"a.rs\"\n[dependencies]\ny = { path = \"../y\" }\n[lib]\nname = \"x\"\n[[bin]]\npath = \"b.rs\"\n";
        assert_eq!(cargo_lib_path(none), None);
        // tree-sitter: `binding_rust/lib.rs` is the root, its siblings its modules.
        let d = temp_crate(
            "customlib",
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"x\"\n\n[lib]\npath = \"binding_rust/lib.rs\"\n",
                ),
                (
                    "binding_rust/lib.rs",
                    "mod cursor;\nmod util;\npub use cursor::TreeCursor;\n",
                ),
                (
                    "binding_rust/cursor.rs",
                    "pub struct TreeCursor;\nimpl crate::TreeCursor { fn walk(&self) {} }\n",
                ),
                (
                    "binding_rust/util/deep.rs",
                    "pub struct Deep;\nimpl crate::util::deep::Deep { fn d(&self) {} }\n",
                ),
            ],
        );
        let br = d.join("binding_rust");
        assert_eq!(rs_lib_rel(&d), ["binding_rust", "lib.rs"]);
        assert!(rs_mod_path(&br.join("lib.rs")).is_empty());
        assert_eq!(rs_mod_path(&br.join("cursor.rs")), ["cursor"]);
        assert_eq!(rs_mod_path(&br.join("util/deep.rs")), ["util", "deep"]);
        assert_eq!(
            rs_crate_root(&br.join("cursor.rs")),
            Some(br.join("lib.rs"))
        );
        assert_eq!(
            file_owners(&br.join("cursor.rs")),
            [("walk".to_string(), vec![1])]
        );
        assert_eq!(
            file_owners(&br.join("util/deep.rs")),
            [("d".to_string(), vec![1])]
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn rust_lib_at_the_crate_root() {
        // selectors: `[lib] path = "lib.rs"`: the crate dir is the library's, but for the other
        // targets' places (`build.rs`, `tests/`, `examples/`).
        let d = temp_crate(
            "rootlib",
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"x\"\n\n[lib]\npath = \"lib.rs\"\n",
                ),
                (
                    "lib.rs",
                    "mod parser;\nmod sub;\npub use parser::Selector;\n",
                ),
                (
                    "parser.rs",
                    "pub struct Selector;\nimpl crate::Selector { fn m(&self) {} }\n",
                ),
                ("sub/mod.rs", "mod x;\n"),
                (
                    "sub/x.rs",
                    "pub struct X;\nimpl crate::sub::x::X { fn x(&self) {} }\n",
                ),
                ("build.rs", "fn main() {}\n"),
                ("tests/t.rs", "mod h;\n"),
                ("tests/t/h.rs", "pub struct H;\n"),
                ("examples/e.rs", "fn main() {}\n"),
            ],
        );
        assert_eq!(rs_lib_rel(&d), ["lib.rs"]);
        let m = |f: &str| rs_mod_path(&d.join(f));
        assert!(m("lib.rs").is_empty());
        assert_eq!(m("parser.rs"), ["parser"]);
        assert_eq!(m("sub/mod.rs"), ["sub"]);
        assert_eq!(m("sub/x.rs"), ["sub", "x"]);
        assert_eq!(m("tests/t/h.rs"), ["h"]);
        for f in ["build.rs", "tests/t.rs", "examples/e.rs"] {
            assert!(m(f).is_empty(), "{f}");
        }
        for f in ["parser.rs", "sub/x.rs"] {
            assert_eq!(rs_crate_root(&d.join(f)), Some(d.join("lib.rs")), "{f}");
        }
        for f in ["build.rs", "tests/t.rs", "tests/t/h.rs", "examples/e.rs"] {
            assert_eq!(rs_crate_root(&d.join(f)), None, "{f}");
        }
        // `crate::Selector` is `lib.rs`'s re-export of `parser.rs`'s, read from the crate dir.
        assert_eq!(
            file_owners(&d.join("parser.rs")),
            [("m".to_string(), vec![1])]
        );
        assert_eq!(
            file_owners(&d.join("sub/x.rs")),
            [("x".to_string(), vec![1])]
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn rust_use_path_keeps_crate_root_aliases() {
        // petgraph: `pub use crate::Direction as EdgeDirection;` (and `self::`, bare).
        let lib = "pub enum Direction { A }\npub use crate::Direction as EdgeDirection;\npub use self::Direction as SelfDir;\npub use Direction as BareDir;\nmod m { pub struct X; }\npub use m::X as Y;\npub use serde as s;\n";
        let d = temp_crate("rootalias", &[("src/lib.rs", lib)]);
        let p = |n: &str| rs_use_path(&d.join("src/lib.rs"), n);
        for n in ["EdgeDirection", "SelfDir", "BareDir"] {
            assert_eq!(p(n), Some(vec!["Direction".to_string()]), "{n}");
        }
        assert_eq!(p("Y"), Some(vec!["m".to_string(), "X".to_string()]));
        // An extern crate: `entry_hint` finds no `serde` declared in the entry and keeps `s`.
        assert_eq!(p("s"), Some(vec!["serde".to_string()]));
        assert_eq!(p("Direction"), None);
        let _ = std::fs::remove_dir_all(d);
        // A private or restricted root `use` names nothing public (ratatui-termwiz's
        // `use termwiz::surface::Position as TermwizPosition;`); only plain `pub use`s are listed.
        let lib = "mod m { pub struct X; }\nuse m::X as Priv;\npub(crate) use m::X as Krate;\npub(super) use m::X as Sup;\npub use m::X as Pub;\nuse other::*;\npub use outer::dir::f as g;\n";
        let d = temp_crate("privalias", &[("src/lib.rs", lib)]);
        let root = d.join("src/lib.rs");
        for n in ["Priv", "Krate", "Sup"] {
            assert_eq!(rs_use_path(&root, n), None, "{n}");
        }
        assert_eq!(
            rs_use_path(&root, "Pub"),
            Some(vec!["m".into(), "X".into()])
        );
        let s = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(
            rs_pub_uses(&root),
            [s("Pub", "m::X"), s("g", "outer::dir::f")]
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn rust_owner_paths_resolve_the_impl_type_in_its_module() {
        // scc: `LinkedEntry` is `linked_list::Entry`; `hash_map` has its own `Entry`.
        let d = temp_crate(
            "ownerpaths",
            &[
                ("src/lib.rs", "mod hash_map;\nmod linked_list;\nmod user;\npub use linked_list::Entry as LinkedEntry;\n"),
                ("src/hash_map.rs", "pub enum Entry { A }\nimpl Entry { pub fn or_insert(self) {} }\nmod t { use super::Entry; impl Entry { fn t1() {} } }\n"),
                ("src/linked_list.rs", "mod sub;\npub struct Entry { pub next: u8 }\nimpl Entry { pub fn take(&self) {} }\n"),
                ("src/linked_list/sub.rs", "impl super::Entry { fn a() {} }\n"),
                ("src/user.rs", "use crate::linked_list::Entry;\nimpl Entry { fn b() {} }\nimpl crate::linked_list::Entry { fn c() {} }\nimpl crate::LinkedEntry { fn e() {} }\nmod g { use crate::linked_list::*; impl Entry { fn d() {} } }\nmod n { impl Entry { fn none() {} } }\n"),
            ],
        );
        let root = d.join("src/lib.rs");
        let owners = |f: &str| -> Vec<(String, Vec<Vec<String>>)> {
            let f = d.join(f);
            let s = Parsed::read(&f).unwrap().symbols();
            s.iter()
                .filter(|m| m.parent.is_some())
                .map(|m| (m.name.clone(), rs_owner_paths(&root, &f, m)))
                .collect()
        };
        let p = |segs: &[&str]| vec![segs.iter().map(|s| s.to_string()).collect::<Vec<_>>()];
        let ll = p(&["linked_list", "Entry"]);
        let hm = p(&["hash_map", "Entry"]);
        let o = |n: &str, v: &Vec<Vec<String>>| (n.to_string(), v.clone());
        assert_eq!(
            owners("src/hash_map.rs"),
            [o("or_insert", &hm), o("t1", &hm)]
        );
        assert_eq!(
            owners("src/linked_list.rs"),
            [o("next", &ll), o("take", &ll)]
        );
        assert_eq!(owners("src/linked_list/sub.rs"), [o("a", &ll)]);
        assert_eq!(
            owners("src/user.rs"),
            [
                o("b", &ll),
                o("c", &ll),
                o("e", &ll),
                o("d", &ll),
                o("none", &vec![])
            ]
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn rust_conditional_declarations_keep_complementary_reexports() {
        let o = |name: &str, n: &[usize]| vec![(name.to_string(), n.to_vec())];
        let alt = "#[cfg(feature = \"alt\")]\n";
        let not_alt = "#[cfg(not(feature = \"alt\"))]\n";
        // `T` under `alt`, `B` as `T` under `not(alt)`: an unconditional `impl crate::T` is both's,
        // an `alt` one `T`'s, a `not(alt)` one `B`'s. An unconditional `T` is the only `T`.
        let impls = format!("impl crate::T {{ fn m(&self) {{}} }}\n{alt}impl crate::T {{ fn alt(&self) {{}} }}\n{not_alt}impl crate::T {{ fn plain(&self) {{}} }}\n");
        let sub = "mod sub { pub struct B; }\n";
        for (case, lib, mid, want) in [
            // Same file: 2 `T`, 3 `B`.
            (
                "same",
                format!("{alt}pub struct T;\n{sub}{not_alt}pub use sub::B as T;\n{impls}"),
                "",
                [o("m", &[2, 3]), o("alt", &[2]), o("plain", &[3])].concat(),
            ),
            // An unconditional `use` beside the `alt` declaration is the `not(alt)` one.
            (
                "plainuse",
                format!("{alt}pub struct T;\n{sub}pub use sub::B as T;\n{impls}"),
                "",
                [o("m", &[2, 3]), o("alt", &[2]), o("plain", &[3])].concat(),
            ),
            (
                "uncond",
                format!("pub struct T;\n{sub}{not_alt}pub use sub::B as T;\n{impls}"),
                "",
                [o("m", &[1]), o("alt", &[1]), o("plain", &[1])].concat(),
            ),
            // The declaration in a module the root re-exports from: 4 `T`, 8 `B`.
            (
                "hop",
                format!("pub use a::T;\nmod a {{\n{alt}pub struct T;\n{not_alt}pub use super::sub::B as T;\n}}\n{sub}{impls}"),
                "",
                [o("m", &[4, 8]), o("alt", &[4]), o("plain", &[8])].concat(),
            ),
            // `not(alt)` goes through `mid.rs` and back: 3 `T`, 6 `B`.
            (
                "multifile",
                format!("mod mid;\n{alt}pub struct T;\n{not_alt}pub use mid::T;\n{sub}{impls}"),
                "pub use crate::sub::B as T;\n",
                [o("m", &[3, 6]), o("alt", &[3]), o("plain", &[6])].concat(),
            ),
        ] {
            let d = temp_crate(
                &format!("conddecl-{case}"),
                &[("src/lib.rs", &lib), ("src/mid.rs", mid)],
            );
            assert_eq!(file_owners(&d.join("src/lib.rs")), want, "{case}");
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn rust_grouped_globs_join_with_one_separator() {
        let src = "use icon::{Other, *};\nuse icon2::{self, *};\nuse crate::{a, *};\nuse a::{b::{*}};\nuse c::{d::*};\nuse e::*;\nuse outer::{nested:: *};\nuse outer::{nested:: /* comment */ *};\nuse outer:: /* c */ *;\nuse f :: /* c */ g::{h:: // c\n *};\n";
        let p = Parsed::new(Lang::Rs, src.into()).unwrap();
        let globs: Vec<String> = p
            .rs_mod_items()
            .into_iter()
            .filter(|i| i.name == "*")
            .filter_map(|i| i.path)
            .collect();
        assert_eq!(
            globs,
            [
                "icon",
                "icon2",
                "crate",
                "a::b",
                "c::d",
                "e",
                "outer::nested",
                "outer::nested",
                "outer",
                "f::g::h"
            ]
        );
        // Named `use`s lose their spaces and comments too.
        let p = Parsed::new(Lang::Rs, "use a :: /* c */ b::{c as d, e :: f};\n".into()).unwrap();
        let named: Vec<(String, Option<String>)> = p
            .rs_mod_items()
            .into_iter()
            .map(|i| (i.name, i.path))
            .collect();
        assert_eq!(
            named,
            [
                ("d".into(), Some("a::b::c".into())),
                ("f".into(), Some("a::b::e::f".into()))
            ]
        );
        // `icon::{Other, *}` from `lib.rs` reaches `icon.rs`, as does a nested `a::{b::{*}}`.
        let icon = "pub enum NativeIcon { Add }\nimpl crate::NativeIcon { fn name(&self) {} }\n";
        let d = temp_crate(
            "grouped",
            &[
                (
                    "src/lib.rs",
                    "mod icon;\nmod a;\nmod spaced;\nmod loose;\npub use icon::{Other, *};\npub use a::{b::{*}};\npub use spaced::{inner:: /* comment */ *};\npub use loose:: /* c */ *;\n",
                ),
                ("src/icon.rs", icon),
                ("src/a/mod.rs", "pub mod b;\n"),
                (
                    "src/a/b.rs",
                    "pub enum NestIcon { Add }\nimpl crate::NestIcon { fn name(&self) {} }\n",
                ),
                ("src/spaced/mod.rs", "pub mod inner;\n"),
                (
                    "src/spaced/inner.rs",
                    "pub enum SpacedIcon { Add }\nimpl crate::SpacedIcon { fn name(&self) {} }\n",
                ),
                (
                    "src/loose.rs",
                    "pub enum LooseIcon { Add }\nimpl crate::LooseIcon { fn name(&self) {} }\n",
                ),
            ],
        );
        for f in [
            "src/icon.rs",
            "src/a/b.rs",
            "src/spaced/inner.rs",
            "src/loose.rs",
        ] {
            assert_eq!(
                file_owners(&d.join(f)),
                [("name".to_string(), vec![1])],
                "{f}"
            );
        }
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn rust_reexport_cfgs_pick_the_binding_the_impl_compiles_with() {
        let o = |name: &str, n: &[usize]| (name.to_string(), n.to_vec());
        // `alt_only` compiles only where `crate::T` is `b::T` (or the root's own `T`): never `a::T`.
        let a = "pub struct T;\n#[cfg(feature = \"alt\")]\nimpl crate::T { pub fn alt_only() {} }\n#[cfg(not(feature = \"alt\"))]\nimpl crate::T { pub fn default_only() {} }\nimpl crate::T { pub fn both() {} }\n";
        for (case, lib) in [
            ("cfgtwins", "mod a;\nmod b;\n#[cfg(not(feature = \"alt\"))]\npub use a::T;\n#[cfg(feature = \"alt\")]\npub use b::T;\n"),
            ("cfgown", "mod a;\n#[cfg(feature = \"alt\")]\npub struct T;\n#[cfg(not(feature = \"alt\"))]\npub use a::T;\n"),
            // An inline `mod`'s cfgs hold for the `use`s inside it.
            ("cfginline", "mod a;\nmod b;\n#[cfg(not(feature = \"alt\"))]\nmod alt { pub use crate::a::T; }\n#[cfg(feature = \"alt\")]\nmod alt { pub use crate::b::T; }\npub use alt::T;\n"),
        ] {
            let d = temp_crate(
                case,
                &[("src/lib.rs", lib), ("src/a.rs", a), ("src/b.rs", "pub struct T;\n")],
            );
            assert_eq!(
                file_owners(&d.join("src/a.rs")),
                [o("alt_only", &[]), o("default_only", &[1]), o("both", &[1])],
                "{case}"
            );
            let _ = std::fs::remove_dir_all(d);
        }
        // The cfgs of every hop add up: `x` then `not(x)` never compiles; `x` then `feature="a"`
        // picks the `feature = "a"` twin.
        let d = temp_crate(
            "cfghops",
            &[
                ("src/lib.rs", "mod p;\n#[cfg(x)]\npub use p::*;\n"),
                (
                    "src/p/mod.rs",
                    "mod w;\n#[cfg(not(x))]\npub use w::T;\n#[cfg(feature = \"a\")]\npub use w::U;\n",
                ),
                (
                    "src/p/w.rs",
                    "pub struct T;\nimpl crate::T { fn t() {} }\n#[cfg(feature = \"a\")]\npub struct U;\n#[cfg(feature = \"b\")]\npub struct U;\nimpl crate::U { fn u() {} }\n",
                ),
            ],
        );
        assert_eq!(
            file_owners(&d.join("src/p/w.rs")),
            [o("t", &[]), o("u", &[4])]
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn rust_crate_paths_reach_out_of_line_children_of_the_file() {
        let o = |name: &str, n: &[usize]| vec![(name.to_string(), n.to_vec())];
        let lib = "pub struct T;\nimpl crate::bridge::T { fn b(&self) {} }\n";
        for (case, files, want) in [
            // `mod bridge;` is `src/bridge.rs`, though `lib.rs`'s module path prefixes it.
            (
                "bridge",
                vec![
                    ("src/lib.rs", format!("mod bridge;\n{lib}")),
                    ("src/bridge.rs", "pub use crate::T;\n".to_string()),
                ],
                o("b", &[2]),
            ),
            // An inline `mod bridge { .. }` is read from `lib.rs`, never from a stray `bridge.rs`.
            (
                "bridgeinline",
                vec![
                    (
                        "src/lib.rs",
                        format!("mod bridge {{ pub use crate::T; }}\n{lib}"),
                    ),
                    ("src/bridge.rs", "pub struct Other;\n".to_string()),
                ],
                o("b", &[2]),
            ),
            (
                "bridgedecoy",
                vec![
                    ("src/lib.rs", format!("mod bridge {{}}\n{lib}")),
                    ("src/bridge.rs", "pub use crate::T;\n".to_string()),
                ],
                o("b", &[]),
            ),
        ] {
            let files: Vec<(&str, &str)> = files.iter().map(|(p, b)| (*p, b.as_str())).collect();
            let d = temp_crate(case, &files);
            assert_eq!(file_owners(&d.join("src/lib.rs")), want, "{case}");
            let _ = std::fs::remove_dir_all(d);
        }
        // `src/a/mod.rs` and its `mod child;` in `src/a/child.rs`, or its inline `mod child { .. }`.
        let a = "pub struct T;
impl crate::a::child::T { fn c(&self) {} }
";
        for (case, child_mod, child_rs, want) in [
            (
                "childmod",
                "mod child;",
                "pub use super::T;
",
                o("c", &[2]),
            ),
            (
                "childinline",
                "mod child { pub use super::T; }",
                "pub struct Other;
",
                o("c", &[2]),
            ),
            (
                "childdecoy",
                "mod child {}",
                "pub use super::T;
",
                o("c", &[]),
            ),
        ] {
            let d = temp_crate(
                case,
                &[
                    (
                        "src/lib.rs",
                        "mod a;
",
                    ),
                    (
                        "src/a/mod.rs",
                        &format!(
                            "{child_mod}
{a}"
                        ),
                    ),
                    ("src/a/child.rs", child_rs),
                ],
            );
            assert_eq!(file_owners(&d.join("src/a/mod.rs")), want, "{case}");
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn rust_cfgs_contradict_on_single_valued_keys_families_and_failed_anys() {
        let c = |a: &str, b: &str| {
            let (mut x, mut y) = (Vec::new(), Vec::new());
            cfg_conjuncts(&a.replace(' ', ""), &mut x);
            cfg_conjuncts(&b.replace(' ', ""), &mut y);
            cfg_contradicts(&x, &y)
        };
        for (a, b) in [
            ("x", "not(x)"),
            ("target_os = \"linux\"", "target_os = \"windows\""),
            ("target_arch = \"x86\"", "target_arch = \"x86_64\""),
            (
                "target_pointer_width = \"32\"",
                "target_pointer_width = \"64\"",
            ),
            ("target_env = \"gnu\"", "target_env = \"msvc\""),
            ("unix", "windows"),
            ("target_family = \"unix\"", "windows"),
            ("target_family = \"unix\"", "target_family = \"windows\""),
            ("target_os = \"windows\"", "unix"),
            ("target_os = \"macos\"", "windows"),
            ("target_os = \"android\"", "target_family = \"windows\""),
            ("not(unix)", "target_os = \"linux\""),
            ("not(windows)", "target_os = \"windows\""),
            ("all(unix, feature = \"a\")", "windows"),
            (
                "any(target_os = \"linux\", target_os = \"macos\")",
                "windows",
            ),
            ("any(all(unix, x), windows,)", "all(not(x), not(windows))"),
            ("any(any(a, b), c)", "all(not(a), not(b), not(c))"),
            // A sibling conjunct rules out a branch: `not(a)` leaves `any(a, b)` only `b`.
            ("all(not(a), any(a, b))", "not(b)"),
            ("all(not(a), any(any(a, c), b))", "all(not(b), not(c))"),
            // Negations go inward: `not(any(a, b))` is `not(a)` and `not(b)`.
            ("not(any(a, b))", "a"),
            ("not(any(a, b))", "b"),
            ("not(any(unix, windows))", "windows"),
            ("not(any(unix, windows))", "unix"),
            ("not(any(unix, windows))", "target_os = \"linux\""),
            ("not(any(unix, windows))", "target_os = \"windows\""),
            ("not(not(not(any(a, b))))", "a"),
            ("not(any(a, not(b)))", "not(b)"),
            ("not(all(a, b))", "all(a, b)"),
            ("not(all(a, b, c))", "all(a, b, c)"),
            ("not(all(a))", "a"),
            ("not(all())", "x"),
            // Trailing commas inside `not(..)`, `any(..)` and `all(..)`, as rustc accepts them.
            ("not(any(unix, windows),)", "windows"),
            ("not(any(unix, windows,),)", "unix"),
            ("not(all(a, b,),)", "all(a, b)"),
            ("not(not(not(a,),),)", "a"),
        ] {
            assert!(c(a, b) && c(b, a), "{a} vs {b}");
        }
        let conj = |a: &str| {
            let mut x = Vec::new();
            cfg_conjuncts(&a.replace(' ', ""), &mut x);
            x
        };
        for (a, b) in [
            ("not(any(unix, windows),)", "not(any(unix, windows))"),
            ("not(all(a, b,),)", "not(all(a, b))"),
            ("not(not(a,),)", "a"),
            ("any(a, b,)", "any(a, b)"),
            ("not(unix,)", "not(unix)"),
        ] {
            assert_eq!(conj(a), conj(b), "{a} vs {b}");
        }
        for (a, b) in [
            ("x", "y"),
            ("target_os = \"linux\"", "target_os = \"linux\""),
            ("target_os = \"linux\"", "unix"),
            ("target_os = \"linux\"", "target_arch = \"x86\""),
            ("feature = \"a\"", "feature = \"b\""),
            ("target_family = \"wasm\"", "target_family = \"unix\""),
            ("target_feature = \"sse2\"", "target_feature = \"avx\""),
            ("not(target_os = \"linux\")", "unix"),
            ("any(target_os = \"linux\", windows)", "windows"),
            ("any(a, b)", "not(a)"),
            ("all(not(a), any(a, b))", "b"),
            ("not(all(a, b))", "a"),
            ("not(all(a, b))", "b"),
            ("not(all(a, b))", "not(a)"),
            ("not(any(unix, windows))", "target_family = \"wasm\""),
            ("not(any())", "x"),
            ("all(not(a), any(a, b))", "c"),
        ] {
            assert!(!c(a, b) && !c(b, a), "{a} vs {b}");
        }
    }

    #[test]
    fn rust_reexports_follow_mutually_exclusive_targets_and_conditional_shadowing() {
        let o = |name: &str, n: &[usize]| (name.to_string(), n.to_vec());
        // `target_os = "windows"` code in `a.rs` sees `b::T` as `crate::T`, never `a::T`.
        let a = "pub struct T;\n#[cfg(target_os = \"windows\")]\nimpl crate::T { fn win() {} }\n#[cfg(target_os = \"linux\")]\nimpl crate::T { fn lin() {} }\n#[cfg(windows)]\nimpl crate::T { fn fam() {} }\n#[cfg(target_arch = \"x86\")]\nimpl crate::T { fn x86() {} }\nimpl crate::T { fn any() {} }\n";
        for (case, lib) in [
            ("os", "mod a;\nmod b;\n#[cfg(target_os = \"linux\")]\npub use a::T;\n#[cfg(target_os = \"windows\")]\npub use b::T;\n"),
            ("family", "mod a;\nmod b;\n#[cfg(unix)]\npub use a::T;\n#[cfg(target_family = \"windows\")]\npub use b::T;\n"),
        ] {
            let d = temp_crate(
                case,
                &[("src/lib.rs", lib), ("src/a.rs", a), ("src/b.rs", "pub struct T;\n")],
            );
            assert_eq!(
                file_owners(&d.join("src/a.rs")),
                [o("win", &[]), o("lin", &[1]), o("fam", &[]), o("x86", &[1]), o("any", &[1])],
                "{case}"
            );
            let _ = std::fs::remove_dir_all(d);
        }
        // `x86` only: the `x86_64` re-export is out, and the linux one still compiles there.
        let d = temp_crate(
            "arch",
            &[
                ("src/lib.rs", "mod a;\nmod b;\n#[cfg(target_arch = \"x86_64\")]\npub use a::T;\n#[cfg(target_arch = \"x86\")]\npub use b::T;\n"),
                ("src/a.rs", "pub struct T;\n#[cfg(target_arch = \"x86\")]\nimpl crate::T { fn x86() {} }\n#[cfg(target_arch = \"x86_64\")]\nimpl crate::T { fn x64() {} }\n"),
                ("src/b.rs", "pub struct T;\n"),
            ],
        );
        assert_eq!(
            file_owners(&d.join("src/a.rs")),
            [o("x86", &[]), o("x64", &[1])]
        );
        let _ = std::fs::remove_dir_all(d);
        // A `not(any(unix, windows))` re-export is never `crate::T` of a `windows` impl.
        let d = temp_crate(
            "notany",
            &[
                ("src/lib.rs", "mod a;\nmod b;\n#[cfg(windows)]\npub use b::T;\n#[cfg(not(any(unix, windows)))]\npub use a::T;\n"),
                ("src/a.rs", "pub struct T;\n#[cfg(windows)]\nimpl crate::T { fn win() {} }\nimpl crate::T { fn any() {} }\n"),
                ("src/b.rs", "pub struct T;\n"),
            ],
        );
        assert_eq!(
            file_owners(&d.join("src/a.rs")),
            [o("win", &[]), o("any", &[1])]
        );
        let _ = std::fs::remove_dir_all(d);
        // A glob is `crate::T` wherever no named `use` or own `T` is: a `#[cfg(feature = "alt")]`
        // one leaves it the `not(feature = "alt")` builds; an unconditional one leaves it none.
        let fallback = "pub struct T;\nimpl crate::T { fn plain() {} }\n#[cfg(feature = \"alt\")]\nimpl crate::T { fn alt() {} }\n#[cfg(not(feature = \"alt\"))]\nimpl crate::T { fn not_alt() {} }\n";
        for (case, lib, want) in [
            (
                "condnamed",
                "mod fallback;\nmod preferred;\npub use fallback::*;\n#[cfg(feature = \"alt\")]\npub use preferred::T;\n",
                [o("plain", &[1]), o("alt", &[]), o("not_alt", &[1])],
            ),
            (
                "condown",
                "mod fallback;\npub use fallback::*;\n#[cfg(feature = \"alt\")]\npub struct T;\n",
                [o("plain", &[1]), o("alt", &[]), o("not_alt", &[1])],
            ),
            (
                "uncondnamed",
                "mod fallback;\nmod preferred;\npub use fallback::*;\npub use preferred::T;\n",
                [o("plain", &[]), o("alt", &[]), o("not_alt", &[])],
            ),
            (
                "twinnamed",
                "mod fallback;\nmod preferred;\npub use fallback::*;\n#[cfg(feature = \"alt\")]\npub use preferred::T;\n#[cfg(not(feature = \"alt\"))]\npub use preferred::T;\n",
                [o("plain", &[]), o("alt", &[]), o("not_alt", &[])],
            ),
            // `not(any(unix, windows))` is two conjuncts; both twins still cover every build.
            (
                "twinany",
                "mod fallback;\nmod preferred;\npub use fallback::*;\n#[cfg(any(unix, windows))]\npub use preferred::T;\n#[cfg(not(any(unix, windows)))]\npub use preferred::T;\n",
                [o("plain", &[]), o("alt", &[]), o("not_alt", &[])],
            ),
            // The glob is gated like the shadow: where it is on, `preferred::T` is `crate::T`.
            (
                "condglob",
                "mod fallback;\nmod preferred;\n#[cfg(feature = \"alt\")]\npub use preferred::T;\n#[cfg(feature = \"alt\")]\npub use fallback::*;\n",
                [o("plain", &[]), o("alt", &[]), o("not_alt", &[])],
            ),
        ] {
            let d = temp_crate(
                case,
                &[
                    ("src/lib.rs", lib),
                    ("src/fallback.rs", fallback),
                    ("src/preferred.rs", "pub struct T;\n"),
                ],
            );
            assert_eq!(file_owners(&d.join("src/fallback.rs")), want, "{case}");
            let _ = std::fs::remove_dir_all(d);
        }
        // The glob's `not(feature = "alt")` picks that twin in the file it reaches.
        let d = temp_crate(
            "globtwin",
            &[
                ("src/lib.rs", "mod fallback;\nmod preferred;\npub use fallback::*;\n#[cfg(feature = \"alt\")]\npub use preferred::T;\n"),
                ("src/fallback.rs", "#[cfg(feature = \"alt\")]\npub struct T;\n#[cfg(not(feature = \"alt\"))]\npub struct T;\nimpl crate::T { fn plain() {} }\n"),
                ("src/preferred.rs", "pub struct T;\n"),
            ],
        );
        assert_eq!(file_owners(&d.join("src/fallback.rs")), [o("plain", &[4])]);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn rust_module_walk_follows_mod_cfgs_and_the_declaring_crate_root() {
        let o = |name: &str, n: &[usize]| (name.to_string(), n.to_vec());
        // `#[cfg(feature = "inline")] mod bridge { .. }` beside `#[cfg(not(..))] mod bridge;`:
        // `bridge.rs` is the module wherever the inline one is off.
        let lib = "#[cfg(feature = \"inline\")]\nmod bridge { pub struct Other; }\n#[cfg(not(feature = \"inline\"))]\nmod bridge;\npub struct T;\nimpl crate::bridge::T { fn plain(&self) {} }\n#[cfg(feature = \"inline\")]\nimpl crate::bridge::T { fn inline(&self) {} }\n#[cfg(not(feature = \"inline\"))]\nimpl crate::bridge::T { fn file(&self) {} }\n";
        let d = temp_crate(
            "cfgbridge",
            &[
                ("src/lib.rs", lib),
                ("src/bridge.rs", "pub use crate::T;\n"),
            ],
        );
        assert_eq!(
            file_owners(&d.join("src/lib.rs")),
            [o("plain", &[5]), o("inline", &[]), o("file", &[5])]
        );
        let _ = std::fs::remove_dir_all(d);
        // A `#[cfg(x)] mod a;` adds `x` to what is read in `a.rs`.
        let d = temp_crate(
            "cfgmod",
            &[
                ("src/lib.rs", "#[cfg(x)]\nmod p;\npub use p::T;\n"),
                (
                    "src/p.rs",
                    "mod w;\n#[cfg(not(x))]\npub use w::T;\n#[cfg(x)]\npub use w::T;\n",
                ),
                (
                    "src/p/w.rs",
                    "pub struct T;\n#[cfg(not(x))]\npub struct T;\nimpl crate::T { fn t() {} }\n",
                ),
            ],
        );
        assert_eq!(file_owners(&d.join("src/p/w.rs")), [o("t", &[1])]);
        let _ = std::fs::remove_dir_all(d);
        // Under `feature = "alt"`, `owner` is the inline `mod` of `lib.rs`, not `owner.rs`: an
        // alt-only `pub use owner::T;` never names `owner.rs`'s `T`.
        let owner = "pub struct T;\nimpl crate::T { fn t() {} }\n";
        for (case, gate, want) in [
            ("cfgownalt", "feature = \"alt\"", vec![]),
            ("cfgownfile", "not(feature = \"alt\")", vec![1]),
        ] {
            let lib = format!("#[cfg(feature = \"alt\")]\nmod owner {{ pub struct T; }}\n#[cfg(not(feature = \"alt\"))]\nmod owner;\n#[cfg({gate})]\npub use owner::T;\n");
            let d = temp_crate(case, &[("src/lib.rs", &lib), ("src/owner.rs", owner)]);
            assert_eq!(
                file_owners(&d.join("src/owner.rs")),
                [o("t", &want)],
                "{case}"
            );
            let _ = std::fs::remove_dir_all(d);
        }
        // `lib.rs` and `main.rs` are two crates: `main.rs`'s `crate::bridge` is its `bridge.rs`,
        // not `lib.rs`'s inline `mod bridge`.
        let main = "mod bridge;\npub struct T;\nimpl crate::bridge::T { fn b(&self) {} }\npub use bridge::B;\n";
        let d = temp_crate(
            "dual",
            &[
                (
                    "src/lib.rs",
                    "mod bridge { pub struct Other; }\npub struct B;\n",
                ),
                ("src/main.rs", main),
                (
                    "src/bridge.rs",
                    "pub use crate::T;\npub struct B;\nimpl crate::B { fn bb(&self) {} }\n",
                ),
            ],
        );
        assert_eq!(file_owners(&d.join("src/main.rs")), [o("b", &[2])]);
        assert_eq!(file_owners(&d.join("src/bridge.rs")), [o("bb", &[2])]);
        // A file `lib.rs` declares belongs to the library.
        let lib = d.join("src/lib.rs");
        std::fs::write(&lib, "mod bridge;\n").unwrap();
        let f = std::fs::File::options().write(true).open(&lib).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
        drop(f);
        assert_eq!(file_owners(&d.join("src/bridge.rs")), [o("bb", &[])]);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn rust_cfgs_flatten_to_conjuncts_and_contradictions_never_own() {
        let cfg = |src: &str| {
            let mut c = syms(Lang::Rs, src)[0].cfg.clone();
            c.sort();
            c
        };
        assert_eq!(
            cfg("#[cfg(all(not(feature = \"union\"), feature = \"const_new\"))]\nstruct S;\n"),
            ["feature=\"const_new\"", "not(feature=\"union\")"]
        );
        assert_eq!(
            cfg("#[cfg(all(all(a, b), c))]\nstruct S;\n"),
            ["a", "b", "c"]
        );
        assert_eq!(cfg("#[cfg(a)]\n#[cfg(b)]\nstruct S;\n"), ["a", "b"]);
        assert_eq!(cfg("#[cfg(not(not(all(x, y,))))]\nstruct S;\n"), ["x", "y"]);
        // `any` stays whole, `not(all)` becomes one; a comma inside a string or nested parens
        // doesn't split.
        assert_eq!(
            cfg("#[cfg(all(any(a, b), not(all(c, d)), feature = \"x,y\"))]\nstruct S;\n"),
            ["any(a,b)", "any(not(c),not(d))", "feature=\"x,y\""]
        );
        assert!(cfg("#[cfg(all())]\nstruct S;\n").is_empty());
        assert_eq!(
            cfg("#[cfg(not(any(unix, windows)))]\nstruct S;\n"),
            ["not(unix)", "not(windows)"]
        );
        // smallvec: `all(not(union), const_new)` is the enum's, `all(union, const_new)` the union's.
        let src = r#"#[cfg(feature = "union")]
union D { a: u8 }
#[cfg(all(feature = "union", feature = "const_new"))]
impl D { const fn u() {} }
#[cfg(not(feature = "union"))]
enum D { A }
#[cfg(all(not(feature = "union"), feature = "const_new"))]
impl D { const fn e() {} }
impl D { fn shared() {} }
#[cfg(x)]
struct Only;
#[cfg(not(x))]
impl Only { fn never() {} }
#[cfg(not(y))]
struct NotY;
#[cfg(not(not(y)))]
impl NotY { fn never2() {} }
#[cfg(z)]
struct Z;
#[cfg(not(z))]
struct Z;
impl Z { #[cfg(not(z))] fn only_not_z() {} }
#[cfg(unix)]
struct W;
#[cfg(not(any(unix, windows)))]
struct W;
#[cfg(windows)]
impl W { fn win_only() {} }
impl W { fn every() {} }
#[cfg(not(all(a, b)))]
struct NotAB;
#[cfg(all(a, b))]
impl NotAB { fn never3() {} }
#[cfg(a)]
impl NotAB { fn a_only() {} }
"#;
        let o = |name: &str, n: Vec<usize>| (name.to_string(), n);
        assert_eq!(
            owner_lines(src, &[]),
            [
                o("u", vec![2]),
                o("e", vec![6]),
                o("shared", vec![2, 6]),
                o("never", vec![]),
                o("never2", vec![]),
                o("only_not_z", vec![21]),
                // The `not(any(unix, windows))` fallback is never a Windows impl's.
                o("win_only", vec![]),
                o("every", vec![24, 26]),
                o("never3", vec![]),
                o("a_only", vec![31]),
            ]
        );
    }

    #[test]
    fn java_methods_and_calls() {
        let src = "package a.b;\nimport org.x.Y;\npublic class C {\n  /** Free. */\n  @GetMapping(\"/f\")\n  public int f(int a) { return g(a) + repo.find(a); }\n  private int g(int a) { return a; }\n}\n";
        let p = Parsed::new(Lang::Java, src.into()).unwrap();
        let s = p.symbols();
        let f = s.iter().find(|s| s.name == "f").unwrap();
        assert_eq!((f.start, f.line, f.exported), (4, 6, true));
        assert!(f.sig.contains("@GetMapping(\"/f\")"));
        assert!(!s.iter().find(|s| s.name == "g").unwrap().exported);
        let calls: Vec<String> = p
            .calls(f.bytes.0, f.bytes.1)
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert!(calls.contains(&"g".to_string()) && calls.contains(&"find".to_string()));
        assert_eq!(p.imports()[0].module, "org.x.Y");
    }

    #[test]
    fn f17_word_boundaries_are_unicode_aware() {
        assert!(!has_word("const αf = 1;", "f"));
        assert!(!has_word("fé()", "f"));
        assert!(has_word("é f()", "f") && has_word("f", "f"));
        assert!(word_at("(f)", 1, 1));
    }

    #[test]
    fn f18_rust_visibility_follows_module_chain() {
        let s = syms(
            Lang::Rs,
            "pub(crate) fn a() {}
mod m { pub fn b() {} }
pub mod p { pub fn c() {} pub(super) fn d() {} }
pub struct S;
impl S { pub fn e(&self) {} }
mod q { pub struct T; impl T { pub fn f(&self) {} } }
",
        );
        let exported = |n: &str| s.iter().find(|s| s.name == n).unwrap().exported;
        assert!(!exported("a") && !exported("b") && exported("c") && !exported("d"));
        assert!(exported("e") && !exported("f"));
    }

    #[test]
    fn fold_is_full_unicode_case_folding() {
        let m = |hay: &str, needle: &str| fold(hay).contains(&fold(needle));
        // ΟΣ against Σ, σ, ς (str::to_lowercase gives "ος", which misses "σ").
        for q in [
            "\u{3A3}",
            "\u{3C3}",
            "\u{3C2}",
            "\u{3BF}\u{3C2}",
            "\u{3BF}\u{3C3}",
        ] {
            assert!(m("\u{39F}\u{3A3}", q), "{q}");
        }
        assert!(m("\u{3BF}\u{3C2}", "\u{39F}\u{3A3}"));
        // straße / STRASSE / ẞ
        assert!(m("stra\u{DF}e", "STRASSE") && m("STRASSE", "stra\u{DF}e"));
        assert!(m("Stra\u{1E9E}e", "strasse") && m("\u{1E9E}", "\u{DF}"));
        // İstanbul / istanbul / ıstanbul
        assert!(m("\u{130}stanbul", "istanbul") && m("istanbul", "\u{130}ST"));
        assert!(m("ISTANBUL", "\u{131}stanbul"));
        // ligature, iota subscript, micro sign, long s
        assert!(m("\u{FB01}le", "FILE") && m("\u{1FBC}", "\u{3B1}\u{3B9}"));
        assert!(m("\u{B5}s", "\u{3BC}") && m("\u{17F}", "S") && m("\u{3A9}mega", "\u{3C9}MEGA"));
        assert_eq!(fold("Hello_World"), "hello_world");
    }

    #[test]
    fn rust_struct_fields_follow_visibility() {
        let s = syms(
            Lang::Rs,
            "pub struct A {\n    /// Rows.\n    pub x: u8,\n    y: u8,\n    pub(crate) z: u8,\n}\nstruct B { pub w: u8 }\nmod m { pub struct C { pub v: u8 } }\npub struct T(pub u8);\n",
        );
        let f = |n: &str| s.iter().find(|s| s.name == n).unwrap();
        let x = f("x");
        assert_eq!(
            (x.kind, x.parent.as_deref(), x.exported, x.sig.as_str()),
            ("field", Some("A"), true, "pub x: u8")
        );
        assert_eq!((x.line, x.doc.as_str()), (3, "Rows."));
        assert!(!f("y").exported && !f("z").exported && !f("w").exported && !f("v").exported);
        assert!(!s.iter().any(|s| s.parent.as_deref() == Some("T")));
    }

    #[test]
    fn rust_trait_associated_types_are_members() {
        let src = "pub trait Deserializer {\n    /// The error.\n    type Error: StdError;\n    fn f(&self);\n}\ntrait Hidden { type Out; }\n";
        let s = syms(Lang::Rs, src);
        let t = |name: &str| s.iter().find(|s| s.name == name).unwrap();
        let e = t("Error");
        assert_eq!(
            (
                e.kind,
                e.parent.as_deref(),
                e.exported,
                e.line,
                e.start,
                e.end
            ),
            ("type", Some("Deserializer"), true, 3, 2, 3)
        );
        assert_eq!(
            (e.sig.as_str(), e.doc.as_str()),
            ("type Error: StdError;", "The error.")
        );
        assert_eq!(e.filter_sig, None);
        assert!(!t("Out").exported);
    }

    #[test]
    fn filter_sig_stops_at_the_initializer() {
        let fs = |lang, src: &str, name: &str| {
            syms(lang, src)
                .into_iter()
                .find(|s| s.name == name)
                .unwrap()
                .filter_sig
        };
        let v = "var Gs=\"Terminal input\",mi={get:()=>Gs};";
        assert_eq!(fs(Lang::Tsx, v, "Gs").as_deref(), Some("Gs"));
        let c = "export const foo: ResizeMode = \"x\";";
        assert_eq!(
            fs(Lang::Ts, c, "foo").as_deref(),
            Some("export const foo: ResizeMode")
        );
        assert_eq!(
            fs(Lang::Ts, "class K { gridLimit = 3; }", "gridLimit").as_deref(),
            Some("gridLimit")
        );
        assert_eq!(
            fs(Lang::Py, "LIMIT: int = 5\n", "LIMIT").as_deref(),
            Some("LIMIT: int")
        );
        assert_eq!(
            fs(Lang::Rs, "const MAX: usize = 3;", "MAX").as_deref(),
            Some("const MAX: usize")
        );
        let j = "class K { private String s=\"Terminal input\"; }";
        assert_eq!(fs(Lang::Java, j, "s").as_deref(), Some("private String s"));
        // Java multi-declarators: the shared head plus the name's own declarator only.
        let j = "class K { final int port, tailnet; int a = 1, b[] = {2}; }";
        assert_eq!(fs(Lang::Java, j, "port").as_deref(), Some("final int port"));
        assert_eq!(
            fs(Lang::Java, j, "tailnet").as_deref(),
            Some("final int tailnet")
        );
        assert_eq!(fs(Lang::Java, j, "a").as_deref(), Some("int a"));
        assert_eq!(fs(Lang::Java, j, "b").as_deref(), Some("int b[]"));
        let t = "const port = 1, tailnet = 2;";
        assert_eq!(fs(Lang::Ts, t, "tailnet").as_deref(), Some("tailnet"));
        assert_eq!(fs(Lang::Ts, "export function f(a: number) {}", "f"), None);
    }

    #[test]
    fn imports_all_languages() {
        let ts = Parsed::new(
            Lang::Ts,
            "import { a as b, c } from './x';\nimport * as ns from '@s/p';\nexport * from './y';"
                .into(),
        )
        .unwrap()
        .imports();
        assert_eq!(
            ts[0].names,
            vec![("a".into(), "b".into()), ("c".into(), "c".into())]
        );
        assert_eq!(
            (ts[1].module.as_str(), ts[1].alias.as_deref()),
            ("@s/p", Some("ns"))
        );
        assert!(ts[2].wildcard);
        let py = Parsed::new(
            Lang::Py,
            "import numpy as np\nfrom mcp.server import (A, B as C)\n".into(),
        )
        .unwrap()
        .imports();
        assert_eq!(py[0].alias.as_deref(), Some("np"));
        assert_eq!(py[1].names[1], ("B".into(), "C".into()));
        let rs = Parsed::new(
            Lang::Rs,
            "use std::collections::{HashMap, HashSet};\nuse serde::Serialize as S;\n".into(),
        )
        .unwrap()
        .imports();
        assert_eq!(rs[0].names.len(), 2);
        assert_eq!(rs[1].names[0], ("Serialize".into(), "S".into()));
    }
}
