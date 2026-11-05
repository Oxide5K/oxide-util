#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{self, Color32, RichText};
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

#[derive(Default)]
struct App {
    pkgs: Vec<Pkg>,
    busy: Option<String>,
    status: String,
    rx: Option<Receiver<Msg>>,
}

const ACCENT: Color32 = Color32::from_rgb(255, 120, 40);
const CARD: Color32 = Color32::from_rgb(34, 34, 40);

fn winget(args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("winget");
    cmd.args(args)
        .args(["--accept-source-agreements", "--disable-interactivity"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output().map_err(|e| format!("Could not run winget: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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
        let idle = self.busy.is_none();

        egui::TopBottomPanel::top("top")
            .frame(egui::Frame::none().inner_margin(16.0).fill(Color32::from_rgb(24, 24, 28)))
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
                    });
                });
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
            .with_title("Oxide Util"),
        ..Default::default()
    };
    eframe::run_native(
        "Oxide Util",
        opts,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            let mut app = App::default();
            app.scan(&cc.egui_ctx);
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
}
