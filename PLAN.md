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

## Milestones

1. **Single-vCPU determinism.** Docker image → initramfs → boot → run → identical
   output on two runs with the same seed. All nondeterminism sources controlled
   except thread scheduling (moot with 1 vCPU). (Steps 0–3)

2. **Multi-vCPU deterministic scheduling.** Add multiple vCPUs with a seeded
   deterministic scheduler. `tool run redis --seed=42` produces identical output
   every time, even with concurrent threads. Different seeds explore different
   interleavings. (Steps 4–5)

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

### Step 2: Docker image → VM rootfs pipeline ✅

Get arbitrary Docker images running in the VM. This gives us a large library
of real-world binaries to test against for every subsequent step.

**Done:**
- Built a static init binary (`tool/init.c`, compiled with musl) that:
  - Opens /dev/console for stdio
  - Mounts proc, sysfs, tmpfs, creates /dev/fd symlinks and /dev/shm
  - Reads entrypoint/env/workdir from `/etc/det/config`
  - Runs entrypoint as child, waits, prints `EXIT:<code>`, powers off
- Built `tool/run.sh` shell script that:
  1. `docker create` + `docker export` → tarball
  2. Extracts into initramfs dir, injects init binary + config
  3. Converts to cpio archive
  4. Calculates memory (3x initramfs + 512MB headroom)
  5. Boots modified cloud-hypervisor with kernel + initramfs
- Tested with:
  - `hello-world` — EXIT:0, prints message ✅
  - `alpine-echo` (alpine + `echo hello from alpine`) — EXIT:0 ✅
  - `redis` — starts, "Ready to accept connections" ✅
  - `postgres` (no password) — EXIT:1, correct error ✅
  - `postgres` (with POSTGRES_PASSWORD) — initializes DB, accepts connections ✅

### Step 3: Control all nondeterminism sources (single vCPU)

Keep using 1 vCPU — no scheduling problem yet. Make every source of
nondeterminism deterministic, one at a time. Test each by running twice with
the same config and diffing the output. Having Docker support means we can
test each trap against many different real-world binaries.

**Decisions:**
- **RDTSC:** KVM doesn't expose RDTSC trapping to userspace — the guest reads
  the virtual TSC directly and KVM handles it in-kernel. We can't intercept it.
  Instead, we steer the guest away from TSC entirely: force the kernel to use
  the ACPI PM timer as its clocksource (`clocksource=acpi_pm tsc=unstable`),
  and clear the invariant TSC CPUID bit so the kernel doesn't trust the TSC.
  Userspace RDTSC remains a known gap (most Docker images don't call it directly).
- **RDRAND/RDSEED:** Clear the CPUID bits so the guest thinks the CPU doesn't
  support these instructions. Guest kernel falls back to software RNG — this
  is fine and actually helps determinism. No instruction emulation needed.
- **ACPI PM timer:** Currently returns real wall-clock time via `Instant::now()`.
  Replace with a deterministic counter that increments by a fixed amount per
  read. Since this becomes the guest's sole time source, all time-dependent
  behavior flows through it.
- **RTC (Real-Time Clock):** Ports `0x70`/`0x71`. Returns host date/time.
  Programs calling `date`, `time()`, or logging with timestamps will differ
  between runs. Fix by setting the RTC to a fixed epoch (e.g. 2024-01-01
  00:00:00 UTC) every boot.
- **kvmclock:** A paravirtualized clocksource Linux prefers over both TSC and
  PM timer when running under KVM. Reads a shared memory page, no VM exit.
  The `clocksource=acpi_pm` cmdline overrides it, but we should also clear
  the KVM paravirt clock CPUID bits so it's not even an option.
- **Interrupt timing:** Timer interrupts (LAPIC timer, PIT) fire based on real
  time. KVM manages the LAPIC in-kernel. When exactly an interrupt arrives
  relative to guest execution varies per run — the point in the guest's
  instruction stream where the interrupt is delivered depends on when the
  previous VM exit occurred, which depends on real time. This is the hardest
  remaining source of nondeterminism for single-vCPU. May need to disable
  LAPIC timer and rely solely on our deterministic PM timer.
- **Kernel entropy:** Even without RDRAND, the kernel gathers entropy from
  interrupt timing jitter. If interrupt timing is nondeterministic,
  `/dev/urandom` output could still vary. Controlling interrupt timing
  fixes this transitively.
- **Kernel RNG:** Don't explicitly seed. Once time sources and interrupts are
  controlled, the kernel's entropy sources are deterministic.
- **Seed passing:** Whatever is simplest. Probably an env var or kernel cmdline
  param that cloud-hypervisor reads at startup.
- **Success criteria:** Run same image twice, diff the full sequence of VM exits
  (not just serial output). Any nondeterminism in interrupt timing, I/O port
  access patterns, etc. must show up.

**Test scheme:**

A small custom Docker image that actively exercises every nondeterminism source:

```dockerfile
FROM alpine
RUN apk add --no-cache util-linux
COPY test.sh /test.sh
CMD ["/test.sh"]
```

```sh
#!/bin/sh
echo "=== RTC/time ==="
date

echo "=== /dev/urandom ==="
dd if=/dev/urandom bs=32 count=1 2>/dev/null | hexdump -C

echo "=== ASLR (stack address) ==="
cat /proc/self/maps | grep stack

echo "=== KASLR (kernel text) ==="
grep ' T _text' /proc/kallsyms 2>/dev/null || echo "(not accessible)"

echo "=== clock_gettime ==="
cat /proc/uptime
```

Each section maps directly to a subtask below. The test script is
`tool/det-test`. Run it after each subtask; results land in
`tool/det-test-results/`.

**Baseline (before any fixes):** recorded before implementation started.
```
✗ Serial output DIFFERS   (RTC date, uptime, /dev/urandom, ASLR stack addr)
✓ VM exit sequence is identical
```

**Current state (after subtasks 1–7):**
```
✓ RTC/time identical     (Mon Jan  1 00:00:00 UTC 2024)
✓ uptime identical       (0.00 0.00)
✗ /dev/urandom differs   (entropy seeded from interrupt jitter via virtio-rng)
✓ ASLR stack addr identical
✓ KASLR text addr identical
✓ clocksource identical  (acpi_pm)
✓ VM exit sequence identical (102,584 exits each, 164 lines differ — all
  virtio-rng data + serial chars from /dev/urandom hexdump)
```

Every subtask below should make the serial diff shorter and must never make
the VM exit diff worse.

---

**Subtask 1 — Networking already disabled ✅**
- No virtio-net device added. Confirmed already the case.

---

**Subtask 2 — Disable ASLR ✅**
- Added `nokaslr` to kernel cmdline in `tool/run.sh` and `tool/det-test`.
- `tool/init.c` writes `0` to `/proc/sys/kernel/randomize_va_space` before exec.
- Result: `[stack]` address and `_text` KASLR address are now identical between runs.

---

**Subtask 3 — Disable kvmclock CPUID bits ✅**
- Cleared `KVM_FEATURE_CLOCKSOURCE`, `KVM_FEATURE_CLOCKSOURCE2`, and
  `KVM_FEATURE_CLOCKSOURCE_STABLE` bits from leaf `0x4000_0001` EAX in
  `arch/src/x86_64/mod.rs` (unconditionally, not just for TDX).
- Result: `=== clocksource ===` never shows `kvm-clock`; only `acpi_pm` appears.

---

**Subtask 4 — Clear invariant TSC CPUID bit + force ACPI PM timer ✅**
- Cleared invariant TSC bit (leaf `0x8000_0007`, EDX bit 8) in
  `arch/src/x86_64/mod.rs`.
- Added `clocksource=acpi_pm tsc=unstable` to kernel cmdline.
- Result: `=== clocksource ===` shows `acpi_pm` only; `=== available clocksources ===`
  shows only `acpi_pm` (TSC and kvm-clock gone).

---

**Subtask 5 — Clear RDRAND/RDSEED CPUID bits ✅**
- Cleared RDRAND (leaf `0x1`, ECX bit 30) and RDSEED (leaf `0x7`, EBX bit 18)
  in `arch/src/x86_64/mod.rs`.
- Guest kernel falls back to software RNG. Direct hardware randomness is gone.

---

**Subtask 6 — Fix RTC to a constant epoch ✅**
- Replaced `clock_gettime`/`gmtime_r` in `devices/src/legacy/cmos.rs` with
  hardcoded constants: 2024-01-01 00:00:00 UTC (Monday).
- Result: `=== RTC/time ===` base epoch is fixed. The displayed time is
  `epoch + uptime`, so it still varies until uptime is deterministic.

---

**Subtask 7 — Make ACPI PM timer deterministic ✅ (partial)**
- Replaced `Instant::now()` in `devices/src/acpi.rs` with a counter that
  increments by 1 tick per read (~0.28µs of virtual time per read).
- PM timer no longer reads real wall-clock time.
- Also suppressed PM timer reads from the vmexit log (they are deterministic
  by construction and were generating ~100k log lines per boot, stalling the VM
  when `-vvv` logging was active).
- **Remaining issue:** uptime still varies between runs. See Subtask 8.

---

**Subtask 8 — Fix uptime/interrupt timing nondeterminism ⬅ TODO**

This is the last remaining source of nondeterminism. Current diff:
```
✓ RTC/time identical   (Mon Jan  1 00:00:00 UTC 2024)
✓ uptime identical     (0.00 0.00)
✗ /dev/urandom differs  (entropy seeded from interrupt jitter)
✓ ASLR stack addr identical
✓ KASLR text addr identical
✓ clocksource identical (acpi_pm)
✓ VM exit sequence — needs verification with fixed det-test
```

**Root cause:** interrupt timing jitter. The LAPIC timer fires based on real
wall-clock time (KVM manages it in-kernel). Each interrupt arrives at a
slightly different point in the guest's instruction stream depending on host
scheduling. This jitter feeds the kernel entropy pool, making `/dev/urandom`
output nondeterministic.

With TICKS_PER_READ=1, uptime and RTC are now deterministic (both show
0.00 / epoch). The only remaining diff is `/dev/urandom`.

**Approach:** the LAPIC timer fires interrupts based on real wall-clock time
(KVM manages it in-kernel). Each interrupt fires at a slightly different point
in the guest's instruction stream depending on host scheduling. Options:

1. **Disable the LAPIC timer entirely.** Pass `nolapic` or `lapic=notimer` to
   the kernel so it falls back to the PIT or PM timer for timekeeping. Fewer
   interrupt sources = less jitter.
2. **Disable the PIT.** Eliminate another real-time interrupt source.
3. **Use `nohz=off` + `highres=off`.** Forces the kernel into periodic-tick
   mode with a fixed tick rate, reducing the impact of variable interrupt timing.
4. **Accept non-deterministic uptime, strip it from the test.** If only the
   displayed time/uptime varies (and nothing else), we could declare
   determinism achieved for all *user-visible outputs* and treat uptime as a
   known non-deterministic kernel internal. This is a weaker but pragmatic goal.

Try options in order. Run `./det-test` after each attempt.
Goal: both diffs empty.

---

**Subtask 9 — Final verification**
- Run `./det-test` one last time.
- Both diffs must be empty:
  ```
  ✓ Serial output is identical
  ✓ VM exit sequence is identical
  ```
- Then test with a second Docker image (e.g. `alpine` with `echo hello`) to
  confirm nothing is image-specific.

---

**After each subtask, the rule is:**
1. Run `./det-test`.
2. At least one diff line that previously appeared must now be gone, OR the
   subtask was a prerequisite with no direct diff effect (subtasks 1, 3, 5).
3. No diff line that was previously identical must now differ.
4. The VM exit diff must remain empty throughout.

### Step 4: Deterministic vCPU scheduler

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
      **Open question:** Single host thread calling KVM_RUN on each vCPU sequentially
      (option b) seems simpler for determinism, but need to verify KVM allows calling
      KVM_RUN for different vCPUs from one thread. Alternative is multi-thread with
      barriers (option a).
- [ ] Make scheduling order deterministic from a seed
- [ ] Handle HLT (guest idle) — skip to next runnable vCPU. When all vCPUs are
      halted, advance virtual clock to next timer tick and inject interrupt.
- [ ] Add virtual timer for preemption of long compute slices
- [ ] (Later) PMU-based branch counting for instruction-level precision

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
| RDTSC | Can't trap via KVM. Steer guest to PM timer instead. Known gap for userspace. | Moderate |
| RDRAND/RDSEED | Clear CPUID bits, guest thinks CPU doesn't support them | Easy |
| clock_gettime/vDSO | Guest uses PM timer clocksource (we control it) | Easy |
| ACPI PM timer | Replace `Instant::now()` with deterministic counter | Easy |
| RTC | Fix to constant epoch (2024-01-01 00:00:00 UTC) | Easy |
| kvmclock | Clear KVM paravirt clock CPUID bits | Easy |
| /dev/urandom | Deterministic once interrupt timing + RDRAND controlled | Free |
| ASLR (userspace) | init writes `randomize_va_space=0` to procfs | Easy |
| KASLR (kernel) | `nokaslr` boot param | Easy |
| Thread scheduling | Deterministic vCPU scheduler (Step 4) | Hard |
| Interrupt timing | LAPIC timer fires based on real time. Hardest single-vCPU source. | Hard |
| Disk I/O ordering | Single queue, deterministic with single vCPU | Easy |
| Network | Disabled entirely (no virtio-net) | Free |

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
