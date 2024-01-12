// Copyright 2017 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Runs hardware devices in child processes.

use std::fs;

use anyhow::anyhow;
use base::error;
use base::info;
use base::linux::process::fork_process;
use base::AsRawDescriptor;
#[cfg(feature = "swap")]
use base::AsRawDescriptors;
use base::RawDescriptor;
use base::SharedMemory;
use base::Tube;
use base::TubeError;
use libc::pid_t;
use minijail::Minijail;
use remain::sorted;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

use crate::bus::ConfigWriteResult;
use crate::pci::CrosvmDeviceId;
use crate::pci::PciAddress;
use crate::BusAccessInfo;
use crate::BusDevice;
use crate::BusRange;
use crate::BusType;
use crate::DeviceId;
use crate::Suspendable;

/// Errors for proxy devices.
#[sorted]
#[derive(Error, Debug)]
pub enum Error {
    #[error("Failed to activate ProxyDevice")]
    ActivatingProxyDevice,
    #[error("Failed to fork jail process: {0}")]
    ForkingJail(#[from] minijail::Error),
    #[error("Failed to configure swap: {0}")]
    Swap(anyhow::Error),
    #[error("Failed to configure tube: {0}")]
    Tube(#[from] TubeError),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Serialize, Deserialize)]
enum Command {
    Activate,
    Read {
        len: u32,
        info: BusAccessInfo,
    },
    Write {
        len: u32,
        info: BusAccessInfo,
        data: [u8; 8],
    },
    ReadConfig(u32),
    WriteConfig {
        reg_idx: u32,
        offset: u32,
        len: u32,
        data: [u8; 4],
    },
    InitPciConfigMapping {
        shmem: SharedMemory,
        base: usize,
        len: usize,
    },
    ReadVirtualConfig(u32),
    WriteVirtualConfig {
        reg_idx: u32,
        value: u32,
    },
    DestroyDevice,
    Shutdown,
    GetRanges,
    Snapshot,
    Restore {
        data: serde_json::Value,
    },
    Sleep,
    Wake,
}
#[derive(Debug, Serialize, Deserialize)]
enum CommandResult {
    Ok,
    ReadResult([u8; 8]),
    ReadConfigResult(u32),
    WriteConfigResult {
        mmio_remove: Vec<BusRange>,
        mmio_add: Vec<BusRange>,
        io_remove: Vec<BusRange>,
        io_add: Vec<BusRange>,
        removed_pci_devices: Vec<PciAddress>,
    },
    InitPciConfigMappingResult(bool),
    ReadVirtualConfigResult(u32),
    GetRangesResult(Vec<(BusRange, BusType)>),
    SnapshotResult(std::result::Result<serde_json::Value, String>),
    RestoreResult(std::result::Result<(), String>),
    SleepResult(std::result::Result<(), String>),
    WakeResult(std::result::Result<(), String>),
}

fn child_proc<D: BusDevice>(tube: Tube, mut device: D) {
    // Wait for activation signal to function as BusDevice.
    match tube.recv() {
        Ok(Command::Activate) => {
            if let Err(e) = tube.send(&CommandResult::Ok) {
                error!("sending activation result failed: {:?}", &e);
                return;
            }
        }
        // Commands other than activate is unexpected, close device.
        Ok(cmd) => {
            panic!("Receiving Command {:?} before device is activated", &cmd);
        }
        // Most likely tube error is caused by other end is dropped, release resource.
        Err(e) => {
            error!("device failed before activation: {:?}. Dropping device", e);
            drop(device);
            return;
        }
    };
    loop {
        let cmd = match tube.recv() {
            Ok(cmd) => cmd,
            Err(err) => {
                error!("child device process failed recv: {}", err);
                break;
            }
        };

        let res = match cmd {
            Command::Activate => {
                panic!("Device shall only be activated once, duplicated ProxyDevice likely");
            }
            Command::Read { len, info } => {
                let mut buffer = [0u8; 8];
                device.read(info, &mut buffer[0..len as usize]);
                tube.send(&CommandResult::ReadResult(buffer))
            }
            Command::Write { len, info, data } => {
                let len = len as usize;
                device.write(info, &data[0..len]);
                // Command::Write does not have a result.
                Ok(())
            }
            Command::ReadConfig(idx) => {
                let val = device.config_register_read(idx as usize);
                tube.send(&CommandResult::ReadConfigResult(val))
            }
            Command::WriteConfig {
                reg_idx,
                offset,
                len,
                data,
            } => {
                let len = len as usize;
                let res =
                    device.config_register_write(reg_idx as usize, offset as u64, &data[0..len]);
                tube.send(&CommandResult::WriteConfigResult {
                    mmio_remove: res.mmio_remove,
                    mmio_add: res.mmio_add,
                    io_remove: res.io_remove,
                    io_add: res.io_add,
                    removed_pci_devices: res.removed_pci_devices,
                })
            }
            Command::InitPciConfigMapping { shmem, base, len } => {
                let success = device.init_pci_config_mapping(&shmem, base, len);
                tube.send(&CommandResult::InitPciConfigMappingResult(success))
            }
            Command::ReadVirtualConfig(idx) => {
                let val = device.virtual_config_register_read(idx as usize);
                tube.send(&CommandResult::ReadVirtualConfigResult(val))
            }
            Command::WriteVirtualConfig { reg_idx, value } => {
                device.virtual_config_register_write(reg_idx as usize, value);
                tube.send(&CommandResult::Ok)
            }
            Command::DestroyDevice => {
                device.destroy_device();
                Ok(())
            }
            Command::Shutdown => {
                // Explicitly drop the device so that its Drop implementation has a chance to run
                // before sending the `Command::Shutdown` response.
                drop(device);

                let _ = tube.send(&CommandResult::Ok);
                return;
            }
            Command::GetRanges => {
                let ranges = device.get_ranges();
                tube.send(&CommandResult::GetRangesResult(ranges))
            }
            Command::Snapshot => {
                let res = device.snapshot();
                tube.send(&CommandResult::SnapshotResult(
                    res.map_err(|e| e.to_string()),
                ))
            }
            Command::Restore { data } => {
                let res = device.restore(data);
                tube.send(&CommandResult::RestoreResult(
                    res.map_err(|e| e.to_string()),
                ))
            }
            Command::Sleep => {
                let res = device.sleep();
                tube.send(&CommandResult::SleepResult(res.map_err(|e| e.to_string())))
            }
            Command::Wake => {
                let res = device.wake();
                tube.send(&CommandResult::WakeResult(res.map_err(|e| e.to_string())))
            }
        };
        if let Err(e) = res {
            error!("child device process failed send: {}", e);
        }
    }
}

/// ChildProcIntf is the interface to the device child process.
///
/// ChildProcIntf implements Serialize, and can be sent across process before it functions as a
/// ProxyDevice. However, a child process shall only correspond to one ProxyDevice. The uniqueness
/// is checked when ChildProcIntf is casted into ProxyDevice.
#[derive(Serialize, Deserialize)]
pub struct ChildProcIntf {
    tube: Tube,
    pid: pid_t,
    debug_label: String,
}

impl ChildProcIntf {
    /// Creates ChildProcIntf that shall be turned into exactly one ProxyDevice.
    ///
    /// The ChildProcIntf struct holds the interface to the device process. It shall be turned into
    /// a ProxyDevice exactly once (at an arbitrary process). Since ChildProcIntf may be duplicated
    /// by serde, the uniqueness of the interface is checked when ChildProcIntf is converted into
    /// ProxyDevice.
    ///
    /// # Arguments
    /// * `device` - The device to isolate to another process.
    /// * `jail` - The jail to use for isolating the given device.
    /// * `keep_rds` - File descriptors that will be kept open in the child.
    pub fn new<D: BusDevice, #[cfg(feature = "swap")] P: swap::PrepareFork>(
        mut device: D,
        jail: Minijail,
        mut keep_rds: Vec<RawDescriptor>,
        #[cfg(feature = "swap")] swap_prepare_fork: &mut Option<P>,
    ) -> Result<ChildProcIntf> {
        let debug_label = device.debug_label();
        let (child_tube, parent_tube) = Tube::pair()?;

        keep_rds.push(child_tube.as_raw_descriptor());

        #[cfg(feature = "swap")]
        let swap_device_uffd_sender = if let Some(prepare_fork) = swap_prepare_fork {
            let sender = prepare_fork.prepare_fork().map_err(Error::Swap)?;
            keep_rds.extend(sender.as_raw_descriptors());
            Some(sender)
        } else {
            None
        };

        // This will be removed after b/183540186 gets fixed.
        // Only enabled it for x86_64 since the original bug mostly happens on x86 boards.
        if cfg!(target_arch = "x86_64") && debug_label == "pcivirtio-gpu" {
            if let Ok(cmd) = fs::read_to_string("/proc/self/cmdline") {
                if cmd.contains("arcvm") {
                    if let Ok(share) = fs::read_to_string("/sys/fs/cgroup/cpu/arcvm/cpu.shares") {
                        info!("arcvm cpu share when booting gpu is {:}", share.trim());
                    }
                }
            }
        }

        let child_process = fork_process(jail, keep_rds, Some(debug_label.clone()), || {
            #[cfg(feature = "swap")]
            if let Some(swap_device_uffd_sender) = swap_device_uffd_sender {
                if let Err(e) = swap_device_uffd_sender.on_process_forked() {
                    error!("failed to SwapController::on_process_forked: {:?}", e);
                    // SAFETY:
                    // exit() is trivially safe.
                    unsafe { libc::exit(1) };
                }
            }

            device.on_sandboxed();
            child_proc(child_tube, device);

            // We're explicitly not using std::process::exit here to avoid the cleanup of
            // stdout/stderr globals. This can cause cascading panics and SIGILL if a worker
            // thread attempts to log to stderr after at_exit handlers have been run.
            // TODO(crbug.com/992494): Remove this once device shutdown ordering is clearly
            // defined.
            //
            // SAFETY:
            // exit() is trivially safe.
            // ! Never returns
            unsafe { libc::exit(0) };
        })?;

        // Suppress the no waiting warning from `base::sys::linux::process::Child` because crosvm
        // does not wait for the processes from ProxyDevice explicitly. Instead it reaps all the
        // child processes on its exit by `crosvm::sys::linux::main::wait_all_children()`.
        let pid = child_process.into_pid();

        Ok(ChildProcIntf {
            tube: parent_tube,
            pid,
            debug_label,
        })
    }
}

/// Wraps an inner `BusDevice` that is run inside a child process via fork.
///
/// The forked device process will automatically be terminated when this is dropped.
pub struct ProxyDevice {
    child_proc_intf: ChildProcIntf,
}

impl TryFrom<ChildProcIntf> for ProxyDevice {
    type Error = Error;
    fn try_from(child_proc_intf: ChildProcIntf) -> Result<Self> {
        // Notify child process to be activated as a BusDevice.
        child_proc_intf.tube.send(&Command::Activate)?;
        // Device returns Ok if it is activated only once.
        match child_proc_intf.tube.recv()? {
            CommandResult::Ok => Ok(Self { child_proc_intf }),
            _ => Err(Error::ActivatingProxyDevice),
        }
    }
}

impl ProxyDevice {
    /// Takes the given device and isolates it into another process via fork before returning.
    ///
    /// Because forks are very unfriendly to destructors and all memory mappings and file
    /// descriptors are inherited, this should be used as early as possible in the main process.
    /// ProxyDevice::new shall not be used for hotplugging. Call ChildProcIntf::new on jail warden
    /// process, send using serde, then cast into ProxyDevice instead.
    ///
    /// # Arguments
    /// * `device` - The device to isolate to another process.
    /// * `jail` - The jail to use for isolating the given device.
    /// * `keep_rds` - File descriptors that will be kept open in the child.
    pub fn new<D: BusDevice, #[cfg(feature = "swap")] P: swap::PrepareFork>(
        device: D,
        jail: Minijail,
        keep_rds: Vec<RawDescriptor>,
        #[cfg(feature = "swap")] swap_prepare_fork: &mut Option<P>,
    ) -> Result<ProxyDevice> {
        ChildProcIntf::new(
            device,
            jail,
            keep_rds,
            #[cfg(feature = "swap")]
            swap_prepare_fork,
        )?
        .try_into()
    }

    pub fn pid(&self) -> pid_t {
        self.child_proc_intf.pid
    }

    /// Send a command that does not expect a response from the child device process.
    fn send_no_result(&self, cmd: &Command) {
        let res = self.child_proc_intf.tube.send(cmd);
        if let Err(e) = res {
            error!(
                "failed write to child device process {}: {}",
                self.child_proc_intf.debug_label, e,
            );
        }
    }

    /// Send a command and read its response from the child device process.
    fn sync_send(&self, cmd: &Command) -> Option<CommandResult> {
        self.send_no_result(cmd);
        match self.child_proc_intf.tube.recv() {
            Err(e) => {
                error!(
                    "failed to read result of {:?} from child device process {}: {}",
                    cmd, self.child_proc_intf.debug_label, e,
                );
                None
            }
            Ok(r) => Some(r),
        }
    }
}

impl BusDevice for ProxyDevice {
    fn device_id(&self) -> DeviceId {
        CrosvmDeviceId::ProxyDevice.into()
    }

    fn debug_label(&self) -> String {
        self.child_proc_intf.debug_label.clone()
    }

    fn config_register_write(
        &mut self,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
    ) -> ConfigWriteResult {
        let len = data.len() as u32;
        let mut buffer = [0u8; 4];
        buffer[0..data.len()].clone_from_slice(data);
        let reg_idx = reg_idx as u32;
        let offset = offset as u32;
        if let Some(CommandResult::WriteConfigResult {
            mmio_remove,
            mmio_add,
            io_remove,
            io_add,
            removed_pci_devices,
        }) = self.sync_send(&Command::WriteConfig {
            reg_idx,
            offset,
            len,
            data: buffer,
        }) {
            ConfigWriteResult {
                mmio_remove,
                mmio_add,
                io_remove,
                io_add,
                removed_pci_devices,
            }
        } else {
            Default::default()
        }
    }

    fn config_register_read(&self, reg_idx: usize) -> u32 {
        let res = self.sync_send(&Command::ReadConfig(reg_idx as u32));
        if let Some(CommandResult::ReadConfigResult(val)) = res {
            val
        } else {
            0
        }
    }

    fn init_pci_config_mapping(&mut self, shmem: &SharedMemory, base: usize, len: usize) -> bool {
        let Ok(shmem) = shmem.try_clone() else {
            error!("Failed to clone pci config mapping shmem");
            return false;
        };
        let res = self.sync_send(&Command::InitPciConfigMapping { shmem, base, len });
        matches!(res, Some(CommandResult::InitPciConfigMappingResult(true)))
    }

    fn virtual_config_register_write(&mut self, reg_idx: usize, value: u32) {
        let reg_idx = reg_idx as u32;
        self.sync_send(&Command::WriteVirtualConfig { reg_idx, value });
    }

    fn virtual_config_register_read(&self, reg_idx: usize) -> u32 {
        let res = self.sync_send(&Command::ReadVirtualConfig(reg_idx as u32));
        if let Some(CommandResult::ReadVirtualConfigResult(val)) = res {
            val
        } else {
            0
        }
    }

    fn read(&mut self, info: BusAccessInfo, data: &mut [u8]) {
        let len = data.len() as u32;
        if let Some(CommandResult::ReadResult(buffer)) =
            self.sync_send(&Command::Read { len, info })
        {
            let len = data.len();
            data.clone_from_slice(&buffer[0..len]);
        }
    }

    fn write(&mut self, info: BusAccessInfo, data: &[u8]) {
        let mut buffer = [0u8; 8];
        let len = data.len() as u32;
        buffer[0..data.len()].clone_from_slice(data);
        self.send_no_result(&Command::Write {
            len,
            info,
            data: buffer,
        });
    }

    fn get_ranges(&self) -> Vec<(BusRange, BusType)> {
        if let Some(CommandResult::GetRangesResult(ranges)) = self.sync_send(&Command::GetRanges) {
            ranges
        } else {
            Default::default()
        }
    }

    fn destroy_device(&mut self) {
        self.send_no_result(&Command::DestroyDevice);
    }
}

impl Suspendable for ProxyDevice {
    fn snapshot(&mut self) -> anyhow::Result<serde_json::Value> {
        let res = self.sync_send(&Command::Snapshot);
        match res {
            Some(CommandResult::SnapshotResult(Ok(snap))) => Ok(snap),
            Some(CommandResult::SnapshotResult(Err(e))) => Err(anyhow!(
                "failed to snapshot {}: {:#}",
                self.debug_label(),
                e
            )),
            _ => Err(anyhow!("unexpected snapshot result {:?}", res)),
        }
    }

    fn restore(&mut self, data: serde_json::Value) -> anyhow::Result<()> {
        let res = self.sync_send(&Command::Restore { data });
        match res {
            Some(CommandResult::RestoreResult(Ok(()))) => Ok(()),
            Some(CommandResult::RestoreResult(Err(e))) => {
                Err(anyhow!("failed to restore {}: {:#}", self.debug_label(), e))
            }
            _ => Err(anyhow!("unexpected restore result {:?}", res)),
        }
    }

    fn sleep(&mut self) -> anyhow::Result<()> {
        let res = self.sync_send(&Command::Sleep);
        match res {
            Some(CommandResult::SleepResult(Ok(()))) => Ok(()),
            Some(CommandResult::SleepResult(Err(e))) => {
                Err(anyhow!("failed to sleep {}: {:#}", self.debug_label(), e))
            }
            _ => Err(anyhow!("unexpected sleep result {:?}", res)),
        }
    }

    fn wake(&mut self) -> anyhow::Result<()> {
        let res = self.sync_send(&Command::Wake);
        match res {
            Some(CommandResult::WakeResult(Ok(()))) => Ok(()),
            Some(CommandResult::WakeResult(Err(e))) => {
                Err(anyhow!("failed to wake {}: {:#}", self.debug_label(), e))
            }
            _ => Err(anyhow!("unexpected wake result {:?}", res)),
        }
    }
}

impl Drop for ProxyDevice {
    fn drop(&mut self) {
        self.sync_send(&Command::Shutdown);
    }
}

/// Note: These tests must be run with --test-threads=1 to allow minijail to fork
/// the process.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::pci::PciId;

    /// A simple test echo device that outputs the same u8 that was written to it.
    struct EchoDevice {
        data: u8,
        config: u8,
    }
    impl EchoDevice {
        fn new() -> EchoDevice {
            EchoDevice { data: 0, config: 0 }
        }
    }
    impl BusDevice for EchoDevice {
        fn device_id(&self) -> DeviceId {
            PciId::new(0, 0).into()
        }

        fn debug_label(&self) -> String {
            "EchoDevice".to_owned()
        }

        fn write(&mut self, _info: BusAccessInfo, data: &[u8]) {
            assert!(data.len() == 1);
            self.data = data[0];
        }

        fn read(&mut self, _info: BusAccessInfo, data: &mut [u8]) {
            assert!(data.len() == 1);
            data[0] = self.data;
        }

        fn config_register_write(
            &mut self,
            _reg_idx: usize,
            _offset: u64,
            data: &[u8],
        ) -> ConfigWriteResult {
            let result = ConfigWriteResult {
                ..Default::default()
            };
            assert!(data.len() == 1);
            self.config = data[0];
            result
        }

        fn config_register_read(&self, _reg_idx: usize) -> u32 {
            self.config as u32
        }
    }

    impl Suspendable for EchoDevice {}

    fn new_proxied_echo_device() -> ProxyDevice {
        let device = EchoDevice::new();
        let keep_fds: Vec<RawDescriptor> = Vec::new();
        let minijail = Minijail::new().unwrap();
        ProxyDevice::new(
            device,
            minijail,
            keep_fds,
            #[cfg(feature = "swap")]
            &mut None::<swap::SwapController>,
        )
        .unwrap()
    }

    // TODO(b/173833661): Find a way to ensure these tests are run single-threaded.
    #[test]
    #[ignore]
    fn test_debug_label() {
        let proxy_device = new_proxied_echo_device();
        assert_eq!(proxy_device.debug_label(), "EchoDevice");
    }

    #[test]
    #[ignore]
    fn test_proxied_read_write() {
        let mut proxy_device = new_proxied_echo_device();
        let address = BusAccessInfo {
            offset: 0,
            address: 0,
            id: 0,
        };
        proxy_device.write(address, &[42]);
        let mut read_buffer = [0];
        proxy_device.read(address, &mut read_buffer);
        assert_eq!(read_buffer, [42]);
    }

    #[test]
    #[ignore]
    fn test_proxied_config() {
        let mut proxy_device = new_proxied_echo_device();
        proxy_device.config_register_write(0, 0, &[42]);
        assert_eq!(proxy_device.config_register_read(0), 42);
    }
}
