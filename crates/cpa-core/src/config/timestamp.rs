//! yaml.v3's plain timestamps (resolve.go `parseTimestamp`), which an untyped value
//! reads as a `time.Time` and serde_yaml_ng as text.

/// The tag [`super::parse_yaml`] gives a plain timestamp in place of yaml.v3's
/// `time.Time`, around its RFC 3339 text. [`super::yaml_string`] writes the text plain.
pub const TAG: &str = "!cpa-timestamp";

/// `text` as Go's `time.RFC3339Nano` writes the `time.Time` yaml.v3 reads from it as a
/// plain scalar; `None` where yaml.v3 reads anything else. yaml.v3 tries Go's
/// `time.Parse` with `2006-1-2T15:4:5.999999999Z07:00`, the same with `t`,
/// `2006-1-2 15:4:5.999999999` and `2006-1-2`, after checking for four digits and `-`.
pub(super) fn go_timestamp(text: &str) -> Option<String> {
    let b = text.as_bytes();
    if b.len() < 5 || !b[..4].iter().all(u8::is_ascii_digit) || b[4] != b'-' {
        return None;
    }
    let year: u32 = text[..4].parse().ok()?;
    let mut rest = &b[5..];
    let month = num(&mut rest)?;
    lit(&mut rest, b'-')?;
    let day = num(&mut rest)?;
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1..=12 => 31,
        _ => return None,
    };
    if !(1..=days).contains(&day) {
        return None;
    }
    let date = format!("{year:04}-{month:02}-{day:02}");
    let zoned = match rest.first() {
        None => return Some(format!("{date}T00:00:00Z")),
        Some(b'T' | b't') => true,
        // Go's `time.Parse` matches a space in the layout to any run of spaces.
        Some(b' ') => false,
        Some(_) => return None,
    };
    rest = if zoned {
        &rest[1..]
    } else {
        let spaces = rest.iter().take_while(|b| **b == b' ').count();
        &rest[spaces..]
    };
    let hour = num(&mut rest).filter(|h| *h < 24)?;
    lit(&mut rest, b':')?;
    let minute = num(&mut rest).filter(|m| *m < 60)?;
    lit(&mut rest, b':')?;
    let second = num(&mut rest).filter(|s| *s < 60)?;
    // A `.` or `,` and every digit after it; nanoseconds from the first nine.
    let mut fraction = String::new();
    if let [b'.' | b',', d, ..] = rest
        && d.is_ascii_digit()
    {
        let digits = rest[1..].iter().take_while(|d| d.is_ascii_digit()).count();
        let kept = std::str::from_utf8(&rest[1..1 + digits.min(9)])
            .ok()?
            .trim_end_matches('0');
        if !kept.is_empty() {
            fraction = format!(".{kept}");
        }
        rest = &rest[1 + digits..];
    }
    let time = format!("{date}T{hour:02}:{minute:02}:{second:02}{fraction}");
    if !zoned {
        return rest.is_empty().then(|| format!("{time}Z"));
    }
    let zone = match rest {
        [b'Z'] => "Z".to_owned(),
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] if [h1, h2, m1, m2].iter().all(|d| d.is_ascii_digit()) => {
            let hours = u32::from(h1 - b'0') * 10 + u32::from(h2 - b'0');
            let minutes = u32::from(m1 - b'0') * 10 + u32::from(m2 - b'0');
            if hours > 24 || minutes > 60 {
                return None;
            }
            // Written from the offset in minutes, so `+00:60` is `+01:00`.
            match hours * 60 + minutes {
                0 => "Z".to_owned(),
                offset => format!("{}{:02}:{:02}", *sign as char, offset / 60, offset % 60),
            }
        }
        _ => return None,
    };
    Some(format!("{time}{zone}"))
}

const WIDE_OFFSET: &str =
    "a timestamp in the config file has a zone offset of 24 hours or more, which is not supported";

/// Refuses a [`TAG`] value with a zone offset of 24 hours or more, which Go reads but
/// cannot marshal as JSON (`time.Time.MarshalJSON`), so its payload rules skip it and its
/// JSON views fail. Part of the config decode, not of [`super::parse_yaml`], whose
/// callers read only their own settings, as Go's do.
pub(super) fn refuse_wide_offsets(value: &serde_yaml_ng::Value) -> anyhow::Result<()> {
    use serde_yaml_ng::Value;
    match value {
        Value::Tagged(t) if t.tag == TAG => {
            let wide = |time: &str| {
                let b = time.as_bytes();
                b.len() > 6 && matches!(b[b.len() - 6], b'+' | b'-') && &time[b.len() - 5..] >= "24:00"
            };
            anyhow::ensure!(!t.value.as_str().is_some_and(wide), WIDE_OFFSET);
        }
        Value::Tagged(t) => refuse_wide_offsets(&t.value)?,
        Value::Sequence(items) => items.iter().try_for_each(refuse_wide_offsets)?,
        Value::Mapping(m) => m.values().try_for_each(refuse_wide_offsets)?,
        _ => {}
    }
    Ok(())
}

/// Go's `getnum` without a fixed width: one digit, or two.
fn num(rest: &mut &[u8]) -> Option<u32> {
    let n = rest.iter().take(2).take_while(|d| d.is_ascii_digit()).count();
    if n == 0 {
        return None;
    }
    let value = rest[..n].iter().fold(0, |acc, d| acc * 10 + u32::from(d - b'0'));
    *rest = &rest[n..];
    Some(value)
}

fn lit(rest: &mut &[u8], byte: u8) -> Option<()> {
    let (first, tail) = rest.split_first()?;
    (*first == byte).then(|| *rest = tail)
}

/// A YAML value serialized for JSON with each [`TAG`] value as its text, as Go marshals
/// a `time.Time`; serde_yaml_ng writes a tagged value as a one-entry map.
pub struct AsJson<'a>(pub &'a serde_yaml_ng::Value);

impl serde::Serialize for AsJson<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde_yaml_ng::Value;
        match self.0 {
            // A `time.Time`; other tags print their value too (yaml.v3 decodes a
            // local tag into `any` as the node's value).
            Value::Tagged(t) => AsJson(&t.value).serialize(s),
            Value::Sequence(items) => s.collect_seq(items.iter().map(AsJson)),
            Value::Mapping(m) => s.collect_map(m.iter().map(|(k, v)| (k, AsJson(v)))),
            other => other.serialize(s),
        }
    }
}
