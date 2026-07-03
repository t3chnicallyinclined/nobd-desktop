namespace NobdHidMaestro;

/// Parity tests ported verbatim from shared/src/sync_window.rs `mod tests`.
/// Run with `--selftest` — no driver, no admin. Proves the C# SyncWindow
/// behaves identically to the Rust/C++/firmware implementations.
internal static class SelfTest
{
    private const ushort LP = 1 << 2;
    private const ushort HP = 1 << 0;
    private const ushort AM = 0x00FF;
    private const uint W = 5000;

    public static int Run()
    {
        int fails = 0;

        // solo_delayed_then_committed
        {
            var w = new SyncWindow();
            fails += Expect("solo: held during window", w.Process(LP, AM, AM, 0, W, true), 0);
            fails += Expect("solo: committed after window", w.Process(LP, AM, AM, W + 1000, W, true), LP);
        }

        // pair_grouped_immediately
        {
            var w = new SyncWindow();
            fails += Expect("pair: lead held", w.Process(LP, AM, AM, 0, W, true), 0);
            fails += Expect("pair: grouped on partner", w.Process((ushort)(LP | HP), AM, AM, 1000, W, true), (ushort)(LP | HP));
        }

        // simultaneous_immediate
        {
            var w = new SyncWindow();
            fails += Expect("simultaneous", w.Process((ushort)(LP | HP), AM, AM, 0, W, true), (ushort)(LP | HP));
        }

        // disabled_passthrough
        {
            var w = new SyncWindow();
            fails += Expect("disabled passthrough", w.Process(LP, AM, AM, 0, W, false), LP);
        }

        Console.WriteLine(fails == 0 ? "\nAll parity tests PASSED (4/4)." : $"\n{fails} assertion(s) FAILED.");
        return fails == 0 ? 0 : 1;
    }

    private static int Expect(string name, ushort got, ushort want)
    {
        bool ok = got == want;
        Console.WriteLine($"  [{(ok ? "PASS" : "FAIL")}] {name}: got 0x{got:X4} want 0x{want:X4}");
        return ok ? 0 : 1;
    }
}
