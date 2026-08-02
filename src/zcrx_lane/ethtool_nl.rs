//! The ethtool GENETLINK `tcp-data-split` probe (design §5 HDS
//! precondition). The 2026-08-02 interface-frontier CORRECTION is pinned
//! here in code: the verdict is the `ETHTOOL_A_RINGS_TCP_DATA_SPLIT`
//! attribute compared against `ETHTOOL_TCP_DATA_SPLIT_ENABLED` — never a
//! rendered string (the probe-spelling false negative that mislabeled the
//! whole zcrx cell driver-blocked).
//!
//! Raw AF_NETLINK/GENERIC — no netlink crate dependency; two round
//! trips: CTRL_CMD_GETFAMILY("ethtool") → ETHTOOL_MSG_RINGS_GET(dev).
//! Field-owed execution (needs a real netdev); every refusal is a
//! `String` the steering arm turns into a loud, NIC-untouched failure.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_ACK: u16 = 4;

const GENL_ID_CTRL: u16 = 16;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;

const ETHTOOL_GENL_NAME: &str = "ethtool";
const ETHTOOL_GENL_VERSION: u8 = 1;
const ETHTOOL_MSG_RINGS_GET: u8 = 15;
const ETHTOOL_A_HEADER_DEV_NAME: u16 = 2;
const ETHTOOL_A_RINGS_HEADER: u16 = 1;
const ETHTOOL_A_RINGS_TCP_DATA_SPLIT: u16 = 11;
const ETHTOOL_TCP_DATA_SPLIT_ENABLED: u8 = 2;
const NLA_F_NESTED: u16 = 0x8000;

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// Append one netlink attribute (padded).
fn put_attr(buf: &mut Vec<u8>, nla_type: u16, payload: &[u8]) {
    let len = 4 + payload.len();
    buf.extend_from_slice(&(len as u16).to_ne_bytes());
    buf.extend_from_slice(&nla_type.to_ne_bytes());
    buf.extend_from_slice(payload);
    buf.resize(buf.len() + (align4(len) - len), 0);
}

/// Build nlmsghdr + genlmsghdr + attrs.
fn genl_msg(family: u16, cmd: u8, version: u8, attrs: &[u8], seq: u32) -> Vec<u8> {
    let len = 16 + 4 + attrs.len();
    let mut msg = Vec::with_capacity(align4(len));
    msg.extend_from_slice(&(len as u32).to_ne_bytes());
    msg.extend_from_slice(&family.to_ne_bytes());
    msg.extend_from_slice(&(NLM_F_REQUEST | NLM_F_ACK).to_ne_bytes());
    msg.extend_from_slice(&seq.to_ne_bytes());
    msg.extend_from_slice(&0u32.to_ne_bytes()); // pid: kernel assigns
    msg.push(cmd);
    msg.push(version);
    msg.extend_from_slice(&0u16.to_ne_bytes());
    msg.extend_from_slice(attrs);
    msg
}

struct GenlSock(OwnedFd);

impl GenlSock {
    fn open() -> Result<GenlSock, String> {
        // SAFETY: plain socket(2).
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                libc::NETLINK_GENERIC,
            )
        };
        if fd < 0 {
            return Err(format!(
                "genetlink socket: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: fresh owned fd.
        Ok(GenlSock(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    fn send(&self, msg: &[u8]) -> Result<(), String> {
        // SAFETY: plain send on our socket.
        let n = unsafe {
            libc::send(
                self.0.as_raw_fd(),
                msg.as_ptr() as *const libc::c_void,
                msg.len(),
                0,
            )
        };
        if n != msg.len() as isize {
            return Err(format!(
                "genetlink send: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    fn recv(&self, buf: &mut [u8]) -> Result<usize, String> {
        // SAFETY: plain recv into our buffer.
        let n = unsafe {
            libc::recv(
                self.0.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                0,
            )
        };
        if n < 0 {
            return Err(format!(
                "genetlink recv: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(n as usize)
    }
}

/// One reply's payload (genl body after nlmsghdr) for `seq`, or the
/// nlmsgerr code as an error. ACK-only exchanges yield `None`.
fn recv_reply(sock: &GenlSock, seq: u32) -> Result<Option<Vec<u8>>, String> {
    let mut buf = vec![0u8; 16384];
    let mut payload = None;
    loop {
        let n = sock.recv(&mut buf)?;
        let mut off = 0usize;
        while off + 16 <= n {
            let len = u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
            let ty = u16::from_ne_bytes(buf[off + 4..off + 6].try_into().unwrap());
            let mseq = u32::from_ne_bytes(buf[off + 8..off + 12].try_into().unwrap());
            if len < 16 || off + len > n {
                return Err("genetlink reply framing".into());
            }
            if mseq == seq {
                match ty {
                    NLMSG_ERROR => {
                        let code = i32::from_ne_bytes(buf[off + 16..off + 20].try_into().unwrap());
                        if code != 0 {
                            return Err(format!(
                                "genetlink error {}",
                                std::io::Error::from_raw_os_error(-code)
                            ));
                        }
                        return Ok(payload); // the ACK terminates
                    }
                    NLMSG_DONE => return Ok(payload),
                    _ => payload = Some(buf[off + 16..off + len].to_vec()),
                }
            }
            off += align4(len);
        }
    }
}

/// Walk attributes in `body` (after the 4-byte genlmsghdr), calling `f`
/// with (type-without-flags, payload).
fn for_each_attr(body: &[u8], mut f: impl FnMut(u16, &[u8])) {
    let mut off = 0usize;
    while off + 4 <= body.len() {
        let len = u16::from_ne_bytes(body[off..off + 2].try_into().unwrap()) as usize;
        let ty = u16::from_ne_bytes(body[off + 2..off + 4].try_into().unwrap());
        if len < 4 || off + len > body.len() {
            return;
        }
        f(ty & !NLA_F_NESTED, &body[off + 4..off + len]);
        off += align4(len);
    }
}

fn resolve_family(sock: &GenlSock) -> Result<u16, String> {
    let mut attrs = Vec::new();
    let mut name = ETHTOOL_GENL_NAME.as_bytes().to_vec();
    name.push(0);
    put_attr(&mut attrs, CTRL_ATTR_FAMILY_NAME, &name);
    let msg = genl_msg(GENL_ID_CTRL, CTRL_CMD_GETFAMILY, 1, &attrs, 1);
    sock.send(&msg)?;
    let body =
        recv_reply(sock, 1)?.ok_or_else(|| "genetlink ctrl: no GETFAMILY reply".to_string())?;
    let mut id = None;
    for_each_attr(&body[4..], |ty, payload| {
        if ty == CTRL_ATTR_FAMILY_ID && payload.len() >= 2 {
            id = Some(u16::from_ne_bytes(payload[..2].try_into().unwrap()));
        }
    });
    id.ok_or_else(|| "ethtool genetlink family not present on this kernel".to_string())
}

/// The HDS verdict for `ifname`: `ETHTOOL_A_RINGS_TCP_DATA_SPLIT ==
/// ENABLED`. `Err` = probe impossible (old kernel, no such device) —
/// the arm refuses loud either way.
pub fn tcp_data_split_on(ifname: &str) -> Result<bool, String> {
    let sock = GenlSock::open()?;
    let family = resolve_family(&sock)?;

    // ETHTOOL_A_RINGS_HEADER (nested) { ETHTOOL_A_HEADER_DEV_NAME }.
    let mut dev = Vec::new();
    let mut name = ifname.as_bytes().to_vec();
    name.push(0);
    put_attr(&mut dev, ETHTOOL_A_HEADER_DEV_NAME, &name);
    let mut attrs = Vec::new();
    put_attr(&mut attrs, ETHTOOL_A_RINGS_HEADER | NLA_F_NESTED, &dev);

    let msg = genl_msg(
        family,
        ETHTOOL_MSG_RINGS_GET,
        ETHTOOL_GENL_VERSION,
        &attrs,
        2,
    );
    sock.send(&msg)?;
    let body = recv_reply(&sock, 2)?.ok_or_else(|| format!("RINGS_GET: no reply for {ifname}"))?;
    let mut split: Option<u8> = None;
    for_each_attr(&body[4..], |ty, payload| {
        if ty == ETHTOOL_A_RINGS_TCP_DATA_SPLIT && !payload.is_empty() {
            split = Some(payload[0]);
        }
    });
    match split {
        Some(v) => Ok(v == ETHTOOL_TCP_DATA_SPLIT_ENABLED),
        // Attribute absent = driver predates HDS reporting ⇒ not capable.
        None => Ok(false),
    }
}
