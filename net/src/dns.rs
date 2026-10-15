//! DNS (RFC 1035): build an A-record query and parse the response.

use crate::wire::Ip;
use alloc::vec::Vec;

pub const PORT: u16 = 53;

#[derive(Debug, PartialEq, Eq)]
pub enum DnsError {
    /// The name is not a valid host name (empty label, label over 63, name over 253...).
    BadName,
    /// The packet is not a well-formed response to our query.
    Malformed,
    /// The id does not match the query.
    WrongId,
    /// The server says the name does not exist.
    NoSuchName,
    /// The server reported another error (code in the low 4 bits of the flags).
    ServerError(u8),
    /// A good answer with no IPv4 address in it.
    NoAddress,
}

pub fn build_query(id: u16, name: &str) -> Result<Vec<u8>, DnsError> {
    let name = name.strip_suffix('.').unwrap_or(name);
    if name.is_empty() || name.len() > 253 {
        return Err(DnsError::BadName);
    }
    let mut q = Vec::with_capacity(18 + name.len());
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&0x0100u16.to_be_bytes()); // recursion desired
    q.extend_from_slice(&1u16.to_be_bytes()); // one question
    q.extend_from_slice(&[0; 6]);
    for label in name.split('.') {
        let b = label.as_bytes();
        if b.is_empty() || b.len() > 63 || !b.iter().all(|&c| c.is_ascii_graphic()) {
            return Err(DnsError::BadName);
        }
        q.push(b.len() as u8);
        q.extend_from_slice(b);
    }
    q.push(0);
    q.extend_from_slice(&1u16.to_be_bytes()); // type A
    q.extend_from_slice(&1u16.to_be_bytes()); // class IN
    Ok(q)
}

/// Skip over a (possibly compressed) name starting at `pos`; returns the position after it.
fn skip_name(b: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let len = *b.get(pos)? as usize;
        if len == 0 {
            return Some(pos + 1);
        }
        if len & 0xC0 == 0xC0 {
            b.get(pos + 1)?;
            return Some(pos + 2);
        }
        if len & 0xC0 != 0 {
            return None;
        }
        pos += 1 + len;
    }
}

/// The first IPv4 address in the answer section (following CNAME records as they come).
pub fn parse_response(id: u16, b: &[u8]) -> Result<Ip, DnsError> {
    if b.len() < 12 {
        return Err(DnsError::Malformed);
    }
    if u16::from_be_bytes([b[0], b[1]]) != id {
        return Err(DnsError::WrongId);
    }
    let flags = u16::from_be_bytes([b[2], b[3]]);
    if flags & 0x8000 == 0 {
        return Err(DnsError::Malformed); // not a response
    }
    match flags & 0x000F {
        0 => {}
        3 => return Err(DnsError::NoSuchName),
        c => return Err(DnsError::ServerError(c as u8)),
    }
    let qd = u16::from_be_bytes([b[4], b[5]]) as usize;
    let an = u16::from_be_bytes([b[6], b[7]]) as usize;
    let mut pos = 12;
    for _ in 0..qd {
        pos = skip_name(b, pos).ok_or(DnsError::Malformed)?;
        pos = pos.checked_add(4).filter(|&p| p <= b.len()).ok_or(DnsError::Malformed)?;
    }
    for _ in 0..an {
        pos = skip_name(b, pos).ok_or(DnsError::Malformed)?;
        let h = b.get(pos..pos + 10).ok_or(DnsError::Malformed)?;
        let rtype = u16::from_be_bytes([h[0], h[1]]);
        let rclass = u16::from_be_bytes([h[2], h[3]]);
        let rdlen = u16::from_be_bytes([h[8], h[9]]) as usize;
        pos += 10;
        let data = b.get(pos..pos + rdlen).ok_or(DnsError::Malformed)?;
        if rtype == 1 && rclass == 1 && rdlen == 4 {
            return Ok(data.try_into().unwrap());
        }
        pos += rdlen;
    }
    Err(DnsError::NoAddress)
}
