using System.Text.Json.Nodes;
using H.NotifyIcon;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Input;
using Microsoft.Windows.AppLifecycle;

namespace CoffeeFilter;

/// <summary>
/// Owns everything that lives for the whole session: the core process, the tray icon,
/// and settings. The window is disposable: closing it frees the entire UI tree while
/// blocking keeps running, and the tray (or launching the app again) recreates it.
/// </summary>
public partial class App : Application
{
    private static App? _instance;
    private readonly Windows.UI.ViewManagement.UISettings _uiSettings = new();
    private readonly Queue<DateTime> _coreRestarts = new();
    private DispatcherQueue? _dispatcher;
    private TaskCompletionSource _coreReady = new(TaskCreationOptions.RunContinuationsAsynchronously);
    private MainWindow? _window;
    private TaskbarIcon? _tray;
    private TrayMenu? _trayMenu;
    private FlyoutWindow? _flyout;
    private readonly List<Problem> _found = [];
    private DispatcherQueueTimer? _foundTimer;
    private bool _running;
    private bool _quitting;
    // Opened by a web page's link or at sign-in: no automatic certificate setup (a site must never
    // raise an admin prompt, and sign-in is no time for one)
    private bool _skipCertSetup;
    private bool _endingSession;
    private bool _runningAtSessionEnd;
    private bool _coreRefused;
    private bool _retriedRefusal;

    public static App Instance => _instance!;

    /// <summary>
    /// This build's version, e.g. "0.1.0.1".
    /// </summary>
    public static string Version { get; } = typeof(App).Assembly.GetName().Version?.ToString(4) ?? "";

    public CoreClient Core { get; private set; } = null!;
    public AppSettings Settings { get; } = AppSettings.Load();
    public Shortcuts Shortcuts { get; private set; } = null!;
    public Notifications Notifications { get; } = new();

    /// <summary>
    /// Latest reply from the core, so a newly opened window can render immediately.
    /// </summary>
    public CoreReply? Last { get; private set; }

    /// <summary>
    /// Raised with every reply; the flag says whether lists or exclusions may have changed.
    /// </summary>
    public event Action<CoreReply, bool>? Changed;
    public event Action<string>? Failed;

    public App()
    {
        _instance = this;
        InitializeComponent();

        // A UI error must not close the app (that would stop blocking); log it instead
        UnhandledException += (_, e) =>
        {
            e.Handled = true;
            LogError(e.Exception);
        };
    }

    /// <summary>
    /// Append to %LOCALAPPDATA%\CoffeeFilter\app-errors.log.
    /// </summary>
    public static void LogError(Exception ex)
    {
        try
        {
            var log = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "CoffeeFilter", "app-errors.log");

            // Starts over past 1 MB, so a repeating error can't fill the disk
            if (new FileInfo(log) is { Exists: true, Length: > 1_000_000 })
            {
                File.Delete(log);
            }
            File.AppendAllText(log, $"{DateTime.Now:s} {ex}{Environment.NewLine}");
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            // Nowhere to log; still keep running
        }
    }

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        // Keep running with no window open; only Quit ends the app
        DispatcherShutdownMode = DispatcherShutdownMode.OnExplicitShutdown;
        _dispatcher            = DispatcherQueue.GetForCurrentThread();

        SetTrayVisible(Settings.ShowTrayIcon);
        _uiSettings.ColorValuesChanged += (_, _) => _dispatcher.TryEnqueue(UpdateTrayIconTheme);

        // Notifications: a click opens the page it's about
        Notifications.Invoked += page => _dispatcher.TryEnqueue(() => Activate(new AppLink(page, "")));
        Notifications.Register();

        // Global Shortcuts
        Shortcuts                 = new Shortcuts();
        Shortcuts.Pressed        += OnShortcut;
        Shortcuts.SessionEnding  += OnSessionEnding;
        Shortcuts.Apply(Settings);

        // Opens in the tray with no window, unless a link asked for a page, you turned on Show
        // window at launch, or there's no tray icon to open it from (never at sign-in)
        var activation = AppInstance.GetCurrent().GetActivatedEventArgs();
        var atSignIn   = activation.Kind == ExtendedActivationKind.StartupTask;
        var link       = ReadLink(activation);
        _skipCertSetup = link is not null || atSignIn;
        if (link is not null || (!atSignIn && (Settings.ShowWindowAtLaunch || !Settings.ShowTrayIcon)))
        {
            Activate(link);
        }
        StartCore(resumeBlocking: Settings.BlockingEnabled);
        _ = Startup.EnableOnFirstRunAsync(Settings);
    }

    /// <summary>
    /// Windows is shutting down, restarting, or signing out. Stop the core now, while Windows
    /// still waits for the app, so the proxy settings are put back; otherwise Windows ends both
    /// programs and leaves Windows pointed at a proxy that's gone until the next start. Ad
    /// blocking stays remembered as on. If the shutdown is canceled, blocking comes back.
    /// </summary>
    private void OnSessionEnding(bool ending)
    {
        if (ending)
        {
            if (_endingSession || _quitting)
            {
                return;
            }
            _endingSession       = true;
            _runningAtSessionEnd = _running;
            // The proxy settings go back within milliseconds; open connections get 2 seconds
            Core?.Dispose(2000);
            return;
        }

        if (!_endingSession)
        {
            return;
        }
        _endingSession = false;
        StartCore(resumeBlocking: _runningAtSessionEnd);
    }

    // =========================================================================
    // CORE
    // =========================================================================

    private void StartCore(bool resumeBlocking)
    {
        if (_coreReady.Task.IsCompleted)
        {
            _coreReady = new(TaskCreationOptions.RunContinuationsAsynchronously);
        }
        CoreClient? core = null;
        try
        {
            core = new CoreClient(() => _dispatcher?.TryEnqueue(() => OnCoreExited(core!)));
            core.ProblemFound += problem => _dispatcher?.TryEnqueue(() => OnProblemFound(problem));
        }
        catch (Exception ex) when (ex is System.ComponentModel.Win32Exception or InvalidOperationException)
        {
            LogError(ex);
            GiveUp("The filter core couldn't start, so ad blocking is off. Reinstall Coffee Filter to fix it.");
            return;
        }
        Core = core;
        _ = InitAsync(resumeBlocking);
    }

    /// <summary>
    /// No core is coming: show why, and release every command waiting for one (they'd
    /// otherwise wait forever, leaving buttons spinning), each with the same message.
    /// </summary>
    private void GiveUp(string message)
    {
        // Commands sent from now on get the same message, not the closed core's error
        var error = new InvalidOperationException(message);
        if (!_coreReady.TrySetException(error))
        {
            _coreReady = new(TaskCreationOptions.RunContinuationsAsynchronously);
            _coreReady.SetException(error);
        }
        Failed?.Invoke(message);
    }

    private async Task InitAsync(bool resumeBlocking)
    {
        try
        {
            var reply = await Core.WaitReadyAsync();
            // Refused to start (another core already runs for this account): restarting won't help.
            // One that's still exiting (the app was just reopened) refuses too, so try once more.
            if (!reply.Ok && reply.Status is null)
            {
                _coreRefused = true;
                if (!_retriedRefusal)
                {
                    _retriedRefusal = true;
                    Core.Dispose();
                    await Task.Delay(3000);
                    _coreRefused = false;
                    StartCore(resumeBlocking);
                    return;
                }
                GiveUp(reply.Error ?? "The filter core couldn't start.");
                return;
            }
            _coreReady.TrySetResult();
            Publish(reply, showErrors: true, lists: true);
            if (reply.Status is not { } s)
            {
                return;
            }

            // Each new install or update sets up (or renews) the system-wide certificate once;
            // declining doesn't nag until the next version. Not when a web page's link started
            // the app: a site must never be able to raise an admin prompt.
            if ((!s.CertInstalled || s.CertRenewal is not null) && Settings.CertSetupVersion != Version && !_skipCertSetup)
            {
                Settings.CertSetupVersion = Version;
                Settings.Save();
                await RunAsync(s.CertInstalled ? "renew_cert" : "install_cert");
            }
            // Without a certificate there's nothing to start; the card already says so
            if (resumeBlocking && Last?.Status?.CertInstalled == true)
            {
                await RunAsync("start");
            }
        }
        catch (Exception ex)
        {
            Failed?.Invoke(ex.Message);
        }
    }

    /// <summary>
    /// The core ended without being asked to. It restores the proxy settings itself on most
    /// exits, and a fresh core restores them on start in every other case, so restart it right
    /// away and resume blocking if it was on. Three crashes within a minute stop the retries.
    /// </summary>
    private void OnCoreExited(CoreClient core)
    {
        if (_quitting || _endingSession || _coreRefused || !ReferenceEquals(core, Core))
        {
            return;
        }

        var wasRunning = _running;
        if (Last?.Status is { } s)
        {
            s.Running = false;
            Publish(Last, showErrors: false, lists: false);
        }
        core.Dispose();

        var now = DateTime.UtcNow;
        while (_coreRestarts.Count > 0 && now - _coreRestarts.Peek() > TimeSpan.FromMinutes(1))
        {
            _coreRestarts.Dequeue();
        }
        if (_coreRestarts.Count >= 3)
        {
            GiveUp("The filter core keeps stopping, so ad blocking is off. Quit and reopen Coffee Filter to try again.");
            return;
        }
        _coreRestarts.Enqueue(now);
        StartCore(resumeBlocking: wasRunning);
    }

    /// <summary>
    /// Send a command to the core and broadcast the reply. Returns the reply, or null if the core is gone.
    /// </summary>
    public async Task<CoreReply?> RunAsync(string cmd, JsonObject? args = null, bool showErrors = true, bool lists = false)
    {
        try
        {
            // Commands sent before the core's ready line would read it as their reply
            await _coreReady.Task;
            var reply = await Core.SendAsync(cmd, args);

            // Remember on/off for the next launch (Quit stops blocking without changing it)
            if (reply.Ok && cmd is "start" or "stop")
            {
                Settings.BlockingEnabled = cmd == "start";
                Settings.Save();
            }
            Publish(reply, showErrors, lists);
            return reply;
        }
        catch (Exception ex)
        {
            Failed?.Invoke(ex.Message);
            return null;
        }
    }

    private void Publish(CoreReply reply, bool showErrors, bool lists)
    {
        Last = reply;
        if (reply.Status is { } s)
        {
            UpdateTrayBlocking(s.Running, s.CertInstalled);
        }
        if (!reply.Ok && showErrors)
        {
            Failed?.Invoke(reply.Error ?? "Something went wrong.");
        }
        Changed?.Invoke(reply, lists);
    }

    // =========================================================================
    // NOTIFICATIONS
    // =========================================================================

    /// <summary>
    /// A host just started having connection problems. Problems often come several at once
    /// (an app talks to a few hosts), so they're collected for 2 seconds and shown as one
    /// notification. A click opens Connection problems.
    /// </summary>
    private void OnProblemFound(Problem problem)
    {
        _found.Add(problem);
        if (_foundTimer is null)
        {
            _foundTimer             = _dispatcher!.CreateTimer();
            _foundTimer.Interval    = TimeSpan.FromSeconds(2);
            _foundTimer.IsRepeating = false;
            _foundTimer.Tick       += (_, _) => NotifyProblems();
        }
        // Starting a running timer restarts it
        _foundTimer.Start();

        // An open window shows it right away
        if (_window is not null)
        {
            _ = RunAsync("status", showErrors: false);
        }
    }

    private void NotifyProblems()
    {
        var hosts = _found.Select(p => p.Host).Distinct().ToList();
        if (hosts.Count == 0)
        {
            return;
        }
        var (title, message) = hosts.Count switch
        {
            1 => ($"{hosts[0]} couldn't connect", _found[^1].Detail),
            2 => ("2 sites couldn't connect", $"{hosts[0]} and {hosts[1]}. Exclude them in Connection problems if something isn't working."),
            _ => ($"{hosts.Count} sites couldn't connect", $"{hosts[0]}, {hosts[1]}, and {hosts.Count - 2} more. Exclude them in Connection problems if something isn't working."),
        };
        _found.Clear();
        Notifications.Show(title, message, open: "problems", tag: "problems");
    }

    // =========================================================================
    // SHORTCUTS
    // =========================================================================

    /// <summary>
    /// A global shortcut was pressed. Turning ad blocking on or off and updating lists happen
    /// in the background, so a notification says how it went.
    /// </summary>
    private async void OnShortcut(ShortcutAction action)
    {
        switch (action)
        {
            case ShortcutAction.OpenFlyout:
                ToggleFlyout(atCursor: false);
                break;
            case ShortcutAction.OpenApp:
                ShowWindow();
                break;
            case ShortcutAction.ToggleBlocking:
                if (Last?.Status is not { CertInstalled: true })
                {
                    Notifications.Show("Ad blocking needs the certificate", "Open Coffee Filter to install it.", tag: "blocking");
                    return;
                }
                var toggled = await RunAsync(_running ? "stop" : "start", showErrors: false);
                if (toggled is { Ok: true })
                {
                    Notifications.Show(_running ? "Ad blocking is on" : "Ad blocking is off", "", tag: "blocking");
                }
                else
                {
                    Notifications.Show("Couldn't change ad blocking", toggled?.Error ?? "The filter core isn't responding.", tag: "blocking");
                }
                break;
            case ShortcutAction.UpdateLists:
                Notifications.Show("Updating filter lists…", "", tag: "update");
                var updated = await RunAsync("update_lists", showErrors: false, lists: true);
                if (updated is { Ok: true })
                {
                    Notifications.Show("Filter lists are up to date", "", tag: "update");
                }
                else
                {
                    Notifications.Show("Some filter lists couldn't update", updated?.Error ?? "The filter core isn't responding.", tag: "update");
                }
                break;
        }
    }

    // =========================================================================
    // WINDOW
    // =========================================================================

    public void ShowWindow()
    {
        SetBackground(false);
        if (_window is null)
        {
            _window         = new MainWindow();
            _window.Closed += (_, _) =>
            {
                _window = null;
                SetBackground(true);
                // Release the UI tree, then trim the working set while running in the tray
                _dispatcher?.TryEnqueue(DispatcherQueuePriority.Low, () =>
                {
                    GC.Collect();
                    GC.WaitForPendingFinalizers();
                    GC.Collect();
                    using var self = System.Diagnostics.Process.GetCurrentProcess();
                    K32EmptyWorkingSet(self.Handle);
                });
            };
        }
        _window.Activate();
    }

    /// <summary>
    /// Add app, chosen in the tray flyout: the window, with the program or Store app picker.
    /// </summary>
    public void PickExcludedApp(bool store)
    {
        ShowWindow();
        _ = _window!.PickExcludedAppAsync(store);
    }

    [System.Runtime.InteropServices.LibraryImport("kernel32.dll")]
    [return: System.Runtime.InteropServices.MarshalAs(System.Runtime.InteropServices.UnmanagedType.Bool)]
    private static partial bool K32EmptyWorkingSet(nint process);

    /// <summary>
    /// With no window open, the app only keeps the tray icon and watches the core, so it runs
    /// in efficiency mode at low priority and stays out of the way when the PC is busy. Opening
    /// the window or the tray menu puts it back to normal right away. (Blocking itself runs in
    /// the core, which is never slowed down.)
    /// </summary>
    public void SetBackground(bool background)
    {
        if (background && _window is not null)
        {
            return;
        }
        var state = new PowerThrottlingState
        {
            Version     = 1,
            ControlMask = PowerThrottlingExecutionSpeed,
            StateMask   = background ? PowerThrottlingExecutionSpeed : 0,
        };
        unsafe
        {
            SetProcessInformation(GetCurrentProcess(), ProcessPowerThrottling, &state, (uint)sizeof(PowerThrottlingState));
        }
        SetPriorityClass(GetCurrentProcess(), background ? BelowNormalPriorityClass : NormalPriorityClass);
    }

    private const int  ProcessPowerThrottling        = 4;
    private const uint PowerThrottlingExecutionSpeed = 0x1;
    private const uint NormalPriorityClass           = 0x20;
    private const uint BelowNormalPriorityClass      = 0x4000;

    [System.Runtime.InteropServices.StructLayout(System.Runtime.InteropServices.LayoutKind.Sequential)]
    private struct PowerThrottlingState
    {
        public uint Version;
        public uint ControlMask;
        public uint StateMask;
    }

    [System.Runtime.InteropServices.LibraryImport("kernel32.dll")]
    private static partial nint GetCurrentProcess();

    [System.Runtime.InteropServices.LibraryImport("kernel32.dll")]
    [return: System.Runtime.InteropServices.MarshalAs(System.Runtime.InteropServices.UnmanagedType.Bool)]
    private static unsafe partial bool SetProcessInformation(nint process, int infoClass, void* info, uint size);

    [System.Runtime.InteropServices.LibraryImport("kernel32.dll")]
    [return: System.Runtime.InteropServices.MarshalAs(System.Runtime.InteropServices.UnmanagedType.Bool)]
    private static partial bool SetPriorityClass(nint process, uint priorityClass);

    [System.Runtime.InteropServices.LibraryImport("user32.dll")]
    private static partial int GetSystemMetrics(int index);

    /// <summary>
    /// Called when Coffee Filter is launched again while running. Safe from any thread.
    /// The arguments are read right away: they belong to the second launch, which exits
    /// once this returns, and reading them after that fails.
    /// </summary>
    public static void ActivateFromAnyThread(AppActivationArguments args)
    {
        var link = ReadLink(args);
        _instance?._dispatcher?.TryEnqueue(() => _instance.Activate(link));
    }

    /// <summary>
    /// A coffee-filter:// link from a web page: what it asks for, and its one value.
    /// </summary>
    /// <param name="Action">"install" (a userscript page's Install button), "exclude" (the blocked page's Exclude button), or a notification's page ("main" or "problems").</param>
    /// <param name="Value">The script address, or the site name.</param>

    /// <summary>
    /// The link that opened the app (coffee-filter://install?url=... or coffee-filter://exclude?host=...),
    /// or null for a normal launch.
    /// </summary>
    private static AppLink? ReadLink(AppActivationArguments args)
    {
        try
        {
            // A notification clicked while the app wasn't running
            if (args.Kind == ExtendedActivationKind.AppNotification && args.Data is Microsoft.Windows.AppNotifications.AppNotificationActivatedEventArgs notification)
            {
                return notification.Arguments.TryGetValue(Notifications.OpenArgument, out var page) ? new AppLink(page, "") : null;
            }
            if (args.Kind != ExtendedActivationKind.Protocol || args.Data is not Windows.ApplicationModel.Activation.IProtocolActivatedEventArgs { Uri: { } uri })
            {
                return null;
            }
            return AppLink.Parse(uri.Host, uri.Query);
        }
        catch (Exception ex)
        {
            LogError(ex);
            return null;
        }
    }

    /// <summary>
    /// Show the window, and for a link, its dialog: Add userscript with the address filled in,
    /// or Exclude this site with the site name.
    /// </summary>
    private void Activate(AppLink? link)
    {
        ShowWindow();
        switch (link?.Action)
        {
            case "install":
                _ = _window!.AddUserscriptAsync(link.Value, fromLink: true);
                break;
            case "exclude":
                _ = _window!.ExcludeSiteAsync(link.Value);
                break;
            case "problems":
                _window!.ShowProblems();
                break;
        }
    }

    public void Quit()
    {
        _quitting = true;

        // Each step on its own: one that fails (no core, say) must not keep the app from exiting
        Action[] steps = [() => Shortcuts?.Suspend(), Notifications.Unregister, () => _tray?.Dispose(), () => _window?.Close(), () => Core?.Dispose()];
        foreach (var step in steps)
        {
            try
            {
                step();
            }
            catch (Exception ex)
            {
                LogError(ex);
            }
        }
        Exit();
    }

    // =========================================================================
    // TRAY ICON
    // =========================================================================

    /// <summary>
    /// Open the tray flyout, or close it if it's open. With the flyout turned off, opens the window.
    /// </summary>
    /// <param name="atCursor">On the monitor under the cursor (a tray click); otherwise the main taskbar's.</param>
    public void ToggleFlyout(bool atCursor)
    {
        if (!Settings.UseFlyout)
        {
            ShowWindow();
            return;
        }
        try
        {
            (_flyout ??= new FlyoutWindow()).Toggle(atCursor);
        }
        catch (Exception ex)
        {
            // A flyout glitch must never take blocking down with it; log it and open the window instead
            LogError(ex);
            ShowWindow();
        }
    }

    /// <summary>
    /// Turn the flyout on or off. Off leaves a flyout that was already made hidden rather than
    /// closing it: WinUI crashes (in Microsoft.UI.Input) when a new one is made after closing one.
    /// </summary>
    public void SetFlyoutEnabled(bool enabled)
    {
        // The window sets its switch on every open; nothing changed, nothing to save
        if (Settings.UseFlyout == enabled)
        {
            return;
        }
        Settings.UseFlyout = enabled;
        Settings.Save();
    }

    /// <summary>
    /// The tray icon is declared in App.xaml. A click opens the flyout; its right-click menu is
    /// a TrayMenu, which keeps it clear of the taskbar. This wires up the commands.
    /// </summary>
    private void CreateTray()
    {
        _tray     = (TaskbarIcon)Resources["TrayIcon"];
        _trayMenu = new TrayMenu(ShowWindow, async () => await RunAsync(_running ? "stop" : "start"), Quit);

        // Installed: one fixed ID, so the icon keeps its place on the taskbar across updates (the
        // exe is signed for this; see SignNativeExecutable). A build run from its own folder keeps
        // the library's ID, made from its path: Windows refuses a fixed ID from an unsigned exe
        // that moved, which would leave no icon at all.
        if (IsPackaged())
        {
            _tray.Id = TrayIconId;
        }

        ((XamlUICommand)Resources["OpenCommand"]).ExecuteRequested += (_, _) => ToggleFlyout(atCursor: true);
        ((XamlUICommand)Resources["MenuCommand"]).ExecuteRequested += (_, _) =>
        {
            try
            {
                _trayMenu.Show();
            }
            catch (Exception ex)
            {
                // A menu glitch must never take blocking down with it; log it and open the window instead
                LogError(ex);
                ShowWindow();
            }
        };

        UpdateTrayIconTheme();
        if (Last?.Status is { } s)
        {
            UpdateTrayBlocking(s.Running, s.CertInstalled);
        }
    }

    /// <summary>
    /// The installed app's tray icon ID. Never change it: Windows keeps the icon's place by it.
    /// </summary>
    private static readonly Guid TrayIconId = new("2DB25E80-4F5F-404A-B185-79860FEAB53E");

    private static bool IsPackaged()
    {
        try
        {
            return Windows.ApplicationModel.Package.Current is not null;
        }
        catch (Exception ex) when (ex is InvalidOperationException or System.Runtime.InteropServices.COMException)
        {
            // No package identity: run from a build folder
            return false;
        }
    }

    private void UpdateTrayBlocking(bool running, bool certInstalled)
    {
        _running = running;
        _trayMenu?.SetBlocking(running, certInstalled);
    }

    public void SetTrayVisible(bool visible)
    {
        if (_tray is null)
        {
            if (!visible)
            {
                return;
            }
            CreateTray();
            _tray!.ForceCreate();
        }
        _tray!.Visibility = visible ? Visibility.Visible : Visibility.Collapsed;
    }

    /// <summary>
    /// Mono Fluent tray icon: white on a dark taskbar, black on a light one.
    /// </summary>
    private void UpdateTrayIconTheme()
    {
        if (_tray is null)
        {
            return;
        }
        using var key = Microsoft.Win32.Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
        var light     = key?.GetValue("SystemUsesLightTheme") is 1;
        var file      = Path.Combine(AppContext.BaseDirectory, "Assets", $"tray-{(light ? "black" : "white")}.ico");

        // Loaded here and handed over ready-made. An IconSource image is converted in the
        // background, and when the tray icon was created first, it sometimes showed no icon.
        // The size Windows draws tray icons at for this display's scaling
        const int SM_CXSMICON = 49;
        var size   = GetSystemMetrics(SM_CXSMICON);
        var old    = _tray.Icon;
        _tray.Icon = new System.Drawing.Icon(file, size, size);
        old?.Dispose();
    }
}
