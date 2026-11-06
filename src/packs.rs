//! Snappy Driver Installer (SDIO) driver packs: .7z archives of drivers. Each pack's .inf files are
//! extracted once (Windows' own tar.exe reads 7z) and indexed by hardware id; devices are then matched
//! against that index, and a matching driver's folder is extracted on demand to install it.

use crate::{hidden, ver, Pkg, Source, Status};
use std::path::{Path, PathBuf};

/// One driver (one .inf) that supports one hardware id.
#[derive(Clone, Debug, PartialEq)]
pub struct PackDriver {
    pub hwid: String,
    pub pack: PathBuf,
    /// path of the .inf inside the pack, '/'-separated
    pub inf: String,
    /// yyyy-mm-dd
    pub date: String,
    pub version: String,
}

fn tar() -> PathBuf {
    PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into())).join(r"System32\tar.exe")
}

fn cache_dir() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default()).join(r"oxide-util\packs")
}

/// INFs are UTF-16 (with BOM) or 8-bit text.
fn decode(bytes: &[u8]) -> String {
    match bytes {
        [0xFF, 0xFE, rest @ ..] => {
            String::from_utf16_lossy(&rest.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>())
        }
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Drops a `;` comment unless it's inside quotes.
fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ';' if !quoted => return &line[..i],
            _ => {}
        }
    }
    line
}

/// Hardware ids an INF supports on 64-bit Windows, plus its DriverVer date (yyyy-mm-dd) and version.
// shortcut: ignores OS-version decorations (NTamd64.10.0...22000) and %string% tokens in DriverVer; good enough to rank, pnputil makes the final call.
pub fn parse_inf(text: &str) -> (Vec<String>, String, String) {
    let mut sections: Vec<(String, Vec<String>)> = vec![];
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            sections.push((line[1..line.len() - 1].trim().to_lowercase(), vec![]));
        } else if let Some((_, lines)) = sections.last_mut() {
            lines.push(line.to_string());
        }
    }
    let section = |name: &str| -> Vec<&String> { sections.iter().filter(|(n, _)| n == name).flat_map(|(_, l)| l.iter()).collect() };
    let value = |line: &str| line.split_once('=').map(|(_, v)| v.trim().to_string());

    let (mut date, mut version) = (String::new(), String::new());
    if let Some(dv) = section("version").into_iter().find(|l| l.to_lowercase().starts_with("driverver")).and_then(|l| value(l)) {
        let mut parts = dv.split(',').map(str::trim);
        if let [m, d, y] = parts.next().unwrap_or("").split('/').collect::<Vec<_>>()[..] {
            date = format!("{y}-{m:0>2}-{d:0>2}");
        }
        version = parts.next().unwrap_or("").to_string();
    }

    let mut hwids = vec![];
    for entry in section("manufacturer").into_iter().filter_map(|l| value(l)) {
        let mut parts = entry.split(',').map(|p| p.trim().to_lowercase());
        let models = parts.next().unwrap_or_default();
        // 64-bit Windows only reads models sections decorated for amd64
        for deco in parts.filter(|d| d.starts_with("ntamd64")) {
            for line in section(&format!("{models}.{deco}")) {
                if let Some(v) = value(line) {
                    hwids.extend(v.split(',').skip(1).map(|h| h.trim().trim_matches('"').to_lowercase()).filter(|h| !h.is_empty()));
                }
            }
        }
    }
    hwids.sort();
    hwids.dedup();
    (hwids, date, version)
}

/// Extracts a pack's INFs (once per pack version) and returns its drivers.
fn index_pack(pack: &Path) -> Result<Vec<PackDriver>, String> {
    let meta = std::fs::metadata(pack).map_err(|e| e.to_string())?;
    let stem = pack.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let dir = cache_dir().join(&stem);
    let stamp = dir.join(".stamp");
    let want = format!("{}:{:?}", meta.len(), meta.modified().ok());
    if std::fs::read_to_string(&stamp).ok().as_deref() != Some(want.as_str()) {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let ok = hidden(&tar().to_string_lossy())
            .arg("-xf")
            .arg(pack)
            .arg("-C")
            .arg(&dir)
            // tar patterns are case-sensitive
            .args(["*.inf", "*.INF", "*.Inf"])
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| format!("Could not run tar: {e}"))?
            .success();
        // tar exits non-zero when one of the patterns matched nothing; only fail if nothing came out at all
        if !ok && walk_infs(&dir).is_empty() {
            return Err(format!("Could not read driver pack {stem}."));
        }
        let _ = std::fs::write(&stamp, &want);
    }
    let mut out = vec![];
    for inf in walk_infs(&dir) {
        let Ok(bytes) = std::fs::read(&inf) else { continue };
        let (hwids, date, version) = parse_inf(&decode(&bytes));
        let rel = inf.strip_prefix(&dir).unwrap_or(&inf).to_string_lossy().replace('\\', "/");
        out.extend(hwids.into_iter().map(|hwid| PackDriver {
            hwid,
            pack: pack.to_path_buf(),
            inf: rel.clone(),
            date: date.clone(),
            version: version.clone(),
        }));
    }
    Ok(out)
}

fn walk_infs(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("inf")) {
                out.push(p);
            }
        }
    }
    out
}

/// Every driver in every .7z pack in `folder`. Packs that can't be read are reported, not fatal.
pub fn index(folder: &Path) -> (Vec<PackDriver>, Vec<String>) {
    let packs: Vec<PathBuf> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("7z")))
        .collect();
    let (mut drivers, mut errors) = (vec![], vec![]);
    for r in std::thread::scope(|s| packs.iter().map(|p| s.spawn(move || index_pack(p))).collect::<Vec<_>>().into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()) {
        match r {
            Ok(d) => drivers.extend(d),
            Err(e) => errors.push(e),
        }
    }
    (drivers, errors)
}

/// Attaches the best pack driver to each device when it beats both the installed driver and any
/// existing offer. Like SDIO, "better" means newer by date, then by version.
pub fn add_offers(rows: &mut [Pkg], drivers: &[PackDriver]) {
    for p in rows.iter_mut().filter(|p| !p.hwids.is_empty()) {
        let best = drivers
            .iter()
            .filter(|d| p.hwids.contains(&d.hwid))
            .max_by(|a, b| (&a.date, ver(&a.version)).cmp(&(&b.date, ver(&b.version))));
        let Some(d) = best else { continue };
        let (cur_date, cur_ver) = if p.has_update() { (&p.offer_date, &p.offer_ver) } else { (&p.date, &p.version) };
        let beats = p.status == Status::Missing && !p.has_update()
            || (d.date.as_str(), ver(&d.version)) > (cur_date.as_str(), ver(cur_ver));
        if beats {
            p.source = Source::Pack;
            p.id = format!("{}|{}", d.pack.display(), d.inf);
            p.offer_date = d.date.clone();
            p.offer_ver = d.version.clone();
            let pack = d.pack.file_stem().unwrap_or_default().to_string_lossy();
            p.available = format!("{} (driver pack {pack}, {})", d.version, d.date);
        }
    }
}

/// Extracts the folder holding a pack driver's .inf (its .sys/.cat files live beside it) for installing.
pub fn extract_driver(id: &str) -> Result<PathBuf, String> {
    let (pack, inf) = id.split_once('|').ok_or("Bad driver pack reference.")?;
    let inf_dir = inf.rsplit_once('/').map_or("", |(d, _)| d);
    let out = std::env::temp_dir().join("oxide-util-drivers").join(format!("pack-{:x}", fnv(id)));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let mut cmd = hidden(&tar().to_string_lossy());
    cmd.arg("-xf").arg(pack).arg("-C").arg(&out);
    if !inf_dir.is_empty() {
        cmd.arg(inf_dir);
    }
    let ok = cmd.status().map_err(|e| format!("Could not run tar: {e}"))?.success();
    if !ok {
        return Err(format!("Could not extract {inf} from the driver pack."));
    }
    Ok(out.join(inf_dir))
}

/// Tiny stable hash for temp folder names.
fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

#[cfg(test)]
mod tests {
    use super::*;

    const INF: &str = "; sample\r\n\
[Version]\r\n\
Signature=\"$WINDOWS NT$\"\r\n\
DriverVer = 1/12/2026,2603.9.4.0 ; comment\r\n\
\r\n\
[Manufacturer]\r\n\
%Intel%=Intel,NTamd64.10.0...22000,NTx86\r\n\
\r\n\
[Intel.NTamd64.10.0...22000]\r\n\
%MEI%=MEI_Inst, PCI\\VEN_8086&DEV_7A68, PCI\\VEN_8086&DEV_7E70\r\n\
\r\n\
[Intel.NTx86]\r\n\
%MEI%=MEI_Inst, PCI\\VEN_8086&DEV_DEAD\r\n\
\r\n\
[Strings]\r\n\
Intel=\"Intel; Corp\"\r\n";

    #[test]
    fn parses_inf_for_amd64() {
        let (hwids, date, version) = parse_inf(INF);
        assert_eq!(hwids, vec!["pci\\ven_8086&dev_7a68", "pci\\ven_8086&dev_7e70"], "x86-only section ignored");
        assert_eq!((date.as_str(), version.as_str()), ("2026-01-12", "2603.9.4.0"));
    }

    #[test]
    fn decodes_utf16_infs() {
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(INF.encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(decode(&bytes), INF);
    }

    #[test]
    fn offers_newest_pack_driver_only_when_better() {
        let d = |date: &str, v: &str| PackDriver {
            hwid: "pci\\ven_8086&dev_7a68".into(),
            pack: "D:\\p\\DP_Chipset.7z".into(),
            inf: "x/mei.inf".into(),
            date: date.into(),
            version: v.into(),
        };
        let row = |date: &str, v: &str, status| Pkg {
            hwids: vec!["pci\\ven_8086&dev_7a68&subsys_1".into(), "pci\\ven_8086&dev_7a68".into()],
            date: date.into(),
            version: v.into(),
            status,
            ..Default::default()
        };
        let drivers = vec![d("2025-01-01", "2500.0"), d("2026-01-12", "2603.9.4.0")];

        let mut rows = vec![row("2025-06-01", "2552.8.10.0", Status::Ok), row("2026-05-01", "2700.0", Status::Ok), row("", "", Status::Missing)];
        add_offers(&mut rows, &drivers);
        assert_eq!(rows[0].source, Source::Pack);
        assert_eq!(rows[0].id, "D:\\p\\DP_Chipset.7z|x/mei.inf");
        assert_eq!(rows[0].available, "2603.9.4.0 (driver pack DP_Chipset, 2026-01-12)");
        assert!(!rows[1].has_update(), "installed driver is newer");
        assert!(rows[2].has_update(), "missing driver gets any match");
    }

    #[test]
    fn indexes_and_extracts_a_real_7z_pack() {
        // needs a 7z writer; NanaZip/7-Zip's `7z` is optional on dev machines, so skip quietly without it
        let tmp = std::env::temp_dir().join(format!("oxide-util-packtest-{}", std::process::id()));
        let src = tmp.join("src").join("Intel").join("x64");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("mei.inf"), INF).unwrap();
        std::fs::write(src.join("mei.sys"), "driver").unwrap();
        let pack = tmp.join("DP_Test.7z");
        let made = std::process::Command::new("7z").arg("a").arg("-t7z").arg(&pack).arg(".").current_dir(tmp.join("src")).output();
        if !made.is_ok_and(|o| o.status.success()) {
            eprintln!("skipping: no 7z on PATH");
            return;
        }
        let (drivers, errors) = index(&tmp);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(drivers.len(), 2);
        assert_eq!(drivers[0].inf, "Intel/x64/mei.inf");
        let dir = extract_driver(&format!("{}|{}", pack.display(), drivers[0].inf)).unwrap();
        assert!(dir.join("mei.sys").exists(), "driver files extracted next to the inf");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(cache_dir().join("DP_Test"));
    }
}
