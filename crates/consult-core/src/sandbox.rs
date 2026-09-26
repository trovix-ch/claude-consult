//! Read-only, root-jailed filesystem tools handed to the reviewer models.
//!
//! There is deliberately no write/edit/exec tool in this module. A reviewer cannot
//! modify the repo because nothing here can: read-only is a property of the code,
//! not of a permission setting someone has to remember to configure.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::util::{
    as_posix, char_len, clip_chars, expand_user, human_size, py_repr, py_str, resolve_lenient,
    splitlines,
};

/// Directories never worth showing a reviewer; they bury real signal in noise.
pub const SKIP_DIRS: [&str; 24] = [
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    "target",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".gradle",
    ".idea",
    ".vscode",
    "vendor",
    ".tox",
    ".cargo",
    ".terraform",
    "site-packages",
    ".claude-cache",
];

// Do NOT tighten these to save tokens — measured 2026-08-01, it backfires.
// Dropping reads 400->250 lines on a real review made the reviewer page instead:
// read_file calls 11 -> 21, total input 706k -> 777k, cost $0.687 -> $0.755.
// Smaller results do not shrink the context, they split it across more steps,
// and every extra step resends the whole conversation.
/// Lines per `read_file` call.
pub const MAX_READ_LINES: i64 = 400;
/// Characters per `read_file` result.
pub const MAX_READ_CHARS: usize = 40_000;
/// Matches per `glob` call.
pub const MAX_GLOB_HITS: usize = 300;
/// Matches per `grep` call.
pub const MAX_GREP_HITS: usize = 100;
/// Characters of git output.
pub const MAX_GIT_CHARS: usize = 30_000;
/// Files in the orientation tree.
pub const MAX_TREE_ENTRIES: usize = 400;
/// Files larger than this are not read.
pub const MAX_FILE_BYTES: u64 = 4_000_000;
/// A git command is killed after this.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(45);

// A read-only *subcommand* is not enough on its own: `git diff --output=FILE`
// writes to disk, and `--ext-diff`/`--textconv` execute commands the repo itself
// names via gitconfig or .gitattributes. Both the flags and the config that
// feeds them have to be shut off.
/// The git subcommands a reviewer may run.
pub const GIT_SUBCOMMANDS: [&str; 11] = [
    "diff",
    "log",
    "show",
    "status",
    "blame",
    "branch",
    "rev-parse",
    "shortlog",
    "describe",
    "ls-files",
    "tag",
];
/// Argument prefixes refused in any git call.
pub const GIT_BANNED_ARG_PREFIXES: [&str; 10] = [
    "--output",
    "-o",
    "--exec",
    "--upload-pack",
    "--receive-pack",
    "--ext-diff",
    "--textconv",
    "--pager",
    "--open-files-in-pager",
    "-c",
];
// Neutralise the config paths that turn a "read-only" git command into an
// arbitrary-execution primitive when the repo under review is untrusted.
/// Flags placed before every git subcommand.
pub const GIT_HARDENING: [&str; 10] = [
    "-c",
    "diff.external=",
    "-c",
    "core.pager=cat",
    "-c",
    "protocol.ext.allow=never",
    "-c",
    "core.fsmonitor=",
    "-c",
    "uploadpack.packObjectsHook=",
];

/// Anything the reviewer asked for but isn't allowed to have.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SandboxError(pub String);

fn err<T>(message: impl Into<String>) -> Result<T, SandboxError> {
    Err(SandboxError(message.into()))
}

fn skipped(name: &str) -> bool {
    SKIP_DIRS.contains(&name) || name.starts_with(".cache")
}

/// A read-only view of a single directory tree.
#[derive(Clone, Debug)]
pub struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    /// Opens `root` (with `~` expanded and symlinks resolved). Refuses anything that is
    /// not a directory, a filesystem root and the home directory.
    pub fn new(root: &str) -> Result<Self, SandboxError> {
        let root = resolve_lenient(&expand_user(root));
        if !root.is_dir() {
            return err(format!("root is not a directory: {}", root.display()));
        }
        // A filesystem root would expose the whole machine while still passing
        // every containment check below, so refuse it outright.
        let home = dirs::home_dir();
        let is_home = home
            .as_ref()
            .is_some_and(|h| *h == root || dunce::canonicalize(h).is_ok_and(|real| real == root));
        if root.parent().is_none() || is_home {
            return err(format!(
                "refusing to use {} as a review root; point it at a project directory",
                root.display()
            ));
        }
        Ok(Self { root })
    }

    /// The resolved review root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    // ---- path safety -----------------------------------------------------

    /// Resolves a reviewer-supplied path, refusing anything outside the root.
    fn resolve(&self, rel: &str) -> Result<PathBuf, SandboxError> {
        let raw = if rel.is_empty() { "." } else { rel };
        let raw = raw.trim().replace('\\', "/");
        if raw.contains('\0') {
            return err("null byte in path");
        }
        let candidate = PathBuf::from(&raw);
        let target = if candidate.is_absolute() {
            candidate
        } else {
            self.root.join(candidate)
        };
        // Resolving collapses .. and follows symlinks, so the containment check
        // below catches both traversal and symlink escapes in one step.
        let target = resolve_lenient(&target);
        if target != self.root && !target.starts_with(&self.root) {
            return err(format!("path escapes the review root: {rel}"));
        }
        Ok(target)
    }

    fn rel(&self, p: &Path) -> String {
        match p.strip_prefix(&self.root) {
            Ok(r) if r.as_os_str().is_empty() => ".".to_string(),
            Ok(r) => as_posix(r),
            Err(_) => as_posix(p),
        }
    }

    // ---- tools -----------------------------------------------------------

    /// Lists one directory: subdirectories first, then files with their sizes.
    pub fn list_dir(&self, path: &str) -> Result<String, SandboxError> {
        let target = self.resolve(path)?;
        if !target.is_dir() {
            return err(format!("not a directory: {}", self.rel(&target)));
        }
        let mut children: Vec<(bool, String, PathBuf)> = std::fs::read_dir(&target)
            .map_err(|e| SandboxError(format!("OSError: {e}")))?
            .filter_map(Result::ok)
            .map(|e| {
                let p = e.path();
                let name = e.file_name().to_string_lossy().to_string();
                (p.is_file(), name, p)
            })
            .collect();
        children.sort_by_key(|c| (c.0, c.1.to_lowercase()));
        let mut entries = Vec::new();
        for (_, name, p) in children {
            if p.is_dir() {
                if SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                entries.push(format!("{name}/"));
            } else {
                let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                entries.push(format!("{name}  ({})", human_size(size)));
            }
        }
        let rel = self.rel(&target);
        if entries.is_empty() {
            return Ok(format!("{rel}/ is empty"));
        }
        let body: Vec<String> = entries.iter().map(|e| format!("  {e}")).collect();
        Ok(format!("{rel}/\n{}", body.join("\n")))
    }

    /// Files whose relative path or bare name match `pattern` (fnmatch: `*` crosses `/`).
    pub fn glob(&self, pattern: &str, path: &str) -> Result<String, SandboxError> {
        let base = self.resolve(path)?;
        let pat = if pattern.is_empty() { "*" } else { pattern }.replace('\\', "/");
        let matcher = FnMatch::new(&pat);
        let mut hits = Vec::new();
        for f in walk_files(&self.root) {
            if base != self.root && !f.starts_with(&base) {
                continue;
            }
            let rel = self.rel(&f);
            let name = f
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if matcher.is_match(&rel) || matcher.is_match(&name) {
                hits.push(rel);
                if hits.len() >= MAX_GLOB_HITS {
                    break;
                }
            }
        }
        if hits.is_empty() {
            return Ok(format!(
                "no files matching {} under {}/",
                py_repr(pattern),
                self.rel(&base)
            ));
        }
        let mut out = hits.join("\n");
        if hits.len() >= MAX_GLOB_HITS {
            out.push_str(&format!(
                "\n... (truncated at {MAX_GLOB_HITS} matches; narrow the pattern)"
            ));
        }
        Ok(out)
    }

    /// Lines matching a regular expression, as `path:line: text`, with up to 5 lines of
    /// context.
    pub fn grep(
        &self,
        pattern: &str,
        path: &str,
        glob: &str,
        context: i64,
    ) -> Result<String, SandboxError> {
        let base = self.resolve(path)?;
        let rx = Regex::new(pattern)
            .map_err(|e| SandboxError(format!("bad regex {}: {e}", py_repr(pattern))))?;
        let context = context.clamp(0, 5) as usize;
        let gl = glob.replace('\\', "/");
        let filter = (!gl.is_empty()).then(|| FnMatch::new(&gl));

        let mut chunks: Vec<String> = Vec::new();
        let mut total = 0;
        'files: for f in walk_files(&self.root) {
            if base != self.root && !f.starts_with(&base) {
                continue;
            }
            let rel = self.rel(&f);
            if let Some(filter) = &filter {
                let name = f
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if !(filter.is_match(&rel) || filter.is_match(&name)) {
                    continue;
                }
            }
            let Some(text) = read_text(&f) else {
                continue;
            };
            let lines = splitlines(&text);
            for (i, line) in lines.iter().enumerate() {
                if !rx.is_match(line) {
                    continue;
                }
                let body = if context > 0 {
                    let lo = i.saturating_sub(context);
                    let hi = (i + context + 1).min(lines.len());
                    (lo..hi)
                        .map(|n| format!("{rel}:{}: {}", n + 1, lines[n]))
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    format!("{rel}:{}: {line}", i + 1)
                };
                chunks.push(clip_chars(&body, 2000).to_string());
                total += 1;
                if total >= MAX_GREP_HITS {
                    break 'files;
                }
            }
        }
        if chunks.is_empty() {
            return Ok(format!(
                "no matches for {} under {}/",
                py_repr(pattern),
                self.rel(&base)
            ));
        }
        let mut out = chunks.join(if context > 0 { "\n--\n" } else { "\n" });
        if total >= MAX_GREP_HITS {
            out.push_str(&format!(
                "\n... (truncated at {MAX_GREP_HITS} matches; narrow the pattern)"
            ));
        }
        Ok(out)
    }

    /// A window of a text file with line numbers, at most [`MAX_READ_LINES`] lines.
    pub fn read_file(&self, path: &str, offset: i64, limit: i64) -> Result<String, SandboxError> {
        let target = self.resolve(path)?;
        let rel = self.rel(&target);
        if target.is_dir() {
            return err(format!("{rel} is a directory; use list_dir"));
        }
        if !target.exists() {
            return err(format!("no such file: {rel}"));
        }
        let size = std::fs::metadata(&target)
            .map_err(|e| SandboxError(format!("OSError: {e}")))?
            .len();
        if size > MAX_FILE_BYTES {
            return Ok(format!(
                "{rel} is {}, over the {} read limit. Use grep to find the parts you need.",
                human_size(size),
                human_size(MAX_FILE_BYTES)
            ));
        }
        let Some(text) = read_text(&target) else {
            return Ok(format!(
                "{rel} is binary or undecodable ({})",
                human_size(size)
            ));
        };
        let lines = splitlines(&text);
        let offset = offset.max(1) as usize;
        let limit = limit.clamp(1, MAX_READ_LINES) as usize;
        let start = (offset - 1).min(lines.len());
        let end = (start + limit).min(lines.len());
        let window = &lines[start..end];
        if window.is_empty() {
            return Ok(format!(
                "{rel} has {} lines; offset {offset} is past the end",
                lines.len()
            ));
        }
        let body = window
            .iter()
            .enumerate()
            .map(|(i, ln)| format!("{:>6}\t{ln}", offset + i))
            .collect::<Vec<_>>()
            .join("\n");
        let body = clip_chars(&body, MAX_READ_CHARS);
        let header = format!(
            "{rel} (lines {offset}-{} of {})",
            offset + window.len() - 1,
            lines.len()
        );
        let read_to = offset - 1 + window.len();
        let more = if read_to < lines.len() {
            format!(
                "\n... {} more lines; re-read with offset={}",
                lines.len() - read_to,
                offset + window.len()
            )
        } else {
            String::new()
        };
        Ok(format!("{header}\n{body}{more}"))
    }

    /// Runs a read-only git subcommand in the root, hardened against repo-named commands.
    pub fn git(&self, subcommand: &str, args: &[String]) -> Result<String, SandboxError> {
        let sub = subcommand.trim();
        if !GIT_SUBCOMMANDS.contains(&sub) {
            let mut allowed = GIT_SUBCOMMANDS.to_vec();
            allowed.sort_unstable();
            return err(format!(
                "git {} is not permitted; read-only subcommands are: {}",
                py_repr(sub),
                allowed.join(", ")
            ));
        }
        let argv: Vec<&String> = args.iter().take(20).collect();
        for a in &argv {
            let low = a.to_lowercase();
            if GIT_BANNED_ARG_PREFIXES.iter().any(|p| low.starts_with(p))
                || low.starts_with("ext::")
            {
                return err(format!("argument not permitted in read-only git: {a}"));
            }
        }
        let mut cmd = Command::new("git");
        cmd.args(GIT_HARDENING)
            .arg("--no-pager")
            .arg(sub)
            .args(argv)
            .current_dir(&self.root);
        let (stdout, stderr) = run_with_timeout(cmd, GIT_TIMEOUT)?;
        let mut out = stdout;
        if !stderr.trim().is_empty() {
            out.push_str("\n[stderr] ");
            out.push_str(&stderr);
        }
        let out = out.trim();
        let out = if out.is_empty() { "(no output)" } else { out };
        if char_len(out) > MAX_GIT_CHARS {
            return Ok(format!(
                "{}\n... (truncated at {MAX_GIT_CHARS} chars)",
                clip_chars(out, MAX_GIT_CHARS)
            ));
        }
        Ok(out.to_string())
    }

    // ---- orientation (built once, not model-driven) ----------------------

    /// The directory layout to `max_depth`, at most 40 files per directory and
    /// [`MAX_TREE_ENTRIES`] in all.
    pub fn tree(&self, max_depth: usize) -> String {
        let mut lines = Vec::new();
        let mut count = 0;
        // Depth-first, a directory's own files before its subdirectories, as os.walk.
        let mut stack: Vec<(PathBuf, usize)> = vec![(self.root.clone(), 0)];
        while let Some((dir, depth)) = stack.pop() {
            let Some((mut dirs, files)) = scan(&dir) else {
                continue;
            };
            if depth >= max_depth {
                dirs.clear();
            } else {
                dirs.retain(|(name, _)| !skipped(name));
            }
            let indent = "  ".repeat(depth);
            if depth > 0 {
                let name = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                lines.push(format!("{indent}{name}/"));
            }
            for name in files.iter().take(40) {
                lines.push(format!("{indent}  {name}"));
                count += 1;
                if count >= MAX_TREE_ENTRIES {
                    lines.push("  ... (tree truncated)".to_string());
                    return lines.join("\n");
                }
            }
            for (name, link) in dirs.into_iter().rev() {
                if !link {
                    stack.push((dir.join(name), depth + 1));
                }
            }
        }
        if lines.is_empty() {
            "(empty directory)".to_string()
        } else {
            lines.join("\n")
        }
    }

    /// Whether the root holds a `.git`.
    pub fn is_git_repo(&self) -> bool {
        self.root.join(".git").exists()
    }
}

/// A subdirectory's name, and whether it is a symlink.
type SubDir = (String, bool);

/// One directory's subdirectories and file names, each sorted; `None` when it cannot be
/// read. A symlink to a directory counts as a directory that is never entered, as
/// os.walk does without followlinks.
fn scan(dir: &Path) -> Option<(Vec<SubDir>, Vec<String>)> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        let link = entry.file_type().is_ok_and(|t| t.is_symlink());
        if path.is_dir() {
            dirs.push((name, link));
        } else {
            files.push(name);
        }
    }
    dirs.sort();
    files.sort();
    Some((dirs, files))
}

/// Every file under `root`, pruning noise directories, in os.walk's top-down order.
fn walk_files(root: &Path) -> impl Iterator<Item = PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    let mut pending: std::collections::VecDeque<PathBuf> = std::collections::VecDeque::new();
    std::iter::from_fn(move || {
        loop {
            if let Some(f) = pending.pop_front() {
                return Some(f);
            }
            let dir = stack.pop()?;
            let Some((dirs, files)) = scan(&dir) else {
                continue;
            };
            pending.extend(files.into_iter().map(|f| dir.join(f)));
            for (name, link) in dirs.into_iter().rev() {
                if !link && !skipped(&name) {
                    stack.push(dir.join(name));
                }
            }
        }
    })
}

/// A file as text: `None` when too big, unreadable or binary; UTF-8, else Latin-1.
fn read_text(p: &Path) -> Option<String> {
    if std::fs::metadata(p).ok()?.len() > MAX_FILE_BYTES {
        return None;
    }
    let raw = std::fs::read(p).ok()?;
    if raw[..raw.len().min(8000)].contains(&0) {
        return None;
    }
    match String::from_utf8(raw) {
        Ok(s) => Some(s),
        Err(e) => Some(e.into_bytes().iter().map(|&b| b as char).collect()),
    }
}

fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<(String, String), SandboxError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: the shared service has no console, and git would open one.
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            SandboxError("git is not installed".into())
        } else {
            SandboxError(format!("OSError: {e}"))
        }
    })?;
    let reader = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = reader(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let errs = reader(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return err("git command timed out");
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return err(format!("OSError: {e}")),
        }
    }
    let decode = |bytes: Vec<u8>| {
        // Text mode, as subprocess's: universal newlines, undecodable bytes replaced.
        String::from_utf8_lossy(&bytes)
            .replace("\r\n", "\n")
            .replace('\r', "\n")
    };
    let stdout = decode(out.join().unwrap_or_default());
    let stderr = decode(errs.join().unwrap_or_default());
    Ok((stdout, stderr))
}

/// A pattern with Python `fnmatch` semantics: `*` matches anything including `/`, `?` one
/// character, `[...]` a set. Case-insensitive on Windows, as `os.path.normcase` makes it.
#[derive(Clone, Debug)]
pub struct FnMatch(Option<Regex>);

impl FnMatch {
    /// Compiles a pattern.
    pub fn new(pattern: &str) -> Self {
        let flags = if cfg!(windows) { "(?si)" } else { "(?s)" };
        Self(Regex::new(&format!("{flags}^(?:{})$", translate(pattern))).ok())
    }

    /// Whether the whole name matches.
    pub fn is_match(&self, name: &str) -> bool {
        self.0.as_ref().is_some_and(|r| r.is_match(name))
    }
}

/// Python's `fnmatch.translate`, into Rust regex syntax.
fn translate(pat: &str) -> String {
    let chars: Vec<char> = pat.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut res = String::new();
    let mut last_star = false;
    while i < n {
        let c = chars[i];
        i += 1;
        if c == '*' {
            if !last_star {
                res.push_str(".*");
            }
            last_star = true;
            continue;
        }
        last_star = false;
        match c {
            '?' => res.push('.'),
            '[' => {
                let mut j = i;
                if j < n && chars[j] == '!' {
                    j += 1;
                }
                if j < n && chars[j] == ']' {
                    j += 1;
                }
                while j < n && chars[j] != ']' {
                    j += 1;
                }
                if j >= n {
                    res.push_str("\\[");
                    continue;
                }
                let stuff: Vec<char> = chars[i..j].to_vec();
                i = j + 1;
                if stuff.is_empty() {
                    // Matches nothing, as Python's "(?!)".
                    res.push_str("[^\\s\\S]");
                } else if stuff == ['!'] {
                    res.push('.');
                } else {
                    let (negate, body) = if stuff[0] == '!' {
                        (true, &stuff[1..])
                    } else {
                        (false, &stuff[..])
                    };
                    res.push('[');
                    if negate {
                        res.push('^');
                    }
                    for (k, &ch) in body.iter().enumerate() {
                        let special = matches!(ch, '\\' | '[' | ']' | '&' | '~' | '|')
                            || (ch == '^' && k == 0 && !negate);
                        if special {
                            res.push('\\');
                        }
                        res.push(ch);
                    }
                    res.push(']');
                }
            }
            other => res.push_str(&regex::escape(&other.to_string())),
        }
    }
    res
}

// ---- OpenAI-format tool schemas advertised to the reviewer models ---------

/// The tool schemas sent with every grounded request.
pub fn tool_schemas() -> &'static Value {
    static SCHEMAS: OnceLock<Value> = OnceLock::new();
    SCHEMAS.get_or_init(|| {
        json!([
            {
                "type": "function",
                "function": {
                    "name": "list_dir",
                    "description": "List the contents of a directory in the project. Use this to orient yourself before reading files.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "Directory relative to the project root. Defaults to the root."}
                        },
                    },
                },
            },
            {
                "type": "function",
                "function": {
                    "name": "glob",
                    "description": "Find files by name pattern, e.g. '**/*.py' or 'test_*.js'. Returns paths relative to the project root.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "pattern": {"type": "string", "description": "Glob pattern matched against the relative path and the bare filename."},
                            "path": {"type": "string", "description": "Optional subdirectory to search under."},
                        },
                        "required": ["pattern"],
                    },
                },
            },
            {
                "type": "function",
                "function": {
                    "name": "grep",
                    "description": "Search file contents with a regular expression (Rust regex syntax). Returns 'path:line: text' hits.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "pattern": {"type": "string", "description": "Regular expression (Rust regex syntax)."},
                            "path": {"type": "string", "description": "Optional subdirectory to search under."},
                            "glob": {"type": "string", "description": "Optional filename filter, e.g. '*.rs'."},
                            "context": {"type": "integer", "description": "Lines of surrounding context, 0-5. Default 0."},
                        },
                        "required": ["pattern"],
                    },
                },
            },
            {
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": format!("Read a text file with line numbers. Returns at most {MAX_READ_LINES} lines per call; page through longer files with offset."),
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "File relative to the project root."},
                            "offset": {"type": "integer", "description": "1-based first line to read. Default 1."},
                            "limit": {"type": "integer", "description": format!("How many lines to read, max {MAX_READ_LINES}.")},
                        },
                        "required": ["path"],
                    },
                },
            },
            {
                "type": "function",
                "function": {
                    "name": "git",
                    "description": "Run a read-only git command in the project (diff, log, show, status, blame, branch, rev-parse, shortlog, describe, ls-files, tag). Useful for reviewing recent or uncommitted changes.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "subcommand": {"type": "string", "description": "One of the permitted read-only subcommands."},
                            "args": {"type": "array", "items": {"type": "string"}, "description": "Arguments, e.g. ['-n','10','--stat']."},
                        },
                        "required": ["subcommand"],
                    },
                },
            },
        ])
    })
}

/// Strips terminal escapes and control bytes from anything leaving the sandbox.
///
/// Content from the repo under review — commit messages especially — reaches the
/// reviewer verbatim, so a crafted commit must not rewrite what the reviewer sees.
pub fn sanitize(text: &str) -> String {
    static ANSI: OnceLock<Regex> = OnceLock::new();
    static CTRL: OnceLock<Regex> = OnceLock::new();
    let ansi = ANSI.get_or_init(|| {
        Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b[@-Z\\-_]").expect("static regex")
    });
    let ctrl =
        CTRL.get_or_init(|| Regex::new(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]").expect("static regex"));
    let once = ansi.replace_all(text, "");
    ctrl.replace_all(&once, "").into_owned()
}

/// Executes one reviewer tool call, returning text. Never fails: a refusal or error is
/// returned as `ERROR: ...` for the model to read.
pub fn dispatch(sandbox: &Sandbox, name: &str, args: &Map<String, Value>) -> String {
    let out = match run_tool(sandbox, name, args) {
        Ok(text) => text,
        Err(ToolError::Sandbox(e)) => format!("ERROR: {e}"),
        // A tool failure must not kill the review.
        Err(ToolError::Arg(e)) => format!("ERROR: {e}"),
        Err(ToolError::Unknown) => format!("ERROR: unknown tool {}", py_repr(name)),
    };
    sanitize(&out)
}

enum ToolError {
    Sandbox(SandboxError),
    Arg(String),
    Unknown,
}

impl From<SandboxError> for ToolError {
    fn from(e: SandboxError) -> Self {
        Self::Sandbox(e)
    }
}

/// `args.get(key)`, falsy meaning the default, as `args.get(key) or default`.
fn text_arg(args: &Map<String, Value>, key: &str, default: &str) -> Result<String, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default.to_string()),
        Some(Value::String(s)) if s.is_empty() => Ok(default.to_string()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(other) => Err(ToolError::Arg(format!(
            "TypeError: {key} must be a string, not {}",
            py_repr(&py_str(other))
        ))),
    }
}

/// Python's `int(args.get(key) or default)`.
fn int_arg(args: &Map<String, Value>, key: &str, default: i64) -> Result<i64, ToolError> {
    let v = match args.get(key) {
        None | Some(Value::Null) | Some(Value::Bool(false)) => return Ok(default),
        Some(v) => v,
    };
    match v {
        Value::Bool(_) => Ok(1),
        Value::Number(n) => {
            let x = n
                .as_i64()
                .or_else(|| {
                    n.as_f64()
                        .filter(|x| x.is_finite())
                        .map(|x| x.trunc() as i64)
                })
                .unwrap_or(default);
            Ok(if x == 0 { default } else { x })
        }
        Value::String(s) if s.is_empty() => Ok(default),
        Value::String(s) => s.trim().parse::<i64>().map_err(|_| {
            ToolError::Arg(format!(
                "ValueError: invalid literal for int() with base 10: {}",
                py_repr(s)
            ))
        }),
        other => Err(ToolError::Arg(format!(
            "TypeError: int() argument must be a string, a bytes-like object or a real number, not {}",
            py_repr(&py_str(other))
        ))),
    }
}

fn run_tool(sandbox: &Sandbox, name: &str, args: &Map<String, Value>) -> Result<String, ToolError> {
    Ok(match name {
        "list_dir" => sandbox.list_dir(&text_arg(args, "path", ".")?)?,
        "glob" => sandbox.glob(
            &text_arg(args, "pattern", "*")?,
            &text_arg(args, "path", ".")?,
        )?,
        "grep" => sandbox.grep(
            &text_arg(args, "pattern", "")?,
            &text_arg(args, "path", ".")?,
            &text_arg(args, "glob", "")?,
            int_arg(args, "context", 0)?,
        )?,
        "read_file" => sandbox.read_file(
            &text_arg(args, "path", "")?,
            int_arg(args, "offset", 1)?,
            int_arg(args, "limit", MAX_READ_LINES)?,
        )?,
        "git" => {
            let argv: Vec<String> = match args.get("args") {
                Some(Value::Array(items)) => items.iter().map(py_str).collect(),
                Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
                _ => Vec::new(),
            };
            sandbox.git(&text_arg(args, "subcommand", "")?, &argv)?
        }
        _ => return Err(ToolError::Unknown),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> (tempfile::TempDir, Sandbox) {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("project");
        std::fs::create_dir_all(root.join("src/deep")).expect("mkdir");
        std::fs::create_dir_all(root.join("node_modules/pkg")).expect("mkdir");
        std::fs::write(root.join("a.txt"), "hello\nworld\n").expect("write");
        std::fs::write(root.join("src/main.py"), "import os\nprint('x')\n").expect("write");
        std::fs::write(root.join("src/deep/Mod.PY"), "x = 1\n").expect("write");
        std::fs::write(root.join("node_modules/pkg/index.py"), "hidden\n").expect("write");
        std::fs::write(root.join("bin.dat"), b"\x00\x01binary").expect("write");
        let sb = Sandbox::new(&root.to_string_lossy()).expect("sandbox");
        (tmp, sb)
    }

    fn call(sb: &Sandbox, name: &str, args: Value) -> String {
        dispatch(sb, name, args.as_object().expect("object"))
    }

    #[test]
    fn roots_that_are_refused() {
        let tmp = tempfile::tempdir().expect("tmp");
        let missing = tmp.path().join("missing");
        let e = Sandbox::new(&missing.to_string_lossy()).expect_err("refused");
        assert!(e.0.starts_with("root is not a directory: "), "{e}");
        let fs_root = if cfg!(windows) { "C:\\" } else { "/" };
        assert!(
            Sandbox::new(fs_root)
                .expect_err("refused")
                .0
                .starts_with("refusing to use ")
        );
        assert!(
            Sandbox::new("~")
                .expect_err("refused")
                .0
                .starts_with("refusing to use ")
        );
    }

    #[test]
    fn paths_cannot_escape() {
        let (tmp, sb) = project();
        std::fs::write(tmp.path().join("secret.txt"), "s").expect("write");
        for p in ["../secret.txt", "src/../../secret.txt", "..\\secret.txt"] {
            let out = call(&sb, "read_file", json!({"path": p}));
            assert!(
                out.starts_with("ERROR: path escapes the review root: "),
                "{p}: {out}"
            );
        }
        let abs = tmp.path().join("secret.txt");
        let out = call(&sb, "read_file", json!({"path": abs.to_string_lossy()}));
        assert!(out.starts_with("ERROR: path escapes"), "{out}");
        let out = call(&sb, "read_file", json!({"path": "a\u{0}b"}));
        assert_eq!(out, "ERROR: null byte in path");
    }

    #[test]
    fn list_dir_puts_directories_first_and_skips_noise() {
        let (_tmp, sb) = project();
        let out = call(&sb, "list_dir", json!({}));
        assert_eq!(out, "./\n  src/\n  a.txt  (12B)\n  bin.dat  (8B)");
        assert_eq!(
            call(&sb, "list_dir", json!({"path": "a.txt"})),
            "ERROR: not a directory: a.txt"
        );
    }

    #[test]
    fn glob_is_fnmatch() {
        let (_tmp, sb) = project();
        let out = call(&sb, "glob", json!({"pattern": "*.py"}));
        if cfg!(windows) {
            assert_eq!(out, "src/main.py\nsrc/deep/Mod.PY");
        } else {
            assert_eq!(out, "src/main.py");
        }
        // `*` crosses `/`, and a top-level file needs no slash to match a bare name.
        assert_eq!(
            call(&sb, "glob", json!({"pattern": "src/*.txt"})),
            "no files matching 'src/*.txt' under ./"
        );
        assert_eq!(call(&sb, "glob", json!({"pattern": "a.t?t"})), "a.txt");
        assert_eq!(
            call(&sb, "glob", json!({"pattern": "*", "path": "src/deep"})),
            "src/deep/Mod.PY"
        );
        assert!(!call(&sb, "glob", json!({"pattern": "index.py"})).contains("node_modules"));
    }

    #[test]
    fn fnmatch_translation() {
        assert!(FnMatch::new("[!a]b").is_match("xb"));
        assert!(!FnMatch::new("[!a]b").is_match("ab"));
        assert!(FnMatch::new("[a-c]").is_match("b"));
        assert!(FnMatch::new("x[").is_match("x["));
        assert!(FnMatch::new("**/*.rs").is_match("a/b.rs"));
        assert!(!FnMatch::new("**/*.rs").is_match("b.rs"));
        assert!(FnMatch::new("a.(b)+").is_match("a.(b)+"));
    }

    #[test]
    fn grep_hits_and_context() {
        let (_tmp, sb) = project();
        assert_eq!(
            call(&sb, "grep", json!({"pattern": "wor"})),
            "a.txt:2: world"
        );
        assert_eq!(
            call(&sb, "grep", json!({"pattern": "print", "context": 1})),
            "src/main.py:1: import os\nsrc/main.py:2: print('x')"
        );
        assert_eq!(
            call(&sb, "grep", json!({"pattern": "x", "glob": "*.txt"})),
            "no matches for 'x' under ./"
        );
        assert!(call(&sb, "grep", json!({"pattern": "("})).starts_with("ERROR: bad regex '(': "));
        assert!(
            call(&sb, "grep", json!({"pattern": "o", "context": "x"}))
                .starts_with("ERROR: ValueError")
        );
    }

    #[test]
    fn read_file_windows() {
        let (_tmp, sb) = project();
        assert_eq!(
            call(&sb, "read_file", json!({"path": "a.txt"})),
            "a.txt (lines 1-2 of 2)\n     1\thello\n     2\tworld"
        );
        assert_eq!(
            call(
                &sb,
                "read_file",
                json!({"path": "a.txt", "offset": 1, "limit": 1})
            ),
            "a.txt (lines 1-1 of 2)\n     1\thello\n... 1 more lines; re-read with offset=2"
        );
        assert_eq!(
            call(&sb, "read_file", json!({"path": "a.txt", "offset": 9})),
            "a.txt has 2 lines; offset 9 is past the end"
        );
        assert_eq!(
            call(&sb, "read_file", json!({"path": "src"})),
            "ERROR: src is a directory; use list_dir"
        );
        assert_eq!(
            call(&sb, "read_file", json!({"path": "nope"})),
            "ERROR: no such file: nope"
        );
        assert_eq!(
            call(&sb, "read_file", json!({"path": "bin.dat"})),
            "bin.dat is binary or undecodable (8B)"
        );
    }

    #[test]
    fn git_refuses_what_could_write_or_execute() {
        let (_tmp, sb) = project();
        let out = call(&sb, "git", json!({"subcommand": "push"}));
        assert!(
            out.starts_with(
                "ERROR: git 'push' is not permitted; read-only subcommands are: blame, branch,"
            ),
            "{out}"
        );
        for bad in ["--output=x", "-O", "--ext-diff", "-c", "ext::sh"] {
            let out = call(&sb, "git", json!({"subcommand": "diff", "args": [bad]}));
            assert_eq!(
                out,
                format!("ERROR: argument not permitted in read-only git: {bad}")
            );
        }
    }

    #[test]
    fn tree_and_sanitize() {
        let (_tmp, sb) = project();
        assert_eq!(
            sb.tree(3),
            "  a.txt\n  bin.dat\n  src/\n    main.py\n    deep/\n      Mod.PY"
        );
        assert_eq!(sb.tree(1), "  a.txt\n  bin.dat\n  src/\n    main.py");
        assert!(!sb.is_git_repo());
        assert_eq!(sanitize("a\x1b[31mred\x1b[0m\x07b\tc\n"), "aredb\tc\n");
        assert_eq!(call(&sb, "nope", json!({})), "ERROR: unknown tool 'nope'");
    }

    #[test]
    fn schemas_name_every_tool() {
        let names: Vec<&str> = tool_schemas()
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect();
        assert_eq!(names, ["list_dir", "glob", "grep", "read_file", "git"]);
    }
}
