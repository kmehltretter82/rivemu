//! Built-in RFB 3.8 (`VNC`) server exporting [`Framebuffer`] and injecting [`InputHub`] events.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use crate::gpu::Framebuffer;
use crate::input::{InputHub, keysym_to_linux_code};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientStage {
    WaitVersion,
    WaitSecurity,
    WaitClientInit,
    Active,
}

#[derive(Debug)]
struct VncClient {
    stream: TcpStream,
    stage: ClientStage,
    rx_buf: Vec<u8>,
    last_gen: u64,
    bgrx: bool,
}

/// Non-blocking RFB `003.008` VNC server.
#[derive(Debug)]
pub struct VncServer {
    listener: TcpListener,
    local_addr: String,
    fb: Arc<Framebuffer>,
    input: Arc<InputHub>,
    client: Option<VncClient>,
}

impl VncServer {
    /// Binds a VNC server from a QEMU `-vnc` display specification
    /// (e.g. `":0"` -> `127.0.0.1:5900`, `"0.0.0.0:1"` -> `0.0.0.0:5901`, or `"127.0.0.1:5900"`).
    pub fn bind(spec: &str, fb: Arc<Framebuffer>, input: Arc<InputHub>) -> io::Result<Self> {
        let base = spec.split(',').next().unwrap_or(":0").trim();
        let addr = parse_vnc_addr(base);
        let listener = TcpListener::bind(&addr)?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr().map(|a| a.to_string()).unwrap_or(addr);
        Ok(Self {
            listener,
            local_addr,
            fb,
            input,
            client: None,
        })
    }

    /// Bound `ip:port` address of the VNC server.
    pub fn local_addr(&self) -> &str {
        &self.local_addr
    }

    /// Polls for new connections and RFB client requests without blocking.
    pub fn poll(&mut self) {
        if self.client.is_none() {
            self.try_accept();
        }
        let Some(mut client) = self.client.take() else {
            return;
        };
        if self.service_client(&mut client).is_ok() {
            self.client = Some(client);
        }
    }

    fn try_accept(&mut self) {
        let Ok((mut stream, _)) = self.listener.accept() else {
            return;
        };
        let _ = stream.set_nonblocking(true);
        let _ = stream.set_nodelay(true);
        if stream.write_all(b"RFB 003.008\n").is_ok() {
            let _ = stream.flush();
            self.client = Some(VncClient {
                stream,
                stage: ClientStage::WaitVersion,
                rx_buf: Vec::with_capacity(256),
                last_gen: 0,
                bgrx: true,
            });
        }
    }

    fn service_client(&self, c: &mut VncClient) -> io::Result<()> {
        let mut tmp = [0u8; 512];
        let mut eof = false;
        loop {
            match c.stream.read(&mut tmp) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => c.rx_buf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }

        loop {
            match c.stage {
                ClientStage::WaitVersion => {
                    if c.rx_buf.len() < 12 {
                        break;
                    }
                    c.rx_buf.drain(..12);
                    // Offer 1 security type: 1 (None).
                    c.stream.write_all(&[1, 1])?;
                    c.stream.flush()?;
                    c.stage = ClientStage::WaitSecurity;
                }
                ClientStage::WaitSecurity => {
                    if c.rx_buf.is_empty() {
                        break;
                    }
                    c.rx_buf.drain(..1);
                    // SecurityResult = 0 (OK).
                    c.stream.write_all(&0u32.to_be_bytes())?;
                    c.stream.flush()?;
                    c.stage = ClientStage::WaitClientInit;
                }
                ClientStage::WaitClientInit => {
                    if c.rx_buf.is_empty() {
                        break;
                    }
                    c.rx_buf.drain(..1);
                    let (w, h, _) = self.fb.dimensions();
                    let name = b"rivemu";
                    let mut init = Vec::with_capacity(24 + name.len());
                    init.extend_from_slice(&(w as u16).to_be_bytes());
                    init.extend_from_slice(&(h as u16).to_be_bytes());
                    // 32-bit truecolor BGRX pixel format (standard VNC default).
                    init.extend_from_slice(&[
                        32, // bits-per-pixel
                        24, // depth
                        0,  // big-endian-flag = 0 (little-endian)
                        1,  // true-colour-flag = 1
                        0, 255, // red-max = 255
                        0, 255, // green-max = 255
                        0, 255, // blue-max = 255
                        16,  // red-shift = 16
                        8,   // green-shift = 8
                        0,   // blue-shift = 0
                        0, 0, 0, // padding
                    ]);
                    init.extend_from_slice(&(name.len() as u32).to_be_bytes());
                    init.extend_from_slice(name);
                    c.stream.write_all(&init)?;
                    c.stream.flush()?;
                    c.stage = ClientStage::Active;
                }
                ClientStage::Active => {
                    let Some(&msg_type) = c.rx_buf.first() else {
                        break;
                    };
                    match msg_type {
                        // SetPixelFormat (20 bytes)
                        0 => {
                            if c.rx_buf.len() < 20 {
                                break;
                            }
                            let red_shift = c.rx_buf[14];
                            c.bgrx = red_shift == 16;
                            c.rx_buf.drain(..20);
                        }
                        // SetEncodings (4 + 4 * n bytes)
                        2 => {
                            if c.rx_buf.len() < 4 {
                                break;
                            }
                            let n = u16::from_be_bytes([c.rx_buf[2], c.rx_buf[3]]) as usize;
                            if c.rx_buf.len() < 4 + n * 4 {
                                break;
                            }
                            c.rx_buf.drain(..4 + n * 4);
                        }
                        // FramebufferUpdateRequest (10 bytes)
                        3 => {
                            if c.rx_buf.len() < 10 {
                                break;
                            }
                            let incremental = c.rx_buf[1] != 0;
                            c.rx_buf.drain(..10);
                            let (w, h, rgba, gen_num) = self.fb.snapshot_rgba();
                            if !incremental || gen_num != c.last_gen || c.last_gen == 0 {
                                c.last_gen = gen_num.max(1);
                                let mut pkt =
                                    Vec::with_capacity(16 + (w as usize) * (h as usize) * 4);
                                // FramebufferUpdate (type=0, pad=0, num_rects=1)
                                pkt.extend_from_slice(&[0, 0, 0, 1]);
                                // Rect header: x=0, y=0, w, h, encoding=0 (Raw)
                                pkt.extend_from_slice(&0u16.to_be_bytes());
                                pkt.extend_from_slice(&0u16.to_be_bytes());
                                pkt.extend_from_slice(&(w as u16).to_be_bytes());
                                pkt.extend_from_slice(&(h as u16).to_be_bytes());
                                pkt.extend_from_slice(&0i32.to_be_bytes());
                                for px in rgba.chunks_exact(4) {
                                    if c.bgrx {
                                        pkt.extend_from_slice(&[px[2], px[1], px[0], 0xff]);
                                    } else {
                                        pkt.extend_from_slice(&[px[0], px[1], px[2], 0xff]);
                                    }
                                }
                                c.stream.write_all(&pkt)?;
                                c.stream.flush()?;
                            }
                        }
                        // KeyEvent (8 bytes)
                        4 => {
                            if c.rx_buf.len() < 8 {
                                break;
                            }
                            let down = c.rx_buf[1] != 0;
                            let keysym = u32::from_be_bytes(c.rx_buf[4..8].try_into().unwrap());
                            c.rx_buf.drain(..8);
                            if let Some(code) = keysym_to_linux_code(keysym) {
                                self.input.send_key(code, down);
                            }
                        }
                        // PointerEvent (6 bytes)
                        5 => {
                            if c.rx_buf.len() < 6 {
                                break;
                            }
                            let buttons = c.rx_buf[1];
                            let x = u16::from_be_bytes([c.rx_buf[2], c.rx_buf[3]]);
                            let y = u16::from_be_bytes([c.rx_buf[4], c.rx_buf[5]]);
                            c.rx_buf.drain(..6);
                            let (w, h, _) = self.fb.dimensions();
                            let x_norm = (u32::from(x) * 32767) / w.max(1);
                            let y_norm = (u32::from(y) * 32767) / h.max(1);
                            self.input.send_pointer_abs(x_norm, y_norm, buttons);
                        }
                        // ClientCutText (8 + len bytes)
                        6 => {
                            if c.rx_buf.len() < 8 {
                                break;
                            }
                            let len =
                                u32::from_be_bytes(c.rx_buf[4..8].try_into().unwrap()) as usize;
                            if c.rx_buf.len() < 8 + len {
                                break;
                            }
                            c.rx_buf.drain(..8 + len);
                        }
                        _ => {
                            c.rx_buf.clear();
                            break;
                        }
                    }
                }
            }
        }
        if eof {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        Ok(())
    }
}

fn parse_vnc_addr(spec: &str) -> String {
    if let Some(disp) = spec.strip_prefix(':') {
        let n = disp.parse::<u16>().unwrap_or(0);
        let port = if n < 100 { 5900 + n } else { n };
        return format!("127.0.0.1:{port}");
    }
    if let Some((host, right)) = spec.rsplit_once(':') {
        let host = if host.is_empty() { "127.0.0.1" } else { host };
        let n = right.parse::<u16>().unwrap_or(0);
        let port = if n < 100 { 5900 + n } else { n };
        return format!("{host}:{port}");
    }
    format!("127.0.0.1:{}", 5900 + spec.parse::<u16>().unwrap_or(0))
}
