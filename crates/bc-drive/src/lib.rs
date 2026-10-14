//! High-level, OS-agnostic optical drive API built on [`bc_scsi::DriveTransport`].

mod profile;

pub use bc_scsi::{DevicePath, ScsiError, Sense};
pub use profile::{MediaFamily, Profile};

use bc_scsi::cdb::{self, rt, toc_format};
use bc_scsi::{DataDir, DriveTransport};

pub type Result<T> = std::result::Result<T, ScsiError>;

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn ascii(b: &[u8]) -> String {
    String::from_utf8_lossy(b).trim().to_string()
}

fn short(what: &str, got: usize, need: usize) -> ScsiError {
    ScsiError::InvalidResponse(format!("{what}: got {got} bytes, need at least {need}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inquiry {
    pub peripheral_type: u8,
    pub vendor: String,
    pub product: String,
    pub revision: String,
}

impl Inquiry {
    pub fn is_mmc(&self) -> bool {
        self.peripheral_type == 0x05
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaState {
    Ready,
    NoMedium,
    BecomingReady,
    NotReady(Sense),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscStatus {
    Empty,
    Incomplete,
    Finalized,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Empty,
    Incomplete,
    Reserved,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscInfo {
    pub erasable: bool,
    pub last_session: SessionState,
    pub status: DiscStatus,
    pub first_track: u16,
    pub sessions: u16,
    pub first_track_last_session: u16,
    pub last_track_last_session: u16,
    /// 0x00 CD-DA/CD-ROM, 0x10 CD-I, 0x20 CD-ROM XA, 0xFF undefined.
    pub disc_type: u8,
}

impl DiscInfo {
    pub fn parse(d: &[u8]) -> Result<DiscInfo> {
        if d.len() < 12 {
            return Err(short("disc information", d.len(), 12));
        }
        Ok(DiscInfo {
            erasable: d[2] & 0x10 != 0,
            last_session: match (d[2] >> 2) & 0x3 {
                0 => SessionState::Empty,
                1 => SessionState::Incomplete,
                2 => SessionState::Reserved,
                _ => SessionState::Complete,
            },
            status: match d[2] & 0x3 {
                0 => DiscStatus::Empty,
                1 => DiscStatus::Incomplete,
                2 => DiscStatus::Finalized,
                _ => DiscStatus::Other,
            },
            first_track: d[3] as u16,
            sessions: u16::from_be_bytes([d[9], d[4]]),
            first_track_last_session: u16::from_be_bytes([d[10], d[5]]),
            last_track_last_session: u16::from_be_bytes([d[11], d[6]]),
            disc_type: d[8],
        })
    }

    pub fn is_appendable(&self) -> bool {
        matches!(self.status, DiscStatus::Empty | DiscStatus::Incomplete)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TocEntry {
    /// Track number; 0xAA is the lead-out.
    pub number: u8,
    pub adr: u8,
    pub control: u8,
    pub lba: u32,
}

impl TocEntry {
    pub const LEADOUT: u8 = 0xAA;

    pub fn is_data(&self) -> bool {
        self.control & 0x04 != 0
    }

    pub fn copy_permitted(&self) -> bool {
        self.control & 0x02 != 0
    }

    pub fn pre_emphasis(&self) -> bool {
        !self.is_data() && self.control & 0x01 != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toc {
    pub first_track: u8,
    pub last_track: u8,
    pub tracks: Vec<TocEntry>,
    pub leadout: Option<TocEntry>,
}

impl Toc {
    pub fn parse(d: &[u8]) -> Result<Toc> {
        if d.len() < 4 {
            return Err(short("TOC", d.len(), 4));
        }
        let len = (be16(d) as usize + 2).min(d.len());
        let mut tracks = Vec::new();
        let mut leadout = None;
        for e in d[4..len].as_chunks::<8>().0 {
            let entry = TocEntry {
                adr: e[1] >> 4,
                control: e[1] & 0x0F,
                number: e[2],
                lba: be32(&e[4..8]),
            };
            if entry.number == TocEntry::LEADOUT {
                leadout = Some(entry);
            } else {
                tracks.push(entry);
            }
        }
        Ok(Toc {
            first_track: d[2],
            last_track: d[3],
            tracks,
            leadout,
        })
    }

    /// Length of each track in sectors, measured to the next track start or the lead-out.
    pub fn track_lengths(&self) -> Vec<(u8, u32)> {
        let ends = self
            .tracks
            .iter()
            .skip(1)
            .map(|t| t.lba)
            .chain(self.leadout.map(|l| l.lba));
        self.tracks
            .iter()
            .zip(ends)
            .map(|(t, end)| (t.number, end.saturating_sub(t.lba)))
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    pub last_lba: u32,
    pub block_len: u32,
}

impl Capacity {
    pub fn bytes(&self) -> u64 {
        (self.last_lba as u64 + 1) * self.block_len as u64
    }
}

/// From the legacy CD capabilities mode page (2Ah). BD/HD DVD support comes from profiles.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CdCapabilities {
    pub read_cd_r: bool,
    pub read_cd_rw: bool,
    pub read_dvd_rom: bool,
    pub read_dvd_r: bool,
    pub read_dvd_ram: bool,
    pub write_cd_r: bool,
    pub write_cd_rw: bool,
    pub test_write: bool,
    pub write_dvd_r: bool,
    pub write_dvd_ram: bool,
    pub cdda_supported: bool,
    pub cdda_accurate: bool,
    pub rw_subchannel: bool,
    pub c2_pointers: bool,
    pub isrc: bool,
    pub upc: bool,
    pub buffer_underrun_protection: bool,
    pub ejectable: bool,
}

impl CdCapabilities {
    /// Parses a MODE SENSE(10) response containing page 2Ah.
    pub fn parse(d: &[u8]) -> Result<CdCapabilities> {
        if d.len() < 8 {
            return Err(short("mode sense header", d.len(), 8));
        }
        let page = 8 + be16(&d[6..8]) as usize;
        let p = d.get(page..).filter(|p| p.len() >= 8).ok_or_else(|| short("page 2Ah", d.len(), page + 8))?;
        if p[0] & 0x3F != 0x2A {
            return Err(ScsiError::InvalidResponse(format!("expected page 2Ah, got {:02X}h", p[0] & 0x3F)));
        }
        let bit = |byte: usize, n: u8| p[byte] & (1 << n) != 0;
        Ok(CdCapabilities {
            read_cd_r: bit(2, 0),
            read_cd_rw: bit(2, 1),
            read_dvd_rom: bit(2, 3),
            read_dvd_r: bit(2, 4),
            read_dvd_ram: bit(2, 5),
            write_cd_r: bit(3, 0),
            write_cd_rw: bit(3, 1),
            test_write: bit(3, 2),
            write_dvd_r: bit(3, 4),
            write_dvd_ram: bit(3, 5),
            cdda_supported: bit(5, 0),
            cdda_accurate: bit(5, 1),
            rw_subchannel: bit(5, 2),
            c2_pointers: bit(5, 4),
            isrc: bit(5, 5),
            upc: bit(5, 6),
            buffer_underrun_protection: bit(4, 7),
            ejectable: bit(6, 3),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackInfo {
    pub track: u16,
    pub session: u16,
    pub blank: bool,
    pub start_lba: u32,
    pub next_writable: Option<u32>,
    pub free_blocks: u32,
    pub size: u32,
    pub last_recorded: Option<u32>,
}

impl TrackInfo {
    pub fn parse(d: &[u8]) -> Result<TrackInfo> {
        if d.len() < 28 {
            return Err(short("track information", d.len(), 28));
        }
        let msb = |i: usize| d.get(i).copied().unwrap_or(0);
        Ok(TrackInfo {
            track: u16::from_be_bytes([msb(32), d[2]]),
            session: u16::from_be_bytes([msb(33), d[3]]),
            blank: d[6] & 0x40 != 0,
            start_lba: be32(&d[8..12]),
            next_writable: (d[7] & 0x01 != 0).then(|| be32(&d[12..16])),
            free_blocks: be32(&d[16..20]),
            size: be32(&d[24..28]),
            last_recorded: (d.len() >= 32 && d[7] & 0x02 != 0).then(|| be32(&d[28..32])),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyProtection {
    None,
    /// CSS (DVD-Video) or CPPM (DVD-Audio).
    CssCppm,
    Cprm,
    Aacs,
    Unknown(u8),
}

impl CopyProtection {
    pub fn is_protected(self) -> bool {
        self != CopyProtection::None
    }

    pub fn name(self) -> &'static str {
        match self {
            CopyProtection::None => "none",
            CopyProtection::CssCppm => "CSS/CPPM",
            CopyProtection::Cprm => "CPRM",
            CopyProtection::Aacs => "AACS",
            CopyProtection::Unknown(_) => "unknown protection system",
        }
    }
}

/// Parses GET PERFORMANCE type 03h: an 8-byte header, then 16-byte write speed descriptors.
pub fn parse_write_speeds(d: &[u8]) -> Result<Vec<u32>> {
    if d.len() < 8 {
        return Err(short("performance header", d.len(), 8));
    }
    let len = (be32(&d[0..4]) as usize + 4).min(d.len());
    let mut speeds: Vec<u32> = d[8..len]
        .as_chunks::<16>()
        .0
        .iter()
        .map(|e| be32(&e[12..16]))
        .filter(|&s| s > 0)
        .collect();
    speeds.sort_unstable_by(|a, b| b.cmp(a));
    speeds.dedup();
    Ok(speeds)
}

/// Wraps a transport with typed MMC commands.
pub struct Drive {
    t: Box<dyn DriveTransport>,
}

impl Drive {
    pub fn open(path: &str) -> Result<Drive> {
        Ok(Drive::new(bc_scsi::open(path)?))
    }

    pub fn new(t: Box<dyn DriveTransport>) -> Drive {
        Drive { t }
    }

    pub fn transport(&mut self) -> &mut dyn DriveTransport {
        self.t.as_mut()
    }

    fn read_in(&mut self, cdb: &[u8], alloc: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; alloc];
        let n = self.t.command(cdb, DataDir::In, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    pub fn inquiry(&mut self) -> Result<Inquiry> {
        let d = self.read_in(&cdb::inquiry(96), 96)?;
        if d.len() < 36 {
            return Err(short("inquiry", d.len(), 36));
        }
        Ok(Inquiry {
            peripheral_type: d[0] & 0x1F,
            vendor: ascii(&d[8..16]),
            product: ascii(&d[16..32]),
            revision: ascii(&d[32..36]),
        })
    }

    pub fn media_state(&mut self) -> Result<MediaState> {
        match self.t.command(&cdb::test_unit_ready(), DataDir::None, &mut []) {
            Ok(_) => Ok(MediaState::Ready),
            Err(ScsiError::CheckCondition(s)) if s.is_medium_not_present() => Ok(MediaState::NoMedium),
            Err(ScsiError::CheckCondition(s)) if s.is_becoming_ready() => Ok(MediaState::BecomingReady),
            Err(ScsiError::CheckCondition(s)) if s.key == bc_scsi::sense::key::UNIT_ATTENTION => {
                // A unit attention is consumed by the command that reports it; ask again.
                match self.t.command(&cdb::test_unit_ready(), DataDir::None, &mut []) {
                    Ok(_) => Ok(MediaState::Ready),
                    Err(ScsiError::CheckCondition(s)) if s.is_medium_not_present() => Ok(MediaState::NoMedium),
                    Err(ScsiError::CheckCondition(s)) => Ok(MediaState::NotReady(s)),
                    Err(e) => Err(e),
                }
            }
            Err(ScsiError::CheckCondition(s)) => Ok(MediaState::NotReady(s)),
            Err(e) => Err(e),
        }
    }

    /// Returns the current profile and every profile the drive supports.
    pub fn profiles(&mut self) -> Result<(Profile, Vec<Profile>)> {
        let d = self.read_in(&cdb::get_configuration(rt::ONE, 0x0000, 1024), 1024)?;
        if d.len() < 8 {
            return Err(short("configuration header", d.len(), 8));
        }
        let current = Profile(be16(&d[6..8]));
        let mut list = Vec::new();
        if d.len() >= 12 && be16(&d[8..10]) == 0x0000 {
            let end = (12 + d[11] as usize).min(d.len());
            for p in d[12..end].as_chunks::<4>().0 {
                list.push(Profile(be16(p)));
            }
        }
        list.sort_by_key(|p| p.0);
        list.dedup();
        Ok((current, list))
    }

    pub fn disc_info(&mut self) -> Result<DiscInfo> {
        let d = self.read_in(&cdb::read_disc_information(34), 34)?;
        DiscInfo::parse(&d)
    }

    pub fn toc(&mut self) -> Result<Toc> {
        let alloc = 4 + 100 * 8;
        let d = self.read_in(&cdb::read_toc(toc_format::TOC, false, 0, alloc as u16), alloc)?;
        Toc::parse(&d)
    }

    pub fn capacity(&mut self) -> Result<Capacity> {
        let d = self.read_in(&cdb::read_capacity(), 8)?;
        if d.len() < 8 {
            return Err(short("capacity", d.len(), 8));
        }
        Ok(Capacity {
            last_lba: be32(&d[0..4]),
            block_len: be32(&d[4..8]),
        })
    }

    pub fn track_info(&mut self, track: u32) -> Result<TrackInfo> {
        let d = self.read_in(&cdb::read_track_information(track, 48), 48)?;
        TrackInfo::parse(&d)
    }

    /// Free blocks in the writable (invisible/incomplete) track of an appendable disc.
    pub fn free_blocks(&mut self, di: &DiscInfo) -> Result<u32> {
        Ok(self.track_info(di.last_track_last_session as u32)?.free_blocks)
    }

    /// Write speeds (kB/s) the drive supports for the inserted disc, fastest first.
    pub fn write_speeds(&mut self) -> Result<Vec<u32>> {
        let d = self.read_in(&cdb::get_performance_write_speeds(32), 8 + 32 * 16)?;
        parse_write_speeds(&d)
    }

    pub fn cd_capabilities(&mut self) -> Result<CdCapabilities> {
        let d = self.read_in(&cdb::mode_sense_10(0x2A, 256), 256)?;
        CdCapabilities::parse(&d)
    }

    /// Reads the DVD copyright information structure (format 01h) for layer 0.
    pub fn dvd_copy_protection(&mut self) -> Result<CopyProtection> {
        let d = self.read_in(&cdb::read_disc_structure(cdb::media_type::DVD, 0, 0x01, 8), 8)?;
        if d.len() < 5 {
            return Err(short("copyright structure", d.len(), 5));
        }
        Ok(match d[4] {
            0x00 => CopyProtection::None,
            0x01 => CopyProtection::CssCppm,
            0x02 => CopyProtection::Cprm,
            0x03 => CopyProtection::Aacs,
            other => CopyProtection::Unknown(other),
        })
    }

    pub fn eject(&mut self) -> Result<()> {
        let _ = self.t.command(&cdb::prevent_allow_medium_removal(false), DataDir::None, &mut []);
        self.t.command(&cdb::start_stop_unit(false, true), DataDir::None, &mut [])?;
        Ok(())
    }

    pub fn load(&mut self) -> Result<()> {
        self.t.command(&cdb::start_stop_unit(true, true), DataDir::None, &mut [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bc_scsi::cdb::op;
    use bc_scsi::mock::{MockTransport, Reply};

    fn inquiry_bytes() -> Vec<u8> {
        let mut d = vec![0u8; 36];
        d[0] = 0x05;
        d[8..16].copy_from_slice(b"PIONEER ");
        d[16..32].copy_from_slice(b"BD-RW   BDR-XD08");
        d[32..36].copy_from_slice(b"1.02");
        d
    }

    fn two_track_toc() -> Vec<u8> {
        let mut d = vec![0, 26, 1, 2];
        d.extend([0, 0x14, 1, 0, 0, 0, 0, 0]);
        d.extend([0, 0x10, 2, 0, 0, 0, 0x10, 0x00]);
        d.extend([0, 0x10, 0xAA, 0, 0, 0, 0x20, 0x00]);
        d
    }

    #[test]
    fn inquiry_parses_strings() {
        let mut drive = Drive::new(Box::new(
            MockTransport::new().on_opcode(op::INQUIRY, Reply::Data(inquiry_bytes())),
        ));
        let i = drive.inquiry().unwrap();
        assert!(i.is_mmc());
        assert_eq!(i.vendor, "PIONEER");
        assert_eq!(i.product, "BD-RW   BDR-XD08");
        assert_eq!(i.revision, "1.02");
    }

    #[test]
    fn no_medium_is_a_state_not_an_error() {
        let mut drive = Drive::new(Box::new(MockTransport::new().on_opcode(
            op::TEST_UNIT_READY,
            Reply::Check(Sense { key: 2, asc: 0x3A, ascq: 0 }),
        )));
        assert_eq!(drive.media_state().unwrap(), MediaState::NoMedium);
    }

    #[test]
    fn profile_list() {
        let mut d = vec![0, 0, 0, 20, 0, 0, 0x00, 0x1B];
        d.extend([0x00, 0x00, 0x03, 12]);
        d.extend([0x00, 0x1B, 0x01, 0]);
        d.extend([0x00, 0x09, 0x00, 0]);
        d.extend([0x00, 0x43, 0x00, 0]);
        let mut drive = Drive::new(Box::new(
            MockTransport::new().on_opcode(op::GET_CONFIGURATION, Reply::Data(d)),
        ));
        let (cur, all) = drive.profiles().unwrap();
        assert_eq!(cur, Profile::DVD_PLUS_R);
        assert_eq!(all, vec![Profile::CD_R, Profile::DVD_PLUS_R, Profile::BD_RE]);
    }

    #[test]
    fn toc_and_track_lengths() {
        let toc = Toc::parse(&two_track_toc()).unwrap();
        assert_eq!((toc.first_track, toc.last_track), (1, 2));
        assert!(toc.tracks[0].is_data());
        assert!(!toc.tracks[1].is_data());
        assert_eq!(toc.leadout.unwrap().lba, 0x2000);
        assert_eq!(toc.track_lengths(), vec![(1, 0x1000), (2, 0x1000)]);
    }

    #[test]
    fn disc_info_fields() {
        let mut d = vec![0u8; 34];
        d[2] = 0x10 | (1 << 2) | 1;
        d[3] = 1;
        d[4] = 2;
        d[5] = 3;
        d[6] = 3;
        let info = DiscInfo::parse(&d).unwrap();
        assert!(info.erasable);
        assert_eq!(info.status, DiscStatus::Incomplete);
        assert_eq!(info.last_session, SessionState::Incomplete);
        assert_eq!(info.sessions, 2);
        assert!(info.is_appendable());
    }

    #[test]
    fn capabilities_page() {
        let mut d = vec![0u8; 8 + 32];
        d[8] = 0x2A;
        d[9] = 30;
        d[8 + 2] = 0b0011_1011;
        d[8 + 3] = 0b0001_0011;
        d[8 + 4] = 0x80;
        d[8 + 5] = 0b0001_0011;
        d[8 + 6] = 0x08;
        let c = CdCapabilities::parse(&d).unwrap();
        assert!(c.read_cd_r && c.read_dvd_r && c.write_cd_rw && c.write_dvd_r);
        assert!(c.cdda_supported && c.cdda_accurate && c.c2_pointers);
        assert!(c.buffer_underrun_protection && c.ejectable);
        assert!(!c.test_write);
    }

    #[test]
    fn blank_track_info() {
        let mut d = vec![0u8; 36];
        d[2] = 1;
        d[3] = 1;
        d[6] = 0x40;
        d[7] = 0x01;
        d[16..20].copy_from_slice(&12_219_392u32.to_be_bytes());
        d[24..28].copy_from_slice(&12_219_392u32.to_be_bytes());
        let t = TrackInfo::parse(&d).unwrap();
        assert!(t.blank);
        assert_eq!(t.next_writable, Some(0));
        assert_eq!(t.free_blocks, 12_219_392);
        assert_eq!(t.last_recorded, None);
    }

    #[test]
    fn write_speed_descriptors() {
        let mut d = vec![0, 0, 0, 4 + 48, 0, 0, 0, 0];
        for kbs in [26_970u32, 17_980, 26_970] {
            let mut e = [0u8; 16];
            e[12..16].copy_from_slice(&kbs.to_be_bytes());
            d.extend(e);
        }
        assert_eq!(parse_write_speeds(&d).unwrap(), vec![26_970, 17_980]);
    }

    #[test]
    fn css_detection() {
        let mut drive = Drive::new(Box::new(MockTransport::new().on_opcode(
            op::READ_DISC_STRUCTURE,
            Reply::Data(vec![0, 6, 0, 0, 0x01, 0xFE, 0, 0]),
        )));
        assert_eq!(drive.dvd_copy_protection().unwrap(), CopyProtection::CssCppm);
    }

    #[test]
    fn eject_sends_allow_then_stop() {
        let mock = MockTransport::new()
            .on_opcode(op::PREVENT_ALLOW_MEDIUM_REMOVAL, Reply::Data(vec![]))
            .on_opcode(op::START_STOP_UNIT, Reply::Data(vec![]));
        let mut drive = Drive::new(Box::new(mock));
        drive.eject().unwrap();
    }
}
