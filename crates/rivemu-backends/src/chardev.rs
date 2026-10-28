//! Character device backends (`null`, `file`, `socket`, `buffer`).

use std::collections::VecDeque;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Host character stream backend.
pub trait Chardev: Send + Sync + fmt::Debug {
    /// Drains any pending input bytes from the host.
    fn read_input(&self) -> Vec<u8>;

    /// Sends guest output bytes to the host.
    fn write_output(&self, data: &[u8]);
}

/// Discards output and never produces input (`-chardev null`).
#[derive(Debug, Default)]
pub struct NullChardev;

impl Chardev for NullChardev {
    fn read_input(&self) -> Vec<u8> {
        Vec::new()
    }

    fn write_output(&self, _data: &[u8]) {}
}

/// Writes guest output to a file (`-chardev file,path=...`).
#[derive(Debug)]
pub struct FileChardev {
    file: Mutex<File>,
}

impl FileChardev {
    /// Creates or truncates `path` for output.
    pub fn create(path: &Path) -> io::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            file: Mutex::new(File::create(path)?),
        }))
    }
}

impl Chardev for FileChardev {
    fn read_input(&self) -> Vec<u8> {
        Vec::new()
    }

    fn write_output(&self, data: &[u8]) {
        if let Ok(mut f) = self.file.lock() {
            let _ = f.write_all(data);
            let _ = f.flush();
        }
    }
}

/// In-memory bidirectional character device (for tests and programmatic I/O).
#[derive(Debug, Default)]
pub struct BufferChardev {
    rx: Mutex<VecDeque<u8>>,
    tx: Mutex<Vec<u8>>,
}

impl BufferChardev {
    /// Creates an empty buffered chardev.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Queues input bytes for the guest to read.
    pub fn push_input(&self, data: &[u8]) {
        self.rx.lock().unwrap().extend(data);
    }

    /// Takes all output bytes written by the guest so far.
    pub fn take_output(&self) -> Vec<u8> {
        std::mem::take(&mut *self.tx.lock().unwrap())
    }
}

impl Chardev for BufferChardev {
    fn read_input(&self) -> Vec<u8> {
        self.rx.lock().unwrap().drain(..).collect()
    }

    fn write_output(&self, data: &[u8]) {
        self.tx.lock().unwrap().extend_from_slice(data);
    }
}

/// Non-blocking TCP socket character backend (`-chardev socket,host=...,port=...`).
#[derive(Debug)]
pub struct SocketChardev {
    listener: Option<TcpListener>,
    stream: Mutex<Option<TcpStream>>,
}

impl SocketChardev {
    /// Binds a TCP server socket (`server,nowait` or waiting for a client).
    pub fn bind_tcp(addr: &str, wait: bool) -> io::Result<Arc<Self>> {
        let listener = TcpListener::bind(addr)?;
        let stream = if wait {
            let (s, _) = listener.accept()?;
            s.set_nonblocking(true)?;
            Some(s)
        } else {
            None
        };
        listener.set_nonblocking(true)?;
        Ok(Arc::new(Self {
            listener: Some(listener),
            stream: Mutex::new(stream),
        }))
    }

    /// Connects to a TCP peer.
    pub fn connect_tcp(addr: &str) -> io::Result<Arc<Self>> {
        let s = TcpStream::connect(addr)?;
        s.set_nonblocking(true)?;
        Ok(Arc::new(Self {
            listener: None,
            stream: Mutex::new(Some(s)),
        }))
    }

    fn poll_accept(&self, guard: &mut Option<TcpStream>) {
        if guard.is_none() {
            if let Some(l) = &self.listener {
                if let Ok((s, _)) = l.accept() {
                    let _ = s.set_nonblocking(true);
                    *guard = Some(s);
                }
            }
        }
    }
}

impl Chardev for SocketChardev {
    fn read_input(&self) -> Vec<u8> {
        let mut guard = self.stream.lock().unwrap();
        self.poll_accept(&mut guard);
        let Some(s) = guard.as_mut() else {
            return Vec::new();
        };
        let mut buf = [0u8; 1024];
        match s.read(&mut buf) {
            Ok(0) => {
                *guard = None;
                Vec::new()
            }
            Ok(n) => buf[..n].to_vec(),
            Err(_) => Vec::new(),
        }
    }

    fn write_output(&self, data: &[u8]) {
        let mut guard = self.stream.lock().unwrap();
        self.poll_accept(&mut guard);
        if let Some(s) = guard.as_mut() {
            if s.write_all(data).is_err() {
                *guard = None;
            }
        }
    }
}
