//! NS16550A UART.
//!
//! Transmission is instantaneous: written bytes are collected in an output
//! buffer that the machine loop drains to the character backend.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use rivemu_core::{AccessSize, IrqLine};
use rivemu_mem::MmioDevice;

const IER_RDI: u8 = 0x01;
const IER_THRI: u8 = 0x02;
const LCR_DLAB: u8 = 0x80;
const MCR_LOOP: u8 = 0x10;
const LSR_DR: u8 = 0x01;
const LSR_THRE: u8 = 0x20;
const LSR_TEMT: u8 = 0x40;
const FCR_ENABLE: u8 = 0x01;
const FCR_CLEAR_RX: u8 = 0x02;

#[derive(Debug, Default)]
struct State {
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    fcr: u8,
    dll: u8,
    dlm: u8,
    rx: VecDeque<u8>,
    tx: Vec<u8>,
    thre_pending: bool,
}

impl State {
    fn irq_level(&self) -> bool {
        (self.ier & IER_RDI != 0 && !self.rx.is_empty())
            || (self.ier & IER_THRI != 0 && self.thre_pending)
    }
}

/// An NS16550A-compatible UART.
pub struct Uart16550 {
    state: Mutex<State>,
    irq: Arc<dyn IrqLine>,
}

impl std::fmt::Debug for Uart16550 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Uart16550").finish_non_exhaustive()
    }
}

impl Uart16550 {
    /// Creates a UART raising `irq`.
    pub fn new(irq: Arc<dyn IrqLine>) -> Self {
        Self {
            state: Mutex::new(State::default()),
            irq,
        }
    }

    fn update(&self, s: &State) {
        self.irq.set_level(s.irq_level());
    }

    /// Queues received bytes (from the host).
    pub fn push_input(&self, data: &[u8]) {
        let mut s = self.state.lock().unwrap();
        s.rx.extend(data);
        self.update(&s);
    }

    /// Whether the receive buffer is empty (used for flow control).
    pub fn input_empty(&self) -> bool {
        self.state.lock().unwrap().rx.is_empty()
    }

    /// Takes all bytes transmitted by the guest since the last call.
    pub fn take_output(&self) -> Vec<u8> {
        std::mem::take(&mut self.state.lock().unwrap().tx)
    }

    /// Exports `(ier, lcr, mcr, scr, fcr, dll, dlm, thre_pending)` for VM snapshotting.
    pub fn snapshot(&self) -> [u8; 8] {
        let s = self.state.lock().unwrap();
        [
            s.ier,
            s.lcr,
            s.mcr,
            s.scr,
            s.fcr,
            s.dll,
            s.dlm,
            u8::from(s.thre_pending),
        ]
    }

    /// Restores UART register state from a VM snapshot.
    pub fn restore(&self, regs: [u8; 8]) {
        let mut s = self.state.lock().unwrap();
        s.ier = regs[0];
        s.lcr = regs[1];
        s.mcr = regs[2];
        s.scr = regs[3];
        s.fcr = regs[4];
        s.dll = regs[5];
        s.dlm = regs[6];
        s.thre_pending = regs[7] != 0;
        self.update(&s);
    }
}

impl MmioDevice for Uart16550 {
    fn read(&self, offset: u64, _size: AccessSize) -> u64 {
        let mut s = self.state.lock().unwrap();
        let dlab = s.lcr & LCR_DLAB != 0;
        let v = match offset {
            0 if dlab => s.dll,
            0 => {
                let b = s.rx.pop_front().unwrap_or(0);
                self.update(&s);
                b
            }
            1 if dlab => s.dlm,
            1 => s.ier,
            2 => {
                let fifo = if s.fcr & FCR_ENABLE != 0 { 0xc0 } else { 0 };
                let id = if s.ier & IER_RDI != 0 && !s.rx.is_empty() {
                    0x04
                } else if s.ier & IER_THRI != 0 && s.thre_pending {
                    // Reading IIR acknowledges the THRE interrupt.
                    s.thre_pending = false;
                    self.update(&s);
                    0x02
                } else {
                    0x01
                };
                fifo | id
            }
            3 => s.lcr,
            4 => s.mcr,
            5 => {
                let dr = if s.rx.is_empty() { 0 } else { LSR_DR };
                dr | LSR_THRE | LSR_TEMT
            }
            6 => {
                if s.mcr & MCR_LOOP != 0 {
                    // DTR->DSR, RTS->CTS, OUT1->RI, OUT2->DCD.
                    let m = s.mcr;
                    ((m & 0x02) << 3) | ((m & 0x01) << 5) | ((m & 0x04) << 4) | ((m & 0x08) << 4)
                } else {
                    0xb0 // DCD | DSR | CTS
                }
            }
            7 => s.scr,
            _ => 0,
        };
        u64::from(v)
    }

    fn write(&self, offset: u64, _size: AccessSize, value: u64) {
        let v = value as u8;
        let mut s = self.state.lock().unwrap();
        let dlab = s.lcr & LCR_DLAB != 0;
        match offset {
            0 if dlab => s.dll = v,
            0 => {
                if s.mcr & MCR_LOOP != 0 {
                    s.rx.push_back(v);
                } else {
                    s.tx.push(v);
                }
                s.thre_pending = true;
            }
            1 if dlab => s.dlm = v,
            1 => {
                let enabling_thri = v & IER_THRI != 0 && s.ier & IER_THRI == 0;
                s.ier = v & 0x0f;
                if enabling_thri {
                    s.thre_pending = true;
                }
            }
            2 => {
                s.fcr = v;
                if v & FCR_CLEAR_RX != 0 {
                    s.rx.clear();
                }
            }
            3 => s.lcr = v,
            4 => s.mcr = v & 0x1f,
            7 => s.scr = v,
            _ => {}
        }
        self.update(&s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Default)]
    struct Line(AtomicBool);
    impl IrqLine for Line {
        fn set_level(&self, level: bool) {
            self.0.store(level, Ordering::Relaxed);
        }
    }

    fn uart() -> (Uart16550, Arc<Line>) {
        let line = Arc::new(Line::default());
        (Uart16550::new(line.clone()), line)
    }

    #[test]
    fn transmit_and_receive() {
        let (u, line) = uart();
        u.write(0, AccessSize::Byte, b'h' as u64);
        u.write(0, AccessSize::Byte, b'i' as u64);
        assert_eq!(u.take_output(), b"hi");
        assert_eq!(u.read(5, AccessSize::Byte) & 0x61, 0x60);

        u.write(1, AccessSize::Byte, IER_RDI as u64);
        assert!(!line.0.load(Ordering::Relaxed));
        u.push_input(b"x");
        assert!(line.0.load(Ordering::Relaxed));
        assert_eq!(u.read(2, AccessSize::Byte) & 0xf, 0x04);
        assert_eq!(u.read(5, AccessSize::Byte) & 1, 1);
        assert_eq!(u.read(0, AccessSize::Byte), b'x' as u64);
        assert!(!line.0.load(Ordering::Relaxed));
    }

    #[test]
    fn thre_interrupt() {
        let (u, line) = uart();
        u.write(1, AccessSize::Byte, IER_THRI as u64);
        assert!(line.0.load(Ordering::Relaxed));
        assert_eq!(u.read(2, AccessSize::Byte), 0x02);
        assert!(!line.0.load(Ordering::Relaxed));
        assert_eq!(u.read(2, AccessSize::Byte), 0x01);
    }

    #[test]
    fn divisor_latch_and_loopback() {
        let (u, _) = uart();
        u.write(3, AccessSize::Byte, 0x83);
        u.write(0, AccessSize::Byte, 0x12);
        assert_eq!(u.read(0, AccessSize::Byte), 0x12);
        u.write(3, AccessSize::Byte, 0x03);
        u.write(4, AccessSize::Byte, 0x1f);
        u.write(0, AccessSize::Byte, 0x55);
        assert!(u.take_output().is_empty());
        assert_eq!(u.read(0, AccessSize::Byte), 0x55);
        assert_eq!(u.read(6, AccessSize::Byte), 0xf0);
    }
}
