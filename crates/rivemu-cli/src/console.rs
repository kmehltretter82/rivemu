//! Host console: stdin reader thread, raw terminal mode and the QEMU-style
//! `Ctrl-A` escape sequences (`Ctrl-A x` quits, `Ctrl-A h` shows help).

use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use rustix::termios::{self, OptionalActions, Termios};

/// What the console produced for the machine loop.
#[derive(Debug, PartialEq, Eq)]
pub enum ConsoleEvent {
    /// Bytes for the guest.
    Input(Vec<u8>),
    /// The user requested exit (`Ctrl-A x`) or stdin closed in mux mode.
    Quit,
}

/// Escape-sequence state machine for the stdio multiplexer.
#[derive(Debug, Default)]
pub struct Mux {
    escape: bool,
}

const CTRL_A: u8 = 0x01;

impl Mux {
    /// Processes raw input. Returns guest bytes, whether to quit and an
    /// optional message for the user.
    pub fn process(&mut self, data: &[u8]) -> (Vec<u8>, bool, Option<&'static str>) {
        let mut out = Vec::with_capacity(data.len());
        let mut msg = None;
        for &b in data {
            if self.escape {
                self.escape = false;
                match b {
                    b'x' | b'X' => return (out, true, msg),
                    CTRL_A | b'a' => out.push(CTRL_A),
                    b'h' | b'?' => msg = Some(MUX_HELP),
                    b'c' => msg = Some("\r\nrivemu: monitor is not available yet\r\n"),
                    _ => {}
                }
            } else if b == CTRL_A {
                self.escape = true;
            } else {
                out.push(b);
            }
        }
        (out, false, msg)
    }
}

const MUX_HELP: &str = "\r\nC-a h    print this help\r\n\
C-a x    exit emulator\r\n\
C-a C-a  sends C-a\r\n";

/// Restores the terminal mode on drop.
#[derive(Debug)]
pub struct RawModeGuard {
    saved: Option<Termios>,
}

impl RawModeGuard {
    /// Switches stdin to raw mode if it is a terminal. With `full`, signal
    /// keys (Ctrl-C) are passed to the guest as well.
    pub fn enable(full: bool) -> Self {
        let stdin = std::io::stdin();
        let fd = stdin.as_fd();
        if !termios::isatty(fd) {
            return Self { saved: None };
        }
        let Ok(saved) = termios::tcgetattr(fd) else {
            return Self { saved: None };
        };
        let mut raw = saved.clone();
        raw.make_raw();
        if !full {
            raw.local_modes |= termios::LocalModes::ISIG;
        }
        if termios::tcsetattr(fd, OptionalActions::Now, &raw).is_err() {
            return Self { saved: None };
        }
        Self { saved: Some(saved) }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if let Some(t) = &self.saved {
            let _ = termios::tcsetattr(std::io::stdin().as_fd(), OptionalActions::Now, t);
        }
    }
}

/// Reads stdin on a background thread.
#[derive(Debug)]
pub struct Console {
    rx: Receiver<Option<Vec<u8>>>,
    mux: Option<Mux>,
    closed: bool,
}

impl Console {
    /// Starts the reader thread. With `mux`, `Ctrl-A` sequences are handled.
    pub fn spawn(mux: bool) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("stdin".into())
            .spawn(move || {
                let mut buf = [0u8; 1024];
                let mut stdin = std::io::stdin().lock();
                loop {
                    match stdin.read(&mut buf) {
                        Ok(0) | Err(_) => {
                            let _ = tx.send(None);
                            return;
                        }
                        Ok(n) => {
                            if tx.send(Some(buf[..n].to_vec())).is_err() {
                                return;
                            }
                        }
                    }
                }
            })
            .expect("spawn stdin thread");
        Self {
            rx,
            mux: mux.then(Mux::default),
            closed: false,
        }
    }

    fn handle(&mut self, data: Option<Vec<u8>>) -> Option<ConsoleEvent> {
        let Some(data) = data else {
            self.closed = true;
            return None;
        };
        match &mut self.mux {
            None => Some(ConsoleEvent::Input(data)),
            Some(mux) => {
                let (out, quit, msg) = mux.process(&data);
                if let Some(m) = msg {
                    let mut o = std::io::stdout().lock();
                    let _ = o.write_all(m.as_bytes());
                    let _ = o.flush();
                }
                if quit {
                    Some(ConsoleEvent::Quit)
                } else {
                    Some(ConsoleEvent::Input(out))
                }
            }
        }
    }

    /// Returns pending input without blocking, or waits up to `timeout`.
    pub fn poll(&mut self, timeout: Duration) -> Vec<ConsoleEvent> {
        let mut events = Vec::new();
        if self.closed {
            if !timeout.is_zero() {
                std::thread::sleep(timeout);
            }
            return events;
        }
        if !timeout.is_zero() {
            match self.rx.recv_timeout(timeout) {
                Ok(d) => events.extend(self.handle(d)),
                Err(RecvTimeoutError::Timeout) => return events,
                Err(RecvTimeoutError::Disconnected) => {
                    self.closed = true;
                    return events;
                }
            }
        }
        while let Ok(d) = self.rx.try_recv() {
            events.extend(self.handle(d));
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mux_sequences() {
        let mut m = Mux::default();
        assert_eq!(m.process(b"ls\r"), (b"ls\r".to_vec(), false, None));
        assert_eq!(m.process(&[CTRL_A, CTRL_A, b'z']).0, vec![CTRL_A, b'z']);
        let (out, quit, _) = m.process(&[b'a', CTRL_A, b'x', b'b']);
        assert_eq!((out, quit), (b"a".to_vec(), true));
        // Escape split across reads.
        assert_eq!(m.process(&[CTRL_A]), (vec![], false, None));
        assert!(m.process(b"x").1);
        assert!(m.process(&[CTRL_A, b'h']).2.is_some());
    }
}
