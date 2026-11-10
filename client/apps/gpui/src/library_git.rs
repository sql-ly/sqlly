//! Git for the SQL library: status list, diff preview, and commit for the
//! library folder (or any folder the library points at, e.g. a project
//! folder). Deliberately scoped to status/diff/commit — no branching,
//! merging, push, or pull.
//!
//! Every git invocation goes through the [`GitRunner`] trait so the porcelain
//! parsing and command construction are unit-tested without a git binary or a
//! real repository, and so the only code path that can MUTATE a repository
//! (`init`, `stage`, `commit`) is exercised with a recording fake. The UI
//! calls those verbs exclusively from button presses — never from render,
//! scanning, or refresh.
//!
//! Desktop-only (git is a subprocess; the library lives on the filesystem).

#![cfg(not(target_arch = "wasm32"))]

use sqlly_core::DisplayMessage;
use std::path::{Path, PathBuf};

/// One `git status --porcelain=v1` line, decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitChange {
    /// Repo-relative path (forward slashes), unquoted. For a rename this is
    /// the NEW path.
    pub path: String,
    /// Rename source, when the entry is a rename/copy.
    pub orig_path: Option<String>,
    /// Index (staged) status letter: M, A, D, R, C, U, or ' '.
    pub x: char,
    /// Worktree (unstaged) status letter: M, D, U, or ' '. '?' for untracked.
    pub y: char,
}

impl GitChange {
    /// Untracked (`??`) entries.
    pub fn is_untracked(&self) -> bool {
        self.x == '?' && self.y == '?'
    }

    /// Anything staged for the next commit.
    pub fn is_staged(&self) -> bool {
        !self.is_untracked() && self.x != ' '
    }

    /// Compact status shown left of the path: the staged letter when staged,
    /// else the worktree letter, else `??`.
    pub fn badge(&self) -> String {
        if self.is_untracked() {
            return "??".to_string();
        }
        match (self.x, self.y) {
            (x, y) if x != ' ' && y != ' ' => format!("{x}{y}"),
            (x, _) if x != ' ' => format!("{x} "),
            (_, y) => format!(" {y}"),
        }
    }

    /// "new name (from old name)" for renames, else the path.
    pub fn display_path(&self) -> String {
        match &self.orig_path {
            Some(orig) => format!("{} (from {})", self.path, orig),
            None => self.path.clone(),
        }
    }
}

/// Parse `git status --porcelain=v1 -z` output. Entries are NUL-terminated
/// and paths are raw (no C-quoting), so file names holding spaces, quotes,
/// newlines, or a literal ` -> ` round-trip exactly. A rename/copy entry is
/// followed by a second NUL-terminated field holding the source path. Every
/// XY code git emits passes through verbatim (the UI renders the letters
/// as-is). Malformed entries are skipped, never fatal: status output changes
/// between git versions and a half-parsed list must not take the panel down.
pub fn parse_porcelain(text: &str) -> Vec<GitChange> {
    let mut changes = Vec::new();
    let mut fields = text.split('\0');
    while let Some(entry) = fields.next() {
        let mut chars = entry.chars();
        let (Some(x), Some(y)) = (chars.next(), chars.next()) else {
            continue;
        };
        let is_rename = matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C');
        // The source path is its own field; consume it even when this entry
        // turns out malformed so the next entry still parses from its start.
        let orig_path = if is_rename {
            fields.next().map(str::to_string)
        } else {
            None
        };
        // Position 2 is the separator space.
        if entry.get(2..3) != Some(" ") {
            continue;
        }
        let Some(path) = entry.get(3..).filter(|p| !p.is_empty()) else {
            continue;
        };
        if is_rename && orig_path.as_deref().is_none_or(str::is_empty) {
            continue;
        }
        changes.push(GitChange {
            path: path.to_string(),
            orig_path,
            x,
            y,
        });
    }
    changes
}

/// A porcelain path (always relative to the repository root, whatever the
/// working directory) as a pathspec git resolves the same way: `top` anchors
/// it at the repo root, `literal` stops `*`/`?`/`[` in a file name from
/// globbing. Needed because the panel's folder is often a subfolder of a
/// larger repository (a project folder, or a library kept inside one), where
/// a bare path would be resolved against the subfolder instead.
pub fn repo_pathspec(path: &str) -> String {
    format!(":(top,literal){path}")
}

/// `status --porcelain=v1 -z`: stable, script-facing output with raw
/// NUL-terminated paths (no quoting, no ` -> ` ambiguity). `-uall` lists each
/// untracked file (not a collapsed `dir/`, which could not be diffed or
/// committed on its own) and never ignored ones. The `.` pathspec scopes the list
/// to the panel's folder when it sits inside a larger repository.
pub fn status_args() -> Vec<String> {
    vec![
        "status".into(),
        "--porcelain=v1".into(),
        "-z".into(),
        "-uall".into(),
        "--".into(),
        ".".into(),
    ]
}

/// Unstaged + staged diff for one path (`HEAD` covers both; a new repo
/// without commits falls back to the unstaged diff by the caller).
/// `--no-color`: a user-side `color.ui=always` must not leak ANSI escapes
/// into the read-only preview.
pub fn diff_args(path: &str) -> Vec<String> {
    vec![
        "diff".into(),
        "--no-color".into(),
        "HEAD".into(),
        "--".into(),
        repo_pathspec(path),
    ]
}

/// Diff for a path that has no HEAD yet (fresh repo, no commits).
pub fn diff_args_no_head(path: &str) -> Vec<String> {
    vec![
        "diff".into(),
        "--no-color".into(),
        "--".into(),
        repo_pathspec(path),
    ]
}

/// Stage everything under the panel's folder (`None`) or the named
/// repo-relative paths (`Some`).
pub fn stage_args(paths: Option<&[String]>) -> Vec<String> {
    match paths {
        None => vec!["add".into(), "-A".into(), "--".into(), ".".into()],
        Some(paths) => {
            let mut args = vec!["add".into(), "--".into()];
            args.extend(paths.iter().map(|p| repo_pathspec(p)));
            args
        }
    }
}

/// Verify + cap the commit message. A blank message is refused here, before
/// git ever runs.
fn validated_message(message: &str) -> Result<String, DisplayMessage> {
    let message = message.trim();
    if message.is_empty() {
        return Err(crate::localization::deferred_text(
            "explorer.git_panel.errors.commit_message_blank",
        ));
    }
    Ok(message.chars().take(500).collect())
}

/// Stage-0 entries of the real index under the commit's scope.
/// `--full-name` keeps paths repo-root-relative no matter which folder the
/// panel hosts; `-z` keeps odd file names raw.
pub fn ls_files_args(paths: &[String]) -> Vec<String> {
    let mut args = vec![
        "ls-files".into(),
        "--full-name".into(),
        "-s".into(),
        "-z".into(),
        "--".into(),
    ];
    args.extend(paths.iter().cloned());
    args
}

/// Paths HEAD holds under the same scope, used to tell staged deletions
/// apart from untouched entries.
pub fn head_paths_args(paths: &[String]) -> Vec<String> {
    let mut args = vec![
        "ls-tree".into(),
        "-r".into(),
        "--name-only".into(),
        "-z".into(),
        "--full-name".into(),
        "HEAD".into(),
        "--".into(),
    ];
    args.extend(paths.iter().cloned());
    args
}

/// Insert staged blobs into the temporary index, one `--cacheinfo` per
/// entry; paths must already be repo-root-relative. Run from the repository
/// top level (see [`removal_args`]).
pub fn cacheinfo_args(entries: &[(u32, String, String)]) -> Vec<String> {
    let mut args = vec!["update-index".into(), "--add".into()];
    for (mode, sha, path) in entries {
        args.push("--cacheinfo".into());
        args.push(format!("{mode:o},{sha},{path}"));
    }
    args
}

/// Drop staged deletions from the temporary index. `update-index` takes plain
/// file paths, not pathspecs, and resolves them against the working
/// directory, so the repo-root-relative paths `ls-tree --full-name` returns
/// only name the right entries when git runs from the repository top level.
/// Run from a subfolder, `proj/gone.sql` would resolve to
/// `proj/proj/gone.sql`, match nothing, and still exit 0 — silently leaving
/// the deletion out of the commit.
pub fn removal_args(paths: &[String]) -> Vec<String> {
    let mut args = vec!["update-index".into(), "--force-remove".into(), "--".into()];
    args.extend(paths.iter().cloned());
    args
}

fn rev_parse_head_args() -> Vec<String> {
    vec![
        "rev-parse".into(),
        "--verify".into(),
        "--quiet".into(),
        "HEAD".into(),
    ]
}

/// The repository's top-level directory: where root-relative paths resolve.
fn show_toplevel_args() -> Vec<String> {
    vec!["rev-parse".into(), "--show-toplevel".into()]
}

fn read_tree_head_args() -> Vec<String> {
    vec!["read-tree".into(), "HEAD".into()]
}

fn commit_index_args(message: &str) -> Vec<String> {
    vec!["commit".into(), "-m".into(), message.to_string()]
}

/// Re-point the real index's entries for the committed scope at the new HEAD
/// so the status list reflects the commit (other staged work stays staged).
fn reset_sync_args(paths: &[String]) -> Vec<String> {
    let mut args = vec!["reset".into(), "-q".into(), "--".into()];
    args.extend(paths.iter().cloned());
    args
}

/// `<mode> <sha> <stage>\t<path>` records, as `ls-files -s -z` prints them.
/// Non-zero stages (unmerged paths) are skipped so a conflicted file cannot
/// sneak into a commit.
fn parse_ls_files(text: &str) -> Vec<(u32, String, String)> {
    text.split('\0')
        .filter(|record| !record.is_empty())
        .filter_map(|record| {
            let (meta, path) = record.split_once('\t')?;
            let mut parts = meta.split_whitespace();
            let mode = u32::from_str_radix(parts.next()?, 8).ok()?;
            let sha = parts.next()?.to_string();
            let stage: u32 = parts.next()?.parse().ok()?;
            if stage != 0 {
                return None;
            }
            Some((mode, sha, path.to_string()))
        })
        .collect()
}

/// Raw NUL-separated paths, as `ls-tree --name-only -z` prints them.
fn parse_raw_paths(text: &str) -> Vec<String> {
    text.split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// A scratch index file a commit is built in. Removed on drop, so a failed
/// commit leaves nothing behind and the real index is only ever touched by
/// the explicit `stage` call and the final sync. It lives in the owner-only
/// `sqlly-git` temp directory (the index lists every staged path and blob id)
/// under an unguessable name.
struct TempIndexFile(PathBuf);

impl TempIndexFile {
    fn new() -> Result<Self, DisplayMessage> {
        let dir = crate::fs_safe::private_temp_dir("git").map_err(|error| {
            crate::localization::deferred(
                "explorer.git_panel.errors.temporary_index_failed",
                &[("detail", error)],
            )
        })?;
        Ok(Self(dir.join(format!("index-{}", uuid::Uuid::new_v4()))))
    }

    fn env(&self) -> [(&'static str, String); 1] {
        [("GIT_INDEX_FILE", self.0.to_string_lossy().into_owned())]
    }
}

impl Drop for TempIndexFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn init_args() -> Vec<String> {
    vec!["init".into()]
}

/// What probing a folder found: a work tree, a plain folder, or no usable
/// git binary at all (so the panel says "install git" instead of offering an
/// init that cannot run).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepoProbe {
    Repo,
    NotRepo,
    GitUnavailable(DisplayMessage),
}

/// Why a git subprocess could not run at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitRunError {
    /// The Mac App Store sandbox forbids launching installed programs.
    Sandboxed,
    /// Spawning `git` failed; carries the operating-system error text.
    Spawn(String),
}

/// Keyed (thread-safe) description; git runs on a background executor and the
/// panel resolves the message on the UI thread.
impl From<GitRunError> for DisplayMessage {
    fn from(error: GitRunError) -> Self {
        match error {
            GitRunError::Sandboxed => {
                crate::localization::deferred_text("explorer.git_panel.errors.sandbox_blocked")
            }
            GitRunError::Spawn(detail) => crate::localization::deferred(
                "explorer.git_panel.errors.run_failed",
                &[("detail", detail)],
            ),
        }
    }
}

/// Whether `dir` sits inside a git work tree.
pub fn rev_parse_args() -> Vec<String> {
    vec!["rev-parse".into(), "--is-inside-work-tree".into()]
}

/// What a git subprocess produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// The seam every git invocation goes through. Production uses the git CLI;
/// tests inject canned answers and record the argv they were asked to run.
pub trait GitRunner {
    /// Run git with extra environment overrides (used for `GIT_INDEX_FILE`
    /// while a commit is built in a temporary index).
    fn run_with_env(
        &self,
        dir: &Path,
        args: &[String],
        env: &[(&'static str, String)],
    ) -> Result<GitOutput, GitRunError>;

    fn run(&self, dir: &Path, args: &[String]) -> Result<GitOutput, GitRunError> {
        self.run_with_env(dir, args, &[])
    }
}

/// Runs the real `git` binary with `-C <dir>` so paths stay repo-relative.
pub struct CliGitRunner;

impl GitRunner for CliGitRunner {
    fn run_with_env(
        &self,
        dir: &Path,
        args: &[String],
        env: &[(&'static str, String)],
    ) -> Result<GitOutput, GitRunError> {
        if !crate::distribution::can_launch_user_programs() {
            return Err(GitRunError::Sandboxed);
        }
        let mut cmd = std::process::Command::new("git");
        cmd.arg("-C")
            .arg(dir)
            .args(args)
            .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
            // Never block the UI on an interactive prompt (credentials,
            // editor, pager): fail instead and surface git's stderr.
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_EDITOR", "true")
            .env("GIT_PAGER", "cat")
            .stdin(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            // CREATE_NO_WINDOW: no console flash per git call.
            cmd.creation_flags(0x0800_0000);
        }
        let output = cmd
            .output()
            .map_err(|error| GitRunError::Spawn(error.to_string()))?;
        Ok(GitOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// The panel's git session over one folder. Read verbs (`status`, `diff`,
/// `probe`) are safe to call any time; `init`, `stage`, and `commit` must
/// only ever run from an explicit user action.
pub struct LibraryGit<R: GitRunner> {
    runner: R,
    dir: PathBuf,
}

impl<R: GitRunner> LibraryGit<R> {
    pub fn new(runner: R, dir: PathBuf) -> Self {
        Self { runner, dir }
    }

    /// Repo / not a repo / git could not be run. A spawn failure (no `git`
    /// on PATH) is reported as such rather than folded into "not a repo".
    pub fn probe(&self) -> RepoProbe {
        // The App Store sandbox can't run git at all; say that plainly rather
        // than suggesting an install that wouldn't help.
        if !crate::distribution::can_launch_user_programs() {
            return RepoProbe::GitUnavailable(GitRunError::Sandboxed.into());
        }
        match self.runner.run(&self.dir, &rev_parse_args()) {
            Err(GitRunError::Spawn(detail)) => {
                RepoProbe::GitUnavailable(crate::localization::deferred(
                    "explorer.git_panel.errors.git_not_installed",
                    &[("detail", detail)],
                ))
            }
            Err(error) => RepoProbe::GitUnavailable(error.into()),
            Ok(out) if out.success && out.stdout.trim() == "true" => RepoProbe::Repo,
            Ok(_) => RepoProbe::NotRepo,
        }
    }

    /// The repository's top-level directory.
    fn toplevel(&self) -> Result<PathBuf, DisplayMessage> {
        let out = self.runner.run(&self.dir, &show_toplevel_args())?;
        let top = out.stdout.trim_end_matches(['\n', '\r']);
        if !out.success || top.is_empty() {
            return Err(git_error("rev-parse --show-toplevel", &out));
        }
        Ok(PathBuf::from(top))
    }

    /// The changed-files list, or the git error text (missing binary,
    /// corrupt repo).
    pub fn status(&self) -> Result<Vec<GitChange>, DisplayMessage> {
        let out = self.runner.run(&self.dir, &status_args())?;
        if !out.success {
            return Err(git_error("status", &out));
        }
        Ok(parse_porcelain(&out.stdout))
    }

    /// The diff for one repo-relative path. Tries `HEAD` first (staged +
    /// unstaged in one view), then the plain worktree diff for a repo with
    /// no commits yet. An untracked file has no diff — the caller says so
    /// rather than showing an empty pane.
    pub fn diff(&self, path: &str) -> Result<String, DisplayMessage> {
        let out = self.runner.run(&self.dir, &diff_args(path))?;
        if out.success {
            return Ok(out.stdout);
        }
        let out = self.runner.run(&self.dir, &diff_args_no_head(path))?;
        if out.success {
            return Ok(out.stdout);
        }
        Err(git_error("diff", &out))
    }

    /// `git init` — mutating; UI must call this only from the button.
    pub fn init(&self) -> Result<(), DisplayMessage> {
        let out = self.runner.run(&self.dir, &init_args())?;
        if out.success {
            Ok(())
        } else {
            Err(git_error("init", &out))
        }
    }

    /// Stage everything (`None`) or the selected paths (`Some`) — mutating;
    /// UI must call this only from the commit buttons. This is the snapshot
    /// [`LibraryGit::commit`] records: content edited after this call stays
    /// out of the commit and shows up as an unstaged change instead.
    pub fn stage(&self, paths: Option<&[String]>) -> Result<(), DisplayMessage> {
        let out = self.runner.run(&self.dir, &stage_args(paths))?;
        if out.success {
            Ok(())
        } else {
            Err(git_error("add", &out))
        }
    }

    /// Commit the staged snapshot of `paths` (`None` = everything under the
    /// panel's folder). The commit is built in a temporary index seeded from
    /// `HEAD` and filled with the blobs `stage` recorded — the working tree
    /// is never consulted at commit time, so content edited after the review
    /// is not picked up, and anything staged outside the scope stays staged.
    /// Mutating; UI must call this only from the commit buttons, right after
    /// `stage` with the same scope.
    ///
    /// Path conventions: pathspec-taking reads (`ls-files`, `ls-tree`,
    /// `reset`) run in the panel's folder with `.` or `:(top,literal)`
    /// pathspecs; their `--full-name` output is repo-root-relative. Everything
    /// that consumes those root-relative paths as plain paths
    /// (`update-index`) — and the rest of the temp-index build with it — runs
    /// from the repository top level, where root-relative is cwd-relative.
    pub fn commit(
        &self,
        message: &str,
        paths: Option<&[String]>,
    ) -> Result<String, DisplayMessage> {
        let message = validated_message(message)?;
        let scope: Vec<String> = match paths {
            // Commit all = everything under the panel's folder, never changes
            // staged elsewhere in an enclosing repository.
            None => vec![".".to_string()],
            Some(paths) => paths.iter().map(|p| repo_pathspec(p)).collect(),
        };
        let top = self.toplevel()?;
        let has_head = self
            .runner
            .run(&self.dir, &rev_parse_head_args())
            .map(|out| out.success)
            .unwrap_or(false);
        let tmp = TempIndexFile::new()?;
        let env = tmp.env();
        let run = |args: &[String]| self.runner.run_with_env(&top, args, &env);
        if has_head {
            let out = run(&read_tree_head_args())?;
            if !out.success {
                return Err(git_error("read-tree", &out));
            }
        }
        // What the stage call left in the real index under the scope.
        let out = self.runner.run(&self.dir, &ls_files_args(&scope))?;
        if !out.success {
            return Err(git_error("ls-files", &out));
        }
        let staged = parse_ls_files(&out.stdout);
        let deletions: Vec<String> = if has_head {
            let out = self.runner.run(&self.dir, &head_paths_args(&scope))?;
            if !out.success {
                return Err(git_error("ls-tree", &out));
            }
            let indexed: std::collections::HashSet<&str> =
                staged.iter().map(|entry| entry.2.as_str()).collect();
            parse_raw_paths(&out.stdout)
                .into_iter()
                .filter(|path| !indexed.contains(path.as_str()))
                .collect()
        } else {
            Vec::new()
        };
        if !staged.is_empty() {
            let out = run(&cacheinfo_args(&staged))?;
            if !out.success {
                return Err(git_error("update-index", &out));
            }
        }
        if !deletions.is_empty() {
            let out = run(&removal_args(&deletions))?;
            if !out.success {
                return Err(git_error("update-index", &out));
            }
        }
        let out = run(&commit_index_args(&message))?;
        if !out.success {
            return Err(git_error("commit", &out));
        }
        let summary = out.stdout.trim().to_string();
        let out = self.runner.run(&self.dir, &reset_sync_args(&scope))?;
        if !out.success {
            return Err(git_error("reset", &out));
        }
        Ok(summary)
    }
}

/// `git <verb> failed: <why>`. Some failures explain themselves on stdout
/// only (`commit` with nothing to commit), so stdout is the fallback detail.
fn git_error(verb: &str, out: &GitOutput) -> DisplayMessage {
    let detail = match out.stderr.trim() {
        "" => out.stdout.trim(),
        stderr => stderr,
    };
    if detail.is_empty() {
        crate::localization::deferred(
            "explorer.git_panel.errors.command_failed",
            &[("command", verb)],
        )
    } else {
        // Bound what reaches the UI: a pathological hook can print a lot.
        let detail: String = detail.chars().take(800).collect();
        crate::localization::deferred(
            "explorer.git_panel.errors.command_failed_with_detail",
            &[("command", verb), ("detail", detail.as_str())],
        )
    }
}

/// How a diff line paints in the read-only preview.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffLineKind {
    /// `+` additions (but not the `+++` header).
    Add,
    /// `-` removals (but not the `---` header).
    Remove,
    /// `@@ … @@` hunk headers.
    Hunk,
    /// `diff --git`, `index`, `---`/`+++` headers.
    Meta,
    Context,
}

/// Classify one line of `git diff` output for coloring. Header lines win
/// over their leading sign (`--- a/x` is meta, not a removal).
pub fn diff_line_kind(line: &str) -> DiffLineKind {
    if line.starts_with("@@") {
        DiffLineKind::Hunk
    } else if line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("diff --git")
        || line.starts_with("index ")
        || line.starts_with("new file mode")
        || line.starts_with("deleted file mode")
        || line.starts_with("similarity index")
        || line.starts_with("rename from")
        || line.starts_with("rename to")
    {
        DiffLineKind::Meta
    } else if line.starts_with('+') {
        DiffLineKind::Add
    } else if line.starts_with('-') {
        DiffLineKind::Remove
    } else {
        DiffLineKind::Context
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct FakeGitRunner {
        calls: RefCell<Vec<(PathBuf, Vec<String>)>>,
        envs: RefCell<Vec<Vec<(String, String)>>>,
        answers: RefCell<Vec<Result<GitOutput, GitRunError>>>,
    }

    impl FakeGitRunner {
        fn answering(answers: Vec<Result<GitOutput, GitRunError>>) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                envs: RefCell::new(Vec::new()),
                answers: RefCell::new(answers),
            }
        }

        fn ok(stdout: &str) -> Result<GitOutput, GitRunError> {
            Ok(GitOutput {
                success: true,
                stdout: stdout.to_string(),
                stderr: String::new(),
            })
        }

        fn fail(stderr: &str) -> Result<GitOutput, GitRunError> {
            Ok(GitOutput {
                success: false,
                stdout: String::new(),
                stderr: stderr.to_string(),
            })
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.borrow().iter().map(|(_, a)| a.clone()).collect()
        }

        fn envs(&self) -> Vec<Vec<(String, String)>> {
            self.envs.borrow().clone()
        }

        fn dirs(&self) -> Vec<PathBuf> {
            self.calls.borrow().iter().map(|(d, _)| d.clone()).collect()
        }
    }

    impl GitRunner for FakeGitRunner {
        fn run_with_env(
            &self,
            dir: &Path,
            args: &[String],
            env: &[(&'static str, String)],
        ) -> Result<GitOutput, GitRunError> {
            self.calls
                .borrow_mut()
                .push((dir.to_path_buf(), args.to_vec()));
            self.envs.borrow_mut().push(
                env.iter()
                    .map(|(k, v)| (k.to_string(), v.clone()))
                    .collect(),
            );
            let mut answers = self.answers.borrow_mut();
            if answers.is_empty() {
                Self::ok("")
            } else {
                answers.remove(0)
            }
        }
    }

    #[test]
    fn porcelain_reads_every_xy_letter() {
        let text = "M  staged.sql\0 M unstaged.sql\0MM both.sql\0A  added.sql\0D  deleted.sql\0 D gone.sql\0U  conflicted.sql\0UU both_conflicted.sql\0?? new file.sql\0";
        let changes = parse_porcelain(text);
        let summary: Vec<(char, char, &str)> = changes
            .iter()
            .map(|c| (c.x, c.y, c.path.as_str()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ('M', ' ', "staged.sql"),
                (' ', 'M', "unstaged.sql"),
                ('M', 'M', "both.sql"),
                ('A', ' ', "added.sql"),
                ('D', ' ', "deleted.sql"),
                (' ', 'D', "gone.sql"),
                ('U', ' ', "conflicted.sql"),
                ('U', 'U', "both_conflicted.sql"),
                ('?', '?', "new file.sql"),
            ]
        );
        assert!(changes[8].is_untracked());
        assert!(changes[0].is_staged());
        assert!(!changes[1].is_staged());
        assert_eq!(changes[3].badge(), "A ");
        assert_eq!(changes[1].badge(), " M");
        assert_eq!(changes[2].badge(), "MM");
        assert_eq!(changes[8].badge(), "??");
    }

    #[test]
    fn porcelain_reads_renames_as_new_then_source_field() {
        // -z order: `XY new\0old\0`, no arrow.
        let changes = parse_porcelain("R  new name.sql\0old name.sql\0M  after.sql\0");
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].x, 'R');
        assert_eq!(changes[0].path, "new name.sql");
        assert_eq!(changes[0].orig_path.as_deref(), Some("old name.sql"));
        assert_eq!(
            changes[0].display_path(),
            "new name.sql (from old name.sql)"
        );
        // The source field is consumed, so the next entry parses cleanly.
        assert_eq!(changes[1].path, "after.sql");
        assert_eq!(changes[1].orig_path, None);
    }

    #[test]
    fn porcelain_keeps_odd_file_names_raw() {
        // A name containing ` -> ` misparsed as a rename in the old
        // line-based format; -z keeps it a plain path.
        let changes = parse_porcelain(
            "?? we -> ird.sql\0M  dir/\"q\" name.sql\0?? line\nbreak.sql\0?? café.sql\0",
        );
        let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "we -> ird.sql",
                "dir/\"q\" name.sql",
                "line\nbreak.sql",
                "café.sql"
            ]
        );
        assert!(changes.iter().all(|c| c.orig_path.is_none()));
    }

    #[test]
    fn porcelain_skips_junk_entries() {
        // Too-short entries, a missing separator, empty paths, and a rename
        // without its source field are dropped; only full entries parse.
        let changes = parse_porcelain("M\0??\0M  \0\0MMreal\0M  real.sql\0R  lonely.sql");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "real.sql");
    }

    #[test]
    fn commit_helpers_build_exact_argv() {
        assert_eq!(
            validated_message("  nightly scripts  ").unwrap(),
            "nightly scripts"
        );
        assert!(validated_message("   ").is_err(), "blank message refused");
        let long = "x".repeat(600);
        assert_eq!(
            validated_message(&long).unwrap().chars().count(),
            500,
            "message is capped"
        );

        let spec = ":(top,literal)a.sql".to_string();
        assert_eq!(
            ls_files_args(std::slice::from_ref(&spec)),
            vec!["ls-files", "--full-name", "-s", "-z", "--", &spec]
        );
        assert_eq!(
            head_paths_args(&[".".to_string()]),
            vec![
                "ls-tree",
                "-r",
                "--name-only",
                "-z",
                "--full-name",
                "HEAD",
                "--",
                "."
            ]
        );
        let entries = vec![(0o100644, "abc".to_string(), "a.sql".to_string())];
        assert_eq!(
            cacheinfo_args(&entries),
            vec!["update-index", "--add", "--cacheinfo", "100644,abc,a.sql"]
        );
        assert_eq!(
            removal_args(&["gone.sql".to_string()]),
            vec!["update-index", "--force-remove", "--", "gone.sql"]
        );
        assert_eq!(commit_index_args("msg"), vec!["commit", "-m", "msg"]);
        assert_eq!(
            reset_sync_args(&[".".to_string()]),
            vec!["reset", "-q", "--", "."]
        );

        let parsed = parse_ls_files("100644 abc 0\ta.sql\0\0");
        assert_eq!(parsed, vec![(0o100644, "abc".into(), "a.sql".into())]);
        // Conflict stages are skipped so an unmerged path cannot sneak in.
        let conflicted = parse_ls_files("100644 abc 0\ta.sql\x00100644 def 2\tb.sql\0");
        assert_eq!(conflicted.len(), 1);
        // Executable bits and symlinks round-trip through the mode.
        let modes = parse_ls_files("100755 abc 0\trun.sh\x00120000 def 0\tlink\0");
        assert_eq!(modes[0].0, 0o100755);
        assert_eq!(modes[1].0, 0o120000);
        assert_eq!(
            parse_raw_paths("a.sql\0dir/b.sql\0"),
            vec!["a.sql", "dir/b.sql"]
        );
    }

    #[test]
    fn status_runs_the_read_only_command_and_parses() {
        let runner = FakeGitRunner::answering(vec![FakeGitRunner::ok("M  a.sql\0?? b.sql\0")]);
        let git = LibraryGit::new(runner, PathBuf::from("/lib"));
        let changes = git.status().unwrap();
        assert_eq!(changes.len(), 2);
        let calls = git.runner.calls();
        assert_eq!(
            calls,
            vec![vec!["status", "--porcelain=v1", "-z", "-uall", "--", "."]]
        );
    }

    #[test]
    fn diff_falls_back_when_head_is_missing() {
        let runner = FakeGitRunner::answering(vec![
            FakeGitRunner::fail("ambiguous argument 'HEAD'"),
            FakeGitRunner::ok("diff --git a/a.sql b/a.sql\n+SELECT 1\n"),
        ]);
        let git = LibraryGit::new(runner, PathBuf::from("/lib"));
        let diff = git.diff("a.sql").unwrap();
        assert!(diff.contains("+SELECT 1"));
        let calls = git.runner.calls();
        assert_eq!(
            calls[0],
            vec!["diff", "--no-color", "HEAD", "--", ":(top,literal)a.sql"]
        );
        assert_eq!(
            calls[1],
            vec!["diff", "--no-color", "--", ":(top,literal)a.sql"]
        );
    }

    #[test]
    fn verbs_report_git_errors_honestly() {
        let runner = FakeGitRunner::answering(vec![FakeGitRunner::fail("not a git repository")]);
        let git = LibraryGit::new(runner, PathBuf::from("/lib"));
        let err = git.status().unwrap_err();
        assert!(err.contains("not a git repository"));
    }

    #[test]
    fn init_and_commit_pass_exact_argv() {
        let runner = FakeGitRunner::answering(vec![
            FakeGitRunner::ok("Initialized empty Git repository"),
            FakeGitRunner::ok(""),
            FakeGitRunner::ok("/lib\n"), // show-toplevel
            FakeGitRunner::fail("fatal: ambiguous argument 'HEAD'"), // unborn
            FakeGitRunner::ok("100644 abc123 0\tq.sql\0"),
            FakeGitRunner::ok(""),
            FakeGitRunner::ok("[main (root-commit) abc123] first"),
            FakeGitRunner::ok(""),
        ]);
        let git = LibraryGit::new(runner, PathBuf::from("/lib"));
        git.init().unwrap();
        git.stage(None).unwrap();
        let summary = git.commit("first", None).unwrap();
        assert!(summary.contains("abc123"));
        let calls = git.runner.calls();
        assert_eq!(calls[0], vec!["init"]);
        assert_eq!(calls[1], vec!["add", "-A", "--", "."]);
        assert_eq!(calls[2], vec!["rev-parse", "--show-toplevel"]);
        assert_eq!(calls[3], vec!["rev-parse", "--verify", "--quiet", "HEAD"]);
        assert_eq!(
            calls[4],
            vec!["ls-files", "--full-name", "-s", "-z", "--", "."]
        );
        assert_eq!(
            calls[5],
            vec![
                "update-index",
                "--add",
                "--cacheinfo",
                "100644,abc123,q.sql"
            ]
        );
        assert_eq!(calls[6], vec!["commit", "-m", "first"]);
        assert_eq!(calls[7], vec!["reset", "-q", "--", "."]);
        let envs = git.runner.envs();
        assert!(
            envs[4].is_empty(),
            "ls-files must read the real index, not the temp one"
        );
        assert_eq!(envs[5][0].0, "GIT_INDEX_FILE");
        assert_eq!(envs[6][0].0, "GIT_INDEX_FILE");
        assert!(
            envs[7].is_empty(),
            "reset must sync the real index, not the temp one"
        );
    }

    /// The panel's folder is `proj/` inside a larger repository: the
    /// pathspec reads run in the folder, while every temp-index write (which
    /// takes the root-relative paths as plain paths) runs from the top level.
    #[test]
    fn commit_in_a_subfolder_builds_the_index_from_the_top_level() {
        let top = PathBuf::from("/repo");
        let folder = top.join("proj");
        let selected = vec!["proj/gone.sql".to_string(), "proj/a.sql".to_string()];
        let runner = FakeGitRunner::answering(vec![
            FakeGitRunner::ok("/repo\n"),                     // show-toplevel
            FakeGitRunner::ok("abc123\n"),                    // rev-parse HEAD
            FakeGitRunner::ok(""),                            // read-tree HEAD
            FakeGitRunner::ok("100644 aaa 0\tproj/a.sql\0"),  // ls-files
            FakeGitRunner::ok("proj/a.sql\0proj/gone.sql\0"), // ls-tree
            FakeGitRunner::ok(""),                            // cacheinfo
            FakeGitRunner::ok(""),                            // force-remove
            FakeGitRunner::ok("[main def456] msg"),
            FakeGitRunner::ok(""),
        ]);
        let git = LibraryGit::new(runner, folder.clone());
        git.commit("msg", Some(&selected)).unwrap();
        let calls = git.runner.calls();
        let dirs = git.runner.dirs();
        assert_eq!(
            calls[6],
            vec!["update-index", "--force-remove", "--", "proj/gone.sql"]
        );
        for (index, expected) in [
            (0usize, &folder),
            (1, &folder),
            (2, &top),
            (3, &folder),
            (4, &folder),
            (5, &top),
            (6, &top),
            (7, &top),
            (8, &folder),
        ] {
            assert_eq!(&dirs[index], expected, "call {index}: {:?}", calls[index]);
        }
    }

    #[test]
    fn commit_seeds_the_temp_index_from_head_for_a_selected_file() {
        let selected = vec!["a.sql".to_string()];
        let spec = ":(top,literal)a.sql".to_string();
        let runner = FakeGitRunner::answering(vec![
            FakeGitRunner::ok("/lib\n"),   // show-toplevel
            FakeGitRunner::ok("abc123\n"), // rev-parse HEAD
            FakeGitRunner::ok(""),         // read-tree HEAD
            FakeGitRunner::ok("100644 aaa 0\ta.sql\0"),
            FakeGitRunner::ok("a.sql\0"), // HEAD still lists it
            FakeGitRunner::ok(""),
            FakeGitRunner::ok("[main def456] msg"),
            FakeGitRunner::ok(""),
        ]);
        let git = LibraryGit::new(runner, PathBuf::from("/lib"));
        let summary = git.commit("msg", Some(&selected)).unwrap();
        assert!(summary.contains("def456"));
        let calls = git.runner.calls();
        assert_eq!(calls[2], vec!["read-tree", "HEAD"]);
        assert_eq!(
            calls[3],
            vec!["ls-files", "--full-name", "-s", "-z", "--", &spec]
        );
        // a.sql is in the index, so nothing is force-removed.
        assert_eq!(
            calls[5],
            vec!["update-index", "--add", "--cacheinfo", "100644,aaa,a.sql"]
        );
        assert_eq!(calls[6], vec!["commit", "-m", "msg"]);
        assert_eq!(calls[7], vec!["reset", "-q", "--", &spec]);
        let envs = git.runner.envs();
        for call in [2usize, 5, 6] {
            assert_eq!(
                envs[call][0].0, "GIT_INDEX_FILE",
                "call {call} must use the temp index"
            );
        }
        assert!(envs[3].is_empty(), "ls-files reads the real index");
        assert!(envs[7].is_empty(), "reset syncs the real index");
    }

    #[test]
    fn commit_records_staged_deletions() {
        let selected = vec!["gone.sql".to_string()];
        let runner = FakeGitRunner::answering(vec![
            FakeGitRunner::ok("/lib\n"),
            FakeGitRunner::ok("abc123\n"),
            FakeGitRunner::ok(""),
            FakeGitRunner::ok(""),           // nothing staged under the path
            FakeGitRunner::ok("gone.sql\0"), // but HEAD still lists it
            FakeGitRunner::ok(""),
            FakeGitRunner::ok("[main def456] msg"),
            FakeGitRunner::ok(""),
        ]);
        let git = LibraryGit::new(runner, PathBuf::from("/lib"));
        git.commit("msg", Some(&selected)).unwrap();
        let calls = git.runner.calls();
        // Deletions come from the HEAD listing as raw repo-relative paths.
        assert_eq!(
            calls[5],
            vec!["update-index", "--force-remove", "--", "gone.sql"]
        );
    }

    #[test]
    fn failed_commit_never_runs_the_real_index_sync() {
        let selected = vec!["a.sql".to_string()];
        let runner = FakeGitRunner::answering(vec![
            FakeGitRunner::ok("/lib\n"),
            FakeGitRunner::ok("abc123\n"),
            FakeGitRunner::ok(""),
            FakeGitRunner::ok("100644 aaa 0\ta.sql\0"),
            FakeGitRunner::ok("a.sql\0"),
            FakeGitRunner::ok(""),
            FakeGitRunner::fail("pre-commit hook declined"),
        ]);
        let git = LibraryGit::new(runner, PathBuf::from("/lib"));
        let err = git.commit("msg", Some(&selected)).unwrap_err();
        assert!(err.contains("pre-commit hook declined"));
        let calls = git.runner.calls();
        assert_eq!(
            calls.last().unwrap(),
            &vec!["commit".to_string(), "-m".to_string(), "msg".to_string()],
            "a failed commit stops before the real index is touched"
        );
    }

    #[test]
    fn git_errors_fall_back_to_stdout_when_stderr_is_empty() {
        let out = GitOutput {
            success: false,
            stdout: "nothing to commit, working tree clean\n".into(),
            stderr: String::new(),
        };
        assert_eq!(
            git_error("commit", &out),
            "git commit failed: nothing to commit, working tree clean"
        );
    }

    #[test]
    #[cfg_attr(feature = "app-store", ignore = "the App Store edition never runs git")]
    fn missing_git_binary_is_not_reported_as_not_a_repo() {
        let runner = FakeGitRunner::answering(vec![Err(GitRunError::Spawn(
            "No such file or directory".to_string(),
        ))]);
        let git = LibraryGit::new(runner, PathBuf::from("/lib"));
        match git.probe() {
            RepoProbe::GitUnavailable(why) => assert!(why.contains("Install git"), "{why}"),
            other => panic!("expected GitUnavailable, got {other:?}"),
        }
        let runner =
            FakeGitRunner::answering(vec![FakeGitRunner::fail("fatal: not a git repository")]);
        let git = LibraryGit::new(runner, PathBuf::from("/lib"));
        assert_eq!(git.probe(), RepoProbe::NotRepo);
    }

    /// End to end against the real git CLI: a project folder that is a
    /// subfolder of a larger repository sees, diffs, and commits only its own
    /// files, with repo-root-relative paths resolved correctly. Skipped when
    /// no `git` binary is installed.
    #[test]
    #[cfg_attr(feature = "app-store", ignore = "the App Store edition never runs git")]
    fn subfolder_of_a_repo_is_scoped_end_to_end() {
        let have_git = std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !have_git {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let setup = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        setup(&["init", "-q"]);
        setup(&["config", "user.name", "Test"]);
        setup(&["config", "user.email", "test@example.invalid"]);
        setup(&["config", "commit.gpgsign", "false"]);
        let project = repo.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("q[1].sql"), "SELECT 1\n").unwrap();
        std::fs::write(repo.join("outside.sql"), "SELECT 2\n").unwrap();

        let git = LibraryGit::new(CliGitRunner, project.clone());
        assert_eq!(git.probe(), RepoProbe::Repo);
        let changes = git.status().unwrap();
        let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, vec!["proj/q[1].sql"], "scoped to the folder");

        // Commit the selected file by its repo-relative path (with glob
        // characters in the name).
        let selected = vec!["proj/q[1].sql".to_string()];
        git.stage(Some(&selected)).unwrap();
        git.commit("add query", Some(&selected)).unwrap();
        assert!(git.status().unwrap().is_empty());

        // Diff of a modified file resolves the repo-relative path.
        std::fs::write(project.join("q[1].sql"), "SELECT 42\n").unwrap();
        let diff = git.diff("proj/q[1].sql").unwrap();
        assert!(diff.contains("+SELECT 42"), "{diff}");

        // Commit all stays inside the folder: the outside file is untouched.
        git.stage(None).unwrap();
        git.commit("update", None).unwrap();
        assert!(git.status().unwrap().is_empty());
        let root_git = LibraryGit::new(CliGitRunner, repo.to_path_buf());
        let root_paths: Vec<String> = root_git
            .status()
            .unwrap()
            .into_iter()
            .map(|c| c.path)
            .collect();
        assert_eq!(root_paths, vec!["outside.sql".to_string()]);
    }

    /// `git ls-tree HEAD` for the whole repository, via the real CLI.
    fn head_files(repo: &Path) -> Vec<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["ls-tree", "-r", "--name-only", "-z", "HEAD"])
            .output()
            .unwrap();
        assert!(out.status.success());
        parse_raw_paths(&String::from_utf8_lossy(&out.stdout))
    }

    /// Regression: in a folder below the repository root, a staged deletion
    /// must be committed. The deleted paths are repo-root-relative
    /// (`proj/gone.sql`); `update-index` resolved them against the subfolder,
    /// removed nothing, and exited 0 — a deletion-only commit then failed
    /// with an empty "git commit failed", and a deletion alongside other
    /// changes was silently left out. Skipped when no `git` binary exists.
    #[test]
    #[cfg_attr(feature = "app-store", ignore = "the App Store edition never runs git")]
    fn subfolder_commit_records_deletions_alone_and_with_edits() {
        let have_git = std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !have_git {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let setup = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        setup(&["init", "-q"]);
        setup(&["config", "user.name", "Test"]);
        setup(&["config", "user.email", "test@example.invalid"]);
        setup(&["config", "commit.gpgsign", "false"]);
        let project = repo.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        for name in ["gone.sql", "gone2.sql", "gone3.sql", "keep.sql"] {
            std::fs::write(project.join(name), "SELECT 1\n").unwrap();
        }
        std::fs::write(repo.join("outside.sql"), "SELECT 2\n").unwrap();
        setup(&["add", "-A"]);
        setup(&["commit", "-q", "-m", "seed"]);
        let git = LibraryGit::new(CliGitRunner, project.clone());

        // 1. Deletion alone, committed as the selected file.
        std::fs::remove_file(project.join("gone.sql")).unwrap();
        let selected = vec!["proj/gone.sql".to_string()];
        git.stage(Some(&selected)).unwrap();
        git.commit("drop gone", Some(&selected))
            .expect("a deletion-only commit succeeds");
        assert!(!head_files(repo).contains(&"proj/gone.sql".to_string()));
        assert!(git.status().unwrap().is_empty(), "nothing left staged");

        // 2. Deletion alongside an edit, via Commit all.
        std::fs::remove_file(project.join("gone2.sql")).unwrap();
        std::fs::write(project.join("keep.sql"), "SELECT 42\n").unwrap();
        git.stage(None).unwrap();
        git.commit("drop gone2 + edit", None).unwrap();
        let head = head_files(repo);
        assert!(!head.contains(&"proj/gone2.sql".to_string()), "{head:?}");
        assert!(head.contains(&"proj/keep.sql".to_string()));
        assert!(
            git.status().unwrap().is_empty(),
            "the deletion is committed, not left un-staged by the sync"
        );

        // 3. Deletion-only via Commit all.
        std::fs::remove_file(project.join("gone3.sql")).unwrap();
        git.stage(None).unwrap();
        git.commit("drop gone3", None).unwrap();
        let head = head_files(repo);
        assert!(!head.contains(&"proj/gone3.sql".to_string()), "{head:?}");
        assert!(
            head.contains(&"outside.sql".to_string()),
            "outside untouched"
        );
    }

    /// The selected-file commit records the staged snapshot, not a fresh read
    /// of the working tree: content is staged, the file changes afterwards,
    /// and the commit must contain what was reviewed. Skipped when no `git`
    /// binary is installed.
    #[test]
    #[cfg_attr(feature = "app-store", ignore = "the App Store edition never runs git")]
    fn commit_records_the_staged_snapshot_not_the_working_tree() {
        let have_git = std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !have_git {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let setup = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        setup(&["init", "-q"]);
        setup(&["config", "user.name", "Test"]);
        setup(&["config", "user.email", "test@example.invalid"]);
        setup(&["config", "commit.gpgsign", "false"]);

        let git = LibraryGit::new(CliGitRunner, repo.to_path_buf());
        std::fs::write(repo.join("a.sql"), "reviewed\n").unwrap();
        git.stage(Some(&["a.sql".to_string()])).unwrap();
        // The file changes after review; the commit must not pick it up.
        std::fs::write(repo.join("a.sql"), "unreviewed\n").unwrap();
        git.commit("snapshot", Some(&["a.sql".to_string()]))
            .unwrap();

        let committed = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["show", "HEAD:a.sql"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(committed.status.success());
        assert_eq!(
            String::from_utf8_lossy(&committed.stdout).replace("\r\n", "\n"),
            "reviewed\n",
            "the commit holds the staged content, not the newer edit"
        );
        // The newer edit stays visible as an unstaged change.
        let status = git.status().unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].path, "a.sql");
        assert!(!status[0].is_staged(), "the post-review edit is unstaged");
    }

    #[cfg(feature = "app-store")]
    #[test]
    fn app_store_edition_probe_explains_the_sandbox_without_running_git() {
        let runner = FakeGitRunner::answering(vec![]);
        let git = LibraryGit::new(runner, PathBuf::from("/lib"));
        match git.probe() {
            RepoProbe::GitUnavailable(why) => {
                assert!(why.contains("Mac App Store edition"), "{why}");
                assert!(!why.contains("Install git"), "{why}");
            }
            other => panic!("expected GitUnavailable, got {other:?}"),
        }
    }

    #[test]
    #[cfg_attr(feature = "app-store", ignore = "the App Store edition never runs git")]
    fn probe_reads_rev_parse() {
        let runner = FakeGitRunner::answering(vec![FakeGitRunner::ok("true\n")]);
        let git = LibraryGit::new(runner, PathBuf::from("/lib"));
        assert_eq!(git.probe(), RepoProbe::Repo);
        let runner =
            FakeGitRunner::answering(vec![FakeGitRunner::fail("fatal: not a git repository")]);
        let git = LibraryGit::new(runner, PathBuf::from("/lib"));
        assert_eq!(git.probe(), RepoProbe::NotRepo);
    }

    #[test]
    fn diff_lines_classify_for_coloring() {
        assert_eq!(
            diff_line_kind("diff --git a/x.sql b/x.sql"),
            DiffLineKind::Meta
        );
        assert_eq!(diff_line_kind("--- a/x.sql"), DiffLineKind::Meta);
        assert_eq!(diff_line_kind("+++ b/x.sql"), DiffLineKind::Meta);
        assert_eq!(diff_line_kind("@@ -1,2 +1,2 @@"), DiffLineKind::Hunk);
        assert_eq!(diff_line_kind("+SELECT 1"), DiffLineKind::Add);
        assert_eq!(diff_line_kind("-SELECT 0"), DiffLineKind::Remove);
        assert_eq!(diff_line_kind(" unchanged"), DiffLineKind::Context);
        assert_eq!(
            diff_line_kind("\\ No newline at end of file"),
            DiffLineKind::Context
        );
        assert_eq!(diff_line_kind("rename from a.sql"), DiffLineKind::Meta);
    }
}
