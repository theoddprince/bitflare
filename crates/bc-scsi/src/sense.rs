use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sense {
    pub key: u8,
    pub asc: u8,
    pub ascq: u8,
}

pub mod key {
    pub const NO_SENSE: u8 = 0x0;
    pub const RECOVERED_ERROR: u8 = 0x1;
    pub const NOT_READY: u8 = 0x2;
    pub const MEDIUM_ERROR: u8 = 0x3;
    pub const HARDWARE_ERROR: u8 = 0x4;
    pub const ILLEGAL_REQUEST: u8 = 0x5;
    pub const UNIT_ATTENTION: u8 = 0x6;
    pub const DATA_PROTECT: u8 = 0x7;
    pub const BLANK_CHECK: u8 = 0x8;
    pub const ABORTED_COMMAND: u8 = 0xB;
}

impl Sense {
    /// Parses fixed (70h/71h) or descriptor (72h/73h) format sense data.
    pub fn parse(data: &[u8]) -> Option<Sense> {
        let code = *data.first()? & 0x7F;
        match code {
            0x70 | 0x71 if data.len() >= 14 => Some(Sense {
                key: data[2] & 0x0F,
                asc: data[12],
                ascq: data[13],
            }),
            0x70 | 0x71 if data.len() >= 3 => Some(Sense {
                key: data[2] & 0x0F,
                asc: 0,
                ascq: 0,
            }),
            0x72 | 0x73 if data.len() >= 4 => Some(Sense {
                key: data[1] & 0x0F,
                asc: data[2],
                ascq: data[3],
            }),
            _ => None,
        }
    }

    pub fn is_medium_not_present(&self) -> bool {
        self.key == key::NOT_READY && self.asc == 0x3A
    }

    pub fn is_becoming_ready(&self) -> bool {
        self.key == key::NOT_READY && self.asc == 0x04 && self.ascq == 0x01
    }

    pub fn is_invalid_opcode(&self) -> bool {
        self.key == key::ILLEGAL_REQUEST && self.asc == 0x20
    }

    pub fn key_name(&self) -> &'static str {
        match self.key {
            key::NO_SENSE => "NO SENSE",
            key::RECOVERED_ERROR => "RECOVERED ERROR",
            key::NOT_READY => "NOT READY",
            key::MEDIUM_ERROR => "MEDIUM ERROR",
            key::HARDWARE_ERROR => "HARDWARE ERROR",
            key::ILLEGAL_REQUEST => "ILLEGAL REQUEST",
            key::UNIT_ATTENTION => "UNIT ATTENTION",
            key::DATA_PROTECT => "DATA PROTECT",
            key::BLANK_CHECK => "BLANK CHECK",
            key::ABORTED_COMMAND => "ABORTED COMMAND",
            _ => "UNKNOWN SENSE KEY",
        }
    }

    pub fn description(&self) -> Option<&'static str> {
        Some(match (self.asc, self.ascq) {
            (0x04, 0x00) => "logical unit not ready, cause not reportable",
            (0x04, 0x01) => "logical unit is becoming ready",
            (0x04, 0x04) => "format in progress",
            (0x04, 0x07) => "operation in progress",
            (0x04, 0x08) => "long write in progress",
            (0x0C, 0x00) => "write error",
            (0x11, 0x00) => "unrecovered read error",
            (0x11, 0x05) => "L-EC uncorrectable error",
            (0x11, 0x06) => "CIRC unrecovered error",
            (0x20, 0x00) => "invalid command operation code",
            (0x21, 0x00) => "logical block address out of range",
            (0x21, 0x02) => "invalid address for write",
            (0x24, 0x00) => "invalid field in CDB",
            (0x26, 0x00) => "invalid field in parameter list",
            (0x27, 0x00) => "write protected",
            (0x28, 0x00) => "medium may have changed",
            (0x29, 0x00) => "power on, reset, or bus device reset occurred",
            (0x30, 0x00) => "incompatible medium installed",
            (0x30, 0x05) => "cannot write - incompatible format",
            (0x3A, 0x00) => "medium not present",
            (0x3A, 0x01) => "medium not present - tray closed",
            (0x3A, 0x02) => "medium not present - tray open",
            (0x6F, 0x00) => "copy protection key exchange failure - authentication failure",
            (0x6F, 0x01) => "copy protection key exchange failure - key not present",
            (0x6F, 0x02) => "copy protection key exchange failure - key not established",
            (0x6F, 0x03) => "read of scrambled sector without authentication",
            (0x72, 0x00) => "session fixation error",
            (0x73, 0x00) => "CD control error",
            (0x73, 0x02) => "power calibration area is full",
            (0x73, 0x03) => "power calibration area error",
            _ => return None,
        })
    }
}

impl fmt::Display for Sense {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (sense {:X}/{:02X}/{:02X})",
            self.key_name(),
            self.key,
            self.asc,
            self.ascq
        )?;
        if let Some(d) = self.description() {
            write!(f, ": {d}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_format() {
        let mut d = [0u8; 18];
        d[0] = 0x70;
        d[2] = 0x02;
        d[12] = 0x3A;
        d[13] = 0x01;
        let s = Sense::parse(&d).unwrap();
        assert!(s.is_medium_not_present());
        assert_eq!(
            s.to_string(),
            "NOT READY (sense 2/3A/01): medium not present - tray closed"
        );
    }

    #[test]
    fn descriptor_format() {
        let s = Sense::parse(&[0x72, 0x05, 0x20, 0x00, 0, 0, 0, 0]).unwrap();
        assert!(s.is_invalid_opcode());
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(Sense::parse(&[]), None);
        assert_eq!(Sense::parse(&[0x00, 0, 0]), None);
    }
}
