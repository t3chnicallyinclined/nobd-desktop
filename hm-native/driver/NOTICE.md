# Vendored HIDMaestro driver

Six files in this folder come from **HIDMaestro** (https://github.com/hifihedgehog/HIDMaestro), which is
**MIT licensed**. Its licence text is in [`LICENSE-HIDMaestro.txt`](LICENSE-HIDMaestro.txt) beside this
file. They are vendored so `hm-native` can install the driver without a WDK build.

| File | What it is | Changed here? |
|---|---|---|
| `HIDMaestro.dll`, `hidmaestro.inf` | The UMDF2 virtual HID driver | No. Byte-identical to the upstream release. |
| `hidmaestro.cat` | Its catalog | Re-signed. |
| `HMXInput.dll`, `hidmaestro_xusb.inf` | The XUSB / XInput companion driver | No. Copied as built by `scripts/package-driver.ps1`. |
| `hidmaestro_xusb.cat` | Its catalog | Re-signed. |

`scripts/package-driver.ps1` copies all six and re-signs the two catalogs with a NOBD-controlled
certificate whose public half is `nobd-driver.cer`. The catalogs' INF and DLL hashes are unchanged, so
they still validate the vendored files.

Pinned HIDMaestro version: **v1.3.17**.

`nobd-driver.pfx` (the private signing key) is **not** committed.

**What installing this does.** `nobd-driver.cer` is a self-signed development certificate. The legacy
virtual-controller setup imports it into the machine's Trusted Root and Trusted Publishers stores so
Windows will load the driver. For a wide release it should be replaced with an EV or attestation-signed
bundle. The default in-game mode installs no driver and no certificate.
