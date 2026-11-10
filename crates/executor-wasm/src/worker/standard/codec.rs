//! Decode bounded binary values shared with the generated WIT dispatcher.
use super::invalid;

/// Little-endian reader over one guest packet.
pub(super) struct Reader<'a>(pub(super) &'a [u8]);

impl<'a> Reader<'a> {
    /// Take exactly `length` bytes.
    pub(super) fn take(&mut self, length: usize) -> wasmtime::Result<&'a [u8]> {
        if self.0.len() < length {
            return Err(invalid("standard packet is truncated"));
        }
        let (head, tail) = self.0.split_at(length);
        self.0 = tail;
        Ok(head)
    }

    /// Read one byte.
    pub(super) fn u8(&mut self) -> wasmtime::Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// Read one little-endian 32-bit field.
    pub(super) fn u32(&mut self) -> wasmtime::Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("four bytes"),
        ))
    }

    /// Read one exact little-endian 64-bit field.
    pub(super) fn u64(&mut self) -> wasmtime::Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("eight bytes"),
        ))
    }

    /// Read a length-prefixed raw byte slice.
    pub(super) fn bytes(&mut self) -> wasmtime::Result<&'a [u8]> {
        let length = self.u32()? as usize;
        self.take(length)
    }

    /// Read a length-prefixed UTF-8 string.
    pub(super) fn string(&mut self) -> wasmtime::Result<String> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| invalid("string is not UTF-8"))
    }

    /// Refuse trailing bytes so one packet has exactly one meaning.
    pub(super) fn end(&self) -> wasmtime::Result<()> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(invalid("standard packet has trailing bytes"))
        }
    }
}

/// Little-endian writer of one host reply.
#[derive(Default)]
pub(super) struct Writer(pub(super) Vec<u8>);

impl Writer {
    /// Append one byte.
    pub(super) fn u8(&mut self, value: u8) {
        self.0.push(value);
    }

    /// Append one little-endian 32-bit field.
    pub(super) fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    /// Append one exact little-endian 64-bit field.
    pub(super) fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    /// Append raw bytes without text conversion.
    pub(super) fn raw(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    /// Append a length-prefixed byte slice.
    pub(super) fn bytes(&mut self, bytes: &[u8]) {
        self.u32(bytes.len() as u32);
        self.0.extend_from_slice(bytes);
    }
}
