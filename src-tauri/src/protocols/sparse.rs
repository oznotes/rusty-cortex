//! Android sparse image detection and decompression.
//!
//! Sparse format uses magic 0xED26FF3A with chunk types:
//! RAW (0xCAC1), FILL (0xCAC2), DONT_CARE (0xCAC3), CRC32 (0xCAC4).

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::FlashError;

const SPARSE_MAGIC: u32 = 0xED26FF3A;
const SPARSE_HEADER_SIZE: usize = 28;
const CHUNK_HEADER_SIZE: usize = 12;
const CHUNK_RAW: u16 = 0xCAC1;
const CHUNK_FILL: u16 = 0xCAC2;
const CHUNK_DONT_CARE: u16 = 0xCAC3;
const CHUNK_CRC32: u16 = 0xCAC4;

/// Check whether the stream starts with the Android sparse image magic.
///
/// Reads the first 4 bytes, compares to `SPARSE_MAGIC`, then seeks back to
/// the original position.  Returns `false` (not an error) when the stream is
/// shorter than 4 bytes.
pub fn is_sparse<R: Read + Seek>(reader: &mut R) -> Result<bool, FlashError> {
    let start = reader.stream_position()?;
    let mut magic_buf = [0u8; 4];
    match reader.read_exact(&mut magic_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            // Stream shorter than 4 bytes — not sparse, seek back as far as
            // possible (might already be at 0).
            let _ = reader.seek(SeekFrom::Start(start));
            return Ok(false);
        }
        Err(e) => return Err(FlashError::Io(e)),
    }
    reader.seek(SeekFrom::Start(start))?;
    Ok(u32::from_le_bytes(magic_buf) == SPARSE_MAGIC)
}

/// Decompress an Android sparse image from `reader` into `writer`.
///
/// The reader must be positioned at the start of the sparse header (byte 0).
pub fn decompress_sparse<R: Read + Seek, W: Write + Seek>(
    reader: &mut R,
    writer: &mut W,
) -> Result<(), FlashError> {
    // --- Read sparse header (28 bytes) ---
    let mut hdr = [0u8; SPARSE_HEADER_SIZE];
    reader.read_exact(&mut hdr)?;

    let magic = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
    if magic != SPARSE_MAGIC {
        return Err(FlashError::Protocol("Not a sparse image".into()));
    }

    let block_size = u32::from_le_bytes(hdr[12..16].try_into().unwrap()) as usize;
    let total_chunks = u32::from_le_bytes(hdr[20..24].try_into().unwrap());

    // --- Process each chunk ---
    for _ in 0..total_chunks {
        let mut chdr = [0u8; CHUNK_HEADER_SIZE];
        reader.read_exact(&mut chdr)?;

        let chunk_type = u16::from_le_bytes(chdr[0..2].try_into().unwrap());
        let chunk_blocks = u32::from_le_bytes(chdr[4..8].try_into().unwrap()) as usize;
        let output_bytes = chunk_blocks * block_size;

        match chunk_type {
            CHUNK_RAW => {
                let mut remaining = output_bytes;
                let mut buf = vec![0u8; block_size.min(65536)];
                while remaining > 0 {
                    let to_read = remaining.min(buf.len());
                    reader.read_exact(&mut buf[..to_read])?;
                    writer.write_all(&buf[..to_read])?;
                    remaining -= to_read;
                }
            }
            CHUNK_FILL => {
                let mut pattern = [0u8; 4];
                reader.read_exact(&mut pattern)?;
                let block = pattern.repeat(block_size / 4);
                for _ in 0..chunk_blocks {
                    writer.write_all(&block)?;
                }
            }
            CHUNK_DONT_CARE => {
                // Write zeros for DONT_CARE regions. These can be large (multi-GB)
                // but the output must be a complete raw image for Firehose streaming.
                // We write in block_size chunks to avoid a single huge allocation.
                let zeros = vec![0u8; block_size];
                for _ in 0..chunk_blocks {
                    writer.write_all(&zeros)?;
                }
            }
            CHUNK_CRC32 => {
                // Skip the 4-byte CRC value.
                let mut crc = [0u8; 4];
                reader.read_exact(&mut crc)?;
            }
            other => {
                return Err(FlashError::Protocol(format!(
                    "Unknown sparse chunk type 0x{:04X}",
                    other
                )));
            }
        }
    }

    Ok(())
}

/// If `path` is an Android sparse image, decompress it to a `.raw.tmp` file in
/// the same directory and return `(temp_path, true)`.  Otherwise return
/// `(original_path, false)`.
///
/// The caller is responsible for deleting the temp file when done.
pub fn ensure_raw_image(path: &Path) -> Result<(PathBuf, bool), FlashError> {
    let mut file = std::fs::File::open(path)?;
    if !is_sparse(&mut file)? {
        return Ok((path.to_path_buf(), false));
    }

    // Build temp path: same directory, original name + ".raw.tmp"
    let tmp_name = format!(
        "{}.raw.tmp",
        path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
    );
    let tmp_path = path.with_file_name(tmp_name);

    let mut out = std::fs::File::create(&tmp_path)?;
    decompress_sparse(&mut file, &mut out)?;

    Ok((tmp_path, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn make_sparse_header(block_size: u32, total_blocks: u32, total_chunks: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&SPARSE_MAGIC.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes()); // major version
        buf.extend_from_slice(&0u16.to_le_bytes()); // minor version
        buf.extend_from_slice(&28u16.to_le_bytes()); // file_hdr_sz
        buf.extend_from_slice(&12u16.to_le_bytes()); // chunk_hdr_sz
        buf.extend_from_slice(&block_size.to_le_bytes());
        buf.extend_from_slice(&total_blocks.to_le_bytes());
        buf.extend_from_slice(&total_chunks.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes()); // image checksum
        buf
    }

    fn make_chunk_header(chunk_type: u16, chunk_sz: u32, data_len: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&chunk_type.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // reserved
        buf.extend_from_slice(&chunk_sz.to_le_bytes());
        buf.extend_from_slice(&(CHUNK_HEADER_SIZE as u32 + data_len).to_le_bytes());
        buf
    }

    #[test]
    fn test_is_sparse_magic() {
        let block_size = 4096u32;
        let mut data = make_sparse_header(block_size, 1, 1);
        // Add a RAW chunk with 1 block of data
        data.extend_from_slice(&make_chunk_header(CHUNK_RAW, 1, block_size));
        data.extend_from_slice(&vec![0xAA; block_size as usize]);

        let mut cursor = Cursor::new(data);
        assert!(is_sparse(&mut cursor).unwrap());
        // Cursor should be back at start
        assert_eq!(cursor.position(), 0);
    }

    #[test]
    fn test_is_not_sparse() {
        // ELF magic
        let data = vec![0x7F, b'E', b'L', b'F', 0, 0, 0, 0];
        let mut cursor = Cursor::new(data);
        assert!(!is_sparse(&mut cursor).unwrap());
        assert_eq!(cursor.position(), 0);
    }

    #[test]
    fn test_is_sparse_too_short() {
        let data = vec![0xED, 0x26];
        let mut cursor = Cursor::new(data);
        assert!(!is_sparse(&mut cursor).unwrap());
    }

    #[test]
    fn test_decompress_raw_chunk() {
        let block_size = 512u32;
        let raw_data: Vec<u8> = (0..block_size).map(|i| (i % 256) as u8).collect();

        let mut sparse = make_sparse_header(block_size, 1, 1);
        sparse.extend_from_slice(&make_chunk_header(CHUNK_RAW, 1, block_size));
        sparse.extend_from_slice(&raw_data);

        let mut reader = Cursor::new(sparse);
        let mut writer = Cursor::new(Vec::new());
        decompress_sparse(&mut reader, &mut writer).unwrap();

        assert_eq!(writer.into_inner(), raw_data);
    }

    #[test]
    fn test_decompress_fill_chunk() {
        let block_size = 512u32;
        let fill_pattern = 0xDEADBEEFu32;

        let mut sparse = make_sparse_header(block_size, 1, 1);
        sparse.extend_from_slice(&make_chunk_header(CHUNK_FILL, 1, 4));
        sparse.extend_from_slice(&fill_pattern.to_le_bytes());

        let mut reader = Cursor::new(sparse);
        let mut writer = Cursor::new(Vec::new());
        decompress_sparse(&mut reader, &mut writer).unwrap();

        let output = writer.into_inner();
        assert_eq!(output.len(), block_size as usize);
        let pattern_bytes = fill_pattern.to_le_bytes();
        for chunk in output.chunks_exact(4) {
            assert_eq!(chunk, &pattern_bytes);
        }
    }

    #[test]
    fn test_decompress_dont_care_chunk() {
        let block_size = 512u32;

        let mut sparse = make_sparse_header(block_size, 1, 1);
        sparse.extend_from_slice(&make_chunk_header(CHUNK_DONT_CARE, 1, 0));

        let mut reader = Cursor::new(sparse);
        let mut writer = Cursor::new(Vec::new());
        decompress_sparse(&mut reader, &mut writer).unwrap();

        let output = writer.into_inner();
        assert_eq!(output.len(), block_size as usize);
        assert!(output.iter().all(|&b| b == 0));
    }

    #[test]
    fn test_decompress_mixed_chunks() {
        let block_size = 256u32;
        let raw_data: Vec<u8> = (0..block_size).map(|i| (i % 256) as u8).collect();
        let fill_pattern = 0xCAFEBABEu32;

        // 3 chunks: RAW + FILL + DONT_CARE = 3 blocks total
        let mut sparse = make_sparse_header(block_size, 3, 3);

        // RAW chunk: 1 block
        sparse.extend_from_slice(&make_chunk_header(CHUNK_RAW, 1, block_size));
        sparse.extend_from_slice(&raw_data);

        // FILL chunk: 1 block
        sparse.extend_from_slice(&make_chunk_header(CHUNK_FILL, 1, 4));
        sparse.extend_from_slice(&fill_pattern.to_le_bytes());

        // DONT_CARE chunk: 1 block
        sparse.extend_from_slice(&make_chunk_header(CHUNK_DONT_CARE, 1, 0));

        let mut reader = Cursor::new(sparse);
        let mut writer = Cursor::new(Vec::new());
        decompress_sparse(&mut reader, &mut writer).unwrap();

        let output = writer.into_inner();
        assert_eq!(output.len(), (block_size * 3) as usize);

        // First block: raw data
        assert_eq!(&output[..block_size as usize], &raw_data[..]);

        // Second block: fill pattern repeated
        let pattern_bytes = fill_pattern.to_le_bytes();
        let fill_start = block_size as usize;
        let fill_end = fill_start + block_size as usize;
        for chunk in output[fill_start..fill_end].chunks_exact(4) {
            assert_eq!(chunk, &pattern_bytes);
        }

        // Third block: zeros
        let zero_start = (block_size * 2) as usize;
        assert!(output[zero_start..].iter().all(|&b| b == 0));
    }

    #[test]
    fn test_ensure_raw_image_already_raw() {
        let dir = tempfile::tempdir().unwrap();
        let raw_path = dir.path().join("boot.img");
        // Write non-sparse data (ELF header)
        std::fs::write(&raw_path, [0x7F, b'E', b'L', b'F', 0, 0, 0, 0]).unwrap();

        let (result_path, was_sparse) = ensure_raw_image(&raw_path).unwrap();
        assert_eq!(result_path, raw_path);
        assert!(!was_sparse);
    }

    #[test]
    fn test_ensure_raw_image_sparse() {
        let dir = tempfile::tempdir().unwrap();
        let sparse_path = dir.path().join("system.img");

        let block_size = 512u32;
        let raw_data: Vec<u8> = (0..block_size).map(|i| (i % 256) as u8).collect();

        let mut sparse = make_sparse_header(block_size, 1, 1);
        sparse.extend_from_slice(&make_chunk_header(CHUNK_RAW, 1, block_size));
        sparse.extend_from_slice(&raw_data);
        std::fs::write(&sparse_path, &sparse).unwrap();

        let (result_path, was_sparse) = ensure_raw_image(&sparse_path).unwrap();
        assert!(was_sparse);
        assert_ne!(result_path, sparse_path);
        assert!(result_path.to_string_lossy().contains(".raw.tmp"));

        // Verify decompressed content
        let decompressed = std::fs::read(&result_path).unwrap();
        assert_eq!(decompressed, raw_data);

        // Clean up temp file
        std::fs::remove_file(&result_path).unwrap();
    }
}
