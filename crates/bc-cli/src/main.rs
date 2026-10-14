use anyhow::{Context, Result, bail};
use bc_drive::{Drive, MediaFamily, MediaState, Profile, TocEntry};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "bitflare-cli", version, about = "Bitflare disc burning suite — diagnostic command-line tool")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List optical drives
    Drives,
    /// Show drive capabilities and the inserted disc
    Info {
        /// Drive (e.g. E:, /dev/sr0). Defaults to the only drive present.
        drive: Option<String>,
    },
    /// Open the tray
    Eject { drive: Option<String> },
    /// Close the tray
    Load { drive: Option<String> },
    /// Check that the drive can be locked for exclusive access (writes nothing)
    LockTest { drive: Option<String> },
    /// Build a UDF 2.50 image file from files and folders
    MakeImage {
        #[arg(long, default_value = "NEW_DISC")]
        label: String,
        output: std::path::PathBuf,
        #[arg(required = true)]
        sources: Vec<std::path::PathBuf>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Drives => drives(),
        Cmd::Info { drive } => info(&resolve(drive)?),
        Cmd::Eject { drive } => Ok(Drive::open(&resolve(drive)?)?.eject()?),
        Cmd::Load { drive } => Ok(Drive::open(&resolve(drive)?)?.load()?),
        Cmd::MakeImage { label, output, sources } => make_image(&label, &output, &sources),
        Cmd::LockTest { drive } => {
            let mut d = Drive::open(&resolve(drive)?)?;
            let t = d.transport();
            println!("Device:   {}", t.device_name().unwrap_or_else(|| "?".into()));
            match t.lock_exclusive() {
                Ok(()) => println!("Lock:     acquired"),
                Err(e) => println!("Lock:     failed ({e})"),
            }
            let inq = d.inquiry()?;
            println!("Owner can still send commands: {} {}", inq.vendor, inq.product);
            let path = resolve(None).unwrap_or_default();
            match Drive::open(&path).and_then(|mut other| other.inquiry()) {
                Ok(_) => println!("Second opener: ALLOWED (lock is not protecting the drive)"),
                Err(e) => println!("Second opener: blocked ({e})"),
            }
            drop(d);
            match Drive::open(&path).and_then(|mut other| other.inquiry()) {
                Ok(_) => println!("After release: second opener allowed again"),
                Err(e) => println!("After release: still blocked ({e})"),
            }
            println!("Lock:     released");
            Ok(())
        }
    }
}

fn make_image(label: &str, output: &std::path::Path, sources: &[std::path::PathBuf]) -> Result<()> {
    let img = bc_fs::udf::build(sources, &bc_fs::udf::UdfOptions::new(label))?;
    let mut out = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(output)?);
    std::io::copy(&mut img.reader(), &mut out)?;
    println!(
        "{} sectors, {} files, {} folders -> {}",
        img.sectors(),
        img.files,
        img.directories,
        output.display()
    );
    Ok(())
}

fn resolve(drive: Option<String>) -> Result<String> {
    if let Some(d) = drive {
        return Ok(d);
    }
    let all = bc_scsi::enumerate()?;
    match all.as_slice() {
        [] => bail!("no optical drives found"),
        [one] => Ok(one.path.clone()),
        _ => bail!(
            "several drives found ({}); name one explicitly",
            all.iter().map(|d| d.label.as_str()).collect::<Vec<_>>().join(", ")
        ),
    }
}

fn drives() -> Result<()> {
    let all = bc_scsi::enumerate()?;
    if all.is_empty() {
        println!("No optical drives found.");
        return Ok(());
    }
    for dev in all {
        let desc = Drive::open(&dev.path)
            .and_then(|mut d| d.inquiry())
            .map(|i| format!("{} {} ({})", i.vendor, i.product, i.revision))
            .unwrap_or_else(|e| format!("<{e}>"));
        println!("{:<6} {}", dev.label, desc);
    }
    Ok(())
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

fn info(path: &str) -> Result<()> {
    let mut d = Drive::open(path).with_context(|| format!("opening {path}"))?;
    let inq = d.inquiry()?;
    println!("Drive:        {} {} (firmware {})", inq.vendor, inq.product, inq.revision);
    if !inq.is_mmc() {
        println!("Warning: peripheral type {:#04x} is not an MMC optical device", inq.peripheral_type);
    }

    let (current, supported) = d.profiles().unwrap_or((Profile::NONE, Vec::new()));
    if !supported.is_empty() {
        let names: Vec<_> = supported
            .iter()
            .filter(|p| !matches!(p.family(), MediaFamily::None | MediaFamily::Other))
            .map(|p| p.name())
            .collect();
        println!("Media types:  {}", names.join(", "));
    }

    match d.cd_capabilities() {
        Ok(c) => {
            println!(
                "CD features:  audio extraction {}, accurate stream {}, C2 pointers {}, R-W subchannel {}, underrun protection {}",
                yes(c.cdda_supported),
                yes(c.cdda_accurate),
                yes(c.c2_pointers),
                yes(c.rw_subchannel),
                yes(c.buffer_underrun_protection)
            );
        }
        Err(e) => println!("CD features:  unavailable ({e})"),
    }

    println!();
    match d.media_state()? {
        MediaState::NoMedium => {
            println!("Disc:         none");
            return Ok(());
        }
        MediaState::BecomingReady => {
            println!("Disc:         spinning up - try again in a moment");
            return Ok(());
        }
        MediaState::NotReady(s) => {
            println!("Disc:         not ready ({s})");
            return Ok(());
        }
        MediaState::Ready => {}
    }

    println!(
        "Disc:         {}{}{}",
        current.name(),
        if current.is_rewritable() { ", rewritable" } else { "" },
        if current.is_recordable() && !current.is_rewritable() { ", recordable" } else { "" }
    );

    let di = d.disc_info().ok();
    if let Some(di) = &di {
        println!(
            "Status:       {:?}, last session {:?}, {} session(s), tracks {}-{}",
            di.status, di.last_session, di.sessions, di.first_track, di.last_track_last_session
        );
    }
    if let Some(cap) = d.capacity().ok().filter(|c| c.last_lba > 0) {
        println!("Used:         {} blocks ({})", cap.last_lba as u64 + 1, mib(cap.bytes()));
    }
    if let Some(di) = di.filter(|di| di.is_appendable())
        && let Ok(free) = d.free_blocks(&di)
    {
        println!("Free:         {} blocks ({})", free, mib(free as u64 * 2048));
    }
    if current.family() == MediaFamily::Dvd {
        match d.dvd_copy_protection() {
            Ok(p) if p.is_protected() => println!(
                "Protection:   {} - burncraft will not copy this disc",
                p.name()
            ),
            Ok(_) => println!("Protection:   none"),
            Err(e) => println!("Protection:   unknown ({e})"),
        }
    }

    match d.write_speeds() {
        Ok(v) if !v.is_empty() => {
            let one_x = match current.family() {
                MediaFamily::Bd => 4495.0,
                MediaFamily::Dvd => 1385.0,
                _ => 176.4,
            };
            let list: Vec<_> = v
                .iter()
                .map(|&k| format!("{:.1}x ({:.1} MB/s)", k as f64 / one_x, k as f64 / 1000.0))
                .collect();
            println!("Write speeds: {}", list.join(", "));
        }
        Ok(_) => println!("Write speeds: none reported for this disc"),
        Err(e) => println!("Write speeds: unavailable ({e})"),
    }
    println!("Write access: {}", if d.transport().can_write() { "yes" } else { "no (run as administrator)" });
    match bc_burn::inspect_target(&mut d) {
        Ok(t) => println!("Burn target:  {} with {} free sectors", t.profile.name(), t.capacity_sectors),
        Err(e) => println!("Burn target:  not usable ({e})"),
    }

    if let Ok(toc) = d.toc() {
        println!();
        println!("Track  Type   Start LBA   Length");
        let lengths = toc.track_lengths();
        for (t, (_, len)) in toc.tracks.iter().zip(lengths) {
            println!(
                "{:>5}  {:<5}  {:>9}  {:>7}",
                t.number,
                if t.is_data() { "data" } else { "audio" },
                t.lba,
                len
            );
        }
        if let Some(TocEntry { lba, .. }) = toc.leadout {
            println!("lead-out      {:>9}", lba);
        }
    }
    Ok(())
}

fn mib(bytes: u64) -> String {
    format!("{:.2} MiB", bytes as f64 / (1024.0 * 1024.0))
}
