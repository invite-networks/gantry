//! `gantry exec` over the API, including interactive TTY sessions with
//! raw mode and window resizing.

use super::Engine;
use crate::errors::{Error, Result};
use bollard::container::LogOutput;
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::query_parameters::ResizeExecOptions;
use futures_util::StreamExt;
use nix::sys::termios::{self, SetArg, Termios};
use std::os::fd::AsFd;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, Default)]
pub struct ExecRequest {
    pub cmd: Vec<String>,
    /// Allocate a TTY.
    pub tty: bool,
    /// Forward this process's stdin.
    pub interactive: bool,
    pub user: Option<String>,
    pub workdir: Option<String>,
    pub env: Vec<String>,
}

/// Puts the terminal on stdin in raw mode until dropped.
struct RawMode(Termios);

impl RawMode {
    fn enter() -> Result<Self> {
        let stdin = std::io::stdin();
        let saved = termios::tcgetattr(stdin.as_fd())
            .map_err(|e| Error::engine(format!("terminal: {e}")))?;
        let mut raw = saved.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(stdin.as_fd(), SetArg::TCSANOW, &raw)
            .map_err(|e| Error::engine(format!("terminal: {e}")))?;
        Ok(Self(saved))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(std::io::stdin().as_fd(), SetArg::TCSANOW, &self.0);
    }
}

nix::ioctl_read_bad!(tiocgwinsz, libc::TIOCGWINSZ, libc::winsize);

/// The terminal size on stdout as (rows, columns).
fn terminal_size() -> Option<(u16, u16)> {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ writes one winsize into the pointer we pass, which
    // points at a live, correctly sized value.
    let r = unsafe { tiocgwinsz(libc::STDOUT_FILENO, &mut ws) };
    (r.is_ok() && ws.ws_row > 0).then_some((ws.ws_row, ws.ws_col))
}

impl Engine {
    /// Runs a command in a container, wired to this process's stdio, and
    /// returns its exit code.
    pub async fn exec(&self, container: &str, req: ExecRequest) -> Result<i64> {
        let d = self.api().await?.clone();
        let opts = CreateExecOptions {
            attach_stdin: Some(req.interactive),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            tty: Some(req.tty),
            env: (!req.env.is_empty()).then_some(req.env.clone()),
            cmd: Some(req.cmd.clone()),
            user: req.user.clone(),
            working_dir: req.workdir.clone(),
            ..Default::default()
        };
        let id = d
            .create_exec(container, opts)
            .await
            .map_err(|e| self.err(&format!("exec {container}"), e))?
            .id;
        let started = d
            .start_exec(
                &id,
                Some(StartExecOptions {
                    detach: false,
                    tty: req.tty,
                    output_capacity: None,
                }),
            )
            .await
            .map_err(|e| self.err(&format!("exec {container}"), e))?;
        let StartExecResults::Attached {
            mut output,
            mut input,
        } = started
        else {
            return Err(Error::engine("exec: the engine did not attach"));
        };

        let raw = if req.tty {
            Some(RawMode::enter()?)
        } else {
            None
        };
        let mut resizer = None;
        if req.tty {
            let resize = |d: bollard::Docker, id: String| async move {
                if let Some((h, w)) = terminal_size() {
                    let _ = d
                        .resize_exec(
                            &id,
                            ResizeExecOptions {
                                h: i32::from(h),
                                w: i32::from(w),
                            },
                        )
                        .await;
                }
            };
            resize(d.clone(), id.clone()).await;
            if let Ok(mut winch) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
            {
                let (d, id) = (d.clone(), id.clone());
                resizer = Some(tokio::spawn(async move {
                    while winch.recv().await.is_some() {
                        resize(d.clone(), id.clone()).await;
                    }
                }));
            }
        }

        let stdin_task = req.interactive.then(|| {
            tokio::spawn(async move {
                let mut stdin = tokio::io::stdin();
                let _ = tokio::io::copy(&mut stdin, &mut input).await;
                let _ = input.shutdown().await;
            })
        });

        let mut out = tokio::io::stdout();
        let mut err = tokio::io::stderr();
        let mut failure = None;
        while let Some(chunk) = output.next().await {
            match chunk {
                Ok(LogOutput::StdErr { message }) => {
                    let _ = err.write_all(&message).await;
                    let _ = err.flush().await;
                }
                Ok(LogOutput::StdOut { message } | LogOutput::Console { message }) => {
                    let _ = out.write_all(&message).await;
                    let _ = out.flush().await;
                }
                Ok(LogOutput::StdIn { .. }) => {}
                Err(e) => {
                    failure = Some(self.err("exec", e));
                    break;
                }
            }
        }
        drop(raw);
        if let Some(t) = resizer {
            t.abort();
        }
        if let Some(t) = stdin_task {
            t.abort();
        }
        if let Some(e) = failure {
            return Err(e);
        }
        let info = d
            .inspect_exec(&id)
            .await
            .map_err(|e| self.err("exec inspect", e))?;
        Ok(info.exit_code.unwrap_or(0))
    }
}
