//! Byte-preserving FlatBuffer patching, ported from the project's GPL-3.0 implementation.
use crate::{Error, Result};
use serde::Serialize;

pub const MAX_STRING: usize = 1 << 20;

pub fn has_cjk(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(c as u32, 0x3040..=0x30ff | 0x31f0..=0x31ff | 0x3400..=0x9fff | 0xf900..=0xfaff | 0xff66..=0xff9f | 0x20000..=0x2fa1f)
    })
}
pub fn forbidden(text: &str) -> bool {
    text.chars().any(|c| matches!(c as u32, 0..=9 | 11..=31))
}
fn u16_at(b: &[u8], at: usize) -> usize {
    u16::from_le_bytes([b[at], b[at + 1]]) as usize
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

#[derive(Debug, Clone, Serialize)]
pub struct Field<'a> {
    pub slot: usize,
    pub fpos: usize,
    pub spos: usize,
    pub text: &'a str,
}

#[derive(Debug, Clone, Serialize)]
pub struct FieldIssue {
    pub slot: usize,
    pub message: String,
}
pub struct Parsed<'a> {
    pub fields: Vec<Field<'a>>,
    pub issues: Vec<FieldIssue>,
    pub skipped: u64,
}
pub fn parse(blob: &[u8], allow_ascii: bool) -> Result<Vec<Field<'_>>> {
    Ok(parse_with_issues(blob, allow_ascii)?.fields)
}
pub fn parse_with_issues(blob: &[u8], allow_ascii: bool) -> Result<Parsed<'_>> {
    inspect(blob, allow_ascii, |_, _| {})
}

pub(crate) fn inspect(
    blob: &[u8],
    allow_ascii: bool,
    mut rejected_string: impl FnMut(usize, &'static str),
) -> Result<Parsed<'_>> {
    let invalid = |message: &str| Error::Parse(message.into());
    let n = blob.len();
    if n < 16 {
        return Err(invalid("blob too small"));
    }
    let root = u32_at(blob, 0) as usize;
    if root < 4 || root > n - 4 {
        return Err(invalid("root out of range"));
    }
    let delta = i32::from_le_bytes(blob[root..root + 4].try_into().unwrap()) as i64;
    let vt = root as i64 - delta;
    if vt < 0 || vt > (n - 4) as i64 {
        return Err(invalid("vtable out of range"));
    }
    let vt = vt as usize;
    let len = u16_at(blob, vt);
    if len < 4 || !len.is_multiple_of(2) || vt.checked_add(len).is_none_or(|end| end > n) {
        return Err(invalid("bad vtable length"));
    }
    let object = u16_at(blob, vt + 2);
    if object < 4 || root.checked_add(object).is_none_or(|end| end > n) {
        return Err(invalid("table object out of range"));
    }
    if !(vt + len <= root || vt >= root + object) {
        return Err(invalid("vtable overlaps table object"));
    }
    let mut out = Vec::new();
    let mut issues = Vec::new();
    let mut skipped = 0;
    for k in 2..len / 2 {
        let off = u16_at(blob, vt + k * 2);
        if off == 0 {
            continue;
        }
        if off < 4 || off >= object {
            skipped += 1;
            if issues.len() < 20 {
                issues.push(FieldIssue {
                    slot: k - 2,
                    message: "字段偏移超出表对象，已保留该槽的原始数据。".into(),
                });
            }
            continue;
        }
        let slot = k - 2;
        match read_string(blob, slot, root + off, root + object) {
            Ok(field) if (allow_ascii || has_cjk(field.text)) && !forbidden(field.text) => {
                out.push(field)
            }
            Ok(_) => {}
            Err(reason) => rejected_string(slot, reason),
        }
    }
    Ok(Parsed {
        fields: out,
        issues,
        skipped,
    })
}

fn read_string(
    blob: &[u8],
    slot: usize,
    fpos: usize,
    object_end: usize,
) -> std::result::Result<Field<'_>, &'static str> {
    if object_end - fpos < 4 {
        return Err("字段位置超出表对象。");
    }
    let spos = fpos
        .checked_add(u32_at(blob, fpos) as usize)
        .filter(|spos| (4..=blob.len() - 5).contains(spos))
        .ok_or("字符串位置超出记录。")?;
    let size = u32_at(blob, spos) as usize;
    if size > MAX_STRING {
        return Err("字符串长度超出限制。");
    }
    let end = spos
        .checked_add(4 + size)
        .filter(|end| *end < blob.len())
        .ok_or("字符串结束位置超出记录。")?;
    if blob[end] != 0 {
        return Err("字符串缺少结束标记。");
    }
    let text = std::str::from_utf8(&blob[spos + 4..end]).map_err(|_| "字符串不是有效的 UTF-8。")?;
    Ok(Field {
        slot,
        fpos,
        spos,
        text,
    })
}

#[derive(Default, Debug, Serialize)]
pub struct BlobStats {
    pub inplace: u64,
    pub appended: u64,
    pub bytes_appended: u64,
    #[serde(skip_serializing_if = "is_zero")]
    pub fields_skipped: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<FieldIssue>,
}
fn is_zero(value: &u64) -> bool {
    *value == 0
}
