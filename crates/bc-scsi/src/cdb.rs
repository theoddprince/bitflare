//! CDB builders for the MMC-6 commands burncraft issues.

pub mod op {
    pub const TEST_UNIT_READY: u8 = 0x00;
    pub const REQUEST_SENSE: u8 = 0x03;
    pub const INQUIRY: u8 = 0x12;
    pub const START_STOP_UNIT: u8 = 0x1B;
    pub const PREVENT_ALLOW_MEDIUM_REMOVAL: u8 = 0x1E;
    pub const READ_CAPACITY: u8 = 0x25;
    pub const READ_10: u8 = 0x28;
    pub const WRITE_10: u8 = 0x2A;
    pub const SYNCHRONIZE_CACHE: u8 = 0x35;
    pub const CLOSE_TRACK_SESSION: u8 = 0x5B;
    pub const SET_CD_SPEED: u8 = 0xBB;
    pub const GET_PERFORMANCE: u8 = 0xAC;
    pub const READ_TOC_PMA_ATIP: u8 = 0x43;
    pub const GET_CONFIGURATION: u8 = 0x46;
    pub const GET_EVENT_STATUS_NOTIFICATION: u8 = 0x4A;
    pub const READ_DISC_INFORMATION: u8 = 0x51;
    pub const READ_TRACK_INFORMATION: u8 = 0x52;
    pub const MODE_SENSE_10: u8 = 0x5A;
    pub const READ_DISC_STRUCTURE: u8 = 0xAD;
    pub const READ_CD: u8 = 0xBE;
}

pub fn test_unit_ready() -> [u8; 6] {
    [op::TEST_UNIT_READY, 0, 0, 0, 0, 0]
}

pub fn inquiry(alloc: u8) -> [u8; 6] {
    [op::INQUIRY, 0, 0, 0, alloc, 0]
}

/// `start=false, load_eject=true` ejects; `start=true, load_eject=true` closes the tray.
pub fn start_stop_unit(start: bool, load_eject: bool) -> [u8; 6] {
    [
        op::START_STOP_UNIT,
        0,
        0,
        0,
        ((load_eject as u8) << 1) | start as u8,
        0,
    ]
}

pub fn prevent_allow_medium_removal(prevent: bool) -> [u8; 6] {
    [op::PREVENT_ALLOW_MEDIUM_REMOVAL, 0, 0, 0, prevent as u8, 0]
}

pub fn read_capacity() -> [u8; 10] {
    [op::READ_CAPACITY, 0, 0, 0, 0, 0, 0, 0, 0, 0]
}

pub fn read_10(lba: u32, blocks: u16) -> [u8; 10] {
    let l = lba.to_be_bytes();
    let b = blocks.to_be_bytes();
    [op::READ_10, 0, l[0], l[1], l[2], l[3], 0, b[0], b[1], 0]
}

pub fn write_10(lba: u32, blocks: u16) -> [u8; 10] {
    let l = lba.to_be_bytes();
    let b = blocks.to_be_bytes();
    [op::WRITE_10, 0, l[0], l[1], l[2], l[3], 0, b[0], b[1], 0]
}

pub fn synchronize_cache(immediate: bool) -> [u8; 10] {
    [op::SYNCHRONIZE_CACHE, (immediate as u8) << 1, 0, 0, 0, 0, 0, 0, 0, 0]
}

pub mod close_function {
    pub const TRACK: u8 = 0b001;
    pub const SESSION: u8 = 0b010;
    /// DVD+R: close the session and finalize the disc.
    pub const SESSION_AND_FINALIZE: u8 = 0b101;
    /// BD-R and DVD+R: finalize the disc.
    pub const FINALIZE: u8 = 0b110;
}

pub fn close_track_session(immediate: bool, function: u8, number: u16) -> [u8; 10] {
    let n = number.to_be_bytes();
    [
        op::CLOSE_TRACK_SESSION,
        immediate as u8,
        function & 0x07,
        0,
        n[0],
        n[1],
        0,
        0,
        0,
        0,
    ]
}

/// Speeds are in kB/s (1000 bytes); 0xFFFF asks for the drive's maximum.
pub fn set_cd_speed(read_kbs: u16, write_kbs: u16) -> [u8; 12] {
    let r = read_kbs.to_be_bytes();
    let w = write_kbs.to_be_bytes();
    [op::SET_CD_SPEED, 0, r[0], r[1], w[0], w[1], 0, 0, 0, 0, 0, 0]
}

/// GET PERFORMANCE, type 03h: write speed descriptors for the inserted media.
pub fn get_performance_write_speeds(max_descriptors: u16) -> [u8; 12] {
    let m = max_descriptors.to_be_bytes();
    [op::GET_PERFORMANCE, 0, 0, 0, 0, 0, 0, 0, m[0], m[1], 0x03, 0]
}

pub mod toc_format {
    pub const TOC: u8 = 0x0;
    pub const SESSION_INFO: u8 = 0x1;
    pub const FULL_TOC: u8 = 0x2;
    pub const PMA: u8 = 0x3;
    pub const ATIP: u8 = 0x4;
    pub const CD_TEXT: u8 = 0x5;
}

pub fn read_toc(format: u8, msf: bool, track_or_session: u8, alloc: u16) -> [u8; 10] {
    let a = alloc.to_be_bytes();
    [
        op::READ_TOC_PMA_ATIP,
        (msf as u8) << 1,
        format & 0x0F,
        0,
        0,
        0,
        track_or_session,
        a[0],
        a[1],
        0,
    ]
}

pub mod rt {
    /// All features, current or not.
    pub const ALL: u8 = 0x0;
    /// Only features marked current.
    pub const CURRENT: u8 = 0x1;
    /// Only the feature named by the starting feature number.
    pub const ONE: u8 = 0x2;
}

pub fn get_configuration(rt: u8, start_feature: u16, alloc: u16) -> [u8; 10] {
    let s = start_feature.to_be_bytes();
    let a = alloc.to_be_bytes();
    [
        op::GET_CONFIGURATION,
        rt & 0x03,
        s[0],
        s[1],
        0,
        0,
        0,
        a[0],
        a[1],
        0,
    ]
}

pub fn read_disc_information(alloc: u16) -> [u8; 10] {
    let a = alloc.to_be_bytes();
    [op::READ_DISC_INFORMATION, 0, 0, 0, 0, 0, 0, a[0], a[1], 0]
}

pub fn read_track_information(track: u32, alloc: u16) -> [u8; 10] {
    let t = track.to_be_bytes();
    let a = alloc.to_be_bytes();
    [
        op::READ_TRACK_INFORMATION,
        0x01,
        t[0],
        t[1],
        t[2],
        t[3],
        0,
        a[0],
        a[1],
        0,
    ]
}

/// MODE SENSE(10) with block descriptors disabled, current values.
pub fn mode_sense_10(page: u8, alloc: u16) -> [u8; 10] {
    let a = alloc.to_be_bytes();
    [op::MODE_SENSE_10, 0x08, page & 0x3F, 0, 0, 0, 0, a[0], a[1], 0]
}

pub mod media_type {
    pub const DVD: u8 = 0x0;
    pub const BD: u8 = 0x1;
}

pub fn read_disc_structure(media_type: u8, layer: u8, format: u8, alloc: u16) -> [u8; 12] {
    let a = alloc.to_be_bytes();
    [
        op::READ_DISC_STRUCTURE,
        media_type & 0x0F,
        0,
        0,
        0,
        0,
        layer,
        format,
        a[0],
        a[1],
        0,
        0,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eject_and_load() {
        assert_eq!(start_stop_unit(false, true), [0x1B, 0, 0, 0, 0x02, 0]);
        assert_eq!(start_stop_unit(true, true), [0x1B, 0, 0, 0, 0x03, 0]);
    }

    #[test]
    fn toc_msf_flag_and_alloc() {
        assert_eq!(
            read_toc(toc_format::FULL_TOC, true, 1, 0x1234),
            [0x43, 0x02, 0x02, 0, 0, 0, 1, 0x12, 0x34, 0]
        );
    }

    #[test]
    fn read_10_big_endian() {
        assert_eq!(
            read_10(0x0102_0304, 16),
            [0x28, 0, 1, 2, 3, 4, 0, 0, 16, 0]
        );
    }

    #[test]
    fn write_and_close() {
        assert_eq!(write_10(0x10, 32), [0x2A, 0, 0, 0, 0, 0x10, 0, 0, 32, 0]);
        assert_eq!(
            close_track_session(true, close_function::FINALIZE, 0),
            [0x5B, 1, 6, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(synchronize_cache(true)[1], 0x02);
    }

    #[test]
    fn get_configuration_layout() {
        assert_eq!(
            get_configuration(rt::ONE, 0x0000, 512),
            [0x46, 0x02, 0, 0, 0, 0, 0, 0x02, 0x00, 0]
        );
    }
}
