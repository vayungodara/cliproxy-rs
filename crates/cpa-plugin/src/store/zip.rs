//! The part of Go's `archive/zip` plugin archives need: the central directory, each
//! entry's name and `FileInfo().Mode()`, and stored or deflated contents with their
//! CRC-32 checked.
//! ponytail: no ZIP64 and no encryption; plugin archives are small, and Go's errors
//! for those cases are not reproduced.

use std::io::Read as _;

pub const ERR_FORMAT: &str = "zip: not a valid zip file";
pub const ERR_ALGORITHM: &str = "zip: unsupported compression algorithm";
pub const ERR_CHECKSUM: &str = "zip: checksum error";

/// Why a member could not be read: Go's `File.Open` failed, or reading it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    Open(String),
    Read(String),
}

/// One central-directory entry.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    method: u16,
    crc32: u32,
    compressed: u64,
    uncompressed: u64,
    offset: u64,
    creator: u16,
    external: u32,
}

/// `fs.FileMode` type bits Go derives (only whether any is set matters here).
const MODE_DIR: u32 = 1 << 31;
const MODE_OTHER_TYPE: u32 = 1 << 30;

impl Entry {
    /// Go `FileHeader.Mode`: Unix or MS-DOS attributes by creator, plus a directory
    /// for a trailing `/`. Returns (type bits, permission bits).
    fn mode(&self) -> (u32, u32) {
        let (mut kind, perm) = match self.creator >> 8 {
            // creatorUnix, creatorMacOSX
            3 | 19 => {
                let m = self.external >> 16;
                // unixModeToFileMode: block and character devices, pipes, links and
                // sockets have a type; regular files and unknown values do not.
                let kind = match m & 0o170000 {
                    0o040000 => MODE_DIR,
                    0o060000 | 0o020000 | 0o010000 | 0o120000 | 0o140000 => MODE_OTHER_TYPE,
                    _ => 0,
                };
                (kind, m & 0o777)
            }
            // creatorNTFS, creatorVFAT, creatorFAT
            11 | 14 | 0 => {
                let (kind, mut perm) = if self.external & 0x10 != 0 {
                    (MODE_DIR, 0o777)
                } else {
                    (0, 0o666)
                };
                if self.external & 0x01 != 0 {
                    perm &= !0o222;
                }
                (kind, perm)
            }
            _ => (0, 0),
        };
        if self.name.ends_with('/') {
            kind |= MODE_DIR;
        }
        (kind, perm)
    }

    /// `FileInfo().IsDir()`.
    pub fn is_dir(&self) -> bool {
        self.mode().0 & MODE_DIR != 0
    }

    /// `mode.IsRegular() || mode.Type() == 0`.
    pub fn is_regular(&self) -> bool {
        self.mode().0 == 0
    }

    /// The uncompressed size the central directory declares.
    pub fn size(&self) -> u64 {
        self.uncompressed
    }

    /// `FileInfo().Mode().Perm()`.
    pub fn perm(&self) -> u32 {
        self.mode().1
    }
}

/// A parsed archive over its bytes.
pub struct Archive<'a> {
    data: &'a [u8],
    pub entries: Vec<Entry>,
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

impl<'a> Archive<'a> {
    /// `zip.NewReader`: the end-of-central-directory record, then every entry.
    pub fn new(data: &'a [u8]) -> Result<Self, String> {
        let format = || ERR_FORMAT.to_owned();
        if data.len() < 22 {
            return Err(format());
        }
        let search_from = data.len().saturating_sub(22 + 65535);
        let eocd = (search_from..=data.len() - 22)
            .rev()
            .find(|&i| u32_at(data, i) == Some(0x0605_4b50))
            .ok_or_else(format)?;
        let count = u16_at(data, eocd + 10).ok_or_else(format)? as usize;
        let size = u32_at(data, eocd + 12).ok_or_else(format)? as usize;
        let start = u32_at(data, eocd + 16).ok_or_else(format)? as usize;
        if start.checked_add(size).is_none_or(|end| end > eocd) {
            return Err(format());
        }
        let mut entries = Vec::with_capacity(count);
        let mut at = start;
        for _ in 0..count {
            if u32_at(data, at) != Some(0x0201_4b50) {
                return Err(format());
            }
            let field = |off: usize| u16_at(data, at + off).ok_or_else(format);
            let word = |off: usize| u32_at(data, at + off).ok_or_else(format);
            let creator = field(4)?;
            let method = field(10)?;
            let crc32 = word(16)?;
            let compressed = word(20)? as u64;
            let uncompressed = word(24)? as u64;
            let (name_len, extra_len, comment_len) = (field(28)? as usize, field(30)? as usize, field(32)? as usize);
            let external = word(38)?;
            let offset = word(42)? as u64;
            let name = data.get(at + 46..at + 46 + name_len).ok_or_else(format)?;
            entries.push(Entry {
                name: String::from_utf8_lossy(name).into_owned(),
                method,
                crc32,
                compressed,
                uncompressed,
                offset,
                creator,
                external,
            });
            at += 46 + name_len + extra_len + comment_len;
        }
        Ok(Self { data, entries })
    }

    /// `File.Open` + `io.ReadAll`: the entry's contents, CRC-checked. Go's reader
    /// stops at one byte past the declared size, so a member never inflates further.
    pub fn read(&self, entry: &Entry) -> Result<Vec<u8>, ReadError> {
        let open = |e: &str| ReadError::Open(e.to_owned());
        let at = entry.offset as usize;
        if u32_at(self.data, at) != Some(0x0403_4b50) {
            return Err(open(ERR_FORMAT));
        }
        let name_len = u16_at(self.data, at + 26).ok_or(open(ERR_FORMAT))? as usize;
        let extra_len = u16_at(self.data, at + 28).ok_or(open(ERR_FORMAT))? as usize;
        if !matches!(entry.method, 0 | 8) {
            return Err(open(ERR_ALGORITHM));
        }
        let read = |e: &str| ReadError::Read(e.to_owned());
        let start = at + 30 + name_len + extra_len;
        let raw = start
            .checked_add(entry.compressed as usize)
            .and_then(|end| self.data.get(start..end))
            .ok_or(read("unexpected EOF"))?;
        let limit = entry.uncompressed.saturating_add(1);
        let mut out = Vec::new();
        match entry.method {
            0 => out.extend_from_slice(&raw[..raw.len().min(limit as usize)]),
            _ => {
                flate2::read::DeflateDecoder::new(raw)
                    .take(limit)
                    .read_to_end(&mut out)
                    .map_err(|e| ReadError::Read(e.to_string()))?;
            }
        }
        // Go `checksumReader`: past the declared size is ErrFormat, short is
        // io.ErrUnexpectedEOF.
        match (out.len() as u64).cmp(&entry.uncompressed) {
            std::cmp::Ordering::Greater => return Err(read(ERR_FORMAT)),
            std::cmp::Ordering::Less => return Err(read("unexpected EOF")),
            std::cmp::Ordering::Equal => {}
        }
        let mut crc = flate2::Crc::new();
        crc.update(&out);
        if crc.sum() != entry.crc32 {
            return Err(read(ERR_CHECKSUM));
        }
        Ok(out)
    }
}
