//! Writes an [`Image`] to DVD/Blu-ray media and verifies it.
//!
//! Supported targets: BD-R (SRM), BD-RE (formatted), DVD+R, DVD+R DL and DVD+RW.
//! These take plain sequential WRITE(10) without a write-parameters mode page.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

use bc_drive::{DiscStatus, Drive, Profile};
use bc_fs::{Image, SECTOR};
use bc_scsi::cdb::{self, close_function};
use bc_scsi::{DataDir, DriveTransport, STATUS_GOOD, ScsiError, Sense, sense};

/// Sectors per WRITE/READ command: one 64 KiB Blu-ray ECC cluster.
const BLOCKS_PER_CMD: usize = 32;
const CHUNK: usize = BLOCKS_PER_CMD * SECTOR as usize;
const LONG_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, thiserror::Error)]
pub enum BurnError {
    #[error("{0}")]
    Media(String),
    #[error("Burning requires administrator rights. Close burncraft and start it again with \"Run as administrator\".")]
    NoWriteAccess,
    #[error("drive error: {0}")]
    Scsi(#[from] ScsiError),
    #[error("could not read source files: {0}")]
    Io(#[from] std::io::Error),
    #[error("write failed at sector {lba}: {source}")]
    Write { lba: u32, source: ScsiError },
    #[error("verification failed: sector {0} on the disc does not match the source")]
    VerifyMismatch(u64),
    #[error("verification read failed at sector {lba}: {source}")]
    VerifyRead { lba: u32, source: ScsiError },
    #[error("cancelled")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, BurnError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Preparing,
    Writing,
    Flushing,
    Closing,
    Verifying,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Preparing => "Preparing",
            Phase::Writing => "Writing",
            Phase::Flushing => "Flushing drive cache",
            Phase::Closing => "Closing and finalizing disc",
            Phase::Verifying => "Verifying",
        }
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Phase(Phase),
    Progress { done: u64, total: u64 },
    Log(String),
}

#[derive(Debug, Clone, Copy)]
pub struct BurnSettings {
    /// Write speed in kB/s (1000 bytes); `None` for the drive maximum.
    pub write_kbs: Option<u16>,
    pub finalize: bool,
    pub verify: bool,
}

/// 1x speeds in kB/s, as used by SET CD SPEED.
pub const BD_1X_KBS: u16 = 4495;
pub const DVD_1X_KBS: u16 = 1385;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recording {
    /// Write-once, appended sequentially from the next writable address.
    Sequential,
    /// Formatted rewritable media written in place from sector 0.
    Overwrite,
}

pub struct Target {
    pub profile: Profile,
    pub capacity_sectors: u64,
    recording: Recording,
    track: u16,
}

fn not_supported(p: Profile) -> BurnError {
    BurnError::Media(format!(
        "{} discs are not supported by this version yet. Use a BD-R, BD-RE, DVD+R or DVD+RW.",
        p.name()
    ))
}

/// Checks that the inserted disc can take a new compilation and returns its capacity.
pub fn inspect_target(drive: &mut Drive) -> Result<Target> {
    let (profile, _) = drive.profiles()?;
    let recording = match profile {
        Profile::BD_R_SRM | Profile::DVD_PLUS_R | Profile::DVD_PLUS_R_DL => Recording::Sequential,
        Profile::BD_RE | Profile::DVD_PLUS_RW => Recording::Overwrite,
        Profile::NONE => return Err(BurnError::Media("No disc in the drive.".into())),
        p if !p.is_recordable() => {
            return Err(BurnError::Media(format!("{} is a read-only disc and cannot be written.", p.name())));
        }
        p => return Err(not_supported(p)),
    };
    let di = drive.disc_info()?;
    match recording {
        Recording::Sequential => {
            if di.status != DiscStatus::Empty {
                return Err(BurnError::Media(format!(
                    "The {} is not blank. Insert a blank disc.",
                    profile.name()
                )));
            }
            let track = di.last_track_last_session;
            let free = drive.track_info(track as u32)?.free_blocks as u64;
            Ok(Target {
                profile,
                capacity_sectors: free,
                recording,
                track,
            })
        }
        Recording::Overwrite => {
            let cap = drive.capacity().map_err(|_| {
                BurnError::Media(format!("The {} is not formatted. Format it first.", profile.name()))
            })?;
            Ok(Target {
                profile,
                capacity_sectors: cap.last_lba as u64 + 1,
                recording,
                track: 1,
            })
        }
    }
}

fn is_busy(s: &Sense) -> bool {
    // NOT READY: becoming ready / format, operation or long write in progress.
    s.key == sense::key::NOT_READY && s.asc == 0x04 && matches!(s.ascq, 0x01 | 0x04 | 0x07 | 0x08)
}

/// Runs a command, retrying while the drive reports it is still busy with earlier work.
fn run_patiently(
    t: &mut dyn DriveTransport,
    cdb: &[u8],
    dir: DataDir,
    buf: &mut [u8],
    timeout: Duration,
    patience: Duration,
) -> std::result::Result<(), ScsiError> {
    let start = Instant::now();
    loop {
        let r = t.execute(cdb, dir, buf, timeout)?;
        if r.status == STATUS_GOOD {
            return Ok(());
        }
        match r.sense {
            Some(s) if (is_busy(&s) || s.key == sense::key::UNIT_ATTENTION) && start.elapsed() < patience => {
                std::thread::sleep(Duration::from_millis(if s.ascq == 0x08 { 5 } else { 500 }));
            }
            Some(s) => return Err(ScsiError::CheckCondition(s)),
            None => return Err(ScsiError::Status(r.status)),
        }
    }
}

fn wait_ready(t: &mut dyn DriveTransport, patience: Duration) -> std::result::Result<(), ScsiError> {
    run_patiently(t, &cdb::test_unit_ready(), DataDir::None, &mut [], Duration::from_secs(30), patience)
}

/// Burns `image` to the disc in `drive`. Progress and log lines are reported through `on_event`.
/// Cancelling during the write phase leaves a write-once disc unusable.
pub fn burn(
    drive: &mut Drive,
    image: &Image,
    settings: &BurnSettings,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(Event),
) -> Result<()> {
    on_event(Event::Phase(Phase::Preparing));
    if !drive.transport().can_write() {
        return Err(BurnError::NoWriteAccess);
    }
    let target = inspect_target(drive)?;
    let total = image.sectors();
    if total > target.capacity_sectors {
        return Err(BurnError::Media(format!(
            "The compilation needs {} sectors but the {} only has {} free.",
            total,
            target.profile.name(),
            target.capacity_sectors
        )));
    }
    on_event(Event::Log(format!(
        "Target: {} ({} sectors free), writing {} sectors",
        target.profile.name(),
        target.capacity_sectors,
        total
    )));

    let t = drive.transport();
    match t.lock_exclusive() {
        Ok(()) => on_event(Event::Log("Drive locked: other programs cannot access the disc while burning".into())),
        Err(e) => on_event(Event::Log(format!("Could not lock the drive for exclusive use ({e}). Burning continues; avoid opening the disc in other programs."))),
    }
    let _ = t.command(&cdb::prevent_allow_medium_removal(true), DataDir::None, &mut []);

    let result = write_and_close(t, image, &target, settings, cancel, on_event);

    let _ = t.command(&cdb::prevent_allow_medium_removal(false), DataDir::None, &mut []);
    result?;

    if settings.verify {
        verify(drive, image, cancel, on_event)?;
    }
    Ok(())
}

fn write_and_close(
    t: &mut dyn DriveTransport,
    image: &Image,
    target: &Target,
    settings: &BurnSettings,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(Event),
) -> Result<()> {
    let speed = settings.write_kbs.unwrap_or(0xFFFF);
    match t.command(&cdb::set_cd_speed(0xFFFF, speed), DataDir::None, &mut []) {
        Ok(_) => on_event(Event::Log(match settings.write_kbs {
            Some(k) => format!("Write speed set to {k} kB/s"),
            None => "Write speed: maximum".into(),
        })),
        Err(e) => on_event(Event::Log(format!("Drive kept its default speed ({e})"))),
    }

    on_event(Event::Phase(Phase::Writing));
    let total = image.sectors();
    let started = Instant::now();
    std::thread::scope(|scope| -> Result<()> {
        let (tx, rx) = sync_channel::<std::io::Result<Vec<u8>>>(256);
        scope.spawn(move || {
            let mut reader = image.reader();
            loop {
                let mut buf = vec![0u8; CHUNK];
                match reader.fill(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.truncate(n);
                        if tx.send(Ok(buf)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        break;
                    }
                }
            }
        });

        let mut lba = 0u32;
        let mut last_report = Instant::now();
        for buf in rx {
            if cancel.load(Ordering::Relaxed) {
                return Err(BurnError::Cancelled);
            }
            let mut buf = buf?;
            let blocks = (buf.len() / SECTOR as usize) as u16;
            run_patiently(
                t,
                &cdb::write_10(lba, blocks),
                DataDir::Out,
                &mut buf,
                Duration::from_secs(60),
                Duration::from_secs(120),
            )
            .map_err(|source| BurnError::Write { lba, source })?;
            lba += blocks as u32;
            if last_report.elapsed() >= Duration::from_millis(250) {
                on_event(Event::Progress { done: lba as u64, total });
                last_report = Instant::now();
            }
        }
        on_event(Event::Progress { done: lba as u64, total });
        Ok(())
    })?;
    on_event(Event::Log(format!(
        "Wrote {} sectors in {:.0} s",
        total,
        started.elapsed().as_secs_f64()
    )));

    on_event(Event::Phase(Phase::Flushing));
    run_patiently(t, &cdb::synchronize_cache(false), DataDir::None, &mut [], LONG_TIMEOUT, LONG_TIMEOUT)?;

    if target.recording == Recording::Sequential {
        on_event(Event::Phase(Phase::Closing));
        let close = |t: &mut dyn DriveTransport, f: u8, n: u16| {
            run_patiently(
                t,
                &cdb::close_track_session(false, f, n),
                DataDir::None,
                &mut [],
                LONG_TIMEOUT,
                LONG_TIMEOUT,
            )
        };
        close(t, close_function::TRACK, target.track)?;
        on_event(Event::Log("Track closed".into()));
        if settings.finalize {
            let mut done = close(t, close_function::FINALIZE, 0);
            if done.is_err() && matches!(target.profile, Profile::DVD_PLUS_R | Profile::DVD_PLUS_R_DL) {
                done = close(t, close_function::SESSION_AND_FINALIZE, 0);
            }
            if let Err(e) = done {
                on_event(Event::Log(format!("Finalize not accepted ({e}); closing session instead")));
                close(t, close_function::SESSION, 0)?;
                on_event(Event::Log("Session closed (disc left appendable)".into()));
            } else {
                on_event(Event::Log("Disc finalized".into()));
            }
        } else {
            close(t, close_function::SESSION, 0)?;
            on_event(Event::Log("Session closed (disc left appendable)".into()));
        }
    }
    wait_ready(t, LONG_TIMEOUT)?;
    Ok(())
}

fn verify(drive: &mut Drive, image: &Image, cancel: &AtomicBool, on_event: &mut dyn FnMut(Event)) -> Result<()> {
    on_event(Event::Phase(Phase::Verifying));
    let t = drive.transport();
    wait_ready(t, Duration::from_secs(120))?;
    let total = image.sectors();
    let mut reader = image.reader();
    let mut expected = vec![0u8; CHUNK];
    let mut actual = vec![0u8; CHUNK];
    let mut lba = 0u64;
    let mut last_report = Instant::now();
    while lba < total {
        if cancel.load(Ordering::Relaxed) {
            return Err(BurnError::Cancelled);
        }
        let n = reader.fill(&mut expected)?;
        let blocks = n / SECTOR as usize;
        run_patiently(
            t,
            &cdb::read_10(lba as u32, blocks as u16),
            DataDir::In,
            &mut actual[..n],
            Duration::from_secs(60),
            Duration::from_secs(60),
        )
        .map_err(|source| BurnError::VerifyRead { lba: lba as u32, source })?;
        if let Some(i) = (0..blocks).find(|i| {
            let r = i * SECTOR as usize..(i + 1) * SECTOR as usize;
            expected[r.clone()] != actual[r]
        }) {
            return Err(BurnError::VerifyMismatch(lba + i as u64));
        }
        lba += blocks as u64;
        if last_report.elapsed() >= Duration::from_millis(250) {
            on_event(Event::Progress { done: lba, total });
            last_report = Instant::now();
        }
    }
    on_event(Event::Progress { done: total, total });
    on_event(Event::Log("Verification passed: every sector matches the source".into()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bc_scsi::cdb::op;
    use bc_scsi::mock::{MockTransport, Reply};

    fn blank_bd_r() -> MockTransport {
        let mut config = vec![0, 0, 0, 12, 0, 0, 0x00, 0x41];
        config.extend([0x00, 0x00, 0x03, 4, 0x00, 0x41, 0x01, 0]);
        let mut di = vec![0u8; 34];
        di[3] = 1;
        di[4] = 1;
        di[5] = 1;
        di[6] = 1;
        let mut ti = vec![0u8; 36];
        ti[6] = 0x40;
        ti[7] = 0x01;
        ti[16..20].copy_from_slice(&12_219_392u32.to_be_bytes());
        MockTransport::new()
            .on_opcode(op::GET_CONFIGURATION, Reply::Data(config))
            .on_opcode(op::READ_DISC_INFORMATION, Reply::Data(di))
            .on_opcode(op::READ_TRACK_INFORMATION, Reply::Data(ti))
    }

    #[test]
    fn blank_bd_r_is_a_sequential_target() {
        let mut d = Drive::new(Box::new(blank_bd_r()));
        let t = inspect_target(&mut d).unwrap();
        assert_eq!(t.profile, Profile::BD_R_SRM);
        assert_eq!(t.capacity_sectors, 12_219_392);
        assert_eq!(t.recording, Recording::Sequential);
    }

    #[test]
    fn non_blank_bd_r_is_rejected() {
        let mut di = vec![0u8; 34];
        di[2] = 0x0E;
        let mock = blank_bd_r().on_opcode(op::READ_DISC_INFORMATION, Reply::Data(di));
        let mut d = Drive::new(Box::new(mock));
        assert!(matches!(inspect_target(&mut d), Err(BurnError::Media(_))));
    }

    #[test]
    fn burn_writes_every_sector_then_closes() {
        let mut img = Image::default();
        img.push(bc_fs::Chunk::Zero(70));
        let mock = blank_bd_r()
            .on_opcode(op::TEST_UNIT_READY, Reply::Data(vec![]))
            .on_opcode(op::PREVENT_ALLOW_MEDIUM_REMOVAL, Reply::Data(vec![]))
            .on_opcode(op::SET_CD_SPEED, Reply::Data(vec![]))
            .on_opcode(op::WRITE_10, Reply::Data(vec![]))
            .on_opcode(op::SYNCHRONIZE_CACHE, Reply::Data(vec![]))
            .on_opcode(op::CLOSE_TRACK_SESSION, Reply::Data(vec![]));
        let mut d = Drive::new(Box::new(mock));
        let mut events = Vec::new();
        burn(
            &mut d,
            &img,
            &BurnSettings {
                write_kbs: None,
                finalize: true,
                verify: false,
            },
            &AtomicBool::new(false),
            &mut |e| events.push(e),
        )
        .unwrap();
        assert!(events.iter().any(|e| matches!(e, Event::Progress { done: 70, total: 70 })));
        assert!(events.iter().any(|e| matches!(e, Event::Log(l) if l == "Disc finalized")));
    }
}
