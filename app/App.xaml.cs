using System.Text.Json.Nodes;
using H.NotifyIcon;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media.Imaging;
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
    private bool _running;
    private bool _quitting;
    private bool _openedByLink;
    private bool _coreRefused;

    public static App Instance => _instance!;

    /// <summary>
    /// This build's version, e.g. "0.1.0.1".
    /// </summary>
    public static string Version { get; } = typeof(App).Assembly.GetName().Version?.ToString(4) ?? "";

    public CoreClient Core { get; private set; } = null!;
    public AppSettings Settings { get; } = AppSettings.Load();

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

        var link = UserscriptLink(AppInstance.GetCurrent().GetActivatedEventArgs());
        _openedByLink = link is not null;
        Activate(link);
        StartCore(resumeBlocking: Settings.BlockingEnabled);
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
        core = new CoreClient(() => _dispatcher?.TryEnqueue(() => OnCoreExited(core!)));
        Core = core;
        _ = InitAsync(resumeBlocking);
    }

    private async Task InitAsync(bool resumeBlocking)
    {
        try
        {
            var reply = await Core.WaitReadyAsync();
            // Refused to start (another core already runs for this account): restarting won't help
            if (!reply.Ok && reply.Status is null)
            {
                _coreRefused = true;
                Failed?.Invoke(reply.Error ?? "The filter core couldn't start.");
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
            if ((!s.CertInstalled || s.CertRenewal is not null) && Settings.CertSetupVersion != Version && !_openedByLink)
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
        if (_quitting || _coreRefused || !ReferenceEquals(core, Core))
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
            Failed?.Invoke("The filter core keeps stopping, so ad blocking is off. Quit and reopen Coffee Filter to try again.");
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
    // WINDOW
    // =========================================================================

    public void ShowWindow()
    {
        if (_window is null)
        {
            _window         = new MainWindow();
            _window.Closed += (_, _) =>
            {
                _window = null;
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

    [System.Runtime.InteropServices.LibraryImport("kernel32.dll")]
    [return: System.Runtime.InteropServices.MarshalAs(System.Runtime.InteropServices.UnmanagedType.Bool)]
    private static partial bool K32EmptyWorkingSet(nint process);

    /// <summary>
    /// Called when Coffee Filter is launched again while running. Safe from any thread.
    /// The arguments are read right away: they belong to the second launch, which exits
    /// once this returns, and reading them after that fails.
    /// </summary>
    public static void ActivateFromAnyThread(AppActivationArguments args)
    {
        var url = UserscriptLink(args);
        _instance?._dispatcher?.TryEnqueue(() => _instance.Activate(url));
    }

    /// <summary>
    /// The script address from a coffee-filter://install?url=... link (an Install button on
    /// a userscript page), or null for a normal launch.
    /// </summary>
    private static string? UserscriptLink(AppActivationArguments args)
    {
        try
        {
            if (args.Kind != ExtendedActivationKind.Protocol || args.Data is not Windows.ApplicationModel.Activation.IProtocolActivatedEventArgs { Uri: { } uri })
            {
                return null;
            }
            if (uri.Host != "install" || string.IsNullOrEmpty(uri.Query))
            {
                return null;
            }
            return new Windows.Foundation.WwwFormUrlDecoder(uri.Query).FirstOrDefault(p => p.Name == "url")?.Value;
        }
        catch (Exception ex)
        {
            LogError(ex);
            return null;
        }
    }

    /// <summary>
    /// Show the window, and for an install link, the Add userscript dialog with the address filled in.
    /// </summary>
    private void Activate(string? userscript)
    {
        ShowWindow();
        if (userscript is not null)
        {
            _ = _window!.AddUserscriptAsync(userscript, fromLink: true);
        }
    }

    public void Quit()
    {
        _quitting = true;
        _tray?.Dispose();
        _window?.Close();
        Core.Dispose();
        Exit();
    }

    // =========================================================================
    // TRAY ICON
    // =========================================================================

    /// <summary>
    /// The tray icon is declared in App.xaml; its right-click menu is a TrayMenu, which keeps
    /// it clear of the taskbar. This wires up the commands.
    /// </summary>
    private void CreateTray()
    {
        _tray     = (TaskbarIcon)Resources["TrayIcon"];
        _trayMenu = new TrayMenu(ShowWindow, async () => await RunAsync(_running ? "stop" : "start"), Quit);

        ((XamlUICommand)Resources["OpenCommand"]).ExecuteRequested += (_, _) => ShowWindow();
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
        _tray.IconSource = new BitmapImage(new Uri($"ms-appx:///Assets/tray-{(light ? "black" : "white")}.ico"));
    }
}
