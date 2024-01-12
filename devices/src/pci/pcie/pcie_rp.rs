// Copyright 2021 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::BTreeMap;

use anyhow::Context;
use anyhow::Result;
use vm_control::GpeNotify;
use vm_control::PmeNotify;

use crate::bus::HotPlugBus;
use crate::bus::HotPlugKey;
use crate::pci::pcie::pcie_host::PcieHostPort;
use crate::pci::pcie::pcie_port::PciePort;
use crate::pci::pcie::pcie_port::PciePortVariant;
use crate::pci::pcie::*;
use crate::pci::PciAddress;

const PCIE_RP_DID: u16 = 0x3420;
pub struct PcieRootPort {
    pcie_port: PciePort,
    downstream_devices: BTreeMap<PciAddress, HotPlugKey>,
    hotplug_out_begin: bool,
    removed_downstream: Vec<PciAddress>,
}

impl PcieRootPort {
    /// Constructs a new PCIE root port
    pub fn new(secondary_bus_num: u8, slot_implemented: bool) -> Self {
        PcieRootPort {
            pcie_port: PciePort::new(
                PCIE_RP_DID,
                "PcieRootPort".to_string(),
                0,
                secondary_bus_num,
                slot_implemented,
                PcieDevicePortType::RootPort,
            ),
            downstream_devices: BTreeMap::new(),
            hotplug_out_begin: false,
            removed_downstream: Vec::new(),
        }
    }

    /// Constructs a new PCIE root port which associated with the host physical pcie RP
    pub fn new_from_host(pcie_host: PcieHostPort, slot_implemented: bool) -> Result<Self> {
        Ok(PcieRootPort {
            pcie_port: PciePort::new_from_host(
                pcie_host,
                slot_implemented,
                PcieDevicePortType::RootPort,
            )
            .context("PciePort::new_from_host failed")?,
            downstream_devices: BTreeMap::new(),
            hotplug_out_begin: false,
            removed_downstream: Vec::new(),
        })
    }
}

impl PciePortVariant for PcieRootPort {
    fn get_pcie_port(&self) -> &PciePort {
        &self.pcie_port
    }

    fn get_pcie_port_mut(&mut self) -> &mut PciePort {
        &mut self.pcie_port
    }

    fn get_removed_devices_impl(&self) -> Vec<PciAddress> {
        if self.pcie_port.removed_downstream_valid() {
            self.removed_downstream.clone()
        } else {
            Vec::new()
        }
    }

    fn hotplug_implemented_impl(&self) -> bool {
        self.pcie_port.hotplug_implemented()
    }

    fn hotplugged_impl(&self) -> bool {
        false
    }
}

impl HotPlugBus for PcieRootPort {
    fn hot_plug(&mut self, addr: PciAddress) {
        if self.downstream_devices.get(&addr).is_none() {
            return;
        }

        self.pcie_port
            .set_slot_status(PCIE_SLTSTA_PDS | PCIE_SLTSTA_ABP);
        self.pcie_port.trigger_hp_or_pme_interrupt();
    }

    fn hot_unplug(&mut self, addr: PciAddress) {
        if self.downstream_devices.remove(&addr).is_none() {
            return;
        }

        if !self.hotplug_out_begin {
            self.removed_downstream.clear();
            self.removed_downstream.push(addr);
            // All the remaine devices will be removed also in this hotplug out interrupt
            for (guest_pci_addr, _) in self.downstream_devices.iter() {
                self.removed_downstream.push(*guest_pci_addr);
            }

            self.pcie_port.set_slot_status(PCIE_SLTSTA_ABP);
            self.pcie_port.trigger_hp_or_pme_interrupt();

            if self.pcie_port.is_host() {
                self.pcie_port.hot_unplug()
            }
        }

        self.hotplug_out_begin = true;
    }

    fn get_address(&self) -> Option<PciAddress> {
        self.pcie_port.get_address()
    }

    fn get_secondary_bus_number(&self) -> Option<u8> {
        Some(self.pcie_port.get_bus_range()?.secondary)
    }

    fn is_match(&self, host_addr: PciAddress) -> Option<u8> {
        self.pcie_port.is_match(host_addr)
    }

    fn add_hotplug_device(&mut self, hotplug_key: HotPlugKey, guest_addr: PciAddress) {
        if !self.pcie_port.hotplug_implemented() {
            return;
        }

        // Begin the next round hotplug in process
        if self.hotplug_out_begin {
            self.hotplug_out_begin = false;
            self.downstream_devices.clear();
            self.removed_downstream.clear();
        }

        self.downstream_devices.insert(guest_addr, hotplug_key);
    }

    fn get_hotplug_device(&self, hotplug_key: HotPlugKey) -> Option<PciAddress> {
        for (guest_address, host_info) in self.downstream_devices.iter() {
            if hotplug_key == *host_info {
                return Some(*guest_address);
            }
        }
        None
    }

    fn is_empty(&self) -> bool {
        self.downstream_devices.is_empty()
    }

    fn get_hotplug_key(&self) -> Option<HotPlugKey> {
        None
    }
}

impl GpeNotify for PcieRootPort {
    fn notify(&mut self) {
        if !self.pcie_port.hotplug_implemented() {
            return;
        }

        if self.pcie_port.is_host() {
            self.pcie_port.prepare_hotplug();
        }

        if self.pcie_port.should_trigger_pme() {
            self.pcie_port
                .inject_pme(self.pcie_port.get_address().unwrap().pme_requester_id());
        }
    }
}

impl PmeNotify for PcieRootPort {
    fn notify(&mut self, requester_id: u16) {
        self.pcie_port.inject_pme(requester_id);
    }
}
