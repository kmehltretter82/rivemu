//! Host backends for `rivemu` devices:
//!
//! * [`block`]: raw disk images, in-memory disks and QCOW2 v2/v3 read/write images.
//! * [`net`]: pure-Rust user-mode network stack (ARP, DHCPv4, ICMP echo, DNS,
//!   UDP/TCP gateway and `hostfwd`).
//! * [`chardev`]: character device backends (`null`, `file`, `socket`, `buffer`).

#![forbid(unsafe_code)]

pub mod block;
pub mod chardev;
pub mod net;

pub use block::{BlockBackend, MemoryBlock, Qcow2Block, RawBlock, SECTOR_SIZE};
pub use chardev::{BufferChardev, Chardev, FileChardev, NullChardev, SocketChardev};
pub use net::{HostFwdRule, NetBackend, Proto, UserNet, UserNetConfig};
