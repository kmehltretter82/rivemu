//! Power control: the SiFive test device (`sifive,test0`) used by Linux
//! and OpenSBI for poweroff / reboot.

use std::sync::{Arc, Mutex};

use rivemu_core::AccessSize;
use rivemu_mem::MmioDevice;

const FINISHER_FAIL: u64 = 0x3333;
const FINISHER_PASS: u64 = 0x5555;
const FINISHER_RESET: u64 = 0x7777;

/// A power state change requested by the guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerEvent {
    /// Power off with the given exit status (0 = success).
    Poweroff(i32),
    /// System reset.
    Reset,
}

/// Shared mailbox for power events, polled by the machine loop.
#[derive(Debug, Default)]
pub struct PowerControl {
    event: Mutex<Option<PowerEvent>>,
}

impl PowerControl {
    /// Creates an empty mailbox.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Records a request (first request wins until taken).
    pub fn request(&self, ev: PowerEvent) {
        self.event.lock().unwrap().get_or_insert(ev);
    }

    /// Takes the pending request, if any.
    pub fn take(&self) -> Option<PowerEvent> {
        self.event.lock().unwrap().take()
    }
}

/// The `sifive,test0` finisher device.
#[derive(Debug)]
pub struct SifiveTest {
    power: Arc<PowerControl>,
}

impl SifiveTest {
    /// Creates the device.
    pub fn new(power: Arc<PowerControl>) -> Self {
        Self { power }
    }
}

impl MmioDevice for SifiveTest {
    fn read(&self, _offset: u64, _size: AccessSize) -> u64 {
        0
    }

    fn write(&self, offset: u64, _size: AccessSize, value: u64) {
        if offset != 0 {
            return;
        }
        match value & 0xffff {
            FINISHER_PASS => self.power.request(PowerEvent::Poweroff(0)),
            FINISHER_FAIL => self
                .power
                .request(PowerEvent::Poweroff(((value >> 16) & 0xffff) as i32)),
            FINISHER_RESET => self.power.request(PowerEvent::Reset),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finisher_codes() {
        let p = PowerControl::new();
        let t = SifiveTest::new(p.clone());
        t.write(0, AccessSize::Word, 0x1234);
        assert_eq!(p.take(), None);
        t.write(0, AccessSize::Word, FINISHER_PASS);
        assert_eq!(p.take(), Some(PowerEvent::Poweroff(0)));
        t.write(0, AccessSize::Word, (3 << 16) | FINISHER_FAIL);
        assert_eq!(p.take(), Some(PowerEvent::Poweroff(3)));
        t.write(0, AccessSize::Word, FINISHER_RESET);
        assert_eq!(p.take(), Some(PowerEvent::Reset));
    }
}
