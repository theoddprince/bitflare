#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod jobs;
mod report;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bc_burn::{BD_1X_KBS, BurnSettings, DVD_1X_KBS};
use bc_drive::{MediaFamily, Profile};
use bc_scsi::DevicePath;
use report::{Report, human_bytes};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

slint::include_modules!();

/// Media presets of the Data Disc capacity selector, in user-data bytes. Index 0 is "inserted disc".
const MEDIA_BYTES: [u64; 6] = [
    0,
    360_000 * 2048,
    2_295_104 * 2048,
    4_173_824 * 2048,
    12_219_392 * 2048,
    24_438_784 * 2048,
];

struct DataEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    bytes: u64,
    files: u64,
}

#[derive(Default)]
struct AppState {
    drives: Vec<DevicePath>,
    selected: usize,
    busy: bool,
    disc_free: Option<u64>,
    has_disc: Option<bool>,
    disc_profile: Option<Profile>,
    write_speeds: Vec<u32>,
    data: Vec<DataEntry>,
    speeds: Vec<Option<u16>>,
    job_cancel: Arc<AtomicBool>,
    /// Full log of the current or last job, for "Copy Log".
    job_log: Vec<String>,
}

type Shared = Arc<Mutex<AppState>>;

/// Write speed choices for the inserted disc, with the recommended default index.
/// Uses the speeds the drive reports for this disc; falls back to common values otherwise.
fn speed_choices(profile: Option<Profile>, reported: &[u32]) -> (Vec<(String, Option<u16>)>, usize) {
    let profile = profile.unwrap_or(Profile::BD_R_SRM);
    let speeds: Vec<u32> = if reported.is_empty() {
        let (unit, steps): (f64, &[f64]) = match profile.family() {
            MediaFamily::Bd => (BD_1X_KBS as f64, &[6.0, 4.0, 2.0]),
            _ => (DVD_1X_KBS as f64, &[16.0, 8.0, 4.0, 2.4]),
        };
        steps.iter().map(|x| (x * unit).round() as u32).collect()
    } else {
        reported.to_vec()
    };
    let max_label = if reported.is_empty() {
        "Maximum".to_string()
    } else {
        format!("Maximum  ({})", report::speed_label(speeds[0], profile).split("  ").next().unwrap_or(""))
    };
    let mut v = vec![(max_label, None)];
    for &k in &speeds {
        v.push((report::speed_label(k, profile), Some(k.min(u16::MAX as u32) as u16)));
    }
    // Recommend the speed closest to 4x on Blu-ray and 8x on DVD: reliable without being slow.
    let target = report::one_x_kbs(profile) * if profile.family() == MediaFamily::Bd { 4.0 } else { 8.0 };
    let default = speeds
        .iter()
        .enumerate()
        .min_by_key(|(_, k)| ((**k as f64 - target).abs()) as u64)
        .map(|(i, _)| i + 1)
        .unwrap_or(0);
    (v, default)
}

fn volume_label(ui: &AppWindow) -> String {
    let l = ui.get_volume_label().trim().to_string();
    if l.is_empty() { "NEW_DISC".into() } else { l }
}

fn table(rows: impl IntoIterator<Item = Vec<String>>) -> ModelRc<ModelRc<SharedString>> {
    let rows: Vec<ModelRc<SharedString>> = rows
        .into_iter()
        .map(|r| ModelRc::new(VecModel::from(r.into_iter().map(SharedString::from).collect::<Vec<_>>())))
        .collect();
    ModelRc::new(VecModel::from(rows))
}

fn info_rows(rows: &[(String, String)]) -> ModelRc<InfoRow> {
    ModelRc::new(VecModel::from(
        rows.iter()
            .map(|(k, v)| InfoRow {
                key: k.into(),
                value: v.into(),
            })
            .collect::<Vec<_>>(),
    ))
}

pub(crate) fn set_busy(ui: &AppWindow, state: &Shared, busy: bool, status: &str) {
    state.lock().unwrap().busy = busy;
    ui.set_busy(busy);
    ui.set_status_text(status.into());
    update_data_view(ui, state);
}

fn current_drive(state: &Shared) -> Option<DevicePath> {
    let s = state.lock().unwrap();
    s.drives.get(s.selected).cloned()
}

fn refresh_drives(weak: Weak<AppWindow>, state: Shared) {
    if let Some(ui) = weak.upgrade() {
        set_busy(&ui, &state, true, "Searching for optical drives…");
    }
    std::thread::spawn(move || {
        let found = bc_scsi::enumerate();
        let named: Vec<(DevicePath, String)> = match &found {
            Ok(list) => list
                .iter()
                .map(|dev| {
                    let model = bc_drive::Drive::open(&dev.path)
                        .and_then(|mut d| d.inquiry())
                        .map(|i| format!("{} {}", i.vendor, i.product))
                        .unwrap_or_else(|_| "Optical drive".into());
                    (dev.clone(), format!("{}   {}", dev.label, model))
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        let error = found.err().map(|e| e.to_string());
        let st = state.clone();
        let _ = weak.clone().upgrade_in_event_loop(move |ui| {
            {
                let mut s = st.lock().unwrap();
                s.drives = named.iter().map(|(d, _)| d.clone()).collect();
                s.selected = s.selected.min(s.drives.len().saturating_sub(1));
            }
            let names: Vec<SharedString> = named.iter().map(|(_, n)| n.into()).collect();
            ui.set_drive_names(ModelRc::new(VecModel::from(names)));
            let sel = st.lock().unwrap().selected;
            ui.set_drive_index(sel as i32);
            ui.set_drive_ok(!named.is_empty());
            set_busy(&ui, &st, false, "Ready");
            match (error, named.is_empty()) {
                (Some(e), _) => {
                    ui.set_status_text(format!("Drive detection failed: {e}").into());
                    ui.set_disc_summary("No drive".into());
                }
                (None, true) => {
                    ui.set_status_text("No optical drives found.".into());
                    ui.set_disc_summary("No drive".into());
                    ui.set_drive_rows(info_rows(&[("Status".into(), "No optical drive detected".into())]));
                    ui.set_disc_rows(info_rows(&[]));
                }
                (None, false) => load_info(ui.as_weak(), st),
            }
        });
    });
}

pub(crate) fn load_info(weak: Weak<AppWindow>, state: Shared) {
    let Some(dev) = current_drive(&state) else { return };
    if let Some(ui) = weak.upgrade() {
        set_busy(&ui, &state, true, &format!("Reading drive {}…", dev.label));
    }
    std::thread::spawn(move || {
        let result = report::gather(&dev.label, &dev.path);
        let st = state.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            match result {
                Ok(r) => apply_report(&ui, &st, r),
                Err(e) => {
                    ui.set_drive_rows(info_rows(&[("Error".into(), e.clone())]));
                    ui.set_disc_rows(info_rows(&[]));
                    ui.set_track_rows(table(Vec::new()));
                    ui.set_disc_summary("Drive unavailable".into());
                    {
                        let mut s = st.lock().unwrap();
                        s.disc_free = None;
                        s.disc_profile = None;
                        s.write_speeds.clear();
                    }
                    set_busy(&ui, &st, false, &e);
                    update_data_view(&ui, &st);
                    return;
                }
            }
            set_busy(&ui, &st, false, "Ready");
        });
    });
}

fn apply_report(ui: &AppWindow, state: &Shared, r: Report) {
    ui.set_drive_rows(info_rows(&r.drive_rows));
    ui.set_disc_rows(info_rows(&r.disc_rows));
    ui.set_track_rows(table(r.tracks.iter().map(|t| t.to_vec())));
    ui.set_disc_summary(r.summary.into());
    {
        let mut s = state.lock().unwrap();
        s.disc_free = r.free_bytes;
        s.has_disc = Some(r.has_disc);
        s.disc_profile = r.has_disc.then_some(r.profile);
        s.write_speeds = r.write_speeds.clone();
    }
    update_data_view(ui, state);
}

fn eject(weak: Weak<AppWindow>, state: Shared) {
    let Some(dev) = current_drive(&state) else { return };
    if let Some(ui) = weak.upgrade() {
        set_busy(&ui, &state, true, "Ejecting…");
    }
    std::thread::spawn(move || {
        let r = bc_drive::Drive::open(&dev.path).and_then(|mut d| d.eject());
        let st = state.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| match r {
            Ok(()) => load_info(ui.as_weak(), st),
            Err(e) => set_busy(&ui, &st, false, &format!("Eject failed: {e}")),
        });
    });
}

fn dir_size(path: &Path) -> (u64, u64) {
    let mut bytes = 0;
    let mut files = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&p) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(e.path());
            } else if let Ok(m) = e.metadata() {
                bytes += m.len();
                files += 1;
            }
        }
    }
    (bytes, files)
}

fn update_data_view(ui: &AppWindow, state: &Shared) {
    let s = state.lock().unwrap();
    let rows = s.data.iter().map(|e| {
        vec![
            e.name.clone(),
            if e.is_dir { format!("Folder ({} files)", e.files) } else { "File".into() },
            human_bytes(e.bytes),
            e.path.parent().map(|p| p.display().to_string()).unwrap_or_default(),
        ]
    });
    ui.set_data_rows(table(rows));
    let total: u64 = s.data.iter().map(|e| e.bytes).sum();
    ui.set_data_total_text(human_bytes(total).into());

    let media = ui.get_media_index().max(0) as usize;
    let capacity = if media == 0 { s.disc_free } else { MEDIA_BYTES.get(media).copied() };
    let over = match capacity {
        Some(cap) if cap > 0 => {
            ui.set_data_capacity_text(human_bytes(cap).into());
            ui.set_data_fill((total as f64 / cap as f64) as f32);
            total > cap
        }
        _ => {
            ui.set_data_capacity_text(if media == 0 { "no writable disc" } else { "—" }.into());
            ui.set_data_fill(0.0);
            false
        }
    };
    ui.set_data_over(over);
    let fits_disc = s.disc_free.is_some_and(|free| total <= free);
    ui.set_can_burn(!s.busy && !s.data.is_empty() && fits_disc);
    let hint = if s.busy {
        "Please wait — a task is running."
    } else if over {
        "The compilation is larger than the target disc. Remove some items."
    } else if s.data.is_empty() {
        "Add files or folders to start."
    } else if fits_disc {
        "Ready to burn."
    } else if s.disc_free.is_some() {
        "The compilation does not fit on the inserted disc."
    } else if s.has_disc == Some(true) {
        "The inserted disc is finalized or not writable. Insert a blank or rewritable disc to burn."
    } else {
        "Insert a blank or rewritable disc to burn."
    };
    ui.set_burn_hint(hint.into());
    if ui.get_data_selected() >= s.data.len() as i32 {
        ui.set_data_selected(-1);
    }
}

fn add_paths(weak: Weak<AppWindow>, state: Shared, paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    if let Some(ui) = weak.upgrade() {
        ui.set_status_text("Calculating sizes…".into());
    }
    std::thread::spawn(move || {
        let entries: Vec<DataEntry> = paths
            .into_iter()
            .filter_map(|p| {
                let meta = std::fs::metadata(&p).ok()?;
                let (bytes, files) = if meta.is_dir() { dir_size(&p) } else { (meta.len(), 1) };
                Some(DataEntry {
                    name: p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string()),
                    is_dir: meta.is_dir(),
                    path: p,
                    bytes,
                    files,
                })
            })
            .collect();
        let st = state.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let added = {
                let mut s = st.lock().unwrap();
                let before = s.data.len();
                for e in entries {
                    if !s.data.iter().any(|x| x.path == e.path) {
                        s.data.push(e);
                    }
                }
                s.data.len() - before
            };
            update_data_view(&ui, &st);
            ui.set_status_text(format!("Added {added} item(s) to the compilation.").into());
        });
    });
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());
    let state: Shared = Arc::new(Mutex::new(AppState::default()));

    {
        let (w, s) = (ui.as_weak(), state.clone());
        ui.on_refresh_drives(move || refresh_drives(w.clone(), s.clone()));
    }
    {
        let (w, s) = (ui.as_weak(), state.clone());
        ui.on_drive_changed(move |i| {
            {
                let mut st = s.lock().unwrap();
                st.selected = i.max(0) as usize;
                st.has_disc = None;
            }
            load_info(w.clone(), s.clone());
        });
    }
    {
        let (w, s) = (ui.as_weak(), state.clone());
        ui.on_eject(move || eject(w.clone(), s.clone()));
    }
    {
        let (w, s) = (ui.as_weak(), state.clone());
        ui.on_add_files(move || {
            let files = rfd::FileDialog::new().set_title("Add files to compilation").pick_files().unwrap_or_default();
            add_paths(w.clone(), s.clone(), files);
        });
    }
    {
        let (w, s) = (ui.as_weak(), state.clone());
        ui.on_add_folder(move || {
            let dirs = rfd::FileDialog::new().set_title("Add folders to compilation").pick_folders().unwrap_or_default();
            add_paths(w.clone(), s.clone(), dirs);
        });
    }
    {
        let (w, s) = (ui.as_weak(), state.clone());
        ui.on_remove_selected(move || {
            let Some(ui) = w.upgrade() else { return };
            let i = ui.get_data_selected();
            if i >= 0 {
                let mut st = s.lock().unwrap();
                if (i as usize) < st.data.len() {
                    st.data.remove(i as usize);
                }
            }
            ui.set_data_selected(-1);
            update_data_view(&ui, &s);
        });
    }
    {
        let (w, s) = (ui.as_weak(), state.clone());
        ui.on_clear_data(move || {
            let Some(ui) = w.upgrade() else { return };
            s.lock().unwrap().data.clear();
            ui.set_data_selected(-1);
            update_data_view(&ui, &s);
        });
    }
    {
        let (w, s) = (ui.as_weak(), state.clone());
        ui.on_media_changed(move |_| {
            if let Some(ui) = w.upgrade() {
                update_data_view(&ui, &s);
            }
        });
    }

    {
        let (w, s) = (ui.as_weak(), state.clone());
        ui.on_open_burn_dialog(move || {
            let Some(ui) = w.upgrade() else { return };
            let (rows, speeds, default) = {
                let mut st = s.lock().unwrap();
                let total: u64 = st.data.iter().map(|e| e.bytes).sum();
                let items = st.data.len();
                let dev = st.drives.get(st.selected).map(|d| d.label.clone()).unwrap_or_default();
                let media = st.disc_profile.map(|p| p.name()).unwrap_or("disc");
                let (choices, default) = speed_choices(st.disc_profile, &st.write_speeds);
                st.speeds = choices.iter().map(|(_, k)| *k).collect();
                let free = st.disc_free.map(human_bytes).unwrap_or_default();
                let rows = vec![
                    ("Volume".to_string(), volume_label(&ui)),
                    ("Contents".to_string(), format!("{items} item(s), {}", human_bytes(total))),
                    ("File system".to_string(), "UDF 2.50".to_string()),
                    ("Target".to_string(), format!("{media} in drive {dev}  ({free} free)")),
                ];
                let names: Vec<SharedString> = choices.into_iter().map(|(n, _)| n.into()).collect();
                (rows, names, default)
            };
            ui.set_burn_rows(info_rows(&rows));
            ui.set_burn_speeds(ModelRc::new(VecModel::from(speeds)));
            ui.set_burn_speed_index(default as i32);
            ui.set_burn_dialog(true);
        });
    }
    {
        let (w, s) = (ui.as_weak(), state.clone());
        ui.on_start_burn(move || {
            let Some(ui) = w.upgrade() else { return };
            ui.set_burn_dialog(false);
            let Some(dev) = current_drive(&s) else { return };
            let (sources, write_kbs) = {
                let st = s.lock().unwrap();
                let idx = ui.get_burn_speed_index().max(0) as usize;
                (
                    st.data.iter().map(|e| e.path.clone()).collect::<Vec<_>>(),
                    st.speeds.get(idx).copied().flatten(),
                )
            };
            let settings = BurnSettings {
                write_kbs,
                finalize: ui.get_burn_finalize(),
                verify: ui.get_burn_verify(),
            };
            let label = volume_label(&ui);
            let cancel = jobs::begin(&ui, &s, &format!("Burning \"{label}\""), true);
            jobs::run_burn(ui.as_weak(), s.clone(), cancel, dev, sources, label, settings);
        });
    }
    {
        let (w, s) = (ui.as_weak(), state.clone());
        ui.on_save_image(move || {
            let Some(ui) = w.upgrade() else { return };
            let label = volume_label(&ui);
            let Some(out) = rfd::FileDialog::new()
                .set_title("Save disc image")
                .set_file_name(format!("{label}.iso"))
                .add_filter("Disc image", &["iso"])
                .save_file()
            else {
                return;
            };
            let sources = s.lock().unwrap().data.iter().map(|e| e.path.clone()).collect();
            let cancel = jobs::begin(&ui, &s, &format!("Creating image \"{label}\""), false);
            jobs::run_save_image(ui.as_weak(), s.clone(), cancel, sources, label, out);
        });
    }
    {
        let (w, s) = (ui.as_weak(), state.clone());
        ui.on_cancel_job(move || {
            s.lock().unwrap().job_cancel.store(true, Ordering::Relaxed);
            if let Some(ui) = w.upgrade() {
                ui.set_job_phase("Cancelling…".into());
            }
        });
    }
    {
        let (w, s) = (ui.as_weak(), state.clone());
        ui.on_copy_log(move || {
            let text = s.lock().unwrap().job_log.join("\n");
            let feedback = match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
                Ok(()) => "Log copied to clipboard",
                Err(_) => "Could not access the clipboard",
            };
            if let Some(ui) = w.upgrade() {
                ui.set_job_copy_feedback(feedback.into());
            }
        });
    }
    {
        let w = ui.as_weak();
        ui.on_close_job(move || {
            if let Some(ui) = w.upgrade() {
                ui.set_job_visible(false);
            }
        });
    }
    {
        let (w, s) = (ui.as_weak(), state.clone());
        ui.on_eject_after_job(move || {
            if let Some(ui) = w.upgrade() {
                ui.set_job_visible(false);
            }
            eject(w.clone(), s.clone());
        });
    }

    // Poll for disc insertion/removal so the UI stays current without a manual refresh.
    let poll = slint::Timer::default();
    {
        let (w, s) = (ui.as_weak(), state.clone());
        poll.start(slint::TimerMode::Repeated, Duration::from_secs(3), move || {
            let (dev, known, busy) = {
                let st = s.lock().unwrap();
                (st.drives.get(st.selected).cloned(), st.has_disc, st.busy)
            };
            let Some(dev) = dev.filter(|_| !busy) else { return };
            let (w, s) = (w.clone(), s.clone());
            std::thread::spawn(move || {
                let now = report::has_disc(&dev.path);
                if now.is_some() && now != known {
                    let _ = w.upgrade_in_event_loop(move |ui| {
                        if !s.lock().unwrap().busy {
                            load_info(ui.as_weak(), s);
                        }
                    });
                }
            });
        });
    }

    update_data_view(&ui, &state);
    refresh_drives(ui.as_weak(), state.clone());

    // Files and folders passed on the command line ("Open with", dropping onto the exe)
    // start the compilation.
    let startup: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).filter(|p| p.exists()).collect();
    add_paths(ui.as_weak(), state.clone(), startup);
    ui.run()
}
