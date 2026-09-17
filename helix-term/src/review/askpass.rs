//! SSH passphrase prompting for `gh pr checkout`.
//!
//! When a remote uses an SSH key with a passphrase, `ssh` would normally ask
//! for it on the terminal. Inside helix the terminal is owned by the TUI, so
//! this module wires up OpenSSH's `SSH_ASKPASS` mechanism instead:
//!
//! 1. A tiny helper script is shipped to a private temp directory and pointed
//!    to via `SSH_ASKPASS` (+ `SSH_ASKPASS_REQUIRE=force`, which makes `ssh`
//!    use it even though a terminal exists).
//! 2. Whenever `ssh` needs a passphrase it runs the helper. The helper writes
//!    ssh's question to a marker file and then waits for a passphrase file.
//! 3. Helix polls the marker file while `gh` runs; once it appears, a real
//!    prompt is shown with ssh's question. On submit the passphrase is
//!    written to the passphrase file, the helper relays it to `ssh`, and the
//!    checkout continues.

use std::ffi::OsStr;
use std::fmt::Debug;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::compositor;
use crate::job;
use crate::ui::{self, Prompt, PromptEvent};

const PROMPT_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Wait at most this long for `gh` (taking any passphrase dialogs into
/// account) before giving up.
const ASK_TIMEOUT: Duration = Duration::from_secs(300);

const ASKPASS_SCRIPT: &str = r#"#!/bin/sh
# ssh-askpass helper for helix: relay ssh's question to the editor and return
# the passphrase the user types there. The prompt file is written first; the
# passphrase file is produced by the editor once the user submits.
printf '%s\n' "$1" > "$HELIX_SSH_ASKPROMPT_FILE" 2>/dev/null || exit 1
while [ ! -s "$HELIX_SSH_ASKPASS_FILE" ]; do sleep 0.05; done
exec cat "$HELIX_SSH_ASKPASS_FILE"
"#;

/// Run `gh` the same way [`super::gh::run_gh`] does, but show an in-editor
/// prompt instead of choking when `ssh` asks for a passphrase.
#[cfg(unix)]
pub async fn run_gh_with_askpass<S: AsRef<OsStr> + Debug>(
    cwd: &Path,
    args: &[S],
) -> anyhow::Result<String> {
    let session = AskpassSession::new()?;
    let args: Vec<String> = args
        .iter()
        .map(|arg| arg.as_ref().to_string_lossy().into_owned())
        .collect();
    // `run` borrows the session (and thus the temp directory), which is
    // removed once it goes out of scope.
    let result = session.run(cwd, &args).await;
    drop(session);
    result
}

/// One passphrase-intercepting `gh` invocation.
#[cfg(unix)]
struct AskpassSession {
    dir: PathBuf,
    helper: PathBuf,
    prompt_file: PathBuf,
    pass_file: PathBuf,
}

#[cfg(unix)]
impl AskpassSession {
    fn new() -> anyhow::Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "helix-ssh-askpass-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default(),
            // Disambiguate sibling sessions created in the same instant.
            NEXT_SESSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        create_private_dir(&dir)
            .map_err(|err| anyhow!("could not create the ssh askpass directory: {err}"))?;
        let helper = dir.join("askpass");
        let prompt_file = dir.join("prompt");
        let pass_file = dir.join("passphrase");
        fs::write(&helper, ASKPASS_SCRIPT)?;
        set_executable(&helper)?;
        Ok(Self {
            dir,
            helper,
            prompt_file,
            pass_file,
        })
    }

    /// Point the spawned `gh`/`git`/`ssh` pipeline at the helper.
    fn configure(&self, command: &mut Command) {
        command
            .env("SSH_ASKPASS", &self.helper)
            // `force` makes ssh use the helper even though a terminal is
            // attached (OpenSSH >= 8.4).
            .env("SSH_ASKPASS_REQUIRE", "force")
            // Legacy ssh versions consult DISPLAY before honoring
            // SSH_ASKPASS; the fake value keeps them from erroring out.
            .env("DISPLAY", ":0")
            .env("HELIX_SSH_ASKPROMPT_FILE", &self.prompt_file)
            .env("HELIX_SSH_ASKPASS_FILE", &self.pass_file);
    }

    /// True once ssh's question has been relayed by the helper.
    fn prompt_requested(&self) -> bool {
        fs::metadata(&self.prompt_file)
            .map(|meta| meta.len() > 0)
            .unwrap_or(false)
    }

    async fn run(&self, cwd: &Path, args: &[String]) -> anyhow::Result<String> {
        let mut command = Command::new("gh");
        command
            .args(args)
            .current_dir(cwd)
            .env("GH_PROMPT_DISABLED", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        self.configure(&mut command);

        let mut child = command.spawn().map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                anyhow!(
                    "the `gh` CLI was not found on PATH; install it from \
                     https://cli.github.com and log in with `gh auth login`"
                )
            } else {
                anyhow!("failed to run `gh`: {err}")
            }
        })?;

        // Drain stderr in the background so a noisy `git fetch` can never
        // fill the pipe and stall the child while we poll for the prompt.
        let mut stderr = child.stderr.take().expect("stderr is piped");
        let stderr_reader = tokio::spawn(async move {
            let mut buffer = String::new();
            let _ = stderr.read_to_string(&mut buffer).await;
            buffer
        });

        let outcome = tokio::time::timeout(ASK_TIMEOUT, async {
            let mut prompt_shown = false;
            loop {
                if let Some(status) = child
                    .try_wait()
                    .map_err(|err| anyhow!("failed to probe the `gh` process: {err}"))?
                {
                    let stderr = stderr_reader.await.unwrap_or_default();
                    return Ok::<_, anyhow::Error>((status, stderr));
                }

                if !prompt_shown && self.prompt_requested() {
                    prompt_shown = true;
                    self.prompt_for_passphrase().await?;
                }

                tokio::time::sleep(PROMPT_POLL_INTERVAL).await;
            }
        })
        .await;

        // Whatever went wrong (incl. the passphrase dialog timing out), the
        // child must not be left hanging with no one to answer its helper.
        let result = match outcome {
            Ok(result) => result,
            Err(_elapsed) => Err(anyhow!(
                "`gh` did not finish within {} seconds while waiting for the SSH passphrase",
                ASK_TIMEOUT.as_secs()
            )),
        };
        let (status, stderr) = match result {
            Ok(result) => result,
            Err(error) => {
                let _ = child.kill().await;
                return Err(error);
            }
        };

        if !status.success() {
            bail!(
                "`gh {args:?}` failed: {}",
                super::gh::error_text(&stderr, "")
            );
        }
        Ok(String::new())
    }

    /// Show ssh's question as an editor prompt and relay the answer to the
    /// helper by writing the passphrase file.
    async fn prompt_for_passphrase(&self) -> anyhow::Result<()> {
        let prompt_text = fs::read_to_string(&self.prompt_file)
            .unwrap_or_default()
            .trim()
            .to_string();
        let prompt_text = if prompt_text.is_empty() {
            "SSH passphrase:".to_string()
        } else {
            prompt_text
        };

        let (answer_sender, mut answer_receiver) = mpsc::channel::<Option<String>>(1);
        job::dispatch_blocking(move |_editor, compositor| {
            let mut prompt = Prompt::new(
                prompt_text.clone().into(),
                None,
                ui::completers::none,
                move |_cx: &mut compositor::Context, input: &str, event: PromptEvent| {
                    // Only terminal events may reach the channel; `Update`
                    // fires on every keystroke and would otherwise be read
                    // as an abort.
                    let answer = match event {
                        PromptEvent::Validate => Some(input.trim().to_string()),
                        PromptEvent::Abort => None,
                        PromptEvent::Update => return,
                    };
                    let _ = answer_sender.try_send(answer);
                },
            );
            prompt.recalculate_completion(_editor);
            compositor.push(Box::new(prompt));
        });

        let passphrase = tokio::time::timeout(ASK_TIMEOUT, answer_receiver.recv())
            .await
            .map_err(|_| anyhow!("timed out waiting for the SSH passphrase"))?
            .ok_or_else(|| anyhow!("could not receive the SSH passphrase"))?
            .unwrap_or_default();

        // Dismiss the prompt; the helper (and with it ssh) resumes as soon as
        // the passphrase file is written.
        job::dispatch_blocking(|_editor, compositor| {
            compositor.remove_type::<Prompt>();
        });
        // Always write a newline so that aborting the dialog yields an empty
        // (failed) passphrase instead of leaving the helper polling forever.
        fs::write(&self.pass_file, format!("{passphrase}\n"))?;
        Ok(())
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions)
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

/// Disambiguate askpass sessions created within the same instant.
static NEXT_SESSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(unix)]
impl Drop for AskpassSession {
    /// Remove the passphrase and helper files; the passphrase file only ever
    /// exists while the checkout is in flight.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
