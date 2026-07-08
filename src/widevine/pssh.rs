//! Extract the Widevine init data from a PSSH box (AUD-56a).
//!
//! An MPD's `<cenc:pssh>` carries a base64 PSSH box; the license challenge
//! needs the box's *data* field (the `WidevinePsshData` / cenc header). If the
//! input is already raw init data (no `pssh` box header), it is returned as-is.

/// Error parsing a PSSH box.
#[derive(Debug, thiserror::Error)]
#[error("malformed PSSH box")]
pub struct PsshError;

/// Returns the init data of a PSSH box, or the input unchanged when it is not a
/// box (already the raw `WidevinePsshData`).
pub fn init_data(bytes: &[u8]) -> Result<Vec<u8>, PsshError> {
    let mut cur = Cursor::new(bytes);
    let _box_size = cur.u32()?;
    if cur.take(4)? != b"pssh" {
        // Not a PSSH box — assume the bytes are already the init data.
        return Ok(bytes.to_vec());
    }
    let version = cur.u8()?;
    let _flags = cur.take(3)?;
    let _system_id = cur.take(16)?;
    if version > 0 {
        let kid_count = cur.u32()? as usize;
        cur.take(kid_count.checked_mul(16).ok_or(PsshError)?)?;
    }
    let data_size = cur.u32()? as usize;
    Ok(cur.take(data_size)?.to_vec())
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], PsshError> {
        let end = self.pos.checked_add(n).ok_or(PsshError)?;
        let slice = self.bytes.get(self.pos..end).ok_or(PsshError)?;
        self.pos = end;
        Ok(slice)
    }
    fn u8(&mut self) -> Result<u8, PsshError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, PsshError> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_v0_box_data() {
        // size, 'pssh', version 0, flags, 16-byte system id, data_size, data.
        let data = b"WIDEVINE_INIT_DATA";
        let mut boxx = Vec::new();
        boxx.extend_from_slice(&0u32.to_be_bytes()); // size (unused)
        boxx.extend_from_slice(b"pssh");
        boxx.push(0); // version
        boxx.extend_from_slice(&[0, 0, 0]); // flags
        boxx.extend_from_slice(&[0xed; 16]); // system id
        boxx.extend_from_slice(&(data.len() as u32).to_be_bytes());
        boxx.extend_from_slice(data);
        assert_eq!(init_data(&boxx).unwrap(), data);
    }

    #[test]
    fn passes_through_non_box() {
        let raw = b"\x00\x00\x00\x10not-a-pssh-box";
        assert_eq!(init_data(raw).unwrap(), raw);
    }
}
