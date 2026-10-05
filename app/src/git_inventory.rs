//! Internal Git tree protocol budgets, independent of task logs and results.
use std::io;

pub(crate) const MAX_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_ENTRIES: usize = 50_000;
pub(crate) const MAX_PATH_BYTES: usize = 4096;
const MAX_HEADER_BYTES: usize = 6 + 1 + 4 + 1 + 64;

#[derive(Default)]
pub(crate) struct Framing {
    bytes: usize,
    entries: usize,
    header: usize,
    path: Option<usize>,
}
impl Framing {
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.bytes += bytes.len();
        if self.bytes > MAX_BYTES {
            return Err(io::Error::other("Git inventory exceeds 4 MiB byte limit"));
        }
        for byte in bytes {
            match (*byte, self.path.as_mut()) {
                (0, Some(path)) if *path > 0 => {
                    self.entries += 1;
                    if self.entries > MAX_ENTRIES {
                        return Err(io::Error::other("Git inventory exceeds 50000 entry limit"));
                    }
                    self.header = 0;
                    self.path = None;
                }
                (0, _) => return Err(io::Error::other("malformed Git inventory record")),
                (b'\t', None) => self.path = Some(0),
                (_, Some(path)) => {
                    *path += 1;
                    if *path > MAX_PATH_BYTES {
                        return Err(io::Error::other(
                            "Git inventory path exceeds 4096 byte limit",
                        ));
                    }
                }
                (_, None) => {
                    self.header += 1;
                    if self.header > MAX_HEADER_BYTES {
                        return Err(io::Error::other("malformed Git inventory header"));
                    }
                }
            }
        }
        Ok(())
    }
    pub(crate) fn finish(&self) -> io::Result<()> {
        if self.header != 0 || self.path.is_some() {
            Err(io::Error::other("incomplete NUL-delimited Git inventory"))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(path_bytes: usize) -> Vec<u8> {
        format!(
            "100644 blob {}\t{}\0",
            "a".repeat(40),
            "x".repeat(path_bytes)
        )
        .into_bytes()
    }
    #[test]
    fn incremental_exact_byte_and_entry_limits() {
        let full = record(MAX_PATH_BYTES);
        let mut data = full.repeat(MAX_BYTES / full.len());
        let remainder = MAX_BYTES - data.len();
        assert!(remainder > 54);
        data.extend(record(remainder - 54));
        assert_eq!(data.len(), MAX_BYTES);
        let mut framing = Framing::default();
        for chunk in data.chunks(17) {
            framing.feed(chunk).unwrap();
        }
        framing.finish().unwrap();
        assert!(framing.feed(b"x").is_err());
        let mut framing = Framing::default();
        let entry = record(1);
        for _ in 0..MAX_ENTRIES {
            framing.feed(&entry).unwrap();
        }
        framing.finish().unwrap();
        assert!(framing.feed(&entry).is_err());
    }
    #[test]
    fn incomplete_empty_and_overlong_records_fail() {
        for data in [b"\0".as_slice(), b"header\t\0", b"header", b"header\tpath"] {
            let mut framing = Framing::default();
            assert!(framing.feed(data).and_then(|_| framing.finish()).is_err());
        }
        assert!(
            Framing::default()
                .feed(&record(MAX_PATH_BYTES + 1))
                .is_err()
        );
        assert!(
            Framing::default()
                .feed(&[b'x'; MAX_HEADER_BYTES + 1])
                .is_err()
        );
    }
}
