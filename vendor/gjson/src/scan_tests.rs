// cliproxy-rs: the patched string scanners against verbatim copies of gjson 0.8.1's
// originals, on every start offset of many short adversarial buffers and of long ones
// with a special byte at every position.

use super::*;

fn original_scan_string<'a>(json: &'a [u8], mut i: usize) -> (&'a [u8], InfoBits, usize) {
    let mut info = 0;
    let s = i;
    i += 1;
    'outer: loop {
        let mut ch;
        'tok: loop {
            while i + 8 < json.len() {
                for _ in 0..8 {
                    ch = json[i] as usize;
                    if CHTABLE[ch] & CHSTRTOK == CHSTRTOK {
                        break 'tok;
                    }
                    i += 1;
                }
            }
            while i < json.len() {
                ch = json[i] as usize;
                if CHTABLE[ch] & CHSTRTOK == CHSTRTOK {
                    break 'tok;
                }
                i += 1;
            }
            break 'outer;
        }
        if ch as u8 == b'"' {
            i += 1;
            return (&json[s..i], info, i);
        } else {
            info |= INFO_ESC;
            i += 1;
            if i == json.len() {
                break;
            }
            i += 1;
        }
    }
    ("".as_bytes(), 0, json.len())
}

fn original_valid_string(json: &[u8], mut i: usize) -> (bool, usize) {
    fn string_byte(c: u8) -> bool {
        c < b' ' || c == b'"' || c == b'\\'
    }
    i += 1;
    loop {
        'tok: loop {
            if i + 32 < json.len() {
                for c in &json[i..i + 32] {
                    if string_byte(*c) {
                        break 'tok;
                    }
                    i += 1;
                }
            }
            while i < json.len() {
                if string_byte(json[i]) {
                    break 'tok;
                }
                i += 1;
            }
            return (false, i);
        }
        if json[i] < b' ' {
            return (false, i);
        }
        if json[i] == b'"' {
            return (true, i + 1);
        }
        if json[i] == b'\\' {
            i += 1;
            if i == json.len() {
                return (false, i);
            }
            match json[i] {
                b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {}
                b'u' => {
                    for _ in 0..4 {
                        i += 1;
                        if i == json.len() {
                            return (false, i);
                        }
                        if !json[i].is_ascii_hexdigit() {
                            return (false, i);
                        }
                    }
                }
                _ => return (false, i),
            }
        }
        i += 1;
    }
}

fn check(buf: &[u8]) {
    for i in 0..buf.len() {
        assert_eq!(scan_string(buf, i), original_scan_string(buf, i), "scan_string {buf:?} at {i}");
        assert_eq!(
            valid::valid_string_for_test(buf, i),
            original_valid_string(buf, i),
            "valid_string {buf:?} at {i}"
        );
    }
}

#[test]
fn patched_scanners_match_the_originals() {
    // Bytes that matter to either scanner, and ones that do not (including UTF-8 and
    // bytes just above the 0x20 boundary).
    let alphabet: &[u8] = b"\"\\\"\\a /bfnrtu0F\x00\x01\x1f\x20\x21\x7f\xc3\xa9\xff{}[],:";
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    for _ in 0..20_000 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let len = (seed % 48) as usize;
        let buf: Vec<u8> = (0..len)
            .map(|k| alphabet[((seed >> (k % 8 * 8)) as usize + k * 7) % alphabet.len()])
            .collect();
        check(&buf);
    }
    // Long plain runs with one special byte at every offset, then a terminator, an
    // escape, a trailing backslash or nothing.
    for special in [b'"', b'\\', 0x00, 0x1f, b'\n'] {
        for at in 0..40 {
            for tail in [&b""[..], b"\"", b"\\", b"\\\"x\"", b"\\u12", b"\\u12ab\""] {
                let mut buf = vec![b'"'];
                buf.extend(std::iter::repeat_n(b'x', at));
                buf.push(special);
                buf.extend(std::iter::repeat_n(b'y', 37));
                buf.extend_from_slice(tail);
                check(&buf);
            }
        }
    }
}
