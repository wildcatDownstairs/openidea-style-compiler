//! Style Compiler for OpenIDEA: compiles a Less, Sass/SCSS or Stylus file to
//! CSS when it is saved and shows the compiler's error as a diagnostic on the
//! file it points at (often a partial the saved file loads).
//!
//! The compiler is the project's own when it has one (the nearest
//! `node_modules/<package>` above the file, run with `node`), else the global
//! command. Saving a partial compiles the files that load it instead (see
//! [`graph`]).

mod graph;

use graph::Graph;
use openidea_plugin::host;
use openidea_plugin::{
    Diagnostic, Error, Event, Invocation, Location, NotificationLevel, Plugin, Position, ProcessOutput, Project, Range,
    RelatedInformation, Severity, register_plugin,
};
use std::collections::{BTreeMap, BTreeSet};

const TITLE: &str = "Style Compiler";
const SOURCE_GLOBS: [&str; 4] = ["**/*.less", "**/*.scss", "**/*.sass", "**/*.styl"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Less,
    Sass,
    Stylus,
}

impl Lang {
    pub fn of(path: &str) -> Option<Self> {
        let ext = path.rsplit_once('.')?.1;
        match ext {
            "less" => Some(Self::Less),
            "scss" | "sass" => Some(Self::Sass),
            "styl" => Some(Self::Stylus),
            _ => None,
        }
    }

    /// Its setting key, npm package and diagnostic source.
    fn id(self) -> &'static str {
        match self {
            Self::Less => "less",
            Self::Sass => "sass",
            Self::Stylus => "stylus",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Less => "Less",
            Self::Sass => "Sass",
            Self::Stylus => "Stylus",
        }
    }

    /// The command line it installs (the `bin` of its package, and the global command).
    fn command(self) -> &'static str {
        match self {
            Self::Less => "lessc",
            Self::Sass => "sass",
            Self::Stylus => "stylus",
        }
    }

    fn args(self, input: &str, output: &str, source_map: bool, minify: bool) -> Vec<String> {
        let mut args: Vec<String> = match self {
            Self::Less => vec!["--no-color".into(), input.into(), output.into()],
            Self::Sass => vec![
                input.into(),
                output.into(),
                "--no-unicode".into(),
                "--no-color".into(),
                "--no-error-css".into(),
                // Sass writes a map unless told not to.
                if source_map { "--source-map" } else { "--no-source-map" }.into(),
            ],
            Self::Stylus => vec![input.into(), "--out".into(), output.into()],
        };
        if source_map {
            match self {
                Self::Less => args.push("--source-map".into()),
                Self::Stylus => args.push("--sourcemap".into()),
                Self::Sass => {}
            }
        }
        if minify {
            args.push(
                match self {
                    Self::Less => "-x",
                    Self::Sass => "--style=compressed",
                    Self::Stylus => "--compress",
                }
                .into(),
            );
        }
        args
    }

    fn parse(self, stderr: &str, root: &str) -> Option<Problem> {
        match self {
            Self::Less => parse_less_error(stderr),
            Self::Sass => parse_sass_error(stderr, root),
            Self::Stylus => parse_stylus_error(stderr),
        }
    }
}

/// Where and why a compile failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// Absolute path of the file the error is in.
    pub path: String,
    /// 0-based.
    pub line: u32,
    /// 0-based, UTF-16.
    pub column: u32,
    pub message: String,
    /// The files that loaded it, innermost first: (absolute path, line, column), 0-based.
    pub via: Vec<(String, u32, u32)>,
}

#[derive(Default)]
pub struct StyleCompiler {
    /// Files each compiled file's last run put diagnostics on, cleared by its next run.
    reported: BTreeMap<String, BTreeSet<String>>,
    /// (project root, language) already told that no compiler is available.
    told_missing: BTreeSet<(String, &'static str)>,
    /// Per project root, built on its first save; rebuilt after the plugin is reclaimed.
    graphs: BTreeMap<String, Graph>,
    /// Project roots already told that a bundler builds their styles.
    told_bundler: BTreeSet<String>,
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn parent(path: &str) -> Option<&str> {
    path.rfind(['/', '\\']).map(|i| &path[..i.max(1)])
}

/// Whether `path` is `root` or inside it (`/proj-x/a` is not inside `/proj`).
fn is_under(path: &str, root: &str) -> bool {
    path.strip_prefix(root)
        .is_some_and(|rest| rest.is_empty() || root.ends_with(['/', '\\']) || rest.starts_with(['/', '\\']))
}

/// `path` relative to `root`, with `/` separators.
fn relative(path: &str, root: &str) -> String {
    path.strip_prefix(root).unwrap_or(path).trim_start_matches(['/', '\\']).replace('\\', "/")
}

fn is_absolute(path: &str) -> bool {
    let b = path.as_bytes();
    path.starts_with(['/', '\\']) || (b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

/// Project-relative glob: `*` stays within a folder, `**` crosses folders.
fn glob_match(glob: &str, path: &str) -> bool {
    fn go(p: &[u8], s: &[u8]) -> bool {
        match p {
            [] => s.is_empty(),
            [b'*', b'*', b'/', rest @ ..] => go(rest, s) || (!s.is_empty() && go(p, &s[1..])),
            [b'*', b'*', rest @ ..] => (0..=s.len()).any(|i| go(rest, &s[i..])),
            [b'*', rest @ ..] => (0..=s.len()).take_while(|&i| i == 0 || s[i - 1] != b'/').any(|i| go(rest, &s[i..])),
            [b'?', rest @ ..] => !s.is_empty() && s[0] != b'/' && go(rest, &s[1..]),
            [c, rest @ ..] => s.first() == Some(c) && go(rest, &s[1..]),
        }
    }
    go(glob.as_bytes(), path.as_bytes())
}

/// The CSS file for `file`: beside it, or under `out_dir` (project-relative)
/// with the same folders. `None` when `out_dir` would leave the project.
pub fn output_path(root: &str, file: &str, out_dir: &str) -> Option<String> {
    let css = match file.rfind('.') {
        Some(dot) if dot > file.rfind(['/', '\\']).unwrap_or(0) => format!("{}.css", &file[..dot]),
        _ => format!("{file}.css"),
    };
    let out_dir = out_dir.trim();
    if is_absolute(out_dir) || out_dir.split(['/', '\\']).any(|part| part == "..") {
        return None;
    }
    let out_dir = out_dir.trim_matches(['/', '\\']);
    if out_dir.is_empty() {
        return Some(css);
    }
    Some(format!("{}/{out_dir}/{}", root.trim_end_matches(['/', '\\']), relative(&css, root)))
}

/// The script a `package.json` runs as `command` (its `bin`, a path or a map).
pub fn bin_script(package_json: &str, command: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(package_json).ok()?;
    let bin = json.get("bin")?;
    let script = bin.as_str().or_else(|| bin.get(command)?.as_str())?;
    Some(script.trim_start_matches("./").to_owned())
}

/// `path  3:8  member` — a line of Sass's stack under the code excerpt.
fn sass_frame(line: &str) -> Option<(&str, u32, u32)> {
    let rest = line.strip_prefix("  ")?;
    if rest.starts_with(' ') || rest.contains('|') {
        return None;
    }
    rest.match_indices(' ').find_map(|(i, _)| {
        let (pos, _) = rest[i + 1..].split_once(char::is_whitespace).unwrap_or((&rest[i + 1..], ""));
        let (l, c) = pos.split_once(':')?;
        Some((&rest[..i], l.parse().ok()?, c.parse().ok()?))
    })
}

/// The error in Sass's `--no-unicode --no-color` stderr. Its paths are
/// relative to the working directory, the project root.
pub fn parse_sass_error(stderr: &str, root: &str) -> Option<Problem> {
    let message = stderr.lines().find_map(|l| l.strip_prefix("Error: "))?.trim().to_owned();
    let abs = |p: &str| if is_absolute(p) { p.to_owned() } else { format!("{}/{p}", root.trim_end_matches('/')) };
    let mut frames = stderr
        .lines()
        .filter_map(sass_frame)
        .map(|(p, l, c): (&str, u32, u32)| (abs(p), l.saturating_sub(1), c.saturating_sub(1)));
    let (path, line, column) = frames.next()?;
    Some(Problem { path, line, column, message, via: frames.collect() })
}

/// `NameError: variable @x is undefined in /abs/a.less on line 2, column 13:`
/// (`--no-color`).
pub fn parse_less_error(stderr: &str) -> Option<Problem> {
    stderr.lines().find_map(|l| {
        let head = l.strip_suffix(':')?;
        let (head, at) = head.rsplit_once(" on line ")?;
        let (line, column) = at.split_once(", column ")?;
        let (kind_message, path) = head.rsplit_once(" in ")?;
        let (kind, message) = kind_message.split_once(": ")?;
        if !kind.ends_with("Error") {
            return None;
        }
        Some(Problem {
            path: path.to_owned(),
            line: line.parse::<u32>().ok()?.saturating_sub(1),
            column: column.parse::<u32>().ok()?.saturating_sub(1),
            message: message.trim().to_owned(),
            via: Vec::new(),
        })
    })
}

/// Stylus throws: `ParseError: /abs/a.styl:4:1`, the code excerpt, a blank
/// line, the message (then Node's stack, ignored).
pub fn parse_stylus_error(stderr: &str) -> Option<Problem> {
    let mut lines = stderr.lines().skip_while(|l| {
        !l.split_once(": ").is_some_and(|(kind, _)| kind.ends_with("Error") && !kind.contains(char::is_whitespace))
    });
    let (_, location) = lines.next()?.split_once(": ")?;
    let mut parts = location.rsplitn(3, ':');
    let column: u32 = parts.next()?.parse().ok()?;
    let line: u32 = parts.next()?.parse().ok()?;
    let path = parts.next()?.to_owned();
    let excerpt = |l: &str| {
        let t = l.trim_start();
        t.is_empty()
            || t.split_once('|').is_some_and(|(n, _)| n.chars().all(|c| c.is_ascii_digit()))
            || t.ends_with('^')
    };
    let message = lines.find(|l| !excerpt(l))?.trim().to_owned();
    Some(Problem { path, line: line.saturating_sub(1), column: column.saturating_sub(1), message, via: Vec::new() })
}

fn point(line: u32, column: u32) -> Range {
    Range::new(Position::new(line, column), Position::new(line, column))
}

fn diagnostic(p: &Problem, text: &str, lang: Lang) -> Diagnostic {
    let line_len =
        text.split('\n').nth(p.line as usize).map_or(0, |l| l.trim_end_matches('\r').encode_utf16().count() as u32);
    let start = p.column.min(line_len);
    let end = if start < line_len { start + 1 } else { line_len };
    let mut d = Diagnostic::new(
        Range::new(Position::new(p.line, start), Position::new(p.line, end)),
        Severity::Error,
        p.message.clone(),
    );
    d.source = Some(lang.id().into());
    d.related = p
        .via
        .iter()
        .map(|(path, line, column)| RelatedInformation {
            location: Location { path: path.clone(), range: point(*line, *column) },
            message: "Loaded from here".into(),
        })
        .collect();
    d
}

/// The first line that names an error, else the first line: for output no parser understood.
fn first_error_line(stderr: &str) -> &str {
    let mut lines = stderr.lines().map(str::trim).filter(|l| !l.is_empty());
    lines.clone().find(|l| l.contains("Error")).or_else(|| lines.next()).unwrap_or("failed without a message")
}

/// Runs the project's compiler with `node`, else the global command.
fn run_compiler(
    project: &Project,
    root: &str,
    file: &str,
    output: &str,
    lang: Lang,
    args: &[String],
) -> Result<ProcessOutput, Error> {
    for d in dirs_up(file, root) {
        let package = format!("{d}/node_modules/{}", lang.id());
        if let Some(script) =
            project.read_file(&format!("{package}/package.json")).ok().and_then(|j| bin_script(&j, lang.command()))
        {
            // lessc 4.9 creates only the last missing folder of the output; make them all.
            if let Some(out_dir) = parent(output).filter(|o| !project.exists(o).unwrap_or(true)) {
                let mkdir =
                    ["-e".into(), "require('fs').mkdirSync(process.argv[1], {recursive: true})".into(), out_dir.into()];
                match project.run_process("node", &mkdir, None) {
                    Err(Error::Denied(_)) => break, // `process:node` refused: try the global command
                    result => result?,
                };
            }
            let argv: Vec<String> =
                std::iter::once(format!("{package}/{script}")).chain(args.iter().cloned()).collect();
            match project.run_process("node", &argv, None) {
                Err(Error::Denied(_)) => break,
                result => return result,
            }
        }
    }
    project.run_process(lang.command(), args, None)
}

/// The folders from `file`'s up to `root`, nearest first.
fn dirs_up<'a>(file: &'a str, root: &str) -> Vec<&'a str> {
    std::iter::successors(parent(file), |d| parent(d).filter(|p| p.len() < d.len()))
        .take_while(|d| is_under(d, root))
        .collect()
}

const BUILD_TOOLS: [&str; 6] = ["vite", "webpack", "parcel", "next", "nuxt", "@angular/cli"];

/// The bundler a `package.json` depends on, which then builds the styles itself.
pub fn build_tool(package_json: &str) -> Option<&'static str> {
    let json: serde_json::Value = serde_json::from_str(package_json).ok()?;
    BUILD_TOOLS.into_iter().find(|tool| ["dependencies", "devDependencies"].iter().any(|k| json[k].get(tool).is_some()))
}

impl StyleCompiler {
    /// Compiles `path` and publishes its problems: how many, or `None` when it is
    /// not compiled (a partial, ignored, turned off, or no compiler).
    fn compile(&mut self, path: &str) -> Result<Option<usize>, Error> {
        let (Some(lang), Some(project)) = (Lang::of(path), host::project_of(path)) else { return Ok(None) };
        let root = project.root()?;
        let rel = relative(path, &root);
        let ignored = host::setting_string("ignore").unwrap_or_default();
        if host::setting_bool(lang.id()) == Some(false)
            || ignored.lines().map(str::trim).any(|g| !g.is_empty() && glob_match(g, &rel))
        {
            return Ok(None);
        }
        let out_dir = host::setting_string("outDir").unwrap_or_default();
        let output = output_path(&root, path, &out_dir)
            .ok_or_else(|| Error::Invalid(format!("the output directory \"{out_dir}\" is outside the project")))?;
        let args = lang.args(
            path,
            &output,
            host::setting_bool("sourceMap") == Some(true),
            host::setting_bool("minify") == Some(true),
        );
        let out = match run_compiler(&project, &root, path, &output, lang, &args) {
            Err(Error::Denied(_) | Error::NotFound(_)) => {
                if self.told_missing.insert((root, lang.id())) {
                    let (name, package, command) = (lang.title(), lang.id(), lang.command());
                    host::notify(
                        NotificationLevel::Warning,
                        TITLE,
                        &format!(
                            "{name} is not available. Install it in the project (npm i -D {package}) and allow \
                             process:node, or allow process:{command} for a global install, in Settings › Plugins."
                        ),
                    );
                }
                return Ok(None);
            }
            result => result?,
        };
        let problems = match out.status {
            Some(0) => Vec::new(),
            None => return Err(Error::Cancelled),
            Some(_) => vec![
                lang.parse(&out.stderr, &root)
                    .ok_or_else(|| Error::Failed(format!("{}: {}", lang.command(), first_error_line(&out.stderr))))?,
            ],
        };
        self.publish(&project, path, lang, &problems)?;
        Ok(Some(problems.len()))
    }

    /// Whether a bundler in a `package.json` above `path` builds it (mode `auto`
    /// skips those on save); says so once per project.
    fn bundled(&mut self, path: &str) -> bool {
        let Some(project) = host::project_of(path) else { return false };
        let Ok(root) = project.root() else { return false };
        let tool = dirs_up(path, &root)
            .into_iter()
            .find_map(|d| project.read_file(&format!("{d}/package.json")).ok().and_then(|j| build_tool(&j)));
        if let Some(tool) = tool
            && self.told_bundler.insert(root)
        {
            host::notify(
                NotificationLevel::Info,
                TITLE,
                &format!(
                    "This project builds its styles with {tool}, so they are not compiled on save. To compile \
                     them anyway, set Mode to \"always\" for this project in Settings › Plugins."
                ),
            );
        }
        tool.is_some()
    }

    /// The entries to compile after `path` changed (see [`Graph::targets`]).
    fn targets(&mut self, path: &str) -> Vec<String> {
        let Some(project) = host::project_of(path) else { return Vec::new() };
        let graph = self.graph(&project);
        graph.read(&project, path);
        graph.targets(path)
    }

    /// `project`'s graph, built when first needed.
    fn graph(&mut self, project: &Project) -> &mut Graph {
        let root = project.root().unwrap_or_default();
        self.graphs.entry(root).or_insert_with(|| {
            let (graph, truncated) = Graph::build(project, &SOURCE_GLOBS);
            if truncated {
                host::notify(
                    NotificationLevel::Warning,
                    TITLE,
                    "This project has too many stylesheets to track which load which: saving a partial may not \
                     recompile every file that uses it. Narrow it with the \"Files not to compile\" setting.",
                );
            }
            graph
        })
    }

    /// Compiles `paths` under one progress task, telling the user of failures
    /// other than compile errors. (compiled cleanly, with errors), or `None` when cancelled.
    fn compile_all<'a>(&mut self, paths: impl ExactSizeIterator<Item = &'a String>) -> Option<(Vec<String>, usize)> {
        let total = paths.len() as u32;
        let task = host::begin_task("Compiling styles", true);
        let (mut clean, mut failed) = (Vec::new(), 0);
        for (done, path) in paths.enumerate() {
            match self.compile(path) {
                Ok(Some(0)) => clean.push(file_name(path).to_owned()),
                Ok(Some(_)) => failed += 1,
                Ok(None) => {}
                Err(Error::Cancelled) => return None,
                Err(e) => host::notify(NotificationLevel::Warning, TITLE, &format!("{}: {e}", file_name(path))),
            }
            if task.is_cancelled() {
                return None;
            }
            task.report(done as u32 + 1, Some(total), None);
        }
        Some((clean, failed))
    }

    /// Keeps the built graphs in step with files created, changed or deleted outside the editor.
    fn files_changed(&mut self, created_or_modified: &[String], removed: &[String]) {
        let changed = created_or_modified.iter().map(|p| (p, true)).chain(removed.iter().map(|p| (p, false)));
        for (path, exists) in changed.filter(|(p, _)| Lang::of(p).is_some()) {
            let Some(project) = host::project_of(path) else { continue };
            let Some(graph) = project.root().ok().and_then(|r| self.graphs.get_mut(&r)) else { continue };
            if exists {
                graph.read(&project, path);
            } else {
                graph.remove(path);
            }
        }
    }

    /// Replaces what `entry`'s last compile reported with `problems`.
    // ponytail: two entries loading one broken partial share its diagnostics; the last compile wins.
    fn publish(&mut self, project: &Project, entry: &str, lang: Lang, problems: &[Problem]) -> Result<(), Error> {
        let now: BTreeSet<String> = problems.iter().map(|p| p.path.clone()).collect();
        for stale in self.reported.get(entry).into_iter().flatten().filter(|f| !now.contains(*f)) {
            host::publish_diagnostics(stale, &[])?;
        }
        for file in &now {
            let text = project.read_file(file).unwrap_or_default();
            let diagnostics: Vec<Diagnostic> =
                problems.iter().filter(|p| &p.path == file).map(|p| diagnostic(p, &text, lang)).collect();
            host::publish_diagnostics(file, &diagnostics)?;
        }
        self.reported.insert(entry.to_owned(), now);
        Ok(())
    }
}

impl Plugin for StyleCompiler {
    fn activate(&mut self, _ctx: &openidea_plugin::Context) -> Result<(), Error> {
        host::subscribe_paths("file-saved", &SOURCE_GLOBS)?;
        host::subscribe_paths("files-changed", &SOURCE_GLOBS)
    }

    fn run_command(&mut self, id: &str, ctx: &Invocation, _arg: Option<&str>) -> Result<Option<String>, Error> {
        match id {
            "compile-current" => {
                let path = ctx.editor().ok_or("no file is open")?.path();
                if Lang::of(&path).is_none() {
                    return Err("the current file is not a Less, Sass, SCSS or Stylus file".into());
                }
                let targets = self.targets(&path);
                let (clean, failed) = self.compile_all(targets.iter()).ok_or(Error::Cancelled)?;
                Ok(Some(if failed > 0 {
                    "Not compiled: see the error in the Problems view".into()
                } else if clean.is_empty() {
                    "Not compiled (a partial nothing loads, ignored or turned off)".into()
                } else {
                    format!("Compiled {}", clean.join(", "))
                }))
            }
            "compile-all" => {
                let project = ctx.project().ok_or("no project is open")?;
                let entries = self.graph(&project).entries();
                let (clean, failed) = self.compile_all(entries.iter()).ok_or(Error::Cancelled)?;
                Ok(Some(match (clean.len(), failed) {
                    (0, 0) => "No stylesheets to compile".into(),
                    (n, 0) => format!("Compiled {n} files"),
                    (n, f) => format!("Compiled {n} files; {f} failed, see the Problems view"),
                }))
            }
            _ => Err(Error::NotFound(format!("command {id}"))),
        }
    }

    fn handle_events(&mut self, events: &[Event], _ctx: &Invocation) {
        let mut saved = BTreeSet::new();
        for event in events {
            match event {
                Event::FileSaved(p) if Lang::of(p).is_some() => {
                    saved.insert(p.as_str());
                }
                Event::FilesChanged(c) => {
                    self.files_changed(&[c.created.as_slice(), c.modified.as_slice()].concat(), &c.removed)
                }
                _ => {}
            }
        }
        let mode = host::setting_string("mode").unwrap_or_default();
        if saved.is_empty() || mode == "off" {
            return;
        }
        // A partial and the file that loads it, saved together, compile that file once.
        let mut targets: BTreeSet<String> = saved.iter().flat_map(|p| self.targets(p)).collect();
        if mode != "always" {
            targets.retain(|t| !self.bundled(t));
        }
        if !targets.is_empty() {
            self.compile_all(targets.iter());
        }
    }
}

register_plugin!(StyleCompiler);

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use openidea_plugin::mock::MockHost;

    // Real stderr of sass 1.108.2 (`--no-unicode --no-color`), run in the project root.
    const SYNTAX: &str =
        "Error: expected \";\".\n  ,\n3 |   width: 10px;\n  |        ^\n  '\n  syntax.scss 3:8  root stylesheet\n";
    const IN_PARTIAL: &str = "Error: Expected digit.\n  ,\n2 | .x { color: $nope; }\n  |  ^\n  '\n  styles/_vars.scss 2:2        @use\n  styles/main.scss 1:1  root stylesheet\n";
    // `.a { content: "😀中"; color: $x; }`: column 29 is UTF-16 index 28, 1-based.
    const EMOJI: &str = "Error: Undefined variable.\n  ,\n1 | .a { content: \"😀中\"; color: $x; }\n  |                             ^^\n  '\n  emoji.scss 1:29  root stylesheet\n";

    fn ok() -> ProcessOutput {
        ProcessOutput { status: Some(0), stdout: String::new(), stderr: String::new() }
    }

    fn failed(stderr: &str) -> ProcessOutput {
        ProcessOutput { status: Some(65), stdout: String::new(), stderr: stderr.into() }
    }

    #[test]
    fn parses_sass_errors() {
        let p = parse_sass_error(SYNTAX, "/project").unwrap();
        assert_eq!(
            (p.path.as_str(), p.line, p.column, p.message.as_str()),
            ("/project/syntax.scss", 2, 7, "expected \";\".")
        );
        assert!(p.via.is_empty());

        let p = parse_sass_error(IN_PARTIAL, "/project/").unwrap();
        assert_eq!((p.path.as_str(), p.line, p.column), ("/project/styles/_vars.scss", 1, 1));
        assert_eq!(p.via, [("/project/styles/main.scss".to_owned(), 0, 0)]);

        assert_eq!(parse_sass_error(EMOJI, "/p").unwrap().column, 28);
        assert_eq!(
            parse_sass_error("Error: boom\n  /abs/a.scss 1:1  root stylesheet\n", "/p").unwrap().path,
            "/abs/a.scss"
        );
        assert_eq!(parse_sass_error("node: bad option\n", "/p"), None);
    }

    #[test]
    fn reads_bin_scripts() {
        assert_eq!(bin_script(r#"{"name":"sass","bin":{"sass":"sass.js"}}"#, "sass").as_deref(), Some("sass.js"));
        assert_eq!(bin_script(r#"{"bin":"./bin/sass"}"#, "sass").as_deref(), Some("bin/sass"));
        assert_eq!(bin_script(r#"{"bin":{"other":"x.js"}}"#, "sass"), None);
        assert_eq!(bin_script("not json", "sass"), None);
    }

    #[test]
    fn places_the_output() {
        assert_eq!(output_path("/p", "/p/src/a.scss", "").as_deref(), Some("/p/src/a.css"));
        assert_eq!(output_path("/p", "/p/src/a.sass", " dist/ ").as_deref(), Some("/p/dist/src/a.css"));
        assert_eq!(output_path("/p/", "/p/a.v2/b.scss", "out").as_deref(), Some("/p/out/a.v2/b.css"));
        assert_eq!(output_path("/p", "/p/a.scss", "../x"), None);
        assert_eq!(output_path("/p", "/p/a.scss", "/tmp"), None);
    }

    #[test]
    fn globs() {
        assert!(glob_match("node_modules/**", "node_modules/bootstrap/scss/a.scss"));
        assert!(glob_match("**/vendor/*.scss", "src/vendor/a.scss"));
        assert!(!glob_match("vendor/*.scss", "vendor/deep/a.scss"));
        assert!(!glob_match("node_modules/**", "src/node_modules.scss"));
    }

    #[test]
    fn compiles_with_the_projects_sass_and_reports_errors_on_the_partial() {
        let host = MockHost::install().with_capabilities(&["project.read", "events", "process:node", "process:sass"]);
        host.add_file("node_modules/sass/package.json", r#"{"bin":{"sass":"sass.js"}}"#);
        host.add_file("styles/main.scss", "@use \"vars\";\n.a { color: vars.$main; }\n");
        host.add_file("styles/_vars.scss", "$main: red\n.x { color: $nope; }\n");
        host.add_dir("styles"); // the mock infers no folders from files; no `mkdir` run for an existing one
        let result = std::rc::Rc::new(std::cell::RefCell::new(failed(IN_PARTIAL)));
        let next = result.clone();
        host.on_process("node", move |_, _| Ok(next.borrow().clone()));
        let mut plugin = StyleCompiler::default();
        let save = |plugin: &mut StyleCompiler| {
            plugin.handle_events(&[Event::FileSaved("/project/styles/main.scss".into())], &host.invocation())
        };

        save(&mut plugin);
        let run = &host.process_runs()[0];
        assert_eq!(run.program, "node");
        assert_eq!(run.cwd.as_deref(), Some("/project"));
        assert_eq!(
            run.args,
            [
                "/project/node_modules/sass/sass.js",
                "/project/styles/main.scss",
                "/project/styles/main.css",
                "--no-unicode",
                "--no-color",
                "--no-error-css",
                "--no-source-map"
            ]
        );
        let d = &host.diagnostics()["/project/styles/_vars.scss"];
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].range, Range::new(Position::new(1, 1), Position::new(1, 2)));
        assert_eq!(d[0].message, "Expected digit.");
        assert_eq!(d[0].related[0].location.path, "/project/styles/main.scss");
        assert!(host.diagnostics().get("/project/styles/main.scss").is_none_or(Vec::is_empty));

        *result.borrow_mut() = ok();
        save(&mut plugin);
        assert!(
            host.diagnostics().get("/project/styles/_vars.scss").is_none_or(Vec::is_empty),
            "a clean compile clears them"
        );
        assert!(host.tasks().iter().all(|t| t.ended));
    }

    #[test]
    fn falls_back_to_the_global_sass_and_follows_the_settings() {
        let host = MockHost::install().with_capabilities(&["project.read", "events", "process:sass"]);
        host.add_file("node_modules/sass/package.json", r#"{"bin":{"sass":"sass.js"}}"#);
        host.add_file("a.scss", ".a{}");
        host.set_setting("outDir", "\"dist\"");
        host.set_setting("sourceMap", "true");
        host.set_setting("minify", "true");
        host.on_process("sass", |_, _| Ok(ok()));
        StyleCompiler::default().handle_events(&[Event::FileSaved("/project/a.scss".into())], &host.invocation());
        // `process:node` is not granted, so the project's copy is skipped.
        let runs = host.process_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].program, "sass");
        assert_eq!(&runs[0].args[..2], ["/project/a.scss", "/project/dist/a.css"]);
        assert_eq!(&runs[0].args[5..], ["--source-map", "--style=compressed"]);
    }

    #[test]
    fn skips_partials_ignored_files_and_other_languages() {
        let host = MockHost::install().with_capabilities(&["project.read", "events", "process:sass"]);
        host.set_setting("ignore", "\"vendor/**\\n\"");
        host.on_process("sass", |_, _| Ok(ok()));
        StyleCompiler::default().handle_events(
            &[
                Event::FileSaved("/project/_vars.scss".into()),
                Event::FileSaved("/project/vendor/lib.scss".into()),
                Event::FileSaved("/project/a.css".into()),
                Event::FileSaved("/project/b.less.txt".into()),
            ],
            &host.invocation(),
        );
        assert!(host.process_runs().is_empty());

        host.set_setting("sass", "false");
        StyleCompiler::default().handle_events(&[Event::FileSaved("/project/a.scss".into())], &host.invocation());
        assert!(host.process_runs().is_empty());
    }

    #[test]
    fn a_missing_compiler_is_reported_once_per_project() {
        let host = MockHost::install().with_capabilities(&["project.read", "events"]);
        let mut plugin = StyleCompiler::default();
        for _ in 0..2 {
            plugin.handle_events(&[Event::FileSaved("/project/a.scss".into())], &host.invocation());
        }
        let notes = host.notifications();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].body.contains("npm i -D sass"));
    }

    #[test]
    fn the_command_says_whether_it_compiled() {
        let host = MockHost::install().with_capabilities(&["project.read", "events", "process:sass"]);
        host.add_file("a.scss", ".a{}");
        host.on_process("sass", |_, _| Ok(failed(SYNTAX)));
        let mut plugin = StyleCompiler::default();
        let run = |plugin: &mut StyleCompiler, path: &str| {
            host.open_editor(path, "scss", ".a{}", &[(0, 0, 0, 0)]);
            plugin.run_command("compile-current", &host.invocation(), None)
        };
        assert_eq!(
            run(&mut plugin, "/project/a.scss").unwrap().as_deref(),
            Some("Not compiled: see the error in the Problems view")
        );
        assert_eq!(
            run(&mut plugin, "/project/_b.scss").unwrap().as_deref(),
            Some("Not compiled (a partial nothing loads, ignored or turned off)")
        );
        assert!(run(&mut plugin, "/project/c.css").is_err());
    }

    // Real stderr of less 4.9.1 (`--no-color`).
    const LESS_NAME: &str = "NameError: variable @nope is undefined in /project/styles/part.less on line 2, column 13:\n1 @main: red;\n2 .x { color: @nope; }\n3 \n\n";
    const LESS_FILE: &str = "FileError: 'missing' wasn't found. Tried - /project/styles/missing.less,npm://missing,missing.less in /project/styles/m.less on line 1, column 1:\n1 @import \"missing\";\n2 \n\n";
    // Real stderr of stylus 0.64.0: an exception, so Node's stack follows.
    const STYLUS_PARSE: &str = "/project/node_modules/stylus/bin/stylus:684\n              throw err;\n              ^\n\nParseError: /project/styles/vars.styl:4:1\n   1| main = red\n   2| .x\n   3|   w: (1px +\n   4| \n------^\n\nexpected \")\", got \"outdent\"\n\n    at Parser.error (/project/node_modules/stylus/lib/parser.js:252:11)\n  filename: '/project/styles/vars.styl',\n  lineno: 4,\n  column: 1,\n}\n\nNode.js v24.16.0\n";
    const STYLUS_IMPORT: &str = "/project/node_modules/stylus/bin/stylus:684\n              throw err;\n              ^\n\nError: /project/badimport.styl:1:9\n   1| @import \"nothere\"\n--------------^\n   2| \n\nfailed to locate @import file nothere.styl\n\n  lineno: 1,\n";

    #[test]
    fn parses_less_and_stylus_errors() {
        let p = parse_less_error(LESS_NAME).unwrap();
        assert_eq!(
            (p.path.as_str(), p.line, p.column, p.message.as_str()),
            ("/project/styles/part.less", 1, 12, "variable @nope is undefined")
        );
        let p = parse_less_error(LESS_FILE).unwrap();
        assert_eq!((p.path.as_str(), p.line, p.column), ("/project/styles/m.less", 0, 0));
        assert!(p.message.starts_with("'missing' wasn't found."), "{}", p.message);
        assert_eq!(parse_less_error("The compress option has been deprecated.\n"), None);

        let p = parse_stylus_error(STYLUS_PARSE).unwrap();
        assert_eq!(
            (p.path.as_str(), p.line, p.column, p.message.as_str()),
            ("/project/styles/vars.styl", 3, 0, "expected \")\", got \"outdent\"")
        );
        let p = parse_stylus_error(STYLUS_IMPORT).unwrap();
        assert_eq!(
            (p.path.as_str(), p.line, p.column, p.message.as_str()),
            ("/project/badimport.styl", 0, 8, "failed to locate @import file nothere.styl")
        );
        assert_eq!(parse_stylus_error("Error: Cannot find module '/x/stylus'\n"), None);
        assert_eq!(first_error_line("\nnode:fs:1350\nError: ENOENT: no such file\n"), "Error: ENOENT: no such file");
    }

    #[test]
    fn builds_each_compilers_command_line() {
        assert_eq!(Lang::Less.args("a.less", "a.css", false, false), ["--no-color", "a.less", "a.css"]);
        assert_eq!(
            Lang::Less.args("a.less", "a.css", true, true),
            ["--no-color", "a.less", "a.css", "--source-map", "-x"]
        );
        assert_eq!(Lang::Stylus.args("a.styl", "o/a.css", false, false), ["a.styl", "--out", "o/a.css"]);
        assert_eq!(
            Lang::Stylus.args("a.styl", "o/a.css", true, true),
            ["a.styl", "--out", "o/a.css", "--sourcemap", "--compress"]
        );
        assert_eq!(Lang::of("/p/a.styl"), Some(Lang::Stylus));
        assert_eq!(Lang::of("/p/a.v2/b"), None);
    }

    #[test]
    fn makes_the_output_folders_before_running_the_projects_lessc() {
        let host = MockHost::install().with_capabilities(&["project.read", "events", "process:node"]);
        host.add_file("node_modules/less/package.json", r#"{"bin":{"lessc":"./bin/lessc"}}"#);
        host.add_file("styles/part.less", "@main: red;\n.x { color: @nope; }\n");
        host.set_setting("outDir", "\"dist\"");
        host.on_process("node", |args, _| Ok(if args[0] == "-e" { ok() } else { failed(LESS_NAME) }));
        StyleCompiler::default()
            .handle_events(&[Event::FileSaved("/project/styles/main.less".into())], &host.invocation());
        let runs = host.process_runs();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].args[2], "/project/dist/styles");
        assert_eq!(
            runs[1].args,
            [
                "/project/node_modules/less/bin/lessc",
                "--no-color",
                "/project/styles/main.less",
                "/project/dist/styles/main.css"
            ]
        );
        let d = &host.diagnostics()["/project/styles/part.less"];
        assert_eq!((d[0].range.start, d[0].source.as_deref()), (Position::new(1, 12), Some("less")));
    }

    #[test]
    fn falls_back_to_the_global_stylus() {
        let host = MockHost::install().with_capabilities(&["project.read", "events", "process:stylus"]);
        host.add_file("badimport.styl", "@import \"nothere\"\n");
        host.on_process("stylus", |_, _| Ok(failed(STYLUS_IMPORT)));
        let mut plugin = StyleCompiler::default();
        plugin.handle_events(&[Event::FileSaved("/project/badimport.styl".into())], &host.invocation());
        assert_eq!(host.process_runs()[0].args, ["/project/badimport.styl", "--out", "/project/badimport.css"]);
        assert_eq!(host.diagnostics()["/project/badimport.styl"][0].range.start, Position::new(0, 8));

        host.set_setting("stylus", "false");
        plugin.handle_events(&[Event::FileSaved("/project/badimport.styl".into())], &host.invocation());
        assert_eq!(host.process_runs().len(), 1, "turned off");
    }

    #[test]
    fn each_missing_compiler_is_reported_once() {
        let host = MockHost::install().with_capabilities(&["project.read", "events"]);
        let mut plugin = StyleCompiler::default();
        for path in ["/project/a.styl", "/project/b.styl", "/project/a.less"] {
            plugin.handle_events(&[Event::FileSaved(path.into())], &host.invocation());
        }
        let bodies: Vec<String> = host.notifications().into_iter().map(|n| n.body).collect();
        assert_eq!(bodies.len(), 2, "{bodies:?}");
        assert!(bodies[0].contains("npm i -D stylus") && bodies[0].contains("process:stylus"));
        assert!(bodies[1].contains("npm i -D less") && bodies[1].contains("process:lessc"));
    }

    #[test]
    fn an_output_directory_outside_the_project_is_refused() {
        let host = MockHost::install().with_capabilities(&["project.read", "events", "process:sass"]);
        host.set_setting("outDir", "\"../out\"");
        host.on_process("sass", |_, _| Ok(ok()));
        StyleCompiler::default().handle_events(&[Event::FileSaved("/project/a.scss".into())], &host.invocation());
        assert!(host.process_runs().is_empty());
        assert!(host.notifications()[0].body.contains("outside the project"));
    }

    /// The input file of each compiler run, in order.
    fn compiled(host: &MockHost) -> Vec<String> {
        host.process_runs()
            .iter()
            .map(|r| r.args.iter().find(|a| Lang::of(a).is_some()).cloned().unwrap_or_default())
            .collect()
    }

    #[test]
    fn saving_a_partial_compiles_the_files_that_load_it() {
        let host = MockHost::install().with_capabilities(&["project.read", "events", "process:lessc"]);
        host.add_file("styles/vars.less", "@main: red;\n");
        host.add_file("styles/part.less", "@import (reference) \"vars\";\n.x { color: @nope; }\n");
        host.add_file("styles/app.less", "@import \"part\";\n");
        host.add_file("styles/admin.less", "@import \"vars.less\";\n");
        host.on_process("lessc", |_, _| Ok(failed(LESS_NAME)));
        let mut plugin = StyleCompiler::default();
        let save = |plugin: &mut StyleCompiler, paths: &[&str]| {
            let events: Vec<Event> = paths.iter().map(|p| Event::FileSaved(format!("/project/{p}"))).collect();
            plugin.handle_events(&events, &host.invocation());
        };

        save(&mut plugin, &["styles/vars.less"]);
        assert_eq!(compiled(&host), ["/project/styles/admin.less", "/project/styles/app.less"]);
        assert_eq!(host.diagnostics()["/project/styles/part.less"][0].message, "variable @nope is undefined");

        save(&mut plugin, &["styles/part.less", "styles/app.less"]);
        assert_eq!(compiled(&host)[2..], ["/project/styles/app.less"], "once, and the partial not on its own");
    }

    #[test]
    fn follows_files_created_and_deleted_outside_the_editor() {
        let host = MockHost::install().with_capabilities(&["project.read", "events", "process:stylus"]);
        host.add_file("vars.styl", "c = red\n");
        host.add_file("a.styl", "@require \"vars\"\n");
        host.on_process("stylus", |_, _| Ok(ok()));
        let mut plugin = StyleCompiler::default();
        let save_vars = |plugin: &mut StyleCompiler| {
            plugin.handle_events(&[Event::FileSaved("/project/vars.styl".into())], &host.invocation())
        };
        save_vars(&mut plugin);
        assert_eq!(compiled(&host), ["/project/a.styl"]);

        host.add_file("b.styl", "@import 'vars'\n");
        let change = |created: &[&str], removed: &[&str]| {
            Event::FilesChanged(openidea_plugin::FilesChanged {
                created: created.iter().map(|p| p.to_string()).collect(),
                modified: Vec::new(),
                removed: removed.iter().map(|p| p.to_string()).collect(),
            })
        };
        plugin.handle_events(&[change(&["/project/b.styl"], &[])], &host.invocation());
        save_vars(&mut plugin);
        assert_eq!(compiled(&host)[1..], ["/project/a.styl", "/project/b.styl"]);

        plugin.handle_events(&[change(&[], &["/project/a.styl"])], &host.invocation());
        save_vars(&mut plugin);
        assert_eq!(compiled(&host)[3..], ["/project/b.styl"]);
    }

    #[test]
    fn the_command_on_a_partial_compiles_its_users() {
        let host = MockHost::install().with_capabilities(&["project.read", "events", "process:sass"]);
        host.add_file("_vars.scss", "$c: red;\n");
        host.add_file("main.scss", "@use \"vars\";\n");
        host.on_process("sass", |_, _| Ok(ok()));
        host.open_editor("/project/_vars.scss", "scss", "$c: red;\n", &[(0, 0, 0, 0)]);
        let message = StyleCompiler::default().run_command("compile-current", &host.invocation(), None).unwrap();
        assert_eq!(message.as_deref(), Some("Compiled main.scss"));
        assert_eq!(compiled(&host), ["/project/main.scss"]);
    }

    #[test]
    fn recognises_bundlers() {
        assert_eq!(build_tool(r#"{"devDependencies":{"vite":"^7.0.0","sass":"^1"}}"#), Some("vite"));
        assert_eq!(build_tool(r#"{"dependencies":{"@angular/cli":"20"}}"#), Some("@angular/cli"));
        assert_eq!(build_tool(r#"{"devDependencies":{"sass":"^1","vite-plugin-x":"1"}}"#), None);
        assert_eq!(build_tool(r#"{"scripts":{"build":"vite build"}}"#), None, "only dependencies count");
        assert_eq!(build_tool("not json"), None);
    }

    #[test]
    fn auto_mode_leaves_bundled_projects_alone_on_save() {
        let host = MockHost::install().with_capabilities(&["project.read", "events", "process:sass"]);
        host.add_file("web/package.json", r#"{"devDependencies":{"vite":"^7.0.0"}}"#);
        host.add_file("web/src/a.scss", ".a{}");
        host.add_file("site/b.scss", ".b{}");
        host.on_process("sass", |_, _| Ok(ok()));
        let mut plugin = StyleCompiler::default();
        let save = |plugin: &mut StyleCompiler, path: &str| {
            plugin.handle_events(&[Event::FileSaved(format!("/project/{path}"))], &host.invocation())
        };

        save(&mut plugin, "web/src/a.scss");
        save(&mut plugin, "web/src/a.scss");
        save(&mut plugin, "site/b.scss");
        assert_eq!(compiled(&host), ["/project/site/b.scss"], "the package without a bundler still compiles");
        let notes = host.notifications();
        assert_eq!(notes.len(), 1, "told once");
        assert!(notes[0].body.contains("with vite"));

        host.open_editor("/project/web/src/a.scss", "scss", ".a{}", &[(0, 0, 0, 0)]);
        plugin.run_command("compile-current", &host.invocation(), None).unwrap();
        assert_eq!(compiled(&host).len(), 2, "a command compiles in every mode");

        host.set_setting("mode", "\"always\"");
        save(&mut plugin, "web/src/a.scss");
        assert_eq!(compiled(&host).len(), 3);

        host.set_setting("mode", "\"off\"");
        save(&mut plugin, "site/b.scss");
        assert_eq!(compiled(&host).len(), 3);
    }

    #[test]
    fn compiles_every_entry_of_the_project() {
        let host = MockHost::install().with_capabilities(&["project.read", "events", "process:sass", "process:lessc"]);
        host.add_file("_vars.scss", "$c: red;\n");
        host.add_file("main.scss", "@use \"vars\";\n");
        host.add_file("broken.scss", "$");
        host.add_file("app.less", "@import \"part\";\n");
        host.add_file("part.less", "@c: red;\n");
        host.on_process("sass", |args, _| Ok(if args[0].ends_with("broken.scss") { failed(SYNTAX) } else { ok() }));
        host.on_process("lessc", |_, _| Ok(ok()));
        let message = StyleCompiler::default().run_command("compile-all", &host.invocation(), None).unwrap();
        assert_eq!(message.as_deref(), Some("Compiled 2 files; 1 failed, see the Problems view"));
        assert_eq!(compiled(&host), ["/project/app.less", "/project/broken.scss", "/project/main.scss"]);
    }
}
