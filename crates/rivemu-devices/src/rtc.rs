//! Goldfish RTC (`google,goldfish-rtc`), reporting host wall-clock time.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rivemu_core::AccessSize;
use rivemu_mem::MmioDevice;

const TIME_LOW: u64 = 0x00;
const TIME_HIGH: u64 = 0x04;
const ALARM_LOW: u64 = 0x08;
const ALARM_HIGH: u64 = 0x0c;
const IRQ_ENABLED: u64 = 0x10;
const ALARM_STATUS: u64 = 0x18;

#[derive(Debug, Default)]
struct State {
    latched_high: u32,
    alarm: u64,
    irq_enabled: bool,
}

/// Goldfish real-time clock. Alarms are accepted but never fire.
#[derive(Debug, Default)]
pub struct GoldfishRtc {
    state: Mutex<State>,
}

impl GoldfishRtc {
    /// Creates the RTC.
    pub fn new() -> Self {
        Self::default()
    }

    fn now_ns() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
    }
}

impl MmioDevice for GoldfishRtc {
    fn read(&self, offset: u64, _size: AccessSize) -> u64 {
        let mut s = self.state.lock().unwrap();
        let v = match offset {
            TIME_LOW => {
                let now = Self::now_ns();
                s.latched_high = (now >> 32) as u32;
                now as u32
            }
            TIME_HIGH => s.latched_high,
            ALARM_LOW => s.alarm as u32,
            ALARM_HIGH => (s.alarm >> 32) as u32,
            IRQ_ENABLED => u32::from(s.irq_enabled),
            ALARM_STATUS => 0,
            _ => 0,
        };
        u64::from(v)
    }

    fn write(&self, offset: u64, _size: AccessSize, value: u64) {
        let mut s = self.state.lock().unwrap();
        let v = value & 0xffff_ffff;
        match offset {
            ALARM_LOW => s.alarm = (s.alarm & !0xffff_ffff) | v,
            ALARM_HIGH => s.alarm = (s.alarm & 0xffff_ffff) | (v << 32),
            IRQ_ENABLED => s.irq_enabled = v & 1 != 0,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_wall_clock() {
        let rtc = GoldfishRtc::new();
        let lo = rtc.read(TIME_LOW, AccessSize::Word);
        let hi = rtc.read(TIME_HIGH, AccessSize::Word);
        let t = (hi << 32) | lo;
        // After 2020-01-01.
        assert!(t > 1_577_836_800 * 1_000_000_000);
    }
}
