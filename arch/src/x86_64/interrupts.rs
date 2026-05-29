// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE-BSD-3-Clause file.

use std::result;

pub type Result<T> = result::Result<T, hypervisor::HypervisorCpuError>;

// Defines poached from apicdef.h kernel header.
pub const APIC_LVT0: usize = 0x350;
pub const APIC_LVT1: usize = 0x360;
pub const APIC_LVT_TIMER: usize = 0x320;
pub const APIC_TMICT: usize = 0x380; // Initial Count
pub const APIC_TMCCT: usize = 0x390; // Current Count
pub const APIC_TDCR: usize  = 0x3e0; // Divide Config
pub const APIC_IRR_BASE: usize = 0x200; // IRR[0..7], stride 0x10
pub const APIC_MODE_NMI: u32 = 0x4;
pub const APIC_MODE_EXTINT: u32 = 0x7;
// Linux LOCAL_TIMER_VECTOR (arch/x86/include/asm/irq_vectors.h)
pub const LOCAL_TIMER_VECTOR: u8 = 0xec;

pub fn set_apic_delivery_mode(reg: u32, mode: u32) -> u32 {
    ((reg) & !0x700) | ((mode) << 8)
}

/// Dump LAPIC timer-related registers for diagnostic purposes.
pub fn log_lapic_timer_state(vcpu: &dyn hypervisor::Vcpu) {
    let klapic = match vcpu.get_lapic() {
        Ok(k) => k,
        Err(e) => { log::warn!("[lapic-probe] get_lapic failed: {e:?}"); return; }
    };
    let lvt_timer = klapic.get_klapic_reg(APIC_LVT_TIMER);
    let tmict    = klapic.get_klapic_reg(APIC_TMICT);
    let tmcct    = klapic.get_klapic_reg(APIC_TMCCT);
    let tdcr     = klapic.get_klapic_reg(APIC_TDCR);
    // Also dump the spurious vector register (SVR) at 0x0f0 — bit 8 = APIC enabled
    let svr      = klapic.get_klapic_reg(0x0f0);
    // ISR / IRR words covering vectors 0xe0-0xff (indices 7, covering bit 28..=31 for 0xec)
    // IRR register index = vector / 32; byte offset = APIC_IRR_BASE + index*0x10
    let timer_irr_word_off = APIC_IRR_BASE + ((LOCAL_TIMER_VECTOR as usize / 32) * 0x10);
    let timer_irr_word     = klapic.get_klapic_reg(timer_irr_word_off);
    let timer_irr_bit      = (LOCAL_TIMER_VECTOR % 32) as u32;
    // Print all 8 IRR words to see any pending interrupt
    let irr: Vec<u32> = (0..8).map(|i| klapic.get_klapic_reg(APIC_IRR_BASE + i * 0x10)).collect();
    log::info!(
        "[lapic-probe] LVT_TIMER=0x{lvt_timer:08x} vector=0x{:02x} masked={} mode={} \
         TMICT={tmict} TMCCT={tmcct} TDCR=0x{tdcr:x} SVR=0x{svr:08x}(en={}) \
         IRR[0xec]_word@0x{timer_irr_word_off:x}=0x{timer_irr_word:08x} bit{timer_irr_bit}={} \
         IRR={:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x}",
        lvt_timer & 0xff,
        (lvt_timer >> 16) & 1,
        (lvt_timer >> 17) & 0x3,
        (svr >> 8) & 1,
        (timer_irr_word >> timer_irr_bit) & 1,
        irr[0], irr[1], irr[2], irr[3], irr[4], irr[5], irr[6], irr[7],
    );
}

/// Configures LAPICs.  LAPIC0 is set for external interrupts, LAPIC1 is set for NMI.
///
/// # Arguments
/// * `vcpu` - The VCPU object to configure.
pub fn set_lint(vcpu: &dyn hypervisor::Vcpu) -> Result<()> {
    let mut klapic = vcpu.get_lapic()?;

    let lvt_lint0 = klapic.get_klapic_reg(APIC_LVT0);
    klapic.set_klapic_reg(
        APIC_LVT0,
        set_apic_delivery_mode(lvt_lint0, APIC_MODE_EXTINT),
    );
    let lvt_lint1 = klapic.get_klapic_reg(APIC_LVT1);
    klapic.set_klapic_reg(APIC_LVT1, set_apic_delivery_mode(lvt_lint1, APIC_MODE_NMI));

    vcpu.set_lapic(&klapic)
}
