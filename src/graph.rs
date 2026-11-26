//! Which stylesheets load which, so that saving a partial recompiles the
//! files that use it rather than the partial on its own.

use crate::{Lang, file_name, parent};
use openidea_plugin::Project;
use std::collections::{BTreeMap, BTreeSet};

const FIND_LIMIT: u32 = 5000;

/// `/a/./b/../c` → `/a/c`, with `/` separators: the form graph keys use.
pub fn normalize(path: &str) -> String {
    let path = path.replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "." => {}
            // Never above the root (the first part: "" for `/`, or a drive).
            ".." => {
                if parts.len() > 1 {
                    parts.pop();
                }
            }
            _ => parts.push(part),
        }
    }
    parts.join("/")
}

/// The paths a stylesheet loads, as written (one per line at most, except
/// Sass's `@import "a", "b"`). Commented-out lines and anything after a
/// line's first statement are not read.
pub fn import_specs(text: &str, lang: Lang) -> Vec<String> {
    let keywords: &[&str] = match lang {
        Lang::Less => &["@import"],
        Lang::Sass => &["@use", "@forward", "@import"],
        Lang::Stylus => &["@import", "@require"],
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim_start();
        let Some((keyword, rest)) = keywords
            .iter()
            .find_map(|k| Some((*k, t.strip_prefix(k).filter(|r| r.starts_with([' ', '\t', '(', '"', '\'']))?)))
        else {
            continue;
        };
        let statement = rest.split(';').next().unwrap_or(rest);
        if statement.contains("url(") {
            continue;
        }
        // Sass's `@import` takes a list; `@use "x" with ($c: "red")` must stop at the first.
        let all = lang == Lang::Sass && keyword == "@import";
        let mut quoted = statement.split(['"', '\'']).skip(1).step_by(2);
        if all {
            out.extend(quoted.map(str::to_owned));
        } else if let Some(first) = quoted.next() {
            out.push(first.to_owned());
        }
    }
    out
}

/// The file `spec` (written in `from`) loads, among `known` (normalized paths).
pub fn resolve(from: &str, spec: &str, lang: Lang, known: &BTreeMap<String, String>) -> Option<String> {
    let dynamic = ["#{", "@{", "$", "*", "~", "://"];
    if spec.is_empty()
        || spec.starts_with("//")
        || spec.starts_with("sass:")
        || dynamic.iter().any(|d| spec.contains(d))
    {
        return None;
    }
    let base = normalize(&format!("{}/{spec}", parent(from)?));
    let (dir, name) = base.rsplit_once('/')?;
    let has_ext = |exts: &[&str]| exts.iter().any(|e| name.ends_with(e));
    let candidates: Vec<String> = match lang {
        _ if name.ends_with(".css") => Vec::new(),
        Lang::Sass if has_ext(&[".scss", ".sass"]) => vec![base.clone(), format!("{dir}/_{name}")],
        Lang::Sass => ["scss", "sass"]
            .iter()
            .flat_map(|e| {
                [
                    format!("{base}.{e}"),
                    format!("{dir}/_{name}.{e}"),
                    format!("{base}/_index.{e}"),
                    format!("{base}/index.{e}"),
                ]
            })
            .collect(),
        Lang::Less if has_ext(&[".less"]) => vec![base.clone()],
        Lang::Less => vec![format!("{base}.less")],
        Lang::Stylus if has_ext(&[".styl"]) => vec![base.clone()],
        Lang::Stylus => vec![format!("{base}.styl"), format!("{base}/index.styl")],
    };
    candidates.into_iter().find(|c| known.contains_key(c))
}

/// The stylesheets of one project and what each loads.
#[derive(Default)]
pub struct Graph {
    /// Normalized path → the path as the host gave it.
    files: BTreeMap<String, String>,
    /// What each file loads (normalized paths).
    imports: BTreeMap<String, BTreeSet<String>>,
    /// What each file loads as written, to resolve again when files come and go.
    specs: BTreeMap<String, Vec<String>>,
}

impl Graph {
    /// Every stylesheet `find_files` returns. The flag says a limit was hit.
    pub fn build(project: &Project, globs: &[&str]) -> (Self, bool) {
        let mut graph = Self::default();
        let mut truncated = false;
        for glob in globs {
            let found = project.find_files(glob, FIND_LIMIT).unwrap_or_default();
            truncated |= found.len() >= FIND_LIMIT as usize;
            graph.files.extend(found.into_iter().map(|f| (normalize(&f), f)));
        }
        let all: Vec<String> = graph.files.values().cloned().collect();
        for file in all {
            graph.read(project, &file);
        }
        (graph, truncated)
    }

    /// (Re)reads what `path` loads; a new file joins the graph.
    pub fn read(&mut self, project: &Project, path: &str) {
        let Some(lang) = Lang::of(path) else { return };
        let key = normalize(path);
        let joined = self.files.insert(key.clone(), path.to_owned()).is_none();
        let text = project.read_file(path).unwrap_or_default();
        self.specs.insert(key.clone(), import_specs(&text, lang));
        // A new file may be what another file's import names.
        if joined { self.relink() } else { self.link(&key) }
    }

    pub fn remove(&mut self, path: &str) {
        let key = normalize(path);
        if self.files.remove(&key).is_some() {
            self.specs.remove(&key);
            self.imports.remove(&key);
            self.relink();
        }
    }

    /// Resolves what `key` loads among the files known now.
    fn link(&mut self, key: &str) {
        let Some(lang) = Lang::of(key) else { return };
        let specs = self.specs.get(key).into_iter().flatten();
        let deps = specs.filter_map(|s| resolve(key, s, lang, &self.files)).collect();
        self.imports.insert(key.to_owned(), deps);
    }

    // ponytail: every import is resolved again when a file comes or goes, from the specs kept (no reads).
    fn relink(&mut self) {
        let keys: Vec<String> = self.specs.keys().cloned().collect();
        for key in keys {
            self.link(&key);
        }
    }

    /// Every file to compile, for Compile All: what [`Self::targets`] calls entries.
    pub fn entries(&self) -> Vec<String> {
        let loaded: BTreeSet<&String> = self.imports.values().flatten().collect();
        self.files
            .iter()
            .filter(|(key, _)| match Lang::of(key) {
                Some(Lang::Sass) => !file_name(key).starts_with('_'),
                Some(_) => !loaded.contains(key),
                None => false,
            })
            .map(|(_, path)| path.clone())
            .collect()
    }

    /// The files to compile when `path` changes: itself when it is an entry,
    /// and every entry that loads it, directly or not. Sass entries are the
    /// files not named `_*`; Less and Stylus entries are the files nothing loads.
    // ponytail: importers are found by scanning every file's imports, fine for the 5000-file limit.
    pub fn targets(&self, path: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        let mut stack = vec![normalize(path)];
        while let Some(file) = stack.pop() {
            if !seen.insert(file.clone()) {
                continue;
            }
            let importers: Vec<String> =
                self.imports.iter().filter(|(_, deps)| deps.contains(&file)).map(|(f, _)| f.clone()).collect();
            let entry = match Lang::of(&file) {
                Some(Lang::Sass) => !file_name(&file).starts_with('_'),
                Some(_) => importers.is_empty(),
                None => false,
            };
            if entry {
                out.push(self.files.get(&file).cloned().unwrap_or_else(|| file.clone()));
            }
            stack.extend(importers);
        }
        out
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    fn known(paths: &[&str]) -> BTreeMap<String, String> {
        paths.iter().map(|p| (p.to_string(), p.to_string())).collect()
    }

    #[test]
    fn normalizes_paths() {
        assert_eq!(normalize("/p/styles/../vars/./a.scss"), "/p/vars/a.scss");
        assert_eq!(normalize("/../a"), "/a");
        assert_eq!(normalize(r"C:\p\a\..\b.less"), "C:/p/b.less");
    }

    #[test]
    fn reads_import_statements() {
        let sass = "@use \"sass:math\";\n@use 'theme' as t with ($c: \"red\");\n  @forward \"tokens\";\n@import \"a\", 'b';\n// @import \"commented\";\n@import url(\"x.css\");\n.a { @include x; }\n";
        assert_eq!(import_specs(sass, Lang::Sass), ["sass:math", "theme", "tokens", "a", "b"]);
        let less = "@import (reference) \"vars\";\n@import 'theme/index';\n@importance: 1;\n";
        assert_eq!(import_specs(less, Lang::Less), ["vars", "theme/index"]);
        let styl = "@require \"vars\"\n@import 'theme'\n.a\n  color c\n";
        assert_eq!(import_specs(styl, Lang::Stylus), ["vars", "theme"]);
    }

    #[test]
    fn resolves_like_each_compiler() {
        let files = known(&[
            "/p/s/_vars.scss",
            "/p/s/theme/_index.scss",
            "/p/s/plain.scss",
            "/p/l/vars.less",
            "/p/l/theme/index.less",
            "/p/y/vars.styl",
            "/p/y/theme/index.styl",
        ]);
        let r = |from: &str, spec: &str, lang| resolve(from, spec, lang, &files);
        assert_eq!(r("/p/s/main.scss", "vars", Lang::Sass).as_deref(), Some("/p/s/_vars.scss"));
        assert_eq!(r("/p/s/main.scss", "theme", Lang::Sass).as_deref(), Some("/p/s/theme/_index.scss"));
        assert_eq!(r("/p/s/deep/a.scss", "../plain.scss", Lang::Sass).as_deref(), Some("/p/s/plain.scss"));
        assert_eq!(r("/p/s/main.scss", "sass:math", Lang::Sass), None);
        assert_eq!(r("/p/s/main.scss", "other.css", Lang::Sass), None);
        assert_eq!(r("/p/s/main.scss", "#{$dir}/x", Lang::Sass), None);
        assert_eq!(r("/p/l/main.less", "vars", Lang::Less).as_deref(), Some("/p/l/vars.less"));
        assert_eq!(r("/p/l/main.less", "theme", Lang::Less), None, "Less has no index files");
        assert_eq!(r("/p/l/main.less", "theme/index", Lang::Less).as_deref(), Some("/p/l/theme/index.less"));
        assert_eq!(r("/p/y/main.styl", "vars", Lang::Stylus).as_deref(), Some("/p/y/vars.styl"));
        assert_eq!(r("/p/y/main.styl", "theme", Lang::Stylus).as_deref(), Some("/p/y/theme/index.styl"));
    }

    fn graph(edges: &[(&str, &[&str])]) -> Graph {
        let mut g = Graph::default();
        for (file, deps) in edges {
            g.files.insert(file.to_string(), file.to_string());
            g.imports.insert(file.to_string(), deps.iter().map(|d| d.to_string()).collect());
        }
        g
    }

    #[test]
    fn finds_the_entries_to_compile() {
        let g = graph(&[
            ("/p/main.scss", &["/p/_layout.scss"]),
            ("/p/admin.scss", &["/p/_vars.scss"]),
            ("/p/_layout.scss", &["/p/_vars.scss"]),
            ("/p/_vars.scss", &[]),
            ("/p/app.less", &["/p/part.less"]),
            ("/p/part.less", &["/p/app.less"]), // a cycle
            ("/p/solo.less", &[]),
            ("/p/main.styl", &["/p/vars.styl"]),
            ("/p/vars.styl", &[]),
        ]);
        let mut t = g.targets("/p/_vars.scss");
        t.sort();
        assert_eq!(t, ["/p/admin.scss", "/p/main.scss"]);
        assert_eq!(g.targets("/p/main.scss"), ["/p/main.scss"]);
        assert_eq!(g.targets("/p/solo.less"), ["/p/solo.less"]);
        assert!(g.targets("/p/part.less").is_empty(), "both load each other: no entry, and no endless loop");
        assert_eq!(g.targets("/p/vars.styl"), ["/p/main.styl"]);
        assert_eq!(g.targets("/p/new.styl"), ["/p/new.styl"], "a file the graph doesn't know yet");
        assert!(g.targets("/p/_new.scss").is_empty());
    }
}
