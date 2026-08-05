//! `EthtoolNic` — the real [`NicControl`] over SIOCETHTOOL ioctls (RSS
//! indirection, ntuple rules, channel counts) plus the ethtool GENETLINK
//! `tcp-data-split` probe (design §5: the corrected probe compares the
//! `ETHTOOL_A_RINGS_TCP_DATA_SPLIT` ATTRIBUTE, never a rendered string —
//! the 2026-08-02 probe-spelling lesson pinned in code).
//!
//! Field-owed surface: this module executes only on a zcrx-capable NIC
//! (the reformat-window bracket); the steering STATE MACHINE it plugs
//! into is contract-tested against the mock (`tests/zcrx_steering_tests`).
//! Every struct layout below mirrors `<linux/ethtool.h>` and was
//! byte-verified against a compiled C probe on kernel 7.1 headers
//! (sizes/offsets in the layout assertions at the bottom — a drifted
//! mirror fails `cargo test` before it can ever hit an ioctl).

use super::steering::{FlowRule, NicControl, RX_CLS_LOC_SPECIAL};
use std::net::{IpAddr, SocketAddr};
use std::os::fd::{AsRawFd, OwnedFd};

// ---------------------------------------------------------------- uapi mirror

const SIOCETHTOOL: libc::c_ulong = 0x8946;

const ETHTOOL_GRINGPARAM: u32 = 0x10;
const ETHTOOL_GFLAGS: u32 = 0x25;
const ETHTOOL_GCHANNELS: u32 = 0x3c;
const ETHTOOL_GRXCLSRLCNT: u32 = 0x2e;
const ETHTOOL_GRXCLSRLALL: u32 = 0x30;
const ETHTOOL_SRXCLSRLDEL: u32 = 0x31;
const ETHTOOL_SRXCLSRLINS: u32 = 0x32;
const ETHTOOL_GRSSH: u32 = 0x46;
const ETHTOOL_SRSSH: u32 = 0x47;

const TCP_V4_FLOW: u32 = 0x01;
const TCP_V6_FLOW: u32 = 0x05;

/// `ETH_FLAG_NTUPLE` (<linux/ethtool.h>): the legacy flags word's ntuple
/// bit. GFLAGS is the STABLE uapi face of the `rx-ntuple-filter` feature
/// (the kernel synthesizes it from netdev features), so the on/off probe
/// needs no ethtool-netlink string-set walk.
const ETH_FLAG_NTUPLE: u32 = 1 << 27;

/// `struct ethtool_value` — the GFLAGS/GRXCSUM-class 2-word command.
#[repr(C)]
#[derive(Default)]
struct EthtoolValue {
    cmd: u32,
    data: u32,
}

/// `struct ethtool_ringparam` (<linux/ethtool.h>).
#[repr(C)]
#[derive(Default)]
struct EthtoolRingparam {
    cmd: u32,
    rx_max_pending: u32,
    rx_mini_max_pending: u32,
    rx_jumbo_max_pending: u32,
    tx_max_pending: u32,
    rx_pending: u32,
    rx_mini_pending: u32,
    rx_jumbo_pending: u32,
    tx_pending: u32,
}

#[repr(C)]
#[derive(Default)]
struct EthtoolChannels {
    cmd: u32,
    max_rx: u32,
    max_tx: u32,
    max_other: u32,
    max_combined: u32,
    rx_count: u32,
    tx_count: u32,
    other_count: u32,
    combined_count: u32,
}

/// `struct ethtool_rxfh` fixed head (the variable indir/key tail follows).
#[repr(C)]
#[derive(Default)]
struct EthtoolRxfhHead {
    cmd: u32,
    rss_context: u32,
    indir_size: u32,
    key_size: u32,
    hfunc: u8,
    input_xfrm: u8,
    rsvd8: [u8; 2],
    rsvd32: u32,
}

/// `union ethtool_flow_union` (52 B) + `struct ethtool_flow_ext` (20 B),
/// mirrored as raw byte fields — we only fill the TCP v4/v6 specs.
#[repr(C)]
struct EthtoolRxFlowSpec {
    flow_type: u32,
    h_u: [u8; 52],
    h_ext: [u8; 20],
    m_u: [u8; 52],
    m_ext: [u8; 20],
    ring_cookie: u64,
    location: u32,
}

impl Default for EthtoolRxFlowSpec {
    fn default() -> Self {
        // SAFETY: all-zero is a valid value for every field (POD mirror).
        unsafe { std::mem::zeroed() }
    }
}

/// `struct ethtool_rxnfc` (192 B; `rule_locs[]` tail allocated by us).
#[repr(C)]
struct EthtoolRxnfc {
    cmd: u32,
    flow_type: u32,
    data: u64,
    fs: EthtoolRxFlowSpec,
    rule_cnt: u32,
    rule_locs: u32, // first element of the trailing array
}

impl Default for EthtoolRxnfc {
    fn default() -> Self {
        // SAFETY: POD mirror, all-zero valid.
        unsafe { std::mem::zeroed() }
    }
}

// Layout law: the mirrors must match the kernel ABI byte-for-byte
// (verified against a compiled C probe of <linux/ethtool.h>).
const _: () = {
    assert!(std::mem::size_of::<EthtoolValue>() == 8);
    assert!(std::mem::size_of::<EthtoolRingparam>() == 36);
    assert!(std::mem::size_of::<EthtoolChannels>() == 36);
    assert!(std::mem::size_of::<EthtoolRxFlowSpec>() == 168);
    assert!(std::mem::offset_of!(EthtoolRxFlowSpec, ring_cookie) == 152);
    assert!(std::mem::offset_of!(EthtoolRxFlowSpec, location) == 160);
    assert!(std::mem::size_of::<EthtoolRxnfc>() == 192);
    assert!(std::mem::offset_of!(EthtoolRxnfc, fs) == 16);
    assert!(std::mem::offset_of!(EthtoolRxnfc, rule_cnt) == 184);
    assert!(std::mem::offset_of!(EthtoolRxnfc, rule_locs) == 188);
    assert!(std::mem::size_of::<EthtoolRxfhHead>() == 20 + 4);
};

#[repr(C)]
struct Ifreq {
    ifr_name: [u8; libc::IFNAMSIZ],
    ifr_data: *mut libc::c_void,
}

// ------------------------------------------------------------------ EthtoolNic

/// The live NIC control (see module docs). Owns one AF_INET dgram socket
/// for the ioctl surface.
pub struct EthtoolNic {
    ifname: String,
    sock: OwnedFd,
}

impl EthtoolNic {
    pub fn open(ifname: &str) -> Result<EthtoolNic, String> {
        if ifname.len() >= libc::IFNAMSIZ {
            return Err(format!("ifname too long: {ifname}"));
        }
        // SAFETY: plain socket(2).
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        if fd < 0 {
            return Err(format!(
                "ethtool control socket: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: fd is a fresh, owned socket.
        let sock = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        Ok(EthtoolNic {
            ifname: ifname.to_string(),
            sock,
        })
    }

    /// Current RX descriptor-ring depth (`ethtool -g` current RX) — the
    /// ring-standing-demand input (round 6). Inherent (not NicControl):
    /// only the arm ladder's sizing consumes it.
    pub fn rx_ring_descriptors(&mut self) -> Result<u32, String> {
        let mut rp = EthtoolRingparam {
            cmd: ETHTOOL_GRINGPARAM,
            ..Default::default()
        };
        self.ethtool_ioctl(&mut rp as *mut _ as *mut libc::c_void)
            .map_err(|e| format!("GRINGPARAM: {e}"))?;
        Ok(rp.rx_pending)
    }

    /// One SIOCETHTOOL round-trip with `data` as the command block.
    fn ethtool_ioctl(&self, data: *mut libc::c_void) -> Result<(), String> {
        let mut ifr = Ifreq {
            ifr_name: [0u8; libc::IFNAMSIZ],
            ifr_data: data,
        };
        ifr.ifr_name[..self.ifname.len()].copy_from_slice(self.ifname.as_bytes());
        // SAFETY: ifr and the caller's command block outlive the call;
        // the kernel validates cmd/geometry and returns errno on refusal.
        let rc = unsafe { libc::ioctl(self.sock.as_raw_fd(), SIOCETHTOOL, &mut ifr) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    }

    /// RSS geometry `(indir_size, key_size)` via a sizes-only GRSSH call.
    fn rxfh_sizes(&self) -> Result<(u32, u32), String> {
        let mut head = EthtoolRxfhHead {
            cmd: ETHTOOL_GRSSH,
            ..Default::default()
        };
        self.ethtool_ioctl(&mut head as *mut _ as *mut libc::c_void)
            .map_err(|e| format!("GRSSH sizes: {e}"))?;
        Ok((head.indir_size, head.key_size))
    }

    fn build_flow_spec(loc: u32, rule: &FlowRule) -> Result<EthtoolRxFlowSpec, String> {
        let mut fs = EthtoolRxFlowSpec {
            ring_cookie: rule.queue as u64,
            location: loc,
            ..Default::default()
        };
        // Match host AND mask layouts of ethtool_tcpip{4,6}_spec.
        match (rule.src, rule.dst) {
            (SocketAddr::V4(s), SocketAddr::V4(d)) => {
                fs.flow_type = TCP_V4_FLOW;
                fs.h_u[0..4].copy_from_slice(&s.ip().octets());
                fs.h_u[4..8].copy_from_slice(&d.ip().octets());
                fs.h_u[8..10].copy_from_slice(&s.port().to_be_bytes());
                fs.h_u[10..12].copy_from_slice(&d.port().to_be_bytes());
                fs.m_u[0..12].fill(0xFF); // full-match ip4src/ip4dst/psrc/pdst
            }
            (SocketAddr::V6(s), SocketAddr::V6(d)) => {
                fs.flow_type = TCP_V6_FLOW;
                fs.h_u[0..16].copy_from_slice(&s.ip().octets());
                fs.h_u[16..32].copy_from_slice(&d.ip().octets());
                fs.h_u[32..34].copy_from_slice(&s.port().to_be_bytes());
                fs.h_u[34..36].copy_from_slice(&d.port().to_be_bytes());
                fs.m_u[0..36].fill(0xFF);
            }
            _ => return Err("mixed-family lane flow".into()),
        }
        Ok(fs)
    }
}

impl NicControl for EthtoolNic {
    fn ifname(&self) -> &str {
        &self.ifname
    }

    fn combined_channels(&mut self) -> Result<u32, String> {
        let mut ch = EthtoolChannels {
            cmd: ETHTOOL_GCHANNELS,
            ..Default::default()
        };
        self.ethtool_ioctl(&mut ch as *mut _ as *mut libc::c_void)
            .map_err(|e| format!("GCHANNELS: {e}"))?;
        // Drivers expose either combined or rx queue counts.
        Ok(ch.combined_count.max(ch.rx_count))
    }

    fn tcp_data_split_on(&mut self) -> Result<bool, String> {
        super::ethtool_nl::tcp_data_split_on(&self.ifname)
    }

    fn rxfh_indir(&mut self) -> Result<Vec<u32>, String> {
        let (indir_size, _) = self.rxfh_sizes()?;
        if indir_size == 0 {
            return Err("NIC exposes no RSS indirection table".into());
        }
        let head_words = std::mem::size_of::<EthtoolRxfhHead>() / 4;
        let mut block = vec![0u32; head_words + indir_size as usize];
        block[0] = ETHTOOL_GRSSH;
        block[2] = indir_size;
        self.ethtool_ioctl(block.as_mut_ptr() as *mut libc::c_void)
            .map_err(|e| format!("GRSSH: {e}"))?;
        Ok(block[head_words..].to_vec())
    }

    fn set_rxfh_indir(&mut self, indir: &[u32]) -> Result<(), String> {
        let head_words = std::mem::size_of::<EthtoolRxfhHead>() / 4;
        let mut block = vec![0u32; head_words + indir.len()];
        block[0] = ETHTOOL_SRSSH;
        block[2] = indir.len() as u32;
        // hfunc 0 = keep; key_size 0 = keep.
        block[head_words..].copy_from_slice(indir);
        self.ethtool_ioctl(block.as_mut_ptr() as *mut libc::c_void)
            .map_err(|e| format!("SRSSH: {e}"))
    }

    fn ntuple_enabled(&mut self) -> Result<bool, String> {
        let mut v = EthtoolValue {
            cmd: ETHTOOL_GFLAGS,
            ..Default::default()
        };
        self.ethtool_ioctl(&mut v as *mut _ as *mut libc::c_void)
            .map_err(|e| format!("GFLAGS: {e}"))?;
        Ok(v.data & ETH_FLAG_NTUPLE != 0)
    }

    fn ntuple_table_size(&mut self) -> Result<u32, String> {
        let mut nfc = EthtoolRxnfc {
            cmd: ETHTOOL_GRXCLSRLCNT,
            ..Default::default()
        };
        self.ethtool_ioctl(&mut nfc as *mut _ as *mut libc::c_void)
            .map_err(|e| format!("GRXCLSRLCNT: {e}"))?;
        // `data` carries the table size (rule_cnt the installed count)
        // — with the RX_CLS_LOC_SPECIAL support FLAG bit masked out
        // (ethtool rxclass.c parity: the word is size + flag).
        Ok(nfc.data as u32 & !RX_CLS_LOC_SPECIAL)
    }

    fn special_loc_supported(&mut self) -> Result<bool, String> {
        let mut nfc = EthtoolRxnfc {
            cmd: ETHTOOL_GRXCLSRLCNT,
            ..Default::default()
        };
        self.ethtool_ioctl(&mut nfc as *mut _ as *mut libc::c_void)
            .map_err(|e| format!("GRXCLSRLCNT: {e}"))?;
        Ok(nfc.data as u32 & RX_CLS_LOC_SPECIAL != 0)
    }

    fn ntuple_table_size_hint(&mut self) -> Result<u32, String> {
        // ETHTOOL_GRXCLSRLALL writes the table size into `data` even
        // when GRXCLSRLCNT advertises 0 (mlx5e_ethtool_get_rxnfc sets
        // `info->data = MAX_NUM_OF_ETHTOOL_RULES` = 1024) — the size
        // source ethtool's rxclass_find_empty_slot scans from.
        let mut cnt = EthtoolRxnfc {
            cmd: ETHTOOL_GRXCLSRLCNT,
            ..Default::default()
        };
        self.ethtool_ioctl(&mut cnt as *mut _ as *mut libc::c_void)
            .map_err(|e| format!("GRXCLSRLCNT: {e}"))?;
        let n = cnt.rule_cnt as usize;
        let head = std::mem::size_of::<EthtoolRxnfc>() - 4;
        let mut raw = vec![0u8; head + n * 4 + 4];
        {
            let nfc = raw.as_mut_ptr() as *mut EthtoolRxnfc;
            // SAFETY: raw is sized ≥ the struct + tail; POD writes.
            unsafe {
                (*nfc).cmd = ETHTOOL_GRXCLSRLALL;
                (*nfc).rule_cnt = n as u32;
            }
        }
        self.ethtool_ioctl(raw.as_mut_ptr() as *mut libc::c_void)
            .map_err(|e| format!("GRXCLSRLALL: {e}"))?;
        // SAFETY: kernel wrote the size into the struct's data word.
        let data = unsafe { (*(raw.as_ptr() as *const EthtoolRxnfc)).data };
        Ok(data as u32 & !RX_CLS_LOC_SPECIAL)
    }

    fn ntuple_locs(&mut self) -> Result<Vec<u32>, String> {
        let mut cnt = EthtoolRxnfc {
            cmd: ETHTOOL_GRXCLSRLCNT,
            ..Default::default()
        };
        self.ethtool_ioctl(&mut cnt as *mut _ as *mut libc::c_void)
            .map_err(|e| format!("GRXCLSRLCNT: {e}"))?;
        let n = cnt.rule_cnt as usize;
        if n == 0 {
            return Ok(Vec::new());
        }
        // GRXCLSRLALL: fixed block + n trailing u32 locs.
        let head = std::mem::size_of::<EthtoolRxnfc>() - 4;
        let mut raw = vec![0u8; head + n * 4 + 4];
        {
            let nfc = raw.as_mut_ptr() as *mut EthtoolRxnfc;
            // SAFETY: raw is sized ≥ the struct + tail; POD writes.
            unsafe {
                (*nfc).cmd = ETHTOOL_GRXCLSRLALL;
                (*nfc).rule_cnt = n as u32;
            }
        }
        self.ethtool_ioctl(raw.as_mut_ptr() as *mut libc::c_void)
            .map_err(|e| format!("GRXCLSRLALL: {e}"))?;
        // SAFETY: kernel wrote rule_cnt locs starting at rule_locs.
        let got = unsafe { (*(raw.as_ptr() as *const EthtoolRxnfc)).rule_cnt } as usize;
        let mut locs = Vec::with_capacity(got);
        for i in 0..got.min(n) {
            let off = head + i * 4;
            locs.push(u32::from_ne_bytes(raw[off..off + 4].try_into().unwrap()));
        }
        Ok(locs)
    }

    fn insert_ntuple(&mut self, loc: u32, rule: &FlowRule) -> Result<u32, String> {
        let mut nfc = EthtoolRxnfc {
            cmd: ETHTOOL_SRXCLSRLINS,
            fs: Self::build_flow_spec(loc, rule)?,
            ..Default::default()
        };
        self.ethtool_ioctl(&mut nfc as *mut _ as *mut libc::c_void)
            .map_err(|e| format!("SRXCLSRLINS @{loc}: {e}"))?;
        // The kernel writes the EFFECTIVE location back into fs.location
        // — meaningful for `RX_CLS_LOC_ANY` (the mlx5-class arm, 2026-08
        // field finding 1: 0-advertised tables accept driver-assigned
        // inserts); an explicit loc echoes itself.
        Ok(nfc.fs.location)
    }

    fn delete_ntuple(&mut self, loc: u32) -> Result<(), String> {
        let mut nfc = EthtoolRxnfc {
            cmd: ETHTOOL_SRXCLSRLDEL,
            ..Default::default()
        };
        nfc.fs.location = loc;
        self.ethtool_ioctl(&mut nfc as *mut _ as *mut libc::c_void)
            .map_err(|e| format!("SRXCLSRLDEL @{loc}: {e}"))
    }
}

/// The NIC an outbound connection to `traddr` rides, resolved WITHOUT
/// netlink route dumps: a connected UDP socket names the local source
/// address (no packet is sent), then getifaddrs maps it to an interface.
pub fn route_ifname_for(traddr: &IpAddr) -> Option<String> {
    let probe = std::net::UdpSocket::bind(match traddr {
        IpAddr::V4(_) => "0.0.0.0:0",
        IpAddr::V6(_) => "[::]:0",
    })
    .ok()?;
    probe.connect((*traddr, 9)).ok()?;
    let local = probe.local_addr().ok()?.ip();
    ifname_of_addr(&local)
}

fn ifname_of_addr(addr: &IpAddr) -> Option<String> {
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs allocates the list; freed below.
    if unsafe { libc::getifaddrs(&mut ifap) } != 0 {
        return None;
    }
    let mut found = None;
    let mut cur = ifap;
    while !cur.is_null() {
        // SAFETY: walking the kernel-built list.
        let ifa = unsafe { &*cur };
        if !ifa.ifa_addr.is_null() {
            // SAFETY: ifa_addr points at a sockaddr of the stated family.
            let matched = unsafe {
                match (*ifa.ifa_addr).sa_family as i32 {
                    libc::AF_INET => {
                        let sa = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                        IpAddr::V4(std::net::Ipv4Addr::from(u32::from_be(sa.sin_addr.s_addr)))
                            == *addr
                    }
                    libc::AF_INET6 => {
                        let sa = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                        IpAddr::V6(std::net::Ipv6Addr::from(sa.sin6_addr.s6_addr)) == *addr
                    }
                    _ => false,
                }
            };
            if matched {
                // SAFETY: ifa_name is a NUL-terminated C string.
                let name = unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) };
                found = name.to_str().ok().map(|s| s.to_string());
                break;
            }
        }
        cur = ifa.ifa_next;
    }
    // SAFETY: freeing the list getifaddrs allocated.
    unsafe { libc::freeifaddrs(ifap) };
    found
}

/// The NIC's NUMA node from sysfs (`class/net/<if>/device/numa_node`);
/// `None` on single-node machines / virtual devices (-1).
pub fn nic_numa_node(ifname: &str) -> Option<usize> {
    let raw = std::fs::read_to_string(format!("/sys/class/net/{ifname}/device/numa_node")).ok()?;
    let node: i64 = raw.trim().parse().ok()?;
    usize::try_from(node).ok()
}

/// The NIC's MTU from sysfs — the fill-grain amplification input (the
/// round-5 admission derivation: the kernel pool spends buffers at
/// wire-segment grain).
pub fn nic_mtu(ifname: &str) -> Option<u32> {
    std::fs::read_to_string(format!("/sys/class/net/{ifname}/mtu"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The NIC's ifindex (`if_nametoindex`).
pub fn nic_ifindex(ifname: &str) -> Option<u32> {
    let c = std::ffi::CString::new(ifname).ok()?;
    // SAFETY: c is a valid NUL-terminated string.
    let idx = unsafe { libc::if_nametoindex(c.as_ptr()) };
    if idx == 0 {
        None
    } else {
        Some(idx)
    }
}
