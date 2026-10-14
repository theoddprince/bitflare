//! macOS backend.
//!
//! The real implementation needs IOKit's `SCSITaskDeviceInterface` with exclusive access,
//! which in turn requires unmounting the disc through DiskArbitration first. Until that
//! lands (and can be tested on a Mac), every entry point reports `Unsupported`.

use std::time::Duration;

use crate::{CommandResult, DataDir, DevicePath, DriveTransport, Result, ScsiError};

pub fn enumerate() -> Result<Vec<DevicePath>> {
    Err(ScsiError::Unsupported("macOS IOKit backend not implemented yet"))
}

pub struct IoKitTransport;

impl IoKitTransport {
    pub fn open(_path: &str) -> Result<Self> {
        Err(ScsiError::Unsupported("macOS IOKit backend not implemented yet"))
    }
}

impl DriveTransport for IoKitTransport {
    fn execute(
        &mut self,
        _cdb: &[u8],
        _dir: DataDir,
        _buf: &mut [u8],
        _timeout: Duration,
    ) -> Result<CommandResult> {
        Err(ScsiError::Unsupported("macOS IOKit backend not implemented yet"))
    }
}
