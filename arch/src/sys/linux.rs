// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::BTreeMap;
use std::sync::Arc;

use acpi_tables::aml::Aml;
use base::syslog;
use base::AsRawDescriptors;
use base::Tube;
use devices::Bus;
use devices::BusDevice;
use devices::IommuDevType;
use devices::IrqChip;
use devices::IrqEventSource;
use devices::ProxyDevice;
use devices::VfioPlatformDevice;
use hypervisor::ProtectionType;
use hypervisor::Vm;
use minijail::Minijail;
use resources::AllocOptions;
use resources::SystemAllocator;
use sync::Mutex;

use crate::DeviceRegistrationError;

/// Adds goldfish battery and returns the platform needed resources including
/// its AML data and mmio base address
///
/// # Arguments
///
/// * `amls` - the vector to put the goldfish battery AML
/// * `battery_jail` - used when sandbox is enabled
/// * `mmio_bus` - bus to add the devices to
/// * `irq_chip` - the IrqChip object for registering irq events
/// * `irq_num` - assigned interrupt to use
/// * `resources` - the SystemAllocator to allocate IO and MMIO for acpi
pub fn add_goldfish_battery(
    amls: &mut Vec<u8>,
    battery_jail: Option<Minijail>,
    mmio_bus: &Bus,
    irq_chip: &mut dyn IrqChip,
    irq_num: u32,
    resources: &mut SystemAllocator,
    #[cfg(feature = "swap")] swap_controller: &mut Option<swap::SwapController>,
) -> Result<(Tube, u64), DeviceRegistrationError> {
    let alloc = resources.get_anon_alloc();
    let mmio_base = resources
        .allocate_mmio(
            devices::bat::GOLDFISHBAT_MMIO_LEN,
            alloc,
            "GoldfishBattery".to_string(),
            AllocOptions::new().align(devices::bat::GOLDFISHBAT_MMIO_LEN),
        )
        .map_err(DeviceRegistrationError::AllocateIoResource)?;

    let (control_tube, response_tube) =
        Tube::pair().map_err(DeviceRegistrationError::CreateTube)?;

    #[cfg(feature = "power-monitor-powerd")]
    let create_monitor = Some(Box::new(power_monitor::powerd::DBusMonitor::connect)
        as Box<dyn power_monitor::CreatePowerMonitorFn>);

    #[cfg(not(feature = "power-monitor-powerd"))]
    let create_monitor = None;

    let irq_evt = devices::IrqLevelEvent::new().map_err(DeviceRegistrationError::EventCreate)?;

    let goldfish_bat = devices::GoldfishBattery::new(
        mmio_base,
        irq_num,
        irq_evt
            .try_clone()
            .map_err(DeviceRegistrationError::EventClone)?,
        response_tube,
        create_monitor,
    )
    .map_err(DeviceRegistrationError::RegisterBattery)?;
    goldfish_bat.to_aml_bytes(amls);

    irq_chip
        .register_level_irq_event(
            irq_num,
            &irq_evt,
            IrqEventSource::from_device(&goldfish_bat),
        )
        .map_err(DeviceRegistrationError::RegisterIrqfd)?;

    match battery_jail {
        #[cfg(not(windows))]
        Some(jail) => {
            let mut keep_rds = goldfish_bat.keep_rds();
            syslog::push_descriptors(&mut keep_rds);
            cros_tracing::push_descriptors!(&mut keep_rds);
            mmio_bus
                .insert(
                    Arc::new(Mutex::new(
                        ProxyDevice::new(
                            goldfish_bat,
                            jail,
                            keep_rds,
                            #[cfg(feature = "swap")]
                            swap_controller,
                        )
                        .map_err(DeviceRegistrationError::ProxyDeviceCreation)?,
                    )),
                    mmio_base,
                    devices::bat::GOLDFISHBAT_MMIO_LEN,
                )
                .map_err(DeviceRegistrationError::MmioInsert)?;
        }
        #[cfg(windows)]
        Some(_) => {}
        None => {
            mmio_bus
                .insert(
                    Arc::new(Mutex::new(goldfish_bat)),
                    mmio_base,
                    devices::bat::GOLDFISHBAT_MMIO_LEN,
                )
                .map_err(DeviceRegistrationError::MmioInsert)?;
        }
    }

    Ok((control_tube, mmio_base))
}

pub struct PlatformBusResources {
    pub dt_symbol: String,        // DT symbol (label) assigned to the device
    pub regions: Vec<(u64, u64)>, // (start address, size)
    pub irqs: Vec<(u32, u32)>,    // (IRQ number, flags)
    pub iommus: Vec<(IommuDevType, Option<u32>, Vec<u32>)>, // (IOMMU type, IOMMU identifier, IDs)
}

impl PlatformBusResources {
    const IRQ_TRIGGER_EDGE: u32 = 1;
    const IRQ_TRIGGER_LEVEL: u32 = 4;

    fn new(symbol: String) -> Self {
        Self {
            dt_symbol: symbol,
            regions: vec![],
            irqs: vec![],
            iommus: vec![],
        }
    }
}

/// Creates a platform device for use by this Vm.
#[cfg(any(target_os = "android", target_os = "linux"))]
pub fn generate_platform_bus(
    devices: Vec<(VfioPlatformDevice, Option<Minijail>)>,
    irq_chip: &mut dyn IrqChip,
    mmio_bus: &Bus,
    resources: &mut SystemAllocator,
    vm: &mut impl Vm,
    #[cfg(feature = "swap")] swap_controller: &mut Option<swap::SwapController>,
    protection_type: ProtectionType,
) -> Result<
    (
        Vec<Arc<Mutex<dyn BusDevice>>>,
        BTreeMap<u32, String>,
        Vec<PlatformBusResources>,
    ),
    DeviceRegistrationError,
> {
    let mut platform_devices = Vec::new();
    let mut pid_labels = BTreeMap::new();
    let mut bus_dev_resources = vec![];

    // Allocate ranges that may need to be in the Platform MMIO region (MmioType::Platform).
    for (mut device, jail) in devices.into_iter() {
        let dt_symbol = device
            .dt_symbol()
            .ok_or(DeviceRegistrationError::MissingDeviceTreeSymbol)?
            .to_owned();
        let mut device_resources = PlatformBusResources::new(dt_symbol);
        let ranges = device
            .allocate_regions(resources)
            .map_err(DeviceRegistrationError::AllocateIoResource)?;

        // If guest memory is private, don't wait for the first access to mmap the device.
        if protection_type.isolates_memory() {
            device.regions_mmap_early(vm);
        }

        let mut keep_rds = device.keep_rds();
        syslog::push_descriptors(&mut keep_rds);
        cros_tracing::push_descriptors!(&mut keep_rds);

        let irqs = device
            .get_platform_irqs()
            .map_err(DeviceRegistrationError::AllocateIrqResource)?;
        for irq in irqs.into_iter() {
            let irq_num = resources
                .allocate_irq()
                .ok_or(DeviceRegistrationError::AllocateIrq)?;

            if device.irq_is_automask(&irq) {
                let irq_evt =
                    devices::IrqLevelEvent::new().map_err(DeviceRegistrationError::EventCreate)?;
                irq_chip
                    .register_level_irq_event(
                        irq_num,
                        &irq_evt,
                        IrqEventSource::from_device(&device),
                    )
                    .map_err(DeviceRegistrationError::RegisterIrqfd)?;
                device
                    .assign_level_platform_irq(&irq_evt, irq.index)
                    .map_err(DeviceRegistrationError::SetupVfioPlatformIrq)?;
                keep_rds.extend(irq_evt.as_raw_descriptors());
                device_resources
                    .irqs
                    .push((irq_num, PlatformBusResources::IRQ_TRIGGER_LEVEL));
            } else {
                let irq_evt =
                    devices::IrqEdgeEvent::new().map_err(DeviceRegistrationError::EventCreate)?;
                irq_chip
                    .register_edge_irq_event(
                        irq_num,
                        &irq_evt,
                        IrqEventSource::from_device(&device),
                    )
                    .map_err(DeviceRegistrationError::RegisterIrqfd)?;
                device
                    .assign_edge_platform_irq(&irq_evt, irq.index)
                    .map_err(DeviceRegistrationError::SetupVfioPlatformIrq)?;
                keep_rds.extend(irq_evt.as_raw_descriptors());
                device_resources
                    .irqs
                    .push((irq_num, PlatformBusResources::IRQ_TRIGGER_EDGE));
            }
        }

        if let Some((iommu_type, id, vsids)) = device.iommu() {
            // We currently only support one IOMMU per VFIO device.
            device_resources
                .iommus
                .push((iommu_type, id, vsids.to_vec()));
        }

        let arced_dev: Arc<Mutex<dyn BusDevice>> = if let Some(jail) = jail {
            let proxy = ProxyDevice::new(
                device,
                jail,
                keep_rds,
                #[cfg(feature = "swap")]
                swap_controller,
            )
            .map_err(DeviceRegistrationError::ProxyDeviceCreation)?;
            pid_labels.insert(proxy.pid() as u32, proxy.debug_label());
            Arc::new(Mutex::new(proxy))
        } else {
            device.on_sandboxed();
            Arc::new(Mutex::new(device))
        };
        platform_devices.push(arced_dev.clone());
        for range in &ranges {
            mmio_bus
                .insert(arced_dev.clone(), range.0, range.1)
                .map_err(DeviceRegistrationError::MmioInsert)?;
            device_resources.regions.push((range.0, range.1));
        }
        bus_dev_resources.push(device_resources);
    }
    Ok((platform_devices, pid_labels, bus_dev_resources))
}
