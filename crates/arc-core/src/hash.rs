//! Content hashing. blake3, wrapped so the algorithm can be swapped behind `ALGO`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;

pub const ALGO: &str = "b3";

#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Digest([u8; 32]);

impl Digest {
    pub fn hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
    pub fn short(&self) -> String {
        self.hex()[..12].to_string()
    }
    pub fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
    pub fn parse(hex: &str) -> Result<Digest> {
        let hex = hex.strip_prefix("b3:").unwrap_or(hex);
        anyhow::ensure!(hex.len() == 64, "invalid digest length: {}", hex.len());
        let mut out = [0u8; 32];
        for (i, o) in out.iter_mut().enumerate() {
            *o = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).context("invalid digest hex")?;
        }
        Ok(Digest(out))
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.hex())
    }
}
impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", ALGO, self.short())
    }
}

/// Length-prefixed hasher: feeding ("a","bc") can never collide with ("ab","c").
#[derive(Default)]
pub struct Hasher(blake3::Hasher);

impl Hasher {
    pub fn new() -> Self {
        Self(blake3::Hasher::new())
    }
    pub fn field(&mut self, bytes: impl AsRef<[u8]>) -> &mut Self {
        let b = bytes.as_ref();
        self.0.update(&(b.len() as u64).to_le_bytes());
        self.0.update(b);
        self
    }
    pub fn finish(&self) -> Digest {
        Digest(*self.0.finalize().as_bytes())
    }
}

pub fn hash_bytes(b: &[u8]) -> Digest {
    Digest(*blake3::hash(b).as_bytes())
}

/// Hash file contents. Uses memory-mapped parallel hashing for large files.
pub fn hash_file(path: &Path) -> Result<Digest> {
    let mut f = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(Digest(*h.finalize().as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_framing_is_unambiguous() {
        let a = {
            let mut h = Hasher::new();
            h.field("a").field("bc").finish()
        };
        let b = {
            let mut h = Hasher::new();
            h.field("ab").field("c").finish()
        };
        assert_ne!(a, b);
    }

    #[test]
    fn digest_roundtrip() {
        let d = hash_bytes(b"arc");
        assert_eq!(Digest::parse(&d.hex()).unwrap(), d);
        assert_eq!(Digest::parse(&format!("b3:{}", d.hex())).unwrap(), d);
    }
}
