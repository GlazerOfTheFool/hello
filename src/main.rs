#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

// ---------- helpers ----------

fn cmd<S: AsRef<std::ffi::OsStr>>(program: S) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(program);
    #[cfg(windows)]
    c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    c
}

fn data_dir() -> PathBuf {
    let d = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("MediaDownloader");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Looks next to the exe, in ./bin, in the app data folder, then on PATH.
fn find_tool(name: &str) -> Option<PathBuf> {
    let exe = format!("{name}.exe");
    let mut cands: Vec<PathBuf> = vec![];
    if let Ok(p) = std::env::current_exe() {
        if let Some(d) = p.parent() {
            cands.push(d.join(&exe));
            cands.push(d.join("bin").join(&exe));
        }
    }
    cands.push(data_dir().join("bin").join(&exe));
    if let Some(paths) = std::env::var_os("PATH") {
        for d in std::env::split_paths(&paths) {
            cands.push(d.join(&exe));
        }
    }
    cands.into_iter().find(|p| p.is_file())
}

fn fetch_ytdlp() -> Result<PathBuf, String> {
    let dir = data_dir().join("bin");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let dest = dir.join("yt-dlp.exe");
    let resp = ureq::get("https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe")
        .call()
        .map_err(|e| format!("Could not download yt-dlp: {e}"))?;
    let mut reader = resp.into_reader();
    let mut f = std::fs::File::create(&dest).map_err(|e| e.to_string())?;
    std::io::copy(&mut reader, &mut f).map_err(|e| e.to_string())?;
    Ok(dest)
}

fn copy_file_to_clipboard(path: &str) -> bool {
    let escaped = path.replace('\'', "''");
    cmd("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("Set-Clipboard -LiteralPath '{escaped}'"),
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn reveal(path: &str) {
    let _ = Command::new("explorer").arg(format!("/select,{path}")).spawn();
}

fn open(path: &str) {
    let _ = Command::new("explorer").arg(path).spawn();
}

// ---------- data ----------

#[derive(Serialize, Deserialize, Clone)]
struct Entry {
    title: String,
    path: String,
    url: String,
}

#[derive(Serialize, Deserialize, Clone)]
struct Settings {
    save_dir: PathBuf,
    auto_copy: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            save_dir: dirs::download_dir().unwrap_or_else(|| PathBuf::from(".")),
            auto_copy: true,
        }
    }
}

fn load_json<T: for<'a> Deserialize<'a> + Default>(name: &str) -> T {
    std::fs::read_to_string(data_dir().join(name))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_json<T: Serialize>(name: &str, v: &T) {
    if let Ok(s) = serde_json::to_string_pretty(v) {
        let _ = std::fs::write(data_dir().join(name), s);
    }
}

enum Msg {
    Status(String),
    Progress(f32),
    Done(Entry),
    Failed(String),
}

// ---------- workers ----------

fn run_download(url: String, save_dir: PathBuf, tx: Sender<Msg>, ctx: egui::Context) {
    let send = |m: Msg| {
        let _ = tx.send(m);
        ctx.request_repaint();
    };

    let ytdlp = match find_tool("yt-dlp") {
        Some(p) => p,
        None => {
            send(Msg::Status("Downloading yt-dlp (first run)...".into()));
            match fetch_ytdlp() {
                Ok(p) => p,
                Err(e) => return send(Msg::Failed(e)),
            }
        }
    };
    let ffmpeg = match find_tool("ffmpeg") {
        Some(p) => p,
        None => {
            return send(Msg::Failed(
                "ffmpeg.exe not found. Put it next to this app, or install it (winget install ffmpeg).".into(),
            ))
        }
    };

    send(Msg::Status("Starting download...".into()));
    let template = save_dir.join("%(title).150B [%(id)s].%(ext)s");

    let mut child = match cmd(&ytdlp)
        .args([
            "-f",
            "bv*[ext=mp4]+ba[ext=m4a]/b[ext=mp4]/bv*+ba/b",
            "--merge-output-format",
            "mp4",
            "--no-playlist",
            "--windows-filenames",
            "--newline",
            "--progress",
            "--progress-template",
            "download:PROG:%(progress._percent_str)s",
            "--print",
            "after_move:DONE:%(filepath)s",
            "--ffmpeg-location",
        ])
        .arg(&ffmpeg)
        .arg("-o")
        .arg(&template)
        .arg("--")
        .arg(&url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return send(Msg::Failed(format!("Could not start yt-dlp: {e}"))),
    };

    // collect stderr on its own thread to avoid pipe deadlocks
    let stderr = child.stderr.take();
    let err_thread = std::thread::spawn(move || {
        let mut last = String::new();
        if let Some(s) = stderr {
            for line in BufReader::new(s).lines().flatten() {
                if !line.trim().is_empty() {
                    last = line;
                }
            }
        }
        last
    });

    let mut final_path: Option<String> = None;
    if let Some(out) = child.stdout.take() {
        for line in BufReader::new(out).lines().flatten() {
            if let Some(rest) = line.strip_prefix("PROG:") {
                let num: String = rest.chars().filter(|c| c.is_ascii_digit() || *c == '.').collect();
                if let Ok(p) = num.parse::<f32>() {
                    send(Msg::Progress((p / 100.0).clamp(0.0, 1.0)));
                }
            } else if let Some(rest) = line.strip_prefix("DONE:") {
                final_path = Some(rest.trim().to_string());
            }
        }
    }

    let ok = child.wait().map(|s| s.success()).unwrap_or(false);
    let last_err = err_thread.join().unwrap_or_default();

    match (ok, final_path) {
        (true, Some(path)) => {
            let title = Path::new(&path)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "video".into());
            send(Msg::Done(Entry { title, path, url }));
        }
        _ => send(Msg::Failed(if last_err.is_empty() {
            "Download failed.".into()
        } else {
            last_err
        })),
    }
}

fn run_trim(src: Entry, start: String, end: String, tx: Sender<Msg>, ctx: egui::Context) {
    let send = |m: Msg| {
        let _ = tx.send(m);
        ctx.request_repaint();
    };
    let Some(ffmpeg) = find_tool("ffmpeg") else {
        return send(Msg::Failed("ffmpeg.exe not found.".into()));
    };
    send(Msg::Status("Trimming...".into()));

    let p = Path::new(&src.path);
    let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let out = p.with_file_name(format!("{stem} (trimmed).mp4"));

    let mut c = cmd(&ffmpeg);
    c.args(["-y", "-i"]).arg(&src.path);
    if !start.trim().is_empty() {
        c.args(["-ss", start.trim()]);
    }
    if !end.trim().is_empty() {
        c.args(["-to", end.trim()]);
    }
    c.args(["-c:v", "libx264", "-preset", "veryfast", "-c:a", "aac"])
        .arg(&out)
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    match c.status() {
        Ok(s) if s.success() => send(Msg::Done(Entry {
            title: format!("{} (trimmed)", src.title),
            path: out.to_string_lossy().to_string(),
            url: src.url,
        })),
        _ => send(Msg::Failed("Trim failed. Use times like 12.5 or 00:01:30.".into())),
    }
}

// ---------- UI ----------

enum Action {
    Copy(usize),
    Reveal(usize),
    Open(usize),
    Remove(usize),
    ToggleTrim(usize),
    DoTrim(usize),
}

struct App {
    url: String,
    settings: Settings,
    history: Vec<Entry>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    busy: bool,
    progress: f32,
    status: String,
    error: Option<String>,
    trim_open: Option<usize>,
    trim_start: String,
    trim_end: String,
}

impl App {
    fn new() -> Self {
        let (tx, rx) = channel();
        Self {
            url: String::new(),
            settings: load_json("settings.json"),
            history: load_json("history.json"),
            tx,
            rx,
            busy: false,
            progress: 0.0,
            status: String::new(),
            error: None,
            trim_open: None,
            trim_start: String::new(),
            trim_end: String::new(),
        }
    }

    fn start_download(&mut self, ctx: &egui::Context) {
        let url = self.url.trim().to_string();
        if url.is_empty() || self.busy {
            return;
        }
        self.busy = true;
        self.progress = 0.0;
        self.error = None;
        self.status = "Starting...".into();
        let (dir, tx, ctx) = (self.settings.save_dir.clone(), self.tx.clone(), ctx.clone());
        std::thread::spawn(move || run_download(url, dir, tx, ctx));
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Status(s) => self.status = s,
                Msg::Progress(p) => {
                    self.progress = p;
                    self.status = format!("Downloading... {:.0}%", p * 100.0);
                }
                Msg::Done(e) => {
                    self.busy = false;
                    self.progress = 1.0;
                    self.status = "Done".into();
                    if self.settings.auto_copy && copy_file_to_clipboard(&e.path) {
                        self.status = "Done. File copied to clipboard.".into();
                    }
                    self.url.clear();
                    self.history.insert(0, e);
                    save_json("history.json", &self.history);
                }
                Msg::Failed(s) => {
                    self.busy = false;
                    self.status.clear();
                    self.error = Some(s);
                }
            }
        }

        let mut action: Option<Action> = None;

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Media Downloader");
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                let resp = ui.add_sized(
                    [ui.available_width() - 190.0, 28.0],
                    egui::TextEdit::singleline(&mut self.url)
                        .hint_text("Paste a video URL (YouTube, X, TikTok, Instagram...)"),
                );
                if ui.button("Paste").clicked() {
                    if let Ok(t) = arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
                        self.url = t.trim().to_string();
                    }
                }
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let go = ui
                    .add_enabled(!self.busy && !self.url.trim().is_empty(), egui::Button::new("Download"))
                    .clicked();
                if go || (enter && !self.busy) {
                    self.start_download(ctx);
                }
            });

            if self.busy {
                ui.add_space(6.0);
                ui.add(egui::ProgressBar::new(self.progress).text(self.status.clone()));
            } else if !self.status.is_empty() {
                ui.add_space(6.0);
                ui.label(&self.status);
            }
            if let Some(err) = &self.error {
                ui.colored_label(egui::Color32::LIGHT_RED, err);
            }

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("Save to:");
                ui.monospace(self.settings.save_dir.display().to_string());
                if ui.button("Change").clicked() {
                    if let Some(d) = rfd::FileDialog::new().set_directory(&self.settings.save_dir).pick_folder() {
                        self.settings.save_dir = d;
                        save_json("settings.json", &self.settings);
                    }
                }
            });
            if ui
                .checkbox(&mut self.settings.auto_copy, "Copy finished file to clipboard")
                .changed()
            {
                save_json("settings.json", &self.settings);
            }

            ui.separator();
            ui.strong("History");
            egui::ScrollArea::vertical().show(ui, |ui| {
                if self.history.is_empty() {
                    ui.weak("Nothing downloaded yet.");
                }
                for i in 0..self.history.len() {
                    ui.group(|ui| {
                        ui.set_width(ui.available_width());
                        ui.label(&self.history[i].title);
                        ui.horizontal(|ui| {
                            if ui.small_button("Copy").clicked() { action = Some(Action::Copy(i)); }
                            if ui.small_button("Reveal").clicked() { action = Some(Action::Reveal(i)); }
                            if ui.small_button("Open").clicked() { action = Some(Action::Open(i)); }
                            if ui.small_button("Trim").clicked() { action = Some(Action::ToggleTrim(i)); }
                            if ui.small_button("Remove").clicked() { action = Some(Action::Remove(i)); }
                        });
                        if self.trim_open == Some(i) {
                            ui.horizontal(|ui| {
                                ui.label("Start");
                                ui.add(egui::TextEdit::singleline(&mut self.trim_start).desired_width(80.0).hint_text("0:00"));
                                ui.label("End");
                                ui.add(egui::TextEdit::singleline(&mut self.trim_end).desired_width(80.0).hint_text("0:30"));
                                if ui.add_enabled(!self.busy, egui::Button::new("Export")).clicked() {
                                    action = Some(Action::DoTrim(i));
                                }
                            });
                        }
                    });
                }
            });
        });

        if let Some(a) = action {
            match a {
                Action::Copy(i) => {
                    let ok = copy_file_to_clipboard(&self.history[i].path);
                    self.status = if ok { "Copied to clipboard.".into() } else { "Copy failed.".into() };
                }
                Action::Reveal(i) => reveal(&self.history[i].path),
                Action::Open(i) => open(&self.history[i].path),
                Action::Remove(i) => {
                    self.history.remove(i);
                    self.trim_open = None;
                    save_json("history.json", &self.history);
                }
                Action::ToggleTrim(i) => {
                    self.trim_open = if self.trim_open == Some(i) { None } else { Some(i) };
                    self.trim_start.clear();
                    self.trim_end.clear();
                }
                Action::DoTrim(i) => {
                    self.busy = true;
                    self.error = None;
                    self.progress = 0.0;
                    let (e, s, en, tx, c) = (
                        self.history[i].clone(),
                        self.trim_start.clone(),
                        self.trim_end.clone(),
                        self.tx.clone(),
                        ctx.clone(),
                    );
                    self.trim_open = None;
                    std::thread::spawn(move || run_trim(e, s, en, tx, c));
                }
            }
        }
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([620.0, 560.0])
            .with_min_inner_size([480.0, 360.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Media Downloader",
        options,
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
}
