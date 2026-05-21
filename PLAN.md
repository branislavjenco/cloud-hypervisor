# Deterministic Hypervisor for Software Correctness Testing

A tool that runs arbitrary Docker images inside a deterministic virtual machine,
where every source of nondeterminism is controlled. Same inputs + same seed =
same execution, bit for bit. Different seeds explore different thread
interleavings, systematically finding concurrency bugs that real-world schedulers
rarely expose.

## Why

Concurrency bugs are hard to find because real-world execution explores a tiny,
biased slice of possible thread interleavings. The OS scheduler has habits — it
tends to schedule things the same way. A race condition might require an
interleaving that production almost never produces.

A deterministic hypervisor flips this: each seed produces a different
interleaving, and 10,000 seeds cover vastly more of the scheduling space than
10,000 real runs. Any single run is "unrealistic," but the ensemble is far more
thorough than reality. And the bugs found are real — if the program crashes under
interleaving X, that's a real data race, regardless of how unlikely that
interleaving is in production.

## Architecture

```
┌─────────────────────────────────────┐
│  Container (arbitrary Docker image) │
│  Arbitrary binaries, any language   │
├─────────────────────────────────────┤
│  Minimal guest Linux kernel         │
├─────────────────────────────────────┤
│  Deterministic VMM (our thing)      │
│  - Deterministic vCPU scheduler     │
│  - Synthetic TSC / timers           │
│  - Seeded PRNG for RDRAND           │
│  - Virtual block device (rootfs)    │
│  - Virtual serial console (logs)    │
│  - No networking                    │
├─────────────────────────────────────┤
│  KVM (/dev/kvm, hardware accel)     │
└─────────────────────────────────────┘
```

We use a VM (not containers/ptrace) because arbitrary binaries can bypass the
syscall boundary via RDTSC, RDRAND, io_uring, and vDSO. The VM boundary is
complete — nothing escapes it. KVM gives us hardware acceleration so the guest
runs at near-native speed.

The program inside the VM has zero side channels to real time. Every clock,
every random number, every interrupt is under our control. We can pause, skip
forward, slow down, or speed up execution — the guest can never tell.

## Why not ptrace/containers?

Instructions like RDTSC (read CPU timestamp counter) and RDRAND (hardware RNG)
execute directly on the CPU with no syscall. ptrace can't see them. io_uring
performs I/O through shared memory rings with minimal syscalls. vDSO lets
clock_gettime skip the kernel entirely. For images we control, these are
workarounds. For arbitrary images, they're showstoppers. The hypervisor
boundary is the only complete interception point.

## Implementation plan

### Step 0: Familiarize with Cloud Hypervisor ✅

Clone Cloud Hypervisor, build it, boot a stock VM, get a shell, shut it down.
Understand the code structure.

**Done:**
- Built Cloud Hypervisor from source (release mode)
- Built a custom guest kernel (Linux 6.12.8) from Cloud Hypervisor's recommended config
- Booted an Ubuntu 20.04 cloud image with 1 vCPU, 512MB RAM
- Got a login shell over serial console, confirmed everything works
- KVM works on this machine (nested virtualization, AMD SVM)

**Key source files identified:**
- `hypervisor/src/kvm/mod.rs` — KVM interface, VM exit handling, `run()` loop
- `vmm/src/cpu.rs` — vCPU thread creation, upper-level exit dispatch
- `vmm/src/vm.rs` — VM setup, memory, devices

### Step 1: Instrument VM exits ✅

Log every VM exit to understand what the guest does. See every interaction
between guest and host before trying to control any of them.

**Done:**
- Added `vcpu_id` field to `KvmVcpu` struct
- Added logging to every VM exit type in `hypervisor/src/kvm/mod.rs`
- Added human-readable annotations for all I/O port and MMIO addresses:
  - `0x3f8` → COM1 serial port (with character decoding)
  - `0xcf8`/`0xcfc` → PCI config space
  - `0xfec00000` → IOAPIC
  - `0x80` → debug port (POST code)
  - `0x608` → ACPI PM timer
  - `0xe7f80000` → virtio-blk
  - And more (PIC, PIT, RTC, virtio-rng, MSI-X, ECAM, etc.)
- Observed a typical boot: ~67,000 VM exits over 20 seconds
  - 40,430 IoOut (serial output, PCI config writes)
  - 26,035 IoIn (PCI config reads, serial status checks)
  - 297 MmioWrite (IOAPIC config, virtio device setup)
  - 144 MmioRead (IOAPIC, virtio device setup)
- Confirmed: no RDTSC/RDRAND/MSR exits yet (traps not enabled)

### Step 2: Control all nondeterminism sources (single vCPU)

Keep using 1 vCPU — no scheduling problem yet. Make every source of
nondeterminism deterministic, one at a time. Test each by running twice with
the same config and diffing the output.

**TODO:**
- [ ] Disable networking (don't add virtio-net — already the case)
- [ ] Disable ASLR (boot with `nokaslr`, `randomize_va_space=0`)
- [ ] Trap RDTSC: set VM exit flag in VMCS, return synthetic monotonic value
- [ ] Trap RDRAND/RDSEED: trap instruction, return seeded PRNG value
- [ ] Control clock: fixed TSC frequency, synthetic APIC timer
- [ ] Seed guest kernel RNG (`rng_seed=` kernel parameter)
- [ ] Verify: run same image twice, diff all output — must be identical

### Step 3: Deterministic vCPU scheduler

Add multiple vCPUs. Build a deterministic scheduler. This is the core of
the project — everything else is plumbing.

**Approach (incremental):**

1. **Syscall-boundary scheduling (simplest).** Only switch vCPUs at VM exits.
   Round-robin in a deterministic order derived from a seed. Zero extra
   overhead, but coarse — can't control interleaving between exits.

2. **Hybrid timer scheduling.** Add a virtual APIC timer that fires at fixed
   virtual time intervals. Forces VM exits in long-running compute loops.
   Deterministic if timer is tied to virtual clock. Better coverage.

3. **PMU branch counting (highest fidelity).** Use hardware performance
   counters to run each vCPU for exactly N retired conditional branches
   before switching. Instruction-level precision. Hardware-dependent
   (varies by CPU model). This is what `rr` uses.

**The scheduler algorithm:**
```
loop {
    if all vCPUs halted:
        advance virtual clock to next timer
        wake vCPUs with pending interrupts
    pick next vCPU and slice size from seeded PRNG
    run vCPU for that slice
    handle any VM exits within the slice
    switch to next vCPU
}
```

**TODO:**
- [ ] Add second vCPU, implement round-robin at VM exit boundaries
- [ ] Make scheduling order deterministic from a seed
- [ ] Handle HLT (guest idle) — skip to next runnable vCPU
- [ ] Add virtual timer for preemption of long compute slices
- [ ] (Later) PMU-based branch counting for instruction-level precision

### Step 4: Docker image → VM rootfs pipeline

Unpack Docker images into a rootfs that the VM can boot.

**TODO:**
- [ ] Pull Docker image layers (via skopeo, crane, or custom code)
- [ ] Untar layers into an ext4 disk image
- [ ] Write a minimal init that sets up env vars and execs the entrypoint
- [ ] Wire up: `tool run <image> --seed=N` boots the VM with that rootfs

### Step 5: Testing determinism

The oracle is simple: run twice with the same seed, diff everything. If
anything differs, there's a nondeterminism leak.

**Test programs (by sensitivity):**

| Level | Test | What it catches |
|-------|------|-----------------|
| 1 | Read RDTSC/RDRAND/clock 1000x, print values | Leaked time/randomness |
| 2 | Hash-and-branch on timestamp | Amplified divergence |
| 3 | Shared counter, 4 threads, no locks | Scheduling nondeterminism |
| 4 | Lock-free producer/consumer ring buffer | Fine-grained interleaving |
| 5 | SQLite concurrent transactions | Real-world complexity |
| 6 | `make -j4` of a C project | Parallel build ordering |

## Prior art

- **Antithesis** — commercial deterministic hypervisor, same concept, founded
  by the FoundationDB team. Not open source.
- **rr (Mozilla)** — deterministic record/replay debugger using ptrace + PMU.
  Battle-tested PMU scheduling. Not a hypervisor.
- **hermit (Meta)** — deterministic container runtime via ptrace + seccomp-bpf.
  Open source. Limited by ptrace boundary (RDTSC etc. escape).
- **QEMU icount mode** — deterministic via instruction counting + binary
  translation. Slow (no KVM). Proves the concept.

## Technical notes

### Nondeterminism sources and how we control them

| Source | Mechanism | Difficulty |
|--------|-----------|------------|
| RDTSC | Trap via VMCS, return synthetic value | Easy |
| RDRAND/RDSEED | Trap instruction, return seeded PRNG | Easy |
| clock_gettime/vDSO | Guest gets time from virtual TSC we control | Easy |
| /dev/urandom | Guest kernel seeds from RDRAND + TSC (controlled) | Free |
| ASLR | Kernel boot param `randomize_va_space=0` | Easy |
| Thread scheduling | Deterministic vCPU scheduler | Hard |
| Interrupt timing | Inject at deterministic virtual time | Moderate |
| Disk I/O ordering | Single queue, deterministic with single vCPU | Easy |
| Network | Disabled entirely | Free |

### Cloud / deployment

- **Nested KVM** works on GCP (flag), Azure (most instances), AWS (metal or
  some Nitro instances). Fine for development.
- **Bare metal** preferred for production: full PMU access, no nesting overhead.
  Hetzner dedicated servers are cheap (~€40/month).
- PMU counters may not work reliably under nested virtualization — important
  for step 3 PMU-based scheduling.

### Files modified

- `hypervisor/src/kvm/mod.rs` — added `vcpu_id` to KvmVcpu, added annotated
  VM exit logging with I/O port and MMIO address descriptions
