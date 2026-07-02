using System.Numerics;

namespace NobdHidMaestro;

/// Pure NOBD sync window — (raw, now_us) -> grouped. No telemetry, no OS deps.
///
/// Direct port of the firmware's syncGpioGetAll() and the Rust
/// shared/src/sync_window.rs (which itself is the verified port of the C++
/// driver/nobd-hid-filter/SyncWindow.h, 16/16 parity tests). The caller passes
/// the current time and window in microseconds, so this is testable in isolation.
///
/// Only RISING EDGES of `synced` bits are delayed; held bits and releases pass
/// through instantly. Driven by a continuous ~1 kHz poll loop, a lone press is
/// released automatically on the tick where the window expires (no injection).
public sealed class SyncWindow
{
    private ushort _committed; // bits the consumer is allowed to see (== debouncedGpio)
    private ushort _syncNew;   // rising edges held inside the open window
    private ulong _startUs;    // when the window opened
    private bool _pending;     // is a window currently open?

    public void Reset()
    {
        _committed = 0;
        _syncNew = 0;
        _startUs = 0;
        _pending = false;
    }

    /// <param name="raw">current raw button bits</param>
    /// <param name="attackMask">which bits count as attacks (>=2 of these =&gt; a chord)</param>
    /// <param name="syncedMask">which bits are subject to the window (attacks only, or all)</param>
    /// <param name="nowUs">monotonic time in microseconds</param>
    /// <param name="windowUs">sync window width in microseconds</param>
    /// <param name="enabled">false =&gt; raw passthrough (live A/B toggle)</param>
    public ushort Process(ushort raw, ushort attackMask, ushort syncedMask,
                          ulong nowUs, uint windowUs, bool enabled)
    {
        if (!enabled)
        {
            _committed = raw;
            _pending = false;
            return raw;
        }

        ushort passthru = (ushort)(raw & ~syncedMask);
        ushort rawS = (ushort)(raw & syncedMask);
        ushort prev = _committed;

        bool haveStart = _pending;
        ulong start = _pending ? _startUs : 0;
        ushort syncNew = _pending ? _syncNew : (ushort)0;

        ushort justPressed = (ushort)(rawS & ~prev & ~syncNew);
        ushort justReleased = (ushort)(prev & ~rawS);

        // Releases are immediate.
        _committed = (ushort)(_committed & ~justReleased);
        // Drop any pending press released before the window closed (bounce filter).
        syncNew = (ushort)(syncNew & rawS);

        if (justPressed != 0)
        {
            if (!haveStart)
            {
                start = nowUs;
                haveStart = true;
                syncNew = justPressed;
            }
            else
            {
                syncNew = (ushort)(syncNew | justPressed);
            }
        }

        if (haveStart)
        {
            ulong held = nowUs - start;
            // Commit on window expiry OR once 2+ attacks are held (deliver-on-grouped).
            bool grouped = BitOperations.PopCount((uint)(ushort)(syncNew & attackMask)) >= 2;
            if (grouped || held >= windowUs)
            {
                _committed = (ushort)(_committed | syncNew);
                syncNew = 0;
                haveStart = false;
            }
        }

        _pending = haveStart;
        if (haveStart)
        {
            _startUs = start;
            _syncNew = syncNew;
        }
        return (ushort)(passthru | _committed);
    }
}
