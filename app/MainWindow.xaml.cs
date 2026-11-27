using System.Diagnostics;
using System.Text.Json.Nodes;
using CommunityToolkit.WinUI.Controls;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;

namespace CoffeeFilter;

/// <summary>
/// Settings-style window. Holds no state of its own beyond what's on screen; it renders
/// App.Last and sends commands through App, so it can be destroyed on close and recreated.
///
/// Split by section: this file has the window itself and what every section shares;
/// MainWindow.Pages.cs the sub-pages (Ad blocking's live log, Connection problems),
/// MainWindow.Exclusions.cs excluded sites and apps, MainWindow.Lists.cs filter lists and
/// userscripts, and MainWindow.Shortcuts.cs the shortcut pickers.
/// </summary>
public sealed partial class MainWindow : Window
{
    private readonly App _app = App.Instance;
    private readonly DispatcherTimer _timer = new() { Interval = TimeSpan.FromSeconds(2) };
    private bool _updating;

    /// <summary>
    /// The window is in front, so the 2-second status check runs; it stops in the background.
    /// </summary>
    private bool _active;

    public MainWindow()
    {
        InitializeComponent();
        ExtendsContentIntoTitleBar = true;
        SetTitleBar(AppTitleBar);
        AppWindow.SetIcon(Path.Combine(AppContext.BaseDirectory, "Assets", "coffee.ico"));

        // The size you last left it at, else 640 x 900. Sizes are saved scale-independent and
        // applied in physical pixels for this display's scaling, kept on screen.
        var scale  = Scale();
        var work   = DisplayArea.GetFromWindowId(AppWindow.Id, DisplayAreaFallback.Primary).WorkArea;
        var width  = _app.Settings.WindowWidth ?? 640;
        var height = _app.Settings.WindowHeight ?? 900;
        AppWindow.Resize(new Windows.Graphics.SizeInt32(Math.Min((int)(width * scale), work.Width), Math.Min((int)(height * scale), work.Height)));

        // Minimum size keeps every card on one row: its text, then its switch or button
        if (AppWindow.Presenter is OverlappedPresenter presenter)
        {
            presenter.PreferredMinimumWidth  = (int)(592 * scale);
            presenter.PreferredMinimumHeight = (int)(520 * scale);
        }
        AnimatePage(MainContent, from: -48);
        AnimatePage(LogView, from: 48);
        AnimatePage(ProblemsView, from: 48);
        ShowMain();

        // Cards keep one row at every width
        foreach (var expander in new[] { UserscriptsCard, ExclusionsCard, ExcludedAppsCard, ShortcutsCard, AboutCard })
        {
            OneRow(expander);
        }

        TraySwitch.IsOn       = _app.Settings.ShowTrayIcon;
        FlyoutSwitch.IsOn     = _app.Settings.UseFlyout;
        LaunchSwitch.IsOn     = _app.Settings.ShowWindowAtLaunch;
        AboutCard.Description = $"Version {App.Version}";
        BuildShortcuts();
        _ = ShowStartupAsync(Startup.GetAsync());

        // Bind to Event Listeners
        _app.Changed   += OnChanged;
        _app.Failed    += ShowError;
        _timer.Tick    += async (_, _) => await PollStatusAsync();
        _logTimer.Tick += async (_, _) => await PollLogAsync();
        // Only poll while the window is in focus, so it costs nothing in the background
        Activated      += (_, e) =>
        {
            _active    = e.WindowActivationState != WindowActivationState.Deactivated;
            var polled = _active && _app.Last is not null;
            if (polled) { _timer.Start(); } else { _timer.Stop(); }
            if (polled && LogView.Visibility == Visibility.Visible) { _logTimer.Start(); } else { _logTimer.Stop(); }
        };
        Closed         += (_, _) =>
        {
            _timer.Stop();
            _logTimer.Stop();
            _app.Changed -= OnChanged;
            _app.Failed  -= ShowError;
            SaveSize();
            // Stop the core recording blocked requests nobody is watching
            if (LogView.Visibility == Visibility.Visible)
            {
                _ = _app.RunAsync("log_stop", showErrors: false);
            }
        };

        if (_app.Last is { } last)
        {
            OnChanged(last, lists: true);
        }
    }

    private void OnChanged(CoreReply reply, bool lists)
    {
        if (reply.Status is not { } s)
        {
            return;
        }
        // First reply: swap the loading message for the page (only while on the main page)
        if (Loading.Visibility == Visibility.Visible)
        {
            LoadingRing.IsActive = false;
            Loading.Visibility   = Visibility.Collapsed;
            if (_subPage is null)
            {
                MainContent.Visibility = Visibility.Visible;
                OneRowWhenShown(MainContent);
            }
            if (_active)
            {
                _timer.Start();
            }
        }

        _updating                = true;
        BlockingSwitch.IsOn      = s.Running;
        BlockingSwitch.IsEnabled = s.CertInstalled;
        DetailedLogSwitch.IsOn   = s.DetailedLog;
        _updating                = false;

        BlockingCard.Description = $"{s.Rules:N0} rules";
        LogSummary.Header        = $"{s.Blocked:N0} blocked · {s.Allowed:N0} allowed";
        (CertCard.Description, CertButton.Content, CertButton.Tag) = s switch
        {
            { CertInstalled: true, CertRenewal: { } why } => ($"{why} Renewing asks for admin twice.", "Renew", "renew_cert"),
            { CertInstalled: true } => ("Generated for this PC only. Will not be usable anywhere else.", "Remove", "uninstall_cert"),
            _ => ("Needed to block ads on secure (HTTPS) sites. Asks for admin once.", "Install", "install_cert"),
        };

        if (lists)
        {
            BuildListGroups(s.Lists);
            BuildUserscripts(s.Userscripts);
            BuildExclusions(s.Exclusions);
            BuildExcludedApps(s.ExcludedApps);
        }
        BuildProblems(s.Problems);
    }

    private bool _statusPending;

    /// <summary>
    /// One brief status request at a time: while a slow command (like Update now) holds the
    /// core, timer ticks are skipped instead of piling up behind it.
    /// </summary>
    private async Task PollStatusAsync()
    {
        if (_statusPending)
        {
            return;
        }
        _statusPending = true;
        try
        {
            await _app.RefreshAsync();
        }
        finally
        {
            _statusPending = false;
        }
    }

    // =========================================================================
    // ONE-ROW CARDS
    // =========================================================================

    /// <summary>
    /// Settings cards move their switch or button below the text when narrower than 476
    /// (a size the toolkit keeps inside its own template, out of reach of app resources).
    /// Turning off that rule keeps text and control side by side at any width. Checks only
    /// the element given, once it's on screen, and an expander's items each time it opens.
    /// </summary>
    private void OneRow(FrameworkElement element)
    {
        element.Loaded += (_, _) => DisableWrap(element);
        if (element is SettingsExpander expander)
        {
            expander.Expanded += (_, _) => DispatcherQueue.TryEnqueue(DispatcherQueuePriority.Low, () => DisableWrap(expander));
        }
    }

    /// <summary>
    /// Give a settings group a new set of rows. Editing the group's existing list in place
    /// (Clear, Add) doesn't redraw a group that's already on screen, so added or deleted rows
    /// didn't appear until the window was reopened; assigning a new list does redraw it.
    /// </summary>
    private static void ReplaceItems(SettingsExpander group, List<object> items) => group.Items = items;

    private SettingsCard Row(SettingsCard card)
    {
        OneRow(card);
        return card;
    }

    /// <summary>
    /// A page that was hidden gets its cards' templates only once shown, so check it after layout.
    /// </summary>
    private void OneRowWhenShown(FrameworkElement page) =>
        DispatcherQueue.TryEnqueue(DispatcherQueuePriority.Low, () => DisableWrap(page));

    private static void DisableWrap(DependencyObject node)
    {
        if (node is FrameworkElement element)
        {
            foreach (var group in VisualStateManager.GetVisualStateGroups(element).Where(g => g.Name == "ContentAlignmentStates"))
            {
                foreach (var trigger in group.States.SelectMany(s => s.StateTriggers).OfType<CommunityToolkit.WinUI.ControlSizeTrigger>())
                {
                    trigger.CanTrigger = false;
                }
            }
        }
        var count = VisualTreeHelper.GetChildrenCount(node);
        for (var i = 0; i < count; i++)
        {
            DisableWrap(VisualTreeHelper.GetChild(node, i));
        }
    }

    // =========================================================================
    // SHARED HELPERS
    // =========================================================================

    /// <summary>
    /// Run a button's work, logging and showing any error instead of letting it close the app
    /// (that would stop blocking).
    /// </summary>
    private async Task GuardAsync(Func<Task> work)
    {
        try
        {
            await work();
        }
        catch (Exception ex)
        {
            App.LogError(ex);
            ShowError(ex.Message);
        }
    }

    /// <summary>
    /// A dialog over this window with a main button and Cancel, the main button as the default
    /// (HoldPrimary takes that away for a dialog a web page's link opened).
    /// </summary>
    private ContentDialog Dialog(string title, object content, string primary) => new()
    {
        XamlRoot          = Content.XamlRoot,
        Title             = title,
        Content           = content,
        PrimaryButtonText = primary,
        CloseButtonText   = "Cancel",
        DefaultButton     = ContentDialogButton.Primary,
    };

    /// <summary>
    /// For a web page's link or the tray flyout: wait for a window that was just opened to
    /// load, then bring it in front (a minimized one comes back).
    /// </summary>
    private async Task ShowInFrontAsync()
    {
        if (Content is FrameworkElement { IsLoaded: false } root)
        {
            var loaded = new TaskCompletionSource();
            root.Loaded += (_, _) => loaded.TrySetResult();
            await loaded.Task;
        }
        BringToFront();
    }

    /// <summary>
    /// A dialog a web page's link opened jumps in front of whatever you were typing in, so its
    /// main button isn't the default and stays off for a second: a keystroke meant for the
    /// page can't accept it.
    /// </summary>
    private static void HoldPrimary(ContentDialog dialog)
    {
        dialog.DefaultButton          = ContentDialogButton.None;
        dialog.IsPrimaryButtonEnabled = false;
        dialog.Opened += async (_, _) =>
        {
            await Task.Delay(1000);
            dialog.IsPrimaryButtonEnabled = true;
        };
    }

    /// <summary>
    /// Bring the window in front of the browser. Windows normally won't let a background app
    /// take the front, so briefly share input with the window that's in front, which lets it.
    /// Only used when the user asked for this window (an Install or Exclude button on a web page,
    /// or Add app in the tray flyout).
    /// </summary>
    private void BringToFront()
    {
        if (AppWindow.Presenter is OverlappedPresenter { State: OverlappedPresenterState.Minimized } presenter)
        {
            presenter.Restore();
        }

        var hwnd       = WinRT.Interop.WindowNative.GetWindowHandle(this);
        // Sharing input with a frozen window would freeze this one too, so skip the trick then
        var foreground = Native.GetForegroundWindow();
        var front      = Native.IsHungAppWindow(foreground) ? 0 : Native.GetWindowThreadProcessId(foreground, 0);
        var mine       = Native.GetCurrentThreadId();
        var attached   = front != 0 && front != mine && Native.AttachThreadInput(mine, front, true);

        Native.BringWindowToTop(hwnd);
        Native.SetForegroundWindow(hwnd);
        Activate();

        if (attached)
        {
            Native.AttachThreadInput(mine, front, false);
        }
    }

    /// <summary>
    /// Display scaling of the monitor the window is on (1.25 at 125%). Asked from the monitor,
    /// because a window that hasn't been shown yet reports 100%.
    /// </summary>
    private double Scale() =>
        Native.MonitorScale(Native.MonitorFromWindow(WinRT.Interop.WindowNative.GetWindowHandle(this), Native.MonitorDefaultToPrimary));

    /// <summary>
    /// Remember the size you left the window at (not maximized or minimized sizes).
    /// </summary>
    private void SaveSize()
    {
        if (AppWindow.Presenter is not OverlappedPresenter { State: OverlappedPresenterState.Restored })
        {
            return;
        }
        var scale = Scale();
        _app.Settings.WindowWidth  = Math.Round(AppWindow.Size.Width / scale);
        _app.Settings.WindowHeight = Math.Round(AppWindow.Size.Height / scale);
        _app.Settings.Save();
    }

    private void ShowError(string text)
    {
        Message.Severity = InfoBarSeverity.Error;
        Message.Message  = text;
        Message.IsOpen   = true;
        // The message is at the top of the page; bring it into view for actions further down
        PageScroll.ChangeView(null, 0, null);
    }

    // =========================================================================
    // EVENT HANDLERS
    // =========================================================================

    private async void OnBlockingToggled(object sender, RoutedEventArgs e)
    {
        if (_updating)
        {
            return;
        }
        await _app.RunAsync(BlockingSwitch.IsOn ? "start" : "stop");
    }

    private async void OnCertClicked(object sender, RoutedEventArgs e) =>
        await Busy.RunAsync(CertButton, CertRing, () => _app.RunAsync((string)CertButton.Tag));

    /// <summary>
    /// Shows Windows' startup state on the switch. Turned off in Task Manager (or by policy),
    /// this app can't turn it back on, so the switch is off-limits and says where to go.
    /// </summary>
    private async Task ShowStartupAsync(Task<(bool On, bool CanChange)?> state)
    {
        var shown = await state;
        _updating               = true;
        StartupSwitch.IsOn      = shown?.On == true;
        StartupSwitch.IsEnabled = shown?.CanChange == true;
        _updating               = false;
        StartupCard.Description = shown switch
        {
            null => "Windows couldn't say whether Coffee Filter starts with it.",
            { CanChange: false, On: false } => "Turned off in Task Manager. Turn it back on there, in Startup apps.",
            { CanChange: false } => "Your organization turned this on.",
            _ => "Opens in the tray when you sign in, so ad blocking is on from the start.",
        };
    }

    private async void OnStartupToggled(object sender, RoutedEventArgs e)
    {
        if (_updating)
        {
            return;
        }
        await ShowStartupAsync(Startup.SetAsync(StartupSwitch.IsOn));
    }

    private void OnTrayToggled(object sender, RoutedEventArgs e)
    {
        // Set once when the window opens; nothing changed, nothing to save
        if (_app.Settings.ShowTrayIcon == TraySwitch.IsOn)
        {
            return;
        }
        _app.Settings.ShowTrayIcon = TraySwitch.IsOn;
        _app.Settings.Save();
        _app.SetTrayVisible(TraySwitch.IsOn);
    }

    /// <summary>
    /// Off leaves a flyout that was already made hidden rather than closing it: WinUI crashes (in
    /// Microsoft.UI.Input) when a new one is made after closing one.
    /// </summary>
    private void OnFlyoutToggled(object sender, RoutedEventArgs e)
    {
        // Set once when the window opens; nothing changed, nothing to save
        if (_app.Settings.UseFlyout == FlyoutSwitch.IsOn)
        {
            return;
        }
        _app.Settings.UseFlyout = FlyoutSwitch.IsOn;
        _app.Settings.Save();
    }

    private void OnLaunchToggled(object sender, RoutedEventArgs e)
    {
        // Set once when the window opens; nothing changed, nothing to save
        if (_app.Settings.ShowWindowAtLaunch == LaunchSwitch.IsOn)
        {
            return;
        }
        _app.Settings.ShowWindowAtLaunch = LaunchSwitch.IsOn;
        _app.Settings.Save();
    }

    private async void OnDetailedLogToggled(object sender, RoutedEventArgs e)
    {
        if (_updating)
        {
            return;
        }
        await _app.RunAsync("set_detailed_log", new JsonObject { ["enabled"] = DetailedLogSwitch.IsOn });
    }

    /// <summary>
    /// The Coffee Filter folder, where debug.log (and app-errors.log) are.
    /// </summary>
    private void OnOpenLogFolderClicked(object sender, RoutedEventArgs e)
    {
        var folder = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "CoffeeFilter");
        Process.Start(new ProcessStartInfo(folder) { UseShellExecute = true });
    }

    private void OnQuitClicked(object sender, RoutedEventArgs e) => _app.Quit();
}
