#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{self, Color32, RichText};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{channel, Receiver};

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
        "system" | "usb" | "pci" | "sbp2" | "computer" | "processor" | "securitydevices" => Kind::Chipset,
        "hdc" | "scsiadapter" | "diskdrive" | "volume" | "volumesnapshot" => Kind::Storage,
        "hidclass" | "keyboard" | "mouse" | "camera" | "image" | "printer" | "biometric" | "monitor" | "smartcardreader" => Kind::Peripherals,
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

#[derive(Clone, Debug, Default, PartialEq)]
struct Pkg {
    kind: Kind,
    name: String,
    /// winget package id, or Windows Update UpdateID
    id: String,
    version: String,
    available: String,
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
    bios: String,
    tab: Kind,
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

/// Lists every pending driver/firmware update Windows Update offers (optional ones included) plus one `bios` line.
/// Tab-separated: kind, id, title, driver class, driver date.
const WU_SCAN: &str = r#"
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$b = Get-CimInstance Win32_BIOS
$m = Get-CimInstance Win32_BaseBoard
"bios`t`t$($m.Manufacturer) $($m.Product)`tBIOS $($b.SMBIOSBIOSVersion) ($($b.ReleaseDate.ToString('yyyy-MM-dd')))`t"
$r = (New-Object -ComObject Microsoft.Update.Session).CreateUpdateSearcher().Search("IsInstalled=0 and IsHidden=0 and Type='Driver'")
foreach ($u in $r.Updates) {
    $d = if ($u.DriverVerDate) { $u.DriverVerDate.ToString('yyyy-MM-dd') } else { '' }
    "driver`t$($u.Identity.UpdateID)`t$($u.Title)`t$($u.DriverClass)`t$d"
}
"#;

/// Installs the given Windows Update ids. Runs elevated. Exit: 2 ok, 3 partial, 10 ok + restart needed.
const WU_INSTALL: &str = r#"
param([string]$Ids)
$want = $Ids -split ','
$s = New-Object -ComObject Microsoft.Update.Session
$r = $s.CreateUpdateSearcher().Search("IsInstalled=0 and IsHidden=0 and Type='Driver'")
$c = New-Object -ComObject Microsoft.Update.UpdateColl
foreach ($u in $r.Updates) {
    if ($want -contains $u.Identity.UpdateID) {
        if (-not $u.EulaAccepted) { $u.AcceptEula() }
        [void]$c.Add($u)
    }
}
if ($c.Count -eq 0) { exit 2 }
$d = $s.CreateUpdateDownloader(); $d.Updates = $c; [void]$d.Download()
$i = $s.CreateUpdateInstaller(); $i.Updates = $c; $res = $i.Install()
if ($res.RebootRequired -and $res.ResultCode -eq 2) { exit 10 }
exit $res.ResultCode
"#;

/// Starts a script elevated (UAC prompt) and passes back its exit code.
const ELEVATE: &str = r#"
param([string]$Script, [string]$Ids)
$ErrorActionPreference = 'Stop'
$p = Start-Process powershell -Verb RunAs -Wait -PassThru -WindowStyle Hidden -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$Script`" -Ids $Ids"
exit $p.ExitCode
"#;

/// Prints `DisplayName<TAB>png path` for every installed app whose icon could be extracted.
const ICONS: &str = r#"
[Console]::OutputEncoding = [Text.Encoding]::UTF8
Add-Type -AssemblyName System.Drawing
$dir = Join-Path $env:LOCALAPPDATA 'oxide-util\icons'
New-Item -ItemType Directory -Force $dir | Out-Null
$keys = 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*',
        'HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*',
        'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*'
foreach ($e in Get-ItemProperty $keys -ErrorAction SilentlyContinue) {
    if (-not $e.DisplayName -or -not $e.DisplayIcon) { continue }
    $src = ($e.DisplayIcon -replace ',\s*-?\d+$', '').Trim('"', ' ')
    if (-not (Test-Path -LiteralPath $src -PathType Leaf)) { continue }
    $out = Join-Path $dir (($e.PSChildName -replace '[^\w.-]', '_') + '.png')
    try {
        if (-not (Test-Path -LiteralPath $out)) {
            [Drawing.Icon]::ExtractAssociatedIcon($src).ToBitmap().Save($out, [Drawing.Imaging.ImageFormat]::Png)
        }
        "$($e.DisplayName)`t$out"
    } catch {}
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
                ..Default::default()
            }
        })
        .filter(|p| !p.id.is_empty())
        .collect()
}

/// Parses WU_SCAN output into update rows and the BIOS summary line.
fn parse_wu(out: &str) -> (Vec<Pkg>, String) {
    let mut bios = String::new();
    let mut pkgs = vec![];
    for l in out.lines() {
        let f: Vec<&str> = l.split('\t').collect();
        let [k, id, name, class, date] = f[..] else { continue };
        match k {
            "bios" => bios = format!("{} - {}", name.trim(), class.trim()),
            "driver" => pkgs.push(Pkg {
                kind: classify(class, name),
                name: name.into(),
                id: id.into(),
                available: date.into(),
                ..Default::default()
            }),
            _ => {}
        }
    }
    (pkgs, bios)
}

/// Finds the icon for a winget row by its (possibly truncated) name.
// shortcut: name-prefix matching misses apps whose winget name differs from their Add/Remove name; match on product codes if that's common.
fn find_icon<'a>(name: &str, icons: &'a [(String, String)]) -> Option<&'a String> {
    let n = name.trim_end_matches('…').trim().to_lowercase();
    if n.is_empty() {
        return None;
    }
    let lower = |s: &String| s.to_lowercase();
    icons
        .iter()
        .find(|(d, _)| lower(d) == n)
        .or_else(|| icons.iter().find(|(d, _)| lower(d).starts_with(&n) || n.starts_with(&lower(d))))
        .map(|(_, p)| p)
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

fn scan_all() -> Scan {
    std::thread::scope(|s| {
        // no --include-unknown: rows with an "Unknown" installed version are mostly games/redistributables winget can't track
        let sw = s.spawn(|| winget(&["upgrade"]).map(|o| parse(&o).into_iter().filter(|p| !is_launcher(&p.id)).collect::<Vec<_>>()));
        let wu = s.spawn(|| powershell("scan", WU_SCAN, &[]));
        let icons = s.spawn(|| powershell("icons", ICONS, &[]));

        let mut scan = Scan::default();
        let icons: Vec<(String, String)> = match icons.join().unwrap() {
            Ok(o) => String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.split_once('\t'))
                .map(|(d, p)| (d.to_string(), p.to_string()))
                .collect(),
            Err(_) => vec![],
        };
        match sw.join().unwrap() {
            Ok(mut p) => {
                for pkg in &mut p {
                    pkg.icon = find_icon(&pkg.name, &icons).cloned();
                }
                scan.pkgs.extend(p);
            }
            Err(e) => scan.errors.push(e),
        }
        match wu.join().unwrap() {
            Ok(o) if o.status.success() => {
                let (p, bios) = parse_wu(&String::from_utf8_lossy(&o.stdout));
                scan.pkgs.extend(p);
                scan.bios = bios;
            }
            Ok(o) => scan.errors.push(format!("Windows Update scan failed: {}", String::from_utf8_lossy(&o.stderr).lines().next().unwrap_or("").trim())),
            Err(e) => scan.errors.push(e),
        }
        scan
    })
}

/// Installs driver and firmware updates elevated (one UAC prompt).
fn install_wu(ids: &[String]) -> String {
    // ids come from Windows Update itself, but they end up on a command line, so allow only GUID characters
    if ids.iter().any(|id| !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')) {
        return "Refusing to install: unexpected update id.".into();
    }
    let run = || -> Result<i32, String> {
        let script = script_file("install", WU_INSTALL)?;
        let out = powershell("elevate", ELEVATE, &["-Script", &script.to_string_lossy(), "-Ids", &ids.join(",")])?;
        Ok(out.status.code().unwrap_or(-1))
    };
    match run() {
        Ok(2) => "Driver updates installed.".into(),
        Ok(3) => "Some driver updates failed to install.".into(),
        Ok(10) => "Driver updates installed. Restart your PC to finish.".into(),
        Ok(1) => "Install cancelled (administrator permission is needed).".into(),
        Ok(c) => format!("Driver install failed (code {c})."),
        Err(e) => e,
    }
}

/// Auto-update is on unless this marker file exists.
fn no_auto_update_marker() -> PathBuf {
    PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join("oxide-util").join("no-auto-update")
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

fn app_icon(ui: &mut egui::Ui, p: &Pkg) {
    let size = egui::vec2(32.0, 32.0);
    if let Some(path) = &p.icon {
        ui.add(egui::Image::new(format!("file://{path}")).fit_to_exact_size(size));
        return;
    }
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    ui.painter().rect_filled(rect, 8.0, ACCENT.gamma_multiply(0.2));
    let letter = p.name.chars().next().unwrap_or('?').to_uppercase().to_string();
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
        self.run(ctx, "Scanning your PC (checking drivers can take a minute)...".into(), || Msg::Scanned(scan_all()));
    }

    /// Updates the given rows of one category; `None` means everything in it.
    fn update(&mut self, ctx: &egui::Context, kind: Kind, one: Option<String>) {
        let target = one.clone().unwrap_or_else(|| format!("all {}", kind.label()));
        let ids: Vec<String> = match &one {
            Some(id) => vec![id.clone()],
            None => self.pkgs.iter().filter(|p| p.kind == kind).map(|p| p.id.clone()).collect(),
        };
        self.run(ctx, format!("Updating {target}..."), move || {
            if kind != Kind::Software {
                return Msg::Updated(install_wu(&ids));
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
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        if let Some(msg) = self.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.busy = None;
            self.rx = None;
            match msg {
                Msg::Scanned(s) => {
                    self.status = if s.errors.is_empty() {
                        format!("{} update(s) available", s.pkgs.len())
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
        let (mut check_now, mut rescan) = (false, false);
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(format!("Oxide Util v{}", env!("CARGO_PKG_VERSION")));
                ui.add_space(4.0);
                rescan = ui.add_enabled(idle, egui::Button::new("Re-scan PC")).clicked();
                ui.separator();
                if ui.checkbox(&mut self.auto_update, "Check for app updates when it opens").changed() {
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
        if rescan {
            self.scan(ctx);
        }

        egui::TopBottomPanel::top("top")
            .frame(egui::Frame::none().inner_margin(16.0).fill(Color32::from_rgb(18, 26, 38)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Oxide Util").size(24.0).strong().color(ACCENT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let any = self.pkgs.iter().any(|p| p.kind == self.tab);
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
                        let n = self.pkgs.iter().filter(|p| p.kind == k).count();
                        let text = RichText::new(format!("{}  {n}", k.label())).size(14.0);
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
            let rows: Vec<&Pkg> = self.pkgs.iter().filter(|p| p.kind == self.tab).collect();
            if rows.is_empty() {
                if idle {
                    ui.centered_and_justified(|ui| {
                        ui.label(RichText::new("Nothing to update here.").size(16.0).weak());
                    });
                }
                return;
            }
            let mut clicked = None;
            egui::ScrollArea::vertical().show(ui, |ui| {
                for p in rows {
                    egui::Frame::none().fill(CARD).rounding(10.0).inner_margin(12.0).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            app_icon(ui, p);
                            ui.vertical(|ui| {
                                ui.label(RichText::new(&p.name).size(15.0).strong());
                                if p.kind == Kind::Software {
                                    ui.label(RichText::new(&p.id).small().weak());
                                }
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.add_enabled(idle, egui::Button::new("Update")).clicked() {
                                    clicked = Some(p.id.clone());
                                }
                                ui.label(RichText::new(&p.available).color(ACCENT).strong());
                                if !p.version.is_empty() {
                                    ui.label("->");
                                    ui.label(RichText::new(&p.version).weak());
                                }
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
                ..Default::default()
            }
        );
        assert_eq!(p[1].id, "Discord.Discord");
        assert!(parse("No installed package found matching input criteria.").is_empty());
    }

    #[test]
    fn parses_driver_scan() {
        let out = "bios\t\tASUSTeK ROG STRIX B650\tBIOS 2613 (2024-05-01)\t\n\
driver\tabc-123\tNVIDIA - Display - 32.0.15.6094\tDisplay\t2026-10-01\n\
driver\tdef-456\tIntel - Firmware - 1.2.3\tFirmware\t2026-09-01\n\
garbage line\n";
        let (p, bios) = parse_wu(out);
        assert_eq!(bios, "ASUSTeK ROG STRIX B650 - BIOS 2613 (2024-05-01)");
        assert_eq!(p.len(), 2);
        assert_eq!((p[0].kind, p[0].id.as_str(), p[0].available.as_str()), (Kind::Graphics, "abc-123", "2026-10-01"));
        assert_eq!(p[1].kind, Kind::Firmware);
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
        let icons = vec![
            ("Discord".to_string(), "d.png".to_string()),
            ("Google Chrome".to_string(), "c.png".to_string()),
        ];
        assert_eq!(find_icon("Google Chr…", &icons).map(String::as_str), Some("c.png"));
        assert_eq!(find_icon("discord", &icons).map(String::as_str), Some("d.png"));
        assert_eq!(find_icon("Steam", &icons), None);
        assert_eq!(find_icon("", &icons), None);
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
