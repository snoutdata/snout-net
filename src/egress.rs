//! Which addresses a request may connect to. Pure: no `unsafe`, no Postgres, tested with plain
//! `#[test]`s and fuzzed (fuzz/). The client asks [`Policy::refuses`] for every address it is about
//! to open a socket to, which is after DNS, for an IP literal, and again on every redirect hop, so
//! nothing reaches a network this refuses whatever the URL said.
#![forbid(unsafe_code)]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// A network: an address and how many leading bits of it matter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Network {
	address: IpAddr,
	prefix: u8,
}

impl Network {
	/// `10.0.0.0/8`, `fd00::/8`, or a bare address (all its bits). Anything else is `None`.
	pub fn parse(text: &str) -> Option<Network> {
		let text = text.trim();
		let (address, prefix) = match text.split_once('/') {
			Some((address, prefix)) => (address, Some(prefix)),
			None => (text, None),
		};
		let address: IpAddr = address.parse().ok()?;
		let max = if address.is_ipv4() { 32 } else { 128 };
		let prefix = match prefix {
			Some(p) if !p.is_empty() && p.len() <= 3 && p.bytes().all(|b| b.is_ascii_digit()) => {
				p.parse::<u8>().ok()?
			}
			Some(_) => return None,
			None => max,
		};
		if prefix > max {
			return None;
		}
		Some(Network { address, prefix })
	}

	pub fn contains(&self, address: IpAddr) -> bool {
		match (self.address, address) {
			(IpAddr::V4(net), IpAddr::V4(a)) => {
				leading_bits_match(&net.octets(), &a.octets(), self.prefix)
			}
			(IpAddr::V6(net), IpAddr::V6(a)) => {
				leading_bits_match(&net.octets(), &a.octets(), self.prefix)
			}
			_ => false,
		}
	}
}

fn leading_bits_match(a: &[u8], b: &[u8], bits: u8) -> bool {
	let whole = usize::from(bits / 8);
	if a[..whole] != b[..whole] {
		return false;
	}
	let rest = bits % 8;
	if rest == 0 {
		return true;
	}
	let mask = 0xffu8 << (8 - rest);
	(a[whole] & mask) == (b[whole] & mask)
}

/// The addresses a request may not reach unless a network is allowed: every one that is not a
/// public unicast address, which is where a platform keeps what a customer must not touch (the
/// instance metadata service, the host's own network, the database's own loopback).
pub fn is_internal(address: IpAddr) -> Option<&'static str> {
	match address {
		IpAddr::V4(a) => internal_v4(a),
		IpAddr::V6(a) => internal_v6(a),
	}
}

fn internal_v4(a: Ipv4Addr) -> Option<&'static str> {
	let [o0, o1, o2, _] = a.octets();
	if o0 == 127 {
		Some("a loopback address")
	} else if o0 == 169 && o1 == 254 {
		Some("a link-local address, where cloud instance metadata lives")
	} else if o0 == 10 || (o0 == 172 && (16..=31).contains(&o1)) || (o0 == 192 && o1 == 168) {
		Some("a private address")
	} else if o0 == 100 && (64..=127).contains(&o1) {
		Some("a shared (carrier-grade NAT) address")
	} else if o0 == 0 {
		Some("an unspecified address")
	} else if o0 == 192 && o1 == 0 && o2 == 0 {
		Some("an IETF protocol assignment")
	} else if o0 == 198 && (o1 == 18 || o1 == 19) {
		Some("a benchmarking address")
	} else if o0 >= 224 {
		Some("a multicast or reserved address")
	} else {
		None
	}
}

fn internal_v6(a: Ipv6Addr) -> Option<&'static str> {
	let s = a.segments();
	// An IPv4 address carried inside an IPv6 one is judged as the IPv4 address it reaches.
	if let Some(v4) = embedded_v4(a) {
		return internal_v4(v4);
	}
	if a.is_unspecified() {
		Some("an unspecified address")
	} else if a.is_loopback() {
		Some("a loopback address")
	} else if s[0] & 0xfe00 == 0xfc00 {
		Some("a unique local address, where cloud instance metadata can live")
	} else if s[0] & 0xffc0 == 0xfe80 {
		Some("a link-local address")
	} else if s[0] & 0xff00 == 0xff00 {
		Some("a multicast address")
	} else if s[0] == 0x0100 && s[1] == 0 && s[2] == 0 && s[3] == 0 {
		Some("a discard-only address")
	} else {
		None
	}
}

/// The IPv4 address an IPv6 address stands for, where it stands for one: IPv4-mapped
/// (`::ffff:a.b.c.d`), IPv4-compatible (`::a.b.c.d`, deprecated but still routed by some stacks),
/// NAT64 (`64:ff9b::/96`) and 6to4 (`2002::/16`).
fn embedded_v4(a: Ipv6Addr) -> Option<Ipv4Addr> {
	let s = a.segments();
	let tail =
		|hi: u16, lo: u16| Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
	let mapped = s[..5] == [0, 0, 0, 0, 0] && s[5] == 0xffff;
	let compatible = s[..6] == [0, 0, 0, 0, 0, 0] && (s[6] != 0 || s[7] > 1);
	let nat64 = s[0] == 0x0064 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0];
	if mapped || compatible || nat64 {
		Some(tail(s[6], s[7]))
	} else if s[0] == 0x2002 {
		Some(tail(s[1], s[2]))
	} else {
		None
	}
}

/// What may be reached. The default refuses every internal address; `allowed` names networks that
/// are reachable anyway (a self-hosted stack's own services, a test server).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Policy {
	allowed: Vec<Network>,
}

impl Policy {
	/// A comma-separated list of networks, as `snout_net.allowed_networks` is written. Empty is
	/// the default policy; one entry that does not parse refuses the whole list.
	pub fn parse(list: &str) -> Option<Policy> {
		let mut allowed = Vec::new();
		for item in list.split(',') {
			if item.trim().is_empty() {
				continue;
			}
			allowed.push(Network::parse(item)?);
		}
		Some(Policy { allowed })
	}

	/// Why `address` may not be connected to, or `None` when it may.
	pub fn refuses(&self, address: IpAddr) -> Option<&'static str> {
		let why = is_internal(address)?;
		if self.allowed.iter().any(|n| {
			n.contains(address)
				|| embedded_v4(ipv6_of(address)).is_some_and(|v4| n.contains(IpAddr::V4(v4)))
		}) {
			None
		} else {
			Some(why)
		}
	}
}

fn ipv6_of(address: IpAddr) -> Ipv6Addr {
	match address {
		IpAddr::V4(a) => a.to_ipv6_mapped(),
		IpAddr::V6(a) => a,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn ip(s: &str) -> IpAddr {
		s.parse().unwrap()
	}

	#[test]
	fn the_metadata_service_and_every_private_range_are_refused() {
		let policy = Policy::default();
		for a in [
			"169.254.169.254",
			"10.0.0.1",
			"172.16.0.1",
			"172.31.255.255",
			"192.168.1.1",
			"127.0.0.1",
			"127.1.2.3",
			"0.0.0.0",
			"100.64.0.1",
			"224.0.0.1",
			"255.255.255.255",
			"::1",
			"::",
			"fd00:ec2::254",
			"fe80::1",
			"ff02::1",
			"::ffff:169.254.169.254",
			"::ffff:10.0.0.1",
			"::127.0.0.1",
			"64:ff9b::a9fe:a9fe",
			"2002:a9fe:a9fe::1",
		] {
			assert!(policy.refuses(ip(a)).is_some(), "{a} should be refused");
		}
	}

	#[test]
	fn public_addresses_are_reached() {
		let policy = Policy::default();
		for a in [
			"1.1.1.1",
			"8.8.8.8",
			"172.32.0.1",
			"172.15.255.255",
			"100.128.0.1",
			"2606:4700::1111",
			"::ffff:1.1.1.1",
			"2002:0101:0101::1",
		] {
			assert_eq!(policy.refuses(ip(a)), None, "{a} should be reached");
		}
	}

	#[test]
	fn an_allowed_network_is_reached_and_nothing_else_is() {
		let policy = Policy::parse(" 172.18.0.0/16 , 10.1.2.3").unwrap();
		assert_eq!(policy.refuses(ip("172.18.4.5")), None);
		assert_eq!(policy.refuses(ip("10.1.2.3")), None);
		assert_eq!(policy.refuses(ip("::ffff:172.18.4.5")), None);
		assert!(policy.refuses(ip("10.1.2.4")).is_some());
		assert!(policy.refuses(ip("172.19.0.1")).is_some());
		assert!(policy.refuses(ip("169.254.169.254")).is_some());
	}

	#[test]
	fn a_list_that_does_not_parse_is_refused_whole() {
		assert_eq!(Policy::parse(""), Some(Policy::default()));
		assert_eq!(Policy::parse(" , "), Some(Policy::default()));
		for bad in [
			"10.0.0.0/33",
			"10.0.0.0/",
			"10.0.0.0/+8",
			"fd00::/129",
			"example.com",
			"10.0.0.0/8,nope",
			"10.0.0.0/08x",
		] {
			assert_eq!(Policy::parse(bad), None, "{bad}");
		}
	}

	#[test]
	fn prefixes_match_on_their_bits() {
		let n = Network::parse("172.16.0.0/12").unwrap();
		assert!(n.contains(ip("172.31.0.1")));
		assert!(!n.contains(ip("172.32.0.1")));
		assert!(Network::parse("0.0.0.0/0").unwrap().contains(ip("9.9.9.9")));
		assert!(!Network::parse("0.0.0.0/0").unwrap().contains(ip("::1")));
		assert!(Network::parse("fd00::/8").unwrap().contains(ip("fd12::1")));
	}
}
