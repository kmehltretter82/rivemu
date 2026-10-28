//! `virtio-input` (VirtIO Device ID 18) keyboard and tablet device (`evdev`).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use rivemu_core::AccessSize;

use crate::{DmaRam, VirtQueue, VirtioDevice};

/// Linux `evdev` event types (`include/uapi/linux/input-event-codes.h`).
pub mod ev {
    /// Synchronization event (`SYN_REPORT = 0`).
    pub const EV_SYN: u16 = 0x00;
    /// Key or button press/release (`0 = release`, `1 = press`, `2 = repeat`).
    pub const EV_KEY: u16 = 0x01;
    /// Relative axis movement (`REL_X = 0`, `REL_Y = 1`).
    pub const EV_REL: u16 = 0x02;
    /// Absolute axis position (`ABS_X = 0`, `ABS_Y = 1` in `0..=32767`).
    pub const EV_ABS: u16 = 0x03;

    /// Left mouse button (`BTN_LEFT`).
    pub const BTN_LEFT: u16 = 0x110;
    /// Right mouse button (`BTN_RIGHT`).
    pub const BTN_RIGHT: u16 = 0x111;
    /// Middle mouse button (`BTN_MIDDLE`).
    pub const BTN_MIDDLE: u16 = 0x112;
}

const VIRTIO_INPUT_CFG_UNSET: u8 = 0x00;
const VIRTIO_INPUT_CFG_ID_NAME: u8 = 0x01;
const VIRTIO_INPUT_CFG_ID_SERIAL: u8 = 0x02;
const VIRTIO_INPUT_CFG_ID_DEVIDS: u8 = 0x03;
const VIRTIO_INPUT_CFG_PROP_BITS: u8 = 0x10;
const VIRTIO_INPUT_CFG_EV_BITS: u8 = 0x11;
const VIRTIO_INPUT_CFG_ABS_INFO: u8 = 0x12;

/// One 8-byte `virtio_input_event` delivered over `eventq` (queue 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtioInputEvent {
    /// Event type (e.g. [`ev::EV_KEY`], [`ev::EV_ABS`], [`ev::EV_SYN`]).
    pub type_: u16,
    /// Event code (e.g. Linux `KEY_*` or `ABS_X`/`ABS_Y`).
    pub code: u16,
    /// Event value (`1` = key down, `0` = key up, or coordinate).
    pub value: u32,
}

impl VirtioInputEvent {
    /// Encodes the event as 8 little-endian bytes (`le16 type, le16 code, le32 value`).
    pub fn to_le_bytes(self) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[0..2].copy_from_slice(&self.type_.to_le_bytes());
        out[2..4].copy_from_slice(&self.code.to_le_bytes());
        out[4..8].copy_from_slice(&self.value.to_le_bytes());
        out
    }
}

/// Shared event queue feeding [`VirtioInput`] from host VNC clients, HMP `sendkey`, or tests.
#[derive(Debug, Default)]
pub struct InputHub {
    queue: Mutex<VecDeque<VirtioInputEvent>>,
    last_buttons: Mutex<u8>,
}

impl InputHub {
    /// Creates an empty [`InputHub`].
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Pushes a raw [`VirtioInputEvent`] without appending `SYN_REPORT`.
    pub fn push_raw(&self, event: VirtioInputEvent) {
        self.queue.lock().unwrap().push_back(event);
    }

    /// Pushes an `EV_KEY` event followed immediately by `EV_SYN / SYN_REPORT`.
    pub fn send_key(&self, code: u16, down: bool) {
        let mut q = self.queue.lock().unwrap();
        q.push_back(VirtioInputEvent {
            type_: ev::EV_KEY,
            code,
            value: u32::from(down),
        });
        q.push_back(VirtioInputEvent {
            type_: ev::EV_SYN,
            code: 0,
            value: 0,
        });
    }

    /// Sends a full key press + release by QEMU key name (e.g. `"ret"`, `"esc"`, `"spc"`, `"a"`).
    pub fn send_key_name(&self, name: &str) -> bool {
        let Some(code) = key_name_to_linux_code(name) else {
            return false;
        };
        self.send_key(code, true);
        self.send_key(code, false);
        true
    }

    /// Pushes an absolute pointer movement (`x`, `y` normalized to `0..=32767`)
    /// and any button changes (`bit 0 = left`, `bit 1 = middle`, `bit 2 = right`).
    pub fn send_pointer_abs(&self, x_norm: u32, y_norm: u32, buttons: u8) {
        let mut q = self.queue.lock().unwrap();
        q.push_back(VirtioInputEvent {
            type_: ev::EV_ABS,
            code: 0, // ABS_X
            value: x_norm.min(32767),
        });
        q.push_back(VirtioInputEvent {
            type_: ev::EV_ABS,
            code: 1, // ABS_Y
            value: y_norm.min(32767),
        });
        let mut prev = self.last_buttons.lock().unwrap();
        let diff = *prev ^ buttons;
        if diff & 1 != 0 {
            q.push_back(VirtioInputEvent {
                type_: ev::EV_KEY,
                code: ev::BTN_LEFT,
                value: u32::from(buttons & 1 != 0),
            });
        }
        if diff & 2 != 0 {
            q.push_back(VirtioInputEvent {
                type_: ev::EV_KEY,
                code: ev::BTN_MIDDLE,
                value: u32::from(buttons & 2 != 0),
            });
        }
        if diff & 4 != 0 {
            q.push_back(VirtioInputEvent {
                type_: ev::EV_KEY,
                code: ev::BTN_RIGHT,
                value: u32::from(buttons & 4 != 0),
            });
        }
        *prev = buttons;
        q.push_back(VirtioInputEvent {
            type_: ev::EV_SYN,
            code: 0,
            value: 0,
        });
    }

    fn pop(&self) -> Option<VirtioInputEvent> {
        self.queue.lock().unwrap().pop_front()
    }
}

/// VirtIO Input device (`virtio-keyboard-device` / `virtio-input-pci`, Device ID 18).
#[derive(Debug)]
pub struct VirtioInput {
    name: String,
    serial: String,
    select: u8,
    subsel: u8,
    payload: [u8; 128],
    payload_size: u8,
    hub: Arc<InputHub>,
    led_state: u32,
}

impl Default for VirtioInput {
    fn default() -> Self {
        Self::new(InputHub::new())
    }
}

impl VirtioInput {
    /// Creates a new `VirtioInput` connected to `hub`.
    pub fn new(hub: Arc<InputHub>) -> Self {
        let mut dev = Self {
            name: "rivemu VirtIO Keyboard/Tablet".into(),
            serial: "rivemu-input-0".into(),
            select: 0,
            subsel: 0,
            payload: [0u8; 128],
            payload_size: 0,
            hub,
            led_state: 0,
        };
        dev.refresh_config();
        dev
    }

    /// Returns the [`InputHub`] used to inject events into this device.
    pub fn hub(&self) -> Arc<InputHub> {
        self.hub.clone()
    }

    /// Last LED status value written by the guest driver on `statusq`.
    pub fn led_state(&self) -> u32 {
        self.led_state
    }

    fn refresh_config(&mut self) {
        self.payload = [0u8; 128];
        self.payload_size = 0;
        match self.select {
            VIRTIO_INPUT_CFG_UNSET => {}
            VIRTIO_INPUT_CFG_ID_NAME => {
                let bytes = self.name.as_bytes();
                let n = bytes.len().min(128);
                self.payload[..n].copy_from_slice(&bytes[..n]);
                self.payload_size = n as u8;
            }
            VIRTIO_INPUT_CFG_ID_SERIAL => {
                let bytes = self.serial.as_bytes();
                let n = bytes.len().min(128);
                self.payload[..n].copy_from_slice(&bytes[..n]);
                self.payload_size = n as u8;
            }
            VIRTIO_INPUT_CFG_ID_DEVIDS => {
                // struct virtio_input_devids: bustype=0x06 (BUS_VIRTUAL), vendor=0x554d, product=1, version=1
                self.payload[0..2].copy_from_slice(&0x0006u16.to_le_bytes());
                self.payload[2..4].copy_from_slice(&0x554du16.to_le_bytes());
                self.payload[4..6].copy_from_slice(&0x0001u16.to_le_bytes());
                self.payload[6..8].copy_from_slice(&0x0001u16.to_le_bytes());
                self.payload_size = 8;
            }
            VIRTIO_INPUT_CFG_PROP_BITS => {
                // INPUT_PROP_DIRECT (bit 1) = 0 (pointer/tablet with cursor)
                self.payload[0] = 0x01; // INPUT_PROP_POINTER
                self.payload_size = 1;
            }
            VIRTIO_INPUT_CFG_EV_BITS => match self.subsel {
                0 => {
                    // Supported EV_* types: EV_SYN (0), EV_KEY (1), EV_REL (2), EV_ABS (3).
                    self.payload[0] = (1 << ev::EV_SYN)
                        | (1 << ev::EV_KEY)
                        | (1 << ev::EV_REL)
                        | (1 << ev::EV_ABS);
                    self.payload_size = 1;
                }
                1 => {
                    // EV_KEY bitmap: keys 1..=127 (bytes 0..16) + mouse buttons 0x110..=0x112 (byte 34).
                    self.payload[0] = 0xfe;
                    for b in &mut self.payload[1..16] {
                        *b = 0xff;
                    }
                    self.payload[34] = 0x07; // BTN_LEFT, BTN_RIGHT, BTN_MIDDLE
                    self.payload_size = 35;
                }
                2 => {
                    // EV_REL bitmap: REL_X (0), REL_Y (1).
                    self.payload[0] = 0x03;
                    self.payload_size = 1;
                }
                3 => {
                    // EV_ABS bitmap: ABS_X (0), ABS_Y (1).
                    self.payload[0] = 0x03;
                    self.payload_size = 1;
                }
                _ => {}
            },
            VIRTIO_INPUT_CFG_ABS_INFO if self.subsel <= 1 => {
                // struct virtio_input_absinfo { min=0, max=32767, fuzz=0, flat=0, res=0 }
                self.payload[4..8].copy_from_slice(&32767u32.to_le_bytes());
                self.payload_size = 20;
            }
            _ => {}
        }
    }
}

impl VirtioDevice for VirtioInput {
    fn device_id(&self) -> u32 {
        18
    }

    fn num_queues(&self) -> usize {
        2
    }

    fn device_features(&self) -> u64 {
        0
    }

    fn read_config(&self, offset: u64, size: AccessSize) -> u64 {
        let off = offset as usize;
        let n = size.bytes() as usize;
        let byte_at = |idx: usize| -> u8 {
            match idx {
                0 => self.select,
                1 => self.subsel,
                2 => self.payload_size,
                3..=7 => 0,
                8..=135 => self.payload[idx - 8],
                _ => 0,
            }
        };
        let mut v = 0u64;
        for i in 0..n {
            v |= u64::from(byte_at(off + i)) << (8 * i);
        }
        v
    }

    fn write_config(&mut self, offset: u64, size: AccessSize, value: u64) {
        let off = offset as usize;
        let n = size.bytes() as usize;
        for i in 0..n {
            let b = ((value >> (8 * i)) & 0xff) as u8;
            match off + i {
                0 => self.select = b,
                1 => self.subsel = b,
                _ => {}
            }
        }
        if off <= 1 {
            self.refresh_config();
        }
    }

    fn process_queue(
        &mut self,
        queue_idx: usize,
        queue: &mut VirtQueue,
        dma: &DmaRam,
        event_idx: bool,
    ) -> u32 {
        if queue_idx == 0 {
            // Queue 0 (`eventq`): fill any available guest buffers with pending input events.
            let mut used = 0;
            while queue.has_available(dma) {
                let Some(ev) = self.hub.pop() else {
                    break;
                };
                let Some(chain) = queue.pop(dma) else {
                    break;
                };
                let bytes = ev.to_le_bytes();
                let written = chain.write_all(dma, &bytes).unwrap_or(0);
                queue.push_used(dma, chain.head, written, event_idx);
                used += 1;
            }
            used
        } else {
            // Queue 1 (`statusq`): guest -> host status/LED events.
            let mut used = 0;
            while let Some(chain) = queue.pop(dma) {
                if let Some(buf) = chain.read_all(dma).filter(|b| b.len() >= 8) {
                    self.led_state = u32::from_le_bytes(buf[4..8].try_into().unwrap());
                }
                queue.push_used(dma, chain.head, 0, event_idx);
                used += 1;
            }
            used
        }
    }

    fn poll(&mut self, queues: &mut [VirtQueue], dma: &DmaRam, event_idx: bool) -> u32 {
        let Some(eventq) = queues.first_mut() else {
            return 0;
        };
        self.process_queue(0, eventq, dma, event_idx)
    }
}

/// Maps QEMU/HMP key names and X11 keysyms to Linux `KEY_*` codes.
pub fn key_name_to_linux_code(name: &str) -> Option<u16> {
    Some(match name.to_ascii_lowercase().as_str() {
        "esc" | "escape" => 1,
        "1" => 2,
        "2" => 3,
        "3" => 4,
        "4" => 5,
        "5" => 6,
        "6" => 7,
        "7" => 8,
        "8" => 9,
        "9" => 10,
        "0" => 11,
        "minus" | "-" => 12,
        "equal" | "=" => 13,
        "backspace" | "bs" => 14,
        "tab" => 15,
        "q" => 16,
        "w" => 17,
        "e" => 18,
        "r" => 19,
        "t" => 20,
        "y" => 21,
        "u" => 22,
        "i" => 23,
        "o" => 24,
        "p" => 25,
        "ret" | "return" | "enter" => 28,
        "ctrl" | "ctrl_l" => 29,
        "a" => 30,
        "s" => 31,
        "d" => 32,
        "f" => 33,
        "g" => 34,
        "h" => 35,
        "j" => 36,
        "k" => 37,
        "l" => 38,
        "shift" | "shift_l" => 42,
        "z" => 44,
        "x" => 45,
        "c" => 46,
        "v" => 47,
        "b" => 48,
        "n" => 49,
        "m" => 50,
        " comma" | "comma" | "," => 51,
        "dot" | "." => 52,
        "slash" | "/" => 53,
        "alt" | "alt_l" => 56,
        "spc" | "space" | " " => 57,
        "up" => 103,
        "left" => 105,
        "right" => 106,
        "down" => 108,
        _ => return None,
    })
}

/// Maps an X11 keysym (sent by RFB/VNC `KeyEvent`) to a Linux `KEY_*` code.
pub fn keysym_to_linux_code(keysym: u32) -> Option<u16> {
    match keysym {
        0xff1b => Some(1),   // XK_Escape
        0xff08 => Some(14),  // XK_BackSpace
        0xff09 => Some(15),  // XK_Tab
        0xff0d => Some(28),  // XK_Return
        0xffe1 => Some(42),  // XK_Shift_L
        0xffe2 => Some(54),  // XK_Shift_R
        0xffe3 => Some(29),  // XK_Control_L
        0xffe9 => Some(56),  // XK_Alt_L
        0xff51 => Some(105), // XK_Left
        0xff52 => Some(103), // XK_Up
        0xff53 => Some(106), // XK_Right
        0xff54 => Some(108), // XK_Down
        0x20..=0x7e => {
            let ch = (keysym as u8 as char).to_ascii_lowercase();
            let mut buf = [0u8; 4];
            key_name_to_linux_code(ch.encode_utf8(&mut buf))
        }
        _ => None,
    }
}
