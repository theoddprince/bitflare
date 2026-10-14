//! Long-running jobs (burning, image creation) shown in the progress window.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use bc_burn::{BurnError, BurnSettings, Event};
use bc_fs::udf::{self, UdfOptions};
use bc_scsi::DevicePath;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

use crate::report::human_bytes;
use crate::{AppWindow, Shared, load_info, set_busy};

const LOG_LINES: usize = 14;

#[derive(Clone, Default)]
struct View {
    phase: String,
    fraction: f32,
    detail: String,
    log: Vec<String>,
}

struct JobUi {
    weak: Weak<AppWindow>,
    state: Shared,
    view: View,
    started: Instant,
    rate_from: (Instant, u64),
}

fn duration_text(secs: u64) -> String {
    if secs >= 3600 {
        format!("{} h {:02} min", secs / 3600, secs % 3600 / 60)
    } else if secs >= 60 {
        format!("{} min {:02} s", secs / 60, secs % 60)
    } else {
        format!("{secs} s")
    }
}

impl JobUi {
    fn new(weak: Weak<AppWindow>, state: Shared) -> Self {
        JobUi {
            weak,
            state,
            view: View::default(),
            started: Instant::now(),
            rate_from: (Instant::now(), 0),
        }
    }

    fn push(&self) {
        let v = self.view.clone();
        let _ = self.weak.upgrade_in_event_loop(move |ui| {
            ui.set_job_phase(v.phase.into());
            ui.set_job_fraction(v.fraction);
            ui.set_job_detail(v.detail.into());
            let start = v.log.len().saturating_sub(LOG_LINES);
            let lines: Vec<SharedString> = v.log[start..].iter().map(|l| l.into()).collect();
            ui.set_job_log(ModelRc::new(VecModel::from(lines)));
        });
    }

    fn log(&mut self, line: impl Into<String>) {
        let t = self.started.elapsed().as_secs();
        let line = format!("[{:02}:{:02}] {}", t / 60, t % 60, line.into());
        self.state.lock().unwrap().job_log.push(line.clone());
        self.view.log.push(line);
        self.push();
    }

    fn phase(&mut self, name: &str, done: u64) {
        self.view.phase = name.to_string();
        self.view.detail.clear();
        self.rate_from = (Instant::now(), done);
        self.log(name);
    }

    fn progress(&mut self, done: u64, total: u64) {
        self.view.fraction = if total == 0 { 0.0 } else { done as f32 / total as f32 };
        let secs = self.rate_from.0.elapsed().as_secs_f64();
        let moved = done.saturating_sub(self.rate_from.1);
        let mut detail = format!("{} of {}", human_bytes(done), human_bytes(total));
        if secs > 2.0 && moved > 0 {
            let rate = moved as f64 / secs;
            let left = ((total - done) as f64 / rate) as u64;
            detail.push_str(&format!(
                "   ·   {:.1} MB/s   ·   {} left",
                rate / (1024.0 * 1024.0),
                duration_text(left)
            ));
        }
        self.view.detail = detail;
        self.push();
    }

    fn finish(self, ok: bool, message: String, can_eject: bool) {
        let state = self.state.clone();
        let elapsed = duration_text(self.started.elapsed().as_secs());
        let _ = self.weak.upgrade_in_event_loop(move |ui| {
            ui.set_job_running(false);
            ui.set_job_ok(ok);
            ui.set_job_can_eject(can_eject);
            if ok {
                ui.set_job_fraction(1.0);
                ui.set_job_phase("Done".into());
            } else {
                ui.set_job_phase("Stopped".into());
            }
            ui.set_job_result(format!("{message}  (total time {elapsed})").into());
            set_busy(&ui, &state, false, if ok { "Ready" } else { "Last job failed" });
            load_info(ui.as_weak(), state);
        });
    }
}

/// Opens the progress window and returns the cancel flag for the new job.
pub fn begin(ui: &AppWindow, state: &Shared, title: &str, is_burn: bool) -> Arc<AtomicBool> {
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut s = state.lock().unwrap();
        s.job_cancel = cancel.clone();
        s.job_log.clear();
    }
    ui.set_job_is_burn(is_burn);
    ui.set_job_copy_feedback("".into());
    set_busy(ui, state, true, title);
    ui.set_job_title(title.into());
    ui.set_job_phase("Starting".into());
    ui.set_job_fraction(0.0);
    ui.set_job_detail("".into());
    ui.set_job_log(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
    ui.set_job_result("".into());
    ui.set_job_running(true);
    ui.set_job_ok(false);
    ui.set_job_visible(true);
    cancel
}

fn build_image(job: &mut JobUi, sources: &[PathBuf], label: &str) -> Result<bc_fs::Image, String> {
    job.phase("Building file system", 0);
    let img = udf::build(sources, &UdfOptions::new(label)).map_err(|e| e.to_string())?;
    job.log(format!(
        "UDF 2.50 volume \"{}\": {} files, {} folders, {}",
        label,
        img.files,
        img.directories.saturating_sub(1),
        human_bytes(img.bytes())
    ));
    Ok(img)
}

pub fn run_burn(
    weak: Weak<AppWindow>,
    state: Shared,
    cancel: Arc<AtomicBool>,
    dev: DevicePath,
    sources: Vec<PathBuf>,
    label: String,
    settings: BurnSettings,
) {
    std::thread::spawn(move || {
        let mut job = JobUi::new(weak, state);
        let img = match build_image(&mut job, &sources, &label) {
            Ok(i) => i,
            Err(e) => return job.finish(false, format!("Could not prepare the disc: {e}"), false),
        };
        let mut drive = match bc_drive::Drive::open(&dev.path) {
            Ok(d) => d,
            Err(e) => return job.finish(false, format!("Cannot open drive {}: {e}", dev.label), false),
        };
        let mut writing_started = false;
        let result = bc_burn::burn(&mut drive, &img, &settings, &cancel, &mut |e| match e {
            Event::Phase(p) => {
                writing_started |= p == bc_burn::Phase::Writing;
                job.phase(p.label(), 0);
            }
            Event::Progress { done, total } => job.progress(done * bc_fs::SECTOR, total * bc_fs::SECTOR),
            Event::Log(l) => job.log(l),
        });
        drop(drive);
        match result {
            Ok(()) => {
                let msg = if settings.verify {
                    "Burn completed and verified successfully."
                } else {
                    "Burn completed successfully."
                };
                job.log(msg);
                job.finish(true, msg.into(), true);
            }
            Err(BurnError::Cancelled) => {
                let msg = if writing_started {
                    "Burning was cancelled. The disc is incomplete."
                } else {
                    "Burning was cancelled before anything was written."
                };
                job.finish(false, msg.into(), false);
            }
            Err(e) => {
                job.log(format!("Error: {e}"));
                job.finish(false, e.to_string(), false);
            }
        }
    });
}

pub fn run_save_image(
    weak: Weak<AppWindow>,
    state: Shared,
    cancel: Arc<AtomicBool>,
    sources: Vec<PathBuf>,
    label: String,
    out: PathBuf,
) {
    std::thread::spawn(move || {
        let mut job = JobUi::new(weak, state);
        let img = match build_image(&mut job, &sources, &label) {
            Ok(i) => i,
            Err(e) => return job.finish(false, format!("Could not prepare the image: {e}"), false),
        };
        job.phase("Writing image file", 0);
        job.log(out.display().to_string());
        let result = (|| -> std::io::Result<bool> {
            let mut file = std::io::BufWriter::with_capacity(4 << 20, std::fs::File::create(&out)?);
            let mut reader = img.reader();
            let mut buf = vec![0u8; 4 << 20];
            let total = img.bytes();
            let mut done = 0u64;
            let mut last = Instant::now();
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Ok(false);
                }
                let n = reader.fill(&mut buf)?;
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n])?;
                done += n as u64;
                if last.elapsed().as_millis() >= 250 {
                    job.progress(done, total);
                    last = Instant::now();
                }
            }
            file.flush()?;
            job.progress(total, total);
            Ok(true)
        })();
        match result {
            Ok(true) => job.finish(true, format!("Image saved to {}", out.display()), false),
            Ok(false) => {
                let _ = std::fs::remove_file(&out);
                job.finish(false, "Image creation was cancelled.".into(), false);
            }
            Err(e) => {
                let _ = std::fs::remove_file(&out);
                job.finish(false, format!("Could not write the image: {e}"), false);
            }
        }
    });
}
