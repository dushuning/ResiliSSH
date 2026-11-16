//! 发现「连接所用的本机地址已经不存在」：换了 Wi‑Fi、VPN 断开重连、拔了网线之后，原来的连接
//! 已经不可能再通，但 TCP 不会马上知道——要等到单次操作超时（几十秒）才报错。
//! 传输期间定期看一眼本机网卡地址，连接用的那个地址没了就立刻打断，让重试循环马上在新网络上重连。
//!
//! 只在「地址确实消失」时出手，而不是任何网络变化都打断：多出一个 VPN 接口、IPv6 临时地址轮换时，
//! 正在用的连接往往完全正常，打断它反而白白损失一块数据。

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// 本机所有网卡上当前配置的 IP 地址。读取失败时返回 None，调用方应视为「无法判断」而不是「地址都没了」。
pub fn local_addresses() -> Option<HashSet<IpAddr>> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs 在成功时把 list 指向它分配的链表，失败时不修改；下面只在成功后读取。
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return None;
    }
    let mut addresses = HashSet::new();
    let mut cursor = list;
    while !cursor.is_null() {
        // SAFETY: cursor 非空，指向 getifaddrs 返回的、尚未释放的链表节点。
        let entry = unsafe { &*cursor };
        if !entry.ifa_addr.is_null() {
            // SAFETY: ifa_addr 非空时指向一个至少含 sa_family 的 sockaddr；
            // 按 sa_family 转成对应的具体类型后才读取其余字段。
            let family = i32::from(unsafe { (*entry.ifa_addr).sa_family });
            if family == libc::AF_INET {
                let v4 = unsafe { &*(entry.ifa_addr as *const libc::sockaddr_in) };
                addresses.insert(IpAddr::V4(Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr))));
            } else if family == libc::AF_INET6 {
                let v6 = unsafe { &*(entry.ifa_addr as *const libc::sockaddr_in6) };
                addresses.insert(IpAddr::V6(Ipv6Addr::from(v6.sin6_addr.s6_addr)));
            }
        }
        cursor = entry.ifa_next;
    }
    // SAFETY: list 来自上面成功的 getifaddrs，且此后不再访问链表。
    unsafe { libc::freeifaddrs(list) };
    Some(addresses)
}

/// 这个地址能不能用来判断「网络换了」。回环地址永远都在；IPv6 链路本地地址在系统给出的列表里
/// 内嵌了接口编号、与连接上看到的写法不同，比对会误判，两者都跳过。
fn is_comparable(address: &IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => !v4.is_loopback(),
        IpAddr::V6(v6) => !v6.is_loopback() && (v6.segments()[0] & 0xffc0) != 0xfe80,
    }
}

/// `in_use` 中是否有地址已经不在本机任何网卡上了。
pub fn any_address_gone(in_use: &[IpAddr], present: &HashSet<IpAddr>) -> bool {
    in_use.iter().filter(|address| is_comparable(address)).any(|address| {
        // 经 IPv4 建立的连接在双栈 socket 上可能表现为 ::ffff:a.b.c.d，还原后再比。
        let canonical = match address {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(*address, IpAddr::V4),
            IpAddr::V4(_) => *address,
        };
        !present.contains(&canonical)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_loopback_among_local_addresses() {
        let addresses = local_addresses().expect("getifaddrs should work");
        assert!(addresses.contains(&IpAddr::V4(Ipv4Addr::LOCALHOST)));
    }

    #[test]
    fn only_a_vanished_address_counts_as_gone() {
        let wifi: IpAddr = "192.168.1.20".parse().unwrap();
        let vpn: IpAddr = "10.8.0.2".parse().unwrap();
        let present: HashSet<IpAddr> = [wifi].into_iter().collect();
        assert!(!any_address_gone(&[wifi], &present));
        assert!(any_address_gone(&[vpn], &present), "the VPN address is no longer configured");
        // 回环与链路本地地址不参与判断。
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        let link_local: IpAddr = "fe80::1".parse().unwrap();
        assert!(!any_address_gone(&[loopback, link_local], &HashSet::new()));
        let mapped: IpAddr = "::ffff:192.168.1.20".parse().unwrap();
        assert!(!any_address_gone(&[mapped], &present));
    }
}
