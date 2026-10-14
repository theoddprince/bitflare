//! Windows backend: SCSI pass-through direct (SPTI) on `\\.\X:` volume handles.

use std::ffi::c_void;
use std::mem::{offset_of, size_of};
use std::time::Duration;

use crate::{CommandResult, DataDir, DevicePath, DriveTransport, Result, STATUS_CHECK_CONDITION, ScsiError, Sense};

type Handle = *mut c_void;
const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_SHARE_READ: u32 = 0x1;
const FILE_SHARE_WRITE: u32 = 0x2;
const OPEN_EXISTING: u32 = 3;
const DRIVE_CDROM: u32 = 5;

// CTL_CODE(IOCTL_SCSI_BASE, 0x405, METHOD_BUFFERED, FILE_READ_ACCESS | FILE_WRITE_ACCESS)
const IOCTL_SCSI_PASS_THROUGH_DIRECT: u32 = 0x0004_D014;

// IOCTL_CDROM_EXCLUSIVE_ACCESS is refused by the in-box cdrom.sys with ERROR_INVALID_HANDLE
// for LockDevice requests, so other programs are kept away by locking the volume instead.
const FSCTL_LOCK_VOLUME: u32 = 0x0009_0018;
const FSCTL_UNLOCK_VOLUME: u32 = 0x0009_001C;

const SCSI_IOCTL_DATA_OUT: u8 = 0;
const SCSI_IOCTL_DATA_IN: u8 = 1;
const SCSI_IOCTL_DATA_UNSPECIFIED: u8 = 2;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *mut c_void,
        disposition: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn DeviceIoControl(
        h: Handle,
        code: u32,
        in_buf: *mut c_void,
        in_size: u32,
        out_buf: *mut c_void,
        out_size: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn CloseHandle(h: Handle) -> i32;
    fn GetLogicalDrives() -> u32;
    fn GetDriveTypeW(root: *const u16) -> u32;
}

#[repr(C)]
struct ScsiPassThroughDirect {
    length: u16,
    scsi_status: u8,
    path_id: u8,
    target_id: u8,
    lun: u8,
    cdb_length: u8,
    sense_info_length: u8,
    data_in: u8,
    data_transfer_length: u32,
    timeout_value: u32,
    data_buffer: *mut c_void,
    sense_info_offset: u32,
    cdb: [u8; 16],
}

#[repr(C)]
struct SptdWithSense {
    sptd: ScsiPassThroughDirect,
    sense: [u8; 32],
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn enumerate() -> Result<Vec<DevicePath>> {
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u8 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i) as char;
        let root = wide(&format!("{letter}:\\"));
        if unsafe { GetDriveTypeW(root.as_ptr()) } == DRIVE_CDROM {
            out.push(DevicePath {
                path: format!(r"\\.\{letter}:"),
                label: format!("{letter}:"),
            });
        }
    }
    Ok(out)
}

fn normalize(path: &str) -> String {
    let p = path.trim();
    if p.starts_with(r"\\") {
        return p.to_string();
    }
    let letter = p.trim_end_matches(['\\', '/']).trim_end_matches(':');
    if letter.len() == 1 && letter.chars().all(|c| c.is_ascii_alphabetic()) {
        format!(r"\\.\{}:", letter.to_ascii_uppercase())
    } else {
        p.to_string()
    }
}

fn open_handle(path: &str, access: u32) -> Handle {
    let name = wide(path);
    unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    }
}

fn simple_ioctl(h: Handle, code: u32) -> std::io::Result<()> {
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h,
            code,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

pub struct WinTransport {
    handle: Handle,
    path: String,
    locked: bool,
    writable: bool,
}

// The handle is an owned kernel object; moving it between threads is safe.
unsafe impl Send for WinTransport {}

impl WinTransport {
    pub fn open(path: &str) -> Result<Self> {
        let path = normalize(path);
        let mut h = open_handle(&path, GENERIC_READ | GENERIC_WRITE);
        let writable = h != INVALID_HANDLE_VALUE;
        if !writable {
            h = open_handle(&path, GENERIC_READ);
        }
        if h == INVALID_HANDLE_VALUE {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                return Err(ScsiError::Io(std::io::Error::new(
                    e.kind(),
                    format!("{e} - the drive is in use by another program, or needs administrator rights"),
                )));
            }
            return Err(e.into());
        }
        Ok(Self {
            handle: h,
            path,
            locked: false,
            writable,
        })
    }
}

impl Drop for WinTransport {
    fn drop(&mut self) {
        if self.locked {
            let _ = simple_ioctl(self.handle, FSCTL_UNLOCK_VOLUME);
        }
        unsafe { CloseHandle(self.handle) };
    }
}

impl DriveTransport for WinTransport {
    fn can_write(&self) -> bool {
        self.writable
    }

    /// Locks the volume through this same handle; the lock fails while any other handle to
    /// the drive is open, and once held it keeps other programs from opening the drive.
    fn lock_exclusive(&mut self) -> Result<()> {
        if !self.locked {
            simple_ioctl(self.handle, FSCTL_LOCK_VOLUME)?;
            self.locked = true;
        }
        Ok(())
    }

    fn device_name(&self) -> Option<String> {
        Some(self.path.clone())
    }

    fn execute(
        &mut self,
        cdb: &[u8],
        dir: DataDir,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<CommandResult> {
        if cdb.is_empty() || cdb.len() > 16 {
            return Err(ScsiError::InvalidResponse(format!("bad CDB length {}", cdb.len())));
        }
        let (data_in, len, ptr) = match dir {
            DataDir::None => (SCSI_IOCTL_DATA_UNSPECIFIED, 0, std::ptr::null_mut()),
            DataDir::In => (SCSI_IOCTL_DATA_IN, buf.len() as u32, buf.as_mut_ptr() as *mut c_void),
            DataDir::Out => (SCSI_IOCTL_DATA_OUT, buf.len() as u32, buf.as_mut_ptr() as *mut c_void),
        };
        let mut req = SptdWithSense {
            sptd: ScsiPassThroughDirect {
                length: size_of::<ScsiPassThroughDirect>() as u16,
                scsi_status: 0,
                path_id: 0,
                target_id: 0,
                lun: 0,
                cdb_length: cdb.len() as u8,
                sense_info_length: 32,
                data_in,
                data_transfer_length: len,
                timeout_value: timeout.as_secs().clamp(1, u32::MAX as u64) as u32,
                data_buffer: ptr,
                sense_info_offset: offset_of!(SptdWithSense, sense) as u32,
                cdb: [0; 16],
            },
            sense: [0; 32],
        };
        req.sptd.cdb[..cdb.len()].copy_from_slice(cdb);
        let mut returned = 0u32;
        let size = size_of::<SptdWithSense>() as u32;
        let p = &mut req as *mut SptdWithSense as *mut c_void;
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_SCSI_PASS_THROUGH_DIRECT,
                p,
                size,
                p,
                size,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let status = req.sptd.scsi_status;
        let sense = if status == STATUS_CHECK_CONDITION {
            Sense::parse(&req.sense)
        } else {
            None
        };
        Ok(CommandResult {
            status,
            sense,
            transferred: req.sptd.data_transfer_length as usize,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sptd_layout_matches_windows_abi() {
        assert_eq!(size_of::<ScsiPassThroughDirect>(), 56);
        assert_eq!(offset_of!(ScsiPassThroughDirect, data_buffer), 24);
        assert_eq!(offset_of!(ScsiPassThroughDirect, cdb), 36);
    }

    #[test]
    fn path_shorthands() {
        assert_eq!(normalize("e"), r"\\.\E:");
        assert_eq!(normalize("E:"), r"\\.\E:");
        assert_eq!(normalize(r"E:\"), r"\\.\E:");
        assert_eq!(normalize(r"\\.\CdRom0"), r"\\.\CdRom0");
    }
}
