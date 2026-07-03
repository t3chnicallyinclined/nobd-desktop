//! Stage 3 — driver install (ELEVATED, one-time). We ship a PRE-SIGNED vendored
//! bundle (`hidmaestro.inf` + `.cat` + `HIDMaestro.dll`, signed at our build
//! time with a cert we control), so runtime install is just: trust the cert,
//! then `pnputil /add-driver /install`. No signtool/inf2cat at runtime.

use std::io;
use std::process::Command;

fn run(exe: &str, args: &[&str]) -> io::Result<()> {
    let status = Command::new(exe).args(args).status()?;
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
        .status()?;
    let inf_name = std::path::Path::new(inf_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("hidmaestro.inf");
    if package_installed(inf_name) {
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

/// Remove every installed `hidmaestro.inf` driver package (best-effort). Used
/// to get a clean slate so we validate OUR vendored bundle, not a prior copy.
/// Parses `pnputil /enum-drivers` for the locale-stable `oemNN.inf` + original
/// `hidmaestro.inf` tokens. Returns how many packages were deleted.
pub fn uninstall_hidmaestro() -> u32 {
    let output = match Command::new("pnputil").arg("/enum-drivers").output() {
        Ok(o) => o,
        Err(_) => return 0,
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut removed = 0;
    for block in text.split("\r\n\r\n") {
        if !block.to_ascii_lowercase().contains("hidmaestro.inf") {
            continue;
        }
        let oem = block.split_whitespace().find(|t| {
            let t = t.to_ascii_lowercase();
            t.starts_with("oem") && t.ends_with(".inf")
        });
        if let Some(oem) = oem {
            let _ = Command::new("pnputil")
                .args(["/delete-driver", oem, "/uninstall", "/force"])
                .status();
            removed += 1;
        }
    }
    removed
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
