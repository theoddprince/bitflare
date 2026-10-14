//! A disc image described as an ordered list of chunks, streamed on demand so a
//! 25-100 GB compilation never has to exist as a temporary file.

use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;

pub const SECTOR: u64 = 2048;

#[derive(Debug, Clone)]
pub enum Chunk {
    /// Literal bytes; always a whole number of sectors.
    Data(Vec<u8>),
    /// Contents of a file, zero-padded to the next sector boundary.
    File { path: PathBuf, len: u64 },
    /// Zero-filled sectors.
    Zero(u64),
}

impl Chunk {
    pub fn sectors(&self) -> u64 {
        match self {
            Chunk::Data(d) => d.len() as u64 / SECTOR,
            Chunk::File { len, .. } => len.div_ceil(SECTOR),
            Chunk::Zero(n) => *n,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Image {
    pub chunks: Vec<Chunk>,
    pub files: u64,
    pub directories: u64,
}

impl Image {
    pub fn sectors(&self) -> u64 {
        self.chunks.iter().map(Chunk::sectors).sum()
    }

    pub fn bytes(&self) -> u64 {
        self.sectors() * SECTOR
    }

    pub fn push(&mut self, c: Chunk) {
        if let Chunk::Data(d) = &c {
            debug_assert_eq!(d.len() as u64 % SECTOR, 0);
        }
        if c.sectors() > 0 {
            self.chunks.push(c);
        }
    }

    pub fn reader(&self) -> ImageReader<'_> {
        ImageReader {
            image: self,
            index: 0,
            pos: 0,
            file: None,
        }
    }
}

/// Produces the image byte stream. A source file that changed size since the image
/// was laid out is reported as an error rather than silently producing a corrupt disc.
pub struct ImageReader<'a> {
    image: &'a Image,
    index: usize,
    /// Byte offset inside the current chunk.
    pos: u64,
    file: Option<File>,
}

impl ImageReader<'_> {
    /// Fills `buf` completely unless the image ends first; returns the bytes written.
    pub fn fill(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut n = 0;
        while n < buf.len() {
            let r = self.read(&mut buf[n..])?;
            if r == 0 {
                break;
            }
            n += r;
        }
        Ok(n)
    }
}

impl Read for ImageReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let Some(chunk) = self.image.chunks.get(self.index) else {
                return Ok(0);
            };
            let total = chunk.sectors() * SECTOR;
            if self.pos >= total {
                self.index += 1;
                self.pos = 0;
                self.file = None;
                continue;
            }
            let want = (buf.len() as u64).min(total - self.pos) as usize;
            let out = &mut buf[..want];
            match chunk {
                Chunk::Data(d) => out.copy_from_slice(&d[self.pos as usize..self.pos as usize + want]),
                Chunk::Zero(_) => out.fill(0),
                Chunk::File { path, len } => {
                    if self.pos < *len {
                        if self.file.is_none() {
                            self.file = Some(File::open(path)?);
                        }
                        let take = ((*len - self.pos) as usize).min(want);
                        let f = self.file.as_mut().unwrap();
                        let got = f.read(&mut out[..take])?;
                        if got == 0 {
                            return Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                format!("{} became shorter while it was being written", path.display()),
                            ));
                        }
                        self.pos += got as u64;
                        return Ok(got);
                    }
                    out.fill(0);
                }
            }
            self.pos += want as u64;
            return Ok(want);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_chunks_with_padding() {
        let dir = std::env::temp_dir().join(format!("bc-fs-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.bin");
        std::fs::write(&f, b"hello").unwrap();

        let mut img = Image::default();
        img.push(Chunk::Data(vec![7u8; 2048]));
        img.push(Chunk::File { path: f.clone(), len: 5 });
        img.push(Chunk::Zero(1));
        assert_eq!(img.sectors(), 3);

        let mut out = Vec::new();
        img.reader().read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), 3 * 2048);
        assert!(out[..2048].iter().all(|&b| b == 7));
        assert_eq!(&out[2048..2053], b"hello");
        assert!(out[2053..].iter().all(|&b| b == 0));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
