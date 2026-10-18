using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using Microsoft.UI.Composition;
using Microsoft.UI.Composition.SystemBackdrops;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Media;

namespace CoffeeFilter;

/// <summary>
/// Makes the whole window see-through, so only the elements drawn on it show. A window's
/// backdrop slot takes any composition brush; a fully transparent one leaves the window clear,
/// and the flyout draws its own acrylic panel with SystemBackdropElement, the way Battery
/// Flyout does. From MagicBuds; same technique as WinUIEx's TransparentTintBackdrop.
/// </summary>
/// <seealso href="https://github.com/dotMorten/WinUIEx/blob/main/src/WinUIEx/TransparentTintBackdrop.cs"/>
public sealed partial class TransparentBackdrop : SystemBackdrop
{
    private Windows.UI.Composition.Compositor? _compositor;

    protected override void OnTargetConnected(ICompositionSupportsSystemBackdrop connectedTarget, XamlRoot xamlRoot)
    {
        base.OnTargetConnected(connectedTarget, xamlRoot);

        // The backdrop slot uses the system compositor, which needs a system dispatcher queue
        Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread().EnsureSystemDispatcherQueue();
        _compositor ??= new Windows.UI.Composition.Compositor();
        connectedTarget.SystemBackdrop = _compositor.CreateColorBrush(Windows.UI.Color.FromArgb(0, 0, 0, 0));

        // Tell DWM to blend the window with what's behind it instead of filling it black:
        // blur-behind with a region that covers nothing turns on alpha without any blur
        var hwnd    = Microsoft.UI.Win32Interop.GetWindowFromWindowId(xamlRoot.ContentIslandEnvironment.AppWindowId);
        var margins = default(PopupWindow.MARGINS);
        _ = PopupWindow.DwmExtendFrameIntoClientArea(hwnd, margins);
        var nowhere    = PopupWindow.CreateRectRgn(-2, -2, -1, -1);
        var blurBehind = new PopupWindow.DWM_BLURBEHIND
        {
            dwFlags  = PopupWindow.DwmBbEnable | PopupWindow.DwmBbBlurRegion,
            fEnable  = 1,
            hRgnBlur = nowhere,
        };
        _ = PopupWindow.DwmEnableBlurBehindWindow(hwnd, blurBehind);
        _ = PopupWindow.DeleteObject(nowhere);

        // No 1px outline around the window, and erase to black (see-through here) instead of
        // the default white, which flashes for a frame when the window first shows
        var noBorder = PopupWindow.DwmwaColorNone;
        _ = PopupWindow.DwmSetWindowAttribute(hwnd, PopupWindow.DwmwaBorderColor, ref noBorder, sizeof(int));
        PopupWindow.Subclass(hwnd, 1);
    }

    protected override void OnTargetDisconnected(ICompositionSupportsSystemBackdrop disconnectedTarget)
    {
        var brush = disconnectedTarget.SystemBackdrop;
        disconnectedTarget.SystemBackdrop = null;
        brush?.Dispose();

        // The flyout's window is closed for good when it's turned off; a new one makes its own
        _compositor?.Dispose();
        _compositor = null;

        base.OnTargetDisconnected(disconnectedTarget);
    }
}

/// <summary>
/// Desktop acrylic that stays see-through when the window isn't focused. The stock
/// DesktopAcrylicBackdrop turns solid whenever its window is inactive; tray flyouts stay
/// acrylic the whole time, including while they slide in before they get focus, so this one
/// always reports the window as active. Theme and high contrast still follow Windows. From MagicBuds.
/// </summary>
/// <seealso href="https://learn.microsoft.com/windows/apps/windows-app-sdk/system-backdrop-controller"/>
public sealed partial class ActiveAcrylicBackdrop : SystemBackdrop
{
    private readonly SystemBackdropConfiguration _configuration = new() { IsInputActive = true };
    private DesktopAcrylicController? _controller;

    protected override void OnTargetConnected(ICompositionSupportsSystemBackdrop connectedTarget, XamlRoot xamlRoot)
    {
        base.OnTargetConnected(connectedTarget, xamlRoot);

        _controller = new DesktopAcrylicController();
        _controller.SetSystemBackdropConfiguration(_configuration);
        _controller.AddSystemBackdropTarget(connectedTarget);
        OnDefaultSystemBackdropConfigurationChanged(connectedTarget, xamlRoot);
    }

    protected override void OnTargetDisconnected(ICompositionSupportsSystemBackdrop disconnectedTarget)
    {
        _controller?.RemoveSystemBackdropTarget(disconnectedTarget);
        _controller?.Dispose();
        _controller = null;

        base.OnTargetDisconnected(disconnectedTarget);
    }

    // Copy theme changes, but never the "inactive" state
    protected override void OnDefaultSystemBackdropConfigurationChanged(ICompositionSupportsSystemBackdrop target, XamlRoot xamlRoot)
    {
        var defaults = GetDefaultSystemBackdropConfiguration(target, xamlRoot);
        _configuration.Theme          = defaults.Theme;
        _configuration.IsHighContrast = defaults.IsHighContrast;
        _configuration.IsInputActive  = true;
    }
}

/// <summary>
/// Screen edge the taskbar sits on.
/// </summary>
public enum TaskbarEdge
{
    Bottom,
    Top,
    Left,
    Right,
}

/// <summary>
/// Frameless popup style and placement for the tray flyout, from MagicBuds (PopupWindowStyle,
/// ScreenGeometry, FlyoutPlacement).
/// </summary>
public static unsafe partial class PopupWindow
{
    public const int WidthDip  = 360;
    public const int MarginDip = 12;

    /// <summary>
    /// The presenter's "no border" still leaves a 3px dialog frame (WS_DLGFRAME) that draws as
    /// a white outline, and WinUI puts it back whenever the window shows. A subclass strips the
    /// frame bits from every style change after WinUI's own handler has run.
    /// </summary>
    public static void Apply(nint hwnd)
    {
        Subclass(hwnd, 2);
        _ = SetWindowLongPtrW(hwnd, GwlStyle, unchecked((nint)(WsPopup | WsClipSiblings | WsSysMenu)));
        _ = SetWindowPos(hwnd, 0, 0, 0, 0, 0, SwpFrameChanged | SwpNoMove | SwpNoSize | SwpNoZOrder | SwpNoActivate);

        // Square corners: the panel inside draws its own
        var corner = DwmwcpDoNotRound;
        _ = DwmSetWindowAttribute(hwnd, DwmwaWindowCornerPreference, ref corner, sizeof(int));
    }

    /// <summary>
    /// Where the flyout goes, in physical pixels: right-aligned (left-aligned for a taskbar on
    /// the left), against the taskbar's edge of the work area, on the monitor under the cursor
    /// (a click) or the main taskbar's (a shortcut). As tall as the work area allows; the panel
    /// inside is only as tall as its content. An auto-hidden taskbar takes no work area, so
    /// it's treated as bottom.
    /// </summary>
    /// <returns>Panel rectangle (left, top, right, bottom), taskbar edge, and DPI divided by 96.</returns>
    public static ((int Left, int Top, int Right, int Bottom) Panel, TaskbarEdge Edge, double Scale) Place(bool atCursor)
    {
        nint monitor;
        if (atCursor && GetCursorPos(out var point))
        {
            monitor = MonitorFromPoint(point, MonitorDefaultToNearest);
        }
        else
        {
            monitor = MonitorFromWindow(FindWindowW("Shell_TrayWnd", null), MonitorDefaultToPrimary);
        }
        var info = new MONITORINFO { cbSize = (uint)sizeof(MONITORINFO) };
        GetMonitorInfoW(monitor, ref info);
        var scale = GetDpiForMonitor(monitor, 0, out var dpi, out _) == 0 ? dpi / 96.0 : 1.0;

        var (screen, work) = (info.rcMonitor, info.rcWork);
        var edge = work.Bottom < screen.Bottom ? TaskbarEdge.Bottom
            : work.Top > screen.Top ? TaskbarEdge.Top
            : work.Left > screen.Left ? TaskbarEdge.Left
            : work.Right < screen.Right ? TaskbarEdge.Right
            : TaskbarEdge.Bottom;

        var margin = (int)Math.Round(MarginDip * scale);
        var width  = Math.Min((int)Math.Round(WidthDip * scale), work.Right - work.Left - (2 * margin));
        var left   = edge == TaskbarEdge.Left ? work.Left + margin : work.Right - margin - width;
        return ((left, work.Top + margin, left + width, work.Bottom - margin), edge, scale);
    }

    /// <summary>
    /// Clip the window to a rectangle (window pixels); the rest doesn't draw and lets clicks through.
    /// </summary>
    public static void Clip(nint hwnd, int left, int top, int right, int bottom) =>
        // The window owns the region after this call
        _ = SetWindowRgn(hwnd, CreateRectRgn(left, top, right, bottom), 1);

    /// <summary>
    /// Give the taskbar focus back, so keys don't go to a closing flyout.
    /// </summary>
    public static void FocusTaskbar() => SetForegroundWindow(FindWindowW("Shell_TrayWnd", null));

    /// <summary>
    /// The top-level window in front. Windows can report an inner input window of a WinUI
    /// window as the foreground one, so this returns the top-level window that owns it.
    /// </summary>
    public static nint Foreground => GetAncestor(GetForegroundWindow(), GaRootOwner);

    /// <summary>
    /// Subclass id 1 erases to black (see-through for TransparentBackdrop); id 2 strips the
    /// frame bits WinUI keeps adding (Apply).
    /// </summary>
    internal static void Subclass(nint hwnd, nuint id) =>
        _ = SetWindowSubclass(hwnd, &SubclassProc, id, 0);

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvStdcall)])]
    private static nint SubclassProc(nint hwnd, uint message, nint wParam, nint lParam, nuint id, nuint data)
    {
        // The window is being destroyed (the flyout was turned off): unhook before it's gone
        if (message == WmNcDestroy)
        {
            _ = RemoveWindowSubclass(hwnd, &SubclassProc, id);
            return DefSubclassProc(hwnd, message, wParam, lParam);
        }
        if (id == 1 && message == WmEraseBkgnd && GetClientRect(hwnd, out var rect))
        {
            _ = FillRect(wParam, rect, GetStockObject(BlackBrush));
            return 1;
        }

        // WinUI's own handler adds the frame back, so it runs first and the frame comes off after
        var result = DefSubclassProc(hwnd, message, wParam, lParam);
        if (id == 2 && message == WmStyleChanging && wParam == GwlStyle)
        {
            ((STYLESTRUCT*)lParam)->styleNew &= ~WsFrameBits;
        }
        return result;
    }

    // =========================================================================
    // WIN32
    // =========================================================================

    private const int GwlStyle                     = -16;
    private const uint WsPopup                     = 0x80000000;
    private const uint WsClipSiblings              = 0x04000000;
    private const uint WsSysMenu                   = 0x00080000;
    private const uint WsFrameBits                 = 0x00800000 | 0x00400000 | 0x00040000;
    private const uint WmStyleChanging             = 0x007C;
    private const uint WmEraseBkgnd                = 0x0014;
    private const uint WmNcDestroy                 = 0x0082;
    private const uint SwpNoSize                   = 0x0001;
    private const uint SwpNoMove                   = 0x0002;
    private const uint SwpNoZOrder                 = 0x0004;
    private const uint SwpNoActivate               = 0x0010;
    private const uint SwpFrameChanged             = 0x0020;
    private const int BlackBrush                   = 4;
    private const int DwmwaWindowCornerPreference  = 33;
    private const int DwmwcpDoNotRound             = 1;
    internal const int DwmwaBorderColor            = 34;
    internal const int DwmwaColorNone              = unchecked((int)0xFFFFFFFE);
    internal const uint DwmBbEnable                = 0x1;
    internal const uint DwmBbBlurRegion            = 0x2;
    private const uint GaRootOwner                 = 3;
    private const uint MonitorDefaultToPrimary     = 1;
    private const uint MonitorDefaultToNearest     = 2;

    [StructLayout(LayoutKind.Sequential)]
    public struct RECT
    {
        public int Left;
        public int Top;
        public int Right;
        public int Bottom;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct POINT
    {
        public int X;
        public int Y;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct MONITORINFO
    {
        public uint cbSize;
        public RECT rcMonitor;
        public RECT rcWork;
        public uint dwFlags;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct STYLESTRUCT
    {
        public uint styleOld;
        public uint styleNew;
    }

    [StructLayout(LayoutKind.Sequential)]
    internal struct MARGINS
    {
        public int Left;
        public int Right;
        public int Top;
        public int Bottom;
    }

    [StructLayout(LayoutKind.Sequential)]
    internal struct DWM_BLURBEHIND
    {
        public uint dwFlags;
        public int fEnable;
        public nint hRgnBlur;
        public int fTransitionOnMaximized;
    }

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool GetCursorPos(out POINT point);

    [LibraryImport("user32.dll")]
    private static partial nint MonitorFromPoint(POINT point, uint flags);

    [LibraryImport("user32.dll")]
    private static partial nint MonitorFromWindow(nint hwnd, uint flags);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool GetMonitorInfoW(nint monitor, ref MONITORINFO info);

    [LibraryImport("shcore.dll")]
    private static partial int GetDpiForMonitor(nint monitor, int type, out uint dpiX, out uint dpiY);

    [LibraryImport("user32.dll", StringMarshalling = StringMarshalling.Utf16)]
    private static partial nint FindWindowW(string className, string? windowName);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool SetForegroundWindow(nint hwnd);

    [LibraryImport("user32.dll")]
    private static partial nint GetForegroundWindow();

    [LibraryImport("user32.dll")]
    private static partial nint GetAncestor(nint hwnd, uint flags);

    [LibraryImport("user32.dll")]
    private static partial nint SetWindowLongPtrW(nint hwnd, int index, nint value);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool SetWindowPos(nint hwnd, nint after, int x, int y, int width, int height, uint flags);

    [LibraryImport("user32.dll")]
    private static partial int SetWindowRgn(nint hwnd, nint region, int redraw);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool GetClientRect(nint hwnd, out RECT rect);

    [LibraryImport("user32.dll")]
    private static partial int FillRect(nint dc, in RECT rect, nint brush);

    [LibraryImport("gdi32.dll")]
    internal static partial nint CreateRectRgn(int left, int top, int right, int bottom);

    [LibraryImport("gdi32.dll")]
    private static partial nint GetStockObject(int index);

    [LibraryImport("gdi32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    internal static partial bool DeleteObject(nint handle);

    [LibraryImport("dwmapi.dll")]
    internal static partial int DwmSetWindowAttribute(nint hwnd, int attribute, ref int value, int size);

    [LibraryImport("dwmapi.dll")]
    internal static partial int DwmExtendFrameIntoClientArea(nint hwnd, in MARGINS margins);

    [LibraryImport("dwmapi.dll")]
    internal static partial int DwmEnableBlurBehindWindow(nint hwnd, in DWM_BLURBEHIND blurBehind);

    [LibraryImport("comctl32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool SetWindowSubclass(nint hwnd, delegate* unmanaged[Stdcall]<nint, uint, nint, nint, nuint, nuint, nint> proc, nuint id, nuint data);

    [LibraryImport("comctl32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool RemoveWindowSubclass(nint hwnd, delegate* unmanaged[Stdcall]<nint, uint, nint, nint, nuint, nuint, nint> proc, nuint id);

    [LibraryImport("comctl32.dll")]
    private static partial nint DefSubclassProc(nint hwnd, uint message, nint wParam, nint lParam);
}
