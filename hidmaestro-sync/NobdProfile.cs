using HIDMaestro;

namespace NobdHidMaestro;

/// The native "NOBD" virtual fightstick identity.
///
/// A plain-HID / DirectInput device (driverMode intentionally unset — NOT the
/// XInput/XUSB path) so it enumerates natively and can carry a custom "NOBD"
/// name in joy.cpl / Steam / game controller pickers. The joy.cpl label itself
/// comes from the three registry OEM tables, which HMOemNameOverride.Set writes
/// — the USB productString alone does not rename the "Game Controllers" entry.
internal static class NobdProfile
{
    // pid.codes VID (0x1209) — a real, free, community-run vendor ID for
    // open-source hardware. PIDs 0x0001–0x000F are the prototyping range and
    // need no registration.
    //
    // TODO before shipping: register a dedicated NOBD PID via a pid.codes PR
    //   (https://github.com/pidcodes/pidcodes.github.com) and update Pid, then
    //   submit an SDL gamecontrollerdb mapping so Steam/SDL3 label buttons
    //   correctly for VID:PID 1209:<pid>.
    public const ushort Vid = 0x1209;
    public const ushort Pid = 0x0001; // dev/prototyping PID (pid.codes test range)

    public const string Id = "nobd-fightstick";
    public const string Label = "NOBD"; // shown in joy.cpl / Steam / game pickers

    public static HMProfile Build() =>
        new HMProfileBuilder()
            .Id(Id)
            .Name("NOBD Controller")
            .Vendor("NOBD")
            .Vid(Vid).Pid(Pid)
            .ProductString(Label)          // USB product string (SDL3/Chrome read this)
            .ManufacturerString("NOBD")
            .DeviceDescription("NOBD Controller") // Device Manager + default label
            .Type("arcadestick")
            .Connection("usb")
            // NOTE: no .DriverMode(...) — leaving it unset yields a plain HID
            // (DirectInput) device. Setting "xinputhid"/"xusb22", or using an
            // Xbox VID, would spin up the XUSB companion and make it an XInput
            // pad (unbrandable "Xbox 360 Controller"). We want the branded path.
            .FromDescriptorBuilder(new HidDescriptorBuilder()
                .Gamepad()            // Usage 0x05 Game Pad — modern games detect this as a controller
                .AddStick("Left", 8)  // X/Y axis pair — many DInput UIs expect one to render the device
                .AddButtons(14)       // 14 buttons (auto byte-aligned to 16 by the builder)
                .AddHat(8))           // 8-way D-pad / POV — the fightstick's directions
            .Build();
}
