// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::Arc;
use sync::Mutex;

use hypervisor::kvm::{KvmVcpu, KvmVm};
use hypervisor::{IoapicState, LapicState, PicSelect, PicState, PitState, Vm};
use kvm_sys::*;
use sys_util::Result;

use crate::{Bus, IrqChipX86_64};

/// IrqChip implementation where the entire IrqChip is emulated by KVM.
///
/// This implementation will use the KVM API to create and configure the in-kernel irqchip.
pub struct KvmKernelIrqChip {
    pub(super) vm: KvmVm,
    pub(super) vcpus: Arc<Mutex<Vec<Option<KvmVcpu>>>>,
}

impl KvmKernelIrqChip {
    /// Construct a new KvmKernelIrqchip.
    pub fn new(vm: KvmVm, num_vcpus: usize) -> Result<KvmKernelIrqChip> {
        vm.create_irq_chip()?;

        Ok(KvmKernelIrqChip {
            vm,
            vcpus: Arc::new(Mutex::new((0..num_vcpus).map(|_| None).collect())),
        })
    }
    /// Attempt to create a shallow clone of this x86_64 KvmKernelIrqChip instance.
    pub(super) fn arch_try_clone(&self) -> Result<Self> {
        Ok(KvmKernelIrqChip {
            vm: self.vm.try_clone()?,
            vcpus: self.vcpus.clone(),
        })
    }
}

impl IrqChipX86_64<KvmVcpu> for KvmKernelIrqChip {
    /// Get the current state of the PIC
    fn get_pic_state(&self, select: PicSelect) -> Result<PicState> {
        Ok(PicState::from(&self.vm.get_pic_state(select)?))
    }

    /// Set the current state of the PIC
    fn set_pic_state(&mut self, select: PicSelect, state: &PicState) -> Result<()> {
        self.vm.set_pic_state(select, &kvm_pic_state::from(state))
    }

    /// Get the current state of the IOAPIC
    fn get_ioapic_state(&self) -> Result<IoapicState> {
        Ok(IoapicState::from(&self.vm.get_ioapic_state()?))
    }

    /// Set the current state of the IOAPIC
    fn set_ioapic_state(&mut self, state: &IoapicState) -> Result<()> {
        self.vm.set_ioapic_state(&kvm_ioapic_state::from(state))
    }

    /// Get the current state of the specified VCPU's local APIC
    fn get_lapic_state(&self, _vcpu_id: usize) -> Result<LapicState> {
        unimplemented!("get_lapic_state for KvmKernelIrqChip is not yet implemented");
    }

    /// Set the current state of the specified VCPU's local APIC
    fn set_lapic_state(&mut self, _vcpu_id: usize, _state: &LapicState) -> Result<()> {
        unimplemented!("set_lapic_state for KvmKernelIrqChip is not yet implemented");
    }

    /// Create a PIT (Programmable Interval Timer) for this VM.
    /// The KvmKernelIrqchip creates the PIT by calling the KVM_CREATE_PIT2 KVM API. The
    /// io_bus is not used in this case because KVM handles intercepting port-mapped io intended
    /// for the PIT.
    fn create_pit(&mut self, _io_bus: &mut Bus) -> Result<()> {
        self.vm.create_pit()
    }

    /// Retrieves the state of the PIT. Gets the pit state via the KVM API.
    fn get_pit(&self) -> Result<PitState> {
        Ok(PitState::from(&self.vm.get_pit_state()?))
    }

    /// Sets the state of the PIT. Sets the pit state via the KVM API.
    fn set_pit(&mut self, state: &PitState) -> Result<()> {
        self.vm.set_pit_state(&kvm_pit_state2::from(state))
    }
}

#[cfg(test)]
mod tests {

    use hypervisor::kvm::{Kvm, KvmVm};
    use sys_util::GuestMemory;

    use crate::irqchip::{IrqChip, IrqChipX86_64, KvmKernelIrqChip};
    use crate::Bus;

    use hypervisor::{PicSelect, Vm, VmX86_64};

    fn get_chip() -> (KvmKernelIrqChip, KvmVm) {
        let kvm = Kvm::new().expect("failed to instantiate Kvm");
        let mem = GuestMemory::new(&[]).unwrap();
        let vm = KvmVm::new(&kvm, mem).expect("failed tso instantiate vm");
        let vcpu = vm.create_vcpu(0).expect("failed to instantiate vcpu");

        let mut chip = KvmKernelIrqChip::new(vm.try_clone().expect("failed to clone vm"), 1)
            .expect("failed to instantiate KvmKernelIrqChip");

        chip.add_vcpu(0, vcpu).expect("failed to add vcpu");

        (chip, vm)
    }

    #[test]
    fn get_pic() {
        let (chip, vm) = get_chip();

        let state = chip
            .get_pic_state(PicSelect::Primary)
            .expect("could not get pic state");

        // Default is that no irq lines are asserted
        assert_eq!(state.irr, 0);

        // Assert Irq Line 0
        vm.set_irq_line(0, true).expect("could not set irq line");

        let state = chip
            .get_pic_state(PicSelect::Primary)
            .expect("could not get pic state");

        // Bit 0 should now be 1
        assert_eq!(state.irr, 1);
    }

    #[test]
    fn set_pic() {
        let (mut chip, _) = get_chip();

        let mut state = chip
            .get_pic_state(PicSelect::Primary)
            .expect("could not get pic state");

        // set bits 0 and 1
        state.irr = 3;

        chip.set_pic_state(PicSelect::Primary, &state)
            .expect("could not set the pic state");

        let state = chip
            .get_pic_state(PicSelect::Primary)
            .expect("could not get pic state");

        // Bits 1 and 0 should now be 1
        assert_eq!(state.irr, 3);
    }

    #[test]
    fn get_ioapic() {
        let (chip, vm) = get_chip();

        let state = chip.get_ioapic_state().expect("could not get ioapic state");

        // Default is that no irq lines are asserted
        assert_eq!(state.current_interrupt_level_bitmap, 0);

        // Default routing entries has routes 0..24 routed to vectors 0..24
        for i in 0..24 {
            // when the ioapic is reset by kvm, it defaults to all zeroes except the
            // interrupt mask is set to 1, which is bit 16
            assert_eq!(state.redirect_table[i].get(0, 64), 1 << 16);
        }

        // Assert Irq Line 1
        vm.set_irq_line(1, true).expect("could not set irq line");

        let state = chip.get_ioapic_state().expect("could not get ioapic state");

        // Bit 1 should now be 1
        assert_eq!(state.current_interrupt_level_bitmap, 2);
    }

    #[test]
    fn set_ioapic() {
        let (mut chip, _) = get_chip();

        let mut state = chip.get_ioapic_state().expect("could not get ioapic state");

        // set a vector in the redirect table
        state.redirect_table[2].set_vector(15);
        // set the irq line status on that entry
        state.current_interrupt_level_bitmap = 4;

        chip.set_ioapic_state(&state)
            .expect("could not set the ioapic state");

        let state = chip.get_ioapic_state().expect("could not get ioapic state");

        // verify that get_ioapic_state returns what we set
        assert_eq!(state.redirect_table[2].get_vector(), 15);
        assert_eq!(state.current_interrupt_level_bitmap, 4);
    }

    #[test]
    fn get_pit() {
        let (mut chip, _) = get_chip();
        let mut io_bus = Bus::new();
        chip.create_pit(&mut io_bus).expect("failed to create pit");

        let state = chip.get_pit().expect("failed to get pit state");

        assert_eq!(state.flags, 0);
        // assert reset state of pit
        for i in 0..3 {
            // initial count of 0 sets it to 0x10000;
            assert_eq!(state.channels[i].count, 0x10000);
            assert_eq!(state.channels[i].mode, 0xff);
            assert_eq!(state.channels[i].gate, i != 2);
        }
    }

    #[test]
    fn set_pit() {
        let (mut chip, _) = get_chip();
        let mut io_bus = Bus::new();
        chip.create_pit(&mut io_bus).expect("failed to create pit");

        let mut state = chip.get_pit().expect("failed to get pit state");

        // set some values
        state.channels[0].count = 500;
        state.channels[0].mode = 1;

        // Setting the pit should initialize the one-shot timer
        chip.set_pit(&state).expect("failed to set pit state");

        let state = chip.get_pit().expect("failed to get pit state");

        // check the values we set
        assert_eq!(state.channels[0].count, 500);
        assert_eq!(state.channels[0].mode, 1);
    }
}
