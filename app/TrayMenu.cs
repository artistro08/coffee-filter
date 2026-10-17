using System.Runtime.InteropServices;
using H.NotifyIcon.Core;
using Microsoft.UI;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Media;
using Windows.Foundation;
using Windows.Graphics;

namespace CoffeeFilter;

/// <summary>
/// Tray right-click menu: a real WinUI MenuFlyout (with its open/close animation) shown in a
/// small transparent, always-on-top window next to the cursor.
///
/// Same approach as H.NotifyIcon's SecondWindow mode, with two fixes: the window is placed
/// clear of the taskbar on whichever edge it's docked (the library always opens upward, so
/// a top taskbar covered the menu), and it's sized from the menu as actually rendered at the
/// monitor's scaling (the library measured before first render, which clipped the text).
/// </summary>
public sealed partial class TrayMenu
{
    private readonly Window _window;
    private readonly Grid _root = new() { Background = new SolidColorBrush(Colors.Transparent) };
    private readonly MenuFlyout _flyout = new();
    // Off until the core's first status says otherwise
    private readonly MenuFlyoutItem _blocking = new() { Text = "Ad blocking is off", Icon = new FontIcon { Glyph = "\uE711" } };
    private readonly nint _hwnd;
    private POINT _anchor;
    private SizeInt32 _size = new(320, 200);
    private bool _refit;
    private long _shownAt;
    private bool? _shownRunning;

    /// <summary>
    /// Focus changes this soon after opening are the shell handing focus around, not the user leaving.
    /// </summary>
    private const long FocusGraceMs = 300;

    /// <summary>
    /// Builds the menu: Open, Ad blocking (toggle), Quit. Elements can belong to only one
    /// window, so this window creates its own instead of sharing app resources.
    /// </summary>
    public TrayMenu(Action open, Action toggleBlocking, Action quit)
    {
        var openItem = new MenuFlyoutItem { Text = "Open Coffee Filter", Icon = new FontIcon { Glyph = "\uE8A7" } };
        var quitItem = new MenuFlyoutItem { Text = "Quit", Icon = new FontIcon { Glyph = "\uE7E8" } };
        openItem.Click  += (_, _) => open();
        _blocking.Click += (_, _) => toggleBlocking();
        quitItem.Click  += (_, _) => quit();
        _flyout.Items.Add(openItem);
        _flyout.Items.Add(_blocking);
        _flyout.Items.Add(new MenuFlyoutSeparator());
        _flyout.Items.Add(quitItem);

        _window  = new Window { Content = _root };
        _hwnd    = WinRT.Interop.WindowNative.GetWindowHandle(_window);

        // Borderless, transparent outside the menu, never in Alt+Tab
        WindowUtilities.MakeTransparent(_hwnd);
        _window.AppWindow.IsShownInSwitchers = false;
        if (_window.AppWindow.Presenter is OverlappedPresenter presenter)
        {
            presenter.IsMaximizable = false;
            presenter.IsMinimizable = false;
            presenter.IsResizable   = false;
            presenter.IsAlwaysOnTop = true;
            presenter.SetBorderAndTitleBar(false, false);
        }

        // Bind to Event Listeners
        _flyout.Opened   += (_, _) => MeasureMenu();
        _flyout.Closed   += (_, _) =>
        {
            // Closed only to resize: place at the new size and reopen
            if (_refit)
            {
                // Reopen once the close has finished; ShowAt from inside Closed is ignored
                _refit = false;
                Place();
                _window.DispatcherQueue.TryEnqueue(Open);
                return;
            }
            WindowUtilities.HideWindow(_hwnd);
            App.Instance.SetBackground(true);
        };
        _window.Activated += (_, e) =>
        {
            if (e.WindowActivationState != WindowActivationState.Deactivated)
            {
                return;
            }
            // Opening from the hidden-icons panel: the panel closes and Windows hands focus back
            // to whatever was active before. Take it back instead of closing the menu.
            if (Environment.TickCount64 - _shownAt < FocusGraceMs)
            {
                WindowUtilities.SetForegroundWindow(_hwnd);
                return;
            }
            _flyout.Hide();
        };
    }

    /// <summary>
    /// Shield and "on" when blocking runs, X and "off" when it doesn't; same icon column as the rest.
    /// </summary>
    public void SetBlocking(bool running, bool certInstalled)
    {
        // Called with every status reply; skip the new icon when nothing changed
        _blocking.IsEnabled = certInstalled;
        if (_shownRunning == running)
        {
            return;
        }
        _shownRunning  = running;
        _blocking.Text = running ? "Ad blocking is on" : "Ad blocking is off";
        _blocking.Icon = new FontIcon { Glyph = running ? "\uEA18" : "\uE711" };
    }

    /// <summary>
    /// Open at the cursor. From the hidden-icons panel, wait for the panel to close first,
    /// otherwise it takes focus back the moment the menu opens.
    /// </summary>
    public void Show()
    {
        GetCursorPos(out _anchor);
        if (!IsShellOverflow(GetForegroundWindow()))
        {
            ShowNow();
            return;
        }

        var timer = _window.DispatcherQueue.CreateTimer();
        timer.Interval    = TimeSpan.FromMilliseconds(150);
        timer.IsRepeating = false;
        timer.Tick       += (_, _) => ShowNow();
        timer.Start();
    }

    private void ShowNow()
    {
        App.Instance.SetBackground(false);
        _shownAt = Environment.TickCount64;
        Place();
        // Activate (not just ShowWindow): WinUI only loads a window's content once it's activated
        _window.Activate();
        WindowUtilities.SetForegroundWindow(_hwnd);

        if (_root.IsLoaded)
        {
            Open();
        }
        else
        {
            void OnLoaded(object s, RoutedEventArgs e) { _root.Loaded -= OnLoaded; Open(); }
            _root.Loaded += OnLoaded;
        }
    }

    private void Open() => _flyout.ShowAt(_root, new FlyoutShowOptions
    {
        Position  = new Point(0, 0),
        Placement = FlyoutPlacementMode.BottomEdgeAlignedLeft,
        ShowMode  = FlyoutShowMode.Transient,
    });

    /// <summary>
    /// Learn the menu's real size once it has rendered. Resizing the window closes an open
    /// flyout, so when the size changed (first open, or new text) close it, resize, and
    /// reopen once; after that the window already has the right size.
    /// </summary>
    private void MeasureMenu()
    {
        var presenter = VisualTreeHelper.GetOpenPopupsForXamlRoot(_root.XamlRoot)
            .Select(p => p.Child)
            .OfType<MenuFlyoutPresenter>()
            .FirstOrDefault();
        if (presenter is null)
        {
            return;
        }

        void Check()
        {
            var scale = _root.XamlRoot.RasterizationScale;
            var size  = new SizeInt32((int)Math.Ceiling(presenter.ActualWidth * scale), (int)Math.Ceiling(presenter.ActualHeight * scale));
            if (size.Width <= 0 || size.Height <= 0 || (size.Width == _size.Width && size.Height == _size.Height))
            {
                return;
            }
            _size  = size;
            _refit = true;
            _flyout.Hide();
        }

        if (presenter.ActualWidth > 0)
        {
            Check();
            return;
        }
        void OnSize(object s, SizeChangedEventArgs e) { presenter.SizeChanged -= OnSize; Check(); }
        presenter.SizeChanged += OnSize;
    }

    /// <summary>
    /// Let Windows fit the menu next to the cursor without overlapping the taskbar.
    /// </summary>
    private void Place()
    {
        var taskbar = new APPBARDATA { cbSize = (uint)Marshal.SizeOf<APPBARDATA>() };
        System.Drawing.Rectangle? exclude = SHAppBarMessage(ABM_GETTASKBARPOS, ref taskbar) != 0
            ? System.Drawing.Rectangle.FromLTRB(taskbar.rc.Left, taskbar.rc.Top, taskbar.rc.Right, taskbar.rc.Bottom)
            : null;

        var rect = CursorUtilities.CalculatePopupWindowPosition(_anchor.X, _anchor.Y, _size.Width, _size.Height, exclude);
        _window.AppWindow.MoveAndResize(new RectInt32(rect.X, rect.Y, rect.Width, rect.Height));
    }

    // =========================================================================
    // WIN32
    // =========================================================================

    private const uint ABM_GETTASKBARPOS = 5;

    [StructLayout(LayoutKind.Sequential)]
    private struct POINT
    {
        public int X;
        public int Y;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct RECT
    {
        public int Left;
        public int Top;
        public int Right;
        public int Bottom;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct APPBARDATA
    {
        public uint cbSize;
        public nint hWnd;
        public uint uCallbackMessage;
        public uint uEdge;
        public RECT rc;
        public nint lParam;
    }

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool GetCursorPos(out POINT point);

    [LibraryImport("shell32.dll")]
    private static partial nuint SHAppBarMessage(uint message, ref APPBARDATA data);

    [LibraryImport("user32.dll")]
    private static partial nint GetForegroundWindow();

    [LibraryImport("user32.dll", EntryPoint = "GetClassNameW")]
    private static unsafe partial int GetClassName(nint hwnd, char* name, int max);

    /// <summary>
    /// The Windows 11 hidden-icons panel (or the classic overflow window).
    /// </summary>
    private static unsafe bool IsShellOverflow(nint hwnd)
    {
        var name   = stackalloc char[64];
        var length = GetClassName(hwnd, name, 64);
        var cls    = new string(name, 0, length);
        return cls is "TopLevelWindowForOverflowXamlIsland" or "NotifyIconOverflowWindow";
    }
}
