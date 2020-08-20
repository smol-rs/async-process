//! Async interface for working with processes.
//!
//! This crate is an async version of [`std::process`].
//!
//! # Implementation
//!
//! A background thread named "async-process" is lazily created on first use, which waits for
//! spawned child processes to exit and then calls the `wait()` syscall to clean up the "zombie"
//! processes. This is unlike the `process` API in the standard library, where dropping a running
//! `Child` leaks its resources.
//!
//! This crate uses [`async-io`] for async I/O on Unix-like systems and [`blocking`] for async I/O
//! on Windows.
//!
//! [`async-io`]: https://docs.rs/async-io
//! [`blocking`]: https://docs.rs/blocking
//!
//! # Examples
//!
//! Spawn a process and collect its output:
//!
//! ```no_run
//! # futures_lite::future::block_on(async {
//! use async_process::Command;
//!
//! let out = Command::new("echo").arg("hello").arg("world").output().await?;
//! assert_eq!(out.stdout, b"hello world\n");
//! # std::io::Result::Ok(()) });
//! ```
//!
//! Read the output line-by-line as it gets produced:
//!
//! ```no_run
//! # futures_lite::future::block_on(async {
//! use async_process::{Command, Stdio};
//! use futures_lite::{AsyncBufReadExt, StreamExt, io::BufReader};
//!
//! let mut child = Command::new("find")
//!     .arg(".")
//!     .stdout(Stdio::piped())
//!     .spawn()?;
//!
//! let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
//!
//! while let Some(line) = lines.next().await {
//!     println!("{}", line?);
//! }
//! # std::io::Result::Ok(()) });
//! ```

#![cfg_attr(unix, forbid(unsafe_code))]
#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]

use std::ffi::OsStr;
use std::fmt;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::thread;

#[cfg(unix)]
use async_io::Async;
#[cfg(windows)]
use blocking::Unblock;
use event_listener::Event;
use futures_lite::{future, io, AsyncReadExt};
use once_cell::sync::Lazy;

#[doc(no_inline)]
pub use std::process::{ExitStatus, Output, Stdio};

/// An event delivered every time the SIGCHLD signal occurs.
static SIGCHLD: Event = Event::new();

/// A guard that pushes abandoned child processes into the zombie list.
struct ChildGuard(Option<std::process::Child>);

impl ChildGuard {
    fn get_mut(&mut self) -> &mut std::process::Child {
        self.0.as_mut().unwrap()
    }
}

/// A spawned child process.
///
/// The process can be in running or exited state. Use [`status()`][`Child::status()`] or
/// [`output()`][`Child::output()`] to wait for it to exit.
///
/// If the [`Child`] is dropped, the process keeps running in the background.
///
/// # Examples
///
/// Spawn a process and wait for it to complete:
///
/// ```no_run
/// # futures_lite::future::block_on(async {
/// use async_process::Command;
///
/// Command::new("cp").arg("a.txt").arg("b.txt").status().await?;
/// # std::io::Result::Ok(()) });
/// ```
pub struct Child {
    /// The handle for writing to the child's standard input (stdin), if it has been captured.
    pub stdin: Option<ChildStdin>,

    /// The handle for reading from the child's standard output (stdout), if it has been captured.
    pub stdout: Option<ChildStdout>,

    /// The handle for reading from the child's standard error (stderr), if it has been captured.
    pub stderr: Option<ChildStderr>,

    /// The inner child process handle.
    child: Arc<Mutex<ChildGuard>>,
}

impl Child {
    /// Wraps the inner child process handle and registers it in the global process list.
    ///
    /// The "async-process" thread waits for processes in the global list and cleans up the
    /// resources when they exit.
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
                static CALLBACK: Lazy<(mpsc::SyncSender<()>, Mutex<mpsc::Receiver<()>>)> =
                    Lazy::new(|| {
                        let (s, r) = mpsc::sync_channel(1);
                        (s, Mutex::new(r))
                    });

                // Called when a child exits.
                unsafe extern "system" fn callback(_: PVOID, _: BOOLEAN) {
                    let _ = CALLBACK.0.try_send(());
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
                    let _ = CALLBACK.1.lock().unwrap().recv();
                }

                // Wraps a sync I/O type into an async I/O type.
                fn wrap<T>(io: T) -> io::Result<Unblock<T>> {
                    Ok(Unblock::new(io))
                }

            } else if #[cfg(unix)] {
                static SIGNALS: Lazy<signal_hook::iterator::Signals> = Lazy::new(|| {
                    signal_hook::iterator::Signals::new(&[signal_hook::SIGCHLD])
                        .expect("cannot set signal handler for SIGCHLD")
                });

                // Make sure the signal handler is registered before interacting with the process.
                Lazy::force(&SIGNALS);

                // Waits for the next SIGCHLD signal.
                fn wait_sigchld() {
                    SIGNALS.forever().next();
                }

                // Wraps a sync I/O type into an async I/O type.
                fn wrap<T: std::os::unix::io::AsRawFd>(io: T) -> io::Result<Async<T>> {
                    Async::new(io)
                }
            }
        }

        static ZOMBIES: Lazy<Mutex<Vec<std::process::Child>>> = Lazy::new(|| {
            // Start a thread that handles SIGCHLD and notifies tasks when child processes exit.
            thread::Builder::new()
                .name("async-process".to_string())
                .spawn(move || {
                    loop {
                        // Wait for the next SIGCHLD signal.
                        wait_sigchld();

                        // Notify all listeners waiting on the SIGCHLD event.
                        SIGCHLD.notify(std::usize::MAX);

                        // Reap zombie processes.
                        let mut zombies = ZOMBIES.lock().unwrap();
                        let mut i = 0;
                        while i < zombies.len() {
                            if let Ok(None) = zombies[i].try_wait() {
                                i += 1;
                            } else {
                                zombies.swap_remove(i);
                            }
                        }
                    }
                })
                .expect("cannot spawn async-process thread");

            Mutex::new(Vec::new())
        });

        // Make sure the thread is started.
        Lazy::force(&ZOMBIES);

        // When the last reference to the child process is dropped, push it into the zombie list.
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let mut zombies = ZOMBIES.lock().unwrap();
                if let Ok(None) = self.get_mut().try_wait() {
                    zombies.push(self.0.take().unwrap());
                }
            }
        }

        // Convert sync I/O types into async I/O types.
        let stdin = child.stdin.take().map(wrap).transpose()?.map(ChildStdin);
        let stdout = child.stdout.take().map(wrap).transpose()?.map(ChildStdout);
        let stderr = child.stderr.take().map(wrap).transpose()?.map(ChildStderr);

        Ok(Child {
            stdin,
            stdout,
            stderr,
            child: Arc::new(Mutex::new(ChildGuard(Some(child)))),
        })
    }

    /// Returns the OS-assigned process identifier associated with this child.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # futures_lite::future::block_on(async {
    /// use async_process::Command;
    ///
    /// let mut child = Command::new("ls").spawn()?;
    /// println!("id: {}", child.id());
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn id(&self) -> u32 {
        self.child.lock().unwrap().get_mut().id()
    }

    /// Forces the child process to exit.
    ///
    /// If the child has already exited, an [`InvalidInput`] error is returned.
    ///
    /// This is equivalent to sending a SIGKILL on Unix platforms.
    ///
    /// [`InvalidInput`]: `std::io::ErrorKind::InvalidInput`
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # futures_lite::future::block_on(async {
    /// use async_process::Command;
    ///
    /// let mut child = Command::new("yes").spawn()?;
    /// child.kill()?;
    /// println!("exit status: {}", child.status().await?);
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn kill(&mut self) -> io::Result<()> {
        self.child.lock().unwrap().get_mut().kill()
    }

    /// Returns the exit status if the process has exited.
    ///
    /// Unlike [`status()`][`Child::status()`], this method will not drop the stdin handle.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # futures_lite::future::block_on(async {
    /// use async_process::Command;
    ///
    /// let mut child = Command::new("ls").spawn()?;
    ///
    /// match child.try_status()? {
    ///     None => println!("still running"),
    ///     Some(status) => println!("exited with: {}", status),
    /// }
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn try_status(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.lock().unwrap().get_mut().try_wait()
    }

    /// Drops the stdin handle and waits for the process to exit.
    ///
    /// Closing the stdin of the process helps avoid deadlocks. It ensures that the process does
    /// not block waiting for input from the parent process while the parent waits for the child to
    /// exit.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # futures_lite::future::block_on(async {
    /// use async_process::{Command, Stdio};
    ///
    /// let mut child = Command::new("cp")
    ///     .arg("a.txt")
    ///     .arg("b.txt")
    ///     .spawn()?;
    ///
    /// println!("exit status: {}", child.status().await?);
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn status(&mut self) -> impl Future<Output = io::Result<ExitStatus>> {
        self.stdin.take();
        let child = self.child.clone();

        async move {
            let mut listener = None;
            loop {
                if let Some(status) = child.lock().unwrap().get_mut().try_wait()? {
                    return Ok(status);
                }
                match listener.take() {
                    None => listener = Some(SIGCHLD.listen()),
                    Some(listener) => listener.await,
                }
            }
        }
    }

    /// Drops the stdin handle and collects the output of the process.
    ///
    /// Closing the stdin of the process helps avoid deadlocks. It ensures that the process does
    /// not block waiting for input from the parent process while the parent waits for the child to
    /// exit.
    ///
    /// In order to capture the output of the process, [`Command::stdout()`] and
    /// [`Command::stderr()`] must be configured with [`Stdio::piped()`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # futures_lite::future::block_on(async {
    /// use async_process::{Command, Stdio};
    ///
    /// let child = Command::new("ls")
    ///     .stdout(Stdio::piped())
    ///     .stderr(Stdio::piped())
    ///     .spawn()?;
    ///
    /// let out = child.output().await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn output(mut self) -> impl Future<Output = io::Result<Output>> {
        // A future that waits for the exit status.
        let status = self.status();

        // A future that collects stdout.
        let stdout = self.stdout.take();
        let stdout = async move {
            let mut v = Vec::new();
            if let Some(mut s) = stdout {
                s.read_to_end(&mut v).await?;
            }
            io::Result::Ok(v)
        };

        // A future that collects stderr.
        let stderr = self.stderr.take();
        let stderr = async move {
            let mut v = Vec::new();
            if let Some(mut s) = stderr {
                s.read_to_end(&mut v).await?;
            }
            io::Result::Ok(v)
        };

        async move {
            let (stdout, stderr) = future::try_join(stdout, stderr).await?;
            let status = status.await?;
            Ok(Output {
                status,
                stdout,
                stderr,
            })
        }
    }
}

impl fmt::Debug for Child {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Child")
            .field("stdin", &self.stdin)
            .field("stdout", &self.stdout)
            .field("stderr", &self.stderr)
            .finish()
    }
}

/// A handle to a child process's standard input (stdin).
///
/// When a [`ChildStdin`] is dropped, the underlying handle gets clossed. If the child process was
/// previously blocked on input, it becomes unblocked after dropping.
#[derive(Debug)]
pub struct ChildStdin(
    #[cfg(windows)] Unblock<std::process::ChildStdin>,
    #[cfg(unix)] Async<std::process::ChildStdin>,
);

impl io::AsyncWrite for ChildStdin {
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

/// A handle to a child process's standard output (stdout).
///
/// When a [`ChildStdout`] is dropped, the underlying handle gets closed.
#[derive(Debug)]
pub struct ChildStdout(
    #[cfg(windows)] Unblock<std::process::ChildStdout>,
    #[cfg(unix)] Async<std::process::ChildStdout>,
);

impl io::AsyncRead for ChildStdout {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

/// A handle to a child process's standard error (stderr).
///
/// When a [`ChildStderr`] is dropped, the underlying handle gets closed.
#[derive(Debug)]
pub struct ChildStderr(
    #[cfg(windows)] Unblock<std::process::ChildStderr>,
    #[cfg(unix)] Async<std::process::ChildStderr>,
);

impl io::AsyncRead for ChildStderr {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

/// A builder for spawning processes.
///
/// # Examples
///
/// ```no_run
/// # futures_lite::future::block_on(async {
/// use async_process::Command;
///
/// let output = if cfg!(target_os = "windows") {
///     Command::new("cmd").args(&["/C", "echo hello"]).output().await?
/// } else {
///     Command::new("sh").arg("-c").arg("echo hello").output().await?
/// };
/// # std::io::Result::Ok(()) });
/// ```
#[derive(Debug)]
pub struct Command {
    cmd: std::process::Command,
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
}

impl Command {
    /// Constructs a new [`Command`] for launching `program`.
    ///
    /// The initial configuration (the working directory and environment variables) is inherited
    /// from the current process.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_process::Command;
    ///
    /// let mut cmd = Command::new("ls");
    /// ```
    pub fn new<S: AsRef<OsStr>>(program: S) -> Command {
        Command {
            cmd: std::process::Command::new(program),
            stdin: None,
            stdout: None,
            stderr: None,
        }
    }

    /// Adds a single argument to pass to the program.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_process::Command;
    ///
    /// let mut cmd = Command::new("echo");
    /// cmd.arg("hello");
    /// cmd.arg("world");
    /// ```
    pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Command {
        self.cmd.arg(arg);
        self
    }

    /// Adds multiple arguments to pass to the program.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_process::Command;
    ///
    /// let mut cmd = Command::new("echo");
    /// cmd.args(&["hello", "world"]);
    /// ```
    pub fn args<I, S>(&mut self, args: I) -> &mut Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.cmd.args(args);
        self
    }

    /// Configures an environment variable for the new process.
    ///
    /// Note that environment variable names are case-insensitive (but case-preserving) on Windows,
    /// and case-sensitive on all other platforms.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_process::Command;
    ///
    /// let mut cmd = Command::new("ls");
    /// cmd.env("PATH", "/bin");
    /// ```
    pub fn env<K, V>(&mut self, key: K, val: V) -> &mut Command
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.cmd.env(key, val);
        self
    }

    /// Configures multiple environment variables for the new process.
    ///
    /// Note that environment variable names are case-insensitive (but case-preserving) on Windows,
    /// and case-sensitive on all other platforms.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_process::Command;
    ///
    /// let mut cmd = Command::new("ls");
    /// cmd.envs(vec![("PATH", "/bin"), ("TERM", "xterm-256color")]);
    /// ```
    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Command
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.cmd.envs(vars);
        self
    }

    /// Removes an environment variable mapping.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_process::Command;
    ///
    /// let mut cmd = Command::new("ls");
    /// cmd.env_remove("PATH");
    /// ```
    pub fn env_remove<K: AsRef<OsStr>>(&mut self, key: K) -> &mut Command {
        self.cmd.env_remove(key);
        self
    }

    /// Removes all environment variable mappings.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_process::Command;
    ///
    /// let mut cmd = Command::new("ls");
    /// cmd.env_clear();
    /// ```
    pub fn env_clear(&mut self) -> &mut Command {
        self.cmd.env_clear();
        self
    }

    /// Configures the working directory for the new process.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_process::Command;
    ///
    /// let mut cmd = Command::new("ls");
    /// cmd.current_dir("/");
    /// ```
    pub fn current_dir<P: AsRef<Path>>(&mut self, dir: P) -> &mut Command {
        self.cmd.current_dir(dir);
        self
    }

    /// Configures the standard input (stdin) for the new process.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_process::{Command, Stdio};
    ///
    /// let mut cmd = Command::new("cat");
    /// cmd.stdin(Stdio::null());
    /// ```
    pub fn stdin<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Command {
        self.stdin = Some(cfg.into());
        self
    }

    /// Configures the standard output (stdout) for the new process.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_process::{Command, Stdio};
    ///
    /// let mut cmd = Command::new("ls");
    /// cmd.stdout(Stdio::piped());
    /// ```
    pub fn stdout<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Command {
        self.stdout = Some(cfg.into());
        self
    }

    /// Configures the standard error (stderr) for the new process.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_process::{Command, Stdio};
    ///
    /// let mut cmd = Command::new("ls");
    /// cmd.stderr(Stdio::piped());
    /// ```
    pub fn stderr<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Command {
        self.stderr = Some(cfg.into());
        self
    }

    /// Executes the command and returns the [`Child`] handle to it.
    ///
    /// If not configured, stdin, stdout and stderr will be set to [`Stdio::inherit()`].
    ///
    /// After spawning the process, stdin, stdout, and stderr become unconfigured again.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # futures_lite::future::block_on(async {
    /// use async_process::Command;
    ///
    /// let child = Command::new("ls").spawn()?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn spawn(&mut self) -> io::Result<Child> {
        let (stdin, stdout, stderr) = (self.stdin.take(), self.stdout.take(), self.stderr.take());
        self.cmd.stdin(stdin.unwrap_or(Stdio::inherit()));
        self.cmd.stdout(stdout.unwrap_or(Stdio::inherit()));
        self.cmd.stderr(stderr.unwrap_or(Stdio::inherit()));

        Child::new(self.cmd.spawn()?)
    }

    /// Executes the command, waits for it to exit, and returns the exit status.
    ///
    /// If not configured, stdin, stdout and stderr will be set to [`Stdio::inherit()`].
    ///
    /// After spawning the process, stdin, stdout, and stderr become unconfigured again.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # futures_lite::future::block_on(async {
    /// use async_process::Command;
    ///
    /// let status = Command::new("cp")
    ///     .arg("a.txt")
    ///     .arg("b.txt")
    ///     .status()
    ///     .await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn status(&mut self) -> impl Future<Output = io::Result<ExitStatus>> {
        let child = self.spawn();
        async { child?.status().await }
    }

    /// Executes the command and collects its output.
    ///
    /// If not configured, stdin will be set to [`Stdio::null()`], and stdout and stderr will be
    /// set to [`Stdio::piped()`].
    ///
    /// After spawning the process, stdin, stdout, and stderr become unconfigured again.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # futures_lite::future::block_on(async {
    /// use async_process::Command;
    ///
    /// let child = Command::new("ls").spawn()?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn output(&mut self) -> impl Future<Output = io::Result<Output>> {
        let (stdin, stdout, stderr) = (self.stdin.take(), self.stdout.take(), self.stderr.take());
        self.cmd.stdin(stdin.unwrap_or(Stdio::null()));
        self.cmd.stdout(stdout.unwrap_or(Stdio::piped()));
        self.cmd.stderr(stderr.unwrap_or(Stdio::piped()));

        let child = self.cmd.spawn();
        async { Child::new(child?)?.output().await }
    }
}
