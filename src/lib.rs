//! Async execution and interaction with processes.

#![cfg_attr(unix, forbid(unsafe_code))]
#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]

use std::ffi::OsStr;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::thread;

use async_channel::{Receiver, Sender};
#[cfg(unix)]
use async_io::Async;
#[cfg(windows)]
use blocking::Unblock;
use futures_lite::*;
use once_cell::sync::Lazy;

#[doc(no_inline)]
pub use std::process::{ExitStatus, Output, Stdio};

pub struct Child {
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,

    child: Arc<Mutex<std::process::Child>>,
    exited: Receiver<()>,
}

impl Child {
    fn new(mut child: std::process::Child) -> io::Result<Child> {
        cfg_if::cfg_if! {
            if #[cfg(windows)] {
                use std::os::windows::io::AsRawHandle;
                use std::sync::mpsc;

                use winapi::um::{
                    winbase::{RegisterWaitForSingleObject, INFINITE},
                    winnt::{BOOLEAN, HANDLE, PVOID, WT_EXECUTEINWAITTHREAD, WT_EXECUTEONLYONCE},
                };

                // This channel is used to simulate SIGCHLD on Windows.
                static SIGCHLD: Lazy<(mpsc::SyncSender<()>, Mutex<mpsc::Receiver<()>>)> =
                    Lazy::new(|| {
                        let (s, r) = mpsc::sync_channel(1);
                        (s, Mutex::new(r))
                    });

                // Called when a child exits.
                unsafe extern "system" fn callback(_: PVOID, _: BOOLEAN) {
                    let _ = SIGCHLD.0.try_send(());
                }

                // Register this child process to invoke `callback` on exit.
                let mut wait_object = std::ptr::null_mut();
                let ret = unsafe {
                    RegisterWaitForSingleObject(
                        &mut wait_object,
                        child.as_raw_handle() as HANDLE,
                        Some(callback),
                        std::ptr::null_mut(),
                        INFINITE,
                        WT_EXECUTEINWAITTHREAD | WT_EXECUTEONLYONCE,
                    )
                };
                if ret == 0 {
                    return Err(io::Error::last_os_error());
                }

                // Waits for the next SIGCHLD signal.
                fn wait_sigchld() {
                    let _ = SIGCHLD.1.lock().unwrap().recv();
                }

                // Wraps a sync I/O type into an async I/O type.
                fn wrap<T>(io: T) -> io::Result<Unblock<T>> {
                    Ok(Unblock::new(io))
                }

            } else if #[cfg(unix)] {
                // Waits for the next SIGCHLD signal.
                fn wait_sigchld() {
                    static SIGNALS: Lazy<signal_hook::iterator::Signals> = Lazy::new(|| {
                        signal_hook::iterator::Signals::new(&[signal_hook::SIGCHLD])
                            .expect("cannot set signal handler for SIGCHLD")
                    });
                    SIGNALS.forever().next();
                }

                // Wraps a sync I/O type into an async I/O type.
                fn wrap<T: std::os::unix::io::AsRawFd>(io: T) -> io::Result<Async<T>> {
                    Async::new(io)
                }
            }
        }

        // An entry in the list of running child processes.
        struct Entry {
            child: Arc<Mutex<std::process::Child>>,
            _exited: Sender<()>,
        }

        // The global list of running child processes.
        static CHILDREN: Lazy<Mutex<Vec<Entry>>> = Lazy::new(|| {
            // Start a thread that handles SIGCHLD and notifies tasks when child processes exit.
            thread::Builder::new()
                .name("async-process".to_string())
                .spawn(move || {
                    loop {
                        // Wait for the next SIGCHLD signal.
                        wait_sigchld();

                        // Remove processes that have exited. When an entry is removed from this
                        // `Vec`, its associated `Sender` is dropped, thus disconnecting the
                        // channel and waking up the task waiting on the `Receiver`.
                        CHILDREN.lock().unwrap().retain(|entry| {
                            let mut child = entry.child.lock().unwrap();
                            child.try_wait().expect("error waiting a child").is_none()
                        });
                    }
                })
                .expect("cannot spawn async-process thread");

            Mutex::new(Vec::new())
        });

        // Convert sync I/O types into async I/O types.
        let stdin = child.stdin.take().map(wrap).transpose()?.map(ChildStdin);
        let stdout = child.stdout.take().map(wrap).transpose()?.map(ChildStdout);
        let stderr = child.stderr.take().map(wrap).transpose()?.map(ChildStderr);

        // Register the child process in the global list.
        let child = Arc::new(Mutex::new(child));
        let (sender, exited) = async_channel::bounded(1);
        CHILDREN.lock().unwrap().push(Entry {
            child: child.clone(),
            _exited: sender,
        });

        Ok(Child {
            stdin,
            stdout,
            stderr,
            child,
            exited,
        })
    }

    pub fn id(&self) -> u32 {
        self.child.lock().unwrap().id()
    }

    pub fn kill(&mut self) -> io::Result<()> {
        self.child.lock().unwrap().kill()
    }

    // NOTE: unlike status(), does not drop stdin
    pub fn try_status(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.lock().unwrap().try_wait()
    }

    // NOTE: drops stdin
    pub fn status(&mut self) -> impl Future<Output = io::Result<ExitStatus>> {
        self.stdin.take();
        let child = self.child.clone();
        let exited = self.exited.clone();

        async move {
            let _ = exited.recv().await;
            child.lock().unwrap().wait()
        }
    }

    // NOTE: this closes stdin and drains stdout+stderr
    pub fn output(mut self) -> impl Future<Output = io::Result<Output>> {
        let status = self.status();

        let stdout = self.stdout.take();
        let stdout = async move {
            let mut v = Vec::new();
            if let Some(mut s) = stdout {
                s.read_to_end(&mut v).await?;
            }
            Ok(v)
        };

        let stderr = self.stderr.take();
        let stderr = async move {
            let mut v = Vec::new();
            if let Some(mut s) = stderr {
                s.read_to_end(&mut v).await?;
            }
            Ok(v)
        };

        async move {
            let (status, (stdout, stderr)) =
                future::try_join(status, future::try_join(stdout, stderr)).await?;
            Ok(Output {
                status,
                stdout,
                stderr,
            })
        }
    }
}

pub struct ChildStdin(
    #[cfg(windows)] Unblock<std::process::ChildStdin>,
    #[cfg(unix)] Async<std::process::ChildStdin>,
);

impl AsyncWrite for ChildStdin {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_close(cx)
    }
}

pub struct ChildStdout(
    #[cfg(windows)] Unblock<std::process::ChildStdout>,
    #[cfg(unix)] Async<std::process::ChildStdout>,
);

impl AsyncRead for ChildStdout {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

pub struct ChildStderr(
    #[cfg(windows)] Unblock<std::process::ChildStderr>,
    #[cfg(unix)] Async<std::process::ChildStderr>,
);

impl AsyncRead for ChildStderr {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

pub struct Command(std::process::Command);

impl Command {
    pub fn new<S: AsRef<OsStr>>(program: S) -> Command {
        Command(std::process::Command::new(program))
    }

    pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Command {
        self.0.arg(arg);
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.0.args(args);
        self
    }

    pub fn env<K, V>(&mut self, key: K, val: V) -> &mut Command
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.0.env(key, val);
        self
    }

    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Command
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.0.envs(vars);
        self
    }

    pub fn env_remove<K: AsRef<OsStr>>(&mut self, key: K) -> &mut Command {
        self.0.env_remove(key);
        self
    }

    pub fn env_clear(&mut self) -> &mut Command {
        self.0.env_clear();
        self
    }

    pub fn current_dir<P: AsRef<Path>>(&mut self, dir: P) -> &mut Command {
        self.0.current_dir(dir);
        self
    }

    pub fn stdin<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Command {
        self.0.stdin(cfg);
        self
    }

    pub fn stdout<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Command {
        self.0.stdout(cfg);
        self
    }

    pub fn stderr<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Command {
        self.0.stderr(cfg);
        self
    }

    pub fn spawn(&mut self) -> io::Result<Child> {
        Child::new(self.0.spawn()?)
    }

    pub fn status(&mut self) -> impl Future<Output = io::Result<ExitStatus>> {
        let child = self.spawn();
        async { child?.status().await }
    }

    pub fn output(&mut self) -> impl Future<Output = io::Result<Output>> {
        self.0.stdout(Stdio::piped());
        self.0.stderr(Stdio::piped());
        let child = self.spawn();
        async { child?.output().await }
    }
}
