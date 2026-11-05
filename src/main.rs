#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{self, Color32, RichText};
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::mpsc::{channel, Receiver};

#[derive(Clone, Debug, PartialEq)]
struct Pkg {
    name: String,
    id: String,
    version: String,
    available: String,
}

enum Msg {
    Scanned(Result<Vec<Pkg>, String>),
    Updated(String),
}

enum Upd {
    Checked(Result<Option<String>, String>),
    InstallerStarted(Result<(), String>),
}

#[derive(Default)]
struct App {
    pkgs: Vec<Pkg>,
    busy: Option<String>,
    status: String,
    rx: Option<Receiver<Msg>>,
    upd_rx: Option<Receiver<Upd>>,
    upd_status: String,
    latest: Option<String>,
    auto_update: bool,
    show_settings: bool,
}

const ACCENT: Color32 = Color32::from_rgb(120, 200, 255);
const CARD: Color32 = Color32::from_rgb(28, 38, 52);
const RELEASES: &str = "https://github.com/Oxide5K/oxide-util/releases/latest";

fn hidden(exe: &str) -> Command {
    let mut cmd = Command::new(exe);
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd
}

fn winget(args: &[&str]) -> Result<String, String> {
    let out = hidden("winget")
        .args(args)
        .args(["--accept-source-agreements", "--disable-interactivity"])
        .output()
        .map_err(|e| format!("Could not run winget: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Auto-update is on unless this marker file exists.
fn no_auto_update_marker() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join("oxide-util").join("no-auto-update")
}

fn ver(s: &str) -> Vec<u32> {
    s.trim().trim_start_matches('v').split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// Latest release tag, if newer than this build.
fn check_latest() -> Result<Option<String>, String> {
    // /releases/latest redirects to /releases/tag/<tag>; reading the redirect skips the rate-limited JSON API
    let out = hidden("curl.exe")
        .args(["-sI", "-o", "NUL", "-w", "%{redirect_url}", RELEASES])
        .output()
        .map_err(|e| format!("Could not run curl: {e}"))?;
    let url = String::from_utf8_lossy(&out.stdout);
    let tag = url.rsplit_once("/tag/").map(|(_, t)| t.trim().to_string()).ok_or("Could not reach GitHub")?;
    Ok((ver(&tag) > ver(env!("CARGO_PKG_VERSION"))).then_some(tag))
}

/// Downloads the latest MSI and starts a detached installer that relaunches the app; the caller must exit.
fn start_installer() -> Result<(), String> {
    let msi = std::env::temp_dir().join("oxide-util-update.msi");
    let ok = hidden("curl.exe")
        .args(["-sfL", "-o"])
        .arg(&msi)
        .arg(format!("{RELEASES}/download/oxide-util.msi"))
        .status()
        .map_err(|e| format!("Could not run curl: {e}"))?
        .success();
    if !ok {
        return Err("Downloading the update failed.".into());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // cmd outlives us: waits for msiexec to replace the files, then reopens the app
    hidden("cmd")
        .arg("/c")
        .raw_arg(format!("start /wait msiexec /i \"{}\" /passive & start \"\" \"{}\"", msi.display(), exe.display()))
        .spawn()
        .map_err(|e| format!("Could not start installer: {e}"))?;
    Ok(())
}

/// Parses the fixed-width table that `winget upgrade` prints.
// shortcut: column offsets are char counts, so names with double-width chars may misalign; switch to the WinGet COM API if that bites.
fn parse(out: &str) -> Vec<Pkg> {
    // winget redraws its spinner with \r; keep only what follows the last one
    let lines: Vec<&str> = out
        .lines()
        .map(|l| l.rsplit('\r').next().unwrap_or(l))
        .collect();
    let Some(h) = lines
        .iter()
        .position(|l| l.contains(" Id ") && l.contains("Version"))
    else {
        return vec![];
    };
    let header = lines[h];
    let col = |name: &str| header.find(name).map(|b| header[..b].chars().count());
    let (Some(id), Some(ver), Some(avail)) = (col("Id"), col("Version"), col("Available")) else {
        return vec![];
    };
    let src = col("Source");
    lines[h + 2..]
        .iter()
        .take_while(|l| l.chars().count() > avail)
        .map(|l| {
            let c: Vec<char> = l.chars().collect();
            let cut = |a: usize, b: Option<usize>| {
                let b = b.unwrap_or(c.len()).min(c.len());
                c[a.min(b)..b].iter().collect::<String>().trim().to_string()
            };
            Pkg {
                name: cut(0, Some(id)),
                id: cut(id, Some(ver)),
                version: cut(ver, Some(avail)),
                available: cut(avail, src),
            }
        })
        .filter(|p| !p.id.is_empty())
        .collect()
}

impl App {
    fn run(&mut self, ctx: &egui::Context, label: String, job: impl FnOnce() -> Msg + Send + 'static) {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(job());
            ctx.request_repaint();
        });
        self.busy = Some(label);
        self.rx = Some(rx);
    }

    fn scan(&mut self, ctx: &egui::Context) {
        self.run(ctx, "Scanning for updates...".into(), || {
            Msg::Scanned(winget(&["upgrade", "--include-unknown"]).map(|o| parse(&o)))
        });
    }

    fn updater(&mut self, ctx: &egui::Context, label: &str, job: impl FnOnce() -> Upd + Send + 'static) {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(job());
            ctx.request_repaint();
        });
        self.upd_status = label.into();
        self.upd_rx = Some(rx);
    }

    fn update(&mut self, ctx: &egui::Context, id: Option<String>) {
        let target = id.clone().unwrap_or_else(|| "everything".into());
        self.run(ctx, format!("Updating {target}..."), move || {
            let mut args = vec!["upgrade", "--silent", "--accept-package-agreements"];
            match &id {
                Some(id) => args.extend(["--id", id.as_str(), "--exact"]),
                None => args.push("--all"),
            }
            Msg::Updated(match winget(&args) {
                Ok(_) => format!("Finished updating {target}."),
                Err(e) => e,
            })
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        if let Some(msg) = self.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.busy = None;
            self.rx = None;
            match msg {
                Msg::Scanned(Ok(p)) => {
                    self.status = format!("{} update(s) available", p.len());
                    self.pkgs = p;
                }
                Msg::Scanned(Err(e)) => self.status = e,
                Msg::Updated(s) => {
                    self.status = s;
                    self.scan(ctx);
                }
            }
        }
        if let Some(msg) = self.upd_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.upd_rx = None;
            match msg {
                Upd::Checked(Ok(Some(tag))) => {
                    self.upd_status = format!("{tag} is available.");
                    self.latest = Some(tag);
                }
                Upd::Checked(Ok(None)) => self.upd_status = "You're on the latest version.".into(),
                Upd::Checked(Err(e)) | Upd::InstallerStarted(Err(e)) => self.upd_status = e,
                Upd::InstallerStarted(Ok(())) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            }
        }
        let idle = self.busy.is_none();
        let upd_idle = self.upd_rx.is_none();

        let mut open = self.show_settings;
        let mut check_now = false;
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(format!("Oxide Util v{}", env!("CARGO_PKG_VERSION")));
                if ui.checkbox(&mut self.auto_update, "Check for updates when the app opens").changed() {
                    let m = no_auto_update_marker();
                    let _ = if self.auto_update {
                        std::fs::remove_file(&m)
                    } else {
                        std::fs::create_dir_all(m.parent().unwrap()).and_then(|_| std::fs::write(&m, ""))
                    };
                }
                ui.horizontal(|ui| {
                    check_now = ui.add_enabled(upd_idle, egui::Button::new("Check now")).clicked();
                    if !upd_idle {
                        ui.spinner();
                    }
                    ui.label(RichText::new(&self.upd_status).weak());
                });
            });
        self.show_settings = open;
        if check_now {
            self.updater(ctx, "Checking for updates...", || Upd::Checked(check_latest()));
        }

        egui::TopBottomPanel::top("top")
            .frame(egui::Frame::none().inner_margin(16.0).fill(Color32::from_rgb(18, 26, 38)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Oxide Util").size(24.0).strong().color(ACCENT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let all = egui::Button::new(RichText::new("Update all").strong().color(Color32::BLACK)).fill(ACCENT);
                        if ui.add_enabled(idle && !self.pkgs.is_empty(), all).clicked() {
                            self.update(ctx, None);
                        }
                        if ui.add_enabled(idle, egui::Button::new("Scan")).clicked() {
                            self.scan(ctx);
                        }
                        if ui.button("Settings").clicked() {
                            self.show_settings = !self.show_settings;
                        }
                    });
                });
                if let Some(tag) = self.latest.clone() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("Oxide Util {tag} is available.")).color(ACCENT));
                        let install = egui::Button::new(RichText::new("Install & restart").color(Color32::BLACK)).fill(ACCENT);
                        if ui.add_enabled(upd_idle, install).clicked() {
                            self.updater(ctx, "Downloading update...", || Upd::InstallerStarted(start_installer()));
                        }
                        if !upd_idle {
                            ui.spinner();
                            ui.label(&self.upd_status);
                        }
                    });
                }
                ui.horizontal(|ui| match &self.busy {
                    Some(b) => {
                        ui.spinner();
                        ui.label(b);
                    }
                    None => {
                        ui.label(RichText::new(&self.status).weak());
                    }
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.pkgs.is_empty() {
                if idle {
                    ui.centered_and_justified(|ui| {
                        ui.label(RichText::new("Nothing to update. Hit Scan to check again.").size(16.0).weak());
                    });
                }
                return;
            }
            let mut clicked = None;
            egui::ScrollArea::vertical().show(ui, |ui| {
                for p in &self.pkgs {
                    egui::Frame::none().fill(CARD).rounding(10.0).inner_margin(12.0).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.label(RichText::new(&p.name).size(15.0).strong());
                                ui.label(RichText::new(&p.id).small().weak());
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.add_enabled(idle, egui::Button::new("Update")).clicked() {
                                    clicked = Some(p.id.clone());
                                }
                                ui.label(RichText::new(&p.available).color(ACCENT).strong());
                                ui.label("->");
                                ui.label(RichText::new(&p.version).weak());
                            });
                        });
                    });
                    ui.add_space(6.0);
                }
            });
            if clicked.is_some() {
                self.update(ctx, clicked);
            }
        });
    }
}

fn main() -> eframe::Result<()> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 560.0])
            .with_title("Oxide Util")
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")).unwrap()),
        ..Default::default()
    };
    eframe::run_native(
        "Oxide Util",
        opts,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            let mut app = App { auto_update: !no_auto_update_marker().exists(), ..Default::default() };
            app.scan(&cc.egui_ctx);
            if app.auto_update {
                app.updater(&cc.egui_ctx, "Checking for updates...", || Upd::Checked(check_latest()));
            }
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_winget_table() {
        let out = "   - \r   \\ \rName              Id                   Version  Available Source\n\
-------------------------------------------------------------------\n\
Google Chrome     Google.Chrome        130.0    131.0     winget\n\
Discord           Discord.Discord      1.0.9    1.0.10    winget\n\
2 upgrades available.\n";
        let p = parse(out);
        assert_eq!(p.len(), 2);
        assert_eq!(
            p[0],
            Pkg {
                name: "Google Chrome".into(),
                id: "Google.Chrome".into(),
                version: "130.0".into(),
                available: "131.0".into(),
            }
        );
        assert_eq!(p[1].id, "Discord.Discord");
        assert!(parse("No installed package found matching input criteria.").is_empty());
    }

    #[test]
    fn compares_versions() {
        assert!(ver("v0.10.0") > ver("0.9.9"));
        assert!(ver("v1.0.0") > ver("0.3.0"));
        assert!(ver("v0.3.0") <= ver("0.3.0"));
    }
}
