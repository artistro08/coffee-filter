using System.Numerics;
using System.Text.Json.Nodes;
using CommunityToolkit.WinUI.Controls;
using Microsoft.UI.Composition;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Hosting;

namespace CoffeeFilter;

/// <summary>
/// The window's pages: the main page, Ad blocking (a live log of blocked requests), and
/// Connection problems, with Windows Settings' page motion between them.
/// </summary>
public sealed partial class MainWindow
{
    private readonly DispatcherTimer _logTimer = new() { Interval = TimeSpan.FromSeconds(1) };
    private ulong _logAfter;
    private bool _logPending;
    private string? _problemsKey;

    /// <summary>
    /// Most log entries kept on screen.
    /// </summary>
    private const int LogMax = 200;

    /// <summary>
    /// The sub-page on screen (Ad blocking or Connection problems), or null on the main page.
    /// </summary>
    private FrameworkElement? _subPage;

    // =========================================================================
    // PAGE MOTION
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

    private static readonly Windows.UI.ViewManagement.UISettings UiSettings = new();

    internal static bool AnimationsOn => UiSettings.AnimationsEnabled;

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

    // =========================================================================
    // NAVIGATION
    // =========================================================================

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
        ProblemsView.Visibility         = Visibility.Collapsed;
        _subPage                        = null;
        MainContent.Visibility          = _app.Last is null ? Visibility.Collapsed : Visibility.Visible;
        PageScroll.ChangeView(null, 0, null, disableAnimation: true);
        OneRowWhenShown(MainContent);
    }

    /// <summary>
    /// Open a sub-page: its name joins the title, the back button appears, and it slides in.
    /// </summary>
    private void ShowSubPage(FrameworkElement page, string title)
    {
        Breadcrumb.ItemsSource          = new[] { "Coffee Filter", title };
        AppTitleBar.IsBackButtonVisible = true;
        Slide(Breadcrumb, from: 48, fade: true);
        Slide(AppTitleBar, from: -BackButtonWidth, fade: false);
        MainContent.Visibility          = Visibility.Collapsed;
        page.Visibility                 = Visibility.Visible;
        _subPage                        = page;
        PageScroll.ChangeView(null, 0, null, disableAnimation: true);
        OneRowWhenShown(page);
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
    /// Leave a sub-page for the main page. Leaving Ad blocking also stops the live log.
    /// </summary>
    private void GoBack()
    {
        if (_subPage is null)
        {
            return;
        }
        if (_subPage == LogView)
        {
            _logTimer.Stop();
            _ = _app.RunAsync("log_stop", showErrors: false);
            LogEntries.Children.Clear();
            _logAfter = 0;
        }
        ShowMain();
    }

    // =========================================================================
    // AD BLOCKING (LIVE LOG)
    // =========================================================================

    /// <summary>
    /// The Ad blocking page: a live log of blocked requests. The core only records while it's open.
    /// </summary>
    private async void ShowLog()
    {
        ShowSubPage(LogView, "Ad blocking");
        _logTimer.Start();
        await PollLogAsync();
    }

    private void OnBlockingCardClicked(object sender, RoutedEventArgs e) => ShowLog();

    private void OnBlockingCardInvoked(object? sender, EventArgs e) => ShowLog();

    private void OnClearLogClicked(object sender, RoutedEventArgs e) => LogEntries.Children.Clear();

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

        // A burst (opening the page, or after a reconnect) skips the slide-in animation, so the
        // GPU doesn't run dozens of them at once; trickling entries keep it
        var transitions = LogEntries.ChildrenTransitions;
        var burst       = entries.Count > 10;
        if (burst)
        {
            LogEntries.ChildrenTransitions = null;
        }

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
        if (burst)
        {
            LogEntries.ChildrenTransitions = transitions;
        }
    }

    // =========================================================================
    // CONNECTION PROBLEMS
    // =========================================================================

    /// <summary>
    /// The Connection problems page: apps and sites that refused Coffee Filter's certificate.
    /// </summary>
    internal void ShowProblems() => ShowSubPage(ProblemsView, "Connection problems");

    private void OnProblemsCardClicked(object sender, RoutedEventArgs e) => ShowProblems();

    private void OnProblemsCardInvoked(object? sender, EventArgs e) => ShowProblems();

    private async void OnClearProblemsClicked(object sender, RoutedEventArgs e) => await _app.RunAsync("clear_problems");

    /// <summary>
    /// One card per host Coffee Filter broke, with an Exclude button, on the Connection problems
    /// page; the main page's card shows how many.
    /// </summary>
    private void BuildProblems(List<Problem> problems)
    {
        var key = string.Join("\n", problems.Select(p => $"{p.Host}|{p.Count}"));
        if (key == _problemsKey)
        {
            return;
        }
        _problemsKey = key;

        ProblemsCard.Description = problems.Count switch
        {
            0 => "No problems so far",
            1 => "1 app or site couldn't connect",
            _ => $"{problems.Count} apps or sites couldn't connect",
        };
        ProblemsList.Children.Clear();

        // Empty State
        if (problems.Count == 0)
        {
            ProblemsList.Children.Add(Row(new SettingsCard { Header = "No connection problems" }));
        }

        foreach (var problem in problems)
        {
            var when    = DateTimeOffset.FromUnixTimeSeconds(problem.Last).ToLocalTime();
            var exclude = new Button { Content = "Exclude" };
            exclude.Click += async (_, _) => await _app.RunAsync("add_exclusion", new JsonObject { ["host"] = problem.Host }, lists: true);

            ProblemsList.Children.Add(Row(new SettingsCard
            {
                Header      = problem.Host,
                Description = $"{problem.Detail} {problem.Count:N0}× · last {when:t}",
                Content     = exclude,
            }));
        }
    }
}
