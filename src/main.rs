#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{self, Color32, RichText};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{channel, Receiver};

mod drivers;
mod packs;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Kind {
    #[default]
    Software,
    Graphics,
    Network,
    Audio,
    Chipset,
    Storage,
    Peripherals,
    Firmware,
    OtherDriver,
}

impl Kind {
    const ALL: [Kind; 9] = [
        Kind::Software,
        Kind::Graphics,
        Kind::Network,
        Kind::Audio,
        Kind::Chipset,
        Kind::Storage,
        Kind::Peripherals,
        Kind::Firmware,
        Kind::OtherDriver,
    ];
    fn label(self) -> &'static str {
        match self {
            Kind::Software => "Software",
            Kind::Graphics => "Graphics",
            Kind::Network => "Network & Bluetooth",
            Kind::Audio => "Audio",
            Kind::Chipset => "Chipset & System",
            Kind::Storage => "Storage",
            Kind::Peripherals => "Peripherals",
            Kind::Firmware => "Firmware & BIOS",
            Kind::OtherDriver => "Other drivers",
        }
    }
}

/// Sorts a Windows Update driver into a category by its driver class, falling back to its title.
fn classify(class: &str, title: &str) -> Kind {
    let by_class = match class.to_lowercase().as_str() {
        "display" => Kind::Graphics,
        "net" | "netservice" | "bluetooth" | "infrared" => Kind::Network,
        "media" | "audioendpoint" | "audioprocessingobject" => Kind::Audio,
        "system" | "usb" | "pci" | "sbp2" | "computer" | "processor" | "securitydevices" | "ports" | "battery" => Kind::Chipset,
        "hdc" | "scsiadapter" | "diskdrive" | "volume" | "volumesnapshot" | "cdrom" => Kind::Storage,
        "hidclass" | "keyboard" | "mouse" | "camera" | "image" | "printer" | "biometric" | "monitor" | "smartcardreader" | "usbdevice" => {
            Kind::Peripherals
        }
        "firmware" => Kind::Firmware,
        _ => Kind::OtherDriver,
    };
    if by_class != Kind::OtherDriver {
        return by_class;
    }
    let t = title.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| t.contains(w));
    if has(&["firmware", "bios", "uefi"]) {
        Kind::Firmware
    } else if has(&["graphics", "display", "nvidia", "radeon", "gpu"]) {
        Kind::Graphics
    } else if has(&["network", "ethernet", "wi-fi", "wifi", "wireless", "wlan", "lan ", "bluetooth"]) {
        Kind::Network
    } else if has(&["audio", "sound", "realtek high definition"]) {
        Kind::Audio
    } else if has(&["chipset", "management engine", "serial io", "smbus", "thunderbolt", "usb", "gna", "dynamic tuning"]) {
        Kind::Chipset
    } else if has(&["nvme", "sata", "raid", "storage", "rapid storage"]) {
        Kind::Storage
    } else if has(&["mouse", "keyboard", "touchpad", "camera", "webcam", "printer", "hid", "fingerprint"]) {
        Kind::Peripherals
    } else {
        Kind::OtherDriver
    }
}

/// Where an update comes from (decides how it's installed).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Source {
    #[default]
    Winget,
    WindowsUpdate,
    Catalog,
    /// a Snappy Driver Installer driver pack
    Pack,
}

/// Health of a device's current driver.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Status {
    #[default]
    Ok,
    /// running a driver that ships with Windows
    Inbox,
    Missing,
    /// Device Manager error code
    Problem(u32),
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Pkg {
    kind: Kind,
    name: String,
    /// what to install: winget id, Windows Update id or Update Catalog id
    id: String,
    /// small text under the name: winget id or driver provider
    detail: String,
    /// devices: main hardware id, and every hardware id (lowercase) for matching offers
    hwid: String,
    hwids: Vec<String>,
    /// installed version
    version: String,
    /// the newer version on offer, as shown; empty when up to date
    available: String,
    /// drivers: the offered driver's version and date, to compare offers from different sources
    offer_ver: String,
    offer_date: String,
    /// install date (programs) or driver date (drivers)
    date: String,
    source: Source,
    status: Status,
    icon: Option<String>,
}

impl Pkg {
    fn has_update(&self) -> bool {
        !self.available.is_empty()
    }

    fn needs_attention(&self) -> bool {
        matches!(self.status, Status::Missing | Status::Problem(_))
    }
}

/// A program from Settings > Installed apps.
#[derive(Clone, Debug, Default, PartialEq)]
struct Installed {
    name: String,
    version: String,
    date: String,
    publisher: String,
    icon: Option<String>,
}

#[derive(Default)]
struct Scan {
    pkgs: Vec<Pkg>,
    bios: String,
    errors: Vec<String>,
}

enum Msg {
    Scanned(Scan),
    Updated(String),
}

enum Upd {
    Checked(Result<Option<String>, String>),
    InstallerStarted(Result<(), String>),
}

#[derive(Default)]
struct App {
    pkgs: Vec<Pkg>,
    filter: String,
    bios: String,
    tab: Kind,
    busy: Option<String>,
    status: String,
    rx: Option<Receiver<Msg>>,
    upd_rx: Option<Receiver<Upd>>,
    upd_status: String,
    latest: Option<String>,
    auto_update: bool,
    restore_point: bool,
    /// folder of SDIO .7z driver packs, if the user set one
    packs_folder: Option<PathBuf>,
    /// show devices running Windows' built-in drivers (hidden by default: there are hundreds)
    show_inbox: bool,
    show_settings: bool,
}

const ACCENT: Color32 = Color32::from_rgb(120, 200, 255);
const CARD: Color32 = Color32::from_rgb(28, 38, 52);
const RELEASES: &str = "https://github.com/Oxide5K/oxide-util/releases/latest";

/// Lists installed programs the way Settings > Installed apps does (no system components or patches).
/// Tab-separated: name, version, install date (yyyy-MM-dd or empty), publisher, icon png path (or empty).
const INSTALLED: &str = r#"
[Console]::OutputEncoding = [Text.Encoding]::UTF8
Add-Type -AssemblyName System.Drawing
$dir = Join-Path $env:LOCALAPPDATA 'oxide-util\icons'
New-Item -ItemType Directory -Force $dir | Out-Null
$keys = 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*',
        'HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*',
        'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*'
foreach ($e in Get-ItemProperty $keys -ErrorAction SilentlyContinue) {
    if (-not $e.DisplayName -or $e.SystemComponent -eq 1 -or $e.ParentKeyName) { continue }
    $src = if ($e.DisplayIcon) { ($e.DisplayIcon -replace ',\s*-?\d+$', '').Trim('"', ' ') } else { '' }
    $date = if ("$($e.InstallDate)" -match '^(\d{4})(\d{2})(\d{2})$') { "$($Matches[1])-$($Matches[2])-$($Matches[3])" } else { '' }
    # many installers skip InstallDate; the install folder's (or main exe's) creation time is the next best record
    if (-not $date) {
        foreach ($p in $e.InstallLocation, $src) {
            if ($p -and (Test-Path -LiteralPath $p -ErrorAction SilentlyContinue)) {
                $date = (Get-Item -LiteralPath $p -ErrorAction SilentlyContinue).CreationTime.ToString('yyyy-MM-dd'); break
            }
        }
    }
    $icon = ''
    if ($src) {
        $out = Join-Path $dir (($e.PSChildName -replace '[^\w.-]', '_') + '.png')
        try {
            if (-not (Test-Path -LiteralPath $out) -and (Test-Path -LiteralPath $src -PathType Leaf -ErrorAction SilentlyContinue)) {
                [Drawing.Icon]::ExtractAssociatedIcon($src).ToBitmap().Save($out, [Drawing.Imaging.ImageFormat]::Png)
            }
            if (Test-Path -LiteralPath $out) { $icon = $out }
        } catch {}
    }
    "$($e.DisplayName)`t$($e.DisplayVersion)`t$date`t$($e.Publisher)`t$icon"
}
"#;

fn hidden(exe: &str) -> Command {
    let mut cmd = Command::new(exe);
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd
}

fn script_file(name: &str, body: &str) -> Result<PathBuf, String> {
    let path = std::env::temp_dir().join(format!("oxide-util-{name}.ps1"));
    std::fs::write(&path, body).map_err(|e| format!("Could not write {name} script: {e}"))?;
    Ok(path)
}

fn powershell(name: &str, body: &str, args: &[&str]) -> Result<std::process::Output, String> {
    hidden("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(script_file(name, body)?)
        .args(args)
        .output()
        .map_err(|e| format!("Could not run PowerShell: {e}"))
}

fn winget(args: &[&str]) -> Result<String, String> {
    let out = hidden("winget")
        .args(args)
        .args(["--accept-source-agreements", "--disable-interactivity"])
        .output()
        .map_err(|e| format!("Could not run winget: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parses the fixed-width table that `winget list` / `winget upgrade` prints.
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
    let (Some(id_col), Some(ver), Some(avail)) = (col("Id"), col("Version"), col("Available")) else {
        return vec![];
    };
    let id = id_col;
    let src = col("Source");
    lines[h + 2..]
        .iter()
        // up-to-date rows end before the Available column, so stop at the first blank line instead
        .take_while(|l| !l.trim().is_empty())
        .map(|l| {
            let c: Vec<char> = l.chars().collect();
            let cut = |a: usize, b: Option<usize>| {
                let b = b.unwrap_or(c.len()).min(c.len());
                c[a.min(b)..b].iter().collect::<String>().trim().to_string()
            };
            let id = cut(id, Some(ver));
            Pkg {
                name: cut(0, Some(id_col)),
                detail: id.clone(),
                id,
                version: cut(ver, Some(avail)),
                available: cut(avail, src),
                ..Default::default()
            }
        })
        // footer lines like "3 upgrades available." have no version and no real id
        .filter(|p| !p.id.is_empty() && !p.version.is_empty() && !p.id.contains(char::is_whitespace))
        .collect()
}

/// Parses INSTALLED output, sorted by name with exact duplicates (same name and version) removed.
fn parse_installed(out: &str) -> Vec<Installed> {
    let mut v: Vec<Installed> = out
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').map(str::trim).collect();
            let [name, version, date, publisher, icon] = f[..] else { return None };
            Some(Installed {
                name: name.into(),
                version: version.into(),
                date: date.into(),
                publisher: publisher.into(),
                icon: (!icon.is_empty()).then(|| icon.to_string()),
            })
        })
        .collect();
    v.sort_by_key(|i| (i.name.to_lowercase(), i.version.clone()));
    v.dedup_by(|a, b| a.name == b.name && a.version == b.version);
    v
}

/// Finds the Installed apps entry (for icon and install date) of a winget row by its (possibly truncated) name.
// shortcut: name-prefix matching misses apps whose winget name differs from their Add/Remove name; match on product codes if that's common.
fn find_installed<'a>(name: &str, installed: &'a [Installed]) -> Option<&'a Installed> {
    let n = name.trim_end_matches('…').trim().to_lowercase();
    if n.is_empty() {
        return None;
    }
    let lower = |s: &String| s.to_lowercase();
    installed
        .iter()
        .find(|i| lower(&i.name) == n)
        .or_else(|| installed.iter().find(|i| lower(&i.name).starts_with(&n) || n.starts_with(&lower(&i.name))))
}

/// Game launchers keep themselves (and their games) updated, so they're not listed.
fn is_launcher(id: &str) -> bool {
    const PREFIXES: [&str; 8] = [
        "Valve.Steam",
        "EpicGames.",
        "RockstarGames.",
        "ElectronicArts.",
        "Ubisoft.",
        "Blizzard.",
        "RiotGames.",
        "GOG.",
    ];
    PREFIXES.iter().any(|p| id.starts_with(p))
}

fn scan_all(packs_folder: Option<PathBuf>) -> Scan {
    std::thread::scope(|s| {
        let packs = s.spawn(move || packs_folder.map(|f| packs::index(&f)));
        // every program winget can track, up to date or not. "Unknown" versions (mostly games and
        // redistributables) can never show an update, and launchers update themselves, so both are left out.
        let sw = s.spawn(|| {
            winget(&["list", "--source", "winget"])
                .map(|o| parse(&o).into_iter().filter(|p| p.version != "Unknown" && !is_launcher(&p.id)).collect::<Vec<_>>())
        });
        let devices = s.spawn(|| {
            drivers::scan().map(|(mut rows, bios)| {
                drivers::add_catalog_offers(&mut rows);
                (rows, bios)
            })
        });
        let installed = s.spawn(|| powershell("installed", INSTALLED, &[]));

        let mut scan = Scan::default();
        let installed = match installed.join().unwrap() {
            Ok(o) => parse_installed(&String::from_utf8_lossy(&o.stdout)),
            Err(e) => {
                scan.errors.push(e);
                vec![]
            }
        };
        match sw.join().unwrap() {
            Ok(mut p) => {
                for pkg in &mut p {
                    if let Some(i) = find_installed(&pkg.name, &installed) {
                        pkg.icon = i.icon.clone();
                        pkg.date = i.date.clone();
                    }
                }
                scan.pkgs.extend(p);
            }
            Err(e) => scan.errors.push(e),
        }
        match devices.join().unwrap() {
            Ok((mut p, bios)) => {
                if let Some((drivers, errors)) = packs.join().unwrap() {
                    packs::add_offers(&mut p, &drivers);
                    scan.errors.extend(errors);
                }
                scan.pkgs.extend(p);
                scan.bios = bios;
            }
            Err(e) => scan.errors.push(e),
        }
        // problems first, then updates, then alphabetical
        scan.pkgs.sort_by_key(|p| (!matches!(p.status, Status::Missing | Status::Problem(_)), !p.has_update(), p.name.to_lowercase()));
        scan
    })
}

/// On/off settings are marker files: the setting is off when its file exists.
fn marker(name: &str) -> PathBuf {
    PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join("oxide-util").join(name)
}

fn set_marker(name: &str, on: bool) {
    let m = marker(name);
    let _ = if on {
        std::fs::remove_file(&m)
    } else {
        std::fs::create_dir_all(m.parent().unwrap()).and_then(|_| std::fs::write(&m, ""))
    };
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

/// Small colored status pill, like SDIO's per-device status.
fn badge(ui: &mut egui::Ui, p: &Pkg) {
    let (text, color) = match p.status {
        Status::Missing => ("Missing driver".to_string(), Color32::from_rgb(255, 110, 110)),
        Status::Problem(c) => (format!("Problem (code {c})"), Color32::from_rgb(255, 170, 80)),
        _ if p.has_update() => ("Update available".to_string(), ACCENT),
        Status::Inbox => ("Windows driver".to_string(), Color32::GRAY),
        Status::Ok => ("Up to date".to_string(), Color32::from_rgb(120, 220, 140)),
    };
    egui::Frame::none().fill(color.gamma_multiply(0.18)).rounding(6.0).inner_margin(egui::Margin::symmetric(6.0, 2.0)).show(ui, |ui| {
        ui.label(RichText::new(text).small().color(color));
    });
}

fn app_icon(ui: &mut egui::Ui, name: &str, icon: &Option<String>) {
    let size = egui::vec2(32.0, 32.0);
    if let Some(path) = icon {
        ui.add(egui::Image::new(format!("file://{path}")).fit_to_exact_size(size));
        return;
    }
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    ui.painter().rect_filled(rect, 8.0, ACCENT.gamma_multiply(0.2));
    let letter = name.chars().next().unwrap_or('?').to_uppercase().to_string();
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, letter, egui::FontId::proportional(16.0), ACCENT);
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

    fn scan(&mut self, ctx: &egui::Context) {
        let packs = self.packs_folder.clone();
        let label = if packs.is_some() {
            "Scanning your PC (the first scan of new driver packs can take a few minutes)..."
        } else {
            "Scanning your PC (checking drivers can take a minute)..."
        };
        self.run(ctx, label.into(), move || Msg::Scanned(scan_all(packs)));
    }

    /// Updates the given rows of one category; `None` means everything in it.
    fn update(&mut self, ctx: &egui::Context, kind: Kind, one: Option<String>) {
        let rows: Vec<Pkg> = self
            .pkgs
            .iter()
            .filter(|p| p.kind == kind && p.has_update() && one.as_ref().map_or(true, |id| &p.id == id))
            .cloned()
            .collect();
        let target = match &one {
            Some(_) => rows.first().map_or("item".into(), |p| p.name.clone()),
            None => format!("all {}", kind.label()),
        };
        let ids: Vec<String> = rows.iter().map(|p| p.id.clone()).collect();
        let restore = self.restore_point;
        self.run(ctx, format!("Updating {target}..."), move || {
            if kind != Kind::Software {
                return Msg::Updated(drivers::install(&rows, &[], restore));
            }
            // one id at a time: `--all` would also update the rows we hide
            for id in &ids {
                if let Err(e) = winget(&["upgrade", "--silent", "--accept-package-agreements", "--id", id, "--exact"]) {
                    return Msg::Updated(e);
                }
            }
            Msg::Updated(format!("Finished updating {target}."))
        });
    }

    /// Installs every driver in a folder of extracted drivers that's newer than what the devices use.
    fn install_folder(&mut self, ctx: &egui::Context) {
        let Some(folder) = rfd::FileDialog::new().set_title("Folder of drivers to install").pick_folder() else { return };
        let restore = self.restore_point;
        self.run(ctx, format!("Installing drivers from {}...", folder.display()), move || {
            Msg::Updated(drivers::install(&[], &[folder], restore))
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        if let Some(msg) = self.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.busy = None;
            self.rx = None;
            match msg {
                Msg::Scanned(s) => {
                    self.status = if s.errors.is_empty() {
                        let n = s.pkgs.iter().filter(|p| p.has_update()).count();
                        format!("{n} update(s) available across {} programs and drivers", s.pkgs.len())
                    } else {
                        s.errors.join(" ")
                    };
                    self.pkgs = s.pkgs;
                    self.bios = s.bios;
                }
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
        let (mut check_now, mut rescan, mut from_folder) = (false, false, false);
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(format!("Oxide Util v{}", env!("CARGO_PKG_VERSION")));
                ui.add_space(4.0);
                rescan = ui.add_enabled(idle, egui::Button::new("Re-scan PC")).clicked();
                ui.separator();
                ui.label(RichText::new("Drivers").strong());
                if ui.checkbox(&mut self.restore_point, "Create a restore point before installing drivers").changed() {
                    set_marker("no-restore-point", self.restore_point);
                }
                from_folder = ui
                    .add_enabled(idle, egui::Button::new("Install drivers from folder..."))
                    .on_hover_text("Pick a folder of extracted drivers (.inf files). Only drivers newer than what your devices use get installed.")
                    .clicked();
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Driver packs:");
                    match &self.packs_folder {
                        Some(f) => ui.label(RichText::new(f.display().to_string()).color(ACCENT)),
                        None => ui.label(RichText::new("not set").weak()),
                    };
                });
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(idle, egui::Button::new("Choose folder..."))
                        .on_hover_text("The folder with Snappy Driver Installer .7z driver packs (SDIO's \"drivers\" folder).")
                        .clicked()
                    {
                        if let Some(f) = rfd::FileDialog::new().set_title("Driver packs folder (.7z)").pick_folder() {
                            let _ = std::fs::create_dir_all(marker("").as_path());
                            let _ = std::fs::write(marker("driverpacks.txt"), f.to_string_lossy().as_bytes());
                            self.packs_folder = Some(f);
                            rescan = true;
                        }
                    }
                    if self.packs_folder.is_some() && ui.add_enabled(idle, egui::Button::new("Clear")).clicked() {
                        let _ = std::fs::remove_file(marker("driverpacks.txt"));
                        self.packs_folder = None;
                        rescan = true;
                    }
                });
                ui.separator();
                ui.label(RichText::new("Oxide Util updates").strong());
                if ui.checkbox(&mut self.auto_update, "Check for app updates when it opens").changed() {
                    set_marker("no-auto-update", self.auto_update);
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
        if rescan {
            self.scan(ctx);
        }
        if from_folder {
            self.install_folder(ctx);
        }

        egui::TopBottomPanel::top("top")
            .frame(egui::Frame::none().inner_margin(16.0).fill(Color32::from_rgb(18, 26, 38)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Oxide Util").size(24.0).strong().color(ACCENT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let any = self.pkgs.iter().any(|p| p.kind == self.tab && p.has_update());
                        let all = egui::Button::new(RichText::new("Update all").strong().color(Color32::BLACK)).fill(ACCENT);
                        if ui.add_enabled(idle && any, all).clicked() {
                            self.update(ctx, self.tab, None);
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
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    for k in Kind::ALL {
                        let n = self.pkgs.iter().filter(|p| p.kind == k && (p.has_update() || p.needs_attention())).count();
                        let label = if n > 0 { format!("{}  ({n})", k.label()) } else { k.label().to_string() };
                        let text = RichText::new(label).size(14.0);
                        let text = if n == 0 && self.tab != k { text.weak() } else { text };
                        let text = if self.tab == k { text.color(ACCENT).strong() } else { text };
                        if ui.selectable_label(self.tab == k, text).clicked() {
                            self.tab = k;
                        }
                    }
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.tab == Kind::Firmware && !self.bios.is_empty() {
                egui::Frame::none().fill(CARD).rounding(10.0).inner_margin(12.0).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(&self.bios).strong());
                    ui.label(
                        RichText::new("BIOS updates come from your motherboard maker. Firmware that Windows Update still has pending is listed below.")
                            .small()
                            .weak(),
                    );
                });
                ui.add_space(6.0);
            }
            let is_drivers = self.tab != Kind::Software;
            ui.horizontal(|ui| {
                ui.label("Search");
                ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("name, maker or hardware id").desired_width(260.0));
                if is_drivers {
                    let hidden = self.pkgs.iter().filter(|p| p.kind == self.tab && p.status == Status::Inbox && !p.has_update()).count();
                    ui.checkbox(&mut self.show_inbox, format!("Show Windows built-in drivers ({hidden})"));
                }
            });
            ui.add_space(6.0);
            let f = self.filter.to_lowercase();
            let rows: Vec<&Pkg> = self
                .pkgs
                .iter()
                .filter(|p| p.kind == self.tab)
                .filter(|p| self.show_inbox || p.status != Status::Inbox || p.has_update())
                .filter(|p| {
                    f.is_empty()
                        || p.name.to_lowercase().contains(&f)
                        || p.detail.to_lowercase().contains(&f)
                        || p.hwid.to_lowercase().contains(&f)
                })
                .collect();
            if rows.is_empty() {
                if idle {
                    ui.centered_and_justified(|ui| {
                        ui.label(RichText::new("Nothing found here.").size(16.0).weak());
                    });
                }
                return;
            }
            let mut clicked = None;
            egui::ScrollArea::vertical().show(ui, |ui| {
                for p in rows {
                    egui::Frame::none().fill(CARD).rounding(10.0).inner_margin(10.0).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            app_icon(ui, &p.name, &p.icon);
                            ui.vertical(|ui| {
                                ui.label(RichText::new(&p.name).size(15.0).strong());
                                let detail = if p.hwid.is_empty() { p.detail.clone() } else { format!("{}  ·  {}", p.detail, p.hwid) };
                                ui.label(RichText::new(detail).small().weak());
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if p.has_update() {
                                    if ui.add_enabled(idle, egui::Button::new("Update")).clicked() {
                                        clicked = Some(p.id.clone());
                                    }
                                    ui.label(RichText::new(&p.available).color(ACCENT).strong());
                                    if !p.version.is_empty() {
                                        ui.label("->");
                                        ui.label(RichText::new(&p.version).weak());
                                    }
                                } else {
                                    let date = if p.date.is_empty() { "date unknown" } else { &p.date };
                                    ui.add_sized([90.0, 20.0], egui::Label::new(RichText::new(date).weak()));
                                    ui.add_sized([130.0, 20.0], egui::Label::new(RichText::new(&p.version)));
                                }
                                badge(ui, p);
                            });
                        });
                    });
                    ui.add_space(6.0);
                }
            });
            if clicked.is_some() {
                self.update(ctx, self.tab, clicked);
            }
        });
    }
}

fn main() -> eframe::Result<()> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([820.0, 600.0])
            .with_title("Oxide Util")
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")).unwrap()),
        ..Default::default()
    };
    eframe::run_native(
        "Oxide Util",
        opts,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            egui_extras::install_image_loaders(&cc.egui_ctx);
            let mut app = App {
                auto_update: !marker("no-auto-update").exists(),
                restore_point: !marker("no-restore-point").exists(),
                packs_folder: std::fs::read_to_string(marker("driverpacks.txt")).ok().map(|s| PathBuf::from(s.trim())).filter(|p| p.is_dir()),
                ..Default::default()
            };
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
                detail: "Google.Chrome".into(),
                version: "130.0".into(),
                available: "131.0".into(),
                ..Default::default()
            }
        );
        assert_eq!(p[1].id, "Discord.Discord");
        assert!(parse("No installed package found matching input criteria.").is_empty());
    }

    #[test]
    fn parses_winget_list_with_up_to_date_rows() {
        // `winget list --source winget`: no Source column, and up-to-date rows stop before Available
        let out = "Name       Id               Version    Available\n\
------------------------------------------------\n\
Git        Git.Git          2.56.0     \n\
Blender    Blender.Blender  5.2.3\n\
Node.js    OpenJS.NodeJS    24.22.1    26.13.0\n";
        let p = parse(out);
        assert_eq!(p.len(), 3);
        assert!(!p[0].has_update() && !p[1].has_update());
        assert_eq!((p[1].id.as_str(), p[1].version.as_str()), ("Blender.Blender", "5.2.3"));
        assert!(p[2].has_update());
        assert_eq!(p[2].available, "26.13.0");
    }

    #[test]
    fn classifies_drivers() {
        assert_eq!(classify("Net", "Intel - Net - 23.0"), Kind::Network);
        assert_eq!(classify("Bluetooth", "x"), Kind::Network);
        assert_eq!(classify("System", "Intel - System - 2.0"), Kind::Chipset);
        assert_eq!(classify("SCSIAdapter", "Samsung NVMe"), Kind::Storage);
        assert_eq!(classify("MEDIA", "Realtek - Audio"), Kind::Audio);
        // unhelpful class falls back to the title
        assert_eq!(classify("OtherHardware", "Microsoft Corporation AudioProcessingObject Driver Update"), Kind::Audio);
        assert_eq!(classify("", "Intel(R) Management Engine Interface"), Kind::Chipset);
        assert_eq!(classify("", "Something unknown"), Kind::OtherDriver);
    }

    #[test]
    fn matches_icons_by_name() {
        let app = |name: &str, icon: Option<&str>| Installed { name: name.into(), icon: icon.map(Into::into), ..Default::default() };
        let icons = vec![app("Discord", Some("d.png")), app("Google Chrome", Some("c.png")), app("Steam", None)];
        let icon = |n: &str| find_installed(n, &icons).and_then(|i| i.icon.as_deref());
        assert_eq!(icon("Google Chr…"), Some("c.png"));
        assert_eq!(icon("discord"), Some("d.png"));
        assert_eq!(icon("Steam"), None);
        assert!(find_installed("", &icons).is_none());
    }

    #[test]
    fn parses_installed_programs() {
        let out = "Zoom\t6.2\t2026-01-02\tZoom Inc\t\n\
Discord\t1.0\t\tDiscord Inc.\tC:\\i\\d.png\n\
Discord\t1.0\t\tDiscord Inc.\tC:\\i\\d.png\n\
bad line\n";
        let v = parse_installed(out);
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].name.as_str(), v[0].icon.as_deref()), ("Discord", Some("C:\\i\\d.png")));
        assert_eq!((v[1].date.as_str(), v[1].publisher.as_str(), v[1].icon.as_deref()), ("2026-01-02", "Zoom Inc", None));
    }

    #[test]
    fn hides_launchers() {
        assert!(is_launcher("RockstarGames.Launcher"));
        assert!(is_launcher("Valve.Steam"));
        assert!(!is_launcher("Proton.ProtonVPN"));
    }

    #[test]
    fn compares_versions() {
        assert!(ver("v0.10.0") > ver("0.9.9"));
        assert!(ver("v1.0.0") > ver("0.3.0"));
        assert!(ver("v0.3.0") <= ver("0.3.0"));
    }
}
