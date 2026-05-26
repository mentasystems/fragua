//! Minimal USTAR archive writer.
//!
//! We avoid the `tar` crate dependency by emitting the on-disk format
//! directly. USTAR is well-defined: each entry is a 512-byte header
//! followed by content padded to 512 bytes. The archive ends with two
//! 512-byte zero blocks.

use std::io::{self, Write};

const BLOCK: usize = 512;

/// Write every `(path, bytes)` entry to `out` as a USTAR archive.
/// Paths longer than 100 characters are truncated (USTAR header
/// limit) — for our use case the longest path is
/// `<board>/steps/pcb/layers/soldermask_bot/features` which is well
/// under that limit.
pub fn write_archive<W: Write>(out: &mut W, entries: &[(String, Vec<u8>)]) -> io::Result<()> {
    for (path, content) in entries {
        write_entry(out, path, content)?;
    }
    // Two empty blocks mark end-of-archive.
    out.write_all(&[0u8; BLOCK])?;
    out.write_all(&[0u8; BLOCK])?;
    Ok(())
}

fn write_entry<W: Write>(out: &mut W, path: &str, content: &[u8]) -> io::Result<()> {
    let mut header = [0u8; BLOCK];

    // Truncate path to 100 bytes; USTAR has a longer-path "prefix"
    // field but we don't need it.
    let path_bytes = path.as_bytes();
    let n = path_bytes.len().min(100);
    header[..n].copy_from_slice(&path_bytes[..n]);

    // Mode: 0644 (octal).
    write_octal(&mut header[100..108], 0o644, 7);
    // uid / gid 0.
    write_octal(&mut header[108..116], 0, 7);
    write_octal(&mut header[116..124], 0, 7);
    // Size.
    write_octal(&mut header[124..136], content.len() as u64, 11);
    // mtime — fixed 0 for reproducible output.
    write_octal(&mut header[136..148], 0, 11);
    // Type flag '0' = regular file.
    header[156] = b'0';
    // USTAR magic + version.
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");

    // Checksum: temporarily fill with spaces, sum, then write.
    for b in &mut header[148..156] {
        *b = b' ';
    }
    let sum: u32 = header.iter().map(|&b| u32::from(b)).sum();
    let csum = format!("{sum:06o}\0 ");
    let csum_bytes = csum.as_bytes();
    header[148..148 + csum_bytes.len()].copy_from_slice(csum_bytes);

    out.write_all(&header)?;
    out.write_all(content)?;
    // Pad to 512-byte boundary.
    let pad = (BLOCK - (content.len() % BLOCK)) % BLOCK;
    if pad > 0 {
        let zeros = vec![0u8; pad];
        out.write_all(&zeros)?;
    }
    Ok(())
}

fn write_octal(dest: &mut [u8], value: u64, digits: usize) {
    let s = format!("{value:0digits$o}\0");
    let bytes = s.as_bytes();
    let n = bytes.len().min(dest.len());
    dest[..n].copy_from_slice(&bytes[..n]);
}
