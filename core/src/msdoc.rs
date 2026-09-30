//! Text extraction for legacy Word 97-2003 (.doc) files.
//!
//! Reads the piece table (CLX) referenced from the File Information Block and
//! decodes each text piece, which is either Windows-1252 or UTF-16LE.

use std::io::{self, Read};
use std::path::Path;

use crate::extract::cp1252;

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_owned())
}

fn u16_at(b: &[u8], off: usize) -> io::Result<u16> {
    b.get(off..off + 2).map(|s| u16::from_le_bytes([s[0], s[1]])).ok_or_else(|| invalid("truncated"))
}

fn u32_at(b: &[u8], off: usize) -> io::Result<u32> {
    b.get(off..off + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| invalid("truncated"))
}

fn read_stream(file: &mut cfb::CompoundFile<std::fs::File>, name: &str) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    file.open_stream(name)?.take(256 << 20).read_to_end(&mut buf)?;
    Ok(buf)
}

pub fn extract(path: &Path) -> io::Result<String> {
    let mut file = cfb::open(path)?;
    let word = read_stream(&mut file, "/WordDocument")?;
    if u16_at(&word, 0)? != 0xA5EC {
        return Err(invalid("not a Word document"));
    }
    let flags = u16_at(&word, 0x0A)?;
    if flags & 0x0100 != 0 {
        return Err(invalid("encrypted document"));
    }
    let table_name = if flags & 0x0200 != 0 { "/1Table" } else { "/0Table" };
    let table = read_stream(&mut file, table_name)?;

    let fc_clx = u32_at(&word, 0x01A2)? as usize;
    let lcb_clx = u32_at(&word, 0x01A6)? as usize;
    let clx = table.get(fc_clx..fc_clx + lcb_clx).ok_or_else(|| invalid("bad CLX"))?;

    // Skip any Prc entries (0x01) to reach the Pcdt (0x02).
    let mut pos = 0;
    while clx.get(pos) == Some(&0x01) {
        let cb = u16_at(clx, pos + 1)? as usize;
        pos += 3 + cb;
    }
    if clx.get(pos) != Some(&0x02) {
        return Err(invalid("missing piece table"));
    }
    let lcb = u32_at(clx, pos + 1)? as usize;
    let plc = clx.get(pos + 5..pos + 5 + lcb).ok_or_else(|| invalid("bad piece table"))?;
    if lcb < 16 {
        return Err(invalid("empty piece table"));
    }
    let n = (lcb - 4) / 12;

    let mut raw = String::new();
    for i in 0..n {
        let cp_start = u32_at(plc, i * 4)? as usize;
        let cp_end = u32_at(plc, (i + 1) * 4)? as usize;
        let count = cp_end.saturating_sub(cp_start);
        let pcd = (n + 1) * 4 + i * 8;
        let fc = u32_at(plc, pcd + 2)?;
        if fc & 0x4000_0000 != 0 {
            let off = ((fc & !0x4000_0000) / 2) as usize;
            if let Some(bytes) = word.get(off..off + count) {
                raw.extend(bytes.iter().map(|&b| cp1252(b)));
            }
        } else {
            let off = fc as usize;
            if let Some(bytes) = word.get(off..off + count * 2) {
                let units = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c));
                raw.extend(char::decode_utf16(units).map(|r| r.unwrap_or('\u{FFFD}')));
            }
        }
        if raw.len() > crate::extract::MAX_TEXT_BYTES {
            break;
        }
    }
    Ok(clean(&raw))
}

/// Converts Word control characters to plain text and drops field instructions.
fn clean(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    // Depth of nested fields whose instruction part we are currently inside.
    let mut field_stack: Vec<bool> = Vec::new();
    for c in raw.chars() {
        match c {
            '\u{13}' => field_stack.push(true),
            '\u{14}' => {
                if let Some(top) = field_stack.last_mut() {
                    *top = false;
                }
            }
            '\u{15}' => {
                field_stack.pop();
            }
            _ if field_stack.last() == Some(&true) => {}
            '\r' | '\u{0B}' | '\u{0C}' | '\u{0E}' => out.push('\n'),
            '\u{07}' => out.push('\t'),
            '\u{1E}' => out.push('-'),
            '\u{1F}' => {}
            '\t' | '\n' => out.push(c),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn clean_fields() {
        let s = super::clean("Hello \u{13} HYPERLINK \"x\" \u{14}link\u{15} world\r");
        assert_eq!(s, "Hello link world\n");
    }
}
