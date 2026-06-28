# Driver signing — future option (NOBD-branded pad / zero-latency HID filter)

The shipping product (universal sync via **ViGEmBus**) needs **no driver signing from
us** — ViGEmBus is already production-signed by Nefarius (Secure-Boot-compatible,
one-click user install, like DS4Windows). This doc captures what it would take to
ship our *own* driver, for when/if we want it.

## When we'd need our own driver
- A **custom "NOBD"-branded** virtual pad (fork ViGEmBus with a NOBD VID/PID +
  product strings, or our own virtual HID gamepad driver).
- The **zero-latency UMDF/KMDF HID filter** (in-path grouping, no virtual-pad hop).

## What it costs (confirmed, 2026)
| Item | Cost |
|---|---|
| EV code-signing certificate (Sectigo EV via reseller — Certera/SignMyCode/CheapSSLWEB) | **$279.99 / year** (USB token included on the 1-yr option) |
| Microsoft Hardware Developer Program (Partner Center) registration | **$0** (free; validated with the EV cert) |
| Attestation signing per driver | **$0** (no HLK lab needed for a virtual gamepad / HID filter) |
| **Total** | **≈ $280 / year** |

- **$279.99 is the floor for EV.** Cheaper certs are OV/standard and **cannot sign
  drivers** — EV is mandatory for kernel-mode AND user-mode (UMDF) drivers.
- **Azure Trusted Signing (~$10/mo) does NOT work for drivers** — apps only; still
  needs an EV cert for the Hardware Dev Center.
- Multi-year (3-yr) "install on existing HSM" + a ~$50 YubiKey FIPS drops it to
  ~$235–250/yr — marginal savings, more hassle.

## The hardware token
EV keys must live on certified hardware. Default: the CA **mails a physical USB
token** (e.g. SafeNet eToken) to the LLC's registered address; plug it in to sign.
Alternative: store the key in a **cloud HSM** (Azure Key Vault / Google Cloud KMS)
or use a cloud signing service — no dongle, slightly more setup, good for CI.

## Eligibility (we have an LLC — we qualify)
- EV has **no 3-year-org rule** (that's Azure Trusted Signing's). A verifiable
  registered business qualifies; a **D-U-N-S number** (free) speeds CA validation.
- A **sole-proprietor EV** tier even covers kernel drivers for individuals without
  a business — so the LLC is well clear.

## The flow
1. Get the EV cert (LLC validation + D-U-N-S + verification call).
2. Register the **Hardware Developer Program** in Partner Center (free), validated
   with the EV cert.
3. Build the driver (WDK). Test on a dev box via test-signing
   (`bcdedit /set testsigning on`, Secure Boot off) before paying for anything.
4. Sign the `.cab` with the EV cert → submit to Partner Center → **attestation
   signing** → distribute. Loads with Secure Boot ON, zero user steps.

## The honest caveat
A custom-VID "NOBD" device **cannot be XInput** (XInput is hardwired to Microsoft's
Xbox VID/PID). So a NOBD-branded pad is **HID/DInput** — works in Steam, emulators,
DInput games, but not raw-XInput-only games. Same compatibility class as the DS4
toggle, just branded. The stronger reason to buy the EV cert is the **zero-latency
HID filter** (a real product win), with branding as a bonus.

Sources: Microsoft Learn (driver code-signing reqs, Hardware Program registration),
Certera/CheapSSLWEB (EV pricing + token), ViGEmBus driver site (already signed).
