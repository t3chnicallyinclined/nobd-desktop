# Vendored HIDMaestro driver

`hidmaestro.inf`, `HIDMaestro.dll`, and `hidmaestro.cat` are the prebuilt UMDF2
driver from **HIDMaestro** (https://github.com/hifihedgehog/HIDMaestro), **MIT
licensed**, vendored here unmodified so `hm-native` can install it without a WDK
build. `HIDMaestro.dll` and `hidmaestro.inf` are byte-identical to the upstream
release; only `hidmaestro.cat` is re-signed (via `scripts/package-driver.ps1`)
with a NOBD-controlled cert whose public half is `nobd-driver.cer`. The catalog's
INF+DLL hashes are unchanged, so it still validates the vendored files.

Pinned HIDMaestro version: **v1.3.17**.

`nobd-driver.pfx` (the private signing key) is **not** committed. For a public
release this self-signed dev cert should be replaced with an EV / attestation
signed bundle.
