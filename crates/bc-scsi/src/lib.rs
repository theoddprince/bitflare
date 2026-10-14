//! Raw MMC/SCSI access to optical drives.
//!
//! Everything above this crate talks to a drive only through [`DriveTransport`],
//! so the OS-specific pass-through code stays in one backend module per platform.

pub mod cdb;
pub mod mock;
pub mod sense;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod win;

use std::time::Duration;

pub use sense::Sense;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataDir {
    None,
    In,
    Out,
}

#[derive(Debug, Clone)]
pub struct CommandResult {
    pub status: u8,
    pub sense: Option<Sense>,
    pub transferred: usize,
}

pub const STATUS_GOOD: u8 = 0x00;
pub const STATUS_CHECK_CONDITION: u8 = 0x02;

#[derive(Debug, thiserror::Error)]
pub enum ScsiError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    CheckCondition(Sense),
    #[error("SCSI status {0:#04x}")]
    Status(u8),
    #[error("not supported on this platform: {0}")]
    Unsupported(&'static str),
    #[error("invalid response: {0}")]
    InvalidResponse(String),
}

impl ScsiError {
    pub fn sense(&self) -> Option<&Sense> {
        match self {
            ScsiError::CheckCondition(s) => Some(s),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, ScsiError>;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

pub trait DriveTransport: Send {
    /// Sends one CDB. A CHECK CONDITION is reported in the result, not as an error.
    fn execute(
        &mut self,
        cdb: &[u8],
        dir: DataDir,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<CommandResult>;

    /// False when the device was opened read-only (e.g. missing administrator rights),
    /// in which case write commands will be rejected by the OS.
    fn can_write(&self) -> bool {
        true
    }

    /// Asks the OS for exclusive access so no other program touches the drive while burning.
    /// Released when the transport is dropped.
    fn lock_exclusive(&mut self) -> Result<()> {
        Ok(())
    }

    /// The OS device actually used for pass-through, when known.
    fn device_name(&self) -> Option<String> {
        None
    }

    /// Sends one CDB and turns any non-GOOD status into an error.
    fn command(&mut self, cdb: &[u8], dir: DataDir, buf: &mut [u8]) -> Result<usize> {
        let r = self.execute(cdb, dir, buf, DEFAULT_TIMEOUT)?;
        match (r.status, r.sense) {
            (STATUS_GOOD, _) => Ok(r.transferred),
            (_, Some(s)) => Err(ScsiError::CheckCondition(s)),
            (st, None) => Err(ScsiError::Status(st)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevicePath {
    /// Path handed to [`open`], e.g. `\\.\E:` or `/dev/sr0`.
    pub path: String,
    /// Short user-facing label, e.g. `E:` or `sr0`.
    pub label: String,
}

/// Lists optical drives known to the OS.
pub fn enumerate() -> Result<Vec<DevicePath>> {
    #[cfg(windows)]
    return win::enumerate();
    #[cfg(target_os = "linux")]
    return linux::enumerate();
    #[cfg(target_os = "macos")]
    return macos::enumerate();
    #[allow(unreachable_code)]
    Err(ScsiError::Unsupported("drive enumeration"))
}

/// Opens a drive for raw command pass-through. Accepts the forms printed by [`enumerate`]
/// as well as shorthands such as `E`, `E:` or `sr0`.
pub fn open(path: &str) -> Result<Box<dyn DriveTransport>> {
    #[cfg(windows)]
    return Ok(Box::new(win::WinTransport::open(path)?));
    #[cfg(target_os = "linux")]
    return Ok(Box::new(linux::SgTransport::open(path)?));
    #[cfg(target_os = "macos")]
    return Ok(Box::new(macos::IoKitTransport::open(path)?));
    #[allow(unreachable_code)]
    {
        let _ = path;
        Err(ScsiError::Unsupported("drive access"))
    }
}
