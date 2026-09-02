# Self-update — what it does and what it needs from you

Modelled on the Retro Receipts agent's updater (`RetroReceipts-agent/agent/src/updater.rs`),
which has been self-applying in the field for months.

## Shape

    startup (+10s) and then hourly
      -> GET https://nobd.net/desktop/latest.json
      -> is the advertised version strictly newer?          (numeric per component)
      -> download nobd.exe AND DINPUT8.dll
      -> verify BOTH minisign signatures
      -> Marvel closed?   yes: apply + restart
                          no:  record it; the UI says "installs when you close Marvel"

## Why both artifacts, together

NOBD ships two files that share `nobd_shared`'s `repr(C)` layout and its magic:

    nobd.exe       the app
    DINPUT8.dll    the in-game hook, copied into the game folder by ensure_installed

They must move together. An app that swapped its own exe and left the old DLL beside it
would push a stale hook into the game on the next install pass — the exact failure v0.7.0
was written to stop ("a stale build had sat in a game folder for two months, compiled
against an older shared-memory layout"). So both are fetched and verified before either is
written, and the DLL is rolled back if the exe swap fails.

The copy already inside the game folder is NOT touched. Updating the one beside the exe is
enough; `gameinstall::ensure_installed` byte-compares and pushes it across on its next pass.

## Why the gate is "Marvel closed"

Mechanical, not cautious. The hook DLL is mapped into the running game, so it cannot be
replaced while the game is open — and shipping a new exe next to a hook the game still has
loaded is precisely the app/DLL version skew that has already produced two bugs here.

## What it needs from you before it does anything

The updater is **inert** until a real signing key is configured (`updater::configured()`
checks for the placeholder). It will not fetch, will not spawn a thread, and logs one line
saying so. That is deliberate: an "armed" updater polling a URL that does not exist yet,
refusing every result, looks like a bug.

To arm it:

1. Generate a keypair OFF this machine. The private half never enters the repo.

       minisign -G -p nobd-desktop.pub -s nobd-desktop.key

2. Paste the `RW...` line from `nobd-desktop.pub` into `MINISIGN_PUBKEY` in
   `app/src/updater.rs`.

3. At release time, sign both artifacts:

       minisign -S -s nobd-desktop.key -m nobd.exe
       minisign -S -s nobd-desktop.key -m DINPUT8.dll

4. Publish the artifacts, their `.minisig` files, and a manifest:

       {
         "version":  "0.7.1",
         "exe_url":  "https://nobd.net/desktop/0.7.1/nobd.exe",
         "dll_url":  "https://nobd.net/desktop/0.7.1/DINPUT8.dll",
         "notes":    "optional"
       }

   Signature URLs default to `<url>.minisig`; override with `exe_sig_url` / `dll_sig_url`.

A wrong or missing key means every update is REFUSED. That is the correct direction to
fail: the alternative is executing an unverified binary.

## Known hazard, inherited

Retro Receipts hit this and it is worth not rediscovering: `self_replace` locates the
running image via `current_exe()`. If the install directory moves out from under a running
process, that path no longer exists and the swap fails with a bare
"The system cannot find the path specified. (os error 3)" — and the install can never
update again. Do not rename or move the install directory from inside the running app.
