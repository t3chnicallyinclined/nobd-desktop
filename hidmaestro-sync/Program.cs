// NOBD universal sync — HIDMaestro SPIKE.
//
// Same idea as vigem-sync, but the virtual pad is a HIDMaestro UMDF2 user-mode
// controller instead of a ViGEmBus kernel virtual pad. Read the real controller
// (XInput), run the NOBD sync window on its attack buttons, and present the
// RESULT as a HIDMaestro "xbox-360-wired" pad. Every game and the Finger Gap
// Tester read the virtual pad, so the grouping is universal.
//
// Why the spike: HIDMaestro is user-mode (no BSOD risk), needs no purchased
// cert and no test-signing mode, and gives full VID/PID/descriptor control.
// The open question is added latency vs ViGEm's ~1 ms virtual-pad floor — this
// program's `--latency` mode measures it with the SAME methodology as
// `vigem-sync --latency`, so the comparison is apples-to-apples on one machine.
//
// Modes:
//   (default)            read real pad -> sync -> HIDMaestro pad (the real thing)
//   --latency [iters]    SubmitState -> XInputGetState round-trip latency (µs)
//   --selftest           run the SyncWindow parity tests (no driver, no admin)
//   --window <ms>        fallback window when no GUI is driving shared memory
//   --profile <id>       HIDMaestro profile id (default xbox-360-wired)
//
// Live config comes from the existing GUI via the shared `Local\NobdSyncState`
// block (enabled + per-player window), exactly like vigem-sync. When no GUI is
// running (magic absent) it falls back to --window and enabled=true.
//
// InstallDriver()/CreateController() need admin — app.manifest requests it.

using System.Diagnostics;
using System.IO.MemoryMappedFiles;
using System.Runtime.InteropServices;
using System.Security.Principal;
using System.Threading;
using HIDMaestro;

namespace NobdHidMaestro;

internal static class Program
{
    // ── XInput read surface (the REAL controller) ────────────────────────────
    [StructLayout(LayoutKind.Sequential)]
    private struct XINPUT_GAMEPAD
    {
        public ushort wButtons;
        public byte bLeftTrigger, bRightTrigger;
        public short sThumbLX, sThumbLY, sThumbRX, sThumbRY;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct XINPUT_STATE
    {
        public uint dwPacketNumber;
        public XINPUT_GAMEPAD Gamepad;
    }

    [DllImport("xinput1_4.dll", EntryPoint = "XInputGetState")]
    private static extern uint XInputGetState(uint dwUserIndex, out XINPUT_STATE pState);

    [DllImport("winmm.dll")] private static extern uint timeBeginPeriod(uint uPeriod);

    // XInput wButtons bits.
    private const ushort DPAD_UP = 0x0001, DPAD_DOWN = 0x0002, DPAD_LEFT = 0x0004, DPAD_RIGHT = 0x0008;
    private const ushort BTN_START = 0x0010, BTN_BACK = 0x0020, LTHUMB = 0x0040, RTHUMB = 0x0080;
    private const ushort LB = 0x0100, RB = 0x0200, GUIDE = 0x0400;
    private const ushort A = 0x1000, B = 0x2000, X = 0x4000, Y = 0x8000;

    // Attack bits: A, B, X, Y, LB, RB. DPad/Start/Back/thumbs pass through
    // ungrouped (zero motion-input lag) — matches vigem-sync's ATTACK_MASK.
    private const ushort ATTACK_MASK = 0xF300;

    // Shared-memory NOBD state (created by the GUI / vigem-sync). repr(C):
    //   u32 magic @0 | u32 enabled @4 | u32 window_ms[0] @8 | window_ms[1] @12 | ...
    private const string SHM_NAME = "Local\\NobdSyncState";
    private const uint SHM_MAGIC = 0x4E424433; // "NBD3"
    private const long SHM_SIZE = 4096;

    private static volatile bool _running = true;

    private static int Main(string[] args)
    {
        if (args.Contains("--selftest")) return SelfTest.Run();

        string? profileArg = ArgValue(args, "--profile");
        uint fallbackWindow = uint.TryParse(ArgValue(args, "--window"), out var w) ? Math.Clamp(w, 1, 16) : 5;

        if (!IsElevated())
            Console.WriteLine("WARNING: not elevated. InstallDriver()/CreateController()/OEM-name branding " +
                              "need admin — run this from an elevated terminal or expect it to fail.");

        if (args.Contains("--latency"))
        {
            // The latency harness reads via XInputGetState, so it only measures
            // the XInput/XUSB path. Default to xbox-360-wired for the A/B vs ViGEm.
            int iters = 400;
            foreach (var a in args) if (int.TryParse(a, out var n) && n >= 100) iters = n;
            return Latency(profileArg ?? "xbox-360-wired", iters);
        }

        // Default: the native branded "NOBD" HID stick.
        return RunSync(profileArg ?? "nobd", fallbackWindow);
    }

    // ── The real thing: real pad -> sync -> HIDMaestro pad ───────────────────
    private static int RunSync(string profileId, uint fallbackWindow)
    {
        timeBeginPeriod(1);
        Console.WriteLine("NOBD universal sync (HIDMaestro spike) — starting");

        // Capture the REAL pad's XInput slot BEFORE creating the virtual one, so
        // we never read our own output back (feedback loop).
        int realSlot = -1;
        for (int waits = 0; realSlot < 0; waits++)
        {
            for (uint s = 0; s < 4; s++)
                if (XInputGetState(s, out _) == 0) { realSlot = (int)s; break; }
            if (realSlot < 0)
            {
                if (waits % 5 == 0) Console.WriteLine("Waiting for a controller… (plug it in / set it to XInput mode)");
                Thread.Sleep(1000);
            }
        }
        Console.WriteLine($"Real controller found on XInput slot {realSlot}.");

        using var ctx = new HMContext();
        // Restore any joy.cpl OEM-name overrides stranded by a prior crash.
        int recovered = HMOemNameOverride.RecoverOrphans();
        if (recovered > 0) Console.WriteLine($"Recovered {recovered} orphaned OEM-name override(s).");
        int loaded = ctx.LoadDefaultProfiles();
        Console.WriteLine($"Loaded {loaded} HIDMaestro profiles.");
        Console.Write("Installing HIDMaestro driver (idempotent)… ");
        ctx.InstallDriver();
        Console.WriteLine("OK");

        // Resolve the profile: "nobd" = our native branded HID stick; anything
        // else = a catalog profile (e.g. xbox-360-wired) for comparison.
        bool nobd = profileId.Equals("nobd", StringComparison.OrdinalIgnoreCase);
        HMProfile? profile = nobd ? NobdProfile.Build() : ctx.GetProfile(profileId);
        if (profile == null) { Console.Error.WriteLine($"Profile '{profileId}' not found."); return 1; }
        Console.Write($"Creating virtual controller ({profile.Name})… ");
        using var ctrl = ctx.CreateController(profile);
        Console.WriteLine("OK");

        // Brand it: force the joy.cpl / DirectInput label to "NOBD" across all
        // three OEM registry tables (the USB product string alone won't rename
        // the "Game Controllers" entry). Paired with Clear() on exit below.
        bool branded = false;
        if (nobd)
        {
            try
            {
                HMOemNameOverride.Set(profile.VendorId, profile.ProductId, NobdProfile.Label);
                branded = true;
                Console.WriteLine($"Branded VID_{profile.VendorId:X4}&PID_{profile.ProductId:X4} as \"{NobdProfile.Label}\" in joy.cpl.");
            }
            catch (Exception e)
            {
                Console.WriteLine($"(OEM-name branding skipped: {e.Message} — need admin)");
            }
        }

        // Clean up the brand on Ctrl+C so we don't strand the override.
        Console.CancelKeyPress += (_, e) => { e.Cancel = true; _running = false; };
        Console.WriteLine("Reading real pad -> syncing -> NOBD pad. Ctrl+C to stop.");

        var shm = OpenSharedState();
        var sync = new SyncWindow();
        var epoch = Stopwatch.StartNew();
        bool lastEnabled = false;
        ushort lastRaw = 0;
        var lastLog = Stopwatch.StartNew();
        var axes = HMGamepadStateHelpers.StandardAxes(profile); // reused; only Buttons/axes values change

        while (_running)
        {
            ulong nowUs = (ulong)(epoch.Elapsed.Ticks * 1_000_000L / Stopwatch.Frequency);

            (bool enabled, uint windowMs) = ReadConfig(shm, fallbackWindow);
            uint windowUs = windowMs * 1000;

            if (enabled != lastEnabled)
            {
                Console.WriteLine($"sync {(enabled ? "ON" : "OFF")} (window {windowMs}ms)");
                lastEnabled = enabled;
            }

            if (XInputGetState((uint)realSlot, out var state) == 0)
            {
                var gp = state.Gamepad;
                ushort raw = gp.wButtons;
                ushort grouped = sync.Process(raw, ATTACK_MASK, ATTACK_MASK, nowUs, windowUs, enabled);

                if (raw != lastRaw)
                {
                    Console.WriteLine($"in 0x{raw:X4} -> out 0x{grouped:X4}  (sync {(enabled ? "ON" : "off")})");
                    lastRaw = raw;
                }

                // Directions + sticks + triggers pass through live (never windowed).
                axes[profile.Sticks[0].XAxis] = Ax(gp.sThumbLX);
                if (profile.Sticks[0].YAxis != HMAxis.None) axes[profile.Sticks[0].YAxis] = Ax(gp.sThumbLY);
                if (profile.Sticks.Count > 1)
                {
                    axes[profile.Sticks[1].XAxis] = Ax(gp.sThumbRX);
                    if (profile.Sticks[1].YAxis != HMAxis.None) axes[profile.Sticks[1].YAxis] = Ax(gp.sThumbRY);
                }
                if (profile.Triggers.Count > 0) axes[profile.Triggers[0].Axis] = Tr(gp.bLeftTrigger);
                if (profile.Triggers.Count > 1) axes[profile.Triggers[1].Axis] = Tr(gp.bRightTrigger);

                var outState = new HMGamepadState
                {
                    Axes = axes,
                    Buttons = MapButtons(grouped),
                    Hat = MapHat(grouped),
                };
                ctrl.SubmitState(in outState);
            }
            else if (lastLog.ElapsedMilliseconds >= 3000)
            {
                Console.WriteLine($"(real pad on slot {realSlot} not reporting — still XInput?)");
                lastLog.Restart();
            }

            Thread.Sleep(1); // ~1 kHz
        }

        if (branded)
        {
            HMOemNameOverride.Clear(profile.VendorId, profile.ProductId);
            Console.WriteLine("Restored the prior joy.cpl label.");
        }
        Console.WriteLine("Stopped.");
        return 0;
    }

    // ── Latency: SubmitState -> XInputGetState round trip (matches vigem) ─────
    private static int Latency(string profileId, int iters)
    {
        timeBeginPeriod(1);
        try { Process.GetCurrentProcess().PriorityClass = ProcessPriorityClass.High; } catch { }
        Console.WriteLine("=== HIDMaestro round-trip latency (SubmitState -> XInputGetState) ===");

        var before = new List<uint>();
        for (uint s = 0; s < 4; s++) if (XInputGetState(s, out _) == 0) before.Add(s);

        using var ctx = new HMContext();
        ctx.LoadDefaultProfiles();
        Console.Write("Installing driver… "); ctx.InstallDriver(); Console.WriteLine("OK");
        var profile = ctx.GetProfile(profileId);
        if (profile == null) { Console.Error.WriteLine($"Profile '{profileId}' not found."); return 1; }
        using var ctrl = ctx.CreateController(profile);

        var axes = HMGamepadStateHelpers.StandardAxes(profile);
        int vslot = -1;
        var find = Stopwatch.StartNew();
        while (find.ElapsedMilliseconds < 6000 && vslot < 0)
        {
            ctrl.SubmitState(new HMGamepadState { Axes = axes, Buttons = HMButton.None });
            for (uint s = 0; s < 4; s++)
                if (!before.Contains(s) && XInputGetState(s, out _) == 0) { vslot = (int)s; break; }
            if (vslot < 0) Thread.Sleep(50);
        }
        if (vslot < 0) { Console.Error.WriteLine("couldn't locate the virtual pad's XInput slot"); return 1; }
        Console.WriteLine($"virtual pad on slot {vslot}; sampling…");

        double freq = Stopwatch.Frequency;
        var samples = new List<double>();
        for (int i = 0; i < iters; i++)
        {
            bool want = i % 2 == 0;
            ctrl.SubmitState(new HMGamepadState { Axes = axes, Buttons = want ? HMButton.A : HMButton.None });
            long t0 = Stopwatch.GetTimestamp();
            bool hit = false;
            long deadline = t0 + (long)(freq * 0.1);
            while (Stopwatch.GetTimestamp() < deadline)
            {
                XInputGetState((uint)vslot, out var st);
                if (((st.Gamepad.wButtons & A) != 0) == want) { hit = true; break; }
            }
            if (hit && i >= 20) samples.Add((Stopwatch.GetTimestamp() - t0) / freq * 1000.0);
            Thread.Sleep(3);
        }

        if (samples.Count == 0) { Console.Error.WriteLine("no samples captured"); return 1; }
        samples.Sort();
        int n = samples.Count;
        double avg = samples.Sum() / n;
        double Pct(double p) => samples[(int)Math.Round((n - 1) * p)];
        Console.WriteLine($"n={n}  min={samples[0]:F2}ms  median={Pct(0.5):F2}ms  avg={avg:F2}ms  p95={Pct(0.95):F2}ms  max={samples[n - 1]:F2}ms");
        Console.WriteLine("(compare to: vigem-sync --latency on the same machine)");
        return 0;
    }

    // ── Mapping helpers ──────────────────────────────────────────────────────
    private static HMButton MapButtons(ushort b)
    {
        HMButton r = HMButton.None;
        if ((b & A) != 0) r |= HMButton.A;
        if ((b & B) != 0) r |= HMButton.B;
        if ((b & X) != 0) r |= HMButton.X;
        if ((b & Y) != 0) r |= HMButton.Y;
        if ((b & LB) != 0) r |= HMButton.LeftBumper;
        if ((b & RB) != 0) r |= HMButton.RightBumper;
        if ((b & BTN_BACK) != 0) r |= HMButton.Back;
        if ((b & BTN_START) != 0) r |= HMButton.Start;
        if ((b & LTHUMB) != 0) r |= HMButton.LeftStick;
        if ((b & RTHUMB) != 0) r |= HMButton.RightStick;
        if ((b & GUIDE) != 0) r |= HMButton.Guide;
        return r;
    }

    private static HMHat MapHat(ushort b)
    {
        bool u = (b & DPAD_UP) != 0, d = (b & DPAD_DOWN) != 0, l = (b & DPAD_LEFT) != 0, r = (b & DPAD_RIGHT) != 0;
        if (u && r) return HMHat.NorthEast;
        if (u && l) return HMHat.NorthWest;
        if (d && r) return HMHat.SouthEast;
        if (d && l) return HMHat.SouthWest;
        if (u) return HMHat.North;
        if (r) return HMHat.East;
        if (d) return HMHat.South;
        if (l) return HMHat.West;
        return HMHat.None;
    }

    // XInput stick short (-32768..32767) -> [0..1]; trigger byte -> [0..1].
    private static float Ax(short v) => (v + 32768) / 65535f;
    private static float Tr(byte v) => v / 255f;

    // ── Shared memory ────────────────────────────────────────────────────────
    private static MemoryMappedViewAccessor? OpenSharedState()
    {
        try
        {
            var mmf = MemoryMappedFile.CreateOrOpen(SHM_NAME, SHM_SIZE, MemoryMappedFileAccess.ReadWrite);
            return mmf.CreateViewAccessor(0, SHM_SIZE, MemoryMappedFileAccess.ReadWrite);
        }
        catch (Exception e)
        {
            Console.WriteLine($"(shared state unavailable: {e.Message}; using CLI/default window)");
            return null;
        }
    }

    private static (bool enabled, uint windowMs) ReadConfig(MemoryMappedViewAccessor? shm, uint fallbackWindow)
    {
        if (shm == null) return (true, fallbackWindow);
        uint magic = shm.ReadUInt32(0);
        if (magic != SHM_MAGIC) return (true, fallbackWindow); // no GUI driving it
        bool enabled = shm.ReadUInt32(4) != 0;
        uint window = shm.ReadUInt32(8);
        if (window < 1 || window > 16) window = fallbackWindow;
        return (enabled, window);
    }

    private static string? ArgValue(string[] args, string flag)
    {
        int i = Array.IndexOf(args, flag);
        return (i >= 0 && i + 1 < args.Length) ? args[i + 1] : null;
    }

    private static bool IsElevated()
    {
        try
        {
            using var id = WindowsIdentity.GetCurrent();
            return new WindowsPrincipal(id).IsInRole(WindowsBuiltInRole.Administrator);
        }
        catch { return false; }
    }
}
