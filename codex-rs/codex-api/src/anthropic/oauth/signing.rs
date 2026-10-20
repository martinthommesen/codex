//! Byte-preserving CCH compatibility, adapted from CLIProxyAPI's MIT implementation.
//! See LICENSE.CLIProxyAPI and the parent module for the pinned reference.

use serde::Deserialize;
use serde_json::value::RawValue;

use super::OAuthCompatibilityError;

const CCH_SEED: u64 = 0x4D65_9218_E32A_3268;

/// Sign the final serialized request. Only the five billing-block CCH digits change.
/// Re-encoding the JSON after this operation invalidates the checksum.
pub fn sign_body(body: &mut [u8]) -> Result<(), OAuthCompatibilityError> {
    let Some(offset) = billing_cch_offset(body)? else {
        return Err(OAuthCompatibilityError::MissingBillingChecksum);
    };
    body[offset..offset + 5].copy_from_slice(b"00000");
    let normalized = normalize_cch_input(body)?;
    let checksum = format!("{:05x}", xxhash64(&normalized, CCH_SEED) & 0xF_FFFF);
    body[offset..offset + 5].copy_from_slice(checksum.as_bytes());
    Ok(())
}

fn billing_cch_offset(body: &[u8]) -> Result<Option<usize>, OAuthCompatibilityError> {
    #[derive(Deserialize)]
    struct Envelope<'a> {
        #[serde(borrow, default)]
        system: Vec<&'a RawValue>,
    }
    #[derive(Deserialize)]
    struct Block<'a> {
        #[serde(borrow)]
        text: &'a RawValue,
    }
    let envelope: Envelope<'_> = serde_json::from_slice(body)?;
    let Some(first) = envelope.system.first() else {
        return Ok(None);
    };
    let block: Block<'_> = serde_json::from_str(first.get())?;
    let text: String = serde_json::from_str(block.text.get())?;
    if !text.starts_with("x-anthropic-billing-header:") {
        return Ok(None);
    }
    let raw = block.text.get().as_bytes();
    let start = (raw.as_ptr() as usize)
        .checked_sub(body.as_ptr() as usize)
        .ok_or(OAuthCompatibilityError::InvalidChecksumInput)?;
    for prefix in 0..raw.len().saturating_sub(9) {
        if raw.get(prefix..prefix + 4) != Some(b"cch=") {
            continue;
        }
        let digits = prefix + 4;
        if raw.get(digits + 5) == Some(&b';')
            && raw[digits..digits + 5]
                .iter()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
        {
            return Ok(Some(start + digits));
        }
    }
    Ok(None)
}

#[derive(Clone, Copy)]
struct Member {
    start: usize,
    end: usize,
    comma_before: Option<usize>,
    comma_after: Option<usize>,
    excluded: bool,
}

struct Scanner<'a> {
    body: &'a [u8],
    pos: usize,
    edits: Vec<(usize, usize)>,
}

/// Build the native hash view without reserializing: empty raw `model` string values and
/// remove dispatch-only members at every nesting depth. Escaped key spellings are untouched.
/// The hash view deliberately retains the native trailing-comma quirk and need not be JSON.
pub fn normalize_cch_input(body: &[u8]) -> Result<Vec<u8>, OAuthCompatibilityError> {
    let _: &RawValue = serde_json::from_slice(body)?;
    let mut scanner = Scanner {
        body,
        pos: 0,
        edits: Vec::new(),
    };
    scanner.value(true)?;
    scanner.whitespace();
    if scanner.pos != body.len() {
        return Err(OAuthCompatibilityError::InvalidChecksumInput);
    }
    scanner.edits.sort_unstable();
    let mut normalized = Vec::with_capacity(body.len());
    let mut last = 0;
    for (start, end) in scanner.edits {
        if start < last || end > body.len() {
            return Err(OAuthCompatibilityError::InvalidChecksumInput);
        }
        normalized.extend_from_slice(&body[last..start]);
        last = end;
    }
    normalized.extend_from_slice(&body[last..]);
    Ok(normalized)
}

impl Scanner<'_> {
    fn value(&mut self, collect: bool) -> Result<(), OAuthCompatibilityError> {
        self.whitespace();
        match self.body.get(self.pos) {
            Some(b'{') => self.object(collect),
            Some(b'[') => self.array(collect),
            Some(b'"') => self.string().map(|_| ()),
            Some(_) => {
                let start = self.pos;
                while let Some(b) = self.body.get(self.pos) {
                    if matches!(b, b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n') {
                        break;
                    }
                    self.pos += 1;
                }
                if self.pos == start {
                    return Err(OAuthCompatibilityError::InvalidChecksumInput);
                }
                Ok(())
            }
            None => Err(OAuthCompatibilityError::InvalidChecksumInput),
        }
    }

    fn object(&mut self, collect: bool) -> Result<(), OAuthCompatibilityError> {
        self.pos += 1;
        self.whitespace();
        if self.consume(b'}') {
            return Ok(());
        }
        let mut members = Vec::new();
        let mut comma_before = None;
        loop {
            self.whitespace();
            let start = self.pos;
            let (key_start, key_end) = self.string()?;
            self.whitespace();
            if !self.consume(b':') {
                return Err(OAuthCompatibilityError::InvalidChecksumInput);
            }
            self.whitespace();
            let key = &self.body[key_start..key_end];
            let excluded = collect
                && matches!(
                    key,
                    b"\"max_tokens\"" | b"\"fallbacks\"" | b"\"fallback_credit_token\""
                );
            if collect && key == b"\"model\"" && self.body.get(self.pos) == Some(&b'"') {
                let (start, end) = self.string()?;
                self.edit(start + 1, end - 1);
            } else {
                self.value(collect && !excluded)?;
            }
            let end = self.pos;
            self.whitespace();
            let comma_after = self.consume(b',').then_some(self.pos - 1);
            members.push(Member {
                start,
                end,
                comma_before,
                comma_after,
                excluded,
            });
            if comma_after.is_some() {
                comma_before = comma_after;
                continue;
            }
            if !self.consume(b'}') {
                return Err(OAuthCompatibilityError::InvalidChecksumInput);
            }
            break;
        }
        if collect {
            let mut start = 0;
            while start < members.len() {
                if !members[start].excluded {
                    start += 1;
                    continue;
                }
                let mut end = start;
                while end + 1 < members.len() && members[end + 1].excluded {
                    end += 1;
                }
                if end + 1 < members.len() {
                    self.edit(
                        members[start].start,
                        members[end].comma_after.unwrap_or(0) + 1,
                    );
                } else if start > 0 && end == start {
                    self.edit(members[start].comma_before.unwrap_or(0), members[end].end);
                } else {
                    // Native leaves the preceding comma for a trailing run of >1 members.
                    self.edit(members[start].start, members[end].end);
                }
                start = end + 1;
            }
        }
        Ok(())
    }

    fn array(&mut self, collect: bool) -> Result<(), OAuthCompatibilityError> {
        self.pos += 1;
        self.whitespace();
        if self.consume(b']') {
            return Ok(());
        }
        loop {
            self.value(collect)?;
            self.whitespace();
            if self.consume(b',') {
                continue;
            }
            if !self.consume(b']') {
                return Err(OAuthCompatibilityError::InvalidChecksumInput);
            }
            return Ok(());
        }
    }

    fn string(&mut self) -> Result<(usize, usize), OAuthCompatibilityError> {
        if !self.consume(b'"') {
            return Err(OAuthCompatibilityError::InvalidChecksumInput);
        }
        let start = self.pos - 1;
        while let Some(b) = self.body.get(self.pos) {
            match b {
                b'\\' => self.pos += 2,
                b'"' => {
                    self.pos += 1;
                    return Ok((start, self.pos));
                }
                _ => self.pos += 1,
            }
        }
        Err(OAuthCompatibilityError::InvalidChecksumInput)
    }

    fn edit(&mut self, start: usize, end: usize) {
        if start < end {
            self.edits.push((start, end));
        }
    }

    fn whitespace(&mut self) {
        while self.body.get(self.pos).is_some_and(u8::is_ascii_whitespace) {
            self.pos += 1;
        }
    }

    fn consume(&mut self, byte: u8) -> bool {
        if self.body.get(self.pos) != Some(&byte) {
            return false;
        }
        self.pos += 1;
        true
    }
}

// Standard XXH64 arithmetic, implemented here to keep the pinned CCH algorithm explicit.
// The fixture vectors below are from CLIProxyAPI, not generated by this implementation.
fn xxhash64(bytes: &[u8], seed: u64) -> u64 {
    const P1: u64 = 11_400_714_785_074_694_791;
    const P2: u64 = 14_029_467_366_897_019_727;
    const P3: u64 = 1_609_587_929_392_839_161;
    const P4: u64 = 9_650_029_242_287_828_579;
    const P5: u64 = 2_870_177_450_012_600_261;
    fn round(acc: u64, lane: u64) -> u64 {
        acc.wrapping_add(lane.wrapping_mul(P2))
            .rotate_left(31)
            .wrapping_mul(P1)
    }
    fn read64(bytes: &[u8]) -> u64 {
        let mut array = [0; 8];
        array.copy_from_slice(&bytes[..8]);
        u64::from_le_bytes(array)
    }
    let mut pos = 0;
    let mut hash = if bytes.len() >= 32 {
        let mut lanes = [
            seed.wrapping_add(P1).wrapping_add(P2),
            seed.wrapping_add(P2),
            seed,
            seed.wrapping_sub(P1),
        ];
        while pos + 32 <= bytes.len() {
            for lane in &mut lanes {
                *lane = round(*lane, read64(&bytes[pos..]));
                pos += 8;
            }
        }
        let mut hash = lanes[0]
            .rotate_left(1)
            .wrapping_add(lanes[1].rotate_left(7))
            .wrapping_add(lanes[2].rotate_left(12))
            .wrapping_add(lanes[3].rotate_left(18));
        for lane in lanes {
            hash = (hash ^ round(0, lane)).wrapping_mul(P1).wrapping_add(P4);
        }
        hash
    } else {
        seed.wrapping_add(P5)
    };
    hash = hash.wrapping_add(bytes.len() as u64);
    while pos + 8 <= bytes.len() {
        hash = (hash ^ round(0, read64(&bytes[pos..])))
            .rotate_left(27)
            .wrapping_mul(P1)
            .wrapping_add(P4);
        pos += 8;
    }
    if pos + 4 <= bytes.len() {
        let mut array = [0; 4];
        array.copy_from_slice(&bytes[pos..pos + 4]);
        hash = (hash ^ u64::from(u32::from_le_bytes(array)).wrapping_mul(P1))
            .rotate_left(23)
            .wrapping_mul(P2)
            .wrapping_add(P3);
        pos += 4;
    }
    for byte in &bytes[pos..] {
        hash = (hash ^ u64::from(*byte).wrapping_mul(P5))
            .rotate_left(11)
            .wrapping_mul(P1);
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(P2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(P3);
    hash ^ (hash >> 32)
}
