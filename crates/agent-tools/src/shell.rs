//! Shell command tool.
//!
//! Release packages ship a bash runtime next to the executable (built from the
//! latest GNU bash on Linux/macOS, from MSYS2 on Windows by
//! `scripts/fetch-bash.sh` / `scripts/bundle-msys-shell.py`). [`ShellSpec`]
//! resolves it once per tool so the agent gets one predictable shell across
//! platforms; developer builds without the runtime keep using the platform
//! default (`sh -lc`, or `cmd /C` on Windows).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use effibuddy_agent_core::{ApprovalPolicy, Tool, ToolContext, ToolError, ToolOutput};

use crate::args::{optional_u64, required_string};
use crate::workspace::Workspace;

const MAX_OUTPUT_CHARS: usize = 24_000;

/// Environment variable that overrides shell discovery. Point it at a bash
/// binary to exercise the bundled path without packaging a release.
pub const SHELL_ENV: &str = "EFFIBUDDY_SHELL";

/// How the shell tool launches a command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellSpec {
    /// Program to spawn (`bash`, `sh`, `cmd`, ...).
    pub program: PathBuf,
    /// Arguments inserted before the command string (`-lc`, `-c`, ...).
    pub args: Vec<String>,
    /// Extra environment for the child process (PATH wiring on Windows).
    pub env: Vec<(String, String)>,
    /// True when the program is the bash runtime shipped with the app.
    pub bundled: bool,
}

impl ShellSpec {
    /// Platform default used when no bundled runtime is present.
    pub fn system() -> Self {
        #[cfg(windows)]
        {
            Self {
                program: PathBuf::from("cmd"),
                args: vec!["/C".to_string()],
                env: Vec::new(),
                bundled: false,
            }
        }
        #[cfg(not(windows))]
        {
            Self {
                program: PathBuf::from("sh"),
                args: vec!["-lc".to_string()],
                env: Vec::new(),
                bundled: false,
            }
        }
    }

    /// Spec for an explicit bash path (used by [`SHELL_ENV`] and embedders).
    /// Arguments follow the host platform's conventions.
    pub fn for_bash(program: impl Into<PathBuf>) -> Self {
        bash_spec(program.into(), false)
    }

    /// Resolve the shell for a new tool: `EFFIBUDDY_SHELL`, then the runtime
    /// shipped next to the executable, then the platform default.
    pub fn resolve() -> Self {
        Self::from_override()
            .or_else(Self::bundled)
            .unwrap_or_else(Self::system)
    }

    fn from_override() -> Option<Self> {
        Self::spec_for_override(&std::env::var(SHELL_ENV).ok()?)
    }

    fn spec_for_override(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        let path = PathBuf::from(raw);
        path.is_file().then(|| Self::for_bash(path))
    }

    fn bundled() -> Option<Self> {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?;
        bundled_candidates(dir)
            .into_iter()
            .find(|path| path.is_file())
            .map(|path| bash_spec(path, true))
    }
}

fn bash_spec(program: PathBuf, bundled: bool) -> ShellSpec {
    #[cfg(windows)]
    {
        windows_bash_spec(program, bundled)
    }
    #[cfg(not(windows))]
    {
        ShellSpec {
            program,
            // Login shells keep sourcing /etc/profile and ~/.bash_profile, so
            // PATH additions from the user's environment still apply; this is
            // the same contract the previous `sh -lc` had.
            args: vec!["-lc".to_string()],
            env: Vec::new(),
            bundled,
        }
    }
}

/// Locations a packaged release may place the bash runtime in, in preference
/// order. Kept separate from [`ShellSpec::bundled`] so tests can exercise the
/// ordering without a real executable.
fn bundled_candidates(exe_dir: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    #[cfg(windows)]
    {
        candidates.push(
            exe_dir
                .join("runtime")
                .join("msys")
                .join("usr")
                .join("bin")
                .join("bash.exe"),
        );
        candidates.push(exe_dir.join("runtime").join("bash.exe"));
    }
    #[cfg(not(windows))]
    {
        // Portable archive / macOS archive: runtime/ sits next to the binary.
        candidates.push(exe_dir.join("runtime").join("bash"));
        candidates.push(exe_dir.join("runtime").join("bin").join("bash"));
        if let Some(prefix) = exe_dir.parent() {
            // deb/rpm: /usr/bin/effibuddy + /usr/lib/effibuddy/runtime/bash.
            candidates.push(
                prefix
                    .join("lib")
                    .join("effibuddy")
                    .join("runtime")
                    .join("bash"),
            );
            // macOS .app bundle: Contents/MacOS/effibuddy and
            // Contents/Resources/runtime/bash.
            candidates.push(prefix.join("Resources").join("runtime").join("bash"));
        }
    }
    candidates
}

/// MSYS2 / Git Bash needs its own `usr/bin` on PATH; the runtime converts the
/// Windows-style list to POSIX paths when the process starts. Profiles are
/// skipped deliberately: they `cd` to `$HOME`, which would break the workspace
/// cwd, and a reduced bundle may not ship them.
#[cfg(windows)]
fn windows_bash_spec(program: PathBuf, bundled: bool) -> ShellSpec {
    // <root>/usr/bin/bash.exe -> <root>
    let root = program
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent);
    let mut entries = Vec::new();
    if let Some(root) = root {
        for relative in ["usr/bin", "usr/local/bin", "mingw64/bin"] {
            let dir = root.join(relative);
            if dir.is_dir() {
                entries.push(dir);
            }
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        entries.extend(std::env::split_paths(&path));
    }
    let mut env = Vec::new();
    let joined = std::env::join_paths(&entries)
        .map(|paths| paths.to_string_lossy().into_owned())
        .unwrap_or_else(|_| {
            // join_paths rejects paths containing a quote; a manual join keeps
            // the bundled userland reachable even then.
            entries
                .iter()
                .map(|path| path.to_string_lossy())
                .collect::<Vec<_>>()
                .join(";")
        });
    if !joined.is_empty() {
        env.push(("PATH".to_string(), joined));
    }
    // Without /etc/passwd the runtime synthesizes an account entry whose home
    // directory may not exist; pointing HOME at the real Windows profile keeps
    // `~` and git's global config working.
    if std::env::var_os("HOME").is_none() {
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            env.push(("HOME".to_string(), profile.to_string_lossy().into_owned()));
        }
    }
    ShellSpec {
        program,
        args: vec![
            "--noprofile".to_string(),
            "--norc".to_string(),
            "-c".to_string(),
        ],
        env,
        bundled,
    }
}

#[derive(Clone)]
pub struct ShellTool {
    workspace: Workspace,
    shell: ShellSpec,
}

impl ShellTool {
    pub fn new(workspace: Workspace) -> Self {
        Self {
            workspace,
            shell: ShellSpec::resolve(),
        }
    }

    /// Build a tool with an explicit shell (tests, embeddings, previews).
    pub fn with_shell(workspace: Workspace, shell: ShellSpec) -> Self {
        Self { workspace, shell }
    }

    pub fn shell(&self) -> &ShellSpec {
        &self.shell
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Run a shell command in the workspace. Commands run under the bundled bash \
         when the app ships one, otherwise under the platform shell. Use for builds, \
         tests and git. Output is captured and truncated. Requires user approval."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Command line to execute."},
                "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 300, "description": "Defaults to 60 seconds."}
            },
            "required": ["command"]
        })
    }

    fn approval(&self) -> ApprovalPolicy {
        ApprovalPolicy::Ask
    }

    fn summarize(&self, arguments: &Value) -> String {
        let command = arguments
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("(missing command)");
        format!("Run shell command: {command}")
    }

    async fn execute(&self, ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        let command_line = required_string(&arguments, "command")?;
        if command_line.trim().is_empty() {
            return Err(ToolError::new("`command` must not be empty"));
        }
        let timeout_secs = optional_u64(&arguments, "timeout_secs")?
            .unwrap_or(60)
            .clamp(1, 300);
        let timeout = Duration::from_secs(timeout_secs);
        if let Some(sink) = &ctx.checkpoint_sink {
            // Shell commands can rewrite arbitrary paths, so the turn is
            // marked as having untracked changes; rewind warns about them
            // instead of pretending the restore is complete.
            sink.note_shell(&command_line);
        }

        let mut command = Command::new(&self.shell.program);
        command.args(&self.shell.args).arg(&command_line);
        if !self.shell.env.is_empty() {
            command.envs(self.shell.env.iter().map(|(key, value)| (key, value)));
        }
        command
            .current_dir(self.workspace.root())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let child = command
            .spawn()
            .map_err(|error| ToolError::new(format!("failed to start shell command: {error}")))?;

        let output = tokio::select! {
            _ = ctx.cancel.cancelled() => {
                return Err(ToolError::new("cancelled"));
            }
            result = tokio::time::timeout(timeout, child.wait_with_output()) => {
                match result {
                    Err(_) => {
                        return Err(ToolError::new(format!(
                            "command timed out after {timeout_secs}s"
                        )));
                    }
                    Ok(Err(error)) => {
                        return Err(ToolError::new(format!(
                            "failed to wait for shell command: {error}"
                        )));
                    }
                    Ok(Ok(output)) => output,
                }
            }
        };

        let stdout = truncate(&String::from_utf8_lossy(&output.stdout), MAX_OUTPUT_CHARS);
        let stderr = truncate(&String::from_utf8_lossy(&output.stderr), MAX_OUTPUT_CHARS);
        let code = output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "terminated by signal".to_string());

        let mut result = format!("exit_code: {code}\n");
        if !stdout.trim().is_empty() {
            result.push_str("\n--- stdout ---\n");
            result.push_str(&stdout);
            if !stdout.ends_with('\n') {
                result.push('\n');
            }
        }
        if !stderr.trim().is_empty() {
            result.push_str("\n--- stderr ---\n");
            result.push_str(&stderr);
            if !stderr.ends_with('\n') {
                result.push('\n');
            }
        }
        if stdout.trim().is_empty() && stderr.trim().is_empty() {
            result.push_str("(no output)");
        }
        Ok(ToolOutput::text(result))
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        out.push_str("\n[output truncated]");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn candidates_prefer_the_runtime_next_to_the_executable() {
        let exe_dir = Path::new("/opt/effibuddy");
        let candidates = bundled_candidates(exe_dir);
        assert_eq!(candidates[0], exe_dir.join("runtime").join("bash"));
        assert!(candidates.contains(&PathBuf::from("/opt/lib/effibuddy/runtime/bash")));
        assert!(candidates.contains(&PathBuf::from("/opt/Resources/runtime/bash")));
    }

    #[cfg(windows)]
    #[test]
    fn candidates_prefer_the_msys_runtime_next_to_the_executable() {
        let exe_dir = Path::new(r"C:\Program Files\EffiBuddy");
        let candidates = bundled_candidates(exe_dir);
        assert_eq!(
            candidates[0],
            exe_dir
                .join("runtime")
                .join("msys")
                .join("usr")
                .join("bin")
                .join("bash.exe")
        );
    }

    #[test]
    fn system_spec_matches_the_platform_default() {
        let system = ShellSpec::system();
        assert!(!system.bundled);
        assert!(system.env.is_empty());
        #[cfg(windows)]
        {
            assert_eq!(system.program, PathBuf::from("cmd"));
            assert_eq!(system.args, vec!["/C".to_string()]);
        }
        #[cfg(not(windows))]
        {
            assert_eq!(system.program, PathBuf::from("sh"));
            assert_eq!(system.args, vec!["-lc".to_string()]);
        }
    }

    #[test]
    fn explicit_bash_uses_platform_arguments() {
        let spec = ShellSpec::for_bash("/tmp/bash");
        assert_eq!(spec.program, PathBuf::from("/tmp/bash"));
        assert!(!spec.bundled);
        #[cfg(windows)]
        assert_eq!(
            spec.args,
            vec!["--noprofile", "--norc", "-c"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
        #[cfg(not(windows))]
        assert_eq!(spec.args, vec!["-lc".to_string()]);
    }

    #[test]
    fn override_requires_an_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let bash = directory.path().join("bash");
        std::fs::write(&bash, b"").unwrap();

        assert_eq!(ShellSpec::spec_for_override("   "), None);
        assert_eq!(
            ShellSpec::spec_for_override("/definitely/missing/bash"),
            None
        );
        let spec = ShellSpec::spec_for_override(&bash.to_string_lossy()).unwrap();
        assert_eq!(spec.program, bash);
        assert!(!spec.bundled);
    }

    #[cfg(windows)]
    #[test]
    fn windows_spec_wires_the_msys_userland_onto_path() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("usr").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let bash = bin.join("bash.exe");
        std::fs::write(&bash, b"").unwrap();

        let spec = ShellSpec::for_bash(&bash);
        let path = spec
            .env
            .iter()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| value.clone())
            .expect("PATH is injected");
        assert!(path.starts_with(bin.to_string_lossy().as_ref()));
    }
}
