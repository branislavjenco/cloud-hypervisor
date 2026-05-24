// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause

use std::cmp::min;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

// libc time functions are no longer needed; RTC is fixed to a constant epoch.
use log::{info, warn};
use vm_device::BusDevice;
use vmm_sys_util::eventfd::EventFd;

const INDEX_MASK: u8 = 0x7f;
const INDEX_OFFSET: u64 = 0x0;
const DATA_OFFSET: u64 = 0x1;
const DATA_LEN: usize = 128;

/// A CMOS/RTC device commonly seen on x86 I/O port 0x70/0x71.
pub struct Cmos {
    index: u8,
    data: [u8; DATA_LEN],
    reset_evt: EventFd,
    vcpus_kill_signalled: Option<Arc<AtomicBool>>,
}

impl Cmos {
    /// Constructs a CMOS/RTC device with initial data.
    /// `mem_below_4g` is the size of memory in bytes below the 32-bit gap.
    /// `mem_above_4g` is the size of memory in bytes above the 32-bit gap.
    pub fn new(
        mem_below_4g: u64,
        mem_above_4g: u64,
        reset_evt: EventFd,
        vcpus_kill_signalled: Option<Arc<AtomicBool>>,
    ) -> Cmos {
        let mut data = [0u8; DATA_LEN];

        // Extended memory from 16 MB to 4 GB in units of 64 KB
        let ext_mem = min(
            0xFFFF,
            mem_below_4g.saturating_sub(16 * 1024 * 1024) / (64 * 1024),
        );
        data[0x34] = ext_mem as u8;
        data[0x35] = (ext_mem >> 8) as u8;

        // High memory (> 4GB) in units of 64 KB
        let high_mem = min(0x00FF_FFFF, mem_above_4g / (64 * 1024));
        data[0x5b] = high_mem as u8;
        data[0x5c] = (high_mem >> 8) as u8;
        data[0x5d] = (high_mem >> 16) as u8;

        Cmos {
            index: 0,
            data,
            reset_evt,
            vcpus_kill_signalled,
        }
    }
}

impl BusDevice for Cmos {
    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        if data.len() != 1 {
            warn!("Invalid write size on CMOS device: {}", data.len());
            return None;
        }

        match offset {
            INDEX_OFFSET => self.index = data[0],
            DATA_OFFSET => {
                if self.index == 0x8f && data[0] == 0 {
                    info!("CMOS reset");
                    self.reset_evt.write(1).unwrap();
                    if let Some(vcpus_kill_signalled) = self.vcpus_kill_signalled.take() {
                        // Spin until we are sure the reset_evt has been handled and that when
                        // we return from the KVM_RUN we will exit rather than re-enter the guest.
                        while !vcpus_kill_signalled.load(Ordering::SeqCst) {
                            // This is more effective than thread::yield_now() at
                            // avoiding a priority inversion with the VMM thread
                            thread::sleep(std::time::Duration::from_millis(1));
                        }
                    }
                } else {
                    self.data[(self.index & INDEX_MASK) as usize] = data[0];
                }
            }
            o => warn!("bad write offset on CMOS device: {o}"),
        }
        None
    }

    fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        fn to_bcd(v: u8) -> u8 {
            assert!(v < 100);
            ((v / 10) << 4) | (v % 10)
        }

        if data.len() != 1 {
            warn!("Invalid read size on CMOS device: {}", data.len());
            return;
        }

        data[0] = match offset {
            INDEX_OFFSET => self.index,
            DATA_OFFSET => {
                // Determinism: RTC is fixed to 2024-01-01 00:00:00 UTC (Monday).
                // year=124 (years since 1900), month=1, day=1, wday=2 (Monday),
                // hours=0, minutes=0, seconds=0.
                const SECONDS: i32 = 0;
                const MINUTES: i32 = 0;
                const HOURS: i32 = 0;
                const WEEK_DAY: i32 = 2; // Monday (1=Sunday, 2=Monday, ...)
                const DAY: i32 = 1;
                const MONTH: i32 = 1;
                const YEAR: i32 = 124; // years since 1900 → 2024
                const CENTURY: i32 = 20;
                match self.index {
                    0x00 => to_bcd(SECONDS as u8),
                    0x02 => to_bcd(MINUTES as u8),
                    0x04 => to_bcd(HOURS as u8),
                    0x06 => to_bcd(WEEK_DAY as u8),
                    0x07 => to_bcd(DAY as u8),
                    0x08 => to_bcd(MONTH as u8),
                    0x09 => to_bcd((YEAR % 100) as u8),
                    // Bit 5 for 32kHz clock. Bit 7: UIP always 0 (never updating).
                    0x0a => 1 << 5,
                    // Bit 0-6 reserved, must be 0. Bit 7 must be 1 (CMOS has power).
                    0x0d => 1 << 7,
                    0x32 => to_bcd(CENTURY as u8),
                    _ => {
                        // self.index is always guaranteed to be in range via INDEX_MASK.
                        self.data[(self.index & INDEX_MASK) as usize]
                    }
                }
            }
            o => {
                warn!("bad read offset on CMOS device: {o}");
                0
            }
        }
    }
}
