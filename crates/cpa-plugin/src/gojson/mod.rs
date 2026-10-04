//! Go `encoding/json` for the plugin RPC contract.
//!
//! Most `sdk/pluginapi` structs carry no json tags, so the wire names are the Go field
//! names (`AuthID`, `StorageJSON`). Encoding follows `json.Marshal`: fields in
//! declaration order, `omitempty`, HTML-escaped strings, `[]byte` as padded standard
//! base64, maps with sorted keys, `time.Time` as RFC 3339 with nanoseconds. Decoding
//! follows `json.Unmarshal`: a key matches its field exactly or, failing that,
//! case-insensitively; unknown keys are ignored; the last duplicate wins; `null` leaves
//! scalars alone and clears slices and maps; numbers into integer fields must be integer
//! literals.
//!
//! Decoding walks a [`Node`] tree that keeps every object member and number literal,
//! and follows Go's decode-into-existing rules: struct fields and map keys merge into
//! what is already there (a map element itself is decoded fresh), a pointer decodes
//! into its pointee, a slice reuses its elements.
//!
//! Structs are declared with [`go_struct!`](crate::go_struct). `[]byte` is
//! [`bytes::Bytes`]; `map[string]any` and `any` are [`serde_json::Value`].
//!
//! ponytail: an empty slice, map or `[]byte` encodes as `null` (Go's nil). Fields Go
//! builds non-nil while empty use [`NonNil`].

use std::collections::BTreeMap;
use std::fmt;

use base64::Engine as _;
use bytes::Bytes;
use chrono::{DateTime, Datelike, FixedOffset, Timelike};
use serde_json::Value;

mod node;
pub use node::{Node, parse, parse_first};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DecodeError {}

/// A value with Go `encoding/json` semantics.
pub trait GoJson: Sized + Default {
    /// Go type name, for error messages.
    const GO_TYPE: &'static str;
    /// The Go type as decode errors spell it; containers name their element type
    /// (`[]string`, `map[string]interface {}`).
    fn go_type() -> String {
        Self::GO_TYPE.to_owned()
    }
    /// Appends `json.Marshal` output.
    fn encode(&self, out: &mut Vec<u8>);
    /// `json.Unmarshal` into a zero value.
    fn decode(v: &Node) -> Result<Self, DecodeError> {
        let mut out = Self::default();
        out.decode_into(v)?;
        Ok(out)
    }
    /// `json.Unmarshal` into an existing value. `null` resets slices, maps, pointers and
    /// `any`, and leaves everything else alone.
    fn decode_into(&mut self, v: &Node) -> Result<(), DecodeError> {
        if v.is_null() {
            if Self::NULL_CLEARS {
                *self = Self::default();
            }
            return Ok(());
        }
        *self = Self::decode_value(v)?;
        Ok(())
    }
    /// Decodes a non-null value from scratch. Implement this or [`Self::decode_into`].
    fn decode_value(v: &Node) -> Result<Self, DecodeError> {
        let mut out = Self::default();
        out.decode_into(v)?;
        Ok(out)
    }
    /// Go `omitempty`: false, 0, "", nil or empty slices and maps, nil pointers. Structs
    /// (including `time.Time`) are never empty.
    fn is_empty(&self) -> bool;
    /// Whether `null` resets the field (slices, maps, pointers, `any`); scalars and
    /// structs keep their value.
    const NULL_CLEARS: bool = false;
}

/// `json.Marshal`.
pub fn to_vec<T: GoJson>(v: &T) -> Vec<u8> {
    let mut out = Vec::new();
    v.encode(&mut out);
    out
}

/// `json.Unmarshal` of a complete document.
pub fn from_slice<T: GoJson>(raw: &[u8]) -> Result<T, DecodeError> {
    T::decode(&parse(raw)?)
}

/// `json.NewDecoder(bytes.NewReader(raw)).Decode`: the first value; see [`parse_first`].
pub fn from_slice_first<T: GoJson>(raw: &[u8]) -> Result<T, DecodeError> {
    T::decode(&parse_first(raw)?)
}

fn kind(v: &Node) -> &'static str {
    match v {
        Node::Null => "null",
        Node::Bool(_) => "bool",
        Node::Number(_) => "number",
        Node::String(_) => "string",
        Node::Array(_) => "array",
        Node::Object(_) => "object",
    }
}

/// A value of the wrong JSON kind for `go_type`. Go names a number without its
/// literal here; [`number_error`] is the numeric-range case that includes it.
pub fn type_error(v: &Node, go_type: &str) -> DecodeError {
    let what = match v {
        Node::Number(_) => "number",
        other => kind(other),
    };
    DecodeError(format!("json: cannot unmarshal {what} into Go value of type {go_type}"))
}

/// A number that does not fit the numeric `go_type` (Go `"number " + literal`).
pub fn number_error(literal: &str, go_type: &str) -> DecodeError {
    DecodeError(format!(
        "json: cannot unmarshal number {literal} into Go value of type {go_type}"
    ))
}

/// Adds the struct field to a decode error, like Go's `UnmarshalTypeError`: the
/// innermost struct's name and the field path from the outermost struct (array
/// indexes and map keys are not part of it).
pub fn in_field(err: DecodeError, strukt: &str, field: &str) -> DecodeError {
    if let Some((head, ty)) = err.0.split_once(" into Go value of type ") {
        return DecodeError(format!("{head} into Go struct field {strukt}.{field} of type {ty}"));
    }
    // A field of a nested struct: this field goes in front of its path.
    if let Some((head, rest)) = err.0.split_once(" into Go struct field ")
        && let Some((inner, path)) = rest.split_once('.')
    {
        return DecodeError(format!("{head} into Go struct field {inner}.{field}.{path}"));
    }
    err
}

/// Writes one JSON object field by field.
pub struct ObjWriter<'a> {
    out: &'a mut Vec<u8>,
    first: bool,
}

impl<'a> ObjWriter<'a> {
    pub fn begin(out: &'a mut Vec<u8>) -> Self {
        out.push(b'{');
        Self { out, first: true }
    }

    fn key(&mut self, name: &str) {
        if !self.first {
            self.out.push(b',');
        }
        self.first = false;
        cpa_common::json::marshal_str(self.out, name.as_bytes(), true);
        self.out.push(b':');
    }

    pub fn field<T: GoJson>(&mut self, name: &str, v: &T, omitempty: bool) {
        if omitempty && v.is_empty() {
            return;
        }
        self.key(name);
        v.encode(self.out);
    }

    /// Fields of an embedded struct, flattened like Go embedding.
    pub fn embed<T: GoStruct>(&mut self, v: &T) {
        let mut inner = ObjWriter {
            out: &mut *self.out,
            first: self.first,
        };
        v.encode_fields(&mut inner);
        self.first = inner.first;
    }

    pub fn end(self) {
        self.out.push(b'}');
    }
}

/// A Go struct: encodes its fields in order and decodes them by name.
pub trait GoStruct: GoJson {
    fn encode_fields(&self, w: &mut ObjWriter<'_>);
    /// Decodes `v` into the field named `key` (exact match first, then
    /// case-insensitive). `None` when no field matches.
    fn decode_field(&mut self, key: &str, v: &Node) -> Option<Result<(), DecodeError>>;
}

/// Decodes an object into a fresh struct.
pub fn decode_struct<T: GoStruct>(v: &Node) -> Result<T, DecodeError> {
    let mut out = T::default();
    decode_struct_into(&mut out, v)?;
    Ok(out)
}

/// Decodes an object member by member into an existing struct; later duplicates decode
/// over earlier ones.
pub fn decode_struct_into<T: GoStruct>(out: &mut T, v: &Node) -> Result<(), DecodeError> {
    match v {
        Node::Null => Ok(()),
        Node::Object(members) => {
            // Go keeps decoding after a type mismatch and reports the first one.
            let mut first_err = None;
            for (key, value) in members {
                if let Some(Err(e)) = out.decode_field(key, value) {
                    first_err.get_or_insert(e);
                }
            }
            first_err.map_or(Ok(()), Err)
        }
        other => Err(type_error(other, &T::go_type())),
    }
}

/// Declares a Go struct with its wire names. Append `omitempty` after a name for Go's
/// `,omitempty`.
///
/// ```ignore
/// go_struct! {
///     pub struct PayloadResponse("pluginapi.PayloadResponse") {
///         "Body" => body: Bytes,
///         "path" omitempty => path: String,
///     }
/// }
/// ```
#[macro_export]
macro_rules! go_struct {
    (
        $(#[$meta:meta])*
        pub struct $name:ident ($go_type:literal) {
            $( $(#[$fmeta:meta])* $wire:literal $($omit:ident)? => $field:ident : $ty:ty ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Default, PartialEq)]
        pub struct $name {
            $( $(#[$fmeta])* pub $field: $ty, )*
        }

        impl $crate::gojson::GoJson for $name {
            const GO_TYPE: &'static str = $go_type;
            fn encode(&self, out: &mut Vec<u8>) {
                let mut w = $crate::gojson::ObjWriter::begin(out);
                $crate::gojson::GoStruct::encode_fields(self, &mut w);
                w.end();
            }
            fn decode_into(&mut self, v: &$crate::gojson::Node) -> Result<(), $crate::gojson::DecodeError> {
                $crate::gojson::decode_struct_into(self, v)
            }
            fn is_empty(&self) -> bool {
                false
            }
        }

        impl $crate::gojson::GoStruct for $name {
            #[allow(unused_variables)]
            fn encode_fields(&self, w: &mut $crate::gojson::ObjWriter<'_>) {
                $( w.field($wire, &self.$field, $crate::go_struct!(@omit $($omit)?)); )*
            }
            #[allow(unused_variables)]
            fn decode_field(
                &mut self,
                key: &str,
                v: &$crate::gojson::Node,
            ) -> Option<Result<(), $crate::gojson::DecodeError>> {
                $(
                    if key == $wire {
                        return Some($crate::gojson::decode_field_value(&mut self.$field, v, $go_type, $wire));
                    }
                )*
                $(
                    if key.eq_ignore_ascii_case($wire) {
                        return Some($crate::gojson::decode_field_value(&mut self.$field, v, $go_type, $wire));
                    }
                )*
                None
            }
        }
    };
    (@omit omitempty) => { true };
    (@omit) => { false };
}

/// Decodes one field into its current value.
pub fn decode_field_value<T: GoJson>(slot: &mut T, v: &Node, strukt: &str, field: &str) -> Result<(), DecodeError> {
    slot.decode_into(v)
        .map_err(|e| in_field(e, strukt.rsplit('.').next().unwrap_or(strukt), field))
}

// ----------------------------------------------------------------------------------------
// Scalars

impl GoJson for String {
    const GO_TYPE: &'static str = "string";
    fn encode(&self, out: &mut Vec<u8>) {
        cpa_common::json::marshal_str(out, self.as_bytes(), true);
    }
    fn decode_value(v: &Node) -> Result<Self, DecodeError> {
        match v {
            Node::String(s) => Ok(s.clone()),
            other => Err(type_error(other, Self::GO_TYPE)),
        }
    }
    fn is_empty(&self) -> bool {
        self.is_empty()
    }
}

impl GoJson for bool {
    const GO_TYPE: &'static str = "bool";
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(if *self { b"true" } else { b"false" });
    }
    fn decode_value(v: &Node) -> Result<Self, DecodeError> {
        match v {
            Node::Bool(b) => Ok(*b),
            other => Err(type_error(other, Self::GO_TYPE)),
        }
    }
    fn is_empty(&self) -> bool {
        !*self
    }
}

macro_rules! go_int {
    ($t:ty, $go:literal) => {
        impl GoJson for $t {
            const GO_TYPE: &'static str = $go;
            fn encode(&self, out: &mut Vec<u8>) {
                out.extend_from_slice(self.to_string().as_bytes());
            }
            /// Go parses the literal with `strconv.ParseInt`/`ParseUint`: `1.0`, `1e2` and
            /// out-of-range values are errors.
            fn decode_value(v: &Node) -> Result<Self, DecodeError> {
                match v {
                    Node::Number(n) => n.parse::<$t>().map_err(|_| number_error(n, Self::GO_TYPE)),
                    other => Err(type_error(other, Self::GO_TYPE)),
                }
            }
            fn is_empty(&self) -> bool {
                *self == 0
            }
        }
    };
}

// Go `int` has the platform's pointer width, like `isize`.
go_int!(isize, "int");
go_int!(i64, "int64");
go_int!(i32, "int32");
go_int!(u32, "uint32");
go_int!(u64, "uint64");

impl GoJson for f64 {
    const GO_TYPE: &'static str = "float64";
    fn encode(&self, out: &mut Vec<u8>) {
        // json.Marshal rejects NaN and Inf; the host never produces them.
        out.extend_from_slice(
            cpa_common::json::json_float(*self)
                .unwrap_or_else(|| "0".into())
                .as_bytes(),
        );
    }
    fn decode_value(v: &Node) -> Result<Self, DecodeError> {
        match v {
            Node::Number(n) => node::parse_f64(n),
            other => Err(type_error(other, Self::GO_TYPE)),
        }
    }
    fn is_empty(&self) -> bool {
        *self == 0.0
    }
}

/// `encoding/base64.StdEncoding.Decode` as `encoding/json` uses it: padding required,
/// CR and LF skipped anywhere, non-zero trailing bits accepted. The error is Go's
/// `CorruptInputError`, naming the same input offset (`decodeQuantum`).
fn decode_base64(s: &str) -> Result<Bytes, DecodeError> {
    let src = s.as_bytes();
    let corrupt = |at: usize| DecodeError(format!("illegal base64 data at input byte {at}"));
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let newline = |c: u8| c == b'\n' || c == b'\r';
    let mut out = Vec::with_capacity(src.len() / 4 * 3);
    let mut si = 0;
    while si < src.len() {
        let mut quantum = [0u8; 4];
        let mut len = 4;
        let mut trailing = None;
        let mut j = 0;
        while j < 4 {
            if si == src.len() {
                if j == 0 {
                    return Ok(out.into());
                }
                return Err(corrupt(si - j));
            }
            let c = src[si];
            si += 1;
            if let Some(v) = value(c) {
                quantum[j] = v;
                j += 1;
                continue;
            }
            if newline(c) {
                continue;
            }
            if c != b'=' || j < 2 {
                return Err(corrupt(si - 1));
            }
            if j == 2 {
                // "==" is expected; the first `=` is consumed.
                while si < src.len() && newline(src[si]) {
                    si += 1;
                }
                if si == src.len() {
                    return Err(corrupt(src.len()));
                }
                if src[si] != b'=' {
                    return Err(corrupt(si - 1));
                }
                si += 1;
            }
            while si < src.len() && newline(src[si]) {
                si += 1;
            }
            if si < src.len() {
                trailing = Some(si);
            }
            len = j;
            break;
        }
        let v = (u32::from(quantum[0]) << 18)
            | (u32::from(quantum[1]) << 12)
            | (u32::from(quantum[2]) << 6)
            | u32::from(quantum[3]);
        out.extend_from_slice(&[(v >> 16) as u8, (v >> 8) as u8, v as u8][..len - 1]);
        if let Some(at) = trailing {
            return Err(corrupt(at));
        }
    }
    Ok(out.into())
}

/// `[]byte`: a base64 string, or a JSON array of byte values (a `null` element is 0).
fn decode_byte_slice(v: &Node) -> Result<Bytes, DecodeError> {
    match v {
        Node::String(s) => decode_base64(s),
        Node::Array(items) => items
            .iter()
            .map(|item| match item {
                Node::Null => Ok(0),
                Node::Number(n) => n.parse::<u8>().map_err(|_| number_error(n, "uint8")),
                other => Err(type_error(other, "uint8")),
            })
            .collect::<Result<Vec<u8>, _>>()
            .map(Bytes::from),
        other => Err(type_error(other, "[]uint8")),
    }
}

/// `[]byte`: standard padded base64; empty encodes as `null` (Go's nil).
impl GoJson for Bytes {
    const GO_TYPE: &'static str = "[]uint8";
    const NULL_CLEARS: bool = true;
    fn encode(&self, out: &mut Vec<u8>) {
        if self.is_empty() {
            out.extend_from_slice(b"null");
            return;
        }
        out.push(b'"');
        out.extend_from_slice(base64::engine::general_purpose::STANDARD.encode(self).as_bytes());
        out.push(b'"');
    }
    fn decode_value(v: &Node) -> Result<Self, DecodeError> {
        decode_byte_slice(v)
    }
    fn is_empty(&self) -> bool {
        Bytes::is_empty(self)
    }
}

/// A `[]byte` Go builds non-nil (for example from `io.ReadAll`): encodes `""` when
/// empty instead of `null`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NonNilBytes(pub Bytes);

impl GoJson for NonNilBytes {
    const GO_TYPE: &'static str = "[]uint8";
    const NULL_CLEARS: bool = true;
    fn encode(&self, out: &mut Vec<u8>) {
        out.push(b'"');
        out.extend_from_slice(base64::engine::general_purpose::STANDARD.encode(&self.0).as_bytes());
        out.push(b'"');
    }
    fn decode_value(v: &Node) -> Result<Self, DecodeError> {
        decode_byte_slice(v).map(NonNilBytes)
    }
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// `any` / `map[string]any` values: Go decodes numbers to float64 and encodes maps with
/// sorted keys.
impl GoJson for Value {
    const GO_TYPE: &'static str = "interface {}";
    const NULL_CLEARS: bool = true;
    fn encode(&self, out: &mut Vec<u8>) {
        encode_any(self, out);
    }
    fn decode_value(v: &Node) -> Result<Self, DecodeError> {
        v.to_value()
    }
    fn is_empty(&self) -> bool {
        self.is_null()
    }
}

/// Go's encoding of a value decoded into `any`.
pub fn encode_any(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(b) => b.encode(out),
        Value::Number(n) => match n.as_f64() {
            Some(f) => f.encode(out),
            None => out.extend_from_slice(n.to_string().as_bytes()),
        },
        Value::String(s) => cpa_common::json::marshal_str(out, s.as_bytes(), true),
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                encode_any(item, out);
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let sorted: BTreeMap<&String, &Value> = map.iter().collect();
            out.push(b'{');
            for (i, (key, item)) in sorted.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                cpa_common::json::marshal_str(out, key.as_bytes(), true);
                out.push(b':');
                encode_any(item, out);
            }
            out.push(b'}');
        }
    }
}

// ----------------------------------------------------------------------------------------
// Containers

/// A slice: decoding reuses existing elements and truncates to the array's length.
impl<T: GoJson> GoJson for Vec<T> {
    const GO_TYPE: &'static str = "slice";
    const NULL_CLEARS: bool = true;
    fn go_type() -> String {
        format!("[]{}", T::go_type())
    }
    fn encode(&self, out: &mut Vec<u8>) {
        if self.is_empty() {
            out.extend_from_slice(b"null");
            return;
        }
        encode_items(self, out);
    }
    fn decode_into(&mut self, v: &Node) -> Result<(), DecodeError> {
        match v {
            Node::Null => {
                self.clear();
                Ok(())
            }
            Node::Array(items) => {
                let mut first_err = None;
                for (i, item) in items.iter().enumerate() {
                    if i == self.len() {
                        self.push(T::default());
                    }
                    if let Err(e) = self[i].decode_into(item) {
                        first_err.get_or_insert(e);
                    }
                }
                self.truncate(items.len());
                first_err.map_or(Ok(()), Err)
            }
            other => Err(type_error(other, &Self::go_type())),
        }
    }
    fn is_empty(&self) -> bool {
        Vec::is_empty(self)
    }
}

fn encode_items<T: GoJson>(items: &[T], out: &mut Vec<u8>) {
    out.push(b'[');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        item.encode(out);
    }
    out.push(b']');
}

/// A pointer: `null` makes it nil; a value decodes into the existing pointee.
impl<T: GoJson> GoJson for Option<T> {
    const GO_TYPE: &'static str = T::GO_TYPE;
    const NULL_CLEARS: bool = true;
    fn go_type() -> String {
        T::go_type()
    }
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            None => out.extend_from_slice(b"null"),
            Some(v) => v.encode(out),
        }
    }
    fn decode_into(&mut self, v: &Node) -> Result<(), DecodeError> {
        if v.is_null() {
            *self = None;
            return Ok(());
        }
        self.get_or_insert_with(T::default).decode_into(v)
    }
    fn is_empty(&self) -> bool {
        self.is_none()
    }
}

/// A map: keys merge into the existing map; each element is decoded fresh.
impl<T: GoJson> GoJson for BTreeMap<String, T> {
    const GO_TYPE: &'static str = "map";
    const NULL_CLEARS: bool = true;
    fn go_type() -> String {
        format!("map[string]{}", T::go_type())
    }
    fn encode(&self, out: &mut Vec<u8>) {
        if self.is_empty() {
            out.extend_from_slice(b"null");
            return;
        }
        encode_map(self, out);
    }
    fn decode_into(&mut self, v: &Node) -> Result<(), DecodeError> {
        match v {
            Node::Null => {
                self.clear();
                Ok(())
            }
            Node::Object(members) => {
                let mut first_err = None;
                for (k, item) in members {
                    match T::decode(item) {
                        Ok(value) => {
                            self.insert(k.clone(), value);
                        }
                        Err(e) => {
                            first_err.get_or_insert(e);
                        }
                    }
                }
                first_err.map_or(Ok(()), Err)
            }
            other => Err(type_error(other, &Self::go_type())),
        }
    }
    fn is_empty(&self) -> bool {
        BTreeMap::is_empty(self)
    }
}

fn encode_map<T: GoJson>(map: &BTreeMap<String, T>, out: &mut Vec<u8>) {
    out.push(b'{');
    for (i, (key, item)) in map.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        cpa_common::json::marshal_str(out, key.as_bytes(), true);
        out.push(b':');
        item.encode(out);
    }
    out.push(b'}');
}

/// A slice or map Go builds non-nil even when empty: encodes `[]` / `{}`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NonNil<T>(pub T);

impl<T: GoJson> GoJson for NonNil<Vec<T>> {
    const GO_TYPE: &'static str = "slice";
    const NULL_CLEARS: bool = true;
    fn go_type() -> String {
        Vec::<T>::go_type()
    }
    fn encode(&self, out: &mut Vec<u8>) {
        encode_items(&self.0, out);
    }
    fn decode_into(&mut self, v: &Node) -> Result<(), DecodeError> {
        self.0.decode_into(v)
    }
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<T: GoJson> GoJson for NonNil<BTreeMap<String, T>> {
    const GO_TYPE: &'static str = "map";
    const NULL_CLEARS: bool = true;
    fn go_type() -> String {
        BTreeMap::<String, T>::go_type()
    }
    fn encode(&self, out: &mut Vec<u8>) {
        encode_map(&self.0, out);
    }
    fn decode_into(&mut self, v: &Node) -> Result<(), DecodeError> {
        self.0.decode_into(v)
    }
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// `http.Header` and `url.Values`.
pub type Header = BTreeMap<String, Vec<String>>;
/// `map[string]any`.
pub type Metadata = BTreeMap<String, Value>;
/// `map[string]string`.
pub type StringMap = BTreeMap<String, String>;

// ----------------------------------------------------------------------------------------
// time.Time and json.RawMessage

/// `time.Time`; `None` is the zero time (`0001-01-01T00:00:00Z`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GoTime(pub Option<DateTime<FixedOffset>>);

impl GoTime {
    pub fn now_utc() -> Self {
        Self(Some(chrono::Utc::now().fixed_offset()))
    }

    pub fn is_zero(&self) -> bool {
        self.0.is_none()
    }

    /// `time.Format(time.RFC3339Nano)`.
    pub fn rfc3339_nano(&self) -> String {
        let Some(t) = self.0 else {
            return "0001-01-01T00:00:00Z".into();
        };
        let mut s = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            t.year(),
            t.month(),
            t.day(),
            t.hour(),
            t.minute(),
            t.second()
        );
        let nanos = t.nanosecond() % 1_000_000_000;
        if nanos != 0 {
            let frac = format!("{nanos:09}");
            s.push('.');
            s.push_str(frac.trim_end_matches('0'));
        }
        let offset = t.offset().local_minus_utc();
        if offset == 0 {
            s.push('Z');
        } else {
            let sign = if offset < 0 { '-' } else { '+' };
            let abs = offset.abs();
            s.push_str(&format!("{sign}{:02}:{:02}", abs / 3600, abs % 3600 / 60));
        }
        s
    }
}

impl GoJson for GoTime {
    const GO_TYPE: &'static str = "time.Time";
    fn encode(&self, out: &mut Vec<u8>) {
        out.push(b'"');
        out.extend_from_slice(self.rfc3339_nano().as_bytes());
        out.push(b'"');
    }
    fn decode_value(v: &Node) -> Result<Self, DecodeError> {
        match v {
            Node::String(s) => {
                let t = DateTime::parse_from_rfc3339(s)
                    .map_err(|e| DecodeError(format!("parsing time {s:?} as RFC3339: {e}")))?;
                let zero = t.naive_utc()
                    == chrono::NaiveDate::from_ymd_opt(1, 1, 1)
                        .unwrap()
                        .and_hms_opt(0, 0, 0)
                        .unwrap();
                Ok(Self((!zero).then_some(t)))
            }
            other => Err(type_error(other, Self::GO_TYPE)),
        }
    }
    fn is_empty(&self) -> bool {
        false
    }
}

/// `json.RawMessage`: marshalled compact with HTML escaping; `null` when empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawJson(pub Bytes);

impl GoJson for RawJson {
    const GO_TYPE: &'static str = "json.RawMessage";
    const NULL_CLEARS: bool = true;
    fn encode(&self, out: &mut Vec<u8>) {
        if self.0.is_empty() {
            out.extend_from_slice(b"null");
        } else {
            out.extend_from_slice(&cpa_common::json::compact(&self.0, true));
        }
    }
    /// The member's JSON text re-rendered compactly. ponytail: Go keeps the raw member
    /// text; whitespace and string escapes (`\u003c` vs `<`) can differ, the JSON value
    /// cannot. Keep source spans in `Node` if a byte-exact consumer appears.
    fn decode_into(&mut self, v: &Node) -> Result<(), DecodeError> {
        self.0 = Bytes::from(v.to_json());
        Ok(())
    }
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::go_struct! {
        pub struct Sample("pluginapi.Sample") {
            "AuthID" => auth_id: String,
            "Body" => body: Bytes,
            "Headers" => headers: Header,
            "Count" => count: i64,
            "path" omitempty => path: String,
            "Meta" => meta: Metadata,
            "At" => at: GoTime,
            "Items" omitempty => items: Vec<String>,
        }
    }

    #[test]
    fn encodes_like_json_marshal() {
        let mut headers = Header::new();
        headers.insert("X-B".into(), vec!["2".into()]);
        headers.insert("A".into(), vec!["<1>".into()]);
        let s = Sample {
            auth_id: "a&b".into(),
            body: Bytes::from_static(b"hi"),
            headers,
            count: 7,
            path: String::new(),
            meta: [
                ("z".to_owned(), serde_json::json!(1)),
                ("a".to_owned(), serde_json::json!(0.5)),
            ]
            .into(),
            at: GoTime::default(),
            items: vec![],
        };
        assert_eq!(
            String::from_utf8(to_vec(&s)).unwrap(),
            r#"{"AuthID":"a\u0026b","Body":"aGk=","Headers":{"A":["\u003c1\u003e"],"X-B":["2"]},"Count":7,"Meta":{"a":0.5,"z":1},"At":"0001-01-01T00:00:00Z"}"#
        );
    }

    /// Go matches keys exactly first, then case-insensitively; unknown keys are ignored;
    /// integer fields reject non-integer literals.
    #[test]
    fn decodes_like_json_unmarshal() {
        let s: Sample = from_slice(br#"{"authid":"x","AuthID":"y","body":"aGk=","unknown":1,"count":3,"PATH":"/p","at":"2026-01-02T03:04:05.5+02:00"}"#).unwrap();
        assert_eq!(s.auth_id, "y");
        assert_eq!(s.body, Bytes::from_static(b"hi"));
        assert_eq!(s.count, 3);
        assert_eq!(s.path, "/p");
        assert_eq!(s.at.rfc3339_nano(), "2026-01-02T03:04:05.5+02:00");
        let err = from_slice::<Sample>(br#"{"Count":1.5}"#).unwrap_err();
        assert!(err.0.contains("Go struct field Sample.Count of type int64"), "{err}");
        assert!(from_slice::<Sample>(br#"{"Body":"!!"}"#).is_err());
        let s: Sample = from_slice(br#"{"Body":[104,105],"Count":null}"#).unwrap();
        assert_eq!(s.body, Bytes::from_static(b"hi"));
    }

    /// Go decodes member by member into the existing value: a second spelling of a map
    /// field merges keys; a later struct member decodes over the earlier one field by
    /// field; base64 accepts non-zero trailing bits; a null byte element is 0.
    #[test]
    fn decodes_into_existing_values_like_go() {
        crate::go_struct! {
            pub struct Resp("pluginapi.ManagementResponse") {
                "Headers" => headers: Header,
                "Body" => body: Bytes,
                "Inner" => inner: Option<Sample>,
            }
        }
        let r: Resp = from_slice(
            br#"{"Headers":{"A":["1"]},"headers":{"B":["2"]},"Body":"Zh==","Inner":{"Count":1},"inner":{"Path":"/p"}}"#,
        )
        .unwrap();
        assert_eq!(r.headers.keys().collect::<Vec<_>>(), ["A", "B"]);
        assert_eq!(r.body, Bytes::from_static(b"f"));
        let inner = r.inner.unwrap();
        assert_eq!((inner.count, inner.path.as_str()), (1, "/p"));
        let r: Resp = from_slice(br#"{"Body":[null,65]}"#).unwrap();
        assert_eq!(r.body, Bytes::from_static(&[0, 65]));
        assert!(from_slice::<Resp>(br#"{"Body":[256]}"#).is_err());
        assert!(from_slice::<Resp>(br#"{"Headers":{"a":["x"]},"Body":1e400}"#).is_err());
        // An unknown member never needs to fit a Go type.
        assert!(from_slice::<Resp>(br#"{"unused":1e400}"#).is_ok());
    }

    #[test]
    fn non_nil_and_option_encode_empty_values() {
        assert_eq!(to_vec(&NonNil(Vec::<String>::new())), b"[]");
        assert_eq!(to_vec(&None::<Vec<String>>), b"null");
        assert_eq!(to_vec(&Vec::<String>::new()), b"null");
    }
}
