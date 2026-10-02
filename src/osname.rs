// Copyright (c) 2026 Mikko Tanner. All rights reserved.

/*!
Lossless names: filesystem names that are not UTF-8, carried as `str`.

A Unix name is a string of bytes, but the tree interns names in a
[UniqueStrStore](stringstore::UniqueStrStore), which holds `str`. A
lossy conversion would replace each invalid byte with U+FFFD. The entry
then comes back as a path naming another file, or none, and two names
that differ only in such bytes become one entry, hiding the other.
A scanner that treats its files as hostile cannot have that.

So names are encoded instead:

- [encode_name] keeps valid UTF-8 as is, and maps each byte that is not
  part of valid UTF-8 (always >= 0x80) to the private-use character
  `U+F700 + byte` (U+F780..U+F7FF). ASCII stays ASCII, so splitting on
  `/` and comparing names work unchanged.
- A name that really contains one of U+F780..U+F7FF would decode to a
  different byte, so `encode_name` escapes the three UTF-8 bytes of
  such a character one by one. The mapping stays one to one.
- [decode_name] undoes it. Every path the tree builds for the kernel
  goes through it ([Directory::path](super::Directory::path) and the
  like).

Every name in the tree's string store, and every `&str` path its API
takes or returns, is in this encoded form. For a UTF-8 name it is the
name itself.
*/

use std::{
    borrow::Cow,
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
    str::{from_utf8, from_utf8_unchecked},
};

/// An escape is `ESCAPE_BASE + byte`, for the bytes 0x80..=0xFF.
const ESCAPE_BASE: u32 = 0xF700;
const ESCAPE_LO: char = '\u{F780}';
const ESCAPE_HI: char = '\u{F7FF}';

/**
The first byte of every escape in UTF-8 (`EF 9E xx` / `EF 9F xx`): a
string without it has nothing to escape or decode.
*/
const ESCAPE_LEAD: u8 = 0xEF;

/// Whether `c` is in the escape range.
#[inline]
fn is_escape(c: char) -> bool {
    (ESCAPE_LO..=ESCAPE_HI).contains(&c)
}

/// Whether `s` contains characters of the escape range.
#[inline]
pub fn is_escaped(s: &str) -> bool {
    s.as_bytes().contains(&ESCAPE_LEAD) && s.chars().any(is_escape)
}

#[inline]
fn push_byte(out: &mut String, b: u8) {
    // b >= 0x80, so this is always within U+F780..U+F7FF
    out.push(char::from_u32(ESCAPE_BASE + b as u32).unwrap_or(char::REPLACEMENT_CHARACTER));
}

/// Valid UTF-8 as is, except characters of the escape range, escaped byte by byte.
fn push_valid(out: &mut String, s: &str) {
    if !is_escaped(s) {
        out.push_str(s);
        return;
    }
    let mut buf = [0u8; 4];
    for c in s.chars() {
        match is_escape(c) {
            true => c.encode_utf8(&mut buf).bytes().for_each(|b| push_byte(out, b)),
            false => out.push(c),
        }
    }
}

/**
A name's bytes as a `str`, losslessly (see the module docs). Borrowed
for the usual valid UTF-8 name, and for an ASCII name (nearly all of
them) on one word-at-a-time ASCII check: the walker encodes every name
it reads.
*/
#[inline]
pub fn encode_name(bytes: &[u8]) -> Cow<'_, str> {
    if bytes.is_ascii() {
        // SAFETY: ASCII is valid UTF-8
        return Cow::Borrowed(unsafe { from_utf8_unchecked(bytes) });
    }
    encode_non_ascii(bytes)
}

/// [encode_name] for a name with bytes >= 0x80.
#[cold]
fn encode_non_ascii(bytes: &[u8]) -> Cow<'_, str> {
    if let Ok(s) = from_utf8(bytes)
        && !is_escaped(s)
    {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(bytes.len() + 8);
    let mut rest: &[u8] = bytes;
    while !rest.is_empty() {
        // error_len() is None for a sequence truncated at the end: escape all of it
        let (good, bad): (usize, usize) = match from_utf8(rest) {
            Ok(_) => (rest.len(), 0),
            Err(e) => (
                e.valid_up_to(),
                e.error_len().unwrap_or(rest.len() - e.valid_up_to()),
            ),
        };
        push_valid(&mut out, from_utf8(&rest[..good]).unwrap_or_default());
        rest[good..good + bad].iter().for_each(|&b| push_byte(&mut out, b));
        rest = &rest[good + bad..];
    }
    Cow::Owned(out)
}

/**
[encode_name] for an [OsStr], a [Path](std::path::Path) or the like. For a whole path,
this is the form the tree's `&str` API takes.
*/
#[inline]
pub fn encode_os<S: AsRef<OsStr> + ?Sized>(name: &S) -> Cow<'_, str> {
    encode_name(name.as_ref().as_bytes())
}

/// The bytes an encoded name stands for: [encode_name] undone.
pub fn decode_name(s: &str) -> Cow<'_, [u8]> {
    if !is_escaped(s) {
        return Cow::Borrowed(s.as_bytes());
    }
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let mut buf = [0u8; 4];
    for c in s.chars() {
        match is_escape(c) {
            true => out.push((c as u32 - ESCAPE_BASE) as u8),
            false => out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes()),
        }
    }
    Cow::Owned(out)
}

/// [decode_name] as an [OsStr], ready for a path or a syscall.
pub fn decode_os(s: &str) -> Cow<'_, OsStr> {
    match decode_name(s) {
        Cow::Borrowed(b) => Cow::Borrowed(OsStr::from_bytes(b)),
        Cow::Owned(v) => Cow::Owned(OsString::from_vec(v)),
    }
}

/// [decode_name] for a whole path given in the encoded form.
#[inline]
pub fn decode_path(s: &str) -> PathBuf {
    PathBuf::from(decode_os(s).into_owned())
}
