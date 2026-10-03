//! The slice of DNS that mDNS service discovery needs: PTR, SRV, TXT, A and AAAA.
//!
//! Names and TXT strings use miekg/dns presentation form, as Go's zeroconf sees them:
//! decoding escapes special bytes (`My\ Server`), encoding splits on unescaped dots and
//! reads `\X` and `\DDD` escapes. Go's discovered `instance_name` and `raw_txt` values
//! are those presentation strings, so cliproxy-rs reproduces them byte for byte.
use std::net::{Ipv4Addr, Ipv6Addr};

pub const TYPE_A: u16 = 1;
pub const TYPE_PTR: u16 = 12;
pub const TYPE_TXT: u16 = 16;
pub const TYPE_AAAA: u16 = 28;
pub const TYPE_SRV: u16 = 33;
pub const CLASS_IN: u16 = 1;
/// mDNS cache-flush bit on a record class, unicast-response bit on a question class.
pub const CLASS_FLUSH: u16 = 1 << 15;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    pub name: String,
    pub qtype: u16,
    pub qclass: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RData {
    A(Ipv4Addr),
    Aaaa(Ipv6Addr),
    Ptr(String),
    Srv {
        priority: u16,
        weight: u16,
        port: u16,
        target: String,
    },
    Txt(Vec<String>),
    /// A zero-length A, AAAA, PTR or SRV: miekg keeps these as zero-value records
    /// (nil IP, empty target) instead of rejecting the message.
    Empty(u16),
    Other(u16, Vec<u8>),
}

impl RData {
    fn rtype(&self) -> u16 {
        match self {
            RData::A(_) => TYPE_A,
            RData::Aaaa(_) => TYPE_AAAA,
            RData::Ptr(_) => TYPE_PTR,
            RData::Srv { .. } => TYPE_SRV,
            RData::Txt(_) => TYPE_TXT,
            RData::Empty(t) | RData::Other(t, _) => *t,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub name: String,
    pub class: u16,
    pub ttl: u32,
    pub data: RData,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Message {
    pub id: u16,
    pub flags: u16,
    pub questions: Vec<Question>,
    pub answers: Vec<Record>,
    pub authority: Vec<Record>,
    pub additional: Vec<Record>,
}

/// QR set, authoritative: Go's `SetReply` plus `Authoritative = true`.
pub const FLAGS_AUTHORITATIVE_RESPONSE: u16 = 0x8400;
/// QR set only: zeroconf's unsolicited announcements and goodbyes.
pub const FLAGS_RESPONSE: u16 = 0x8000;

/// miekg `isDomainNameLabelSpecial`.
fn label_special(b: u8) -> bool {
    matches!(b, b'.' | b' ' | b'\'' | b'@' | b';' | b'(' | b')' | b'"' | b'\\')
}

/// miekg `escapeByte`: `\DDD`.
fn escape_byte(out: &mut String, b: u8) {
    out.push('\\');
    out.push_str(&format!("{b:03}"));
}

struct Reader<'a> {
    msg: &'a [u8],
    off: usize,
}

impl Reader<'_> {
    fn u8(&mut self) -> Option<u8> {
        let b = *self.msg.get(self.off)?;
        self.off += 1;
        Some(b)
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes([self.u8()?, self.u8()?]))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes([self.u8()?, self.u8()?, self.u8()?, self.u8()?]))
    }

    fn bytes(&mut self, n: usize) -> Option<&[u8]> {
        let out = self.msg.get(self.off..self.off.checked_add(n)?)?;
        self.off += n;
        Some(out)
    }

    /// miekg `UnpackDomainName`: follows compression pointers, at most 126 of them,
    /// within the 255-octet wire budget.
    fn name(&mut self) -> Option<String> {
        let mut out = String::new();
        let mut off = self.off;
        let mut resume = None;
        let mut pointers = 0;
        let mut budget: isize = 255;
        loop {
            let c = *self.msg.get(off)? as usize;
            off += 1;
            match c & 0xC0 {
                0x00 if c == 0 => break,
                0x00 => {
                    let label = self.msg.get(off..off + c)?;
                    budget -= c as isize + 1;
                    if budget <= 0 {
                        return None;
                    }
                    for &b in label {
                        if label_special(b) {
                            out.push('\\');
                            out.push(b as char);
                        } else if !(b' '..=b'~').contains(&b) {
                            escape_byte(&mut out, b);
                        } else {
                            out.push(b as char);
                        }
                    }
                    out.push('.');
                    off += c;
                }
                0xC0 => {
                    let low = *self.msg.get(off)? as usize;
                    off += 1;
                    if resume.is_none() {
                        resume = Some(off);
                    }
                    pointers += 1;
                    if pointers > 126 {
                        return None;
                    }
                    off = (c ^ 0xC0) << 8 | low;
                }
                _ => return None,
            }
        }
        self.off = resume.unwrap_or(off);
        if out.is_empty() {
            out.push('.');
        }
        Some(out)
    }

    /// miekg `unpackString`: one character-string in presentation form.
    fn txt_string(&mut self) -> Option<String> {
        let len = self.u8()? as usize;
        let raw = self.bytes(len)?;
        let mut out = String::with_capacity(len);
        for &b in raw {
            match b {
                b'"' | b'\\' => {
                    out.push('\\');
                    out.push(b as char);
                }
                b if !(b' '..=b'~').contains(&b) => escape_byte(&mut out, b),
                b => out.push(b as char),
            }
        }
        Some(out)
    }

    fn record(&mut self) -> Option<Record> {
        let name = self.name()?;
        let rtype = self.u16()?;
        let class = self.u16()?;
        let ttl = self.u32()?;
        let len = self.u16()? as usize;
        let end = self.off.checked_add(len)?;
        if end > self.msg.len() {
            return None;
        }
        // miekg decodes RDATA against the message cut at this record's end, so a
        // compression pointer cannot reach into the following records.
        let mut rd = Reader {
            msg: &self.msg[..end],
            off: self.off,
        };
        let data = match rtype {
            TYPE_A | TYPE_AAAA | TYPE_PTR | TYPE_SRV if len == 0 => RData::Empty(rtype),
            TYPE_A if len == 4 => {
                let b = rd.bytes(4)?;
                RData::A(Ipv4Addr::new(b[0], b[1], b[2], b[3]))
            }
            TYPE_AAAA if len == 16 => RData::Aaaa(Ipv6Addr::from(<[u8; 16]>::try_from(rd.bytes(16)?).ok()?)),
            TYPE_PTR => RData::Ptr(rd.name()?),
            TYPE_SRV => RData::Srv {
                priority: rd.u16()?,
                weight: rd.u16()?,
                port: rd.u16()?,
                target: rd.name()?,
            },
            TYPE_TXT => {
                let mut strings = Vec::new();
                while rd.off < end {
                    strings.push(rd.txt_string()?);
                }
                RData::Txt(strings)
            }
            TYPE_A | TYPE_AAAA => return None,
            other => RData::Other(other, rd.bytes(len)?.to_vec()),
        };
        self.off = rd.off;
        (self.off == end).then_some(Record { name, class, ttl, data })
    }
}

impl Message {
    /// Decodes a whole message; like miekg's `Unpack`, any malformed part drops it.
    pub fn decode(msg: &[u8]) -> Option<Message> {
        let mut r = Reader { msg, off: 0 };
        let id = r.u16()?;
        let flags = r.u16()?;
        let counts = [r.u16()?, r.u16()?, r.u16()?, r.u16()?];
        let mut out = Message {
            id,
            flags,
            ..Default::default()
        };
        for _ in 0..counts[0] {
            out.questions.push(Question {
                name: r.name()?,
                qtype: r.u16()?,
                qclass: r.u16()?,
            });
        }
        for (count, section) in counts[1..]
            .iter()
            .zip([&mut out.answers, &mut out.authority, &mut out.additional])
        {
            for _ in 0..*count {
                section.push(r.record()?);
            }
        }
        Some(out)
    }

    /// Encodes without name compression (every mDNS packet here stays far below 9000
    /// bytes). `None` for a name or string that cannot be packed.
    pub fn encode(&self) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(512);
        out.extend(self.id.to_be_bytes());
        out.extend(self.flags.to_be_bytes());
        for n in [
            self.questions.len(),
            self.answers.len(),
            self.authority.len(),
            self.additional.len(),
        ] {
            out.extend(u16::try_from(n).ok()?.to_be_bytes());
        }
        for q in &self.questions {
            pack_name(&mut out, &q.name)?;
            out.extend(q.qtype.to_be_bytes());
            out.extend(q.qclass.to_be_bytes());
        }
        for rr in self.answers.iter().chain(&self.authority).chain(&self.additional) {
            pack_name(&mut out, &rr.name)?;
            out.extend(rr.data.rtype().to_be_bytes());
            out.extend(rr.class.to_be_bytes());
            out.extend(rr.ttl.to_be_bytes());
            let at = out.len();
            out.extend([0, 0]);
            match &rr.data {
                RData::A(ip) => out.extend(ip.octets()),
                RData::Aaaa(ip) => out.extend(ip.octets()),
                RData::Ptr(name) => pack_name(&mut out, name)?,
                RData::Srv {
                    priority,
                    weight,
                    port,
                    target,
                } => {
                    out.extend(priority.to_be_bytes());
                    out.extend(weight.to_be_bytes());
                    out.extend(port.to_be_bytes());
                    pack_name(&mut out, target)?;
                }
                // An empty TXT has no strings at all (miekg 1.1.43 `packTxt`).
                RData::Txt(strings) => {
                    for s in strings {
                        let bytes = unescape(s);
                        out.push(u8::try_from(bytes.len()).ok()?);
                        out.extend(bytes);
                    }
                }
                RData::Empty(_) => {}
                RData::Other(_, bytes) => out.extend(bytes),
            }
            let len = u16::try_from(out.len() - at - 2).ok()?;
            out[at..at + 2].copy_from_slice(&len.to_be_bytes());
        }
        Some(out)
    }
}

/// Reads presentation bytes the way miekg packs them: `\DDD` is one byte (wrapping like
/// Go's byte arithmetic), `\X` is X, a trailing lone backslash is dropped. With `split`,
/// an unescaped dot is reported as `None` (a label separator).
fn unescaped(b: &[u8], mut each: impl FnMut(Option<u8>) -> Option<()>, split: bool) -> Option<()> {
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' {
            i += 1;
            if i == b.len() {
                break;
            }
            if i + 2 < b.len() && b[i..i + 3].iter().all(u8::is_ascii_digit) {
                let v = (b[i] - b'0') as u32 * 100 + (b[i + 1] - b'0') as u32 * 10 + (b[i + 2] - b'0') as u32;
                each(Some(v as u8))?;
                i += 3;
                continue;
            }
            each(Some(b[i]))?;
        } else if split && b[i] == b'.' {
            each(None)?;
        } else {
            each(Some(b[i]))?;
        }
        i += 1;
    }
    Some(())
}

/// miekg `packTxtString`.
fn unescape(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    unescaped(
        s.as_bytes(),
        |b| {
            out.extend(b);
            Some(())
        },
        false,
    );
    out
}

/// Presentation name to wire labels: unescaped dots separate labels, a trailing dot
/// is optional. Empty labels (`a..b`) and oversized labels or names are rejected.
fn pack_name(out: &mut Vec<u8>, name: &str) -> Option<()> {
    let mut label = Vec::new();
    let mut total = 1;
    let mut flush = |label: &mut Vec<u8>, out: &mut Vec<u8>| -> Option<()> {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        total += label.len() + 1;
        if total > 255 {
            return None;
        }
        out.push(label.len() as u8);
        out.append(label);
        Some(())
    };
    if name != "." {
        unescaped(
            name.as_bytes(),
            |b| match b {
                Some(b) => {
                    label.push(b);
                    Some(())
                }
                None => flush(&mut label, out),
            },
            true,
        )?;
        if !label.is_empty() {
            flush(&mut label, out)?;
        }
    }
    out.push(0);
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_in_miekg_presentation_form() {
        let msg = Message {
            questions: vec![Question {
                name: "My\\ Server\\.x\\001._ai-gateway._tcp.local.".into(),
                qtype: TYPE_PTR,
                qclass: CLASS_IN | CLASS_FLUSH,
            }],
            answers: vec![Record {
                name: "_ai-gateway._tcp.local.".into(),
                class: CLASS_IN,
                ttl: 3200,
                data: RData::Txt(vec!["a=\\\"b\\\\".into(), "\\255".into()]),
            }],
            ..Default::default()
        };
        let wire = msg.encode().unwrap();
        // One 12-byte label "My Server.x\x01", not three.
        assert_eq!(&wire[12..25], b"\x0cMy Server.x\x01");
        assert_eq!(Message::decode(&wire).unwrap(), msg);
    }

    #[test]
    fn unescaped_spaces_pack_like_miekg_and_decode_escaped() {
        let msg = Message {
            answers: vec![Record {
                name: "x.local.".into(),
                class: CLASS_IN,
                ttl: 1,
                data: RData::Ptr("My Server._a._tcp.local".into()),
            }],
            ..Default::default()
        };
        let back = Message::decode(&msg.encode().unwrap()).unwrap();
        assert_eq!(back.answers[0].data, RData::Ptr("My\\ Server._a._tcp.local.".into()));
    }

    #[test]
    fn compression_pointers_and_malformed_messages() {
        // Header, one answer: name "a.local." then PTR pointing back at offset 12.
        let mut wire = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        wire.extend(b"\x01a\x05local\x00");
        wire.extend([0, 12, 0, 1, 0, 0, 0, 10, 0, 2, 0xC0, 12]);
        let msg = Message::decode(&wire).unwrap();
        assert_eq!(msg.answers[0].data, RData::Ptr("a.local.".into()));
        // A pointer loop, a truncated record and reserved label bits are all rejected.
        let mut looped = wire.clone();
        let n = looped.len();
        looped[n - 1] = (n - 2) as u8;
        assert!(Message::decode(&looped).is_none());
        assert!(Message::decode(&wire[..wire.len() - 1]).is_none());
        let mut reserved = wire.clone();
        reserved[12] = 0x41;
        assert!(Message::decode(&reserved).is_none());
        // An A record with the wrong length is malformed.
        let mut bad_a = vec![
            0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 1, 0, 1, 0, 0, 0, 1, 0, 3, 1, 2, 3,
        ];
        assert!(Message::decode(&bad_a).is_none());
        bad_a[22] = 4;
        bad_a.push(4);
        assert_eq!(
            Message::decode(&bad_a).unwrap().answers[0].data,
            RData::A(Ipv4Addr::new(1, 2, 3, 4))
        );
    }

    #[test]
    fn rdata_stays_inside_its_record_and_empty_rdata_is_kept() {
        // Answer 1: PTR whose 2-byte RDATA points forward into answer 2's owner name.
        let mut wire = vec![0, 0, 0x84, 0, 0, 0, 0, 2, 0, 0, 0, 0];
        wire.extend(b"\x01a\x00");
        wire.extend([0, 12, 0, 1, 0, 0, 0, 1, 0, 2, 0xC0, 27]);
        wire.extend(b"\x01b\x00");
        wire.extend([0, 1, 0, 1, 0, 0, 0, 1, 0, 4, 10, 0, 0, 1]);
        assert_eq!(wire[27], 1, "offset 27 is answer 2's name");
        assert!(Message::decode(&wire).is_none(), "miekg rejects the forward reference");
        // Zero-length A/PTR/SRV keep the message; zero-length TXT is an empty list.
        for rtype in [TYPE_A, TYPE_AAAA, TYPE_PTR, TYPE_SRV, TYPE_TXT] {
            let mut wire = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, b'x', 0];
            wire.extend(rtype.to_be_bytes());
            wire.extend([0, 1, 0, 0, 0, 1, 0, 0]);
            let msg = Message::decode(&wire).unwrap();
            let want = if rtype == TYPE_TXT {
                RData::Txt(vec![])
            } else {
                RData::Empty(rtype)
            };
            assert_eq!(msg.answers[0].data, want);
            assert_eq!(msg.encode().unwrap(), wire, "re-encodes with RDLENGTH 0");
        }
    }

    #[test]
    fn oversized_labels_and_names_do_not_pack() {
        let long = "a".repeat(64);
        let msg = |name: String| Message {
            questions: vec![Question {
                name,
                qtype: TYPE_PTR,
                qclass: CLASS_IN,
            }],
            ..Default::default()
        };
        assert!(msg(format!("{long}.local.")).encode().is_none());
        assert!(msg("a..local.".into()).encode().is_none());
        assert!(msg(format!("{}local.", "abcdefghi.".repeat(26))).encode().is_none());
        assert!(msg(format!("{}.local.", "a".repeat(63))).encode().is_some());
    }
}
