//! SiFive-compatible platform-level interrupt controller (PLIC).

use std::sync::{Arc, Mutex};

use rivemu_core::{AccessSize, IrqLine};
use rivemu_cpu_riscv::HartIrqs;
use rivemu_cpu_riscv::csr::irq;
use rivemu_mem::MmioDevice;

const PRIORITY_BASE: u64 = 0x0;
const PENDING_BASE: u64 = 0x1000;
const ENABLE_BASE: u64 = 0x2000;
const ENABLE_STRIDE: u64 = 0x80;
const CONTEXT_BASE: u64 = 0x20_0000;
const CONTEXT_STRIDE: u64 = 0x1000;

#[derive(Debug)]
struct State {
    priority: Vec<u32>,
    level: Vec<bool>,
    pending: Vec<bool>,
    claimed: Vec<bool>,
    /// Per context, one bit per source.
    enable: Vec<Vec<u32>>,
    threshold: Vec<u32>,
}

/// The PLIC. Context `2h` targets hart `h` M-mode, `2h + 1` S-mode.
pub struct Plic {
    num_sources: usize,
    harts: Vec<Arc<HartIrqs>>,
    state: Mutex<State>,
}

impl std::fmt::Debug for Plic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plic")
            .field("num_sources", &self.num_sources)
            .finish_non_exhaustive()
    }
}

impl Plic {
    /// Creates a PLIC with sources `1..num_sources` and two contexts per hart.
    pub fn new(num_sources: usize, harts: Vec<Arc<HartIrqs>>) -> Self {
        let contexts = harts.len() * 2;
        let words = num_sources.div_ceil(32);
        Self {
            num_sources,
            harts,
            state: Mutex::new(State {
                priority: vec![0; num_sources],
                level: vec![false; num_sources],
                pending: vec![false; num_sources],
                claimed: vec![false; num_sources],
                enable: vec![vec![0; words]; contexts],
                threshold: vec![0; contexts],
            }),
        }
    }

    /// Number of interrupt sources (including the reserved source 0).
    pub fn num_sources(&self) -> usize {
        self.num_sources
    }

    /// Returns an [`IrqLine`] driving source `src`.
    pub fn line(self: &Arc<Self>, src: u32) -> Arc<PlicLine> {
        assert!(src > 0 && (src as usize) < self.num_sources);
        Arc::new(PlicLine {
            plic: self.clone(),
            src,
        })
    }

    /// Sets the level of source `src`.
    pub fn set_irq(&self, src: u32, level: bool) {
        let mut s = self.state.lock().unwrap();
        let i = src as usize;
        s.level[i] = level;
        s.pending[i] = level && !s.claimed[i];
        self.update(&s);
    }

    fn best(&self, s: &State, ctx: usize) -> Option<usize> {
        let mut best: Option<(usize, u32)> = None;
        for src in 1..self.num_sources {
            let prio = s.priority[src];
            if s.pending[src]
                && (s.enable[ctx][src / 32] >> (src % 32)) & 1 != 0
                && prio > s.threshold[ctx]
                && best.is_none_or(|(_, p)| prio > p)
            {
                best = Some((src, prio));
            }
        }
        best.map(|(src, _)| src)
    }

    fn update(&self, s: &State) {
        for (h, lines) in self.harts.iter().enumerate() {
            lines.set(irq::MEI, self.best(s, 2 * h).is_some());
            lines.set(irq::SEI, self.best(s, 2 * h + 1).is_some());
        }
    }
}

impl MmioDevice for Plic {
    fn read(&self, offset: u64, _size: AccessSize) -> u64 {
        let mut s = self.state.lock().unwrap();
        let contexts = self.harts.len() * 2;
        let v = match offset {
            o if o < PENDING_BASE => s
                .priority
                .get(((o - PRIORITY_BASE) / 4) as usize)
                .copied()
                .unwrap_or(0),
            o if o < ENABLE_BASE => {
                let word = ((o - PENDING_BASE) / 4) as usize;
                let mut v = 0u32;
                for bit in 0..32 {
                    if s.pending.get(word * 32 + bit) == Some(&true) {
                        v |= 1 << bit;
                    }
                }
                v
            }
            o if o < CONTEXT_BASE => {
                let ctx = ((o - ENABLE_BASE) / ENABLE_STRIDE) as usize;
                let word = ((o - ENABLE_BASE) % ENABLE_STRIDE / 4) as usize;
                s.enable
                    .get(ctx)
                    .and_then(|e| e.get(word))
                    .copied()
                    .unwrap_or(0)
            }
            o => {
                let ctx = ((o - CONTEXT_BASE) / CONTEXT_STRIDE) as usize;
                if ctx >= contexts {
                    return 0;
                }
                match (o - CONTEXT_BASE) % CONTEXT_STRIDE {
                    0 => s.threshold[ctx],
                    4 => {
                        // Claim.
                        match self.best(&s, ctx) {
                            Some(src) => {
                                s.pending[src] = false;
                                s.claimed[src] = true;
                                self.update(&s);
                                src as u32
                            }
                            None => 0,
                        }
                    }
                    _ => 0,
                }
            }
        };
        u64::from(v)
    }

    fn write(&self, offset: u64, _size: AccessSize, value: u64) {
        let v = value as u32;
        let mut s = self.state.lock().unwrap();
        let contexts = self.harts.len() * 2;
        match offset {
            o if o < PENDING_BASE => {
                let src = ((o - PRIORITY_BASE) / 4) as usize;
                if src > 0 && src < self.num_sources {
                    s.priority[src] = v & 7;
                }
            }
            o if o < ENABLE_BASE => {} // pending bits are read-only
            o if o < CONTEXT_BASE => {
                let ctx = ((o - ENABLE_BASE) / ENABLE_STRIDE) as usize;
                let word = ((o - ENABLE_BASE) % ENABLE_STRIDE / 4) as usize;
                if let Some(w) = s.enable.get_mut(ctx).and_then(|e| e.get_mut(word)) {
                    // Source 0 does not exist.
                    *w = if word == 0 { v & !1 } else { v };
                }
            }
            o => {
                let ctx = ((o - CONTEXT_BASE) / CONTEXT_STRIDE) as usize;
                if ctx >= contexts {
                    return;
                }
                match (o - CONTEXT_BASE) % CONTEXT_STRIDE {
                    0 => s.threshold[ctx] = v & 7,
                    4 => {
                        // Complete.
                        let src = v as usize;
                        if src > 0 && src < self.num_sources && s.claimed[src] {
                            s.claimed[src] = false;
                            if s.level[src] {
                                s.pending[src] = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        self.update(&s);
    }
}

/// One PLIC input line.
#[derive(Debug)]
pub struct PlicLine {
    plic: Arc<Plic>,
    src: u32,
}

impl IrqLine for PlicLine {
    fn set_level(&self, level: bool) {
        self.plic.set_irq(self.src, level);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_reg(ctx: u64, reg: u64) -> u64 {
        CONTEXT_BASE + ctx * CONTEXT_STRIDE + reg
    }

    #[test]
    fn claim_complete_cycle() {
        let h = HartIrqs::new();
        let plic = Arc::new(Plic::new(96, vec![h.clone()]));
        let line = plic.line(10);
        // S-mode context 1: enable source 10, priority 1, threshold 0.
        plic.write(4 * 10, AccessSize::Word, 1);
        plic.write(ENABLE_BASE + ENABLE_STRIDE, AccessSize::Word, 1 << 10);

        line.raise();
        assert_eq!(h.get(), irq::SEIP);
        assert_eq!(plic.read(PENDING_BASE, AccessSize::Word), 1 << 10);
        assert_eq!(plic.read(ctx_reg(1, 4), AccessSize::Word), 10);
        assert_eq!(h.get(), 0, "claimed source no longer pending");
        assert_eq!(plic.read(ctx_reg(1, 4), AccessSize::Word), 0);

        // Still asserted at completion -> pending again.
        plic.write(ctx_reg(1, 4), AccessSize::Word, 10);
        assert_eq!(h.get(), irq::SEIP);
        line.lower();
        assert_eq!(h.get(), 0);
    }

    #[test]
    fn priority_and_threshold() {
        let h = HartIrqs::new();
        let plic = Arc::new(Plic::new(96, vec![h.clone()]));
        plic.write(4 * 3, AccessSize::Word, 2);
        plic.write(4 * 5, AccessSize::Word, 5);
        plic.write(ENABLE_BASE, AccessSize::Word, (1 << 3) | (1 << 5));
        plic.set_irq(3, true);
        plic.set_irq(5, true);
        assert_eq!(h.get(), irq::MEIP);
        plic.write(ctx_reg(0, 0), AccessSize::Word, 5);
        assert_eq!(h.get(), 0, "threshold masks both");
        plic.write(ctx_reg(0, 0), AccessSize::Word, 1);
        assert_eq!(plic.read(ctx_reg(0, 4), AccessSize::Word), 5);
        assert_eq!(plic.read(ctx_reg(0, 4), AccessSize::Word), 3);
    }
}
