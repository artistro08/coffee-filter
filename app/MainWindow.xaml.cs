using System.Numerics;
using System.Text.Json.Nodes;
using CommunityToolkit.WinUI.Controls;
using Microsoft.UI.Composition;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Hosting;

namespace CoffeeFilter;

/// <summary>
/// Settings-style window. Holds no state of its own beyond what's on screen; it renders
/// App.Last and sends commands through App, so it can be destroyed on close and recreated.
/// </summary>
public sealed partial class MainWindow : Window
{
    // List groups, in display order, with their label and icon
    private static readonly (string Kind, string Label, string Glyph)[] Groups =
    [
        ("ads", "Ads", ""),
        ("privacy", "Privacy", ""),
        ("malware", "Malware", ""),
        ("general", "General", ""),
        ("digitalWellbeing", "Digital wellbeing", ""),
        ("regional", "Regional", ""),
        ("custom", "Custom", ""),
    ];

    // Expanded groups survive the window being closed and reopened
    private static readonly HashSet<string> Expanded = [];

    private readonly App _app             = App.Instance;
    private readonly DispatcherTimer _timer    = new() { Interval = TimeSpan.FromSeconds(2) };
    private readonly DispatcherTimer _logTimer = new() { Interval = TimeSpan.FromSeconds(1) };
    private string _listsKey                   = "";
    private string _scriptsKey                 = "";
    private string _problemsKey                = "";
    private string? _exclusionsKey;
    private List<string> _exclusions           = [];
    private ulong _logAfter;
    private bool _updating;

    /// <summary>
    /// Most log entries kept on screen.
    /// </summary>
    private const int LogMax = 200;

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
        ShowMain();

        // Cards keep one row at every width
        foreach (var expander in new[] { UserscriptsCard, ExclusionsCard, ProblemsCard, AboutCard })
        {
            OneRow(expander);
        }

        TraySwitch.IsOn       = _app.Settings.ShowTrayIcon;
        AboutCard.Description = $"Version {App.Version}";

        // Bind to Event Listeners
        _app.Changed += OnChanged;
        _app.Failed  += ShowError;
        _timer.Tick    += async (_, _) => await PollStatusAsync();
        _logTimer.Tick += async (_, _) => await PollLogAsync();
        // Only poll while the window is in focus, so it costs nothing in the background
        Activated      += (_, e) =>
        {
            var active = _app.Last is not null && e.WindowActivationState != WindowActivationState.Deactivated;
            if (active) { _timer.Start(); } else { _timer.Stop(); }
            if (active && LogView.Visibility == Visibility.Visible) { _logTimer.Start(); } else { _logTimer.Stop(); }
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
            Loading.Visibility = Visibility.Collapsed;
            if (LogView.Visibility == Visibility.Collapsed)
            {
                MainContent.Visibility = Visibility.Visible;
                OneRowWhenShown(MainContent);
            }
            _timer.Start();
        }

        _updating                = true;
        BlockingSwitch.IsOn      = s.Running;
        BlockingSwitch.IsEnabled = s.CertInstalled;
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
        }
        BuildProblems(s.Problems, s.Exclusions);
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
            expander.Expanded += (_, _) => DispatcherQueue.TryEnqueue(Microsoft.UI.Dispatching.DispatcherQueuePriority.Low, () => DisableWrap(expander));
        }
    }

    private SettingsCard Row(SettingsCard card)
    {
        OneRow(card);
        return card;
    }

    /// <summary>
    /// A page that was hidden gets its cards' templates only once shown, so check it after layout.
    /// </summary>
    private void OneRowWhenShown(FrameworkElement page) =>
        DispatcherQueue.TryEnqueue(Microsoft.UI.Dispatching.DispatcherQueuePriority.Low, () => DisableWrap(page));

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
        var count = Microsoft.UI.Xaml.Media.VisualTreeHelper.GetChildrenCount(node);
        for (var i = 0; i < count; i++)
        {
            DisableWrap(Microsoft.UI.Xaml.Media.VisualTreeHelper.GetChild(node, i));
        }
    }

    // =========================================================================
    // PAGES
    // =========================================================================

    /// <summary>
    /// Page motion like Windows Settings: a page fades and slides in when shown (sub-pages from
    /// the right, the main page back from the left) and fades out when hidden. Runs on the
    /// compositor, off the UI thread. Skipped when animations are off in Windows settings.
    /// </summary>
    private static void AnimatePage(UIElement page, float from)
    {
        if (!AnimationsOn)
        {
            return;
        }

        var compositor = ElementCompositionPreview.GetElementVisual(page).Compositor;
        var easing     = Decelerate(compositor);
        ElementCompositionPreview.SetIsTranslationEnabled(page, true);

        // Show
        var fadeIn = compositor.CreateScalarKeyFrameAnimation();
        fadeIn.Target   = "Opacity";
        fadeIn.Duration = TimeSpan.FromMilliseconds(250);
        fadeIn.InsertKeyFrame(0, 0);
        fadeIn.InsertKeyFrame(1, 1, easing);

        var slideIn = compositor.CreateVector3KeyFrameAnimation();
        slideIn.Target   = "Translation";
        slideIn.Duration = TimeSpan.FromMilliseconds(300);
        slideIn.InsertKeyFrame(0, new Vector3(from, 0, 0));
        slideIn.InsertKeyFrame(1, Vector3.Zero, easing);

        var show = compositor.CreateAnimationGroup();
        show.Add(fadeIn);
        show.Add(slideIn);

        // Hide
        var fadeOut = compositor.CreateScalarKeyFrameAnimation();
        fadeOut.Target   = "Opacity";
        fadeOut.Duration = TimeSpan.FromMilliseconds(120);
        fadeOut.InsertKeyFrame(1, 0);

        ElementCompositionPreview.SetImplicitShowAnimation(page, show);
        ElementCompositionPreview.SetImplicitHideAnimation(page, fadeOut);
    }

    /// <summary>
    /// Room the title bar's back button takes, which its icon and title slide across.
    /// </summary>
    private const float BackButtonWidth = 40;

    private static bool AnimationsOn => new Windows.UI.ViewManagement.UISettings().AnimationsEnabled;

    /// <summary>
    /// Windows' standard decelerate curve: fast start, gentle stop.
    /// </summary>
    private static CubicBezierEasingFunction Decelerate(Compositor compositor) =>
        compositor.CreateCubicBezierEasingFunction(new Vector2(0.1f, 0.9f), new Vector2(0.2f, 1f));

    /// <summary>
    /// Slide an element from `from` pixels to where it now sits, optionally fading it in, for
    /// changes that aren't a show or hide (new title text, the back button making room).
    /// </summary>
    private static void Slide(UIElement element, float from, bool fade)
    {
        if (!AnimationsOn)
        {
            return;
        }

        var visual = ElementCompositionPreview.GetElementVisual(element);
        var easing = Decelerate(visual.Compositor);
        ElementCompositionPreview.SetIsTranslationEnabled(element, true);

        var slide = visual.Compositor.CreateVector3KeyFrameAnimation();
        slide.Duration = TimeSpan.FromMilliseconds(300);
        slide.InsertKeyFrame(0, new Vector3(from, 0, 0));
        slide.InsertKeyFrame(1, Vector3.Zero, easing);
        visual.StartAnimation("Translation", slide);

        if (!fade)
        {
            return;
        }
        var fadeIn = visual.Compositor.CreateScalarKeyFrameAnimation();
        fadeIn.Duration = TimeSpan.FromMilliseconds(250);
        fadeIn.InsertKeyFrame(0, 0);
        fadeIn.InsertKeyFrame(1, 1, easing);
        visual.StartAnimation("Opacity", fadeIn);
    }

    private void ShowMain()
    {
        // Coming back from a sub-page: the title follows the page in from the left, and the
        // title bar slides back over the space the back button leaves
        if (AppTitleBar.IsBackButtonVisible)
        {
            Slide(Breadcrumb, from: -48, fade: true);
            Slide(AppTitleBar, from: BackButtonWidth, fade: false);
        }
        Breadcrumb.ItemsSource          = new[] { "Coffee Filter" };
        AppTitleBar.IsBackButtonVisible = false;
        LogView.Visibility              = Visibility.Collapsed;
        MainContent.Visibility          = _app.Last is null ? Visibility.Collapsed : Visibility.Visible;
        PageScroll.ChangeView(null, 0, null, disableAnimation: true);
        OneRowWhenShown(MainContent);
    }

    /// <summary>
    /// The Ad blocking page: a live log of blocked requests. The core only records while it's open.
    /// </summary>
    private async void ShowLog()
    {
        Breadcrumb.ItemsSource          = new[] { "Coffee Filter", "Ad blocking" };
        AppTitleBar.IsBackButtonVisible = true;
        Slide(Breadcrumb, from: 48, fade: true);
        Slide(AppTitleBar, from: -BackButtonWidth, fade: false);
        MainContent.Visibility          = Visibility.Collapsed;
        LogView.Visibility              = Visibility.Visible;
        PageScroll.ChangeView(null, 0, null, disableAnimation: true);
        OneRowWhenShown(LogView);
        _logTimer.Start();
        await PollLogAsync();
    }

    private void OnBreadcrumbClicked(BreadcrumbBar sender, BreadcrumbBarItemClickedEventArgs e)
    {
        if (e.Index == 0)
        {
            GoBack();
        }
    }

    private void OnBackRequested(TitleBar sender, object args) => GoBack();

    /// <summary>
    /// Leave the Ad blocking page: stop the live log and return to the main page.
    /// </summary>
    private void GoBack()
    {
        if (LogView.Visibility != Visibility.Visible)
        {
            return;
        }
        _logTimer.Stop();
        _ = _app.RunAsync("log_stop", showErrors: false);
        LogEntries.Children.Clear();
        _logAfter = 0;
        ShowMain();
    }

    private void OnBlockingCardClicked(object sender, RoutedEventArgs e) => ShowLog();

    private void OnBlockingCardInvoked(object? sender, EventArgs e) => ShowLog();

    private void OnClearLogClicked(object sender, RoutedEventArgs e) => LogEntries.Children.Clear();

    private bool _statusPending;
    private bool _logPending;

    /// <summary>
    /// One status request at a time: while a slow command (like Update now) holds the core,
    /// timer ticks are skipped instead of piling up behind it.
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
            await _app.RunAsync("status", showErrors: false);
        }
        finally
        {
            _statusPending = false;
        }
    }

    private async Task PollLogAsync()
    {
        // One at a time, so two overlapping polls can't add the same entries twice
        if (_logPending)
        {
            return;
        }
        _logPending = true;
        CoreReply? reply;
        try
        {
            reply = await _app.RunAsync("log", new JsonObject { ["after"] = _logAfter }, showErrors: false);
        }
        finally
        {
            _logPending = false;
        }
        if (reply?.Log is not { Count: > 0 } entries || LogView.Visibility != Visibility.Visible)
        {
            return;
        }
        _logAfter = entries[^1].Id;

        // Newest first; keep the page light by dropping the oldest
        foreach (var entry in entries.TakeLast(LogMax))
        {
            var when = DateTimeOffset.FromUnixTimeMilliseconds(entry.Time).ToLocalTime();
            var host = Uri.TryCreate(entry.Url, UriKind.Absolute, out var uri) ? uri.Host : entry.Url;
            var card = new SettingsCard
            {
                Header      = host,
                Description = entry.Page is null ? entry.Url : $"{entry.Url}\non {entry.Page}",
                Content     = new TextBlock { Text = $"{entry.Kind} · {when:T}", Style = (Style)Application.Current.Resources["CaptionTextBlockStyle"] },
            };
            OneRow(card);
            LogEntries.Children.Insert(0, card);
        }
        while (LogEntries.Children.Count > LogMax)
        {
            LogEntries.Children.RemoveAt(LogEntries.Children.Count - 1);
        }
    }

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

    // =========================================================================
    // EXCLUDED SITES
    // =========================================================================

    /// <summary>
    /// One card per excluded site, each with a Delete button.
    /// </summary>
    private void BuildExclusions(List<string> exclusions)
    {
        var key = string.Join("\n", exclusions);
        if (key == _exclusionsKey)
        {
            return;
        }
        _exclusionsKey = key;
        _exclusions    = exclusions;
        ExclusionsCard.Items.Clear();

        // Bulk Add
        var bulk = new Button { Content = "Bulk add" };
        bulk.Click += OnBulkExclusionsClicked;
        ExclusionsCard.Items.Add(Row(new SettingsCard { Header = "Add several sites", Description = "Paste one site per line.", Content = bulk }));

        // Empty State
        if (exclusions.Count == 0)
        {
            ExclusionsCard.Items.Add(Row(new SettingsCard { Header = "No excluded sites" }));
        }

        foreach (var site in exclusions)
        {
            var delete = new Button { Content = "Delete" };
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(delete, $"Delete {site}");
            delete.Click += async (_, _) => await SaveExclusionsAsync(_exclusions.Where(h => h != site));
            ExclusionsCard.Items.Add(Row(new SettingsCard { Header = site, Content = delete }));
        }
    }

    private async Task SaveExclusionsAsync(IEnumerable<string> hosts)
    {
        ExclusionsCard.IsExpanded = true;
        await _app.RunAsync("set_exclusions", new JsonObject { ["hosts"] = new JsonArray([.. hosts.Select(h => (JsonNode)h)]) }, lists: true);
    }

    /// <summary>
    /// Ask for sites in a dialog: one site, or one per line for bulk adding.
    /// </summary>
    private async Task<string[]> AskForSitesAsync(bool bulk)
    {
        var box = new TextBox
        {
            Header          = bulk ? "One site per line" : "Site",
            PlaceholderText = bulk ? "mybank.com\nwork.example.com" : "mybank.com",
            AcceptsReturn   = bulk,
            MinHeight       = bulk ? 160 : 0,
            MinWidth        = 400,
        };
        var dialog = new ContentDialog
        {
            XamlRoot          = Content.XamlRoot,
            Title             = bulk ? "Add sites" : "Add a site",
            Content           = box,
            PrimaryButtonText = "Add",
            CloseButtonText   = "Cancel",
            DefaultButton     = ContentDialogButton.Primary,
        };
        if (await dialog.ShowAsync() != ContentDialogResult.Primary)
        {
            return [];
        }
        return box.Text.Split(['\r', '\n'], StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries);
    }

    private async void OnAddExclusionClicked(object sender, RoutedEventArgs e)
    {
        var sites = await AskForSitesAsync(bulk: false);
        if (sites.Length > 0)
        {
            await SaveExclusionsAsync(_exclusions.Concat(sites));
        }
    }

    private async void OnBulkExclusionsClicked(object sender, RoutedEventArgs e)
    {
        var sites = await AskForSitesAsync(bulk: true);
        if (sites.Length > 0)
        {
            await SaveExclusionsAsync(_exclusions.Concat(sites));
        }
    }

    // =========================================================================
    // CONNECTION PROBLEMS
    // =========================================================================

    /// <summary>
    /// One card per host Coffee Filter broke, with an Exclude button. Hidden when empty.
    /// </summary>
    private void BuildProblems(List<Problem> problems, List<string> exclusions)
    {
        var key = string.Join("\n", problems.Select(p => $"{p.Host}|{p.Count}"));
        if (key == _problemsKey)
        {
            return;
        }
        _problemsKey = key;

        ProblemsSection.Visibility = problems.Count > 0 ? Visibility.Visible : Visibility.Collapsed;
        ProblemsCard.Description   = problems.Count == 1 ? "1 app or site" : $"{problems.Count} apps or sites";
        ProblemsCard.Items.Clear();

        foreach (var problem in problems)
        {
            var when    = DateTimeOffset.FromUnixTimeSeconds(problem.Last).ToLocalTime();
            var exclude = new Button { Content = "Exclude" };
            exclude.Click += async (_, _) =>
            {
                // The current list, not the one from when this card was made
                var hosts = new JsonArray([.. _exclusions.Append(problem.Host).Select(h => (JsonNode)h)]);
                await _app.RunAsync("set_exclusions", new JsonObject { ["hosts"] = hosts }, lists: true);
            };

            ProblemsCard.Items.Add(Row(new SettingsCard
            {
                Header      = problem.Host,
                Description = $"{problem.Detail} {problem.Count:N0}× · last {when:t}",
                Content     = exclude,
            }));
        }
    }

    [System.Runtime.InteropServices.LibraryImport("user32.dll")]
    private static partial nint MonitorFromWindow(nint hwnd, uint flags);

    [System.Runtime.InteropServices.LibraryImport("shcore.dll")]
    private static partial int GetDpiForMonitor(nint monitor, int type, out uint dpiX, out uint dpiY);

    /// <summary>
    /// Display scaling of the monitor the window is on (1.25 at 125%). Asked from the monitor,
    /// because a window that hasn't been shown yet reports 100%.
    /// </summary>
    private double Scale()
    {
        const uint MONITOR_DEFAULTTOPRIMARY = 1;
        const int  MDT_EFFECTIVE_DPI        = 0;
        var monitor = MonitorFromWindow(WinRT.Interop.WindowNative.GetWindowHandle(this), MONITOR_DEFAULTTOPRIMARY);
        return GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, out var dpi, out _) == 0 ? dpi / 96.0 : 1.0;
    }

    [System.Runtime.InteropServices.LibraryImport("user32.dll")]
    private static partial nint GetForegroundWindow();

    [System.Runtime.InteropServices.LibraryImport("user32.dll")]
    private static partial uint GetWindowThreadProcessId(nint hwnd, nint processId);

    [System.Runtime.InteropServices.LibraryImport("kernel32.dll")]
    private static partial uint GetCurrentThreadId();

    [System.Runtime.InteropServices.LibraryImport("user32.dll")]
    [return: System.Runtime.InteropServices.MarshalAs(System.Runtime.InteropServices.UnmanagedType.Bool)]
    private static partial bool IsHungAppWindow(nint hwnd);

    [System.Runtime.InteropServices.LibraryImport("user32.dll")]
    [return: System.Runtime.InteropServices.MarshalAs(System.Runtime.InteropServices.UnmanagedType.Bool)]
    private static partial bool AttachThreadInput(uint attach, uint attachTo, [System.Runtime.InteropServices.MarshalAs(System.Runtime.InteropServices.UnmanagedType.Bool)] bool join);

    [System.Runtime.InteropServices.LibraryImport("user32.dll")]
    [return: System.Runtime.InteropServices.MarshalAs(System.Runtime.InteropServices.UnmanagedType.Bool)]
    private static partial bool SetForegroundWindow(nint hwnd);

    [System.Runtime.InteropServices.LibraryImport("user32.dll")]
    [return: System.Runtime.InteropServices.MarshalAs(System.Runtime.InteropServices.UnmanagedType.Bool)]
    private static partial bool BringWindowToTop(nint hwnd);

    /// <summary>
    /// Bring the window in front of the browser. Windows normally won't let a background app
    /// take the front, so briefly share input with the window that's in front, which lets it.
    /// Only used when the user asked for this window (an Install button).
    /// </summary>
    private void BringToFront()
    {
        if (AppWindow.Presenter is OverlappedPresenter { State: OverlappedPresenterState.Minimized } presenter)
        {
            presenter.Restore();
        }

        var hwnd     = WinRT.Interop.WindowNative.GetWindowHandle(this);
        // Sharing input with a frozen window would freeze this one too, so skip the trick then
        var foreground = GetForegroundWindow();
        var front      = IsHungAppWindow(foreground) ? 0 : GetWindowThreadProcessId(foreground, 0);
        var mine       = GetCurrentThreadId();
        var attached   = front != 0 && front != mine && AttachThreadInput(mine, front, true);

        BringWindowToTop(hwnd);
        SetForegroundWindow(hwnd);
        Activate();

        if (attached)
        {
            AttachThreadInput(mine, front, false);
        }
    }

    private void ShowError(string text)
    {
        Message.Severity = InfoBarSeverity.Error;
        Message.Message  = text;
        Message.IsOpen   = true;
    }

    // =========================================================================
    // FILTER LIST CARDS
    // =========================================================================

    /// <summary>
    /// One expander per group. A group's cards are only created when it's opened,
    /// so the 60 list cards don't exist until someone looks at them.
    /// </summary>
    private void BuildListGroups(List<FilterList> lists)
    {
        // Skip the rebuild when nothing visible changed
        var key = string.Join("\n", lists.Select(l => $"{l.Kind}|{l.Name}|{l.Enabled}"));
        if (key == _listsKey)
        {
            return;
        }
        _listsKey = key;
        ListGroups.Children.Clear();

        foreach (var (kind, label, glyph) in Groups)
        {
            var members = lists.Where(l => l.Kind == kind || (kind == "custom" && !Groups.Any(g => g.Kind == l.Kind))).ToList();
            if (members.Count == 0)
            {
                continue;
            }

            var expander = new SettingsExpander
            {
                Header      = label,
                Description = $"{members.Count(l => l.Enabled)} of {members.Count} on",
                HeaderIcon  = new FontIcon { Glyph = glyph },
            };
            OneRow(expander);
            void Fill()
            {
                if (expander.Items.Count > 0)
                {
                    return;
                }
                foreach (var list in members)
                {
                    expander.Items.Add(ListCard(list.Name, list.Url, list.Enabled, kind == "custom", "set_list", "remove_list"));
                }
            }
            expander.Expanded  += (_, _) => { Expanded.Add(kind); Fill(); };
            expander.Collapsed += (_, _) => Expanded.Remove(kind);

            if (Expanded.Contains(kind))
            {
                Fill();
                expander.IsExpanded = true;
            }
            ListGroups.Children.Add(expander);
        }
    }

    /// <summary>
    /// A filter list or userscript: on/off switch, plus a remove button for ones the user added.
    /// </summary>
    private SettingsCard ListCard(string name, string url, bool enabled, bool removable, string setCommand, string removeCommand)
    {
        var toggle = new ToggleSwitch { IsOn = enabled };
        toggle.Toggled += async (_, _) => await _app.RunAsync(setCommand, new JsonObject { ["name"] = name, ["enabled"] = toggle.IsOn }, lists: true);

        var content = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        if (removable)
        {
            var remove = new Button { Content = new FontIcon { Glyph = "", FontSize = 14 } };
            ToolTipService.SetToolTip(remove, "Remove");
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(remove, $"Remove {name}");
            remove.Click += async (_, _) => await _app.RunAsync(removeCommand, new JsonObject { ["name"] = name }, lists: true);
            content.Children.Add(remove);
        }
        content.Children.Add(toggle);

        return Row(new SettingsCard { Header = name, Description = url, Content = content });
    }

    // =========================================================================
    // USERSCRIPT CARDS
    // =========================================================================

    private void BuildUserscripts(List<Userscript> scripts)
    {
        // Skip the rebuild when nothing visible changed
        var key = string.Join("\n", scripts.Select(s => $"{s.Name}|{s.Enabled}"));
        if (key == _scriptsKey)
        {
            return;
        }
        _scriptsKey = key;
        UserscriptsCard.Items.Clear();

        foreach (var script in scripts)
        {
            UserscriptsCard.Items.Add(ListCard(script.Name, script.Url, script.Enabled, true, "set_userscript", "remove_userscript"));
        }
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

    /// <summary>
    /// Run slow work with the button disabled and a loading circle beside it, like Windows Settings.
    /// </summary>
    private static async Task BusyAsync(Button button, ProgressRing ring, Func<Task> work)
    {
        button.IsEnabled = false;
        ring.Visibility  = Visibility.Visible;
        ring.IsActive    = true;
        try
        {
            await work();
        }
        finally
        {
            ring.IsActive    = false;
            ring.Visibility  = Visibility.Collapsed;
            button.IsEnabled = true;
        }
    }

    private async void OnCertClicked(object sender, RoutedEventArgs e) =>
        await BusyAsync(CertButton, CertRing, () => _app.RunAsync((string)CertButton.Tag));

    private async void OnUpdateClicked(object sender, RoutedEventArgs e) =>
        await BusyAsync(UpdateButton, UpdateRing, () => _app.RunAsync("update_lists", lists: true));

    private async void OnAddListClicked(object sender, RoutedEventArgs e)
    {
        var name  = new TextBox { Header = "Name", PlaceholderText = "My list" };
        var url   = new TextBox { Header = "Web address", PlaceholderText = "https://example.com/list.txt" };
        var panel = new StackPanel { Spacing = 12, MinWidth = 400 };
        panel.Children.Add(name);
        panel.Children.Add(url);

        var dialog = new ContentDialog
        {
            XamlRoot          = Content.XamlRoot,
            Title             = "Add a filter list",
            Content           = panel,
            PrimaryButtonText = "Add",
            CloseButtonText   = "Cancel",
            DefaultButton     = ContentDialogButton.Primary,
        };

        if (await dialog.ShowAsync() == ContentDialogResult.Primary)
        {
            Expanded.Add("custom");
            await BusyAsync(AddListButton, AddListRing, () => _app.RunAsync("add_list", new JsonObject { ["name"] = name.Text, ["url"] = url.Text }, lists: true));
        }
    }

    private async void OnAddUserscriptClicked(object sender, RoutedEventArgs e) => await AddUserscriptAsync("", fromLink: false);

    /// <summary>
    /// Ask to add a userscript, with the address filled in when it came from a site's Install button.
    /// From a link, the window jumps in front of whatever you were typing in, so Add isn't the
    /// default button and stays off for a moment: a keystroke meant for the page can't accept it.
    /// </summary>
    public async Task AddUserscriptAsync(string address, bool fromLink)
    {
        // Opened from a link, the window may still be loading
        if (Content is FrameworkElement { IsLoaded: false } root)
        {
            var loaded = new TaskCompletionSource();
            root.Loaded += (_, _) => loaded.TrySetResult();
            await loaded.Task;
        }
        BringToFront();

        var url = new TextBox { Header = "Web address of the .user.js file", PlaceholderText = "https://greasyfork.org/scripts/…/script.user.js", Text = address, MinWidth = 400 };
        var dialog = new ContentDialog
        {
            XamlRoot          = Content.XamlRoot,
            Title             = "Add a userscript",
            Content           = url,
            PrimaryButtonText = "Add",
            CloseButtonText   = "Cancel",
            DefaultButton     = fromLink ? ContentDialogButton.None : ContentDialogButton.Primary,
        };
        if (fromLink)
        {
            dialog.IsPrimaryButtonEnabled = false;
            dialog.Opened += async (_, _) =>
            {
                await Task.Delay(1000);
                dialog.IsPrimaryButtonEnabled = true;
            };
        }

        // Only one dialog can be open at a time
        try
        {
            if (await dialog.ShowAsync() != ContentDialogResult.Primary)
            {
                return;
            }
        }
        catch (Exception ex)
        {
            App.LogError(ex);
            return;
        }
        await BusyAsync(AddScriptButton, AddScriptRing, () => _app.RunAsync("add_userscript", new JsonObject { ["url"] = url.Text }, lists: true));
    }

    private void OnTrayToggled(object sender, RoutedEventArgs e)
    {
        _app.Settings.ShowTrayIcon = TraySwitch.IsOn;
        _app.Settings.Save();
        _app.SetTrayVisible(TraySwitch.IsOn);
    }

    private void OnQuitClicked(object sender, RoutedEventArgs e) => _app.Quit();

    private async void OnClearProblemsClicked(object sender, RoutedEventArgs e) => await _app.RunAsync("clear_problems");
}
