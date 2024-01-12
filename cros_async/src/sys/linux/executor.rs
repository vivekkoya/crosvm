// Copyright 2020 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use base::debug;
use base::warn;
use base::AsRawDescriptors;
use base::RawDescriptor;
use once_cell::sync::OnceCell;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error as ThisError;

use super::fd_executor::EpollReactor;
use super::uring_executor::check_uring_availability;
use super::uring_executor::is_uring_stable;
use super::uring_executor::Error as UringError;
use super::uring_executor::UringReactor;
use crate::common_executor;
use crate::common_executor::RawExecutor;
use crate::AsyncResult;
use crate::IntoAsync;
use crate::IoSource;

/// An executor for scheduling tasks that poll futures to completion.
///
/// All asynchronous operations must run within an executor, which is capable of spawning futures as
/// tasks. This executor also provides a mechanism for performing asynchronous I/O operations.
///
/// The returned type is a cheap, clonable handle to the underlying executor. Cloning it will only
/// create a new reference, not a new executor.
///
/// Note that language limitations (trait objects can have <=1 non auto trait) require this to be
/// represented on the POSIX side as an enum, rather than a trait. This leads to some code &
/// interface duplication, but as far as we understand that is unavoidable.
///
/// See <https://chromium-review.googlesource.com/c/chromiumos/platform/crosvm/+/2571401/2..6/cros_async/src/executor.rs#b75>
/// for further details.
///
/// # Examples
///
/// Concurrently wait for multiple files to become readable/writable and then read/write the data.
///
/// ```
/// use std::cmp::min;
/// use std::error::Error;
/// use std::fs::{File, OpenOptions};
///
/// use cros_async::{AsyncResult, Executor, IoSource, complete3};
/// const CHUNK_SIZE: usize = 32;
///
/// // Write all bytes from `data` to `f`.
/// async fn write_file(f: &IoSource<File>, mut data: Vec<u8>) -> AsyncResult<()> {
///     while data.len() > 0 {
///         let (count, mut buf) = f.write_from_vec(None, data).await?;
///
///         data = buf.split_off(count);
///     }
///
///     Ok(())
/// }
///
/// // Transfer `len` bytes of data from `from` to `to`.
/// async fn transfer_data(
///     from: IoSource<File>,
///     to: IoSource<File>,
///     len: usize,
/// ) -> AsyncResult<usize> {
///     let mut rem = len;
///
///     while rem > 0 {
///         let buf = vec![0u8; min(rem, CHUNK_SIZE)];
///         let (count, mut data) = from.read_to_vec(None, buf).await?;
///
///         if count == 0 {
///             // End of file. Return the number of bytes transferred.
///             return Ok(len - rem);
///         }
///
///         data.truncate(count);
///         write_file(&to, data).await?;
///
///         rem = rem.saturating_sub(count);
///     }
///
///     Ok(len)
/// }
///
/// #[cfg(any(target_os = "android", target_os = "linux"))]
/// # fn do_it() -> Result<(), Box<dyn Error>> {
///     let ex = Executor::new()?;
///
///     let (rx, tx) = base::linux::pipe(true)?;
///     let zero = File::open("/dev/zero")?;
///     let zero_bytes = CHUNK_SIZE * 7;
///     let zero_to_pipe = transfer_data(
///         ex.async_from(zero)?,
///         ex.async_from(tx.try_clone()?)?,
///         zero_bytes,
///     );
///
///     let rand = File::open("/dev/urandom")?;
///     let rand_bytes = CHUNK_SIZE * 19;
///     let rand_to_pipe = transfer_data(ex.async_from(rand)?, ex.async_from(tx)?, rand_bytes);
///
///     let null = OpenOptions::new().write(true).open("/dev/null")?;
///     let null_bytes = zero_bytes + rand_bytes;
///     let pipe_to_null = transfer_data(ex.async_from(rx)?, ex.async_from(null)?, null_bytes);
///
///     ex.run_until(complete3(
///         async { assert_eq!(pipe_to_null.await.unwrap(), null_bytes) },
///         async { assert_eq!(zero_to_pipe.await.unwrap(), zero_bytes) },
///         async { assert_eq!(rand_to_pipe.await.unwrap(), rand_bytes) },
///     ))?;
///
/// #     Ok(())
/// # }
/// #[cfg(any(target_os = "android", target_os = "linux"))]
/// # do_it().unwrap();
/// ```

#[derive(Clone)]
pub enum Executor {
    Uring(Arc<RawExecutor<UringReactor>>),
    Fd(Arc<RawExecutor<EpollReactor>>),
}

/// An enum to express the kind of the backend of `Executor`
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, serde_keyvalue::FromKeyValues,
)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub enum ExecutorKind {
    Uring,
    // For command-line parsing, user-friendly "epoll" is chosen instead of fd.
    #[serde(rename = "epoll")]
    Fd,
}

/// If set, [`ExecutorKind::default()`] returns the value of `DEFAULT_EXECUTOR_KIND`.
/// If not set, [`ExecutorKind::default()`] returns a statically-chosen default value, and
/// [`ExecutorKind::default()`] initializes `DEFAULT_EXECUTOR_KIND` with that value.
static DEFAULT_EXECUTOR_KIND: OnceCell<ExecutorKind> = OnceCell::new();

impl Default for ExecutorKind {
    fn default() -> Self {
        *DEFAULT_EXECUTOR_KIND.get_or_init(|| ExecutorKind::Fd)
    }
}

/// The error type for [`Executor::set_default_executor_kind()`].
#[derive(Debug, ThisError)]
pub enum SetDefaultExecutorKindError {
    /// The default executor kind is set more than once.
    #[error("The default executor kind is already set to {0:?}")]
    SetMoreThanOnce(ExecutorKind),

    /// io_uring is unavailable. The reason might be the lack of the kernel support,
    /// but is not limited to that.
    #[error("io_uring is unavailable: {0}")]
    UringUnavailable(UringError),
}

/// Reference to a task managed by the executor.
///
/// Dropping a `TaskHandle` attempts to cancel the associated task. Call `detach` to allow it to
/// continue running the background.
///
/// `await`ing the `TaskHandle` waits for the task to finish and yields its result.
pub enum TaskHandle<R> {
    Uring(common_executor::TaskHandle<UringReactor, R>),
    Fd(common_executor::TaskHandle<EpollReactor, R>),
}

impl<R: Send + 'static> TaskHandle<R> {
    pub fn detach(self) {
        match self {
            TaskHandle::Uring(x) => x.detach(),
            TaskHandle::Fd(x) => x.detach(),
        }
    }

    // Cancel the task and wait for it to stop. Returns the result of the task if it was already
    // finished.
    pub async fn cancel(self) -> Option<R> {
        match self {
            TaskHandle::Uring(x) => x.cancel().await,
            TaskHandle::Fd(x) => x.cancel().await,
        }
    }
}

impl<R: 'static> Future for TaskHandle<R> {
    type Output = R;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context) -> std::task::Poll<Self::Output> {
        match self.get_mut() {
            TaskHandle::Uring(x) => Pin::new(x).poll(cx),
            TaskHandle::Fd(x) => Pin::new(x).poll(cx),
        }
    }
}

impl Executor {
    /// Create a new `Executor`.
    pub fn new() -> AsyncResult<Self> {
        Executor::with_executor_kind(ExecutorKind::default())
    }

    /// Create a new `Executor` of the given `ExecutorKind`.
    pub fn with_executor_kind(kind: ExecutorKind) -> AsyncResult<Self> {
        match kind {
            ExecutorKind::Uring => RawExecutor::new().map(Executor::Uring),
            ExecutorKind::Fd => RawExecutor::new().map(Executor::Fd),
        }
    }

    /// Set the default ExecutorKind for [`Self::new()`]. This call is effective only once.
    /// If a call is the first call, it sets the default, and `set_default_executor_kind`
    /// returns `Ok(())`. Otherwise, it returns `SetDefaultExecutorKindError::SetMoreThanOnce`
    /// which contains the existing ExecutorKind value configured by the first call.
    pub fn set_default_executor_kind(
        executor_kind: ExecutorKind,
    ) -> Result<(), SetDefaultExecutorKindError> {
        if executor_kind == ExecutorKind::Uring {
            check_uring_availability().map_err(SetDefaultExecutorKindError::UringUnavailable)?;
            if !is_uring_stable() {
                warn!(
                    "Enabling io_uring executor on the kernel version where io_uring is unstable"
                );
            }
        }

        debug!("setting the default executor to {:?}", executor_kind);
        DEFAULT_EXECUTOR_KIND.set(executor_kind).map_err(|_|
            // `expect` succeeds since this closure runs only when DEFAULT_EXECUTOR_KIND is set.
            SetDefaultExecutorKindError::SetMoreThanOnce(
                *DEFAULT_EXECUTOR_KIND
                    .get()
                    .expect("Failed to get DEFAULT_EXECUTOR_KIND"),
            ))
    }

    /// Create a new `IoSource<F>` associated with `self`. Callers may then use the returned
    /// `IoSource` to directly start async operations without needing a separate reference to the
    /// executor.
    pub fn async_from<'a, F: IntoAsync + 'a>(&self, f: F) -> AsyncResult<IoSource<F>> {
        match self {
            Executor::Uring(ex) => ex.new_source(f),
            Executor::Fd(ex) => ex.new_source(f),
        }
    }

    /// Spawn a new future for this executor to run to completion. Callers may use the returned
    /// `TaskHandle` to await on the result of `f`. Dropping the returned `TaskHandle` will cancel
    /// `f`, preventing it from being polled again. To drop a `TaskHandle` without canceling the
    /// future associated with it use `TaskHandle::detach`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use cros_async::AsyncResult;
    /// # fn example_spawn() -> AsyncResult<()> {
    /// #      use std::thread;
    ///
    /// #      use cros_async::Executor;
    ///       use futures::executor::block_on;
    ///
    /// #      let ex = Executor::new()?;
    ///
    /// #      // Spawn a thread that runs the executor.
    /// #      let ex2 = ex.clone();
    /// #      thread::spawn(move || ex2.run());
    ///
    ///       let task = ex.spawn(async { 7 + 13 });
    ///
    ///       let result = block_on(task);
    ///       assert_eq!(result, 20);
    /// #     Ok(())
    /// # }
    ///
    /// # example_spawn().unwrap();
    /// ```
    pub fn spawn<F>(&self, f: F) -> TaskHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        match self {
            Executor::Uring(ex) => TaskHandle::Uring(ex.spawn(f)),
            Executor::Fd(ex) => TaskHandle::Fd(ex.spawn(f)),
        }
    }

    /// Spawn a thread-local task for this executor to drive to completion. Like `spawn` but without
    /// requiring `Send` on `F` or `F::Output`. This method should only be called from the same
    /// thread where `run()` or `run_until()` is called.
    ///
    /// # Panics
    ///
    /// `Executor::run` and `Executor::run_until` will panic if they try to poll a future that was
    /// added by calling `spawn_local` from a different thread.
    ///
    /// # Examples
    ///
    /// ```
    /// # use cros_async::AsyncResult;
    /// # fn example_spawn_local() -> AsyncResult<()> {
    /// #      use cros_async::Executor;
    ///
    /// #      let ex = Executor::new()?;
    ///
    ///       let task = ex.spawn_local(async { 7 + 13 });
    ///
    ///       let result = ex.run_until(task)?;
    ///       assert_eq!(result, 20);
    /// #     Ok(())
    /// # }
    ///
    /// # example_spawn_local().unwrap();
    /// ```
    pub fn spawn_local<F>(&self, f: F) -> TaskHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        match self {
            Executor::Uring(ex) => TaskHandle::Uring(ex.spawn_local(f)),
            Executor::Fd(ex) => TaskHandle::Fd(ex.spawn_local(f)),
        }
    }

    /// Run the provided closure on a dedicated thread where blocking is allowed.
    ///
    /// Callers may `await` on the returned `TaskHandle` to wait for the result of `f`. Dropping
    /// the returned `TaskHandle` may not cancel the operation if it was already started on a
    /// worker thread.
    ///
    /// # Panics
    ///
    /// `await`ing the `TaskHandle` after the `Executor` is dropped will panic if the work was not
    /// already completed.
    ///
    /// # Examples
    ///
    /// ```edition2018
    /// # use cros_async::Executor;
    ///
    /// # async fn do_it(ex: &Executor) {
    ///     let res = ex.spawn_blocking(move || {
    ///         // Do some CPU-intensive or blocking work here.
    ///
    ///         42
    ///     }).await;
    ///
    ///     assert_eq!(res, 42);
    /// # }
    ///
    /// # let ex = Executor::new().unwrap();
    /// # ex.run_until(do_it(&ex)).unwrap();
    /// ```
    pub fn spawn_blocking<F, R>(&self, f: F) -> TaskHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        match self {
            Executor::Uring(ex) => TaskHandle::Uring(ex.spawn_blocking(f)),
            Executor::Fd(ex) => TaskHandle::Fd(ex.spawn_blocking(f)),
        }
    }

    /// Run the executor indefinitely, driving all spawned futures to completion. This method will
    /// block the current thread and only return in the case of an error.
    ///
    /// # Panics
    ///
    /// Once this method has been called on a thread, it may only be called on that thread from that
    /// point on. Attempting to call it from another thread will panic.
    ///
    /// # Examples
    ///
    /// ```
    /// # use cros_async::AsyncResult;
    /// # fn example_run() -> AsyncResult<()> {
    ///       use std::thread;
    ///
    ///       use cros_async::Executor;
    ///       use futures::executor::block_on;
    ///
    ///       let ex = Executor::new()?;
    ///
    ///       // Spawn a thread that runs the executor.
    ///       let ex2 = ex.clone();
    ///       thread::spawn(move || ex2.run());
    ///
    ///       let task = ex.spawn(async { 7 + 13 });
    ///
    ///       let result = block_on(task);
    ///       assert_eq!(result, 20);
    /// #     Ok(())
    /// # }
    ///
    /// # example_run().unwrap();
    /// ```
    pub fn run(&self) -> AsyncResult<()> {
        self.run_until(std::future::pending())
    }

    /// Drive all futures spawned in this executor until `f` completes. This method will block the
    /// current thread only until `f` is complete and there may still be unfinished futures in the
    /// executor.
    ///
    /// # Panics
    ///
    /// Once this method has been called on a thread, from then onwards it may only be called on
    /// that thread. Attempting to call it from another thread will panic.
    ///
    /// # Examples
    ///
    /// ```
    /// # use cros_async::AsyncResult;
    /// # fn example_run_until() -> AsyncResult<()> {
    ///       use cros_async::Executor;
    ///
    ///       let ex = Executor::new()?;
    ///
    ///       let task = ex.spawn_local(async { 7 + 13 });
    ///
    ///       let result = ex.run_until(task)?;
    ///       assert_eq!(result, 20);
    /// #     Ok(())
    /// # }
    ///
    /// # example_run_until().unwrap();
    /// ```
    pub fn run_until<F: Future>(&self, f: F) -> AsyncResult<F::Output> {
        match self {
            Executor::Uring(ex) => Ok(ex.run_until(f)?),
            Executor::Fd(ex) => Ok(ex.run_until(f)?),
        }
    }
}

impl AsRawDescriptors for Executor {
    fn as_raw_descriptors(&self) -> Vec<RawDescriptor> {
        match self {
            Executor::Uring(ex) => ex.as_raw_descriptors(),
            Executor::Fd(ex) => ex.as_raw_descriptors(),
        }
    }
}
