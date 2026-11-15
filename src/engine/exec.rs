//! Interactive sessions over the API: `gantry exec` in a running service
//! and `gantry run` for one-off containers, with TTY raw mode, window
//! resizing and stdin forwarding.

use super::{Engine, LABEL_HASH};
use crate::errors::{Error, Result};
use bollard::container::LogOutput;
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::models::ContainerCreateBody;
use bollard::query_parameters::{
    AttachContainerOptions, ResizeContainerTTYOptions, ResizeExecOptions, WaitContainerOptions,
};
use futures_util::{Stream, StreamExt};
use nix::sys::termios::{self, SetArg, Termios};
use std::future::Future;
use std::os::fd::AsFd;
use std::pin::Pin;
use tokio::io::{AsyncWrite, AsyncWriteExt};

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

type Output =
    Pin<Box<dyn Stream<Item = std::result::Result<LogOutput, bollard::errors::Error>> + Send>>;
type Input = Pin<Box<dyn AsyncWrite + Send>>;

/// Wires an attached stream to this process's stdio until the output
/// ends. `resize` is called with (rows, columns) at the start and on every
/// window change when a TTY is in use.
async fn pump<R, F>(
    mut output: Output,
    mut input: Input,
    tty: bool,
    interactive: bool,
    resize: R,
) -> Result<()>
where
    R: Fn(u16, u16) -> F + Send + Sync + 'static,
    F: Future<Output = ()> + Send,
{
    let raw = if tty { Some(RawMode::enter()?) } else { None };
    let mut resizer = None;
    if tty {
        if let Some((h, w)) = terminal_size() {
            resize(h, w).await;
        }
        if let Ok(mut winch) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
        {
            resizer = Some(tokio::spawn(async move {
                while winch.recv().await.is_some() {
                    if let Some((h, w)) = terminal_size() {
                        resize(h, w).await;
                    }
                }
            }));
        }
    }
    let stdin_task = interactive.then(|| {
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
                failure = Some(Error::engine(format!("attach: {e}")));
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
    failure.map_or(Ok(()), Err)
}

/// What `gantry run` changes in a service's create body.
#[derive(Debug, Clone, Default)]
pub struct RunRequest {
    /// Replaces the service command when not empty.
    pub cmd: Vec<String>,
    pub entrypoint: Option<Vec<String>>,
    pub tty: bool,
    pub interactive: bool,
    pub user: Option<String>,
    pub workdir: Option<String>,
    pub env: Vec<String>,
}

/// Label marking one-off containers, so `up` and `ps` leave them alone.
pub const LABEL_ONEOFF: &str = "io.gantry.oneoff";

/// Turns a service's create body into a one-off container's.
pub fn oneoff_body(mut body: ContainerCreateBody, req: &RunRequest) -> ContainerCreateBody {
    if !req.cmd.is_empty() {
        body.cmd = Some(req.cmd.clone());
    }
    if let Some(ep) = &req.entrypoint {
        body.entrypoint = Some(ep.clone());
    }
    if let Some(u) = &req.user {
        body.user = Some(u.clone());
    }
    if let Some(w) = &req.workdir {
        body.working_dir = Some(w.clone());
    }
    let mut env = body.env.take().unwrap_or_default();
    env.extend(req.env.iter().cloned());
    body.env = Some(env);
    body.tty = Some(req.tty);
    body.open_stdin = Some(req.interactive);
    body.stdin_once = Some(req.interactive);
    body.attach_stdin = Some(req.interactive);
    body.attach_stdout = Some(true);
    body.attach_stderr = Some(true);
    body.healthcheck = None;
    if let Some(h) = body.host_config.as_mut() {
        h.restart_policy = None;
    }
    if let Some(l) = body.labels.as_mut() {
        l.remove(LABEL_HASH);
        l.insert(LABEL_ONEOFF.into(), "true".into());
    }
    body
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
        let StartExecResults::Attached { output, input } = started else {
            return Err(Error::engine("exec: the engine did not attach"));
        };
        let (rd, rid) = (d.clone(), id.clone());
        pump(output, input, req.tty, req.interactive, move |h, w| {
            let (d, id) = (rd.clone(), rid.clone());
            async move {
                let opts = ResizeExecOptions {
                    h: i32::from(h),
                    w: i32::from(w),
                };
                let _ = d.resize_exec(&id, opts).await;
            }
        })
        .await?;
        let info = d
            .inspect_exec(&id)
            .await
            .map_err(|e| self.err("exec inspect", e))?;
        Ok(info.exit_code.unwrap_or(0))
    }

    /// Creates a one-off container, attaches to it, starts it, and returns
    /// its exit code once it ends. With `rm`, the container is removed
    /// afterwards, but only if this call created it.
    pub async fn run_attached(
        &self,
        name: &str,
        body: ContainerCreateBody,
        tty: bool,
        interactive: bool,
        rm: bool,
    ) -> Result<i64> {
        self.create(name, body).await?;
        let code = self.attach_and_wait(name, tty, interactive).await;
        if rm {
            let _ = self.remove(name).await;
        }
        code
    }

    async fn attach_and_wait(&self, name: &str, tty: bool, interactive: bool) -> Result<i64> {
        let d = self.api().await?.clone();
        let attached = d
            .attach_container(
                name,
                Some(AttachContainerOptions {
                    stdin: interactive,
                    stdout: true,
                    stderr: true,
                    stream: true,
                    logs: true,
                    detach_keys: None,
                }),
            )
            .await
            .map_err(|e| self.err(&format!("attach {name}"), e))?;
        // Wait for the exit before starting so a fast exit is not missed.
        let mut waiter = d.wait_container(name, None::<WaitContainerOptions>);
        self.start(name).await?;
        let (rd, rname) = (d.clone(), name.to_string());
        pump(
            attached.output,
            attached.input,
            tty,
            interactive,
            move |h, w| {
                let (d, name) = (rd.clone(), rname.clone());
                async move {
                    let opts = ResizeContainerTTYOptions {
                        h: i32::from(h),
                        w: i32::from(w),
                    };
                    let _ = d.resize_container_tty(&name, opts).await;
                }
            },
        )
        .await?;
        match waiter.next().await {
            Some(Ok(r)) => Ok(r.status_code),
            // bollard reports a non-zero exit as an error carrying the code.
            Some(Err(bollard::errors::Error::DockerContainerWaitError { code, .. })) => Ok(code),
            Some(Err(e)) => Err(self.err(&format!("wait {name}"), e)),
            None => Ok(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn oneoff_overrides() {
        let body = ContainerCreateBody {
            cmd: Some(vec!["serve".into()]),
            env: Some(vec!["A=1".into()]),
            labels: Some(HashMap::from([(LABEL_HASH.to_string(), "x".to_string())])),
            host_config: Some(bollard::models::HostConfig {
                restart_policy: Some(bollard::models::RestartPolicy::default()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let req = RunRequest {
            cmd: vec!["migrate".into()],
            env: vec!["B=2".into()],
            interactive: true,
            ..Default::default()
        };
        let b = oneoff_body(body, &req);
        assert_eq!(b.cmd.as_deref(), Some(&["migrate".to_string()][..]));
        assert_eq!(b.env.as_ref().map(Vec::len), Some(2));
        assert_eq!(b.open_stdin, Some(true));
        let labels = b.labels.unwrap();
        assert!(!labels.contains_key(LABEL_HASH));
        assert_eq!(labels[LABEL_ONEOFF], "true");
        assert!(b.host_config.unwrap().restart_policy.is_none());
    }
}
