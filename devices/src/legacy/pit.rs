// Copyright 2024 Oxide Computer Company
// SPDX-License-Identifier: Apache-2.0
//
// Minimal emulation of the 8253/8254 PIT (Programmable Interval Timer).
//
// Only counter 2 (port 0x42) and the mode/command register (port 0x43) are
// emulated. Counter 0 (port 0x40) and counter 1 (port 0x41) are not used by
// the guest paths we care about.
//
// ## Purpose
//
// Linux's `quick_pit_calibrate()` programs counter 2 (via port 0x43/0x42) to
// count down from 0xffff and then reads pairs of bytes from port 0x42 to
// observe the MSB decrement. When port 0x42 is unregistered (returns 0xff
// always), `pit_expect_msb(0xff)` loops 50,000 iterations before giving up,
// consuming ~100,000 VM exits before TSC calibration can complete.
//
// This emulator makes the counter decrement on every read so that each MSB
// value is visible for ~256 reads (enough to satisfy the >5-iteration
// threshold) and then rolls to the next value. `quick_pit_calibrate` succeeds
// in a handful of iterations instead of 50,000.
//
// ## Ports
//
// Port 0x40 — counter 0 (IRQ 0 timer): reads return 0x00 (not used).
// Port 0x41 — counter 1 (DRAM refresh): reads return 0x00 (not used).
// Port 0x42 — counter 2 (PIT / speaker): read returns decrementing counter;
//             write loads the initial countdown latch (LSB then MSB).
// Port 0x43 — mode/command: write programs the mode; read returns 0x00.
//
// The bus is registered at base 0x40, size 4.

use std::sync::{Arc, Barrier};

use vm_device::BusDevice;

/// Minimal PIT emulator for counter 2 TSC calibration.
///
/// ## Design goal
///
/// Make Linux's `quick_pit_calibrate()` and `pit_calibrate_tsc()` fail
/// **deterministically** (same number of VM exits every run) with the minimum
/// number of VM exits, so that calibration falls through to the PM-timer path
/// (`pit_hpet_ptimer_calibrate_cpu`) which our deterministic PM-timer already
/// handles.
///
/// ## How
///
/// We make every byte read from port 0x42 return a different value by using a
/// free-running 8-bit counter that increments on each byte read.  This causes
/// `pit_verify_msb(val)` to return false on the very first call (MSB never
/// matches the expected value), so:
///   - `pit_expect_msb(0xff)` returns 0 iterations (< 5) → false.
///   - `quick_pit_calibrate` bails out immediately with "Fast TSC calibration
///     failed" after only ~8 port-0x42 reads.
///   - `pit_calibrate_tsc` loops on port 0x61 bit 5 (OUT2), which i8042
///     already emulates as always-set, so it exits after one iteration.
///
/// Result: both functions return 0 (failure) deterministically in <10 VM exits,
/// then `pit_hpet_ptimer_calibrate_cpu` uses the acpi_pm timer for TSC
/// calibration.
pub struct Pit {
    /// Free-running 8-bit sequence counter; increments on every byte read
    /// from port 0x42 to ensure no two consecutive reads return the same byte.
    read_counter: u8,
    /// Write latch: `Some(lsb)` after the first write byte, waiting for MSB.
    write_latch: Option<u8>,
}

impl Pit {
    pub fn new() -> Pit {
        Pit {
            read_counter: 0xff,
            write_latch: None,
        }
    }
}

impl Default for Pit {
    fn default() -> Self {
        Pit::new()
    }
}

impl BusDevice for Pit {
    fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        if data.len() != 1 {
            return;
        }
        match offset {
            0 | 1 => {
                // Counter 0 / counter 1 — not used, return 0.
                data[0] = 0x00;
            }
            2 => {
                // Counter 2: return the next byte from the free-running
                // counter.  Every read returns a different value, so
                // pit_verify_msb(val) always returns false, causing
                // quick_pit_calibrate to fail fast and deterministically.
                let val = self.read_counter;
                self.read_counter = self.read_counter.wrapping_add(1);
                data[0] = val;
            }
            3 => {
                // Mode register read: not meaningful, return 0.
                data[0] = 0x00;
            }
            _ => {}
        }
    }

    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        if data.len() != 1 {
            return None;
        }
        let byte = data[0];
        match offset {
            0 | 1 => {
                // Counter 0 / counter 1 writes: ignore.
            }
            2 => {
                // Counter 2 data writes: accept but do nothing.
                // We don't actually simulate a countdown; our read_counter
                // provides a deterministic non-repeating sequence instead.
                match self.write_latch {
                    None => {
                        self.write_latch = Some(byte);
                    }
                    Some(_) => {
                        self.write_latch = None;
                    }
                }
            }
            3 => {
                // Mode/command register: accept, reset latch state.
                self.write_latch = None;
            }
            _ => {}
        }
        None
    }
}
