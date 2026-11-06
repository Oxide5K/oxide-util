//! Device and driver scanning (like Snappy Driver Installer): every device in the PC, its current
//! driver, and newer drivers from Windows Update and the Microsoft Update Catalog. Installs run
//! elevated in one batch, optionally behind a restore point.

use crate::{classify, hidden, powershell, script_file, ver, Kind, Pkg, Source, Status};
use std::path::PathBuf;

/// Lists the BIOS, every present device with its driver, and pending Windows Update drivers.
/// Tab-separated lines:
///   bios   <board> <bios>
///   device <hwid> <name> <class> <date> <version> <provider> <error code> <all hwids, |-joined>
///   wu     <update id> <title> <class> <date> <hwid> <provider>
const SCAN: &str = r#"
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$b = Get-CimInstance Win32_BIOS
$m = Get-CimInstance Win32_BaseBoard
"bios`t$($m.Manufacturer) $($m.Product)`tBIOS $($b.SMBIOSBIOSVersion) ($($b.ReleaseDate.ToString('yyyy-MM-dd')))"
$drv = @{}
foreach ($d in Get-CimInstance Win32_PnPSignedDriver) { if ($d.DeviceID) { $drv[$d.DeviceID] = $d } }
foreach ($e in Get-CimInstance Win32_PnPEntity) {
    if (-not $e.Present -or -not $e.Name -or -not $e.HardwareID) { continue }
    $d = $drv[$e.DeviceID]
    $date = if ($d -and $d.DriverDate) { $d.DriverDate.ToString('yyyy-MM-dd') } else { '' }
    "device`t$($e.HardwareID[0])`t$($e.Name)`t$($e.PNPClass)`t$date`t$($d.DriverVersion)`t$($d.DriverProviderName)`t$($e.ConfigManagerErrorCode)`t$($e.HardwareID -join '|')"
}
$r = (New-Object -ComObject Microsoft.Update.Session).CreateUpdateSearcher().Search("IsInstalled=0 and IsHidden=0 and Type='Driver'")
foreach ($u in $r.Updates) {
    $date = if ($u.DriverVerDate) { $u.DriverVerDate.ToString('yyyy-MM-dd') } else { '' }
    "wu`t$($u.Identity.UpdateID)`t$($u.Title)`t$($u.DriverClass)`t$date`t$($u.DriverHardwareID)`t$($u.DriverProvider)"
}
"#;

/// Runs elevated. Reads instructions from -ArgFile (lines: `restore`, `wu <id>`, `folder <path>`)
/// and appends one human-readable result per step to "<ArgFile>.out" (`RESTART` means a reboot is needed).
const INSTALL: &str = r#"
param([string]$ArgFile)
$out = "$ArgFile.out"
Set-Content -LiteralPath $out -Value ''
function say($m) { Add-Content -LiteralPath $out -Value $m }
$lines = @(Get-Content -LiteralPath $ArgFile)
if ($lines -contains 'restore') {
    try {
        Checkpoint-Computer -Description 'Oxide Util driver install' -RestorePointType MODIFY_SETTINGS -ErrorAction Stop -WarningVariable w 3>$null
        if ($w) { say "Restore point skipped: Windows only allows one per day." } else { say 'Restore point created.' }
    } catch { say "Restore point not created ($($_.Exception.Message.Trim())). Is System Protection on?" }
}
$wu = @($lines | Where-Object { $_ -like 'wu *' } | ForEach-Object { $_.Substring(3) })
if ($wu.Count) {
    $s = New-Object -ComObject Microsoft.Update.Session
    $r = $s.CreateUpdateSearcher().Search("IsInstalled=0 and IsHidden=0 and Type='Driver'")
    $c = New-Object -ComObject Microsoft.Update.UpdateColl
    foreach ($u in $r.Updates) {
        if ($wu -contains $u.Identity.UpdateID) { if (-not $u.EulaAccepted) { $u.AcceptEula() }; [void]$c.Add($u) }
    }
    if ($c.Count) {
        $d = $s.CreateUpdateDownloader(); $d.Updates = $c; [void]$d.Download()
        $i = $s.CreateUpdateInstaller(); $i.Updates = $c; $res = $i.Install()
        $word = @{ 2 = 'installed'; 3 = 'partly installed' }[[int]$res.ResultCode]
        say ("Windows Update drivers " + $(if ($word) { $word } else { "failed (code $($res.ResultCode))" }) + '.')
        if ($res.RebootRequired) { say 'RESTART' }
    }
}
foreach ($f in @($lines | Where-Object { $_ -like 'folder *' } | ForEach-Object { $_.Substring(7) })) {
    pnputil /add-driver (Join-Path $f '*.inf') /subdirs /install | Out-Null
    switch ($LASTEXITCODE) {
        0 { say "Installed drivers from $f." }
        3010 { say "Installed drivers from $f."; say 'RESTART' }
        259 { say "Nothing in $f was newer than what your devices already use." }
        default { say "Installing from $f failed (pnputil code $LASTEXITCODE)." }
    }
}
"#;

/// Starts INSTALL elevated (one UAC prompt) and waits for it.
const ELEVATE: &str = r#"
param([string]$Script, [string]$ArgFile)
$ErrorActionPreference = 'Stop'
$p = Start-Process powershell -Verb RunAs -Wait -PassThru -WindowStyle Hidden -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$Script`" -ArgFile `"$ArgFile`""
exit $p.ExitCode
"#;

const CATALOG: &str = "https://www.catalog.update.microsoft.com";

fn status_of(code: &str, version: &str, provider: &str) -> Status {
    match code.parse::<u32>().unwrap_or(0) {
        // 1 = not configured, 28 = drivers not installed
        1 | 28 => Status::Missing,
        0 if version.is_empty() => Status::Missing,
        0 if provider == "Microsoft" => Status::Inbox,
        0 => Status::Ok,
        c => Status::Problem(c),
    }
}

/// Parses SCAN output into device rows (with Windows Update offers attached) and the BIOS line.
pub fn parse_scan(out: &str) -> (Vec<Pkg>, String) {
    let mut bios = String::new();
    let mut rows: Vec<Pkg> = vec![];
    for l in out.lines() {
        let f: Vec<&str> = l.split('\t').map(str::trim).collect();
        match f[..] {
            ["bios", board, b] => bios = format!("{board} - {b}"),
            ["device", hwid, name, class, date, version, provider, code, hwids] => {
                let status = status_of(code, version, provider);
                // identical devices (e.g. 32 CPU threads) share one row
                if rows.iter().any(|p| p.name == name && p.version == version && p.status == status) {
                    continue;
                }
                rows.push(Pkg {
                    kind: classify(class, name),
                    name: name.into(),
                    detail: if provider.is_empty() { "No driver".into() } else { provider.into() },
                    hwid: hwid.into(),
                    hwids: hwids.split('|').map(str::to_lowercase).collect(),
                    version: version.into(),
                    date: date.into(),
                    status,
                    ..Default::default()
                });
            }
            ["wu", id, title, class, date, hwid, provider] => {
                let offer = format!("{} (Windows Update)", title_version(title).unwrap_or(date));
                let offer_ver = title_version(title).unwrap_or("").to_string();
                let h = hwid.to_lowercase();
                match rows.iter_mut().find(|p| !h.is_empty() && p.hwids.contains(&h)) {
                    Some(p) => {
                        p.id = id.into();
                        p.source = Source::WindowsUpdate;
                        p.available = offer;
                        p.offer_ver = offer_ver;
                        p.offer_date = date.into();
                    }
                    None => rows.push(Pkg {
                        kind: classify(class, title),
                        name: title.into(),
                        detail: provider.into(),
                        hwid: hwid.into(),
                        id: id.into(),
                        source: Source::WindowsUpdate,
                        available: offer,
                        offer_ver,
                        offer_date: date.into(),
                        ..Default::default()
                    }),
                }
            }
            _ => {}
        }
    }
    (rows, bios)
}

/// "NVIDIA - Display - 32.0.15.6094" / "Foo Update (1.0.4.55794)" -> the version, if the title has one.
fn title_version(title: &str) -> Option<&str> {
    let t = title.trim_end_matches(')');
    let last = t.rsplit(['(', ' ']).next()?;
    (last.contains('.') && last.chars().all(|c| c.is_ascii_digit() || c == '.')).then_some(last)
}

/// Catalog search key: maker + device part of a hardware id ("PCI\VEN_10DE&DEV_2C02&SUBSYS_..." ->
/// "PCI\VEN_10DE&DEV_2C02"). The full id is usually too specific for the catalog to match.
pub fn catalog_key(hwid: &str) -> String {
    hwid.split('&').take(2).collect::<Vec<_>>().join("&")
}

#[derive(Debug, PartialEq)]
pub struct CatalogHit {
    pub id: String,
    pub title: String,
    pub date: String,
    pub version: String,
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&").replace("&nbsp;", " ").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Picks the newest Windows 10/11 driver from a catalog search results page.
// shortcut: scrapes the catalog's HTML (it has no public API); if Microsoft changes the page this finds nothing rather than breaking.
pub fn parse_catalog(html: &str) -> Option<CatalogHit> {
    let mut best: Option<CatalogHit> = None;
    for row in html.split("<tr id=\"").skip(1) {
        let Some((id, rest)) = row.split_once("_R") else { continue };
        let body = rest.split("</tr>").next().unwrap_or("");
        let cells: Vec<String> = body.split("<td").skip(1).map(|c| strip_tags(&format!("<td{c}"))).collect();
        // columns: [checkbox, title, products, classification, last updated, version, size, ...]
        let [_, title, products, _, date, version, ..] = &cells[..] else { continue };
        if !products.contains("Windows 1") || version.is_empty() {
            continue;
        }
        let date = match date.split('/').collect::<Vec<_>>()[..] {
            [m, d, y] => format!("{y}-{m:0>2}-{d:0>2}"),
            _ => date.clone(),
        };
        if best.as_ref().map_or(true, |b| ver(version) > ver(&b.version)) {
            best = Some(CatalogHit { id: id.into(), title: title.clone(), date, version: version.clone() });
        }
    }
    best
}

fn catalog_search(key: &str) -> Option<CatalogHit> {
    let out = hidden("curl.exe")
        .args(["-s", "--max-time", "30", "-G", &format!("{CATALOG}/Search.aspx"), "--data-urlencode"])
        .arg(format!("q={key}"))
        .output()
        .ok()?;
    parse_catalog(&String::from_utf8_lossy(&out.stdout))
}

/// Looks up every vendor-driver or driverless device in the Update Catalog and attaches newer drivers.
/// Devices on Microsoft's built-in drivers are skipped: there are hundreds and the catalog rarely beats them.
pub fn add_catalog_offers(rows: &mut [Pkg]) {
    let keys: Vec<String> = {
        let mut k: Vec<String> = rows
            .iter()
            .filter(|p| p.kind != Kind::Software && !p.has_update() && !p.hwid.is_empty())
            .filter(|p| matches!(p.status, Status::Ok | Status::Missing | Status::Problem(_)))
            .map(|p| catalog_key(&p.hwid))
            .collect();
        k.sort();
        k.dedup();
        k
    };
    // 8 lookups at a time keeps the scan quick without hammering the site
    let hits: Vec<(String, CatalogHit)> = keys
        .chunks(8)
        .flat_map(|chunk| {
            std::thread::scope(|s| {
                let handles: Vec<_> = chunk.iter().map(|k| s.spawn(move || catalog_search(k).map(|h| (k.clone(), h)))).collect();
                handles.into_iter().filter_map(|h| h.join().ok().flatten()).collect::<Vec<_>>()
            })
        })
        .collect();
    for p in rows.iter_mut() {
        if p.has_update() || p.hwid.is_empty() {
            continue;
        }
        let key = catalog_key(&p.hwid);
        let Some((_, hit)) = hits.iter().find(|(k, _)| *k == key) else { continue };
        // like SDIO, a driver must be at least as new by date as well as by version (catches mis-versioned
        // catalog entries); yyyy-mm-dd strings compare correctly as text
        let newer = p.status == Status::Missing || (ver(&hit.version) > ver(&p.version) && hit.date >= p.date);
        if newer && p.status != Status::Inbox {
            p.id = hit.id.clone();
            p.source = Source::Catalog;
            p.available = format!("{} (Update Catalog, {})", hit.version, hit.date);
            p.offer_ver = hit.version.clone();
            p.offer_date = hit.date.clone();
        }
    }
}

/// Resolves a catalog update id to its .cab download URL.
fn catalog_download_url(id: &str) -> Result<String, String> {
    let body = format!(r#"updateIDs=[{{"size":0,"languages":"","uidInfo":"{id}","updateID":"{id}"}}]"#);
    let out = hidden("curl.exe")
        .args(["-s", "--max-time", "60", "-X", "POST", &format!("{CATALOG}/DownloadDialog.aspx"), "--data-binary"])
        .arg(body)
        .output()
        .map_err(|e| format!("Could not run curl: {e}"))?;
    let html = String::from_utf8_lossy(&out.stdout);
    html.split(['\'', '"'])
        .find(|s| s.starts_with("https://") && s.ends_with(".cab"))
        .map(str::to_string)
        .ok_or_else(|| format!("The Update Catalog gave no download for {id}."))
}

/// Downloads and unpacks a catalog driver into a folder pnputil can install from.
fn fetch_catalog_driver(id: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("oxide-util-drivers").join(id);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let cab = dir.with_extension("cab");
    let ok = hidden("curl.exe")
        .args(["-sfL", "--max-time", "1800", "-o"])
        .arg(&cab)
        .arg(catalog_download_url(id)?)
        .status()
        .map_err(|e| format!("Could not run curl: {e}"))?
        .success();
    if !ok {
        return Err("Downloading a driver from the Update Catalog failed.".into());
    }
    let ok = hidden("expand.exe").arg("-F:*").arg(&cab).arg(&dir).status().map_err(|e| e.to_string())?.success();
    let _ = std::fs::remove_file(&cab);
    if !ok {
        return Err("Unpacking a downloaded driver failed.".into());
    }
    Ok(dir)
}

/// Installs Windows Update and catalog drivers, plus any extra folders, in one elevated batch.
pub fn install(rows: &[Pkg], folders: &[PathBuf], restore_point: bool) -> String {
    let mut lines = vec![];
    let mut notes = vec![];
    if restore_point {
        lines.push("restore".to_string());
    }
    for p in rows {
        // Windows Update / catalog ids reach a script and a URL, so allow only GUID characters
        let guid = p.id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
        if matches!(p.source, Source::WindowsUpdate | Source::Catalog) && !guid {
            return "Refusing to install: unexpected update id.".into();
        }
        let folder = match p.source {
            Source::WindowsUpdate => {
                lines.push(format!("wu {}", p.id));
                continue;
            }
            Source::Catalog => fetch_catalog_driver(&p.id),
            Source::Pack => crate::packs::extract_driver(&p.id),
            Source::Winget => continue,
        };
        match folder {
            Ok(dir) => lines.push(format!("folder {}", dir.display())),
            Err(e) => notes.push(format!("{}: {e}", p.name)),
        }
    }
    lines.extend(folders.iter().map(|f| format!("folder {}", f.display())));
    if lines.iter().all(|l| l == "restore") {
        return if notes.is_empty() { "Nothing to install.".into() } else { notes.join(" ") };
    }
    let run = || -> Result<String, String> {
        let args = std::env::temp_dir().join("oxide-util-install.txt");
        std::fs::write(&args, lines.join("\n")).map_err(|e| e.to_string())?;
        let script = script_file("install", INSTALL)?;
        let code = powershell("elevate", ELEVATE, &["-Script", &script.to_string_lossy(), "-ArgFile", &args.to_string_lossy()])?
            .status
            .code();
        if code == Some(1) {
            return Err("Install cancelled (administrator permission is needed).".into());
        }
        Ok(std::fs::read_to_string(args.with_extension("txt.out")).unwrap_or_default())
    };
    match run() {
        Ok(out) => {
            let restart = out.lines().any(|l| l == "RESTART");
            notes.extend(out.lines().filter(|l| !l.trim().is_empty() && *l != "RESTART").map(str::to_string));
            if restart {
                notes.push("Restart your PC to finish.".into());
            }
            notes.join(" ")
        }
        Err(e) => e,
    }
}

pub fn scan() -> Result<(Vec<Pkg>, String), String> {
    let o = powershell("scan", SCAN, &[])?;
    if !o.status.success() {
        return Err(format!("Driver scan failed: {}", String::from_utf8_lossy(&o.stderr).lines().next().unwrap_or("").trim()));
    }
    Ok(parse_scan(&String::from_utf8_lossy(&o.stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_devices_and_attaches_wu_offers() {
        let out = "bios\tASUS TUF Z790\tBIOS 1836 (2026-04-15)\n\
device\tPCI\\VEN_8086&DEV_125C\tIntel(R) Ethernet I226-V\tNet\t2025-01-10\t2.1.5.12\tIntel\t0\tPCI\\VEN_8086&DEV_125C|PCI\\VEN_8086\n\
device\tACPI\\GenuineIntel\tIntel Core i9\tProcessor\t2006-06-21\t10.0.1\tMicrosoft\t0\tACPI\\GenuineIntel\n\
device\tACPI\\GenuineIntel\tIntel Core i9\tProcessor\t2006-06-21\t10.0.1\tMicrosoft\t0\tACPI\\GenuineIntel\n\
device\tPCI\\VEN_1234&DEV_0001\tUnknown device\t\t\t\t\t28\tPCI\\VEN_1234&DEV_0001\n\
device\tUSB\\VID_1&PID_2\tBroken thing\tUSB\t2020-01-01\t1.0\tAcme\t10\tUSB\\VID_1&PID_2\n\
wu\tabc-123\tVoice Clarity Update (1.0.4.55794)\tOtherHardware\t2026-10-16\tPCI\\VEN_8086&DEV_125C\tMicrosoft\n\
wu\tdef-456\tIntel - Firmware - 1.2.3\tFirmware\t2026-09-01\tFW\\X\tIntel\n";
        let (p, bios) = parse_scan(out);
        assert_eq!(bios, "ASUS TUF Z790 - BIOS 1836 (2026-04-15)");
        assert_eq!(p.len(), 5, "duplicate CPU rows collapse; unmatched WU driver gets its own row");
        assert_eq!((p[0].kind, p[0].source, p[0].id.as_str()), (Kind::Network, Source::WindowsUpdate, "abc-123"));
        assert_eq!(p[0].available, "1.0.4.55794 (Windows Update)");
        assert_eq!(p[1].status, Status::Inbox);
        assert_eq!((p[2].status, p[2].detail.as_str()), (Status::Missing, "No driver"));
        assert_eq!(p[3].status, Status::Problem(10));
        assert_eq!((p[4].kind, p[4].available.as_str()), (Kind::Firmware, "1.2.3 (Windows Update)"));
    }

    /// Runs the real scan + catalog lookups on this PC: `cargo test --release -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_scan() {
        let t = std::time::Instant::now();
        let (mut rows, bios) = scan().unwrap();
        let scanned = t.elapsed();
        add_catalog_offers(&mut rows);
        println!("{bios}\n{} device rows, scan {scanned:?}, with catalog {:?}", rows.len(), t.elapsed());
        for p in rows.iter().filter(|p| p.status != Status::Inbox || p.has_update()) {
            println!("{:?} {:?} | {} | {} | {} -> {}", p.kind, p.status, p.name, p.hwid, p.version, p.available);
        }
    }

    #[test]
    fn builds_catalog_keys() {
        assert_eq!(catalog_key("PCI\\VEN_10DE&DEV_2C02&SUBSYS_209510DE&REV_A1"), "PCI\\VEN_10DE&DEV_2C02");
        assert_eq!(catalog_key("ACPI\\INTC1085"), "ACPI\\INTC1085");
    }

    #[test]
    fn picks_newest_windows_driver_from_catalog_page() {
        let row = |id: &str, title: &str, products: &str, date: &str, v: &str| {
            format!(
                "<tr id=\"{id}_R0\"><td><input/></td><td><a>{title}</a></td><td>{products}</td><td>Drivers (Video)</td>\
                 <td>{date}</td><td>{v}</td><td>1.0 MB</td></tr>"
            )
        };
        let html = [
            row("aaa", "Old", "Windows 11 Client", "1/2/2025", "31.0.1"),
            row("bbb", "New &amp; shiny", "Windows 10 and later", "7/21/2026", "32.0.16.1088"),
            row("ccc", "Server only", "Windows Server 2022", "8/1/2026", "40.0"),
        ]
        .concat();
        let hit = parse_catalog(&html).unwrap();
        assert_eq!(hit, CatalogHit { id: "bbb".into(), title: "New & shiny".into(), date: "2026-07-21".into(), version: "32.0.16.1088".into() });
        assert!(parse_catalog("<html>no results</html>").is_none());
    }

    #[test]
    fn reads_versions_from_titles() {
        assert_eq!(title_version("Foo Update (1.0.4.55794)"), Some("1.0.4.55794"));
        assert_eq!(title_version("NVIDIA - Display - 32.0.15.6094"), Some("32.0.15.6094"));
        assert_eq!(title_version("No version here"), None);
    }
}
