// Copyright 2021 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod block;
#[cfg(feature = "gpu")]
pub mod gpu;
mod handler;
mod listener;
#[cfg(feature = "net")]
mod net;
#[cfg(feature = "audio")]
pub mod snd;

pub use block::run_block_device;
pub use block::Options as BlockOptions;
use cros_async::Executor;
use cros_async::ExecutorKind;
#[cfg(feature = "gpu")]
pub use gpu::run_gpu_device;
#[cfg(feature = "gpu")]
pub use gpu::Options as GpuOptions;
pub use handler::VhostBackendReqConnectionState;
pub use handler::VhostUserBackend;
pub use listener::sys::VhostUserListener;
pub use listener::VhostUserListenerTrait;
#[cfg(feature = "net")]
pub use net::run_net_device;
#[cfg(feature = "net")]
pub use net::NetBackend;
#[cfg(feature = "net")]
pub use net::Options as NetOptions;
#[cfg(feature = "audio")]
pub use snd::run_snd_device;
#[cfg(feature = "audio")]
pub use snd::Options as SndOptions;
use vmm_vhost::VhostUserSlaveReqHandler;

cfg_if::cfg_if! {
    if #[cfg(any(target_os = "android", target_os = "linux"))] {
        mod console;
        mod fs;
        mod vsock;
        mod wl;

        pub use vsock::{run_vsock_device, Options as VsockOptions, VhostUserVsockDevice};
        pub use wl::{run_wl_device, parse_wayland_sock, Options as WlOptions};
        pub use console::{create_vu_console_device, run_console_device, Options as ConsoleOptions};
        pub use fs::{run_fs_device, Options as FsOptions};
    } else if #[cfg(windows)] {
        #[cfg(all(feature = "net", feature = "slirp"))]
        pub use net::sys::windows::NetBackendConfig;
    }
}

/// A trait for vhost-user devices.
///
/// Upon being given an [[Executor]], a device can be converted into a
/// [[VhostUserSlaveReqHandler]], which can then process the requests from the front-end.
///
/// We don't build request handlers directly to ensure that the device starts to process queues in
/// the jailed process, not in the main process. [[VhostUserDevice::into_req_handler()]] is called
/// only after jailing, which ensures that any operations by the request handler is done in the
/// jailed process.
pub trait VhostUserDevice {
    /// The maximum number of queues that this device can manage.
    fn max_queue_num(&self) -> usize;

    /// Turn this device into a vhost-user request handler that will run the device.
    ///
    /// `ex` is an executor the device can use to schedule its tasks.
    fn into_req_handler(
        self: Box<Self>,
        ex: &Executor,
    ) -> anyhow::Result<Box<dyn VhostUserSlaveReqHandler>>;

    /// The preferred ExecutorKind of an Executor to accept by [`VhostUserDevice::into_req_handler()`].
    fn executor_kind(&self) -> Option<ExecutorKind> {
        None
    }
}
