//! The kernel's thermal events, over generic netlink: a thermal zone crossing one of its trip points, on
//! the way up or on the way down.
//!
//! The family `thermal` and its multicast group `event` are looked up by name, then joined; what arrives
//! is read as it comes, with nothing polled. [`parse`] reads a buffer of messages and knows nothing of
//! sockets, so that it can be tested on bytes.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const NETLINK_GENERIC: libc::c_int = 16;
const SOL_NETLINK: libc::c_int = 270;
const NETLINK_ADD_MEMBERSHIP: libc::c_int = 1;
const NLMSG_ERROR: u16 = 2;
const NLM_F_REQUEST: u16 = 1;
const GENL_ID_CTRL: u16 = 0x10;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;
const CTRL_ATTR_MCAST_GROUPS: u16 = 7;
const CTRL_ATTR_MCAST_GRP_NAME: u16 = 1;
const CTRL_ATTR_MCAST_GRP_ID: u16 = 2;

/// The kernel's names and numbers for its thermal events (`linux/thermal.h`).
pub const FAMILY: &str = "thermal";
pub const GROUP: &str = "event";
pub const EVENT_TRIP_UP: u8 = 5;
pub const EVENT_TRIP_DOWN: u8 = 6;
pub const ATTR_TZ_ID: u16 = 2;
pub const ATTR_TZ_TEMP: u16 = 3;
pub const ATTR_TZ_TRIP_ID: u16 = 5;

/// A zone crossed a trip point. The temperature is the zone's, in millidegrees, where the kernel gave it.
#[derive(Debug, Clone, PartialEq)]
pub struct TripEvent {
    pub zone: u32,
    pub trip: u32,
    pub up: bool,
    pub temperature: Option<i32>,
}

fn align(n: usize) -> usize {
    (n + 3) & !3
}

/// The attributes in a buffer, as `(type, payload)`; the nesting and byte-order flags are dropped from the
/// type.
pub fn attributes(mut buf: &[u8]) -> Vec<(u16, &[u8])> {
    let mut out = Vec::new();
    while buf.len() >= 4 {
        let len = u16::from_ne_bytes([buf[0], buf[1]]) as usize;
        let kind = u16::from_ne_bytes([buf[2], buf[3]]) & 0x3fff;
        if len < 4 || len > buf.len() {
            break;
        }
        out.push((kind, &buf[4..len]));
        buf = &buf[align(len).min(buf.len())..];
    }
    out
}

fn u32_of(b: &[u8]) -> Option<u32> {
    b.get(..4).map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
}

/// The messages in a buffer, as `(type, body)`, the body starting after the netlink header.
fn messages(mut buf: &[u8]) -> Vec<(u16, &[u8])> {
    let mut out = Vec::new();
    while buf.len() >= 16 {
        let len = u32_of(buf).unwrap_or(0) as usize;
        if len < 16 || len > buf.len() {
            break;
        }
        out.push((u16::from_ne_bytes([buf[4], buf[5]]), &buf[16..len]));
        buf = &buf[align(len).min(buf.len())..];
    }
    out
}

/// The trip events in a buffer of messages of the family `family`; any other event is not one.
pub fn parse(buf: &[u8], family: u16) -> Vec<TripEvent> {
    messages(buf)
        .into_iter()
        .filter(|(kind, body)| *kind == family && body.len() >= 4)
        .filter_map(|(_, body)| {
            let up = match body[0] {
                EVENT_TRIP_UP => true,
                EVENT_TRIP_DOWN => false,
                _ => return None,
            };
            let attrs = attributes(&body[4..]);
            let find = |k: u16| attrs.iter().find(|(t, _)| *t == k).and_then(|(_, v)| u32_of(v));
            Some(TripEvent { zone: find(ATTR_TZ_ID)?, trip: find(ATTR_TZ_TRIP_ID)?, up, temperature: find(ATTR_TZ_TEMP).map(|t| t as i32) })
        })
        .collect()
}

/// A message: the netlink header, the generic one, and the attributes given.
pub fn message(kind: u16, flags: u16, cmd: u8, attrs: &[(u16, &[u8])]) -> Vec<u8> {
    let mut m = vec![0u8; 16];
    m[4..6].copy_from_slice(&kind.to_ne_bytes());
    m[6..8].copy_from_slice(&flags.to_ne_bytes());
    m.extend_from_slice(&[cmd, 1, 0, 0]);
    for (t, v) in attrs {
        let len = 4 + v.len();
        m.extend_from_slice(&(len as u16).to_ne_bytes());
        m.extend_from_slice(&t.to_ne_bytes());
        m.extend_from_slice(v);
        m.resize(align(m.len()), 0);
    }
    let total = m.len() as u32;
    m[0..4].copy_from_slice(&total.to_ne_bytes());
    m
}

/// The family's number and its group's, from the controller's answer.
pub fn resolved(buf: &[u8], group: &str) -> Result<(u16, u32), String> {
    for (kind, body) in messages(buf) {
        if kind == NLMSG_ERROR {
            let code = u32_of(body).map(|c| -(c as i32)).unwrap_or(0);
            return Err(format!("the kernel has no {FAMILY} events: {}", std::io::Error::from_raw_os_error(code)));
        }
        if body.len() < 4 {
            continue;
        }
        let attrs = attributes(&body[4..]);
        let family = attrs.iter().find(|(t, _)| *t == CTRL_ATTR_FAMILY_ID).and_then(|(_, v)| v.get(..2)).map(|v| u16::from_ne_bytes([v[0], v[1]]));
        let groups = attrs.iter().find(|(t, _)| *t == CTRL_ATTR_MCAST_GROUPS).map(|(_, v)| *v).unwrap_or_default();
        let id = attributes(groups).into_iter().find_map(|(_, g)| {
            let g = attributes(g);
            let name = g.iter().find(|(t, _)| *t == CTRL_ATTR_MCAST_GRP_NAME)?.1;
            (name.strip_suffix(&[0]).unwrap_or(name) == group.as_bytes())
                .then(|| g.iter().find(|(t, _)| *t == CTRL_ATTR_MCAST_GRP_ID).and_then(|(_, v)| u32_of(v)))
                .flatten()
        });
        return match (family, id) {
            (Some(f), Some(g)) => Ok((f, g)),
            (Some(_), None) => Err(format!("the kernel's {FAMILY} family has no group {group}")),
            _ => Err(format!("the kernel did not name the {FAMILY} family")),
        };
    }
    Err("the kernel did not answer".into())
}

/// A socket joined to the kernel's thermal events.
pub struct Events {
    fd: OwnedFd,
    family: u16,
}

fn last_error(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

impl Events {
    /// Looks the family up and joins its event group; says why where it cannot.
    pub fn join() -> Result<Events, String> {
        // SAFETY: a plain socket call; the descriptor is owned at once.
        let raw = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, NETLINK_GENERIC) };
        if raw < 0 {
            return Err(last_error("no generic netlink socket"));
        }
        // SAFETY: `raw` is a descriptor just opened and owned by nobody else.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let name = [FAMILY.as_bytes(), &[0]].concat();
        let ask = message(GENL_ID_CTRL, NLM_F_REQUEST, CTRL_CMD_GETFAMILY, &[(CTRL_ATTR_FAMILY_NAME, &name)]);
        // SAFETY: the buffer is valid for its length.
        if unsafe { libc::send(fd.as_raw_fd(), ask.as_ptr().cast(), ask.len(), 0) } < 0 {
            return Err(last_error("the kernel cannot be asked for its thermal family"));
        }
        let mut buf = vec![0u8; 8192];
        // SAFETY: the buffer is valid for its length.
        let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            return Err(last_error("the kernel did not say its thermal family"));
        }
        let (family, group) = resolved(&buf[..n as usize], GROUP)?;
        // SAFETY: the option is a u32 read for its size.
        let joined = unsafe {
            libc::setsockopt(fd.as_raw_fd(), SOL_NETLINK, NETLINK_ADD_MEMBERSHIP, (&group as *const u32).cast(), 4)
        };
        if joined < 0 {
            return Err(last_error("the thermal events cannot be joined"));
        }
        Ok(Events { fd, family })
    }

    /// Waits for the next messages, and gives the trip events among them.
    pub fn next(&self) -> Result<Vec<TripEvent>, String> {
        let mut buf = vec![0u8; 16384];
        loop {
            // SAFETY: the buffer is valid for its length.
            let n = unsafe { libc::recv(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                match e.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    // The kernel sent more than was read in time; what follows is still good.
                    Some(libc::ENOBUFS) => return Ok(Vec::new()),
                    _ => return Err(format!("the thermal events stopped: {e}")),
                }
            }
            return Ok(parse(&buf[..n as usize], self.family));
        }
    }
}
