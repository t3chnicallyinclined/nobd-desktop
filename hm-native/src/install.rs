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
    if driver_installed() {
        Ok(())
    } else {
        Err(io::Error::other("pnputil /add-driver did not populate the DriverStore"))
    }
}

/// True if a `hidmaestro.inf` package is present in the DriverStore — the
/// reliable success signal (pnputil rc is unreliable across locales).
pub fn driver_installed() -> bool {
    installed_inf_path().is_some()
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
