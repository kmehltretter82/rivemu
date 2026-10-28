//! User-mode network backend (`-netdev user,id=...,hostfwd=...`).
//!
//! Implements a self-contained virtual router/NAT gateway matching QEMU's
//! SLIRP defaults (`10.0.2.0/24`, gateway `10.0.2.2`, DNS `10.0.2.3`, guest
//! `10.0.2.15`), including ARP, DHCPv4, ICMP echo, UDP forwarding and TCP
//! stream proxying with `hostfwd`.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};

/// Default MAC address assigned to a `virtio-net` NIC (`52:54:00:12:34:56`).
pub const DEFAULT_GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
/// Virtual gateway MAC address (`52:55:0a:00:02:02`).
pub const GATEWAY_MAC: [u8; 6] = [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02];
/// Virtual gateway IPv4 address (`10.0.2.2`).
pub const GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
/// Virtual DNS server IPv4 address (`10.0.2.3`).
pub const DNS_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);
/// Default guest IPv4 address (`10.0.2.15`).
pub const GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
/// Default subnet mask (`255.255.255.0`).
pub const NETMASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);

const ETH_P_IP: u16 = 0x0800;
const ETH_P_ARP: u16 = 0x0806;

const IPPROTO_ICMP: u8 = 1;
const IPPROTO_TCP: u8 = 6;
const IPPROTO_UDP: u8 = 17;

const TCP_FIN: u8 = 0x01;
const TCP_SYN: u8 = 0x02;
const TCP_RST: u8 = 0x04;
const TCP_PSH: u8 = 0x08;
const TCP_ACK: u8 = 0x10;

/// Transport protocol for `hostfwd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    /// TCP port forwarding.
    Tcp,
    /// UDP port forwarding.
    Udp,
}

/// A parsed `hostfwd=[tcp|udp]:[hostaddr]:hostport-[guestaddr]:guestport` rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFwdRule {
    /// Protocol (`tcp` or `udp`).
    pub proto: Proto,
    /// Host bind IP (defaults to `127.0.0.1`).
    pub host_addr: Ipv4Addr,
    /// Host port.
    pub host_port: u16,
    /// Guest destination IP (defaults to `10.0.2.15`).
    pub guest_addr: Ipv4Addr,
    /// Guest destination port.
    pub guest_port: u16,
}

impl HostFwdRule {
    /// Parses a QEMU `hostfwd=` specification.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (proto_s, rest) = spec
            .split_once(':')
            .ok_or_else(|| format!("invalid hostfwd '{spec}': missing protocol"))?;
        let proto = match proto_s {
            "tcp" | "" => Proto::Tcp,
            "udp" => Proto::Udp,
            _ => return Err(format!("invalid hostfwd protocol '{proto_s}'")),
        };
        let (host_part, guest_part) = rest
            .split_once('-')
            .ok_or_else(|| format!("invalid hostfwd '{spec}': missing '-' separator"))?;
        let (host_ip_s, host_port_s) = host_part
            .rsplit_once(':')
            .ok_or_else(|| format!("invalid hostfwd host part '{host_part}'"))?;
        let (guest_ip_s, guest_port_s) = guest_part
            .rsplit_once(':')
            .ok_or_else(|| format!("invalid hostfwd guest part '{guest_part}'"))?;
        let host_addr = if host_ip_s.is_empty() {
            Ipv4Addr::LOCALHOST
        } else {
            host_ip_s
                .parse()
                .map_err(|_| format!("invalid host IP '{host_ip_s}'"))?
        };
        let guest_addr = if guest_ip_s.is_empty() {
            GUEST_IP
        } else {
            guest_ip_s
                .parse()
                .map_err(|_| format!("invalid guest IP '{guest_ip_s}'"))?
        };
        let host_port = host_port_s
            .parse()
            .map_err(|_| format!("invalid host port '{host_port_s}'"))?;
        let guest_port = guest_port_s
            .parse()
            .map_err(|_| format!("invalid guest port '{guest_port_s}'"))?;
        Ok(Self {
            proto,
            host_addr,
            host_port,
            guest_addr,
            guest_port,
        })
    }
}

/// Configuration for [`UserNet`].
#[derive(Debug, Clone)]
pub struct UserNetConfig {
    /// Guest NIC MAC address.
    pub mac: [u8; 6],
    /// Port-forwarding rules.
    pub hostfwd: Vec<HostFwdRule>,
}

impl Default for UserNetConfig {
    fn default() -> Self {
        Self {
            mac: DEFAULT_GUEST_MAC,
            hostfwd: Vec::new(),
        }
    }
}

/// Host network backend for a `virtio-net` device.
pub trait NetBackend: Send + Sync + fmt::Debug {
    /// Hardware MAC address advertised to the guest NIC.
    fn mac(&self) -> [u8; 6];

    /// Transmits a raw Ethernet frame from the guest into the backend.
    fn send_packet(&self, frame: &[u8]);

    /// Pops the next raw Ethernet frame destined for the guest, if any.
    fn recv_packet(&self) -> Option<Vec<u8>>;

    /// Polls host sockets for incoming packets/connections.
    fn poll(&self);
}

/// User-mode SLIRP-like network backend.
#[derive(Debug)]
pub struct UserNet {
    mac: [u8; 6],
    inner: Mutex<UserNetInner>,
}

#[derive(Debug)]
struct UserNetInner {
    rx_queue: VecDeque<Vec<u8>>,
    guest_mac: [u8; 6],
    guest_ip: Ipv4Addr,
    ip_id: u16,
    udp_nat: HashMap<(u16, SocketAddrV4), UdpSocket>,
    tcp_conns: HashMap<(Ipv4Addr, u16, Ipv4Addr, u16), TcpFlow>,
    tcp_listeners: Vec<(HostFwdRule, TcpListener)>,
    next_ephemeral_port: u16,
}

#[derive(Debug)]
struct TcpFlow {
    stream: TcpStream,
    /// Sequence number we send to the guest.
    seq: u32,
    /// Next sequence number we expect from the guest.
    ack: u32,
    connected: bool,
    fin_sent: bool,
}

impl UserNet {
    /// Creates a new user-mode network stack.
    pub fn new(cfg: UserNetConfig) -> Arc<Self> {
        let mut tcp_listeners = Vec::new();
        for rule in &cfg.hostfwd {
            if rule.proto == Proto::Tcp {
                if let Ok(l) = TcpListener::bind(SocketAddrV4::new(rule.host_addr, rule.host_port))
                {
                    let _ = l.set_nonblocking(true);
                    tcp_listeners.push((rule.clone(), l));
                }
            }
        }
        Arc::new(Self {
            mac: cfg.mac,
            inner: Mutex::new(UserNetInner {
                rx_queue: VecDeque::new(),
                guest_mac: cfg.mac,
                guest_ip: GUEST_IP,
                ip_id: 1,
                udp_nat: HashMap::new(),
                tcp_conns: HashMap::new(),
                tcp_listeners,
                next_ephemeral_port: 49152,
            }),
        })
    }
}

impl NetBackend for UserNet {
    fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn send_packet(&self, frame: &[u8]) {
        if frame.len() < 14 {
            return;
        }
        let mut st = self.inner.lock().unwrap();
        st.guest_mac.copy_from_slice(&frame[6..12]);
        let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
        let payload = &frame[14..];
        match ethertype {
            ETH_P_ARP => st.handle_arp(payload),
            ETH_P_IP => st.handle_ipv4(payload),
            _ => {}
        }
    }

    fn recv_packet(&self) -> Option<Vec<u8>> {
        self.inner.lock().unwrap().rx_queue.pop_front()
    }

    fn poll(&self) {
        self.inner.lock().unwrap().poll_sockets();
    }
}

impl UserNetInner {
    fn handle_arp(&mut self, arp: &[u8]) {
        // Ethernet IPv4 ARP packet is 28 bytes.
        if arp.len() < 28 {
            return;
        }
        let htype = u16::from_be_bytes([arp[0], arp[1]]);
        let ptype = u16::from_be_bytes([arp[2], arp[3]]);
        let oper = u16::from_be_bytes([arp[6], arp[7]]);
        if htype != 1 || ptype != ETH_P_IP || oper != 1 {
            return;
        }
        let sender_mac: [u8; 6] = arp[8..14].try_into().unwrap();
        let sender_ip = Ipv4Addr::new(arp[14], arp[15], arp[16], arp[17]);
        let target_ip = Ipv4Addr::new(arp[24], arp[25], arp[26], arp[27]);
        if !sender_ip.is_unspecified() {
            self.guest_ip = sender_ip;
        }
        self.guest_mac = sender_mac;
        if target_ip == self.guest_ip {
            return;
        }

        let mut reply = Vec::with_capacity(42);
        reply.extend_from_slice(&sender_mac);
        reply.extend_from_slice(&GATEWAY_MAC);
        reply.extend_from_slice(&ETH_P_ARP.to_be_bytes());
        reply.extend_from_slice(&1u16.to_be_bytes()); // HTYPE Ethernet
        reply.extend_from_slice(&ETH_P_IP.to_be_bytes()); // PTYPE IPv4
        reply.push(6); // HLEN
        reply.push(4); // PLEN
        reply.extend_from_slice(&2u16.to_be_bytes()); // OPER Reply
        reply.extend_from_slice(&GATEWAY_MAC);
        reply.extend_from_slice(&target_ip.octets());
        reply.extend_from_slice(&sender_mac);
        reply.extend_from_slice(&sender_ip.octets());
        self.rx_queue.push_back(reply);
    }

    fn handle_ipv4(&mut self, ip: &[u8]) {
        if ip.len() < 20 || (ip[0] >> 4) != 4 {
            return;
        }
        let ihl = ((ip[0] & 0x0f) as usize) * 4;
        let total_len = u16::from_be_bytes([ip[2], ip[3]]) as usize;
        if ihl < 20 || ip.len() < total_len || total_len < ihl {
            return;
        }
        let proto = ip[9];
        let src_ip = Ipv4Addr::new(ip[12], ip[13], ip[14], ip[15]);
        let dst_ip = Ipv4Addr::new(ip[16], ip[17], ip[18], ip[19]);
        if !src_ip.is_unspecified() {
            self.guest_ip = src_ip;
        }
        let body = &ip[ihl..total_len];
        match proto {
            IPPROTO_ICMP => self.handle_icmp(src_ip, dst_ip, body),
            IPPROTO_UDP => self.handle_udp(src_ip, dst_ip, body),
            IPPROTO_TCP => self.handle_tcp(src_ip, dst_ip, body),
            _ => {}
        }
    }

    fn handle_icmp(&mut self, src_ip: Ipv4Addr, dst_ip: Ipv4Addr, icmp: &[u8]) {
        // ICMP Echo Request (type 8, code 0).
        if icmp.len() < 8 || icmp[0] != 8 || icmp[1] != 0 {
            return;
        }
        let mut reply = icmp.to_vec();
        reply[0] = 0; // Echo Reply
        reply[2] = 0;
        reply[3] = 0;
        let csum = internet_checksum(&reply);
        reply[2..4].copy_from_slice(&csum.to_be_bytes());
        self.send_ipv4(dst_ip, src_ip, IPPROTO_ICMP, &reply);
    }

    fn handle_udp(&mut self, src_ip: Ipv4Addr, dst_ip: Ipv4Addr, udp: &[u8]) {
        if udp.len() < 8 {
            return;
        }
        let src_port = u16::from_be_bytes([udp[0], udp[1]]);
        let dst_port = u16::from_be_bytes([udp[2], udp[3]]);
        let ulen = u16::from_be_bytes([udp[4], udp[5]]) as usize;
        if udp.len() < ulen || ulen < 8 {
            return;
        }
        let payload = &udp[8..ulen];

        // DHCPv4 request (client 68 -> server 67).
        if src_port == 68 && dst_port == 67 {
            self.handle_dhcp(payload);
            return;
        }

        // Local DNS query to 10.0.2.3:53 for "gateway" / "localhost".
        if (dst_ip == DNS_IP || dst_ip == GATEWAY_IP) && dst_port == 53 {
            if let Some(ans) = try_local_dns(payload) {
                self.send_udp(dst_ip, 53, src_ip, src_port, &ans);
                return;
            }
        }

        // Forward external UDP datagrams when dst is outside 10.0.2.0/24.
        let target_ip = if dst_ip == GATEWAY_IP {
            Ipv4Addr::LOCALHOST
        } else {
            dst_ip
        };
        let dst_sock = SocketAddrV4::new(target_ip, dst_port);
        let key = (src_port, dst_sock);
        let sock = match self.udp_nat.get(&key) {
            Some(s) => s,
            None => {
                let Ok(s) = UdpSocket::bind("0.0.0.0:0") else {
                    return;
                };
                let _ = s.set_nonblocking(true);
                self.udp_nat.insert(key, s);
                self.udp_nat.get(&key).unwrap()
            }
        };
        let _ = sock.send_to(payload, dst_sock);
    }

    fn handle_dhcp(&mut self, pkt: &[u8]) {
        // BOOTP header is 236 bytes + 4-byte magic cookie 99.130.83.99.
        if pkt.len() < 240 || pkt[0] != 1 || pkt[236..240] != [99, 130, 83, 99] {
            return;
        }
        let xid = &pkt[4..8];
        let chaddr = &pkt[28..44];
        let mut msg_type = 0u8;
        let mut i = 240;
        while i < pkt.len() {
            let opt = pkt[i];
            if opt == 255 {
                break;
            }
            if opt == 0 {
                i += 1;
                continue;
            }
            if i + 1 >= pkt.len() {
                break;
            }
            let len = pkt[i + 1] as usize;
            if i + 2 + len > pkt.len() {
                break;
            }
            if opt == 53 && len >= 1 {
                msg_type = pkt[i + 2];
            }
            i += 2 + len;
        }
        let reply_type = match msg_type {
            1 => 2u8, // DISCOVER -> OFFER
            3 => 5u8, // REQUEST  -> ACK
            _ => return,
        };

        let mut resp = vec![0u8; 240];
        resp[0] = 2; // BOOTREPLY
        resp[1] = 1; // Ethernet
        resp[2] = 6; // HLEN
        resp[4..8].copy_from_slice(xid);
        resp[16..20].copy_from_slice(&GUEST_IP.octets()); // yiaddr
        resp[20..24].copy_from_slice(&GATEWAY_IP.octets()); // siaddr
        resp[28..44].copy_from_slice(chaddr);
        resp[236..240].copy_from_slice(&[99, 130, 83, 99]);
        // DHCP options:
        resp.extend_from_slice(&[53, 1, reply_type]); // Message Type
        resp.extend_from_slice(&[54, 4]); // Server Identifier
        resp.extend_from_slice(&GATEWAY_IP.octets());
        resp.extend_from_slice(&[51, 4, 0, 1, 0x51, 0x80]); // Lease Time = 86400s
        resp.extend_from_slice(&[1, 4]); // Subnet Mask
        resp.extend_from_slice(&NETMASK.octets());
        resp.extend_from_slice(&[3, 4]); // Router
        resp.extend_from_slice(&GATEWAY_IP.octets());
        resp.extend_from_slice(&[6, 4]); // DNS
        resp.extend_from_slice(&DNS_IP.octets());
        resp.push(255); // End

        self.send_udp(GATEWAY_IP, 67, Ipv4Addr::BROADCAST, 68, &resp);
    }

    fn handle_tcp(&mut self, src_ip: Ipv4Addr, dst_ip: Ipv4Addr, tcp: &[u8]) {
        if tcp.len() < 20 {
            return;
        }
        let src_port = u16::from_be_bytes([tcp[0], tcp[1]]);
        let dst_port = u16::from_be_bytes([tcp[2], tcp[3]]);
        let seq = u32::from_be_bytes(tcp[4..8].try_into().unwrap());
        let data_off = ((tcp[12] >> 4) as usize) * 4;
        let flags = tcp[13];
        if data_off < 20 || tcp.len() < data_off {
            return;
        }
        let payload = &tcp[data_off..];
        let key = (src_ip, src_port, dst_ip, dst_port);

        if flags & TCP_RST != 0 {
            self.tcp_conns.remove(&key);
            return;
        }

        if flags & TCP_SYN != 0 && flags & TCP_ACK == 0 {
            let host_target = if dst_ip == GATEWAY_IP {
                Ipv4Addr::LOCALHOST
            } else {
                dst_ip
            };
            let Ok(stream) = TcpStream::connect(SocketAddrV4::new(host_target, dst_port)) else {
                self.send_tcp_segment(
                    dst_ip,
                    dst_port,
                    src_ip,
                    src_port,
                    0,
                    seq.wrapping_add(1),
                    TCP_RST | TCP_ACK,
                    &[],
                );
                return;
            };
            let _ = stream.set_nonblocking(true);
            let _ = stream.set_nodelay(true);
            let iss = 0x1000_0000u32;
            let ack = seq.wrapping_add(1);
            self.tcp_conns.insert(
                key,
                TcpFlow {
                    stream,
                    seq: iss.wrapping_add(1),
                    ack,
                    connected: true,
                    fin_sent: false,
                },
            );
            self.send_tcp_segment(
                dst_ip,
                dst_port,
                src_ip,
                src_port,
                iss,
                ack,
                TCP_SYN | TCP_ACK,
                &[],
            );
            return;
        }

        let Some(mut flow) = self.tcp_conns.remove(&key) else {
            return;
        };
        if flags & TCP_SYN != 0 && flags & TCP_ACK != 0 {
            flow.connected = true;
            flow.ack = seq.wrapping_add(1);
            self.send_tcp_segment(
                dst_ip,
                dst_port,
                src_ip,
                src_port,
                flow.seq,
                flow.ack,
                TCP_ACK,
                &[],
            );
            self.tcp_conns.insert(key, flow);
            return;
        }
        if !payload.is_empty() && seq == flow.ack {
            let _ = flow.stream.write_all(payload);
            flow.ack = flow.ack.wrapping_add(payload.len() as u32);
            self.send_tcp_segment(
                dst_ip,
                dst_port,
                src_ip,
                src_port,
                flow.seq,
                flow.ack,
                TCP_ACK,
                &[],
            );
        }
        if flags & TCP_FIN != 0 {
            flow.ack = flow.ack.wrapping_add(1);
            self.send_tcp_segment(
                dst_ip,
                dst_port,
                src_ip,
                src_port,
                flow.seq,
                flow.ack,
                TCP_FIN | TCP_ACK,
                &[],
            );
            return;
        }
        self.tcp_conns.insert(key, flow);
    }

    fn poll_sockets(&mut self) {
        // 1. Poll UDP NAT sockets.
        let mut udp_replies = Vec::new();
        let guest_ip = self.guest_ip;
        for (&(guest_port, peer), sock) in &self.udp_nat {
            let mut buf = [0u8; 2048];
            if let Ok((n, _)) = sock.recv_from(&mut buf) {
                udp_replies.push((
                    *peer.ip(),
                    peer.port(),
                    guest_ip,
                    guest_port,
                    buf[..n].to_vec(),
                ));
            }
        }
        for (src_ip, src_port, dst_ip, dst_port, data) in udp_replies {
            let from_ip = if src_ip.is_loopback() {
                GATEWAY_IP
            } else {
                src_ip
            };
            self.send_udp(from_ip, src_port, dst_ip, dst_port, &data);
        }

        // 2. Poll hostfwd TCP listeners.
        let mut new_incoming = Vec::new();
        for (rule, listener) in &self.tcp_listeners {
            if let Ok((stream, _)) = listener.accept() {
                let _ = stream.set_nonblocking(true);
                let _ = stream.set_nodelay(true);
                let src_port = self.next_ephemeral_port;
                self.next_ephemeral_port = self.next_ephemeral_port.wrapping_add(1).max(49152);
                new_incoming.push((rule.guest_addr, rule.guest_port, src_port, stream));
            }
        }
        for (guest_ip, guest_port, host_port, stream) in new_incoming {
            let iss = 0x2000_0000u32;
            let key = (guest_ip, guest_port, GATEWAY_IP, host_port);
            self.tcp_conns.insert(
                key,
                TcpFlow {
                    stream,
                    seq: iss.wrapping_add(1),
                    ack: 0,
                    connected: false,
                    fin_sent: false,
                },
            );
            self.send_tcp_segment(
                GATEWAY_IP,
                host_port,
                guest_ip,
                guest_port,
                iss,
                0,
                TCP_SYN,
                &[],
            );
        }

        // 3. Poll active TCP streams.
        let keys: Vec<_> = self.tcp_conns.keys().copied().collect();
        for key in keys {
            let (g_ip, g_port, r_ip, r_port) = key;
            let Some(mut flow) = self.tcp_conns.remove(&key) else {
                continue;
            };
            if flow.connected && !flow.fin_sent {
                let mut buf = [0u8; 1400];
                match flow.stream.read(&mut buf) {
                    Ok(0) => {
                        flow.fin_sent = true;
                        let seq = flow.seq;
                        flow.seq = flow.seq.wrapping_add(1);
                        self.send_tcp_segment(
                            r_ip,
                            r_port,
                            g_ip,
                            g_port,
                            seq,
                            flow.ack,
                            TCP_FIN | TCP_ACK,
                            &[],
                        );
                    }
                    Ok(n) => {
                        let seq = flow.seq;
                        flow.seq = flow.seq.wrapping_add(n as u32);
                        self.send_tcp_segment(
                            r_ip,
                            r_port,
                            g_ip,
                            g_port,
                            seq,
                            flow.ack,
                            TCP_PSH | TCP_ACK,
                            &buf[..n],
                        );
                    }
                    Err(_) => {}
                }
            }
            self.tcp_conns.insert(key, flow);
        }
    }

    fn send_udp(
        &mut self,
        src_ip: Ipv4Addr,
        src_port: u16,
        dst_ip: Ipv4Addr,
        dst_port: u16,
        payload: &[u8],
    ) {
        let ulen = (8 + payload.len()) as u16;
        let mut udp = Vec::with_capacity(ulen as usize);
        udp.extend_from_slice(&src_port.to_be_bytes());
        udp.extend_from_slice(&dst_port.to_be_bytes());
        udp.extend_from_slice(&ulen.to_be_bytes());
        udp.extend_from_slice(&0u16.to_be_bytes()); // UDP checksum optional in IPv4
        udp.extend_from_slice(payload);
        self.send_ipv4(src_ip, dst_ip, IPPROTO_UDP, &udp);
    }

    #[allow(clippy::too_many_arguments)]
    fn send_tcp_segment(
        &mut self,
        src_ip: Ipv4Addr,
        src_port: u16,
        dst_ip: Ipv4Addr,
        dst_port: u16,
        seq: u32,
        ack: u32,
        flags: u8,
        payload: &[u8],
    ) {
        let mut seg = Vec::with_capacity(20 + payload.len());
        seg.extend_from_slice(&src_port.to_be_bytes());
        seg.extend_from_slice(&dst_port.to_be_bytes());
        seg.extend_from_slice(&seq.to_be_bytes());
        seg.extend_from_slice(&ack.to_be_bytes());
        seg.push(5 << 4); // Data offset = 5 (20 bytes)
        seg.push(flags);
        seg.extend_from_slice(&65535u16.to_be_bytes()); // Window
        seg.extend_from_slice(&0u16.to_be_bytes()); // Checksum placeholder
        seg.extend_from_slice(&0u16.to_be_bytes()); // Urgent pointer
        seg.extend_from_slice(payload);

        let csum = tcp_udp_checksum(src_ip, dst_ip, IPPROTO_TCP, &seg);
        seg[16..18].copy_from_slice(&csum.to_be_bytes());
        self.send_ipv4(src_ip, dst_ip, IPPROTO_TCP, &seg);
    }

    fn send_ipv4(&mut self, src_ip: Ipv4Addr, dst_ip: Ipv4Addr, proto: u8, body: &[u8]) {
        let total_len = (20 + body.len()) as u16;
        let id = self.ip_id;
        self.ip_id = self.ip_id.wrapping_add(1);

        let mut ip = [0u8; 20];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&total_len.to_be_bytes());
        ip[4..6].copy_from_slice(&id.to_be_bytes());
        ip[6..8].copy_from_slice(&0x4000u16.to_be_bytes()); // DF
        ip[8] = 64; // TTL
        ip[9] = proto;
        ip[12..16].copy_from_slice(&src_ip.octets());
        ip[16..20].copy_from_slice(&dst_ip.octets());
        let csum = internet_checksum(&ip);
        ip[10..12].copy_from_slice(&csum.to_be_bytes());

        let dst_mac = if dst_ip.is_broadcast() {
            [0xff; 6]
        } else {
            self.guest_mac
        };
        let mut frame = Vec::with_capacity(14 + 20 + body.len());
        frame.extend_from_slice(&dst_mac);
        frame.extend_from_slice(&GATEWAY_MAC);
        frame.extend_from_slice(&ETH_P_IP.to_be_bytes());
        frame.extend_from_slice(&ip);
        frame.extend_from_slice(body);
        self.rx_queue.push_back(frame);
    }
}

fn try_local_dns(query: &[u8]) -> Option<Vec<u8>> {
    // Minimal DNS A-record synthesiser for single-question queries.
    if query.len() < 12 {
        return None;
    }
    let qdcount = u16::from_be_bytes([query[4], query[5]]);
    if qdcount != 1 {
        return None;
    }
    let mut pos = 12;
    let mut labels = Vec::new();
    while pos < query.len() {
        let len = query[pos] as usize;
        pos += 1;
        if len == 0 {
            break;
        }
        if pos + len > query.len() {
            return None;
        }
        labels.push(String::from_utf8_lossy(&query[pos..pos + len]).to_ascii_lowercase());
        pos += len;
    }
    if pos + 4 > query.len() {
        return None;
    }
    let qtype = u16::from_be_bytes([query[pos], query[pos + 1]]);
    let qclass = u16::from_be_bytes([query[pos + 2], query[pos + 3]]);
    let qend = pos + 4;
    if qclass != 1 {
        return None;
    }
    let name = labels.join(".");
    let ip = match name.as_str() {
        "gateway" | "host.containers.internal" => GATEWAY_IP,
        "localhost" => Ipv4Addr::LOCALHOST,
        _ => return None,
    };
    let mut resp = Vec::with_capacity(qend + 16);
    resp.extend_from_slice(&query[0..2]); // ID
    resp.extend_from_slice(&0x8180u16.to_be_bytes()); // Standard response, no error
    resp.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT = 1
    resp.extend_from_slice(&u16::from(qtype == 1).to_be_bytes()); // ANCOUNT
    resp.extend_from_slice(&0u32.to_be_bytes()); // NSCOUNT, ARCOUNT
    resp.extend_from_slice(&query[12..qend]);
    if qtype == 1 {
        resp.extend_from_slice(&0xc00cu16.to_be_bytes()); // Name pointer to offset 12
        resp.extend_from_slice(&1u16.to_be_bytes()); // TYPE A
        resp.extend_from_slice(&1u16.to_be_bytes()); // CLASS IN
        resp.extend_from_slice(&60u32.to_be_bytes()); // TTL 60s
        resp.extend_from_slice(&4u16.to_be_bytes()); // RDLENGTH 4
        resp.extend_from_slice(&ip.octets());
    }
    Some(resp)
}

fn internet_checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut chunks = data.chunks_exact(2);
    for c in &mut chunks {
        sum += u32::from(u16::from_be_bytes([c[0], c[1]]));
    }
    if let Some(&rem) = chunks.remainder().first() {
        sum += u32::from(u16::from_be_bytes([rem, 0]));
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn tcp_udp_checksum(src: Ipv4Addr, dst: Ipv4Addr, proto: u8, segment: &[u8]) -> u16 {
    let mut pseudo = Vec::with_capacity(12 + segment.len());
    pseudo.extend_from_slice(&src.octets());
    pseudo.extend_from_slice(&dst.octets());
    pseudo.push(0);
    pseudo.push(proto);
    pseudo.extend_from_slice(&(segment.len() as u16).to_be_bytes());
    pseudo.extend_from_slice(segment);
    internet_checksum(&pseudo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostfwd_parsing() {
        let r = HostFwdRule::parse("tcp::2222-:22").unwrap();
        assert_eq!(
            r,
            HostFwdRule {
                proto: Proto::Tcp,
                host_addr: Ipv4Addr::LOCALHOST,
                host_port: 2222,
                guest_addr: GUEST_IP,
                guest_port: 22,
            }
        );
        let r2 = HostFwdRule::parse("udp:127.0.0.1:5353-10.0.2.15:53").unwrap();
        assert_eq!(r2.proto, Proto::Udp);
        assert_eq!(r2.host_port, 5353);
        assert_eq!(r2.guest_port, 53);
    }

    #[test]
    fn arp_and_icmp_echo() {
        let net = UserNet::new(UserNetConfig::default());
        // 1. Send ARP request for 10.0.2.2.
        let mut arp_req = Vec::new();
        arp_req.extend_from_slice(&[0xff; 6]);
        arp_req.extend_from_slice(&DEFAULT_GUEST_MAC);
        arp_req.extend_from_slice(&ETH_P_ARP.to_be_bytes());
        arp_req.extend_from_slice(&1u16.to_be_bytes()); // HTYPE
        arp_req.extend_from_slice(&ETH_P_IP.to_be_bytes()); // PTYPE
        arp_req.extend_from_slice(&[6, 4, 0, 1]); // HLEN, PLEN, OPER=1
        arp_req.extend_from_slice(&DEFAULT_GUEST_MAC);
        arp_req.extend_from_slice(&GUEST_IP.octets());
        arp_req.extend_from_slice(&[0; 6]);
        arp_req.extend_from_slice(&GATEWAY_IP.octets());
        net.send_packet(&arp_req);

        let arp_rep = net.recv_packet().expect("ARP reply");
        assert_eq!(&arp_rep[12..14], &ETH_P_ARP.to_be_bytes());
        assert_eq!(&arp_rep[20..22], &2u16.to_be_bytes()); // OPER=2 Reply
        assert_eq!(&arp_rep[22..28], &GATEWAY_MAC);

        // 2. Send ICMP Echo Request to 10.0.2.2.
        let mut icmp = [8u8, 0, 0, 0, 0x12, 0x34, 0x00, 0x01, b'p', b'i', b'n', b'g'];
        let csum = internet_checksum(&icmp);
        icmp[2..4].copy_from_slice(&csum.to_be_bytes());

        let mut ip = [0u8; 20];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&((20 + icmp.len()) as u16).to_be_bytes());
        ip[8] = 64;
        ip[9] = IPPROTO_ICMP;
        ip[12..16].copy_from_slice(&GUEST_IP.octets());
        ip[16..20].copy_from_slice(&GATEWAY_IP.octets());
        let ip_csum = internet_checksum(&ip);
        ip[10..12].copy_from_slice(&ip_csum.to_be_bytes());

        let mut frame = Vec::new();
        frame.extend_from_slice(&GATEWAY_MAC);
        frame.extend_from_slice(&DEFAULT_GUEST_MAC);
        frame.extend_from_slice(&ETH_P_IP.to_be_bytes());
        frame.extend_from_slice(&ip);
        frame.extend_from_slice(&icmp);
        net.send_packet(&frame);

        let reply = net.recv_packet().expect("ICMP Echo Reply");
        assert_eq!(reply[14 + 20], 0); // Echo Reply type 0
        assert_eq!(&reply[14 + 20 + 8..], b"ping");
    }

    #[test]
    fn dhcp_discover_offer() {
        let net = UserNet::new(UserNetConfig::default());
        let mut bootp = vec![0u8; 240];
        bootp[0] = 1; // BOOTREQUEST
        bootp[1] = 1;
        bootp[2] = 6;
        bootp[4..8].copy_from_slice(&0xdead_beefu32.to_be_bytes());
        bootp[28..34].copy_from_slice(&DEFAULT_GUEST_MAC);
        bootp[236..240].copy_from_slice(&[99, 130, 83, 99]);
        bootp.extend_from_slice(&[53, 1, 1, 255]); // DHCPDISCOVER

        let ulen = (8 + bootp.len()) as u16;
        let mut udp = Vec::new();
        udp.extend_from_slice(&68u16.to_be_bytes());
        udp.extend_from_slice(&67u16.to_be_bytes());
        udp.extend_from_slice(&ulen.to_be_bytes());
        udp.extend_from_slice(&0u16.to_be_bytes());
        udp.extend_from_slice(&bootp);

        let mut ip = [0u8; 20];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&(20 + ulen).to_be_bytes());
        ip[8] = 64;
        ip[9] = IPPROTO_UDP;
        ip[16..20].copy_from_slice(&[255, 255, 255, 255]);

        let mut frame = Vec::new();
        frame.extend_from_slice(&[0xff; 6]);
        frame.extend_from_slice(&DEFAULT_GUEST_MAC);
        frame.extend_from_slice(&ETH_P_IP.to_be_bytes());
        frame.extend_from_slice(&ip);
        frame.extend_from_slice(&udp);
        net.send_packet(&frame);

        let offer = net.recv_packet().expect("DHCP offer");
        let dhcp = &offer[14 + 20 + 8..];
        assert_eq!(dhcp[0], 2); // BOOTREPLY
        assert_eq!(&dhcp[16..20], &GUEST_IP.octets()); // yiaddr = 10.0.2.15
    }
}
