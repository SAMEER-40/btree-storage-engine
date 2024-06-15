use crate::disk::page::crc32;

/// Log Sequence Number — monotonically increasing identifier for each log record.
pub type Lsn = u64;

/// Types of WAL log records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LogRecordType {
    /// Invalid / padding
    Invalid = 0,
    /// Transaction begin
    Begin = 1,
    /// Transaction commit
    Commit = 2,
    /// Transaction abort
    Abort = 3,
    /// Page write: contains before-image and after-image for undo/redo
    PageWrite = 4,
    /// Checkpoint record
    Checkpoint = 5,
    /// Compensation Log Record (for undo operations during recovery)
    Clr = 6,
}

impl From<u8> for LogRecordType {
    fn from(v: u8) -> Self {
        match v {
            1 => LogRecordType::Begin,
            2 => LogRecordType::Commit,
            3 => LogRecordType::Abort,
            4 => LogRecordType::PageWrite,
            5 => LogRecordType::Checkpoint,
            6 => LogRecordType::Clr,
            _ => LogRecordType::Invalid,
        }
    }
}

/// Helper: read a u32 from a byte slice at a given offset (little-endian)
fn read_u32_le(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

/// Helper: read a u64 from a byte slice at a given offset (little-endian)
fn read_u64_le(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        buf[off],
        buf[off + 1],
        buf[off + 2],
        buf[off + 3],
        buf[off + 4],
        buf[off + 5],
        buf[off + 6],
        buf[off + 7],
    ])
}

/// A single WAL log record.
///
/// On-disk format:
/// ```text
/// [total_len: u32]      — total byte length of the entire record
/// [checksum: u32]       — CRC32 of everything after this field
/// [lsn: u64]            — log sequence number
/// [txn_id: u64]         — transaction ID
/// [prev_lsn: u64]       — previous LSN for this transaction (for undo chain)
/// [record_type: u8]     — type tag
/// [page_id: u32]        — affected page (0 for non-page records)
/// [data_len: u32]       — length of data payload
/// [data: [u8]]          — payload (before/after images for PageWrite)
/// ```
#[derive(Debug, Clone)]
pub struct LogRecord {
    pub lsn: Lsn,
    pub txn_id: u64,
    pub prev_lsn: Lsn,
    pub record_type: LogRecordType,
    pub page_id: u32,
    pub data: Vec<u8>, // For PageWrite: [before_image_len: u32][before_image][after_image]
}

/// Fixed header size: total_len(4) + checksum(4) + lsn(8) + txn_id(8) + prev_lsn(8) + type(1) + page_id(4) + data_len(4) = 41
const RECORD_HEADER_SIZE: usize = 41;

impl LogRecord {
    pub fn new(
        lsn: Lsn,
        txn_id: u64,
        prev_lsn: Lsn,
        record_type: LogRecordType,
        page_id: u32,
        data: Vec<u8>,
    ) -> Self {
        LogRecord {
            lsn,
            txn_id,
            prev_lsn,
            record_type,
            page_id,
            data,
        }
    }

    /// Create a Begin record
    pub fn begin(lsn: Lsn, txn_id: u64) -> Self {
        Self::new(lsn, txn_id, 0, LogRecordType::Begin, 0, Vec::new())
    }

    /// Create a Commit record
    pub fn commit(lsn: Lsn, txn_id: u64, prev_lsn: Lsn) -> Self {
        Self::new(lsn, txn_id, prev_lsn, LogRecordType::Commit, 0, Vec::new())
    }

    /// Create an Abort record
    pub fn abort(lsn: Lsn, txn_id: u64, prev_lsn: Lsn) -> Self {
        Self::new(lsn, txn_id, prev_lsn, LogRecordType::Abort, 0, Vec::new())
    }

    /// Create a PageWrite record with before and after images
    pub fn page_write(
        lsn: Lsn,
        txn_id: u64,
        prev_lsn: Lsn,
        page_id: u32,
        before_image: &[u8],
        after_image: &[u8],
    ) -> Self {
        let mut data = Vec::with_capacity(4 + before_image.len() + after_image.len());
        data.extend_from_slice(&(before_image.len() as u32).to_le_bytes());
        data.extend_from_slice(before_image);
        data.extend_from_slice(after_image);

        Self::new(
            lsn,
            txn_id,
            prev_lsn,
            LogRecordType::PageWrite,
            page_id,
            data,
        )
    }

    /// Create a Checkpoint record with a list of active transaction IDs
    pub fn checkpoint(lsn: Lsn, active_txns: &[u64]) -> Self {
        let mut data = Vec::with_capacity(4 + active_txns.len() * 8);
        data.extend_from_slice(&(active_txns.len() as u32).to_le_bytes());
        for &txn_id in active_txns {
            data.extend_from_slice(&txn_id.to_le_bytes());
        }
        Self::new(lsn, 0, 0, LogRecordType::Checkpoint, 0, data)
    }

    /// Extract before and after images from a PageWrite record
    pub fn page_write_images(&self) -> Option<(&[u8], &[u8])> {
        if self.record_type != LogRecordType::PageWrite || self.data.len() < 4 {
            return None;
        }
        let before_len =
            u32::from_le_bytes([self.data[0], self.data[1], self.data[2], self.data[3]]) as usize;
        let before = &self.data[4..4 + before_len];
        let after = &self.data[4 + before_len..];
        Some((before, after))
    }

    /// Serialize this record to bytes.
    pub fn serialize(&self) -> Vec<u8> {
        let total_len = RECORD_HEADER_SIZE + self.data.len();
        let mut buf = Vec::with_capacity(total_len);

        buf.extend_from_slice(&(total_len as u32).to_le_bytes()); // total_len
        buf.extend_from_slice(&0u32.to_le_bytes()); // placeholder for checksum
        buf.extend_from_slice(&self.lsn.to_le_bytes());
        buf.extend_from_slice(&self.txn_id.to_le_bytes());
        buf.extend_from_slice(&self.prev_lsn.to_le_bytes());
        buf.push(self.record_type as u8);
        buf.extend_from_slice(&self.page_id.to_le_bytes());
        buf.extend_from_slice(&(self.data.len() as u32).to_le_bytes());
        buf.extend_from_slice(&self.data);

        // Compute checksum over everything after the checksum field (offset 8..)
        let checksum = crc32(&buf[8..]);
        buf[4..8].copy_from_slice(&checksum.to_le_bytes());

        buf
    }

    /// Deserialize a record from bytes. Returns (record, bytes_consumed).
    pub fn deserialize(buf: &[u8]) -> crate::Result<(Self, usize)> {
        if buf.len() < RECORD_HEADER_SIZE {
            return Err(crate::Error::WalCorrupted(
                "Buffer too small for record header".to_string(),
            ));
        }

        let total_len = read_u32_le(buf, 0) as usize;
        if buf.len() < total_len {
            return Err(crate::Error::WalCorrupted(format!(
                "Buffer too small: need {} bytes, have {}",
                total_len,
                buf.len()
            )));
        }

        let stored_checksum = read_u32_le(buf, 4);
        let computed_checksum = crc32(&buf[8..total_len]);
        if stored_checksum != computed_checksum {
            return Err(crate::Error::ChecksumMismatch {
                expected: stored_checksum,
                actual: computed_checksum,
            });
        }

        let lsn = read_u64_le(buf, 8);
        let txn_id = read_u64_le(buf, 16);
        let prev_lsn = read_u64_le(buf, 24);
        let record_type = LogRecordType::from(buf[32]);
        let page_id = read_u32_le(buf, 33);
        let data_len = read_u32_le(buf, 37) as usize;

        let data = buf[41..41 + data_len].to_vec();

        let record = LogRecord {
            lsn,
            txn_id,
            prev_lsn,
            record_type,
            page_id,
            data,
        };

        Ok((record, total_len))
    }

    /// Total serialized size
    pub fn serialized_size(&self) -> usize {
        RECORD_HEADER_SIZE + self.data.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_deserialize_begin() {
        let record = LogRecord::begin(1, 100);
        let bytes = record.serialize();
        let (decoded, consumed) = LogRecord::deserialize(&bytes).unwrap();

        assert_eq!(consumed, bytes.len());
        assert_eq!(decoded.lsn, 1);
        assert_eq!(decoded.txn_id, 100);
        assert_eq!(decoded.record_type, LogRecordType::Begin);
    }

    #[test]
    fn test_serialize_deserialize_page_write() {
        let before = b"old data here";
        let after = b"new data here!";
        let record = LogRecord::page_write(5, 200, 3, 42, before, after);
        let bytes = record.serialize();
        let (decoded, _) = LogRecord::deserialize(&bytes).unwrap();

        assert_eq!(decoded.lsn, 5);
        assert_eq!(decoded.txn_id, 200);
        assert_eq!(decoded.prev_lsn, 3);
        assert_eq!(decoded.page_id, 42);

        let (b, a) = decoded.page_write_images().unwrap();
        assert_eq!(b, before);
        assert_eq!(a, after);
    }

    #[test]
    fn test_checksum_detects_corruption() {
        let record = LogRecord::begin(1, 100);
        let mut bytes = record.serialize();
        // Corrupt a byte
        bytes[10] ^= 0xFF;
        let result = LogRecord::deserialize(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn test_checkpoint_record() {
        let active = vec![10, 20, 30];
        let record = LogRecord::checkpoint(99, &active);
        let bytes = record.serialize();
        let (decoded, _) = LogRecord::deserialize(&bytes).unwrap();

        assert_eq!(decoded.lsn, 99);
        assert_eq!(decoded.record_type, LogRecordType::Checkpoint);

        // Parse active txns from data
        let num = u32::from_le_bytes([
            decoded.data[0],
            decoded.data[1],
            decoded.data[2],
            decoded.data[3],
        ]) as usize;
        assert_eq!(num, 3);
    }
}
