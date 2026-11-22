//! rtnetlink `RTM_GETLINK` 主数据源：一次 dump 读取全部接口的 `IFLA_STATS64`。
//!
//! 只接受 64 位计数器；任一接口缺少 `IFLA_STATS64` 或 socket 不可用时返回错误，
//! 由调用方回退到 `/proc/net/dev`，不混用 32 位计数器。

use std::{
    io, mem,
    os::fd::RawFd,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
};

use libc::{
    c_void, ifinfomsg, rtattr, sockaddr_nl, AF_NETLINK, AF_UNSPEC, NETLINK_ROUTE, SOCK_CLOEXEC,
    SOCK_RAW,
};

/// 某接口自启动以来的原始字节计数器。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkCounters {
    pub name: String,
    pub rx_total: u64,
    pub tx_total: u64,
}

const NLMSG_HDRLEN: usize = 16;
const NLMSG_ALIGNTO: usize = 4;
const NLMSG_ERROR_CODE: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_DUMP: u16 = 0x300;
const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
const RTM_GETLINK: u16 = 18;
const RTM_NEWROUTE: u16 = 24;
const IFLA_IFNAME: u16 = 3;
const IFLA_STATS64: u16 = 23;
const RTMGRP_LINK: u32 = 0x1;
const RTMGRP_IPV4_ROUTE: u32 = 0x100;

fn align4(value: usize) -> usize {
    (value + NLMSG_ALIGNTO - 1) & !(NLMSG_ALIGNTO - 1)
}

/// 从一次 dump 的原始字节中解析所有同时具备名称与 64 位计数器的接口。
pub fn parse_link_dump(buffer: &[u8]) -> io::Result<Vec<LinkCounters>> {
    let mut links = Vec::new();
    let mut offset = 0usize;
    while offset + NLMSG_HDRLEN <= buffer.len() {
        let header = &buffer[offset..offset + NLMSG_HDRLEN];
        let length = u32::from_ne_bytes(header[0..4].try_into().unwrap()) as usize;
        let kind = u16::from_ne_bytes(header[4..6].try_into().unwrap());
        if length < NLMSG_HDRLEN || offset + length > buffer.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated netlink message",
            ));
        }
        match kind {
            NLMSG_DONE => break,
            NLMSG_ERROR_CODE => {
                if length >= NLMSG_HDRLEN + 4 {
                    let code = i32::from_ne_bytes(
                        buffer[offset + NLMSG_HDRLEN..offset + NLMSG_HDRLEN + 4]
                            .try_into()
                            .unwrap(),
                    );
                    if code != 0 {
                        return Err(io::Error::from_raw_os_error(-code));
                    }
                }
            }
            RTM_NEWLINK => {
                let message = &buffer[offset..offset + length];
                if let Some(link) = parse_new_link(message)? {
                    links.push(link);
                }
            }
            _ => {}
        }
        offset += align4(length);
    }
    if links.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "netlink dump contained no IFLA_STATS64 interfaces",
        ));
    }
    Ok(links)
}

fn parse_new_link(message: &[u8]) -> io::Result<Option<LinkCounters>> {
    let body = NLMSG_HDRLEN;
    if message.len() < body + mem::size_of::<ifinfomsg>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "RTM_NEWLINK is shorter than ifinfomsg",
        ));
    }
    let mut offset = align4(body + mem::size_of::<ifinfomsg>());
    let mut name: Option<String> = None;
    let mut stats: Option<(u64, u64)> = None;
    while offset + mem::size_of::<rtattr>() <= message.len() {
        let attr_length =
            u16::from_ne_bytes(message[offset..offset + 2].try_into().unwrap()) as usize;
        let attr_kind = u16::from_ne_bytes(message[offset + 2..offset + 4].try_into().unwrap());
        if attr_length < mem::size_of::<rtattr>() || offset + attr_length > message.len() {
            break;
        }
        let payload = &message[offset + 4..offset + attr_length];
        match attr_kind {
            IFLA_IFNAME => {
                let end = payload
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(payload.len());
                name = Some(String::from_utf8_lossy(&payload[..end]).into_owned());
            }
            // rtnl_link_stats64 前四个字段：rx_packets, tx_packets, rx_bytes, tx_bytes。
            IFLA_STATS64 if payload.len() >= 32 => {
                let rx_bytes = u64::from_ne_bytes(payload[16..24].try_into().unwrap());
                let tx_bytes = u64::from_ne_bytes(payload[24..32].try_into().unwrap());
                stats = Some((rx_bytes, tx_bytes));
            }
            _ => {}
        }
        offset += align4(attr_length);
    }
    Ok(match (name, stats) {
        (Some(name), Some((rx_total, tx_total))) => Some(LinkCounters {
            name,
            rx_total,
            tx_total,
        }),
        (Some(name), None) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("interface {name} has no IFLA_STATS64"),
            ))
        }
        (None, _) => None,
    })
}

/// 一次 `RTM_GETLINK` dump；同步调用，供采样线程使用。
pub fn read_link_counters() -> io::Result<Vec<LinkCounters>> {
    let fd = socket(0)?;
    let result = (|| {
        let request = build_getlink_request();
        let mut address: sockaddr_nl = unsafe { mem::zeroed() };
        address.nl_family = AF_NETLINK as u16;
        let sent = unsafe {
            libc::sendto(
                fd,
                request.as_ptr() as *const c_void,
                request.len(),
                0,
                &address as *const sockaddr_nl as *const libc::sockaddr,
                mem::size_of::<sockaddr_nl>() as u32,
            )
        };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0u8; 64 * 1024];
        let mut all = Vec::new();
        loop {
            let received =
                unsafe { libc::recv(fd, buffer.as_mut_ptr() as *mut c_void, buffer.len(), 0) };
            if received < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if received == 0 {
                break;
            }
            let chunk = &buffer[..received as usize];
            let done = chunk
                .windows(2)
                .step_by(4)
                .any(|window| u16::from_ne_bytes([window[0], window[1]]) == NLMSG_DONE);
            match parse_link_dump(chunk) {
                Ok(mut links) => all.append(&mut links),
                Err(error) if error.kind() == io::ErrorKind::NotFound && !done => {
                    // 单个 recv 只包含部分消息时继续读取。
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Err(error) => return Err(error),
            }
            if done {
                break;
            }
        }
        if all.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "netlink returned no interface counters",
            ));
        }
        Ok(all)
    })();
    unsafe { libc::close(fd) };
    result
}

fn build_getlink_request() -> Vec<u8> {
    let length = NLMSG_HDRLEN + mem::size_of::<ifinfomsg>();
    let mut request = vec![0u8; align4(length)];
    request[0..4].copy_from_slice(&(length as u32).to_ne_bytes());
    request[4..6].copy_from_slice(&RTM_GETLINK.to_ne_bytes());
    request[6..8].copy_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes());
    request[8..12].copy_from_slice(&1u32.to_ne_bytes());
    request[12..16].copy_from_slice(&0u32.to_ne_bytes());
    let info = {
        let mut info: ifinfomsg = unsafe { mem::zeroed() };
        info.ifi_family = AF_UNSPEC as u8;
        info_to_bytes(info)
    };
    request[NLMSG_HDRLEN..NLMSG_HDRLEN + mem::size_of::<ifinfomsg>()].copy_from_slice(&info);
    request
}

fn info_to_bytes(info: ifinfomsg) -> [u8; mem::size_of::<ifinfomsg>()] {
    // 与内核 ABI 对齐的 16 字节结构。
    let mut bytes = [0u8; 16];
    bytes[0] = info.ifi_family;
    bytes[2..4].copy_from_slice(&info.ifi_type.to_ne_bytes());
    bytes[4..8].copy_from_slice(&info.ifi_index.to_ne_bytes());
    bytes[8..12].copy_from_slice(&info.ifi_flags.to_ne_bytes());
    bytes[12..16].copy_from_slice(&info.ifi_change.to_ne_bytes());
    bytes
}

fn socket(groups: u32) -> io::Result<RawFd> {
    let fd = unsafe { libc::socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let address = {
        let mut address: sockaddr_nl = unsafe { mem::zeroed() };
        address.nl_family = AF_NETLINK as u16;
        address.nl_groups = groups;
        address
    };
    let bound = unsafe {
        libc::bind(
            fd,
            &address as *const sockaddr_nl as *const libc::sockaddr,
            mem::size_of::<sockaddr_nl>() as u32,
        )
    };
    if bound < 0 {
        let error = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(error);
    }
    Ok(fd)
}

/// 订阅链路/路由变化的轻量 watcher：只置 dirty，采样线程在下一周期合并处理。
pub struct NetlinkWatcher {
    dirty: Arc<AtomicBool>,
}

impl NetlinkWatcher {
    pub fn spawn() -> io::Result<Self> {
        let fd = socket(RTMGRP_LINK | RTMGRP_IPV4_ROUTE)?;
        let dirty = Arc::new(AtomicBool::new(false));
        let flag = dirty.clone();
        thread::Builder::new()
            .name("trafficview-netlink".into())
            .spawn(move || {
                let mut buffer = vec![0u8; 8192];
                loop {
                    let received = unsafe {
                        libc::recv(fd, buffer.as_mut_ptr() as *mut c_void, buffer.len(), 0)
                    };
                    if received < 0 {
                        let error = io::Error::last_os_error();
                        if error.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        break;
                    }
                    if received == 0 {
                        break;
                    }
                    let chunk = &buffer[..received as usize];
                    let mut offset = 0usize;
                    while offset + NLMSG_HDRLEN <= chunk.len() {
                        let length =
                            u32::from_ne_bytes(chunk[offset..offset + 4].try_into().unwrap())
                                as usize;
                        let kind =
                            u16::from_ne_bytes(chunk[offset + 4..offset + 6].try_into().unwrap());
                        if length < NLMSG_HDRLEN || offset + length > chunk.len() {
                            break;
                        }
                        if matches!(kind, RTM_NEWLINK | RTM_DELLINK | RTM_NEWROUTE) {
                            flag.store(true, Ordering::Release);
                        }
                        offset += align4(length);
                    }
                }
                unsafe { libc::close(fd) };
            })?;
        Ok(Self { dirty })
    }

    /// 读取并清除 dirty 标志；事件被合并到下一次采样周期。
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attribute(kind: u16, payload: &[u8]) -> Vec<u8> {
        let length = 4 + payload.len();
        let mut bytes = vec![0u8; align4(length)];
        bytes[0..2].copy_from_slice(&(length as u16).to_ne_bytes());
        bytes[2..4].copy_from_slice(&kind.to_ne_bytes());
        bytes[4..length].copy_from_slice(payload);
        bytes
    }

    fn link_message(name: &str, stats: Option<(u64, u64)>) -> Vec<u8> {
        let mut body = {
            let mut info: ifinfomsg = unsafe { mem::zeroed() };
            info.ifi_family = AF_UNSPEC as u8;
            info.ifi_type = 1;
            info.ifi_index = 2;
            info.ifi_flags = 1;
            info_to_bytes(info).to_vec()
        };
        body.extend(attribute(IFLA_IFNAME, format!("{name}\0").as_bytes()));
        if let Some((rx, tx)) = stats {
            let mut payload = vec![0u8; 64];
            payload[16..24].copy_from_slice(&rx.to_ne_bytes());
            payload[24..32].copy_from_slice(&tx.to_ne_bytes());
            body.extend(attribute(IFLA_STATS64, &payload));
        }
        let length = NLMSG_HDRLEN + body.len();
        let mut message = vec![0u8; align4(length)];
        message[0..4].copy_from_slice(&(length as u32).to_ne_bytes());
        message[4..6].copy_from_slice(&RTM_NEWLINK.to_ne_bytes());
        message[NLMSG_HDRLEN..NLMSG_HDRLEN + body.len()].copy_from_slice(&body);
        message
    }

    #[test]
    fn parses_only_ifla_stats64_counters() {
        let mut buffer = link_message("eth0", Some((1024, 2048)));
        buffer.extend(link_message("wlan0", Some((7, 8))));
        let links = parse_link_dump(&buffer).unwrap();
        assert_eq!(
            links,
            vec![
                LinkCounters {
                    name: "eth0".into(),
                    rx_total: 1024,
                    tx_total: 2048
                },
                LinkCounters {
                    name: "wlan0".into(),
                    rx_total: 7,
                    tx_total: 8
                }
            ]
        );
    }

    #[test]
    fn missing_stats64_is_rejected_so_the_caller_can_fall_back() {
        let buffer = link_message("eth0", None);
        let error = parse_link_dump(&buffer).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
