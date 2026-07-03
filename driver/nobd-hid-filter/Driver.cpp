// NOBD HID Filter (UMDF2) — in-path input-report transform.
//
// SCAFFOLD (v0/v1). Targets the standard UMDF2 HID upper-filter pattern. Build
// with the WDK (see README.md). Compiles against UMDF2 + WDF headers; the
// button-bit extraction and the v2 lone-press timer-injection are marked TODO.
//
// Flow: we attach as a filter; the game's input reads (IOCTL_HID_READ_REPORT)
// pass down and complete back up THROUGH us. We hook the completion, run the
// NOBD sync window on the report's button bits, and write the grouped result
// back before it reaches the game. See DESIGN.md.

#include <windows.h>
#include <wdf.h>
#include <hidsdi.h>
#include "SyncWindow.h"

// ---- Device-specific layout (RETARGET PER STICK from its HID report descriptor) ----
// v1 assumes buttons live in two bytes at a fixed offset in the input report.
// ATTACK_MASK selects which of those bits are "attacks" for grouping.
static const ULONG  BUTTON_BYTE_OFFSET = 1;       // TODO: from report descriptor
static const USHORT ATTACK_MASK        = 0x00FF;  // TODO: which bits are attacks
static const USHORT SYNCED_MASK        = ATTACK_MASK; // attacks only (dirs bypass)
static const UINT32 WINDOW_US          = 5000;    // 5 ms; v3 makes this live-config

// Per-device context: one sync window + a perf-counter time base.
typedef struct _DEVICE_CONTEXT {
    SyncWindow sync;
    LARGE_INTEGER qpcFreq;
    bool          enabled;   // v3: drive from Local\NobdSyncState
} DEVICE_CONTEXT, *PDEVICE_CONTEXT;
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(DEVICE_CONTEXT, DeviceGetContext)

static UINT64 NowUs(PDEVICE_CONTEXT ctx) {
    LARGE_INTEGER c; QueryPerformanceCounter(&c);
    return (UINT64)((c.QuadPart * 1000000ULL) / ctx->qpcFreq.QuadPart);
}

// Completion of a forwarded IOCTL_HID_READ_REPORT: the report bytes are now in
// the request's output buffer. Transform the button bits in place.
static void EvtReadReportComplete(
    WDFREQUEST Request, WDFIOTARGET, PWDF_REQUEST_COMPLETION_PARAMS Params, WDFCONTEXT Context)
{
    PDEVICE_CONTEXT ctx = (PDEVICE_CONTEXT)Context;
    NTSTATUS status = Params->IoStatus.Status;

    if (NT_SUCCESS(status)) {
        WDFMEMORY mem = Params->Parameters.Ioctl.Output.Buffer;
        size_t len = 0;
        PUCHAR rpt = mem ? (PUCHAR)WdfMemoryGetBuffer(mem, &len) : nullptr;

        if (rpt && len >= (size_t)BUTTON_BYTE_OFFSET + 2) {
            // Pull raw buttons (little-endian two bytes at the fixed offset).
            USHORT raw = (USHORT)(rpt[BUTTON_BYTE_OFFSET] | (rpt[BUTTON_BYTE_OFFSET + 1] << 8));
            USHORT out = ctx->sync.process(raw, ATTACK_MASK, SYNCED_MASK,
                                           NowUs(ctx), WINDOW_US, ctx->enabled);
            rpt[BUTTON_BYTE_OFFSET]     = (UCHAR)(out & 0xFF);
            rpt[BUTTON_BYTE_OFFSET + 1] = (UCHAR)(out >> 8);

            // v2 TODO: if ctx->sync.windowOpen(), arm a WDF timer for the
            // remaining window and complete a held read on expiry so a LONE
            // press releases without a new device report (DESIGN.md hold model).
        }
    }
    WdfRequestComplete(Request, status);
}

// Default queue: forward everything; hook only the input-report read.
static void EvtIoDeviceControl(
    WDFQUEUE Queue, WDFREQUEST Request, size_t, size_t, ULONG IoControlCode)
{
    WDFDEVICE dev = WdfIoQueueGetDevice(Queue);
    PDEVICE_CONTEXT ctx = DeviceGetContext(dev);
    WDFIOTARGET target = WdfDeviceGetIoTarget(dev);

    WDF_REQUEST_SEND_OPTIONS opts;
    WDF_REQUEST_SEND_OPTIONS_INIT(&opts, 0);
    WdfRequestFormatRequestUsingCurrentType(Request);

    if (IoControlCode == IOCTL_HID_READ_REPORT) {
        WdfRequestSetCompletionRoutine(Request, EvtReadReportComplete, ctx);
        if (!WdfRequestSend(Request, target, WDF_NO_SEND_OPTIONS)) {
            WdfRequestComplete(Request, WdfRequestGetStatus(Request));
        }
        return;
    }
    // Everything else: forward and forget (true filter passthrough).
    if (!WdfRequestSend(Request, target, &opts)) {
        WdfRequestComplete(Request, WdfRequestGetStatus(Request));
    }
}

static NTSTATUS EvtDeviceAdd(WDFDRIVER, PWDFDEVICE_INIT DeviceInit)
{
    WdfFdoInitSetFilter(DeviceInit);

    WDF_OBJECT_ATTRIBUTES attribs;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attribs, DEVICE_CONTEXT);

    WDFDEVICE device;
    NTSTATUS status = WdfDeviceCreate(&DeviceInit, &attribs, &device);
    if (!NT_SUCCESS(status)) return status;

    PDEVICE_CONTEXT ctx = DeviceGetContext(device);
    ctx->sync.reset();
    ctx->enabled = true;                 // v3: read from shared config
    QueryPerformanceFrequency(&ctx->qpcFreq);

    WDF_IO_QUEUE_CONFIG qcfg;
    WDF_IO_QUEUE_CONFIG_INIT_DEFAULT_QUEUE(&qcfg, WdfIoQueueDispatchParallel);
    qcfg.EvtIoDeviceControl = EvtIoDeviceControl;
    return WdfIoQueueCreate(device, &qcfg, WDF_NO_OBJECT_ATTRIBUTES, nullptr);
}

extern "C" NTSTATUS DriverEntry(PDRIVER_OBJECT DriverObject, PUNICODE_STRING RegistryPath)
{
    WDF_DRIVER_CONFIG config;
    WDF_DRIVER_CONFIG_INIT(&config, EvtDeviceAdd);
    return WdfDriverCreate(DriverObject, RegistryPath, WDF_NO_OBJECT_ATTRIBUTES, &config, nullptr);
}
