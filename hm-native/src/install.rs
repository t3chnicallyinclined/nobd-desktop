//! Stage 3 — driver install (ELEVATED, one-time). We ship a PRE-SIGNED vendored
//! bundle (`hidmaestro.inf` + `.cat` + `HIDMaestro.dll`, signed at our build
//! time with a cert we control), so runtime install is just: trust the cert,
//! then `pnputil /add-driver /install`. No signtool/inf2cat at runtime.

use std::io;
use std::os::windows::process::CommandExt;
use std::process::Command;

/// Don't flash a console window for each helper (certutil/pnputil) — the app is
/// a windowless GUI, so a spawned console pops up and vanishes otherwise.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn run(exe: &str, args: &[&str]) -> io::Result<()> {
    let status = Command::new(exe)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{exe} {args:?} failed: {status}")))
    }
}

/// Import the bundle's signing cert into LocalMachine Root + TrustedPublisher
/// (mandatory for a self-signed UMDF driver), then add+install the package.
/// `cert_path` = our `.cer`; `inf_path` = the vendored `hidmaestro.inf`.
pub fn install_driver(cert_path: &str, inf_path: &str) -> io::Result<()> {
    run("certutil", &["-addstore", "-f", "Root", cert_path])?;
    run("certutil", &["-addstore", "-f", "TrustedPublisher", cert_path])?;
    // pnputil's exit code is locale-flaky, so we verify via the DriverStore below.
    let _ = Command::new("pnputil")
        .args(["/add-driver", inf_path, "/install"])
        .creation_flags(CREATE_NO_WINDOW)
        .status()?;
    let inf_name = std::path::Path::new(inf_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("hidmaestro.inf");
    // Verify by version where we can. `package_installed` is a filename-prefix
    // scan of the DriverStore, so ANY leftover copy of the package satisfied it
    // - a genuinely failed pnputil reported success whenever an older version
    // happened to still be present.
    let ok = match inf_driver_ver(inf_path) {
        Some(want) => enum_packages(inf_name.trim_end_matches(".inf"))
            .iter()
            .any(|p| p.original.eq_ignore_ascii_case(inf_name) && p.version == want),
        None => package_installed(inf_name),
    };
    if ok {
        Ok(())
    } else {
        Err(io::Error::other("pnputil /add-driver did not populate the DriverStore"))
    }
}

/// True if a driver package for `inf_name` (e.g. "hidmaestro.inf" or
/// "hidmaestro_xusb.inf") is present in the DriverStore.
pub fn package_installed(inf_name: &str) -> bool {
    let prefix = format!("{}_amd64_", inf_name.to_ascii_lowercase());
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let repo = format!(r"{root}\System32\DriverStore\FileRepository");
    std::fs::read_dir(repo)
        .map(|rd| {
            rd.flatten().any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .starts_with(&prefix)
            })
        })
        .unwrap_or(false)
}

/// True if a `hidmaestro.inf` package is present in the DriverStore — the
/// reliable success signal (pnputil rc is unreliable across locales).
pub fn driver_installed() -> bool {
    installed_inf_path().is_some()
}

/// The `DriverVer` version field of an INF — e.g. `1.3.17.0` from
/// `DriverVer = 06/11/2026,1.3.17.0`. This is what lets us tell OUR bundle apart
/// from a copy an older NOBD release left in the DriverStore.
pub fn inf_driver_ver(inf_path: &str) -> Option<String> {
    let text = std::fs::read_to_string(inf_path).ok()?;
    for line in text.lines() {
        let l = line.trim();
        if !l.to_ascii_lowercase().starts_with("driverver") {
            continue;
        }
        // `DriverVer = <date>,<version>`, but every part is optional in the
        // wild: the date may be absent, and a trailing `; comment` is legal.
        // Splitting on ',' alone returned the WHOLE LINE for a dateless
        // DriverVer, which then matched nothing and made the package look
        // permanently stale - a delete-and-reinstall on every single run.
        let rhs = l.split_once('=').map(|(_, r)| r).unwrap_or(l);
        let rhs = rhs.split(';').next().unwrap_or(rhs);
        let v = rhs.rsplit(',').next()?.trim();
        if v.is_empty() {
            return None;
        }
        return Some(norm_ver(v));
    }
    None
}

/// Pad a version to the 4 parts pnputil always prints, so `1.3.17` and
/// `1.3.17.0` compare equal instead of forcing a reinstall forever.
pub fn norm_ver(v: &str) -> String {
    let mut parts: Vec<&str> = v.split('.').collect();
    if parts.len() > 4 || parts.iter().any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit())) {
        return v.to_owned(); // not a version quad - compare it verbatim
    }
    while parts.len() < 4 {
        parts.push("0");
    }
    parts.join(".")
}

/// One driver package as `pnputil /enum-drivers` reports it.
#[derive(Clone, Debug)]
pub struct InstalledPkg {
    /// `oemNN.inf` — the name `pnputil /delete-driver` takes.
    pub published: String,
    /// The original INF name, e.g. `hidmaestro_xusb.inf`.
    pub original: String,
    /// Version quad, e.g. `1.3.17.0`. Empty when it could not be parsed.
    pub version: String,
}

/// Every installed package whose block mentions `needle` (matched case-insensitively).
///
/// pnputil's field LABELS are localized, so nothing here reads them: the
/// published name is the token shaped `oemNN.inf`, the original is the token
/// ending `.inf` that contains the needle, and the version is the dotted quad.
pub fn enum_packages(needle: &str) -> Vec<InstalledPkg> {
    let output = match Command::new("pnputil")
        .arg("/enum-drivers")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let needle = needle.to_ascii_lowercase();
    let mut out = Vec::new();
    for block in text.split("\r\n\r\n") {
        let lower = block.to_ascii_lowercase();
        if !lower.contains(&needle) {
            continue;
        }
        let mut published = String::new();
        let mut original = String::new();
        let mut version = String::new();
        for tok in block.split_whitespace() {
            let t = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '_');
            let tl = t.to_ascii_lowercase();
            if tl.starts_with("oem") && tl.ends_with(".inf") {
                published = t.to_owned();
            } else if tl.ends_with(".inf") && tl.contains(&needle) {
                original = t.to_owned();
            } else if t.split('.').count() == 4
                && t.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
            {
                // LAST quad wins, not the first: some classes print a
                // `Class Version` BEFORE `Driver Version`, and nothing after the
                // driver version is numeric. Taking the first would read the
                // class version and make every package look stale forever.
                version = t.to_owned();
            }
        }
        if !published.is_empty() {
            out.push(InstalledPkg { published, original, version });
        }
    }
    out
}

/// Delete these packages from the DriverStore. Returns how many succeeded.
pub fn remove_packages(pkgs: &[InstalledPkg]) -> u32 {
    let mut removed = 0;
    for p in pkgs {
        let ok = Command::new("pnputil")
            .args(["/delete-driver", &p.published, "/uninstall", "/force"])
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            removed += 1;
        }
    }
    removed
}

/// Remove EVERY NOBD driver package, of any version.
///
/// The needle is `hidmaestro`, not `hidmaestro.inf`: the XInput package is
/// `hidmaestro_xusb.inf`, which does not contain the string `hidmaestro.inf`, so
/// the old substring test skipped it entirely and left the XUSB driver installed
/// while reporting success.
pub fn uninstall_hidmaestro() -> u32 {
    remove_packages(&enum_packages("hidmaestro"))
}

/// Install `inf_path`, first clearing out any OLDER copy of the same package.
///
/// The previous logic was `if !package_installed(inf) { install() }`, which on an
/// upgrade found the PREVIOUS release's package already present and skipped the
/// install — so a new NOBD kept running the old driver forever. Now the DriverVer
/// of the vendored INF is compared against what is installed, stale copies are
/// deleted, and ours is installed when it is not already the live one.
pub fn ensure_driver(cert_path: &str, inf_path: &str, inf_name: &str) -> io::Result<()> {
    // An unreadable DriverVer used to mean "everything is stale", which DELETED
    // every installed NOBD package and then tried to install from that same
    // unreadable INF. Bail before touching the store instead.
    let want = inf_driver_ver(inf_path)
        .ok_or_else(|| io::Error::other(format!("no readable DriverVer in {inf_name}")))?;
    let needle = inf_name.trim_end_matches(".inf");
    let mine = |p: &InstalledPkg| p.original.eq_ignore_ascii_case(inf_name);

    let current = enum_packages(needle).iter().filter(|p| mine(p)).any(|p| p.version == want);
    if !current {
        // INSTALL FIRST. Deleting the old package before its replacement exists
        // runs `/delete-driver /uninstall /force` against the binding of a LIVE
        // devnode, leaving the NOBD Controller driverless (Code 28) in the
        // window between. If setup then died there, the machine was left with a
        // reboot-surviving driverless devnode that `is_present()` still counts,
        // so the app reported Working and never offered to reinstall.
        // pnputil is happy to hold two versions of a package at once.
        install_driver(cert_path, inf_path)?;
    }

    // Only now retire anything that is not the version we just put in.
    let stale: Vec<InstalledPkg> = enum_packages(needle)
        .into_iter()
        .filter(|p| mine(p) && p.version != want)
        .collect();
    if !stale.is_empty() {
        let n = remove_packages(&stale);
        if (n as usize) < stale.len() {
            // Not fatal - ours is installed and live - but two versions in the
            // store means Windows picks the binding by rank, not by our intent.
            return Err(io::Error::other(format!(
                "installed {inf_name} {want}, but {} older copy/copies could not be removed",
                stale.len() - n as usize
            )));
        }
    }
    Ok(())
}

/// Full path to the installed `hidmaestro.inf` in the DriverStore, if present.
/// Lets the de-risk test bind our devnode against a C#-installed driver without
/// vendoring our own bundle yet.
pub fn installed_inf_path() -> Option<String> {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let repo = format!(r"{root}\System32\DriverStore\FileRepository");
    let entry = std::fs::read_dir(repo).ok()?.flatten().find(|e| {
        e.file_name()
            .to_string_lossy()
            .to_ascii_lowercase()
            .starts_with("hidmaestro.inf_amd64_")
    })?;
    let inf = entry.path().join("hidmaestro.inf");
    inf.exists().then(|| inf.to_string_lossy().into_owned())
}
