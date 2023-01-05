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
//! However, note that you can reap zombie processes without spawning the "async-process" thread
//! by calling the [`cleanup_zombies`] method. The "async-process" thread is therefore only spawned
//! if no other thread calls [`cleanup_zombies`].
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
//! use futures_lite::{io::BufReader, prelude::*};
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

#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]

use std::ffi::OsStr;
use std::fmt;
use std::mem;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::thread;

#[cfg(unix)]
use async_io::Async;
#[cfg(all(not(async_process_no_io_safety), unix))]
use std::convert::{TryFrom, TryInto};
#[cfg(all(not(async_process_no_io_safety), unix))]
use std::os::unix::io::{AsFd, BorrowedFd, OwnedFd};
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, RawFd};

#[cfg(windows)]
use blocking::Unblock;

use async_lock::{Mutex as AsyncMutex, MutexGuard, OnceCell};
use event_listener::Event;
use futures_lite::{future, io, prelude::*};

#[doc(no_inline)]
pub use std::process::{ExitStatus, Output, Stdio};

#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

mod sealed {
    pub trait Sealed {}
}

/// The reaper that cleans up "zombie" processes.
struct Reaper {
    /// The event that is signalled every time the SIGCHLD signal occurs.
    sigchld: Event,

    /// The list of "zombie" processes that have exited but have not been waited for.
    zombies: Mutex<Vec<std::process::Child>>,

    /// A pipe that signals the "async-process" thread when a new process is spawned.
    pipe: Pipe,

    /// The guard used to poll this reactor.
    ///
    /// This is used to ensure that the reactor is only polled by a single thread at a time.
    polling_guard: AsyncMutex<()>,
}

impl Reaper {
    /// Get a reference to the global reactor.
    fn get() -> &'static Reaper {
        static REACTOR: OnceCell<Reaper> = OnceCell::new();

        REACTOR.get_or_init_blocking(|| {
            let sigchld = Event::new();
            let pipe = Pipe::new().expect("cannot set signal handler for SIGCHLD");

            Reaper {
                sigchld,
                zombies: Mutex::new(Vec::new()),
                pipe,
                polling_guard: AsyncMutex::new(()),
            }
        })
    }

    /// Register a child process in the reactor.
    fn register(&self, child: &std::process::Child) -> io::Result<()> {
        self.pipe.register(child)
    }

    /// Push a new "zombie" process into the reactor.
    fn push_zombie(&self, mut child: std::process::Child) {
        let mut zombies = self.zombies.lock().unwrap();

        // If the child process has already exited, then we don't need to push it into the list of zombies.
        if let Ok(None) = child.try_wait() {
            zombies.push(child);
        }
    }

    /// Poll the reactor for "zombie" processes.
    async fn reap(&self, _guard: MutexGuard<'_, ()>) -> ! {
        loop {
            // Wait for the next SIGCHLD signal.
            self.pipe.wait().await;

            // Notify all listeners waiting on the SIGCHLD event.
            self.sigchld.notify(std::usize::MAX);

            // Take out the list of zombie processes.
            let mut zombies = {
                let mut zombies_lock = self.zombies.lock().unwrap();
                if zombies_lock.is_empty() {
                    continue;
                }
                mem::take(&mut *zombies_lock)
            };

            // Poll the zombie processes to see if they are finished.
            let mut i = 0;
            'poll_zombies: loop {
                // Only poll a set number of zombies at a time to avoid starvation.
                for _ in 0..100 {
                    // Get the zombie process.
                    let zombie = match zombies.get_mut(i) {
                        Some(zombie) => zombie,
                        None => break 'poll_zombies,
                    };

                    // Try to wait on it. If it's done, remove it from the list.
                    if let Ok(None) = zombie.try_wait() {
                        i += 1;
                    } else {
                        zombies.swap_remove(i);
                    }
                }

                // Yield to avoid starving other tasks.
                future::yield_now().await;
            }

            // Put the list of zombie processes back.
            let mut zombies_lock = self.zombies.lock().unwrap();
            let mut new_zombies = mem::replace(&mut *zombies_lock, zombies);

            // If any new zombies have been added, append them to the list.
            if !new_zombies.is_empty() {
                zombies_lock.append(&mut new_zombies);
            }
        }
    }

    /// Spawn a backup thread to poll the reactor if it isn't being driven already.
    fn driven() -> &'static Self {
        let this = Reaper::get();

        // Check to see if no one else is polling the reactor.
        if let Some(guard) = this.polling_guard.try_lock() {
            // If no one else is polling the reactor, then spawn a backup thread to poll it.
            thread::Builder::new()
                .name("async-process".to_string())
                .spawn(move || {
                    #[cfg(unix)]
                    async_io::block_on(this.reap(guard));

                    #[cfg(windows)]
                    future::block_on(this.reap(guard));
                })
                .expect("cannot spawn async-process thread");
        }

        this
    }
}

cfg_if::cfg_if! {
    if #[cfg(windows)] {
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// A callback that simulates a SIGCHLD signal on Windows.
        struct Pipe {
            /// The event to signal when a child process completes.
            signal: Event,

            /// The number of child processes we've seen complete but haven't acted on yet.
            complete: AtomicUsize,
        }

        impl Pipe {
            /// Create a new pipe.
            fn new() -> io::Result<Self> {
                Ok(Pipe {
                    signal: Event::new(),
                    complete: AtomicUsize::new(0),
                })
            }

            /// Waits for the SIGCHLD signal.
            async fn wait(&self) {
                let mut completed = self.complete.load(Ordering::Acquire);

                loop {
                    // If there's already a completed process, decrement and return.
                    if completed > 0 {
                        if let Err(actual) =
                            self.complete.compare_exchange(completed, completed - 1, Ordering::SeqCst, Ordering::SeqCst) {
                            completed = actual;
                            continue;
                        }

                        // Finish.
                        return;
                    } else {
                        // Register a listener for the next completed process.
                        let listener = self.signal.listen();

                        // See if there's a completed process now.
                        completed = self.complete.load(Ordering::Acquire);
                        if completed > 0 {
                            // There is, so we can drop the listener.
                            drop(listener);
                            continue;
                        }

                        // Wait for the next completed process.
                        listener.await;

                        // Loop around and try again.
                        completed = self.complete.load(Ordering::Acquire);
                    }
                }
            }

            /// Registers a child process in this pipe.
            fn register(&self, child: &std::process::Child) -> io::Result<()> {
                use std::ffi::c_void;
                use std::os::windows::io::AsRawHandle;

                use windows_sys::Win32::{
                    System::{
                        Threading::{RegisterWaitForSingleObject, WT_EXECUTEINWAITTHREAD, WT_EXECUTEONLYONCE},
                        WindowsProgramming::INFINITE,
                    },
                    Foundation::{BOOLEAN, HANDLE},
                };

                // Called when a child exits.
                unsafe extern "system" fn callback(_: *mut c_void, _: BOOLEAN) {
                    // A panic here would unwind into Win32, causing undefined behavior.
                    abort_on_panic(|| {
                        // Get the global pipe.
                        let pipe = &Reaper::get().pipe;

                        // Increment the number of completed processes.
                        pipe.complete.fetch_add(1, Ordering::SeqCst);

                        // Signal the SIGCHLD event.
                        pipe.signal.notify_additional(std::usize::MAX);
                    });
                }

                // Register this child process to invoke `callback` on exit.
                let mut wait_object = 0;
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
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            }
        }

        /// Abort if the given closure panics.
        fn abort_on_panic<R>(f: impl FnOnce() -> R) -> R {
            struct Bomb;

            impl Drop for Bomb {
                fn drop(&mut self) {
                    std::process::abort();
                }
            }

            let bomb = Bomb;
            let r = f();
            mem::forget(bomb);
            r
        }
    } else if #[cfg(unix)] {
        use std::os::unix::net::UnixStream;

        /// A pipe that waits on the `SIGCHLD` signal.
        struct Pipe {
            /// The read end of the pipe.
            reader: Async<UnixStream>,
        }

        impl Pipe {
            /// Create a new pipe.
            fn new() -> io::Result<Self> {
                // Create a pipe.
                let (reader, writer) = UnixStream::pair()?;

                // Register the writer end of the pipe to be signalled on SIGCHLD.
                signal_hook::low_level::pipe::register(
                    signal_hook::consts::SIGCHLD,
                    writer
                )?;

                // Register the reader end of the pipe into the `async-io` reactor.
                Async::new(reader).map(|reader| Pipe { reader })
            }

            /// Wait for the next `SIGCHLD` signal.
            async fn wait(&self) {
                // Wait for anything to be written to the pipe.
                let mut buf = [0; 1];
                (&self.reader).read_exact(&mut buf).await.ok();
            }

            /// Registers a child process in this pipe.
            fn register(&self, _child: &std::process::Child) -> io::Result<()> {
                // Do nothing.
                Ok(())
            }
        }
    }
}

/// Run the zombie process cleanup routine along with the provided future.
///
/// This avoids the need to spawn the "async-process" thread in order to reap zombie processes.
/// This waits for the `SIGCHLD` signal, and then reaps zombie processes. Only one thread can
/// wait on the `SIGCHLD` signal at a time, so if another thread is already waiting on the
/// signal, then it will just run the future without reaping zombie processes.
///
/// # Example
///
/// ```no_run
/// use async_process::{Command, cleanup_zombies};
///
/// # futures_lite::future::block_on(async {
/// // This will not spawn the "async-process" thread.
/// cleanup_zombies(async {
///     let out = Command::new("echo").arg("hello").arg("world").output().await?;
///     assert_eq!(out.stdout, b"hello world\n");
///     std::io::Result::Ok(())
/// }).await?;
/// # std::io::Result::Ok(()) });
/// ```
///
/// If you are using the [`async-executor`] crate, then you may want to spawn `cleanup_zombies`
/// as its own detached task.
///
/// [`async-executor`]: https://docs.rs/async-executor
///
/// ```no_run
/// use async_executor::Executor;
/// use async_process::{Command, cleanup_zombies};
/// use std::future::pending;
///
/// # futures_lite::future::block_on(async {
/// let executor = Executor::new();
///
/// // Spawn `cleanup_zombies` as a task.
/// let task = executor.spawn(cleanup_zombies(pending::<()>()));
/// task.detach();
///
/// // Run the executor with your own future.
/// executor.run(async {
///     let out = Command::new("echo").arg("hello").arg("world").output().await?;
///     assert_eq!(out.stdout, b"hello world\n");
///     std::io::Result::Ok(())
/// }).await?;
/// # std::io::Result::Ok(()) });
/// ```
pub async fn cleanup_zombies<R>(f: impl Future<Output = R>) -> R {
    // A future that cleans up zombie processes.
    let cleanup = async {
        // Acquire a lock on the reaper.
        let guard = Reaper::get().polling_guard.lock().await;

        // Poll the reaper.
        Reaper::get().reap(guard).await
    };

    // Run these futures in parallel.
    future::or(f, cleanup).await
}

/// A guard that can kill child processes, or push them into the zombie list.
struct ChildGuard {
    inner: Option<std::process::Child>,
    reap_on_drop: bool,
    kill_on_drop: bool,
}

impl ChildGuard {
    fn get_mut(&mut self) -> &mut std::process::Child {
        self.inner.as_mut().unwrap()
    }
}

// When the last reference to the child process is dropped, push it into the zombie list.
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.kill_on_drop {
            self.get_mut().kill().ok();
        }
        if self.reap_on_drop {
            Reaper::driven().push_zombie(self.inner.take().unwrap());
        }
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
    fn new(cmd: &mut Command) -> io::Result<Child> {
        let mut child = cmd.inner.spawn()?;

        // Convert sync I/O types into async I/O types.
        let stdin = child.stdin.take().map(wrap).transpose()?.map(ChildStdin);
        let stdout = child.stdout.take().map(wrap).transpose()?.map(ChildStdout);
        let stderr = child.stderr.take().map(wrap).transpose()?.map(ChildStderr);

        cfg_if::cfg_if! {
            if #[cfg(windows)] {
                // Wraps a sync I/O type into an async I/O type.
                fn wrap<T>(io: T) -> io::Result<Unblock<T>> {
                    Ok(Unblock::new(io))
                }

            } else if #[cfg(unix)] {
                // Wraps a sync I/O type into an async I/O type.
                fn wrap<T: std::os::unix::io::AsRawFd>(io: T) -> io::Result<Async<T>> {
                    Async::new(io)
                }
            }
        }

        // Register the child process in the global process list.
        Reaper::driven().register(&child)?;

        Ok(Child {
            stdin,
            stdout,
            stderr,
            child: Arc::new(Mutex::new(ChildGuard {
                inner: Some(child),
                reap_on_drop: cmd.reap_on_drop,
                kill_on_drop: cmd.kill_on_drop,
            })),
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
                    None => listener = Some(Reaper::driven().sigchld.listen()),
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
            let (stdout, stderr) = future::try_zip(stdout, stderr).await?;
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

impl ChildStdin {
    /// Convert async_process::ChildStdin into std::process::Stdio.
    ///
    /// You can use it to associate to the next process.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # futures_lite::future::block_on(async {
    /// use async_process::Command;
    /// use std::process::Stdio;
    ///
    /// let mut ls_child = Command::new("ls").stdin(Stdio::piped()).spawn()?;
    /// let stdio:Stdio = ls_child.stdin.take().unwrap().into_stdio().await?;
    ///
    /// let mut echo_child = Command::new("echo").arg("./").stdout(stdio).spawn()?;
    ///
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn into_stdio(self) -> io::Result<std::process::Stdio> {
        cfg_if::cfg_if! {
            if #[cfg(windows)] {
                Ok(self.0.into_inner().await.into())
            } else if #[cfg(unix)] {
                let child_stdin = self.0.into_inner()?;
                blocking_fd(rustix::fd::AsFd::as_fd(&child_stdin))?;
                Ok(child_stdin.into())
            }
        }
    }
}

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

#[cfg(unix)]
impl AsRawFd for ChildStdin {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// **Note:** This implementation is only available on Rust 1.63+.
#[cfg(all(not(async_process_no_io_safety), unix))]
impl AsFd for ChildStdin {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// **Note:** This implementation is only available on Rust 1.63+.
#[cfg(all(not(async_process_no_io_safety), unix))]
impl TryFrom<ChildStdin> for OwnedFd {
    type Error = io::Error;

    fn try_from(value: ChildStdin) -> Result<Self, Self::Error> {
        value.0.try_into()
    }
}

// TODO(notgull): Add mirroring AsRawHandle impls for all of the child handles
//
// at the moment this is pretty hard to do because of how they're wrapped in
// Unblock, meaning that we can't always access the underlying handle. async-fs
// gets around this by putting the handle in an Arc, but there's still some decision
// to be made about how to handle this (no pun intended)

/// A handle to a child process's standard output (stdout).
///
/// When a [`ChildStdout`] is dropped, the underlying handle gets closed.
#[derive(Debug)]
pub struct ChildStdout(
    #[cfg(windows)] Unblock<std::process::ChildStdout>,
    #[cfg(unix)] Async<std::process::ChildStdout>,
);

impl ChildStdout {
    /// Convert async_process::ChildStdout into std::process::Stdio.
    ///
    /// You can use it to associate to the next process.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # futures_lite::future::block_on(async {
    /// use async_process::Command;
    /// use std::process::Stdio;
    /// use std::io::Read;
    /// use futures_lite::AsyncReadExt;
    ///
    /// let mut ls_child = Command::new("ls").stdout(Stdio::piped()).spawn()?;
    /// let stdio:Stdio = ls_child.stdout.take().unwrap().into_stdio().await?;
    ///
    /// let mut echo_child = Command::new("echo").stdin(stdio).stdout(Stdio::piped()).spawn()?;
    /// let mut buf = vec![];
    /// echo_child.stdout.take().unwrap().read(&mut buf).await;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn into_stdio(self) -> io::Result<std::process::Stdio> {
        cfg_if::cfg_if! {
            if #[cfg(windows)] {
                Ok(self.0.into_inner().await.into())
            } else if #[cfg(unix)] {
                let child_stdout = self.0.into_inner()?;
                blocking_fd(rustix::fd::AsFd::as_fd(&child_stdout))?;
                Ok(child_stdout.into())
            }
        }
    }
}

impl io::AsyncRead for ChildStdout {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

#[cfg(unix)]
impl AsRawFd for ChildStdout {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// **Note:** This implementation is only available on Rust 1.63+.
#[cfg(all(not(async_process_no_io_safety), unix))]
impl AsFd for ChildStdout {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// **Note:** This implementation is only available on Rust 1.63+.
#[cfg(all(not(async_process_no_io_safety), unix))]
impl TryFrom<ChildStdout> for OwnedFd {
    type Error = io::Error;

    fn try_from(value: ChildStdout) -> Result<Self, Self::Error> {
        value.0.try_into()
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

impl ChildStderr {
    /// Convert async_process::ChildStderr into std::process::Stdio.
    ///
    /// You can use it to associate to the next process.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # futures_lite::future::block_on(async {
    /// use async_process::Command;
    /// use std::process::Stdio;
    ///
    /// let mut ls_child = Command::new("ls").arg("x").stderr(Stdio::piped()).spawn()?;
    /// let stdio:Stdio = ls_child.stderr.take().unwrap().into_stdio().await?;
    ///
    /// let mut echo_child = Command::new("echo").stdin(stdio).spawn()?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub async fn into_stdio(self) -> io::Result<std::process::Stdio> {
        cfg_if::cfg_if! {
            if #[cfg(windows)] {
                Ok(self.0.into_inner().await.into())
            } else if #[cfg(unix)] {
                let child_stderr = self.0.into_inner()?;
                blocking_fd(rustix::fd::AsFd::as_fd(&child_stderr))?;
                Ok(child_stderr.into())
            }
        }
    }
}

impl io::AsyncRead for ChildStderr {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

#[cfg(unix)]
impl AsRawFd for ChildStderr {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// **Note:** This implementation is only available on Rust 1.63+.
#[cfg(all(not(async_process_no_io_safety), unix))]
impl AsFd for ChildStderr {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// **Note:** This implementation is only available on Rust 1.63+.
#[cfg(all(not(async_process_no_io_safety), unix))]
impl TryFrom<ChildStderr> for OwnedFd {
    type Error = io::Error;

    fn try_from(value: ChildStderr) -> Result<Self, Self::Error> {
        value.0.try_into()
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
pub struct Command {
    inner: std::process::Command,
    stdin: bool,
    stdout: bool,
    stderr: bool,
    reap_on_drop: bool,
    kill_on_drop: bool,
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
        Self::from(std::process::Command::new(program))
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
        self.inner.arg(arg);
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
        self.inner.args(args);
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
        self.inner.env(key, val);
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
        self.inner.envs(vars);
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
        self.inner.env_remove(key);
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
        self.inner.env_clear();
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
        self.inner.current_dir(dir);
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
        self.stdin = true;
        self.inner.stdin(cfg);
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
        self.stdout = true;
        self.inner.stdout(cfg);
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
        self.stderr = true;
        self.inner.stderr(cfg);
        self
    }

    /// Configures whether to reap the zombie process when [`Child`] is dropped.
    ///
    /// When the process finishes, it becomes a "zombie" and some resources associated with it
    /// remain until [`Child::try_status()`], [`Child::status()`], or [`Child::output()`] collects
    /// its exit code.
    ///
    /// If its exit code is never collected, the resources may leak forever. This crate has a
    /// background thread named "async-process" that collects such "zombie" processes and then
    /// "reaps" them, thus preventing the resource leaks.
    ///
    /// The default value of this option is `true`.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_process::{Command, Stdio};
    ///
    /// let mut cmd = Command::new("cat");
    /// cmd.reap_on_drop(false);
    /// ```
    pub fn reap_on_drop(&mut self, reap_on_drop: bool) -> &mut Command {
        self.reap_on_drop = reap_on_drop;
        self
    }

    /// Configures whether to kill the process when [`Child`] is dropped.
    ///
    /// The default value of this option is `false`.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_process::{Command, Stdio};
    ///
    /// let mut cmd = Command::new("cat");
    /// cmd.kill_on_drop(true);
    /// ```
    pub fn kill_on_drop(&mut self, kill_on_drop: bool) -> &mut Command {
        self.kill_on_drop = kill_on_drop;
        self
    }

    /// Executes the command and returns the [`Child`] handle to it.
    ///
    /// If not configured, stdin, stdout and stderr will be set to [`Stdio::inherit()`].
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
        if !self.stdin {
            self.inner.stdin(Stdio::inherit());
        }
        if !self.stdout {
            self.inner.stdout(Stdio::inherit());
        }
        if !self.stderr {
            self.inner.stderr(Stdio::inherit());
        }

        Child::new(self)
    }

    /// Executes the command, waits for it to exit, and returns the exit status.
    ///
    /// If not configured, stdin, stdout and stderr will be set to [`Stdio::inherit()`].
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
    /// # Examples
    ///
    /// ```no_run
    /// # futures_lite::future::block_on(async {
    /// use async_process::Command;
    ///
    /// let output = Command::new("cat")
    ///     .arg("a.txt")
    ///     .output()
    ///     .await?;
    /// # std::io::Result::Ok(()) });
    /// ```
    pub fn output(&mut self) -> impl Future<Output = io::Result<Output>> {
        if !self.stdin {
            self.inner.stdin(Stdio::null());
        }
        if !self.stdout {
            self.inner.stdout(Stdio::piped());
        }
        if !self.stderr {
            self.inner.stderr(Stdio::piped());
        }

        let child = Child::new(self);
        async { child?.output().await }
    }
}

impl From<std::process::Command> for Command {
    fn from(inner: std::process::Command) -> Self {
        Self {
            inner,
            stdin: false,
            stdout: false,
            stderr: false,
            reap_on_drop: true,
            kill_on_drop: false,
        }
    }
}

impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if f.alternate() {
            f.debug_struct("Command")
                .field("inner", &self.inner)
                .field("stdin", &self.stdin)
                .field("stdout", &self.stdout)
                .field("stderr", &self.stderr)
                .field("reap_on_drop", &self.reap_on_drop)
                .field("kill_on_drop", &self.kill_on_drop)
                .finish()
        } else {
            // Stdlib outputs command-line in Debug for Command. This does the
            // same, if not in "alternate" (long pretty-printed) mode.
            // This is useful for logs, for example.
            fmt::Debug::fmt(&self.inner, f)
        }
    }
}

/// Moves `Fd` out of non-blocking mode.
#[cfg(unix)]
fn blocking_fd(fd: rustix::fd::BorrowedFd<'_>) -> io::Result<()> {
    cfg_if::cfg_if! {
        // ioctl(FIONBIO) sets the flag atomically, but we use this only on Linux
        // for now, as with the standard library, because it seems to behave
        // differently depending on the platform.
        // https://github.com/rust-lang/rust/commit/efeb42be2837842d1beb47b51bb693c7474aba3d
        // https://github.com/libuv/libuv/blob/e9d91fccfc3e5ff772d5da90e1c4a24061198ca0/src/unix/poll.c#L78-L80
        // https://github.com/tokio-rs/mio/commit/0db49f6d5caf54b12176821363d154384357e70a
        if #[cfg(target_os = "linux")] {
            rustix::io::ioctl_fionbio(fd, false)?;
        } else {
            let previous = rustix::fs::fcntl_getfl(fd)?;
            let new = previous & !rustix::fs::OFlags::NONBLOCK;
            if new != previous {
                rustix::fs::fcntl_setfl(fd, new)?;
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
mod test {

    #[test]
    fn test_into_inner() {
        futures_lite::future::block_on(async {
            use crate::Command;

            use std::io::Result;
            use std::process::Stdio;
            use std::str::from_utf8;

            use futures_lite::AsyncReadExt;

            let mut ls_child = Command::new("cat")
                .arg("Cargo.toml")
                .stdout(Stdio::piped())
                .spawn()?;

            let stdio: Stdio = ls_child.stdout.take().unwrap().into_stdio().await?;

            let mut echo_child = Command::new("grep")
                .arg("async")
                .stdin(stdio)
                .stdout(Stdio::piped())
                .spawn()?;

            let mut buf = vec![];
            let mut stdout = echo_child.stdout.take().unwrap();

            stdout.read_to_end(&mut buf).await?;
            dbg!(from_utf8(&buf).unwrap_or(""));

            Result::Ok(())
        })
        .unwrap();
    }
}
