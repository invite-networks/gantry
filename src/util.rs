//! Small helpers shared across modules.

use crate::errors::{Context, Result};
use std::io::Write;
use std::path::Path;

/// FNV-1a, 64 bit. Stable across builds, used for config hashes and MACs.
pub fn fnv1a64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

pub fn hash_hex(data: &[u8]) -> String {
    format!("{:016x}", fnv1a64(data))
}

/// A random hex identifier from the kernel's CSPRNG.
pub fn random_id(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    let mut filled = 0;
    while filled < buf.len() {
        // SAFETY: the pointer and length describe the unfilled tail of buf.
        let n =
            unsafe { libc::getrandom(buf[filled..].as_mut_ptr().cast(), buf.len() - filled, 0) };
        if n > 0 {
            filled += n as usize;
        } else if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            // getrandom only fails on ancient kernels; fall back to a
            // time and pid mix rather than zeros.
            let seed = format!(
                "{:?}{}{filled}",
                std::time::SystemTime::now(),
                std::process::id()
            );
            let h = fnv1a64(seed.as_bytes()).to_le_bytes();
            for (i, b) in buf[filled..].iter_mut().enumerate() {
                *b = h[i % 8];
            }
            break;
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Writes a file atomically: write to a sibling temp file, then rename.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp).ctx(|| format!("create {}", tmp.display()))?;
        f.write_all(data)
            .ctx(|| format!("write {}", tmp.display()))?;
        f.sync_all().ctx(|| format!("sync {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).ctx(|| format!("rename {}", path.display()))
}

/// Rewrites a file in place, keeping its inode. Bind mounts of the file
/// inside containers (resolv.conf, hosts) see the new content this way.
pub fn write_in_place(path: &Path, data: &[u8]) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .ctx(|| format!("open {}", path.display()))?;
    f.write_all(data)
        .ctx(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_known_values() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(random_id(4).len(), 8);
    }
}
