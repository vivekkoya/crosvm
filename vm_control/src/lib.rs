// Copyright 2017 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Handles IPC for controlling the main VM process.
//!
//! The VM Control IPC protocol is synchronous, meaning that each `VmRequest` sent over a connection
//! will receive a `VmResponse` for that request next time data is received over that connection.
//!
//! The wire message format is a little-endian C-struct of fixed size, along with a file descriptor
//! if the request type expects one.

pub mod api;
#[cfg(feature = "gdb")]
pub mod gdb;
#[cfg(feature = "gpu")]
pub mod gpu;

#[cfg(any(target_os = "android", target_os = "linux"))]
use base::linux::MemoryMappingBuilderUnix;
#[cfg(windows)]
use base::MemoryMappingBuilderWindows;
use hypervisor::BalloonEvent;
use hypervisor::MemRegion;

#[cfg(feature = "balloon")]
mod balloon_tube;
pub mod client;
pub mod sys;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::_rdtsc;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::convert::TryInto;
use std::fmt;
use std::fmt::Display;
use std::fs::File;
use std::path::PathBuf;
use std::result::Result as StdResult;
use std::str::FromStr;
use std::sync::mpsc;
use std::sync::Arc;

use anyhow::bail;
use anyhow::Context;
use base::error;
use base::info;
use base::warn;
use base::with_as_descriptor;
use base::AsRawDescriptor;
use base::Descriptor;
use base::Error as SysError;
use base::Event;
use base::ExternalMapping;
use base::IntoRawDescriptor;
use base::MappedRegion;
use base::MemoryMappingBuilder;
use base::MmapError;
use base::Protection;
use base::Result;
use base::SafeDescriptor;
use base::SharedMemory;
use base::Tube;
use hypervisor::Datamatch;
use hypervisor::IoEventAddress;
use hypervisor::IrqRoute;
use hypervisor::IrqSource;
pub use hypervisor::MemSlot;
use hypervisor::VcpuSnapshot;
use hypervisor::Vm;
use libc::EINVAL;
use libc::EIO;
use libc::ENODEV;
use libc::ENOTSUP;
use libc::ERANGE;
#[cfg(feature = "registered_events")]
use protos::registered_events;
use remain::sorted;
use resources::Alloc;
use resources::SystemAllocator;
use rutabaga_gfx::DeviceId;
use rutabaga_gfx::RutabagaDescriptor;
use rutabaga_gfx::RutabagaFromRawDescriptor;
use rutabaga_gfx::RutabagaGralloc;
use rutabaga_gfx::RutabagaHandle;
use rutabaga_gfx::RutabagaMappedRegion;
use rutabaga_gfx::VulkanInfo;
use serde::Deserialize;
use serde::Serialize;
use swap::SwapStatus;
use sync::Mutex;
#[cfg(any(target_os = "android", target_os = "linux"))]
pub use sys::FsMappingRequest;
#[cfg(any(target_os = "android", target_os = "linux"))]
pub use sys::VmMsyncRequest;
#[cfg(any(target_os = "android", target_os = "linux"))]
pub use sys::VmMsyncResponse;
use thiserror::Error;
pub use vm_control_product::GpuSendToMain;
pub use vm_control_product::GpuSendToService;
pub use vm_control_product::ServiceSendToGpu;
use vm_memory::GuestAddress;

#[cfg(feature = "balloon")]
pub use crate::balloon_tube::*;
#[cfg(feature = "gdb")]
pub use crate::gdb::VcpuDebug;
#[cfg(feature = "gdb")]
pub use crate::gdb::VcpuDebugStatus;
#[cfg(feature = "gdb")]
pub use crate::gdb::VcpuDebugStatusMessage;
#[cfg(feature = "gpu")]
use crate::gpu::GpuControlCommand;
#[cfg(feature = "gpu")]
use crate::gpu::GpuControlResult;

/// Control the state of a particular VM CPU.
#[derive(Clone, Debug)]
pub enum VcpuControl {
    #[cfg(feature = "gdb")]
    Debug(VcpuDebug),
    RunState(VmRunMode),
    MakeRT,
    // Request the current state of the vCPU. The result is sent back over the included channel.
    GetStates(mpsc::Sender<VmRunMode>),
    Snapshot(mpsc::Sender<anyhow::Result<VcpuSnapshot>>),
    Restore(VcpuRestoreRequest),
}

/// Request to restore a Vcpu from a given snapshot, and report the results
/// back via the provided channel.
#[derive(Clone, Debug)]
pub struct VcpuRestoreRequest {
    pub result_sender: mpsc::Sender<anyhow::Result<()>>,
    pub snapshot: Box<VcpuSnapshot>,
    #[cfg(target_arch = "x86_64")]
    pub host_tsc_reference_moment: u64,
}

/// Mode of execution for the VM.
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub enum VmRunMode {
    /// The default run mode indicating the VCPUs are running.
    #[default]
    Running,
    /// Indicates that the VCPUs are suspending execution until the `Running` mode is set.
    Suspending,
    /// Indicates that the VM is exiting all processes.
    Exiting,
    /// Indicates that the VM is in a breakpoint waiting for the debugger to do continue.
    Breakpoint,
}

impl Display for VmRunMode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::VmRunMode::*;

        match self {
            Running => write!(f, "running"),
            Suspending => write!(f, "suspending"),
            Exiting => write!(f, "exiting"),
            Breakpoint => write!(f, "breakpoint"),
        }
    }
}

// Trait for devices that get notification on specific GPE trigger
pub trait GpeNotify: Send {
    fn notify(&mut self) {}
}

// Trait for devices that get notification on specific PCI PME
pub trait PmeNotify: Send {
    fn notify(&mut self, _requester_id: u16) {}
}

pub trait PmResource {
    fn pwrbtn_evt(&mut self) {}
    fn slpbtn_evt(&mut self) {}
    fn rtc_evt(&mut self) {}
    fn gpe_evt(&mut self, _gpe: u32) {}
    fn pme_evt(&mut self, _requester_id: u16) {}
    fn register_gpe_notify_dev(&mut self, _gpe: u32, _notify_dev: Arc<Mutex<dyn GpeNotify>>) {}
    fn register_pme_notify_dev(&mut self, _bus: u8, _notify_dev: Arc<Mutex<dyn PmeNotify>>) {}
}

/// The maximum number of devices that can be listed in one `UsbControlCommand`.
///
/// This value was set to be equal to `xhci_regs::MAX_PORTS` for convenience, but it is not
/// necessary for correctness. Importing that value directly would be overkill because it would
/// require adding a big dependency for a single const.
pub const USB_CONTROL_MAX_PORTS: usize = 16;

#[derive(Serialize, Deserialize, Debug)]
pub enum DiskControlCommand {
    /// Resize a disk to `new_size` in bytes.
    Resize { new_size: u64 },
}

impl Display for DiskControlCommand {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::DiskControlCommand::*;

        match self {
            Resize { new_size } => write!(f, "disk_resize {}", new_size),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum DiskControlResult {
    Ok,
    Err(SysError),
}

/// Net control commands for adding and removing tap devices.
#[cfg(feature = "pci-hotplug")]
#[derive(Serialize, Deserialize, Debug)]
pub enum NetControlCommand {
    AddTap(String),
    RemoveTap(u8),
}

#[derive(Serialize, Deserialize, Debug)]
pub enum UsbControlCommand {
    AttachDevice {
        #[serde(with = "with_as_descriptor")]
        file: File,
    },
    DetachDevice {
        port: u8,
    },
    ListDevice {
        ports: [u8; USB_CONTROL_MAX_PORTS],
    },
}

#[derive(Serialize, Deserialize, Copy, Clone, Debug, Default)]
pub struct UsbControlAttachedDevice {
    pub port: u8,
    pub vendor_id: u16,
    pub product_id: u16,
}

impl UsbControlAttachedDevice {
    pub fn valid(self) -> bool {
        self.port != 0
    }
}

#[cfg(feature = "pci-hotplug")]
#[derive(Serialize, Deserialize, Debug, Clone)]
#[must_use]
/// Result for hotplug and removal of PCI device.
pub enum PciControlResult {
    AddOk { bus: u8 },
    ErrString(String),
    RemoveOk,
}

#[cfg(feature = "pci-hotplug")]
impl Display for PciControlResult {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::PciControlResult::*;

        match self {
            AddOk { bus } => write!(f, "add_ok {}", bus),
            ErrString(e) => write!(f, "error: {}", e),
            RemoveOk => write!(f, "remove_ok"),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum UsbControlResult {
    Ok { port: u8 },
    NoAvailablePort,
    NoSuchDevice,
    NoSuchPort,
    FailedToOpenDevice,
    Devices([UsbControlAttachedDevice; USB_CONTROL_MAX_PORTS]),
    FailedToInitHostDevice,
}

impl Display for UsbControlResult {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::UsbControlResult::*;

        match self {
            UsbControlResult::Ok { port } => write!(f, "ok {}", port),
            NoAvailablePort => write!(f, "no_available_port"),
            NoSuchDevice => write!(f, "no_such_device"),
            NoSuchPort => write!(f, "no_such_port"),
            FailedToOpenDevice => write!(f, "failed_to_open_device"),
            Devices(devices) => {
                write!(f, "devices")?;
                for d in devices.iter().filter(|d| d.valid()) {
                    write!(f, " {} {:04x} {:04x}", d.port, d.vendor_id, d.product_id)?;
                }
                std::result::Result::Ok(())
            }
            FailedToInitHostDevice => write!(f, "failed_to_init_host_device"),
        }
    }
}

/// Commands for snapshot feature
#[derive(Serialize, Deserialize, Debug)]
pub enum SnapshotCommand {
    Take { snapshot_path: PathBuf },
}

/// Commands for restore feature
#[derive(Serialize, Deserialize, Debug)]
pub enum RestoreCommand {
    Apply { restore_path: PathBuf },
}

/// Commands for actions on devices and the devices control thread.
#[derive(Serialize, Deserialize, Debug)]
pub enum DeviceControlCommand {
    SleepDevices,
    WakeDevices,
    SnapshotDevices { snapshot_path: PathBuf },
    RestoreDevices { restore_path: PathBuf },
    GetDevicesState,
    Exit,
}

/// Commands to control the IRQ handler thread.
#[derive(Serialize, Deserialize)]
pub enum IrqHandlerRequest {
    /// No response is sent for this command.
    AddIrqControlTubes(Vec<Tube>),
    /// Refreshes the set of event tokens (Events) from the Irqchip that the IRQ
    /// handler waits on to forward IRQs to their final destination (e.g. via
    /// Irqchip::service_irq_event).
    ///
    /// If the set of tokens exposed by the Irqchip changes while the VM is
    /// running (such as for snapshot restore), this command must be sent
    /// otherwise the VM will not receive IRQs as expected.
    RefreshIrqEventTokens,
    WakeAndNotifyIteration,
    /// No response is sent for this command.
    Exit,
}

const EXPECTED_MAX_IRQ_FLUSH_ITERATIONS: usize = 100;

/// Response for [IrqHandlerRequest].
#[derive(Serialize, Deserialize, Debug)]
pub enum IrqHandlerResponse {
    /// Sent when the IRQ event tokens have been refreshed.
    IrqEventTokenRefreshComplete,
    /// Specifies the number of tokens serviced in the requested iteration
    /// (less the token for the `WakeAndNotifyIteration` request).
    HandlerIterationComplete(usize),
}

/// Source of a `VmMemoryRequest::RegisterMemory` mapping.
#[derive(Serialize, Deserialize)]
pub enum VmMemorySource {
    /// Register shared memory represented by the given descriptor.
    /// On Windows, descriptor MUST be a mapping handle.
    SharedMemory(SharedMemory),
    /// Register a file mapping from the given descriptor.
    Descriptor {
        /// File descriptor to map.
        descriptor: SafeDescriptor,
        /// Offset within the file in bytes.
        offset: u64,
        /// Size of the mapping in bytes.
        size: u64,
    },
    /// Register memory mapped by Vulkano.
    Vulkan {
        descriptor: SafeDescriptor,
        handle_type: u32,
        memory_idx: u32,
        device_uuid: [u8; 16],
        driver_uuid: [u8; 16],
        size: u64,
    },
    /// Register the current rutabaga external mapping.
    ExternalMapping { ptr: u64, size: u64 },
}

// The following are wrappers to avoid base dependencies in the rutabaga crate
fn to_rutabaga_desciptor(s: SafeDescriptor) -> RutabagaDescriptor {
    // SAFETY:
    // Safe because we own the SafeDescriptor at this point.
    unsafe { RutabagaDescriptor::from_raw_descriptor(s.into_raw_descriptor()) }
}

struct RutabagaMemoryRegion {
    region: Box<dyn RutabagaMappedRegion>,
}

impl RutabagaMemoryRegion {
    pub fn new(region: Box<dyn RutabagaMappedRegion>) -> RutabagaMemoryRegion {
        RutabagaMemoryRegion { region }
    }
}

// SAFETY:
//
// Self guarantees `ptr`..`ptr+size` is an mmaped region owned by this object that
// can't be unmapped during the `MappedRegion`'s lifetime.
unsafe impl MappedRegion for RutabagaMemoryRegion {
    fn as_ptr(&self) -> *mut u8 {
        self.region.as_ptr()
    }

    fn size(&self) -> usize {
        self.region.size()
    }
}

impl VmMemorySource {
    /// Map the resource and return its mapping and size in bytes.
    pub fn map(
        self,
        gralloc: &mut RutabagaGralloc,
        prot: Protection,
    ) -> Result<(Box<dyn MappedRegion>, u64, Option<SafeDescriptor>)> {
        let (mem_region, size, descriptor) = match self {
            VmMemorySource::Descriptor {
                descriptor,
                offset,
                size,
            } => (
                map_descriptor(&descriptor, offset, size, prot)?,
                size,
                Some(descriptor),
            ),

            VmMemorySource::SharedMemory(shm) => {
                (map_descriptor(&shm, 0, shm.size(), prot)?, shm.size(), None)
            }
            VmMemorySource::Vulkan {
                descriptor,
                handle_type,
                memory_idx,
                device_uuid,
                driver_uuid,
                size,
            } => {
                let device_id = DeviceId {
                    device_uuid,
                    driver_uuid,
                };
                let mapped_region = match gralloc.import_and_map(
                    RutabagaHandle {
                        os_handle: to_rutabaga_desciptor(descriptor),
                        handle_type,
                    },
                    VulkanInfo {
                        memory_idx,
                        device_id,
                    },
                    size,
                ) {
                    Ok(mapped_region) => {
                        let mapped_region: Box<dyn MappedRegion> =
                            Box::new(RutabagaMemoryRegion::new(mapped_region));
                        mapped_region
                    }
                    Err(e) => {
                        error!("gralloc failed to import and map: {}", e);
                        return Err(SysError::new(EINVAL));
                    }
                };
                (mapped_region, size, None)
            }
            VmMemorySource::ExternalMapping { ptr, size } => {
                let mapped_region: Box<dyn MappedRegion> = Box::new(ExternalMapping {
                    ptr,
                    size: size as usize,
                });
                (mapped_region, size, None)
            }
        };
        Ok((mem_region, size, descriptor))
    }
}

/// Destination of a `VmMemoryRequest::RegisterMemory` mapping in guest address space.
#[derive(Serialize, Deserialize)]
pub enum VmMemoryDestination {
    /// Map at an offset within an existing PCI BAR allocation.
    ExistingAllocation { allocation: Alloc, offset: u64 },
    /// Map at the specified guest physical address.
    GuestPhysicalAddress(u64),
}

impl VmMemoryDestination {
    /// Allocate and return the guest address of a memory mapping destination.
    pub fn allocate(self, allocator: &mut SystemAllocator, size: u64) -> Result<GuestAddress> {
        let addr = match self {
            VmMemoryDestination::ExistingAllocation { allocation, offset } => allocator
                .mmio_allocator_any()
                .address_from_pci_offset(allocation, offset, size)
                .map_err(|_e| SysError::new(EINVAL))?,
            VmMemoryDestination::GuestPhysicalAddress(gpa) => gpa,
        };
        Ok(GuestAddress(addr))
    }
}

/// Request to register or unregister an ioevent.
#[derive(Serialize, Deserialize)]
pub struct IoEventUpdateRequest {
    pub event: Event,
    pub addr: u64,
    pub datamatch: Datamatch,
    pub register: bool,
}

#[derive(Serialize, Deserialize)]
pub enum VmMemoryRequest {
    /// Prepare a shared memory region to make later operations more efficient. This
    /// may be a no-op depending on underlying platform support.
    PrepareSharedMemoryRegion { alloc: Alloc },
    RegisterMemory {
        /// Source of the memory to register (mapped file descriptor, shared memory region, etc.)
        source: VmMemorySource,
        /// Where to map the memory in the guest.
        dest: VmMemoryDestination,
        /// Whether to map the memory read only (true) or read-write (false).
        prot: Protection,
    },
    /// Call hypervisor to free the given memory range.
    DynamicallyFreeMemoryRange {
        guest_address: GuestAddress,
        size: u64,
    },
    /// Call hypervisor to reclaim a priorly freed memory range.
    DynamicallyReclaimMemoryRange {
        guest_address: GuestAddress,
        size: u64,
    },
    /// Balloon allocation/deallocation target reached.
    BalloonTargetReached { size: u64 },
    /// Unregister the given memory slot that was previously registered with `RegisterMemory`.
    UnregisterMemory(VmMemoryRegionId),
    /// Register an ioeventfd by looking up using Alloc info.
    IoEventWithAlloc {
        evt: Event,
        allocation: Alloc,
        offset: u64,
        datamatch: Datamatch,
        register: bool,
    },
    /// Register an eventfd with raw guest memory address.
    IoEventRaw(IoEventUpdateRequest),
}

/// Struct for managing `VmMemoryRequest`s IOMMU related state.
pub struct VmMemoryRequestIommuClient {
    tube: Arc<Mutex<Tube>>,
    gpu_memory: BTreeSet<MemSlot>,
}

impl VmMemoryRequestIommuClient {
    /// Constructs `VmMemoryRequestIommuClient` from a tube for communication with the viommu.
    pub fn new(tube: Arc<Mutex<Tube>>) -> Self {
        Self {
            tube,
            gpu_memory: BTreeSet::new(),
        }
    }
}

pub struct VmMemoryRegionState {
    // alloc -> (pfn, slot)
    slot_map: HashMap<Alloc, (u64, MemSlot)>,
    // id -> (slot, Option<offset, size>)
    mapped_regions: BTreeMap<VmMemoryRegionId, (MemSlot, Option<(usize, usize)>)>,
}

impl VmMemoryRegionState {
    pub fn new() -> VmMemoryRegionState {
        Self {
            slot_map: HashMap::new(),
            mapped_regions: BTreeMap::new(),
        }
    }
}

impl Default for VmMemoryRegionState {
    fn default() -> Self {
        Self::new()
    }
}

fn handle_prepared_region(
    vm: &mut impl Vm,
    region_state: &mut VmMemoryRegionState,
    source: &VmMemorySource,
    dest: &VmMemoryDestination,
    prot: &Protection,
) -> Option<VmMemoryResponse> {
    let VmMemoryDestination::ExistingAllocation { allocation, offset } = dest else {
        return None;
    };

    let (pfn, slot) = region_state.slot_map.get(allocation)?;

    let (descriptor, file_offset, size) = match source {
        VmMemorySource::Descriptor {
            descriptor,
            offset,
            size,
        } => (
            Descriptor(descriptor.as_raw_descriptor()),
            *offset,
            *size as usize,
        ),
        VmMemorySource::SharedMemory(shm) => {
            let size = shm.size() as usize;
            (Descriptor(shm.as_raw_descriptor()), 0, size)
        }
        _ => return Some(VmMemoryResponse::Err(SysError::new(EINVAL))),
    };
    if let Err(err) = vm.add_fd_mapping(
        *slot,
        *offset as usize,
        size,
        &descriptor,
        file_offset,
        *prot,
    ) {
        return Some(VmMemoryResponse::Err(err));
    }
    let pfn = pfn + (offset >> 12);
    region_state.mapped_regions.insert(
        VmMemoryRegionId(pfn),
        (*slot, Some((*offset as usize, size))),
    );
    Some(VmMemoryResponse::RegisterMemory(VmMemoryRegionId(pfn)))
}

impl VmMemoryRequest {
    /// Executes this request on the given Vm.
    ///
    /// # Arguments
    /// * `vm` - The `Vm` to perform the request on.
    /// * `allocator` - Used to allocate addresses.
    ///
    /// This does not return a result, instead encapsulating the success or failure in a
    /// `VmMemoryResponse` with the intended purpose of sending the response back over the socket
    /// that received this `VmMemoryResponse`.
    pub fn execute(
        self,
        vm: &mut impl Vm,
        sys_allocator: &mut SystemAllocator,
        gralloc: &mut RutabagaGralloc,
        iommu_client: Option<&mut VmMemoryRequestIommuClient>,
        region_state: &mut VmMemoryRegionState,
    ) -> VmMemoryResponse {
        use self::VmMemoryRequest::*;
        match self {
            PrepareSharedMemoryRegion { alloc } => {
                // Currently the iommu_client is only used by virtio-gpu, and virtio-gpu
                // is incompatible with PrepareSharedMemoryRegion because we can't use
                // add_fd_mapping with VmMemorySource::Vulkan.
                assert!(iommu_client.is_none());

                if !sys::should_prepare_memory_region() {
                    return VmMemoryResponse::Ok;
                }

                match sys::prepare_shared_memory_region(vm, sys_allocator, alloc) {
                    Ok(info) => {
                        region_state.slot_map.insert(alloc, info);
                        VmMemoryResponse::Ok
                    }
                    Err(e) => VmMemoryResponse::Err(e),
                }
            }
            RegisterMemory { source, dest, prot } => {
                if let Some(resp) = handle_prepared_region(vm, region_state, &source, &dest, &prot)
                {
                    return resp;
                }

                // Correct on Windows because callers of this IPC guarantee descriptor is a mapping
                // handle.
                let (mapped_region, size, descriptor) = match source.map(gralloc, prot) {
                    Ok((region, size, descriptor)) => (region, size, descriptor),
                    Err(e) => return VmMemoryResponse::Err(e),
                };

                let guest_addr = match dest.allocate(sys_allocator, size) {
                    Ok(addr) => addr,
                    Err(e) => return VmMemoryResponse::Err(e),
                };

                let slot = match vm.add_memory_region(
                    guest_addr,
                    mapped_region,
                    prot == Protection::read(),
                    false,
                ) {
                    Ok(slot) => slot,
                    Err(e) => return VmMemoryResponse::Err(e),
                };

                if let (Some(descriptor), Some(iommu_client)) = (descriptor, iommu_client) {
                    let request =
                        VirtioIOMMURequest::VfioCommand(VirtioIOMMUVfioCommand::VfioDmabufMap {
                            mem_slot: slot,
                            gfn: guest_addr.0 >> 12,
                            size,
                            dma_buf: descriptor,
                        });

                    match virtio_iommu_request(&iommu_client.tube.lock(), &request) {
                        Ok(VirtioIOMMUResponse::VfioResponse(VirtioIOMMUVfioResult::Ok)) => (),
                        resp => {
                            error!("Unexpected message response: {:?}", resp);
                            // Ignore the result because there is nothing we can do with a failure.
                            let _ = vm.remove_memory_region(slot);
                            return VmMemoryResponse::Err(SysError::new(EINVAL));
                        }
                    };

                    iommu_client.gpu_memory.insert(slot);
                }

                let pfn = guest_addr.0 >> 12;
                region_state
                    .mapped_regions
                    .insert(VmMemoryRegionId(pfn), (slot, None));
                VmMemoryResponse::RegisterMemory(VmMemoryRegionId(pfn))
            }
            UnregisterMemory(id) => match region_state.mapped_regions.remove(&id) {
                Some((slot, None)) => match vm.remove_memory_region(slot) {
                    Ok(_) => {
                        if let Some(iommu_client) = iommu_client {
                            if iommu_client.gpu_memory.remove(&slot) {
                                let request = VirtioIOMMURequest::VfioCommand(
                                    VirtioIOMMUVfioCommand::VfioDmabufUnmap(slot),
                                );

                                match virtio_iommu_request(&iommu_client.tube.lock(), &request) {
                                    Ok(VirtioIOMMUResponse::VfioResponse(
                                        VirtioIOMMUVfioResult::Ok,
                                    )) => VmMemoryResponse::Ok,
                                    resp => {
                                        error!("Unexpected message response: {:?}", resp);
                                        VmMemoryResponse::Err(SysError::new(EINVAL))
                                    }
                                }
                            } else {
                                VmMemoryResponse::Ok
                            }
                        } else {
                            VmMemoryResponse::Ok
                        }
                    }
                    Err(e) => VmMemoryResponse::Err(e),
                },
                Some((slot, Some((offset, size)))) => match vm.remove_mapping(slot, offset, size) {
                    Ok(()) => VmMemoryResponse::Ok,
                    Err(e) => VmMemoryResponse::Err(e),
                },
                None => VmMemoryResponse::Err(SysError::new(EINVAL)),
            },
            DynamicallyFreeMemoryRange {
                guest_address,
                size,
            } => match vm.handle_balloon_event(BalloonEvent::Inflate(MemRegion {
                guest_address,
                size,
            })) {
                Ok(_) => VmMemoryResponse::Ok,
                Err(e) => VmMemoryResponse::Err(e),
            },
            DynamicallyReclaimMemoryRange {
                guest_address,
                size,
            } => match vm.handle_balloon_event(BalloonEvent::Deflate(MemRegion {
                guest_address,
                size,
            })) {
                Ok(_) => VmMemoryResponse::Ok,
                Err(e) => VmMemoryResponse::Err(e),
            },
            BalloonTargetReached { size } => {
                match vm.handle_balloon_event(BalloonEvent::BalloonTargetReached(size)) {
                    Ok(_) => VmMemoryResponse::Ok,
                    Err(e) => VmMemoryResponse::Err(e),
                }
            }
            IoEventWithAlloc {
                evt,
                allocation,
                offset,
                datamatch,
                register,
            } => {
                let len = match datamatch {
                    Datamatch::AnyLength => 1,
                    Datamatch::U8(_) => 1,
                    Datamatch::U16(_) => 2,
                    Datamatch::U32(_) => 4,
                    Datamatch::U64(_) => 8,
                };
                let addr = match sys_allocator
                    .mmio_allocator_any()
                    .address_from_pci_offset(allocation, offset, len)
                {
                    Ok(addr) => addr,
                    Err(e) => {
                        error!("error getting target address: {:#}", e);
                        return VmMemoryResponse::Err(SysError::new(EINVAL));
                    }
                };
                let res = if register {
                    vm.register_ioevent(&evt, IoEventAddress::Mmio(addr), datamatch)
                } else {
                    vm.unregister_ioevent(&evt, IoEventAddress::Mmio(addr), datamatch)
                };
                match res {
                    Ok(_) => VmMemoryResponse::Ok,
                    Err(e) => VmMemoryResponse::Err(e),
                }
            }
            IoEventRaw(request) => {
                let res = if request.register {
                    vm.register_ioevent(
                        &request.event,
                        IoEventAddress::Mmio(request.addr),
                        request.datamatch,
                    )
                } else {
                    vm.unregister_ioevent(
                        &request.event,
                        IoEventAddress::Mmio(request.addr),
                        request.datamatch,
                    )
                };
                match res {
                    Ok(_) => VmMemoryResponse::Ok,
                    Err(e) => VmMemoryResponse::Err(e),
                }
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialOrd, PartialEq, Eq, Ord, Clone, Copy)]
/// Identifer for registered memory regions. Globally unique.
// The current implementation uses pfn as the unique identifier.
pub struct VmMemoryRegionId(u64);

#[derive(Serialize, Deserialize, Debug)]
pub enum VmMemoryResponse {
    /// The request to register memory into guest address space was successful.
    RegisterMemory(VmMemoryRegionId),
    Ok,
    Err(SysError),
}

#[derive(Serialize, Deserialize, Debug)]
pub enum VmIrqRequest {
    /// Allocate one gsi, and associate gsi to irqfd with register_irqfd()
    AllocateOneMsi {
        irqfd: Event,
        device_id: u32,
        queue_id: usize,
        device_name: String,
    },
    /// Allocate a specific gsi to irqfd with register_irqfd(). This must only
    /// be used when it is known that the gsi is free. Only the snapshot
    /// subsystem can make this guarantee, and use of this request by any other
    /// caller is strongly discouraged.
    AllocateOneMsiAtGsi {
        irqfd: Event,
        gsi: u32,
        device_id: u32,
        queue_id: usize,
        device_name: String,
    },
    /// Add one msi route entry into the IRQ chip.
    AddMsiRoute {
        gsi: u32,
        msi_address: u64,
        msi_data: u32,
    },
    // unregister_irqfs() and release gsi
    ReleaseOneIrq {
        gsi: u32,
        irqfd: Event,
    },
}

/// Data to set up an IRQ event or IRQ route on the IRQ chip.
/// VmIrqRequest::execute can't take an `IrqChip` argument, because of a dependency cycle between
/// devices and vm_control, so it takes a Fn that processes an `IrqSetup`.
pub enum IrqSetup<'a> {
    Event(u32, &'a Event, u32, usize, String),
    Route(IrqRoute),
    UnRegister(u32, &'a Event),
}

impl VmIrqRequest {
    /// Executes this request on the given Vm.
    ///
    /// # Arguments
    /// * `set_up_irq` - A function that applies an `IrqSetup` to an IRQ chip.
    ///
    /// This does not return a result, instead encapsulating the success or failure in a
    /// `VmIrqResponse` with the intended purpose of sending the response back over the socket
    /// that received this `VmIrqResponse`.
    pub fn execute<F>(&self, set_up_irq: F, sys_allocator: &mut SystemAllocator) -> VmIrqResponse
    where
        F: FnOnce(IrqSetup) -> Result<()>,
    {
        use self::VmIrqRequest::*;
        match *self {
            AllocateOneMsi {
                ref irqfd,
                device_id,
                queue_id,
                ref device_name,
            } => {
                if let Some(irq_num) = sys_allocator.allocate_irq() {
                    match set_up_irq(IrqSetup::Event(
                        irq_num,
                        irqfd,
                        device_id,
                        queue_id,
                        device_name.clone(),
                    )) {
                        Ok(_) => VmIrqResponse::AllocateOneMsi { gsi: irq_num },
                        Err(e) => VmIrqResponse::Err(e),
                    }
                } else {
                    VmIrqResponse::Err(SysError::new(EINVAL))
                }
            }
            AllocateOneMsiAtGsi {
                ref irqfd,
                gsi,
                device_id,
                queue_id,
                ref device_name,
            } => {
                match set_up_irq(IrqSetup::Event(
                    gsi,
                    irqfd,
                    device_id,
                    queue_id,
                    device_name.clone(),
                )) {
                    Ok(_) => VmIrqResponse::Ok,
                    Err(e) => VmIrqResponse::Err(e),
                }
            }
            AddMsiRoute {
                gsi,
                msi_address,
                msi_data,
            } => {
                let route = IrqRoute {
                    gsi,
                    source: IrqSource::Msi {
                        address: msi_address,
                        data: msi_data,
                    },
                };
                match set_up_irq(IrqSetup::Route(route)) {
                    Ok(_) => VmIrqResponse::Ok,
                    Err(e) => VmIrqResponse::Err(e),
                }
            }
            ReleaseOneIrq { gsi, ref irqfd } => {
                let _ = set_up_irq(IrqSetup::UnRegister(gsi, irqfd));
                sys_allocator.release_irq(gsi);
                VmIrqResponse::Ok
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum VmIrqResponse {
    AllocateOneMsi { gsi: u32 },
    Ok,
    Err(SysError),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum DevicesState {
    Sleep,
    Wake,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum BatControlResult {
    Ok,
    NoBatDevice,
    NoSuchHealth,
    NoSuchProperty,
    NoSuchStatus,
    NoSuchBatType,
    StringParseIntErr,
}

impl Display for BatControlResult {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::BatControlResult::*;

        match self {
            Ok => write!(f, "Setting battery property successfully"),
            NoBatDevice => write!(f, "No battery device created"),
            NoSuchHealth => write!(f, "Invalid Battery health setting. Only support: unknown/good/overheat/dead/overvoltage/unexpectedfailure/cold/watchdogtimerexpire/safetytimerexpire/overcurrent"),
            NoSuchProperty => write!(f, "Battery doesn't have such property. Only support: status/health/present/capacity/aconline"),
            NoSuchStatus => write!(f, "Invalid Battery status setting. Only support: unknown/charging/discharging/notcharging/full"),
            NoSuchBatType => write!(f, "Invalid Battery type setting. Only support: goldfish"),
            StringParseIntErr => write!(f, "Battery property target ParseInt error"),
        }
    }
}

#[derive(Serialize, Deserialize, Copy, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BatteryType {
    #[default]
    Goldfish,
}

impl FromStr for BatteryType {
    type Err = BatControlResult;

    fn from_str(s: &str) -> StdResult<Self, Self::Err> {
        match s {
            "goldfish" => Ok(BatteryType::Goldfish),
            _ => Err(BatControlResult::NoSuchBatType),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum BatProperty {
    Status,
    Health,
    Present,
    Capacity,
    ACOnline,
}

impl FromStr for BatProperty {
    type Err = BatControlResult;

    fn from_str(s: &str) -> StdResult<Self, Self::Err> {
        match s {
            "status" => Ok(BatProperty::Status),
            "health" => Ok(BatProperty::Health),
            "present" => Ok(BatProperty::Present),
            "capacity" => Ok(BatProperty::Capacity),
            "aconline" => Ok(BatProperty::ACOnline),
            _ => Err(BatControlResult::NoSuchProperty),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum BatStatus {
    Unknown,
    Charging,
    DisCharging,
    NotCharging,
    Full,
}

impl BatStatus {
    pub fn new(status: String) -> std::result::Result<Self, BatControlResult> {
        match status.as_str() {
            "unknown" => Ok(BatStatus::Unknown),
            "charging" => Ok(BatStatus::Charging),
            "discharging" => Ok(BatStatus::DisCharging),
            "notcharging" => Ok(BatStatus::NotCharging),
            "full" => Ok(BatStatus::Full),
            _ => Err(BatControlResult::NoSuchStatus),
        }
    }
}

impl FromStr for BatStatus {
    type Err = BatControlResult;

    fn from_str(s: &str) -> StdResult<Self, Self::Err> {
        match s {
            "unknown" => Ok(BatStatus::Unknown),
            "charging" => Ok(BatStatus::Charging),
            "discharging" => Ok(BatStatus::DisCharging),
            "notcharging" => Ok(BatStatus::NotCharging),
            "full" => Ok(BatStatus::Full),
            _ => Err(BatControlResult::NoSuchStatus),
        }
    }
}

impl From<BatStatus> for u32 {
    fn from(status: BatStatus) -> Self {
        status as u32
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum BatHealth {
    Unknown,
    Good,
    Overheat,
    Dead,
    OverVoltage,
    UnexpectedFailure,
    Cold,
    WatchdogTimerExpire,
    SafetyTimerExpire,
    OverCurrent,
}

impl FromStr for BatHealth {
    type Err = BatControlResult;

    fn from_str(s: &str) -> StdResult<Self, Self::Err> {
        match s {
            "unknown" => Ok(BatHealth::Unknown),
            "good" => Ok(BatHealth::Good),
            "overheat" => Ok(BatHealth::Overheat),
            "dead" => Ok(BatHealth::Dead),
            "overvoltage" => Ok(BatHealth::OverVoltage),
            "unexpectedfailure" => Ok(BatHealth::UnexpectedFailure),
            "cold" => Ok(BatHealth::Cold),
            "watchdogtimerexpire" => Ok(BatHealth::WatchdogTimerExpire),
            "safetytimerexpire" => Ok(BatHealth::SafetyTimerExpire),
            "overcurrent" => Ok(BatHealth::OverCurrent),
            _ => Err(BatControlResult::NoSuchHealth),
        }
    }
}

impl From<BatHealth> for u32 {
    fn from(status: BatHealth) -> Self {
        status as u32
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum BatControlCommand {
    SetStatus(BatStatus),
    SetHealth(BatHealth),
    SetPresent(u32),
    SetCapacity(u32),
    SetACOnline(u32),
}

impl BatControlCommand {
    pub fn new(property: String, target: String) -> std::result::Result<Self, BatControlResult> {
        let cmd = property.parse::<BatProperty>()?;
        match cmd {
            BatProperty::Status => Ok(BatControlCommand::SetStatus(target.parse::<BatStatus>()?)),
            BatProperty::Health => Ok(BatControlCommand::SetHealth(target.parse::<BatHealth>()?)),
            BatProperty::Present => Ok(BatControlCommand::SetPresent(
                target
                    .parse::<u32>()
                    .map_err(|_| BatControlResult::StringParseIntErr)?,
            )),
            BatProperty::Capacity => Ok(BatControlCommand::SetCapacity(
                target
                    .parse::<u32>()
                    .map_err(|_| BatControlResult::StringParseIntErr)?,
            )),
            BatProperty::ACOnline => Ok(BatControlCommand::SetACOnline(
                target
                    .parse::<u32>()
                    .map_err(|_| BatControlResult::StringParseIntErr)?,
            )),
        }
    }
}

/// Used for VM to control battery properties.
pub struct BatControl {
    pub type_: BatteryType,
    pub control_tube: Tube,
}

// Used to mark hotplug pci device's device type
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum HotPlugDeviceType {
    UpstreamPort,
    DownstreamPort,
    EndPoint,
}

// Used for VM to hotplug pci devices
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HotPlugDeviceInfo {
    pub device_type: HotPlugDeviceType,
    pub path: PathBuf,
    pub hp_interrupt: bool,
}

/// Message for communicating a suspend or resume to the virtio-pvclock device.
#[derive(Serialize, Deserialize, Debug)]
pub enum PvClockCommand {
    Suspend,
    Resume,
}

/// Message used by virtio-pvclock to communicate command results.
#[derive(Serialize, Deserialize, Debug)]
pub enum PvClockCommandResponse {
    Ok,
    Err(SysError),
}

/// Commands for vmm-swap feature
#[derive(Serialize, Deserialize, Debug)]
pub enum SwapCommand {
    Enable,
    Trim,
    SwapOut,
    Disable { slow_file_cleanup: bool },
    Status,
}

///
/// A request to the main process to perform some operation on the VM.
///
/// Unless otherwise noted, each request should expect a `VmResponse::Ok` to be received on success.
#[derive(Serialize, Deserialize, Debug)]
pub enum VmRequest {
    /// Break the VM's run loop and exit.
    Exit,
    /// Trigger a power button event in the guest.
    Powerbtn,
    /// Trigger a sleep button event in the guest.
    Sleepbtn,
    /// Trigger a RTC interrupt in the guest.
    Rtc,
    /// Suspend the VM's VCPUs until resume.
    SuspendVcpus,
    /// Swap the memory content into files on a disk
    Swap(SwapCommand),
    /// Resume the VM's VCPUs that were previously suspended.
    ResumeVcpus,
    /// Inject a general-purpose event.
    Gpe(u32),
    /// Inject a PCI PME
    PciPme(u16),
    /// Make the VM's RT VCPU real-time.
    MakeRT,
    /// Command for balloon driver.
    #[cfg(feature = "balloon")]
    BalloonCommand(BalloonControlCommand),
    /// Send a command to a disk chosen by `disk_index`.
    /// `disk_index` is a 0-based count of `--disk`, `--rwdisk`, and `-r` command-line options.
    DiskCommand {
        disk_index: usize,
        command: DiskControlCommand,
    },
    /// Command to use controller.
    UsbCommand(UsbControlCommand),
    #[cfg(feature = "gpu")]
    /// Command to modify the gpu.
    GpuCommand(GpuControlCommand),
    /// Command to set battery.
    BatCommand(BatteryType, BatControlCommand),
    /// Command to add/remove multiple vfio-pci devices
    HotPlugVfioCommand {
        device: HotPlugDeviceInfo,
        add: bool,
    },
    /// Command to add/remove network tap device as virtio-pci device
    #[cfg(feature = "pci-hotplug")]
    HotPlugNetCommand(NetControlCommand),
    /// Command to Snapshot devices
    Snapshot(SnapshotCommand),
    /// Command to Restore devices
    Restore(RestoreCommand),
    /// Register for event notification
    #[cfg(feature = "registered_events")]
    RegisterListener {
        socket_addr: String,
        event: RegisteredEvent,
    },
    /// Unregister for notifications for event
    #[cfg(feature = "registered_events")]
    UnregisterListener {
        socket_addr: String,
        event: RegisteredEvent,
    },
    /// Unregister for all event notification
    #[cfg(feature = "registered_events")]
    Unregister { socket_addr: String },
    /// Suspend VM VCPUs and Devices until resume.
    SuspendVm,
    /// Resume VM VCPUs and Devices.
    ResumeVm,
}

/// NOTE: when making any changes to this enum please also update
/// RegisteredEventFfi in crosvm_control/src/lib.rs
#[cfg(feature = "registered_events")]
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub enum RegisteredEvent {
    VirtioBalloonWsReport,
    VirtioBalloonResize,
    VirtioBalloonOOMDeflation,
}

#[cfg(feature = "registered_events")]
#[derive(Serialize, Deserialize, Debug)]
pub enum RegisteredEventWithData {
    VirtioBalloonWsReport {
        ws_buckets: Vec<WSBucket>,
        balloon_actual: u64,
    },
    VirtioBalloonResize,
    VirtioBalloonOOMDeflation,
}

#[cfg(feature = "registered_events")]
impl RegisteredEventWithData {
    pub fn into_event(&self) -> RegisteredEvent {
        match self {
            Self::VirtioBalloonWsReport { .. } => RegisteredEvent::VirtioBalloonWsReport,
            Self::VirtioBalloonResize => RegisteredEvent::VirtioBalloonResize,
            Self::VirtioBalloonOOMDeflation => RegisteredEvent::VirtioBalloonOOMDeflation,
        }
    }

    pub fn into_proto(&self) -> registered_events::RegisteredEvent {
        match self {
            Self::VirtioBalloonWsReport {
                ws_buckets,
                balloon_actual,
            } => {
                let mut report = registered_events::VirtioBalloonWsReport {
                    balloon_actual: *balloon_actual,
                    ..registered_events::VirtioBalloonWsReport::new()
                };
                for ws in ws_buckets {
                    report.ws_buckets.push(registered_events::VirtioWsBucket {
                        age: ws.age,
                        file_bytes: ws.bytes[0],
                        anon_bytes: ws.bytes[1],
                        ..registered_events::VirtioWsBucket::new()
                    });
                }
                let mut event = registered_events::RegisteredEvent::new();
                event.set_ws_report(report);
                event
            }
            Self::VirtioBalloonResize => {
                let mut event = registered_events::RegisteredEvent::new();
                event.set_resize(registered_events::VirtioBalloonResize::new());
                event
            }
            Self::VirtioBalloonOOMDeflation => {
                let mut event = registered_events::RegisteredEvent::new();
                event.set_oom_deflation(registered_events::VirtioBalloonOOMDeflation::new());
                event
            }
        }
    }

    pub fn from_ws(ws: &BalloonWS, balloon_actual: u64) -> Self {
        RegisteredEventWithData::VirtioBalloonWsReport {
            ws_buckets: ws.ws.clone(),
            balloon_actual,
        }
    }
}

pub fn handle_disk_command(command: &DiskControlCommand, disk_host_tube: &Tube) -> VmResponse {
    // Forward the request to the block device process via its control socket.
    if let Err(e) = disk_host_tube.send(command) {
        error!("disk socket send failed: {}", e);
        return VmResponse::Err(SysError::new(EINVAL));
    }

    // Wait for the disk control command to be processed
    match disk_host_tube.recv() {
        Ok(DiskControlResult::Ok) => VmResponse::Ok,
        Ok(DiskControlResult::Err(e)) => VmResponse::Err(e),
        Err(e) => {
            error!("disk socket recv failed: {}", e);
            VmResponse::Err(SysError::new(EINVAL))
        }
    }
}

/// WARNING: descriptor must be a mapping handle on Windows.
fn map_descriptor(
    descriptor: &dyn AsRawDescriptor,
    offset: u64,
    size: u64,
    prot: Protection,
) -> Result<Box<dyn MappedRegion>> {
    let size: usize = size.try_into().map_err(|_e| SysError::new(ERANGE))?;
    match MemoryMappingBuilder::new(size)
        .from_descriptor(descriptor)
        .offset(offset)
        .protection(prot)
        .build()
    {
        Ok(mmap) => Ok(Box::new(mmap)),
        Err(MmapError::SystemCallFailed(e)) => Err(e),
        _ => Err(SysError::new(EINVAL)),
    }
}

// Get vCPU state. vCPUs are expected to all hold the same state.
// In this function, there may be a time where vCPUs are not
fn get_vcpu_state(kick_vcpus: impl Fn(VcpuControl), vcpu_num: usize) -> anyhow::Result<VmRunMode> {
    let (send_chan, recv_chan) = mpsc::channel();
    kick_vcpus(VcpuControl::GetStates(send_chan));
    if vcpu_num == 0 {
        bail!("vcpu_num is zero");
    }
    let mut current_mode_vec: Vec<VmRunMode> = Vec::new();
    for _ in 0..vcpu_num {
        match recv_chan.recv() {
            Ok(state) => current_mode_vec.push(state),
            Err(e) => {
                bail!("Failed to get vCPU state: {}", e);
            }
        };
    }
    let first_state = current_mode_vec[0];
    if first_state == VmRunMode::Exiting {
        panic!("Attempt to snapshot while exiting.");
    }
    if current_mode_vec.iter().any(|x| *x != first_state) {
        // We do not panic here. It could be that vCPUs are transitioning from one mode to another.
        bail!("Unknown VM state: vCPUs hold different states.");
    }
    Ok(first_state)
}

/// A guard to guarantee that all the vCPUs are suspended during the scope.
///
/// When this guard is dropped, it rolls back the state of CPUs.
pub struct VcpuSuspendGuard<'a> {
    saved_run_mode: VmRunMode,
    kick_vcpus: &'a dyn Fn(VcpuControl),
}

impl<'a> VcpuSuspendGuard<'a> {
    /// Check the all vCPU state and suspend the vCPUs if they are running.
    ///
    /// This returns [VcpuSuspendGuard] to rollback the vcpu state.
    ///
    /// # Arguments
    ///
    /// * `kick_vcpus` - A funtion to send [VcpuControl] message to all the vCPUs and interrupt
    ///   them.
    /// * `vcpu_num` - The number of vCPUs.
    pub fn new(kick_vcpus: &'a impl Fn(VcpuControl), vcpu_num: usize) -> anyhow::Result<Self> {
        // get initial vcpu state
        let saved_run_mode = get_vcpu_state(kick_vcpus, vcpu_num)?;
        match saved_run_mode {
            VmRunMode::Running => {
                kick_vcpus(VcpuControl::RunState(VmRunMode::Suspending));
                // Blocking call, waiting for response to ensure vCPU state was updated.
                // In case of failure, where a vCPU still has the state running, start up vcpus and
                // abort operation.
                let current_mode = get_vcpu_state(kick_vcpus, vcpu_num)?;
                if current_mode != VmRunMode::Suspending {
                    kick_vcpus(VcpuControl::RunState(saved_run_mode));
                    bail!("vCPUs failed to all suspend. Kicking back all vCPUs to their previous state: {saved_run_mode}");
                }
            }
            VmRunMode::Suspending => {
                // do nothing. keep the state suspending.
            }
            other => {
                bail!("vcpus are not in running/suspending state, but {}", other);
            }
        };
        Ok(Self {
            saved_run_mode,
            kick_vcpus,
        })
    }
}

impl Drop for VcpuSuspendGuard<'_> {
    fn drop(&mut self) {
        if self.saved_run_mode != VmRunMode::Suspending {
            (self.kick_vcpus)(VcpuControl::RunState(self.saved_run_mode));
        }
    }
}

/// A guard to guarantee that all devices are sleeping during its scope.
///
/// When this guard is dropped, it wakes the devices.
pub struct DeviceSleepGuard<'a> {
    device_control_tube: &'a Tube,
    devices_state: DevicesState,
}

impl<'a> DeviceSleepGuard<'a> {
    fn new(device_control_tube: &'a Tube) -> anyhow::Result<Self> {
        device_control_tube
            .send(&DeviceControlCommand::GetDevicesState)
            .context("send command to devices control socket")?;
        let devices_state = match device_control_tube
            .recv()
            .context("receive from devices control socket")?
        {
            VmResponse::DevicesState(state) => state,
            resp => bail!("failed to get devices state. Unexpected behavior: {}", resp),
        };
        if let DevicesState::Wake = devices_state {
            device_control_tube
                .send(&DeviceControlCommand::SleepDevices)
                .context("send command to devices control socket")?;
            match device_control_tube
                .recv()
                .context("receive from devices control socket")?
            {
                VmResponse::Ok => (),
                resp => bail!("device sleep failed: {}", resp),
            }
        }
        Ok(Self {
            device_control_tube,
            devices_state,
        })
    }
}

impl Drop for DeviceSleepGuard<'_> {
    fn drop(&mut self) {
        if let DevicesState::Wake = self.devices_state {
            if let Err(e) = self
                .device_control_tube
                .send(&DeviceControlCommand::WakeDevices)
            {
                panic!("failed to request device wake after snapshot: {}", e);
            }
            match self.device_control_tube.recv() {
                Ok(VmResponse::Ok) => (),
                Ok(resp) => panic!("unexpected response to device wake request: {}", resp),
                Err(e) => panic!("failed to get reply for device wake request: {}", e),
            }
        }
    }
}

impl VmRequest {
    /// Executes this request on the given Vm and other mutable state.
    ///
    /// This does not return a result, instead encapsulating the success or failure in a
    /// `VmResponse` with the intended purpose of sending the response back over the  socket that
    /// received this `VmRequest`.
    pub fn execute(
        &self,
        run_mode: &mut Option<VmRunMode>,
        disk_host_tubes: &[Tube],
        pm: &mut Option<Arc<Mutex<dyn PmResource + Send>>>,
        #[cfg(feature = "gpu")] gpu_control_tube: Option<&Tube>,
        usb_control_tube: Option<&Tube>,
        bat_control: &mut Option<BatControl>,
        kick_vcpus: impl Fn(VcpuControl),
        kick_vcpu: impl Fn(VcpuControl, usize),
        force_s2idle: bool,
        #[cfg(feature = "swap")] swap_controller: Option<&swap::SwapController>,
        device_control_tube: &Tube,
        vcpu_size: usize,
        irq_handler_control: &Tube,
        snapshot_irqchip: impl Fn() -> anyhow::Result<serde_json::Value>,
        restore_irqchip: impl FnMut(serde_json::Value) -> anyhow::Result<()>,
    ) -> VmResponse {
        match *self {
            VmRequest::Exit => {
                *run_mode = Some(VmRunMode::Exiting);
                VmResponse::Ok
            }
            VmRequest::Powerbtn => {
                if let Some(pm) = pm {
                    pm.lock().pwrbtn_evt();
                    VmResponse::Ok
                } else {
                    error!("{:#?} not supported", *self);
                    VmResponse::Err(SysError::new(ENOTSUP))
                }
            }
            VmRequest::Sleepbtn => {
                if let Some(pm) = pm {
                    pm.lock().slpbtn_evt();
                    VmResponse::Ok
                } else {
                    error!("{:#?} not supported", *self);
                    VmResponse::Err(SysError::new(ENOTSUP))
                }
            }
            VmRequest::Rtc => {
                if let Some(pm) = pm {
                    pm.lock().rtc_evt();
                    VmResponse::Ok
                } else {
                    error!("{:#?} not supported", *self);
                    VmResponse::Err(SysError::new(ENOTSUP))
                }
            }
            VmRequest::SuspendVcpus => {
                *run_mode = Some(VmRunMode::Suspending);
                VmResponse::Ok
            }
            VmRequest::ResumeVcpus => {
                if let Err(e) = device_control_tube.send(&DeviceControlCommand::GetDevicesState) {
                    error!("failed to send GetDevicesState: {}", e);
                    return VmResponse::Err(SysError::new(EIO));
                }
                let devices_state = match device_control_tube.recv() {
                    Ok(VmResponse::DevicesState(state)) => state,
                    Ok(resp) => {
                        error!("failed to get devices state. Unexpected behavior: {}", resp);
                        return VmResponse::Err(SysError::new(EINVAL));
                    }
                    Err(e) => {
                        error!("failed to get devices state. Unexpected behavior: {}", e);
                        return VmResponse::Err(SysError::new(EINVAL));
                    }
                };
                if let DevicesState::Sleep = devices_state {
                    error!("Trying to wake Vcpus while Devices are asleep. Did you mean to use `crosvm resume --full`?");
                    return VmResponse::Err(SysError::new(EINVAL));
                }
                *run_mode = Some(VmRunMode::Running);

                if force_s2idle {
                    // During resume also emulate powerbtn event which will allow to wakeup fully
                    // suspended guest.
                    if let Some(pm) = pm {
                        pm.lock().pwrbtn_evt();
                    } else {
                        error!("triggering power btn during resume not supported");
                        return VmResponse::Err(SysError::new(ENOTSUP));
                    }
                }
                VmResponse::Ok
            }
            VmRequest::Swap(SwapCommand::Enable) => {
                #[cfg(feature = "swap")]
                if let Some(swap_controller) = swap_controller {
                    // Suspend all vcpus and devices while vmm-swap is enabling (move the guest
                    // memory contents to the staging memory) to guarantee no processes other than
                    // the swap monitor process access the guest memory.
                    let _vcpu_guard = match VcpuSuspendGuard::new(&kick_vcpus, vcpu_size) {
                        Ok(guard) => guard,
                        Err(e) => {
                            error!("failed to suspend vcpus: {:?}", e);
                            return VmResponse::Err(SysError::new(EINVAL));
                        }
                    };
                    // TODO(b/253386409): Use `devices::Suspendable::sleep()` instead of sending
                    // `SIGSTOP` signal.
                    let _devices_guard = match swap_controller.suspend_devices() {
                        Ok(guard) => guard,
                        Err(e) => {
                            error!("failed to suspend devices: {:?}", e);
                            return VmResponse::Err(SysError::new(EINVAL));
                        }
                    };

                    return match swap_controller.enable() {
                        Ok(()) => VmResponse::Ok,
                        Err(e) => {
                            error!("swap enable failed: {}", e);
                            VmResponse::Err(SysError::new(EINVAL))
                        }
                    };
                }
                VmResponse::Err(SysError::new(ENOTSUP))
            }
            VmRequest::Swap(SwapCommand::Trim) => {
                #[cfg(feature = "swap")]
                if let Some(swap_controller) = swap_controller {
                    return match swap_controller.trim() {
                        Ok(()) => VmResponse::Ok,
                        Err(e) => {
                            error!("swap trim failed: {}", e);
                            VmResponse::Err(SysError::new(EINVAL))
                        }
                    };
                }
                VmResponse::Err(SysError::new(ENOTSUP))
            }
            VmRequest::Swap(SwapCommand::SwapOut) => {
                #[cfg(feature = "swap")]
                if let Some(swap_controller) = swap_controller {
                    return match swap_controller.swap_out() {
                        Ok(()) => VmResponse::Ok,
                        Err(e) => {
                            error!("swap out failed: {}", e);
                            VmResponse::Err(SysError::new(EINVAL))
                        }
                    };
                }
                VmResponse::Err(SysError::new(ENOTSUP))
            }
            VmRequest::Swap(SwapCommand::Disable {
                #[cfg(feature = "swap")]
                slow_file_cleanup,
                ..
            }) => {
                #[cfg(feature = "swap")]
                if let Some(swap_controller) = swap_controller {
                    return match swap_controller.disable(slow_file_cleanup) {
                        Ok(()) => VmResponse::Ok,
                        Err(e) => {
                            error!("swap disable failed: {}", e);
                            VmResponse::Err(SysError::new(EINVAL))
                        }
                    };
                }
                VmResponse::Err(SysError::new(ENOTSUP))
            }
            VmRequest::Swap(SwapCommand::Status) => {
                #[cfg(feature = "swap")]
                if let Some(swap_controller) = swap_controller {
                    return match swap_controller.status() {
                        Ok(status) => VmResponse::SwapStatus(status),
                        Err(e) => {
                            error!("swap status failed: {}", e);
                            VmResponse::Err(SysError::new(EINVAL))
                        }
                    };
                }
                VmResponse::Err(SysError::new(ENOTSUP))
            }
            VmRequest::SuspendVm => {
                info!("Starting crosvm suspend");
                kick_vcpus(VcpuControl::RunState(VmRunMode::Suspending));
                let current_mode = match get_vcpu_state(kick_vcpus, vcpu_size) {
                    Ok(state) => state,
                    Err(e) => {
                        error!("failed to get vcpu state: {e}");
                        return VmResponse::Err(SysError::new(EIO));
                    }
                };
                if current_mode != VmRunMode::Suspending {
                    error!("vCPUs failed to all suspend.");
                    return VmResponse::Err(SysError::new(EIO));
                }
                if let Err(e) = device_control_tube
                    .send(&DeviceControlCommand::SleepDevices)
                    .context("send command to devices control socket")
                {
                    error!("{:?}", e);
                    return VmResponse::Err(SysError::new(EIO));
                };
                match device_control_tube
                    .recv()
                    .context("receive from devices control socket")
                {
                    Ok(VmResponse::Ok) => {
                        info!("Finished crosvm suspend successfully");
                        VmResponse::Ok
                    }
                    Ok(resp) => {
                        error!("device sleep failed: {}", resp);
                        VmResponse::Err(SysError::new(EIO))
                    }
                    Err(e) => {
                        error!("receive from devices control socket: {:?}", e);
                        VmResponse::Err(SysError::new(EIO))
                    }
                }
            }
            VmRequest::ResumeVm => {
                info!("Starting crosvm resume");
                if let Err(e) = device_control_tube
                    .send(&DeviceControlCommand::WakeDevices)
                    .context("send command to devices control socket")
                {
                    error!("{:?}", e);
                    return VmResponse::Err(SysError::new(EIO));
                };
                match device_control_tube
                    .recv()
                    .context("receive from devices control socket")
                {
                    Ok(VmResponse::Ok) => {
                        info!("Finished crosvm resume successfully");
                    }
                    Ok(resp) => {
                        error!("device wake failed: {}", resp);
                        return VmResponse::Err(SysError::new(EIO));
                    }
                    Err(e) => {
                        error!("receive from devices control socket: {:?}", e);
                        return VmResponse::Err(SysError::new(EIO));
                    }
                }
                kick_vcpus(VcpuControl::RunState(VmRunMode::Running));
                VmResponse::Ok
            }
            VmRequest::Gpe(gpe) => {
                if let Some(pm) = pm.as_ref() {
                    pm.lock().gpe_evt(gpe);
                    VmResponse::Ok
                } else {
                    error!("{:#?} not supported", *self);
                    VmResponse::Err(SysError::new(ENOTSUP))
                }
            }
            VmRequest::PciPme(requester_id) => {
                if let Some(pm) = pm.as_ref() {
                    pm.lock().pme_evt(requester_id);
                    VmResponse::Ok
                } else {
                    error!("{:#?} not supported", *self);
                    VmResponse::Err(SysError::new(ENOTSUP))
                }
            }
            VmRequest::MakeRT => {
                kick_vcpus(VcpuControl::MakeRT);
                VmResponse::Ok
            }
            #[cfg(feature = "balloon")]
            VmRequest::BalloonCommand(_) => unreachable!("Should be handled with BalloonTube"),
            VmRequest::DiskCommand {
                disk_index,
                ref command,
            } => match &disk_host_tubes.get(disk_index) {
                Some(tube) => handle_disk_command(command, tube),
                None => VmResponse::Err(SysError::new(ENODEV)),
            },
            #[cfg(feature = "gpu")]
            VmRequest::GpuCommand(ref cmd) => match gpu_control_tube {
                Some(gpu_control) => {
                    let res = gpu_control.send(cmd);
                    if let Err(e) = res {
                        error!("fail to send command to gpu control socket: {}", e);
                        return VmResponse::Err(SysError::new(EIO));
                    }
                    match gpu_control.recv() {
                        Ok(response) => VmResponse::GpuResponse(response),
                        Err(e) => {
                            error!("fail to recv command from gpu control socket: {}", e);
                            VmResponse::Err(SysError::new(EIO))
                        }
                    }
                }
                None => {
                    error!("gpu control is not enabled in crosvm");
                    VmResponse::Err(SysError::new(EIO))
                }
            },
            VmRequest::UsbCommand(ref cmd) => {
                let usb_control_tube = match usb_control_tube {
                    Some(t) => t,
                    None => {
                        error!("attempted to execute USB request without control tube");
                        return VmResponse::Err(SysError::new(ENODEV));
                    }
                };
                let res = usb_control_tube.send(cmd);
                if let Err(e) = res {
                    error!("fail to send command to usb control socket: {}", e);
                    return VmResponse::Err(SysError::new(EIO));
                }
                match usb_control_tube.recv() {
                    Ok(response) => VmResponse::UsbResponse(response),
                    Err(e) => {
                        error!("fail to recv command from usb control socket: {}", e);
                        VmResponse::Err(SysError::new(EIO))
                    }
                }
            }
            VmRequest::BatCommand(type_, ref cmd) => {
                match bat_control {
                    Some(battery) => {
                        if battery.type_ != type_ {
                            error!("ignored battery command due to battery type: expected {:?}, got {:?}", battery.type_, type_);
                            return VmResponse::Err(SysError::new(EINVAL));
                        }

                        let res = battery.control_tube.send(cmd);
                        if let Err(e) = res {
                            error!("fail to send command to bat control socket: {}", e);
                            return VmResponse::Err(SysError::new(EIO));
                        }

                        match battery.control_tube.recv() {
                            Ok(response) => VmResponse::BatResponse(response),
                            Err(e) => {
                                error!("fail to recv command from bat control socket: {}", e);
                                VmResponse::Err(SysError::new(EIO))
                            }
                        }
                    }
                    None => VmResponse::BatResponse(BatControlResult::NoBatDevice),
                }
            }
            VmRequest::HotPlugVfioCommand { device: _, add: _ } => VmResponse::Ok,
            #[cfg(feature = "pci-hotplug")]
            VmRequest::HotPlugNetCommand(ref _net_cmd) => {
                VmResponse::ErrString("hot plug not supported".to_owned())
            }
            VmRequest::Snapshot(SnapshotCommand::Take { ref snapshot_path }) => {
                info!("Starting crosvm snapshot");
                match do_snapshot(
                    snapshot_path.to_path_buf(),
                    kick_vcpus,
                    irq_handler_control,
                    device_control_tube,
                    vcpu_size,
                    snapshot_irqchip,
                ) {
                    Ok(()) => {
                        info!("Finished crosvm snapshot successfully");
                        VmResponse::Ok
                    }
                    Err(e) => {
                        error!("failed to handle snapshot: {:?}", e);
                        VmResponse::Err(SysError::new(EIO))
                    }
                }
            }
            VmRequest::Restore(RestoreCommand::Apply { ref restore_path }) => {
                info!("Starting crosvm restore");
                match do_restore(
                    restore_path.clone(),
                    kick_vcpus,
                    kick_vcpu,
                    irq_handler_control,
                    device_control_tube,
                    vcpu_size,
                    restore_irqchip,
                ) {
                    Ok(()) => {
                        info!("Finished crosvm restore successfully");
                        VmResponse::Ok
                    }
                    Err(e) => {
                        error!("failed to handle restore: {:?}", e);
                        VmResponse::Err(SysError::new(EIO))
                    }
                }
            }
            #[cfg(feature = "registered_events")]
            VmRequest::RegisterListener {
                socket_addr: _,
                event: _,
            } => VmResponse::Ok,
            #[cfg(feature = "registered_events")]
            VmRequest::UnregisterListener {
                socket_addr: _,
                event: _,
            } => VmResponse::Ok,
            #[cfg(feature = "registered_events")]
            VmRequest::Unregister { socket_addr: _ } => VmResponse::Ok,
        }
    }
}

/// Snapshot the VM to file at `snapshot_path`
fn do_snapshot(
    snapshot_path: PathBuf,
    kick_vcpus: impl Fn(VcpuControl),
    irq_handler_control: &Tube,
    device_control_tube: &Tube,
    vcpu_size: usize,
    snapshot_irqchip: impl Fn() -> anyhow::Result<serde_json::Value>,
) -> anyhow::Result<()> {
    let _vcpu_guard = VcpuSuspendGuard::new(&kick_vcpus, vcpu_size)?;
    let _device_guard = DeviceSleepGuard::new(device_control_tube)?;

    // We want to flush all pending IRQs to the LAPICs. There are two cases:
    //
    // MSIs: these are directly delivered to the LAPIC. We must verify the handler
    // thread cycles once to deliver these interrupts.
    //
    // Legacy interrupts: in the case of a split IRQ chip, these interrupts may
    // flow through the userspace IOAPIC. If the hypervisor does not support
    // irqfds (e.g. WHPX), a single iteration will only flush the IRQ to the
    // IOAPIC. The underlying MSI will be asserted at this point, but if the
    // IRQ handler doesn't run another iteration, it won't be delivered to the
    // LAPIC. This is why we cycle the handler thread twice (doing so ensures we
    // process the underlying MSI).
    //
    // We can handle both of these cases by iterating until there are no tokens
    // serviced on the requested iteration. Note that in the legacy case, this
    // ensures at least two iterations.
    //
    // Note: within CrosVM, *all* interrupts are eventually converted into the
    // same mechanicism that MSIs use. This is why we say "underlying" MSI for
    // a legacy IRQ.
    let mut flush_attempts = 0;
    loop {
        irq_handler_control
            .send(&IrqHandlerRequest::WakeAndNotifyIteration)
            .context("failed to send flush command to IRQ handler thread")?;
        let resp = irq_handler_control
            .recv()
            .context("failed to recv flush response from IRQ handler thread")?;
        match resp {
            IrqHandlerResponse::HandlerIterationComplete(tokens_serviced) => {
                if tokens_serviced == 0 {
                    break;
                }
            }
            _ => bail!("received unexpected reply from IRQ handler: {:?}", resp),
        }
        flush_attempts += 1;
        if flush_attempts > EXPECTED_MAX_IRQ_FLUSH_ITERATIONS {
            warn!("flushing IRQs for snapshot may be stalled after iteration {}, expected <= {} iterations", flush_attempts, EXPECTED_MAX_IRQ_FLUSH_ITERATIONS);
        }
    }
    info!("flushed IRQs in {} iterations", flush_attempts);

    // Snapshot Vcpus
    let vcpu_path = snapshot_path.with_extension("vcpu");
    let cpu_file = File::create(&vcpu_path)
        .with_context(|| format!("failed to open path {}", vcpu_path.display()))?;
    let (send_chan, recv_chan) = mpsc::channel();
    kick_vcpus(VcpuControl::Snapshot(send_chan));
    // Validate all Vcpus snapshot successfully
    let mut cpu_vec = Vec::with_capacity(vcpu_size);
    for _ in 0..vcpu_size {
        match recv_chan
            .recv()
            .context("Failed to snapshot Vcpu, aborting snapshot")?
        {
            Ok(snap) => {
                cpu_vec.push(snap);
            }
            Err(e) => bail!("Failed to snapshot Vcpu, aborting snapshot: {}", e),
        }
    }
    serde_json::to_writer(cpu_file, &cpu_vec).expect("Failed to write Vcpu state");

    // Snapshot irqchip
    let irqchip_path = snapshot_path.with_extension("irqchip");
    let irqchip_file = File::create(&irqchip_path)
        .with_context(|| format!("failed to open path {}", irqchip_path.display()))?;
    let irqchip_snap = snapshot_irqchip()?;
    serde_json::to_writer(irqchip_file, &irqchip_snap).expect("Failed to write irqchip state");

    // Snapshot devices
    device_control_tube
        .send(&DeviceControlCommand::SnapshotDevices { snapshot_path })
        .context("send command to devices control socket")?;
    let resp: VmResponse = device_control_tube
        .recv()
        .context("receive from devices control socket")?;
    if !matches!(resp, VmResponse::Ok) {
        bail!("unexpected SnapshotDevices response: {resp}");
    }
    Ok(())
}

/// Restore the VM to the snapshot at `restore_path`.
///
/// Same as `VmRequest::execute` with a `VmRequest::Restore`. Exposed as a separate function
/// because not all the `VmRequest::execute` arguments are available in the "cold restore" flow.
pub fn do_restore(
    restore_path: PathBuf,
    kick_vcpus: impl Fn(VcpuControl),
    kick_vcpu: impl Fn(VcpuControl, usize),
    irq_handler_control: &Tube,
    device_control_tube: &Tube,
    vcpu_size: usize,
    mut restore_irqchip: impl FnMut(serde_json::Value) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let _guard = VcpuSuspendGuard::new(&kick_vcpus, vcpu_size);
    let _devices_guard = DeviceSleepGuard::new(device_control_tube)?;

    // Restore IrqChip
    let irq_path = restore_path.with_extension("irqchip");
    let irq_file = File::open(&irq_path)
        .with_context(|| format!("failed to open path {}", irq_path.display()))?;
    let irq_snapshot: serde_json::Value = serde_json::from_reader(irq_file)?;
    restore_irqchip(irq_snapshot)?;

    // Restore Vcpu(s)
    let vcpu_path = restore_path.with_extension("vcpu");
    let cpu_file = File::open(&vcpu_path)
        .with_context(|| format!("failed to open path {}", vcpu_path.display()))?;
    let vcpu_snapshots: Vec<VcpuSnapshot> = serde_json::from_reader(cpu_file)?;
    if vcpu_snapshots.len() != vcpu_size {
        bail!(
            "bad cpu count in snapshot: expected={} got={}",
            vcpu_size,
            vcpu_snapshots.len()
        );
    }

    #[cfg(target_arch = "x86_64")]
    let host_tsc_reference_moment = {
        // SAFETY: rdtsc takes no arguments.
        unsafe { _rdtsc() }
    };
    let (send_chan, recv_chan) = mpsc::channel();
    for vcpu_snap in vcpu_snapshots {
        let vcpu_id = vcpu_snap.vcpu_id;
        kick_vcpu(
            VcpuControl::Restore(VcpuRestoreRequest {
                result_sender: send_chan.clone(),
                snapshot: Box::new(vcpu_snap),
                #[cfg(target_arch = "x86_64")]
                host_tsc_reference_moment,
            }),
            vcpu_id,
        );
    }
    for _ in 0..vcpu_size {
        recv_chan
            .recv()
            .context("Failed to recv restore response")?
            .context("Failed to restore vcpu")?;
    }

    // Restore devices
    device_control_tube
        .send(&DeviceControlCommand::RestoreDevices { restore_path })
        .context("send command to devices control socket")?;
    let resp: VmResponse = device_control_tube
        .recv()
        .context("receive from devices control socket")?;
    if !matches!(resp, VmResponse::Ok) {
        bail!("unexpected RestoreDevices response: {resp}");
    }

    irq_handler_control
        .send(&IrqHandlerRequest::RefreshIrqEventTokens)
        .context("failed to send refresh irq event token command to IRQ handler thread")?;
    let resp: IrqHandlerResponse = irq_handler_control
        .recv()
        .context("failed to recv refresh response from IRQ handler thread")?;
    if !matches!(resp, IrqHandlerResponse::IrqEventTokenRefreshComplete) {
        bail!(
            "received unexpected reply from IRQ handler thread: {:?}",
            resp
        );
    }
    Ok(())
}

/// Indication of success or failure of a `VmRequest`.
///
/// Success is usually indicated `VmResponse::Ok` unless there is data associated with the response.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[must_use]
pub enum VmResponse {
    /// Indicates the request was executed successfully.
    Ok,
    /// Indicates the request encountered some error during execution.
    Err(SysError),
    /// Indicates the request encountered some error during execution.
    ErrString(String),
    /// The request to register memory into guest address space was successfully done at page frame
    /// number `pfn` and memory slot number `slot`.
    RegisterMemory { pfn: u64, slot: u32 },
    /// Results of balloon control commands.
    #[cfg(feature = "balloon")]
    BalloonStats {
        stats: BalloonStats,
        balloon_actual: u64,
    },
    /// Results of balloon WS-R command
    #[cfg(feature = "balloon")]
    BalloonWS { ws: BalloonWS, balloon_actual: u64 },
    /// Results of PCI hot plug
    #[cfg(feature = "pci-hotplug")]
    PciHotPlugResponse { bus: u8 },
    /// Results of usb control commands.
    UsbResponse(UsbControlResult),
    #[cfg(feature = "gpu")]
    /// Results of gpu control commands.
    GpuResponse(GpuControlResult),
    /// Results of battery control commands.
    BatResponse(BatControlResult),
    /// Results of swap status command.
    SwapStatus(SwapStatus),
    /// Gets the state of Devices (sleep/wake)
    DevicesState(DevicesState),
}

impl Display for VmResponse {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::VmResponse::*;

        match self {
            Ok => write!(f, "ok"),
            Err(e) => write!(f, "error: {}", e),
            ErrString(e) => write!(f, "error: {}", e),
            RegisterMemory { pfn, slot } => write!(
                f,
                "memory registered to page frame number {:#x} and memory slot {}",
                pfn, slot
            ),
            #[cfg(feature = "balloon")]
            VmResponse::BalloonStats {
                stats,
                balloon_actual,
            } => {
                write!(
                    f,
                    "stats: {}\nballoon_actual: {}",
                    serde_json::to_string_pretty(&stats)
                        .unwrap_or_else(|_| "invalid_response".to_string()),
                    balloon_actual
                )
            }
            #[cfg(feature = "balloon")]
            VmResponse::BalloonWS { ws, balloon_actual } => {
                write!(
                    f,
                    "ws: {}, balloon_actual: {}",
                    serde_json::to_string_pretty(&ws)
                        .unwrap_or_else(|_| "invalid_response".to_string()),
                    balloon_actual,
                )
            }
            UsbResponse(result) => write!(f, "usb control request get result {:?}", result),
            #[cfg(feature = "pci-hotplug")]
            PciHotPlugResponse { bus } => write!(f, "pci hotplug bus {:?}", bus),
            #[cfg(feature = "gpu")]
            GpuResponse(result) => write!(f, "gpu control request result {:?}", result),
            BatResponse(result) => write!(f, "{}", result),
            SwapStatus(status) => {
                write!(
                    f,
                    "{}",
                    serde_json::to_string(&status)
                        .unwrap_or_else(|_| "invalid_response".to_string()),
                )
            }
            DevicesState(status) => write!(f, "devices status: {:?}", status),
        }
    }
}

/// Enum that allows remote control of a wait context (used between the Windows GpuDisplay & the
/// GPU worker).
#[derive(Serialize, Deserialize)]
pub enum ModifyWaitContext {
    Add(#[serde(with = "with_as_descriptor")] Descriptor),
}

#[sorted]
#[derive(Error, Debug)]
pub enum VirtioIOMMUVfioError {
    #[error("socket failed")]
    SocketFailed,
    #[error("unexpected response: {0}")]
    UnexpectedResponse(VirtioIOMMUResponse),
    #[error("unknown command: `{0}`")]
    UnknownCommand(String),
    #[error("{0}")]
    VfioControl(VirtioIOMMUVfioResult),
}

#[derive(Serialize, Deserialize, Debug)]
pub enum VirtioIOMMUVfioCommand {
    // Add the vfio device attached to virtio-iommu.
    VfioDeviceAdd {
        endpoint_addr: u32,
        wrapper_id: u32,
        #[serde(with = "with_as_descriptor")]
        container: File,
    },
    // Delete the vfio device attached to virtio-iommu.
    VfioDeviceDel {
        endpoint_addr: u32,
    },
    // Map a dma-buf into vfio iommu table
    VfioDmabufMap {
        mem_slot: MemSlot,
        gfn: u64,
        size: u64,
        dma_buf: SafeDescriptor,
    },
    // Unmap a dma-buf from vfio iommu table
    VfioDmabufUnmap(MemSlot),
}

#[derive(Serialize, Deserialize, Debug)]
pub enum VirtioIOMMUVfioResult {
    Ok,
    NotInPCIRanges,
    NoAvailableContainer,
    NoSuchDevice,
    NoSuchMappedDmabuf,
    InvalidParam,
}

impl Display for VirtioIOMMUVfioResult {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::VirtioIOMMUVfioResult::*;

        match self {
            Ok => write!(f, "successfully"),
            NotInPCIRanges => write!(f, "not in the pci ranges of virtio-iommu"),
            NoAvailableContainer => write!(f, "no available vfio container"),
            NoSuchDevice => write!(f, "no such a vfio device"),
            NoSuchMappedDmabuf => write!(f, "no such a mapped dmabuf"),
            InvalidParam => write!(f, "invalid parameters"),
        }
    }
}

/// A request to the virtio-iommu process to perform some operations.
///
/// Unless otherwise noted, each request should expect a `VirtioIOMMUResponse::Ok` to be received on
/// success.
#[derive(Serialize, Deserialize, Debug)]
pub enum VirtioIOMMURequest {
    /// Command for vfio related operations.
    VfioCommand(VirtioIOMMUVfioCommand),
}

/// Indication of success or failure of a `VirtioIOMMURequest`.
///
/// Success is usually indicated `VirtioIOMMUResponse::Ok` unless there is data associated with the
/// response.
#[derive(Serialize, Deserialize, Debug)]
pub enum VirtioIOMMUResponse {
    /// Indicates the request was executed successfully.
    Ok,
    /// Indicates the request encountered some error during execution.
    Err(SysError),
    /// Results for Vfio commands.
    VfioResponse(VirtioIOMMUVfioResult),
}

impl Display for VirtioIOMMUResponse {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::VirtioIOMMUResponse::*;
        match self {
            Ok => write!(f, "ok"),
            Err(e) => write!(f, "error: {}", e),
            VfioResponse(result) => write!(
                f,
                "The vfio-related virtio-iommu request got result: {:?}",
                result
            ),
        }
    }
}

/// Send VirtioIOMMURequest without waiting for the response
pub fn virtio_iommu_request_async(
    iommu_control_tube: &Tube,
    req: &VirtioIOMMURequest,
) -> VirtioIOMMUResponse {
    match iommu_control_tube.send(&req) {
        Ok(_) => VirtioIOMMUResponse::Ok,
        Err(e) => {
            error!("virtio-iommu socket send failed: {:?}", e);
            VirtioIOMMUResponse::Err(SysError::last())
        }
    }
}

pub type VirtioIOMMURequestResult = std::result::Result<VirtioIOMMUResponse, ()>;

/// Send VirtioIOMMURequest and wait to get the response
pub fn virtio_iommu_request(
    iommu_control_tube: &Tube,
    req: &VirtioIOMMURequest,
) -> VirtioIOMMURequestResult {
    let response = match virtio_iommu_request_async(iommu_control_tube, req) {
        VirtioIOMMUResponse::Ok => match iommu_control_tube.recv() {
            Ok(response) => response,
            Err(e) => {
                error!("virtio-iommu socket recv failed: {:?}", e);
                VirtioIOMMUResponse::Err(SysError::last())
            }
        },
        resp => resp,
    };
    Ok(response)
}
