using System.Runtime.InteropServices;

namespace CoffeeFilter;

/// <summary>
/// Win32 calls shared across the app: windows and focus, monitors and scaling, and the app's
/// own process. Calls that belong to one feature stay next to it (PopupWindow for the flyout's
/// window style, Shortcuts and KeyCapture for the keyboard, TrayMenu for the taskbar position).
/// </summary>
/// <seealso href="https://learn.microsoft.com/windows/win32/api/winuser/"/>
internal static unsafe partial class Native
{
    [StructLayout(LayoutKind.Sequential)]
    public struct POINT
    {
        public int X;
        public int Y;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct RECT
    {
        public int Left;
        public int Top;
        public int Right;
        public int Bottom;
    }

    // =========================================================================
    // WINDOWS AND FOCUS
    // =========================================================================

    [LibraryImport("user32.dll")]
    public static partial nint GetForegroundWindow();

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool SetForegroundWindow(nint hwnd);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool BringWindowToTop(nint hwnd);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool AllowSetForegroundWindow(uint processId);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool IsHungAppWindow(nint hwnd);

    [LibraryImport("user32.dll")]
    public static partial uint GetWindowThreadProcessId(nint hwnd, nint processId);

    [LibraryImport("kernel32.dll")]
    public static partial uint GetCurrentThreadId();

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool AttachThreadInput(uint attach, uint attachTo, [MarshalAs(UnmanagedType.Bool)] bool join);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool GetCursorPos(out POINT point);

    [LibraryImport("user32.dll")]
    public static partial int GetSystemMetrics(int index);

    [LibraryImport("user32.dll", EntryPoint = "GetClassNameW")]
    private static partial int GetClassName(nint hwnd, char* name, int max);

    /// <summary>
    /// The Windows 11 hidden-icons panel (or the classic overflow window).
    /// </summary>
    public static bool IsShellOverflow(nint hwnd)
    {
        var name   = stackalloc char[64];
        var length = GetClassName(hwnd, name, 64);
        var cls    = new string(name, 0, length);
        return cls is "TopLevelWindowForOverflowXamlIsland" or "NotifyIconOverflowWindow";
    }

    // =========================================================================
    // MONITORS
    // =========================================================================

    public const uint MonitorDefaultToPrimary = 1;
    public const uint MonitorDefaultToNearest = 2;

    [LibraryImport("user32.dll")]
    public static partial nint MonitorFromWindow(nint hwnd, uint flags);

    [LibraryImport("user32.dll")]
    public static partial nint MonitorFromPoint(POINT point, uint flags);

    [LibraryImport("shcore.dll")]
    private static partial int GetDpiForMonitor(nint monitor, int type, out uint dpiX, out uint dpiY);

    /// <summary>
    /// A monitor's display scaling (1.25 at 125%), or 1 when Windows can't say.
    /// </summary>
    public static double MonitorScale(nint monitor)
    {
        const int MDT_EFFECTIVE_DPI = 0;
        return GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, out var dpi, out _) == 0 ? dpi / 96.0 : 1.0;
    }

    // =========================================================================
    // THIS PROCESS
    // =========================================================================

    public const int  ProcessPowerThrottling        = 4;
    public const uint PowerThrottlingExecutionSpeed = 0x1;
    public const uint NormalPriorityClass           = 0x20;
    public const uint BelowNormalPriorityClass      = 0x4000;

    [StructLayout(LayoutKind.Sequential)]
    public struct PowerThrottlingState
    {
        public uint Version;
        public uint ControlMask;
        public uint StateMask;
    }

    [LibraryImport("kernel32.dll")]
    public static partial nint GetCurrentProcess();

    [LibraryImport("kernel32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool SetProcessInformation(nint process, int infoClass, void* info, uint size);

    [LibraryImport("kernel32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool SetPriorityClass(nint process, uint priorityClass);

    [LibraryImport("kernel32.dll", StringMarshalling = StringMarshalling.Utf16)]
    public static partial nint GetModuleHandleW(string? name);

    [LibraryImport("psapi.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool EmptyWorkingSet(nint process);
}
