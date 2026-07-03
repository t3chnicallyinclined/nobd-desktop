# HidHide (vendored installer)

nobd-desktop uses [HidHide](https://github.com/nefarius/HidHide) by Nefarius
Software Solutions to hide the physical stick from games while the app still
reads it — so only the virtual **NOBD Controller** shows up in Steam.

The installer binary is **not committed** (8 MB, signed third-party binary). Fetch
the official signed release into this folder:

```
gh release download v1.5.230.0 --repo nefarius/HidHide \
  --pattern "HidHide_1.5.230_x64.exe" --dir tools/hidhide
```

- Version pinned: **v1.5.230** (`HidHide_1.5.230_x64.exe`)
- SHA256: `F4BBBCB82E6258641B887C74BC81C4C5F66E4AA811808DFC304347687B7605F6`
- Authenticode signer: **Nefarius Software Solutions e.U.** (EV)
- HidHide license: MIT

The app runs this installer on request (Sync tab → "Enable device hiding"). It's a
boot-start filter driver, so installation requires a one-time reboot. After that,
nobd-desktop drives it via `HidHideCLI.exe` (whitelist nobd.exe, hide the stick,
cloak on/off).
