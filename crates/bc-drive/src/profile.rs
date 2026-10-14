use std::fmt;

/// MMC media profile, as reported by GET CONFIGURATION.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Profile(pub u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaFamily {
    None,
    Cd,
    Dvd,
    Bd,
    HdDvd,
    Other,
}

impl Profile {
    pub const NONE: Profile = Profile(0x0000);
    pub const CD_ROM: Profile = Profile(0x0008);
    pub const CD_R: Profile = Profile(0x0009);
    pub const CD_RW: Profile = Profile(0x000A);
    pub const DVD_ROM: Profile = Profile(0x0010);
    pub const DVD_R_SEQ: Profile = Profile(0x0011);
    pub const DVD_RAM: Profile = Profile(0x0012);
    pub const DVD_RW_RO: Profile = Profile(0x0013);
    pub const DVD_RW_SEQ: Profile = Profile(0x0014);
    pub const DVD_R_DL_SEQ: Profile = Profile(0x0015);
    pub const DVD_R_DL_JUMP: Profile = Profile(0x0016);
    pub const DVD_PLUS_RW: Profile = Profile(0x001A);
    pub const DVD_PLUS_R: Profile = Profile(0x001B);
    pub const DVD_PLUS_RW_DL: Profile = Profile(0x002A);
    pub const DVD_PLUS_R_DL: Profile = Profile(0x002B);
    pub const BD_ROM: Profile = Profile(0x0040);
    pub const BD_R_SRM: Profile = Profile(0x0041);
    pub const BD_R_RRM: Profile = Profile(0x0042);
    pub const BD_RE: Profile = Profile(0x0043);
    pub const HD_DVD_ROM: Profile = Profile(0x0050);
    pub const HD_DVD_R: Profile = Profile(0x0051);
    pub const HD_DVD_RAM: Profile = Profile(0x0052);

    pub fn name(self) -> &'static str {
        match self.0 {
            0x0000 => "no media",
            0x0008 => "CD-ROM",
            0x0009 => "CD-R",
            0x000A => "CD-RW",
            0x0010 => "DVD-ROM",
            0x0011 => "DVD-R",
            0x0012 => "DVD-RAM",
            0x0013 => "DVD-RW (restricted overwrite)",
            0x0014 => "DVD-RW (sequential)",
            0x0015 => "DVD-R DL (sequential)",
            0x0016 => "DVD-R DL (layer jump)",
            0x001A => "DVD+RW",
            0x001B => "DVD+R",
            0x002A => "DVD+RW DL",
            0x002B => "DVD+R DL",
            0x0040 => "BD-ROM",
            0x0041 => "BD-R (SRM)",
            0x0042 => "BD-R (RRM)",
            0x0043 => "BD-RE",
            0x0050 => "HD DVD-ROM",
            0x0051 => "HD DVD-R",
            0x0052 => "HD DVD-RAM",
            0x0001..=0x0005 => "non-optical / MO",
            0xFFFF => "non-standard",
            _ => "unknown",
        }
    }

    pub fn family(self) -> MediaFamily {
        match self.0 {
            0x0000 => MediaFamily::None,
            0x0008..=0x000A => MediaFamily::Cd,
            0x0010..=0x002B => MediaFamily::Dvd,
            0x0040..=0x0043 => MediaFamily::Bd,
            0x0050..=0x005A => MediaFamily::HdDvd,
            _ => MediaFamily::Other,
        }
    }

    /// Media that can be written at least once.
    pub fn is_recordable(self) -> bool {
        !matches!(
            self,
            Profile::NONE | Profile::CD_ROM | Profile::DVD_ROM | Profile::BD_ROM | Profile::HD_DVD_ROM
        ) && self.family() != MediaFamily::Other
    }

    /// Media that can be erased or overwritten.
    pub fn is_rewritable(self) -> bool {
        matches!(
            self,
            Profile::CD_RW
                | Profile::DVD_RAM
                | Profile::DVD_RW_RO
                | Profile::DVD_RW_SEQ
                | Profile::DVD_PLUS_RW
                | Profile::DVD_PLUS_RW_DL
                | Profile::BD_RE
                | Profile::HD_DVD_RAM
        )
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        assert_eq!(Profile::DVD_PLUS_R_DL.family(), MediaFamily::Dvd);
        assert!(Profile::BD_RE.is_rewritable());
        assert!(Profile::CD_R.is_recordable());
        assert!(!Profile::CD_R.is_rewritable());
        assert!(!Profile::BD_ROM.is_recordable());
        assert!(!Profile::NONE.is_recordable());
    }
}
