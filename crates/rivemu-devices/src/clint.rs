//! SiFive-compatible CLINT (ACLINT MSWI + MTIMER).

use std::sync::{Arc, Mutex};

use rivemu_core::{AccessSize, Timebase};
use rivemu_cpu_riscv::HartIrqs;
use rivemu_cpu_riscv::csr::irq;
use rivemu_mem::MmioDevice;

use crate::{reg_read, reg_write};

const MSIP_BASE: u64 = 0x0;
const MTIMECMP_BASE: u64 = 0x4000;
const MTIME: u64 = 0xbff8;

#[derive(Debug)]
struct State {
    msip: Vec<bool>,
    mtimecmp: Vec<u64>,
    /// Added to the timebase to form `mtime` (allows guest writes).
    offset: u64,
}

/// Core-local interruptor: software interrupts and the machine timer.
pub struct Clint {
    timebase: Arc<dyn Timebase>,
    harts: Vec<Arc<HartIrqs>>,
    state: Mutex<State>,
}

impl std::fmt::Debug for Clint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Clint")
            .field("harts", &self.harts.len())
            .finish()
    }
}

impl Clint {
    /// Creates a CLINT for the given harts.
    pub fn new(timebase: Arc<dyn Timebase>, harts: Vec<Arc<HartIrqs>>) -> Self {
        let n = harts.len();
        Self {
            timebase,
            harts,
            state: Mutex::new(State {
                msip: vec![false; n],
                mtimecmp: vec![u64::MAX; n],
                offset: 0,
            }),
        }
    }

    /// Current `mtime`.
    pub fn mtime(&self) -> u64 {
        let off = self.state.lock().unwrap().offset;
        self.timebase.now().wrapping_add(off)
    }

    /// Re-evaluates timer interrupts against the current time.
    pub fn update(&self) {
        let s = self.state.lock().unwrap();
        self.update_locked(&s);
    }

    fn update_locked(&self, s: &State) {
        let now = self.timebase.now().wrapping_add(s.offset);
        for (h, lines) in self.harts.iter().enumerate() {
            lines.set(irq::MTI, now >= s.mtimecmp[h]);
            lines.set(irq::MSI, s.msip[h]);
        }
    }

    /// Earliest future timer deadline (in timebase ticks), if any.
    pub fn next_deadline(&self) -> Option<u64> {
        let s = self.state.lock().unwrap();
        let now = self.timebase.now().wrapping_add(s.offset);
        s.mtimecmp
            .iter()
            .filter(|&&t| t > now && t != u64::MAX)
            .min()
            .map(|t| t.wrapping_sub(s.offset))
    }
}

impl MmioDevice for Clint {
    fn read(&self, offset: u64, size: AccessSize) -> u64 {
        let s = self.state.lock().unwrap();
        let n = self.harts.len() as u64;
        match offset {
            o if o < MSIP_BASE + 4 * n => u64::from(s.msip[(o / 4) as usize]) & size.mask(),
            o if (MTIMECMP_BASE..MTIMECMP_BASE + 8 * n).contains(&o) => {
                let h = ((o - MTIMECMP_BASE) / 8) as usize;
                reg_read(s.mtimecmp[h], o, size)
            }
            o if (MTIME..MTIME + 8).contains(&o) => {
                reg_read(self.timebase.now().wrapping_add(s.offset), o, size)
            }
            _ => 0,
        }
    }

    fn write(&self, offset: u64, size: AccessSize, value: u64) {
        let mut s = self.state.lock().unwrap();
        let n = self.harts.len() as u64;
        match offset {
            o if o < MSIP_BASE + 4 * n => {
                if o % 4 == 0 {
                    s.msip[(o / 4) as usize] = value & 1 != 0;
                }
            }
            o if (MTIMECMP_BASE..MTIMECMP_BASE + 8 * n).contains(&o) => {
                let h = ((o - MTIMECMP_BASE) / 8) as usize;
                s.mtimecmp[h] = reg_write(s.mtimecmp[h], o, size, value);
            }
            o if (MTIME..MTIME + 8).contains(&o) => {
                let base = self.timebase.now();
                let cur = base.wrapping_add(s.offset);
                s.offset = reg_write(cur, o, size, value).wrapping_sub(base);
            }
            _ => {}
        }
        self.update_locked(&s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Default)]
    struct FakeTime(AtomicU64);
    impl Timebase for FakeTime {
        fn now(&self) -> u64 {
            self.0.load(Ordering::Relaxed)
        }
    }

    #[test]
    fn timer_and_software_interrupts() {
        let t = Arc::new(FakeTime::default());
        let h0 = HartIrqs::new();
        let h1 = HartIrqs::new();
        let c = Clint::new(t.clone(), vec![h0.clone(), h1.clone()]);
        c.update();
        assert_eq!(h0.get(), 0);

        c.write(MTIMECMP_BASE + 8, AccessSize::Double, 100);
        assert_eq!(c.next_deadline(), Some(100));
        t.0.store(100, Ordering::Relaxed);
        c.update();
        assert_eq!(h1.get(), irq::MTIP);
        assert_eq!(h0.get(), 0);

        // 32-bit halves.
        c.write(MTIMECMP_BASE + 12, AccessSize::Word, 1);
        assert_eq!(
            c.read(MTIMECMP_BASE + 8, AccessSize::Double),
            (1 << 32) | 100
        );
        assert_eq!(h1.get(), 0);

        c.write(4, AccessSize::Word, 1);
        assert_eq!(h1.get(), irq::MSIP);
        assert_eq!(c.read(4, AccessSize::Word), 1);
    }

    #[test]
    fn mtime_write() {
        let t = Arc::new(FakeTime::default());
        t.0.store(50, Ordering::Relaxed);
        let c = Clint::new(t.clone(), vec![HartIrqs::new()]);
        assert_eq!(c.read(MTIME, AccessSize::Double), 50);
        c.write(MTIME, AccessSize::Double, 1000);
        assert_eq!(c.mtime(), 1000);
        t.0.store(60, Ordering::Relaxed);
        assert_eq!(c.read(MTIME, AccessSize::Word), 1010);
    }
}
