//! UDF 2.50 image builder (ECMA-167 3rd edition + OSTA UDF 2.50) for read-only and
//! write-once media.
//!
//! Volume layout, in sectors:
//!
//! ```text
//! 0-15     system area (zero)
//! 16-18    volume recognition sequence: BEA01, NSR03, TEA01
//! 32-37    main volume descriptor sequence: PVD, IUVD, PD, LVD, USD, TD
//! 48-53    reserve volume descriptor sequence (copy)
//! 64-65    logical volume integrity sequence: LVID, TD
//! 256      anchor volume descriptor pointer
//! P...     partition 0:
//!            +0      metadata file EFE
//!            +1      metadata mirror file EFE
//!            +32     metadata partition contents (FSD, directory and file EFEs, FIDs)
//!            +32+M   mirror copy of the metadata
//!            +32+2M  file data
//! last     anchor volume descriptor pointer (volume rounded up to a 32-sector cluster)
//! ```

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, io};

use crate::image::{Chunk, Image, SECTOR};

const BS: usize = SECTOR as usize;
const UDF_REV: u16 = 0x0250;
const TAG_VERSION: u16 = 3;
const PARTITION_START: u32 = 288;
/// Allocation and alignment unit of the metadata partition; one Blu-ray ECC cluster.
const META_UNIT: u32 = 32;
const MAX_EXTENT: u64 = (1 << 30) - SECTOR;
const IMPL_ID: &str = "*burncraft";

const PHYS_REF: u16 = 0;
const META_REF: u16 = 1;

mod tag {
    pub const PVD: u16 = 1;
    pub const AVDP: u16 = 2;
    pub const IUVD: u16 = 4;
    pub const PD: u16 = 5;
    pub const LVD: u16 = 6;
    pub const USD: u16 = 7;
    pub const TD: u16 = 8;
    pub const LVID: u16 = 9;
    pub const FSD: u16 = 256;
    pub const FID: u16 = 257;
    pub const EFE: u16 = 266;
}

mod file_type {
    pub const DIRECTORY: u8 = 4;
    pub const FILE: u8 = 5;
    pub const METADATA: u8 = 250;
    pub const METADATA_MIRROR: u8 = 251;
}

const AD_SHORT: u16 = 0;
const AD_LONG: u16 = 1;

#[derive(Debug, Clone)]
pub struct UdfOptions {
    pub volume_label: String,
    /// Recording time stamped on the volume; fixed so a rebuilt image is byte-identical.
    pub recorded: SystemTime,
}

impl UdfOptions {
    pub fn new(label: &str) -> Self {
        UdfOptions {
            volume_label: label.to_string(),
            recorded: SystemTime::now(),
        }
    }
}

// ---------------------------------------------------------------------------
// Primitive encoders
// ---------------------------------------------------------------------------

fn crc_itu(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn put64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// Fills in the 16-byte descriptor tag; `body_len` is the descriptor length including the tag.
fn finish_tag(b: &mut [u8], id: u16, location: u32, body_len: usize) {
    put16(b, 0, id);
    put16(b, 2, TAG_VERSION);
    b[5] = 0;
    put16(b, 6, 1);
    let crc_len = body_len - 16;
    put16(b, 8, crc_itu(&b[16..16 + crc_len]));
    put16(b, 10, crc_len as u16);
    put32(b, 12, location);
    let sum = b[..16]
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 4)
        .fold(0u8, |a, (_, &x)| a.wrapping_add(x));
    b[4] = sum;
}

/// OSTA CS0: 8-bit when every character fits in Latin-1, otherwise UTF-16BE.
fn cs0(s: &str) -> Vec<u8> {
    if s.is_empty() {
        return Vec::new();
    }
    if s.chars().all(|c| (c as u32) < 0x100) {
        std::iter::once(8u8).chain(s.chars().map(|c| c as u8)).collect()
    } else {
        let mut v = vec![16u8];
        for u in s.encode_utf16() {
            v.extend_from_slice(&u.to_be_bytes());
        }
        v
    }
}

/// Fixed-length d-string: CS0 bytes, truncated to fit, with the used length in the last byte.
fn dstring(b: &mut [u8], at: usize, field: usize, s: &str) {
    let mut enc = cs0(s);
    if enc.len() > field - 1 {
        let unit = if enc.first() == Some(&16) { 2 } else { 1 };
        let keep = 1 + ((field - 2) / unit) * unit;
        enc.truncate(keep);
    }
    b[at..at + enc.len()].copy_from_slice(&enc);
    b[at + field - 1] = enc.len() as u8;
}

fn charspec(b: &mut [u8], at: usize) {
    b[at] = 0;
    let s = b"OSTA Compressed Unicode";
    b[at + 1..at + 1 + s.len()].copy_from_slice(s);
}

fn regid(b: &mut [u8], at: usize, flags: u8, ident: &str, suffix: [u8; 8]) {
    b[at] = flags;
    b[at + 1..at + 1 + ident.len()].copy_from_slice(ident.as_bytes());
    b[at + 24..at + 32].copy_from_slice(&suffix);
}

fn udf_suffix() -> [u8; 8] {
    let r = UDF_REV.to_le_bytes();
    [r[0], r[1], 0, 0, 0, 0, 0, 0]
}

fn impl_regid(b: &mut [u8], at: usize) {
    regid(b, at, 0, IMPL_ID, [0; 8]);
}

fn domain_regid(b: &mut [u8], at: usize) {
    regid(b, at, 0, "*OSTA UDF Compliant", udf_suffix());
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 12-byte ECMA-167 timestamp in UTC.
fn timestamp(b: &mut [u8], at: usize, t: SystemTime) {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (y, mo, day) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    let micros = d.subsec_micros();
    put16(b, at, 0x1000);
    put16(b, at + 2, y as u16);
    b[at + 4] = mo as u8;
    b[at + 5] = day as u8;
    b[at + 6] = (sod / 3600) as u8;
    b[at + 7] = (sod % 3600 / 60) as u8;
    b[at + 8] = (sod % 60) as u8;
    b[at + 9] = (micros / 10_000) as u8;
    b[at + 10] = (micros / 100 % 100) as u8;
    b[at + 11] = (micros % 100) as u8;
}

fn short_ad(b: &mut [u8], at: usize, len: u32, pos: u32) {
    put32(b, at, len);
    put32(b, at + 4, pos);
}

fn long_ad(b: &mut [u8], at: usize, len: u32, lbn: u32, part: u16, unique_id: u32) {
    put32(b, at, len);
    put32(b, at + 4, lbn);
    put16(b, at + 8, part);
    put16(b, at + 10, 0);
    put32(b, at + 12, unique_id);
}

// ---------------------------------------------------------------------------
// Tree
// ---------------------------------------------------------------------------

enum Child {
    Dir(usize),
    File(usize),
}

struct DirRec {
    name: Vec<u8>,
    mtime: SystemTime,
    parent: usize,
    children: Vec<Child>,
    uid: u64,
    efe: u32,
    data: u32,
    data_len: u32,
}

struct FileRec {
    name: Vec<u8>,
    path: PathBuf,
    len: u64,
    mtime: SystemTime,
    uid: u64,
    efe: u32,
    data: u32,
}

#[derive(Default)]
struct Tree {
    dirs: Vec<DirRec>,
    files: Vec<FileRec>,
}

fn name_of(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

fn fid_name(name: &str, path: &Path) -> io::Result<Vec<u8>> {
    let enc = cs0(name);
    if enc.len() > 255 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("name too long for UDF (max 254 Latin-1 or 127 Unicode characters): {}", path.display()),
        ));
    }
    Ok(enc)
}

fn mtime(m: &fs::Metadata) -> SystemTime {
    m.modified().unwrap_or(UNIX_EPOCH)
}

impl Tree {
    fn add_dir(&mut self, name: Vec<u8>, mtime: SystemTime, parent: usize) -> usize {
        self.dirs.push(DirRec {
            name,
            mtime,
            parent,
            children: Vec::new(),
            uid: 0,
            efe: 0,
            data: 0,
            data_len: 0,
        });
        self.dirs.len() - 1
    }

    fn add_path(&mut self, parent: usize, path: &Path, name: String) -> io::Result<()> {
        let meta = fs::metadata(path)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
        let enc = fid_name(&name, path)?;
        let duplicate = self.dirs[parent].children.iter().any(|c| match c {
            Child::Dir(i) => self.dirs[*i].name == enc,
            Child::File(i) => self.files[*i].name == enc,
        });
        if duplicate {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("two items would be named \"{name}\" in the same folder on the disc"),
            ));
        }
        if meta.is_dir() {
            let d = self.add_dir(enc, mtime(&meta), parent);
            self.dirs[parent].children.push(Child::Dir(d));
            let mut entries: Vec<_> = fs::read_dir(path)
                .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?
                .collect::<io::Result<_>>()?;
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let p = e.path();
                self.add_path(d, &p, name_of(&p))?;
            }
        } else {
            self.files.push(FileRec {
                name: enc,
                path: path.to_path_buf(),
                len: meta.len(),
                mtime: mtime(&meta),
                uid: 0,
                efe: 0,
                data: 0,
            });
            let f = self.files.len() - 1;
            self.dirs[parent].children.push(Child::File(f));
        }
        Ok(())
    }
}

fn fid_len(name_len: usize) -> usize {
    (38 + name_len).div_ceil(4) * 4
}

fn write_fid(out: &mut Vec<u8>, block: u32, chars: u8, icb: u32, uid: u64, name: &[u8]) {
    let len = fid_len(name.len());
    let mut b = vec![0u8; len];
    put16(&mut b, 16, 1);
    b[18] = chars;
    b[19] = name.len() as u8;
    long_ad(&mut b, 20, BS as u32, icb, META_REF, uid as u32);
    put16(&mut b, 36, 0);
    b[38..38 + name.len()].copy_from_slice(name);
    finish_tag(&mut b, tag::FID, block, len);
    out.extend_from_slice(&b);
}

struct Efe<'a> {
    file_type: u8,
    ad_type: u16,
    link_count: u16,
    info_len: u64,
    blocks: u64,
    unique_id: u64,
    time: SystemTime,
    ads: &'a [u8],
}

const PERM_READ_EXEC: u32 = 0x1 | 0x4 | 0x20 | 0x80 | 0x400 | 0x1000;

fn write_efe(block: &mut [u8], location: u32, e: &Efe) {
    let b = &mut block[..BS];
    b.fill(0);
    // ICB tag
    put16(b, 20, 4);
    put16(b, 24, 1);
    b[27] = e.file_type;
    put16(b, 34, e.ad_type);
    put32(b, 36, u32::MAX);
    put32(b, 40, u32::MAX);
    put32(b, 44, PERM_READ_EXEC);
    put16(b, 48, e.link_count);
    put64(b, 56, e.info_len);
    put64(b, 64, e.info_len);
    put64(b, 72, e.blocks);
    for at in [80, 92, 104, 116] {
        timestamp(b, at, e.time);
    }
    put32(b, 128, 1);
    impl_regid(b, 168);
    put64(b, 200, e.unique_id);
    put32(b, 208, 0);
    put32(b, 212, e.ads.len() as u32);
    b[216..216 + e.ads.len()].copy_from_slice(e.ads);
    finish_tag(b, tag::EFE, location, 216 + e.ads.len());
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Lays out a UDF 2.50 volume containing `sources` (files and folders) at the disc root.
pub fn build(sources: &[PathBuf], opts: &UdfOptions) -> io::Result<Image> {
    let mut tree = Tree::default();
    tree.add_dir(Vec::new(), opts.recorded, 0);
    for p in sources {
        tree.add_path(0, p, name_of(p))?;
    }

    // Unique IDs: root is 0, everything else starts at 16 (UDF 3.2.1.1).
    let mut next_uid = 16u64;
    for d in tree.dirs.iter_mut().skip(1) {
        d.uid = next_uid;
        next_uid += 1;
    }
    for f in tree.files.iter_mut() {
        f.uid = next_uid;
        next_uid += 1;
    }

    // Metadata partition layout: block 0 is the FSD.
    let mut lbn = 1u32;
    for i in 0..tree.dirs.len() {
        tree.dirs[i].efe = lbn;
        lbn += 1;
        let len: usize = fid_len(0)
            + tree.dirs[i]
                .children
                .iter()
                .map(|c| match c {
                    Child::Dir(d) => fid_len(tree.dirs[*d].name.len()),
                    Child::File(f) => fid_len(tree.files[*f].name.len()),
                })
                .sum::<usize>();
        tree.dirs[i].data = lbn;
        tree.dirs[i].data_len = len as u32;
        lbn += len.div_ceil(BS) as u32;
    }
    for f in tree.files.iter_mut() {
        f.efe = lbn;
        lbn += 1;
    }
    let meta_blocks = lbn.div_ceil(META_UNIT) * META_UNIT;

    // Physical partition layout.
    let meta_start = META_UNIT;
    let mirror_start = meta_start + meta_blocks;
    let mut cur = mirror_start as u64 + meta_blocks as u64;
    for f in tree.files.iter_mut() {
        f.data = u32::try_from(cur).map_err(|_| io::Error::other("compilation too large for UDF"))?;
        cur += f.len.div_ceil(SECTOR);
    }
    let part_len = u32::try_from(cur).map_err(|_| io::Error::other("compilation too large for UDF"))?;

    // Metadata contents.
    let mut meta = vec![0u8; meta_blocks as usize * BS];
    write_fsd(&mut meta[..BS], opts, tree.dirs[0].efe);
    for i in 0..tree.dirs.len() {
        let d = &tree.dirs[i];
        let mut data = Vec::with_capacity(d.data_len as usize);
        let parent = &tree.dirs[d.parent];
        write_fid(&mut data, d.data, 0x0A, parent.efe, parent.uid, &[]);
        for c in &d.children {
            let block = d.data + (data.len() / BS) as u32;
            match c {
                Child::Dir(x) => {
                    let s = &tree.dirs[*x];
                    write_fid(&mut data, block, 0x02, s.efe, s.uid, &s.name);
                }
                Child::File(x) => {
                    let s = &tree.files[*x];
                    write_fid(&mut data, block, 0x00, s.efe, s.uid, &s.name);
                }
            }
        }
        debug_assert_eq!(data.len(), d.data_len as usize);
        let at = d.data as usize * BS;
        meta[at..at + data.len()].copy_from_slice(&data);

        let mut ads = [0u8; 8];
        short_ad(&mut ads, 0, d.data_len, d.data);
        let subdirs = d.children.iter().filter(|c| matches!(c, Child::Dir(_))).count() as u16;
        let efe = Efe {
            file_type: file_type::DIRECTORY,
            ad_type: AD_SHORT,
            link_count: 1 + subdirs,
            info_len: d.data_len as u64,
            blocks: (d.data_len as u64).div_ceil(SECTOR),
            unique_id: d.uid,
            time: d.mtime,
            ads: &ads,
        };
        let at = d.efe as usize * BS;
        write_efe(&mut meta[at..at + BS], d.efe, &efe);
    }
    for f in &tree.files {
        let mut ads = Vec::new();
        let mut left = f.len;
        let mut lbn = f.data;
        while left > 0 {
            let piece = left.min(MAX_EXTENT);
            let mut ad = [0u8; 16];
            long_ad(&mut ad, 0, piece as u32, lbn, PHYS_REF, 0);
            ads.extend_from_slice(&ad);
            lbn += piece.div_ceil(SECTOR) as u32;
            left -= piece;
        }
        if 216 + ads.len() > BS {
            return Err(io::Error::other(format!(
                "{} is too large for a single UDF file entry",
                f.path.display()
            )));
        }
        let efe = Efe {
            file_type: file_type::FILE,
            ad_type: AD_LONG,
            link_count: 1,
            info_len: f.len,
            blocks: f.len.div_ceil(SECTOR),
            unique_id: f.uid,
            time: f.mtime,
            ads: &ads,
        };
        let at = f.efe as usize * BS;
        write_efe(&mut meta[at..at + BS], f.efe, &efe);
    }

    // Partition head: metadata file and mirror file entries.
    let mut head = vec![0u8; META_UNIT as usize * BS];
    for (i, (ft, start)) in [(file_type::METADATA, meta_start), (file_type::METADATA_MIRROR, mirror_start)]
        .into_iter()
        .enumerate()
    {
        let mut ads = [0u8; 8];
        short_ad(&mut ads, 0, meta_blocks * BS as u32, start);
        let efe = Efe {
            file_type: ft,
            ad_type: AD_SHORT,
            link_count: 1,
            info_len: meta_blocks as u64 * SECTOR,
            blocks: meta_blocks as u64,
            unique_id: 0,
            time: opts.recorded,
            ads: &ads,
        };
        write_efe(&mut head[i * BS..(i + 1) * BS], i as u32, &efe);
    }

    // Volume: rounded up to a 32-sector cluster with the closing anchor in the last sector.
    let content_end = PARTITION_START as u64 + part_len as u64;
    let total = (content_end + 1).div_ceil(32) * 32;
    let last = u32::try_from(total - 1).map_err(|_| io::Error::other("compilation too large for UDF"))?;

    let ctx = VolumeCtx {
        opts,
        part_len,
        meta_blocks,
        files: tree.files.len() as u32,
        dirs: tree.dirs.len() as u32,
        next_uid,
    };
    let mut system = vec![0u8; PARTITION_START as usize * BS];
    write_volume_area(&mut system, &ctx);

    let mut img = Image {
        chunks: Vec::new(),
        files: tree.files.len() as u64,
        directories: tree.dirs.len() as u64,
    };
    img.push(Chunk::Data(system));
    img.push(Chunk::Data(head));
    img.push(Chunk::Data(meta.clone()));
    img.push(Chunk::Data(meta));
    for f in &tree.files {
        img.push(Chunk::File { path: f.path.clone(), len: f.len });
    }
    img.push(Chunk::Zero(total - 1 - content_end));
    let mut tail = vec![0u8; BS];
    write_avdp(&mut tail, last);
    img.push(Chunk::Data(tail));
    debug_assert_eq!(img.sectors(), total);
    Ok(img)
}

struct VolumeCtx<'a> {
    opts: &'a UdfOptions,
    part_len: u32,
    meta_blocks: u32,
    files: u32,
    dirs: u32,
    next_uid: u64,
}

fn write_fsd(b: &mut [u8], opts: &UdfOptions, root_efe: u32) {
    timestamp(b, 16, opts.recorded);
    put16(b, 28, 3);
    put16(b, 30, 3);
    put32(b, 32, 1);
    put32(b, 36, 1);
    put32(b, 40, 0);
    put32(b, 44, 0);
    charspec(b, 48);
    dstring(b, 112, 128, &opts.volume_label);
    charspec(b, 240);
    dstring(b, 304, 32, &opts.volume_label);
    long_ad(b, 400, BS as u32, root_efe, META_REF, 0);
    domain_regid(b, 416);
    finish_tag(b, tag::FSD, 0, 512);
}

fn write_avdp(b: &mut [u8], location: u32) {
    put32(b, 16, 16 * BS as u32);
    put32(b, 20, 32);
    put32(b, 24, 16 * BS as u32);
    put32(b, 28, 48);
    finish_tag(b, tag::AVDP, location, 512);
}

fn sector(area: &mut [u8], n: u32) -> &mut [u8] {
    &mut area[n as usize * BS..(n as usize + 1) * BS]
}

fn write_volume_area(area: &mut [u8], c: &VolumeCtx) {
    for (n, id) in [(16, b"BEA01"), (17, b"NSR03"), (18, b"TEA01")] {
        let s = sector(area, n);
        s[0] = 0;
        s[1..6].copy_from_slice(id);
        s[6] = 1;
    }
    for base in [32u32, 48] {
        write_vds(area, base, c);
    }

    let s = sector(area, 64);
    timestamp(s, 16, c.opts.recorded);
    put32(s, 28, 1);
    put64(s, 40, c.next_uid);
    let n = 2usize;
    put32(s, 72, n as u32);
    let l_iu = 46usize;
    put32(s, 76, l_iu as u32);
    put32(s, 80, 0);
    put32(s, 84, 0);
    put32(s, 88, c.part_len);
    put32(s, 92, c.meta_blocks);
    let iu = 80 + 8 * n;
    impl_regid(s, iu);
    put32(s, iu + 32, c.files);
    put32(s, iu + 36, c.dirs);
    put16(s, iu + 40, UDF_REV);
    put16(s, iu + 42, UDF_REV);
    put16(s, iu + 44, UDF_REV);
    finish_tag(s, tag::LVID, 64, iu + l_iu);

    finish_tag(sector(area, 65), tag::TD, 65, 512);
    write_avdp(sector(area, 256), 256);
}

fn write_vds(area: &mut [u8], base: u32, c: &VolumeCtx) {
    let label = &c.opts.volume_label;
    let secs = c.opts.recorded.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();

    // Primary Volume Descriptor
    let loc = base;
    let s = sector(area, loc);
    put32(s, 16, 0);
    put32(s, 20, 0);
    dstring(s, 24, 32, label);
    put16(s, 56, 1);
    put16(s, 58, 1);
    put16(s, 60, 2);
    put16(s, 62, 3);
    put32(s, 64, 1);
    put32(s, 68, 1);
    dstring(s, 72, 128, &format!("{secs:016X}{label}"));
    charspec(s, 200);
    charspec(s, 264);
    regid(s, 344, 0, IMPL_ID, [0; 8]);
    timestamp(s, 376, c.opts.recorded);
    impl_regid(s, 388);
    finish_tag(s, tag::PVD, loc, 512);

    // Implementation Use Volume Descriptor (UDF LV Info)
    let loc = base + 1;
    let s = sector(area, loc);
    put32(s, 16, 1);
    regid(s, 20, 0, "*UDF LV Info", udf_suffix());
    charspec(s, 52);
    dstring(s, 116, 128, label);
    impl_regid(s, 352);
    finish_tag(s, tag::IUVD, loc, 512);

    // Partition Descriptor
    let loc = base + 2;
    let s = sector(area, loc);
    put32(s, 16, 2);
    put16(s, 20, 1);
    put16(s, 22, 0);
    regid(s, 24, 2, "+NSR03", [0; 8]);
    put32(s, 184, 1);
    put32(s, 188, PARTITION_START);
    put32(s, 192, c.part_len);
    impl_regid(s, 196);
    finish_tag(s, tag::PD, loc, 512);

    // Logical Volume Descriptor
    let loc = base + 3;
    let s = sector(area, loc);
    put32(s, 16, 3);
    charspec(s, 20);
    dstring(s, 84, 128, label);
    put32(s, 212, BS as u32);
    domain_regid(s, 216);
    long_ad(s, 248, BS as u32, 0, META_REF, 0);
    let mt_l = 6 + 64;
    put32(s, 264, mt_l);
    put32(s, 268, 2);
    impl_regid(s, 272);
    put32(s, 432, 2 * BS as u32);
    put32(s, 436, 64);
    // Type 1 map: the physical partition.
    let m = 440;
    s[m] = 1;
    s[m + 1] = 6;
    put16(s, m + 2, 1);
    put16(s, m + 4, 0);
    // Type 2 map: the metadata partition.
    let m = 446;
    s[m] = 2;
    s[m + 1] = 64;
    regid(s, m + 4, 0, "*UDF Metadata Partition", udf_suffix());
    put16(s, m + 36, 1);
    put16(s, m + 38, 0);
    put32(s, m + 40, 0);
    put32(s, m + 44, 1);
    put32(s, m + 48, u32::MAX);
    put32(s, m + 52, META_UNIT);
    put16(s, m + 56, META_UNIT as u16);
    s[m + 58] = 1;
    finish_tag(s, tag::LVD, loc, 440 + mt_l as usize);

    // Unallocated Space Descriptor
    let loc = base + 4;
    let s = sector(area, loc);
    put32(s, 16, 4);
    put32(s, 20, 0);
    finish_tag(s, tag::USD, loc, 24);

    // Terminating Descriptor
    let loc = base + 5;
    finish_tag(sector(area, loc), tag::TD, loc, 512);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn crc_matches_reference() {
        // CRC-16/XMODEM check value.
        assert_eq!(crc_itu(b"123456789"), 0x31C3);
    }

    #[test]
    fn cs0_encodings() {
        assert_eq!(cs0("Ab"), vec![8, b'A', b'b']);
        assert_eq!(cs0("ド"), vec![16, 0x30, 0xC9]);
        let mut b = [0u8; 32];
        dstring(&mut b, 0, 32, "DBS");
        assert_eq!(&b[..4], &[8, b'D', b'B', b'S']);
        assert_eq!(b[31], 4);
    }

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_740), (2026, 10, 14));
    }

    fn tag_ok(b: &[u8]) -> bool {
        let sum = b[..16]
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 4)
            .fold(0u8, |a, (_, &x)| a.wrapping_add(x));
        let crc_len = u16::from_le_bytes([b[10], b[11]]) as usize;
        sum == b[4] && crc_itu(&b[16..16 + crc_len]) == u16::from_le_bytes([b[8], b[9]])
    }

    #[test]
    fn builds_consistent_volume() {
        let dir = std::env::temp_dir().join(format!("bc-fs-udf-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.txt"), b"alpha").unwrap();
        fs::write(dir.join("sub").join("b.bin"), vec![1u8; 5000]).unwrap();

        let img = build(std::slice::from_ref(&dir), &UdfOptions::new("TEST")).unwrap();
        let mut bytes = Vec::new();
        img.reader().read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len() as u64, img.bytes());
        assert_eq!(img.sectors() % 32, 0);
        assert_eq!(img.files, 2);
        assert_eq!(img.directories, 3);

        let sec = |n: u64| &bytes[n as usize * BS..(n as usize + 1) * BS];
        assert_eq!(&sec(17)[1..6], b"NSR03");
        for n in [32, 33, 34, 35, 36, 37, 48, 64, 65, 256, img.sectors() - 1] {
            assert!(tag_ok(sec(n)), "bad tag at sector {n}");
        }
        assert_eq!(u16::from_le_bytes([sec(img.sectors() - 1)[0], 0]), tag::AVDP);

        // FSD at metadata block 0 and the root EFE right after it.
        let meta0 = PARTITION_START as u64 + META_UNIT as u64;
        assert_eq!(u16::from_le_bytes([sec(meta0)[0], sec(meta0)[1]]), tag::FSD);
        assert!(tag_ok(sec(meta0)));
        assert_eq!(u16::from_le_bytes([sec(meta0 + 1)[0], sec(meta0 + 1)[1]]), tag::EFE);
        assert!(tag_ok(sec(meta0 + 1)));

        // File contents land where the data area starts.
        assert!(bytes.windows(5).any(|w| w == b"alpha"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_duplicate_names() {
        let base = std::env::temp_dir().join(format!("bc-fs-dup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("x")).unwrap();
        fs::create_dir_all(base.join("y")).unwrap();
        fs::write(base.join("x").join("same.txt"), b"1").unwrap();
        fs::write(base.join("y").join("same.txt"), b"2").unwrap();
        let err = build(
            &[base.join("x").join("same.txt"), base.join("y").join("same.txt")],
            &UdfOptions::new("T"),
        )
        .err()
        .unwrap();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        fs::remove_dir_all(base).unwrap();
    }
}
