//! Linux backend: SG_IO ioctl on `/dev/sr*`.

use std::ffi::{c_int, c_void};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::time::Duration;

use crate::{CommandResult, DataDir, DevicePath, DriveTransport, Result, STATUS_CHECK_CONDITION, ScsiError, Sense};

const SG_IO: u32 = 0x2285;
const SG_DXFER_NONE: c_int = -1;
const SG_DXFER_TO_DEV: c_int = -2;
const SG_DXFER_FROM_DEV: c_int = -3;
const DRIVER_SENSE: u16 = 0x08;

#[repr(C)]
struct SgIoHdr {
    interface_id: c_int,
    dxfer_direction: c_int,
    cmd_len: u8,
    mx_sb_len: u8,
    iovec_count: u16,
    dxfer_len: u32,
    dxferp: *mut c_void,
    cmdp: *const u8,
    sbp: *mut u8,
    timeout: u32,
    flags: u32,
    pack_id: c_int,
    usr_ptr: *mut c_void,
    status: u8,
    masked_status: u8,
    msg_status: u8,
    sb_len_wr: u8,
    host_status: u16,
    driver_status: u16,
    resid: c_int,
    duration: u32,
    info: u32,
}

pub fn enumerate() -> Result<Vec<DevicePath>> {
    let mut out: Vec<DevicePath> = std::fs::read_dir("/dev")?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let is_sr = name.strip_prefix("sr")?.chars().all(|c| c.is_ascii_digit());
            is_sr.then(|| DevicePath {
                path: format!("/dev/{name}"),
                label: name,
            })
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

pub struct SgTransport {
    file: File,
}

impl SgTransport {
    pub fn open(path: &str) -> Result<Self> {
        let path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/dev/{path}")
        };
        let open = |write| {
            OpenOptions::new()
                .read(true)
                .write(write)
                .custom_flags(libc::O_NONBLOCK)
                .open(&path)
        };
        let file = open(true).or_else(|_| open(false)).map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                std::io::Error::new(
                    e.kind(),
                    format!("{e} - add your user to the 'cdrom' (or 'optical') group"),
                )
            } else {
                e
            }
        })?;
        Ok(Self { file })
    }
}

impl DriveTransport for SgTransport {
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
        let mut sense = [0u8; 32];
        let (direction, len, ptr) = match dir {
            DataDir::None => (SG_DXFER_NONE, 0, std::ptr::null_mut()),
            DataDir::In => (SG_DXFER_FROM_DEV, buf.len() as u32, buf.as_mut_ptr() as *mut c_void),
            DataDir::Out => (SG_DXFER_TO_DEV, buf.len() as u32, buf.as_mut_ptr() as *mut c_void),
        };
        let mut hdr = SgIoHdr {
            interface_id: b'S' as c_int,
            dxfer_direction: direction,
            cmd_len: cdb.len() as u8,
            mx_sb_len: sense.len() as u8,
            iovec_count: 0,
            dxfer_len: len,
            dxferp: ptr,
            cmdp: cdb.as_ptr(),
            sbp: sense.as_mut_ptr(),
            timeout: timeout.as_millis().clamp(1, u32::MAX as u128) as u32,
            flags: 0,
            pack_id: 0,
            usr_ptr: std::ptr::null_mut(),
            status: 0,
            masked_status: 0,
            msg_status: 0,
            sb_len_wr: 0,
            host_status: 0,
            driver_status: 0,
            resid: 0,
            duration: 0,
            info: 0,
        };
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), SG_IO as _, &mut hdr) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let has_sense = hdr.sb_len_wr > 0
            && (hdr.status == STATUS_CHECK_CONDITION || hdr.driver_status & DRIVER_SENSE != 0);
        if hdr.host_status != 0 && !has_sense {
            return Err(ScsiError::Io(std::io::Error::other(format!(
                "SG_IO host status {:#06x}",
                hdr.host_status
            ))));
        }
        let parsed = has_sense
            .then(|| Sense::parse(&sense[..hdr.sb_len_wr as usize]))
            .flatten();
        let status = if parsed.is_some() { STATUS_CHECK_CONDITION } else { hdr.status };
        Ok(CommandResult {
            status,
            sense: parsed,
            transferred: (len as i64 - hdr.resid as i64).max(0) as usize,
        })
    }
}
