//! Collects everything the Info page shows about a drive and its disc.

use bc_drive::{DiscStatus, Drive, MediaFamily, MediaState, Profile};

pub struct Report {
    pub drive_rows: Vec<(String, String)>,
    pub disc_rows: Vec<(String, String)>,
    pub tracks: Vec<[String; 5]>,
    pub summary: String,
    pub free_bytes: Option<u64>,
    pub has_disc: bool,
    pub profile: Profile,
    /// Write speeds in kB/s supported for the inserted disc, fastest first.
    pub write_speeds: Vec<u32>,
}

pub fn one_x_kbs(profile: Profile) -> f64 {
    match profile.family() {
        MediaFamily::Bd => 4495.0,
        MediaFamily::Dvd => 1385.0,
        _ => 176.4,
    }
}

pub fn speed_label(kbs: u32, profile: Profile) -> String {
    let x = kbs as f64 / one_x_kbs(profile);
    let x = if (x - x.round()).abs() < 0.05 { format!("{:.0}", x.round()) } else { format!("{x:.1}") };
    format!("{x}x  ({:.1} MB/s)", kbs as f64 * 1000.0 / (1024.0 * 1024.0))
}

/// Sizes in the same units Windows Explorer shows (1 GB = 1024³ bytes).
pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} bytes")
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

fn row(k: &str, v: impl Into<String>) -> (String, String) {
    (k.to_string(), v.into())
}

fn yes(b: bool) -> &'static str {
    if b { "Yes" } else { "No" }
}

fn family_list(profiles: &[Profile], family: MediaFamily, writable: bool) -> Option<String> {
    let names: Vec<_> = profiles
        .iter()
        .filter(|p| p.family() == family && (!writable || p.is_recordable()))
        .map(|p| p.name())
        .collect();
    (!names.is_empty()).then(|| names.join(", "))
}

pub fn gather(label: &str, path: &str) -> Result<Report, String> {
    let mut d = Drive::open(path).map_err(|e| format!("Cannot open {label}: {e}"))?;
    let inq = d.inquiry().map_err(|e| format!("{label} did not answer: {e}"))?;
    let (current, profiles) = d.profiles().unwrap_or((Profile::NONE, Vec::new()));

    let mut drive_rows = vec![
        row("Model", format!("{} {}", inq.vendor, inq.product)),
        row("Firmware", inq.revision.clone()),
        row("Device", label),
    ];
    let reads: Vec<_> = [(MediaFamily::Cd, "CD"), (MediaFamily::Dvd, "DVD"), (MediaFamily::Bd, "Blu-ray")]
        .into_iter()
        .filter(|(f, _)| profiles.iter().any(|p| p.family() == *f))
        .map(|(_, n)| n)
        .collect();
    if !reads.is_empty() {
        drive_rows.push(row("Reads", reads.join(", ")));
    }
    for (fam, name) in [(MediaFamily::Cd, "Writes CD"), (MediaFamily::Dvd, "Writes DVD"), (MediaFamily::Bd, "Writes Blu-ray")] {
        if let Some(list) = family_list(&profiles, fam, true) {
            drive_rows.push(row(name, list));
        }
    }
    if let Ok(c) = d.cd_capabilities() {
        let mut audio = vec![];
        if c.cdda_supported {
            audio.push("supported");
        }
        if c.cdda_accurate {
            audio.push("accurate stream");
        }
        if c.c2_pointers {
            audio.push("C2 error pointers");
        }
        drive_rows.push(row(
            "Audio extraction",
            if audio.is_empty() { "No".to_string() } else { audio.join(", ") },
        ));
        drive_rows.push(row("Underrun protection", yes(c.buffer_underrun_protection)));
        drive_rows.push(row("Subchannel R-W", yes(c.rw_subchannel)));
    }

    let mut report = Report {
        drive_rows,
        disc_rows: Vec::new(),
        tracks: Vec::new(),
        summary: String::new(),
        free_bytes: None,
        has_disc: false,
        profile: current,
        write_speeds: Vec::new(),
    };

    let state = d.media_state().map_err(|e| e.to_string())?;
    let not_ready = match state {
        MediaState::Ready => None,
        MediaState::NoMedium => Some("No disc inserted".to_string()),
        MediaState::BecomingReady => Some("Disc is spinning up…".to_string()),
        MediaState::NotReady(s) => Some(format!("Not ready ({s})")),
    };
    if let Some(msg) = not_ready {
        report.disc_rows.push(row("Status", msg.clone()));
        report.summary = msg;
        return Ok(report);
    }
    report.has_disc = true;

    let mut kind = current.name().to_string();
    if current.is_rewritable() {
        kind.push_str("  (rewritable)");
    } else if current.is_recordable() {
        kind.push_str("  (recordable)");
    }
    report.disc_rows.push(row("Media", kind));

    let di = d.disc_info().ok();
    if let Some(di) = &di {
        let status = match di.status {
            DiscStatus::Empty => "Blank",
            DiscStatus::Incomplete => "Appendable (open session)",
            DiscStatus::Finalized => "Finalized",
            DiscStatus::Other => "Other",
        };
        report.disc_rows.push(row("State", status));
        if di.status != DiscStatus::Empty {
            report.disc_rows.push(row("Sessions", di.sessions.to_string()));
            report
                .disc_rows
                .push(row("Tracks", format!("{} – {}", di.first_track, di.last_track_last_session)));
        }
        report.disc_rows.push(row("Erasable", yes(di.erasable)));
    }

    if let Some(cap) = d.capacity().ok().filter(|c| c.last_lba > 0) {
        report.disc_rows.push(row("Used", human_bytes(cap.bytes())));
    }
    if let Some(di) = di.as_ref().filter(|di| di.is_appendable())
        && let Ok(free) = d.free_blocks(di)
    {
        let bytes = free as u64 * 2048;
        report.free_bytes = Some(bytes);
        report.disc_rows.push(row("Free space", human_bytes(bytes)));
    }

    if current.is_recordable() {
        report.write_speeds = d.write_speeds().unwrap_or_default();
        if !report.write_speeds.is_empty() {
            let list: Vec<_> = report.write_speeds.iter().map(|&k| speed_label(k, current)).collect();
            report.disc_rows.push(row("Write speeds", list.join(", ")));
        }
    }

    if current.family() == MediaFamily::Dvd {
        let p = match d.dvd_copy_protection() {
            Ok(p) if p.is_protected() => format!("{} — this disc cannot be copied", p.name()),
            Ok(_) => "None".to_string(),
            Err(_) => "Unknown".to_string(),
        };
        report.disc_rows.push(row("Copy protection", p));
    }

    if di.as_ref().is_some_and(|di| di.status != DiscStatus::Empty)
        && let Ok(toc) = d.toc()
    {
        for (t, (_, len)) in toc.tracks.iter().zip(toc.track_lengths()) {
            let sector = if t.is_data() { 2048 } else { 2352 };
            report.tracks.push([
                t.number.to_string(),
                if t.is_data() { "Data" } else { "Audio" }.to_string(),
                t.lba.to_string(),
                len.to_string(),
                human_bytes(len as u64 * sector),
            ]);
        }
    }

    report.summary = match (&di, report.free_bytes) {
        (Some(di), Some(free)) if di.status == DiscStatus::Empty => {
            format!("{} · blank · {} free", current.name(), human_bytes(free))
        }
        (_, Some(free)) => format!("{} · {} free", current.name(), human_bytes(free)),
        _ => format!("{} · {} track(s)", current.name(), report.tracks.len()),
    };
    Ok(report)
}

/// Cheap check used by the background poller to notice disc changes.
pub fn has_disc(path: &str) -> Option<bool> {
    let mut d = Drive::open(path).ok()?;
    match d.media_state().ok()? {
        MediaState::Ready => Some(true),
        MediaState::NoMedium => Some(false),
        _ => None,
    }
}
