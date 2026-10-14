use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::job::{self, Event, Options, Reporter, Summary};

const MAX_LOG_LINES: usize = 5000;

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    user: String,
    out_root: String,
    comments: bool,
    videos: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let o = Options::default();
        Self {
            user: String::new(),
            out_root: o.out_root.display().to_string(),
            comments: o.comments,
            videos: o.videos,
        }
    }
}

fn settings_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("aipicdl").join("settings.json"))
}

impl Settings {
    fn load() -> Self {
        settings_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
    fn save(&self) {
        if let Some(p) = settings_path() {
            if let Some(d) = p.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            if let Ok(b) = serde_json::to_vec_pretty(self) {
                let _ = std::fs::write(p, b);
            }
        }
    }
}

struct Running {
    rx: Receiver<Event>,
    cancel: Arc<AtomicBool>,
}

struct App {
    s: Settings,
    refresh: bool,
    running: Option<Running>,
    logs: Vec<String>,
    phase: String,
    done: usize,
    total: usize,
    /// 残り時間の見積もり用 (フェーズ開始時刻と、その時点の完了数)。
    phase_start: (Instant, usize),
    last: Option<Result<Summary, String>>,
    archive_dir: Option<PathBuf>,
}

pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("aipicdl — Aipictors アーカイブ")
            .with_inner_size([780.0, 640.0])
            .with_min_inner_size([520.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "aipicdl",
        options,
        Box::new(|cc| {
            install_fonts(&cc.egui_ctx);
            Ok(Box::new(App::new()))
        }),
    )
}

/// egui の既定フォントには日本語が無いので、Windows のシステムフォントを後ろに足す。
fn install_fonts(ctx: &egui::Context) {
    let windir = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let candidates = ["meiryo.ttc", "YuGothM.ttc", "msgothic.ttc"];
    let Some(bytes) = candidates
        .iter()
        .find_map(|f| std::fs::read(windir.join("Fonts").join(f)).ok())
    else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("jp".into(), Arc::new(egui::FontData::from_owned(bytes)));
    // 本文は和文フォントを先頭にして、英数字と仮名漢字のベースラインを揃える。
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "jp".into());
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .push("jp".into());
    ctx.set_fonts(fonts);
}

impl App {
    fn new() -> Self {
        Self {
            s: Settings::load(),
            refresh: false,
            running: None,
            logs: Vec::new(),
            phase: String::new(),
            done: 0,
            total: 0,
            phase_start: (Instant::now(), 0),
            last: None,
            archive_dir: None,
        }
    }

    fn log(&mut self, s: String) {
        self.logs.push(s);
        if self.logs.len() > MAX_LOG_LINES {
            let n = self.logs.len() - MAX_LOG_LINES;
            self.logs.drain(..n);
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
        self.s.save();
        let opts = Options {
            user: self.s.user.clone(),
            out_root: PathBuf::from(self.s.out_root.trim()),
            comments: self.s.comments,
            videos: self.s.videos,
            refresh: self.refresh,
            limit: None,
            only_ids: Vec::new(),
        };
        self.archive_dir = crate::api::parse_user_input(&opts.user)
            .map(|login| opts.out_root.join(crate::util::safe_file_name(&login)));
        let (tx, rx) = channel();
        let repaint = ctx.clone();
        let reporter = Arc::new(Reporter::new(move |ev| {
            let _ = tx.send(ev);
            repaint.request_repaint();
        }));
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = cancel.clone();
        self.logs.clear();
        self.last = None;
        self.phase = "開始中".into();
        self.done = 0;
        self.total = 0;
        self.phase_start = (Instant::now(), 0);
        std::thread::spawn(move || {
            let r = job::run(&opts, c2, reporter.clone());
            let r = match r {
                Ok(s) => Ok(s),
                Err(e) if crate::net::is_cancelled(&e) => Err("中断しました".to_string()),
                Err(e) => Err(format!("{e:#}")),
            };
            reporter.finish(r);
        });
        self.running = Some(Running { rx, cancel });
    }

    fn poll(&mut self) {
        let Some(run) = &self.running else { return };
        let mut events = Vec::new();
        while let Ok(ev) = run.rx.try_recv() {
            events.push(ev);
        }
        for ev in events {
            match ev {
                Event::Log(s) => self.log(s),
                Event::Progress { phase, done, total } => {
                    if phase != self.phase {
                        self.phase_start = (Instant::now(), done);
                    }
                    self.phase = phase;
                    self.done = done;
                    self.total = total;
                }
                Event::Finished(r) => {
                    match &r {
                        Ok(s) => {
                            self.archive_dir = s.index.parent().map(Path::to_path_buf);
                            self.phase = if s.cancelled {
                                "中断しました"
                            } else {
                                "完了"
                            }
                            .into();
                        }
                        Err(e) => {
                            self.log(format!("エラー: {e}"));
                            self.phase = "停止".into();
                        }
                    }
                    self.last = Some(r);
                    self.running = None;
                    return;
                }
            }
        }
    }

    fn eta(&self) -> Option<Duration> {
        if self.phase != "作品を保存中" {
            return None;
        }
        let (t0, d0) = self.phase_start;
        let done = self.done.checked_sub(d0)?;
        if done < 3 {
            return None;
        }
        let per = t0.elapsed().as_secs_f64() / done as f64;
        Some(Duration::from_secs_f64(
            per * self.total.saturating_sub(self.done) as f64,
        ))
    }
}

fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{} 時間 {} 分", s / 3600, s % 3600 / 60)
    } else if s >= 60 {
        format!("{} 分", s / 60 + 1)
    } else {
        "1 分未満".into()
    }
}

fn open_path(p: &Path) {
    let _ = std::process::Command::new("explorer").arg(p).spawn();
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        let running = self.running.is_some();

        egui::TopBottomPanel::bottom("footer").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "サイトに負荷をかけないよう、間隔を空けて 1 件ずつ取得します。\
                     途中で停止しても、次回は保存済みの作品を飛ばして続きから再開します。\n\
                     同時に複数起動しないでください。保存した他の方の作品は個人的な閲覧の範囲でお使いください。",
                )
                .small()
                .weak(),
            );
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Aipictors アーカイブ");
            ui.add_space(6.0);
            ui.label("プロフィール URL");
            ui.add_enabled(
                !running,
                egui::TextEdit::singleline(&mut self.s.user)
                    .hint_text("https://www.aipictors.com/users/○○")
                    .desired_width(ui.available_width()),
            );
            ui.add_space(4.0);
            ui.label("保存先フォルダ (この中にユーザー ID のフォルダを作ります)");
            ui.horizontal(|ui| {
                let w = (ui.available_width() - 72.0).max(100.0);
                ui.add_enabled(
                    !running,
                    egui::TextEdit::singleline(&mut self.s.out_root).desired_width(w),
                );
                if ui
                    .add_enabled(!running, egui::Button::new("参照…"))
                    .clicked()
                    && let Some(dir) = rfd::FileDialog::new()
                        .set_directory(&self.s.out_root)
                        .pick_folder()
                {
                    self.s.out_root = dir.display().to_string();
                }
            });

            ui.add_space(4.0);
            egui::CollapsingHeader::new("詳細設定")
                .default_open(false)
                .show(ui, |ui| {
                    ui.add_enabled_ui(!running, |ui| {
                        ui.checkbox(
                            &mut self.s.comments,
                            "コメント (スタンプ・アイコン含む) も保存する",
                        );
                        ui.checkbox(&mut self.s.videos, "動画も保存する");
                        ui.checkbox(
                            &mut self.refresh,
                            "保存済みの作品も情報 (いいね数・コメントなど) を取り直す",
                        );
                    });
                });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if running {
                    let cancelling = self
                        .running
                        .as_ref()
                        .is_some_and(|r| r.cancel.load(Ordering::Relaxed));
                    if ui
                        .add_enabled(
                            !cancelling,
                            egui::Button::new(if cancelling { "停止中…" } else { "停止" }),
                        )
                        .clicked()
                        && let Some(r) = &self.running
                    {
                        r.cancel.store(true, Ordering::Relaxed);
                    }
                } else {
                    let ok = crate::api::parse_user_input(&self.s.user).is_some()
                        && !self.s.out_root.trim().is_empty();
                    if ui
                        .add_enabled(ok, egui::Button::new("ダウンロード開始"))
                        .clicked()
                    {
                        self.start(ctx);
                    }
                }
                if let Some(dir) = self.archive_dir.clone().filter(|d| d.exists()) {
                    if ui.button("フォルダを開く").clicked() {
                        open_path(&dir);
                    }
                    let index = dir.join("index.html");
                    if index.exists() && ui.button("index.html を開く").clicked() {
                        open_path(&index);
                    }
                }
            });

            ui.add_space(6.0);
            if running || self.last.is_some() {
                let frac = if self.total > 0 {
                    self.done as f32 / self.total as f32
                } else {
                    0.0
                };
                let mut text = self.phase.clone();
                if self.total > 0 {
                    text.push_str(&format!("  {} / {}", self.done, self.total));
                }
                if let Some(eta) = self.eta() {
                    text.push_str(&format!("  — 残り約 {}", fmt_duration(eta)));
                }
                let mut bar = egui::ProgressBar::new(frac).text(text);
                if running && self.total == 0 {
                    bar = bar.animate(true);
                }
                ui.add(bar);
            }
            if let Some(Ok(s)) = &self.last {
                ui.label(format!(
                    "新規/更新 {} 件 · スキップ {} 件 · 失敗 {} 件{}",
                    s.saved,
                    s.skipped,
                    s.failed,
                    if s.failed > 0 {
                        " (もう一度実行すると失敗分を再取得します)"
                    } else {
                        ""
                    }
                ));
            }

            ui.add_space(6.0);
            ui.separator();
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for l in &self.logs {
                        ui.label(egui::RichText::new(l).monospace().size(12.0));
                    }
                });
        });

        if running {
            // 残り時間表示を進めるため、イベントが無くても時々描き直す。
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(r) = &self.running {
            r.cancel.store(true, Ordering::Relaxed);
        }
        self.s.save();
    }
}
