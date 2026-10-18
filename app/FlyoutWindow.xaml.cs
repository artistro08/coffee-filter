using System.Numerics;
using Microsoft.UI.Composition;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Hosting;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media.Animation;
using Windows.Graphics;

namespace CoffeeFilter;

/// <summary>
/// The tray flyout: Ad blocking, Update lists, Excluded sites and apps, and Settings, in an acrylic panel that slides
/// out of the taskbar on whichever edge it's docked. Built the way MagicBuds (and Battery
/// Flyout) build theirs: a borderless, always-on-top, see-through window sits flush on the
/// taskbar edge, as tall as the work area and one margin wider than the panel. Only the panel
/// shows, and it slides inside the window, so the window's edge at the taskbar cuts it off and
/// it looks like it comes out from behind the taskbar. Hides when it loses focus, like Windows'
/// own flyouts. The click on the tray icon that takes focus away arrives right after the hide,
/// so a toggle within <see cref="ReopenGuardMs"/> of hiding is ignored. Lives for the session
/// once opened, hidden in between.
/// </summary>
public sealed partial class FlyoutWindow : Window
{
    private const long ReopenGuardMs = 300;

    /// <summary>
    /// Focus changes this soon after opening are the shell handing focus around, not the user leaving.
    /// </summary>
    private const long FocusGraceMs = 300;

    // Fluent motion: https://learn.microsoft.com/windows/apps/design/motion/timing-and-easing
    private static readonly TimeSpan EnterDuration = TimeSpan.FromMilliseconds(250);
    private static readonly TimeSpan ExitDuration  = TimeSpan.FromMilliseconds(167);

    private readonly App _app = App.Instance;
    private readonly nint _hwnd;
    private readonly Visual _panelVisual;
    private readonly Compositor _compositor;
    private long _shownAt;
    private long _hiddenAt;
    private bool _isOpen;
    private bool _updating;
    private TaskbarEdge _edge;

    // Current window region in window pixels
    private (int Left, int Top, int Right, int Bottom) _clip;
    private CompositionScopedBatch? _slide;
    private Storyboard? _resize;

    public FlyoutWindow()
    {
        InitializeComponent();
        SystemBackdrop          = new TransparentBackdrop();
        Acrylic.SystemBackdrop  = new ActiveAcrylicBackdrop();
        _hwnd                   = WinRT.Interop.WindowNative.GetWindowHandle(this);

        // Window Chrome
        var presenter = OverlappedPresenter.Create();
        presenter.IsResizable   = false;
        presenter.IsMaximizable = false;
        presenter.IsMinimizable = false;
        presenter.IsAlwaysOnTop = true;
        presenter.SetBorderAndTitleBar(false, false);
        AppWindow.SetPresenter(presenter);
        AppWindow.IsShownInSwitchers = false;

        // Plain frameless popup; the panel inside draws its own rounded corners and stroke
        PopupWindow.Apply(_hwnd);

        // Slide Animation Target
        ElementCompositionPreview.SetIsTranslationEnabled(SlideHost, true);
        _panelVisual = ElementCompositionPreview.GetElementVisual(SlideHost);
        _compositor  = _panelVisual.Compositor;

        // Esc closes it, like Windows' own flyouts; with the Add site box or Add app choices
        // open, just those
        var escape = new KeyboardAccelerator { Key = Windows.System.VirtualKey.Escape };
        escape.Invoked += (_, e) =>
        {
            e.Handled = true;
            if (AddSitePanel.Visibility == Visibility.Visible)
            {
                ShowAddSite(false);
                return;
            }
            if (AddAppPanel.Visibility == Visibility.Visible)
            {
                ShowAddApp(false);
                return;
            }
            HideFlyout();
        };
        WindowRoot.KeyboardAccelerators.Add(escape);
        WindowRoot.KeyboardAcceleratorPlacementMode = KeyboardAcceleratorPlacementMode.Hidden;

        // Bind to Event Listeners
        // Live resize: the panel's height animates to the content's, and the region follows the
        // panel. The region updates after every layout pass, because a hidden window lays out
        // only once it's shown again.
        PanelContent.SizeChanged += (_, _) => ResizePanel(animate: _isOpen);
        WindowRoot.LayoutUpdated += (_, _) => ClipToPanel();
        _app.Changed             += (reply, _) => Render(reply);
        AppWindow.Closing        += (_, e) =>
        {
            // Only Quit really closes it
            e.Cancel = true;
            HideFlyout();
        };
        Activated += (_, e) =>
        {
            if (e.WindowActivationState != WindowActivationState.Deactivated || !_isOpen)
            {
                return;
            }
            // Opening from the hidden-icons panel: the panel closes and Windows hands focus
            // back to whatever was active before. Take it back instead of closing.
            if (Environment.TickCount64 - _shownAt < FocusGraceMs)
            {
                H.NotifyIcon.Core.WindowUtilities.SetForegroundWindow(_hwnd);
                return;
            }
            HideFlyout();
        };

        if (_app.Last is { } last)
        {
            Render(last);
        }
    }

    /// <summary>
    /// Open, or close if open. From the tray icon (on the monitor under the cursor) or a
    /// shortcut (on the main taskbar's monitor).
    /// </summary>
    public void Toggle(bool atCursor)
    {
        if (_isOpen)
        {
            HideFlyout();
            return;
        }
        if (Environment.TickCount64 - _hiddenAt < ReopenGuardMs)
        {
            return;
        }

        // From the hidden-icons panel, wait for the panel to close first, otherwise it takes
        // focus back the moment the flyout opens
        if (!TrayMenu.IsShellOverflow(TrayMenu.GetForegroundWindow()))
        {
            ShowFlyout(atCursor);
            return;
        }
        var timer = DispatcherQueue.CreateTimer();
        timer.Interval    = TimeSpan.FromMilliseconds(150);
        timer.IsRepeating = false;
        timer.Tick       += (_, _) => ShowFlyout(atCursor);
        timer.Start();
    }

    /// <summary>
    /// Slides the panel back behind the taskbar, then hides the window.
    /// </summary>
    public void HideFlyout()
    {
        if (!_isOpen)
        {
            return;
        }
        _isOpen   = false;
        _hiddenAt = Environment.TickCount64;

        // Hand focus back so keys don't go to the closing window
        if (PopupWindow.Foreground == _hwnd)
        {
            PopupWindow.FocusTaskbar();
        }
        Slide(HiddenOffset(), ExitDuration, new Vector2(1, 0), new Vector2(1, 1));
    }

    private void ShowFlyout(bool atCursor)
    {
        _isOpen  = true;
        _shownAt = Environment.TickCount64;
        _app.SetBackground(false);

        // Still sliding out from a close that hasn't finished: turn around from where it is
        if (!AppWindow.IsVisible)
        {
            PlaceWindow(atCursor);
            _panelVisual.Properties.InsertVector3("Translation", HiddenOffset());
            AppWindow.Show(true);
        }
        Activate();
        H.NotifyIcon.Core.WindowUtilities.SetForegroundWindow(_hwnd);

        // WinUI loads a window's content only once it's first activated, so the very first
        // open waits for that to know how far to slide
        if (!WindowRoot.IsLoaded)
        {
            SlideHost.Opacity = 0;
            void OnLoaded(object s, RoutedEventArgs e)
            {
                WindowRoot.Loaded -= OnLoaded;
                PlaceWindow(atCursor);
                _panelVisual.Properties.InsertVector3("Translation", HiddenOffset());
                SlideHost.Opacity = 1;
                Slide(Vector3.Zero, EnterDuration, new Vector2(0, 0), new Vector2(0, 1));
            }
            WindowRoot.Loaded += OnLoaded;
            return;
        }
        Slide(Vector3.Zero, EnterDuration, new Vector2(0, 0), new Vector2(0, 1));
    }

    // =========================================================================
    // CONTENT
    // =========================================================================

    private void Render(CoreReply reply)
    {
        if (reply.Status is not { } s)
        {
            return;
        }
        _updating                = true;
        BlockingSwitch.IsOn      = s.Running;
        BlockingSwitch.IsEnabled = s.CertInstalled;
        _updating                = false;
        BlockingText.Text        = s.CertInstalled ? $"{s.Rules:N0} rules" : "Needs the certificate. Install it in Settings.";
        ExclusionsText.Text      = s.Exclusions.Count switch
        {
            0 => "None yet",
            1 => "1 site",
            var n => $"{n} sites",
        };
        AppsText.Text            = s.ExcludedApps.Count switch
        {
            0 => "None yet",
            1 => "1 app",
            var n => $"{n} apps",
        };
    }

    private void OnAddSiteClicked(object sender, RoutedEventArgs e)
    {
        // One open at a time
        if (AddAppPanel.Visibility == Visibility.Visible)
        {
            ShowAddApp(false);
        }
        ShowAddSite(AddSitePanel.Visibility == Visibility.Collapsed);
    }

    /// <summary>
    /// Opens or closes the box for a site to exclude; the panel grows to fit it. While it's
    /// open, Add site turns into Cancel, which closes it and forgets what was typed.
    /// </summary>
    private void ShowAddSite(bool open)
    {
        AddSitePanel.Visibility = open ? Visibility.Visible : Visibility.Collapsed;
        AddSiteButton.Content   = open ? "Cancel" : "Add site";
        if (open)
        {
            SiteBox.Focus(FocusState.Programmatic);
            return;
        }
        SiteBox.Text = "";
        AddSiteButton.Focus(FocusState.Programmatic);
    }

    private void OnAddAppClicked(object sender, RoutedEventArgs e)
    {
        // One open at a time
        if (AddSitePanel.Visibility == Visibility.Visible)
        {
            ShowAddSite(false);
        }
        ShowAddApp(AddAppPanel.Visibility == Visibility.Collapsed);
    }

    /// <summary>
    /// Opens or closes the choice of a program or a Store app; while it's open, Add app turns
    /// into Cancel, like Add site.
    /// </summary>
    private void ShowAddApp(bool open)
    {
        AddAppPanel.Visibility = open ? Visibility.Visible : Visibility.Collapsed;
        AddAppButton.Content   = open ? "Cancel" : "Add app";
        (open ? AddProgramButton : AddAppButton).Focus(FocusState.Programmatic);
    }

    private void OnAddProgramClicked(object sender, RoutedEventArgs e) => PickApp(store: false);

    private void OnAddStoreAppClicked(object sender, RoutedEventArgs e) => PickApp(store: true);

    /// <summary>
    /// The file picker and the Store app list take focus, which would close the flyout under
    /// them, so they open in the window instead.
    /// </summary>
    private void PickApp(bool store)
    {
        ShowAddApp(false);

        // Window first, while the flyout still has the foreground to pass on (see OnSettingsClicked)
        _app.PickExcludedApp(store);
        HideFlyout();
    }

    private async void OnSiteBoxKeyDown(object sender, KeyRoutedEventArgs e)
    {
        if (e.Key != Windows.System.VirtualKey.Enter)
        {
            return;
        }
        e.Handled = true;
        await SaveSiteAsync();
    }

    private async void OnSaveSiteClicked(object sender, RoutedEventArgs e) => await SaveSiteAsync();

    /// <summary>
    /// Excludes the typed site. The core checks it's a site name, so its message shows as is.
    /// </summary>
    private async Task SaveSiteAsync()
    {
        var host = SiteBox.Text.Trim();
        if (host.Length == 0 || !SaveSiteButton.IsEnabled)
        {
            return;
        }

        SaveSiteButton.IsEnabled = false;
        AddSiteRing.IsActive     = true;
        AddSiteRing.Visibility   = Visibility.Visible;
        Message.Visibility       = Visibility.Collapsed;
        try
        {
            var reply = await _app.RunAsync("add_exclusion", new System.Text.Json.Nodes.JsonObject { ["host"] = host }, showErrors: false, lists: true);
            if (reply is { Ok: true })
            {
                ShowAddSite(false);
                ShowMessage($"{host} is excluded. Reload its page to apply.", error: false);
            }
            else
            {
                ShowMessage(reply?.Error ?? "The filter core isn't responding. Open Settings for details.", error: true);
            }
        }
        finally
        {
            SaveSiteButton.IsEnabled = true;
            AddSiteRing.IsActive     = false;
            AddSiteRing.Visibility   = Visibility.Collapsed;
        }
    }

    private async void OnBlockingToggled(object sender, RoutedEventArgs e)
    {
        if (_updating)
        {
            return;
        }
        var reply = await _app.RunAsync(BlockingSwitch.IsOn ? "start" : "stop", showErrors: false);
        if (reply is not { Ok: true })
        {
            ShowMessage(reply?.Error ?? "The filter core isn't responding. Open Settings for details.", error: true);
        }
    }

    private async void OnUpdateClicked(object sender, RoutedEventArgs e)
    {
        UpdateButton.IsEnabled  = false;
        UpdateRing.IsActive     = true;
        UpdateRing.Visibility   = Visibility.Visible;
        Message.Visibility      = Visibility.Collapsed;
        try
        {
            var reply = await _app.RunAsync("update_lists", showErrors: false, lists: true);
            if (reply is { Ok: true })
            {
                ShowMessage("Lists are up to date.", error: false);
            }
            else
            {
                ShowMessage(reply?.Error ?? "The filter core isn't responding. Open Settings for details.", error: true);
            }
        }
        finally
        {
            UpdateButton.IsEnabled = true;
            UpdateRing.IsActive    = false;
            UpdateRing.Visibility  = Visibility.Collapsed;
        }
    }

    private void ShowMessage(string text, bool error)
    {
        Message.Text       = text;
        Message.Foreground = (Microsoft.UI.Xaml.Media.Brush)Application.Current.Resources[error ? "SystemFillColorCriticalBrush" : "TextFillColorSecondaryBrush"];
        Message.Visibility = Visibility.Visible;
    }

    private void OnSettingsClicked(object sender, RoutedEventArgs e)
    {
        // Window first, while the flyout still has the foreground to pass on; closing first
        // would hand it to the taskbar, and Windows then refuses to bring the window forward
        _app.ShowWindow();
        HideFlyout();
    }

    // =========================================================================
    // PLACEMENT AND MOTION
    // =========================================================================

    // Window = the tallest the flyout may get, one margin wider than the panel, flush on the
    // taskbar edge. The panel sits against the taskbar side and is only as tall as its content,
    // so it grows and shrinks live; the window region keeps the rest see-through to clicks.
    private void PlaceWindow(bool atCursor)
    {
        var (panel, edge, scale) = PopupWindow.Place(atCursor);
        var margin = (int)Math.Round(PopupWindow.MarginDip * scale);
        _edge = edge;
        SlideHost.VerticalAlignment = edge == TaskbarEdge.Top ? VerticalAlignment.Top : VerticalAlignment.Bottom;

        // ponytail: sized twice because moving to a monitor with another DPI rescales the
        // window once it's shown; a WM_DPICHANGED hook would do it in one pass
        var bounds = new RectInt32(panel.Left - margin, panel.Top - margin, panel.Right - panel.Left + (2 * margin), panel.Bottom - panel.Top + (2 * margin));
        AppWindow.MoveAndResize(bounds);
        AppWindow.MoveAndResize(bounds);

        WindowRoot.UpdateLayout();
        ResizePanel(animate: false);
        WindowRoot.UpdateLayout();
        _clip = default;
        ClipToPanel();
    }

    // Panel height = content height (plus its border), capped at what the window allows
    private void ResizePanel(bool animate)
    {
        var target  = PanelContent.ActualHeight + Panel.BorderThickness.Top + Panel.BorderThickness.Bottom;
        var tallest = WindowRoot.ActualHeight - WindowRoot.Padding.Top - WindowRoot.Padding.Bottom;
        if (tallest > 0)
        {
            target = Math.Min(target, tallest);
        }

        // Start from wherever a running resize got to
        var current = Panel.ActualHeight;
        _resize?.Stop();
        _resize = null;
        if (!animate || current <= 0 || Math.Abs(current - target) < 0.5)
        {
            Panel.Height = target;
            return;
        }
        Panel.Height = current;

        var animation = new DoubleAnimationUsingKeyFrames { EnableDependentAnimation = true };
        animation.KeyFrames.Add(new SplineDoubleKeyFrame
        {
            KeyTime   = EnterDuration,
            Value     = target,
            KeySpline = new KeySpline { ControlPoint1 = new Windows.Foundation.Point(0, 0), ControlPoint2 = new Windows.Foundation.Point(0, 1) },
        });
        Storyboard.SetTarget(animation, Panel);
        Storyboard.SetTargetProperty(animation, "Height");

        var resize = new Storyboard();
        resize.Children.Add(animation);
        resize.Completed += (_, _) =>
        {
            if (_resize == resize)
            {
                Panel.Height = target;
                resize.Stop();
                _resize = null;
            }
        };
        _resize = resize;
        resize.Begin();
    }

    // Far enough to put the whole panel past the window edge at the taskbar, in DIPs
    private Vector3 HiddenOffset()
    {
        var width  = (float)(SlideHost.ActualWidth + PopupWindow.MarginDip);
        var height = (float)(SlideHost.ActualHeight + PopupWindow.MarginDip);
        return _edge switch
        {
            TaskbarEdge.Top => new Vector3(0, -height, 0),
            TaskbarEdge.Left => new Vector3(-width, 0, 0),
            TaskbarEdge.Right => new Vector3(width, 0, 0),
            _ => new Vector3(0, height, 0),
        };
    }

    // Window region = the panel plus its margin (room for the shadow), stretched to the taskbar
    // edge so the slide still shows. Everything else doesn't draw and lets clicks through.
    private void ClipToPanel()
    {
        if (WindowRoot.XamlRoot is not { } root)
        {
            return;
        }
        var scale  = root.RasterizationScale;
        var top    = SlideHost.ActualOffset.Y - PopupWindow.MarginDip;
        var bottom = SlideHost.ActualOffset.Y + SlideHost.ActualHeight + PopupWindow.MarginDip;
        if (_edge == TaskbarEdge.Top)
        {
            top = 0;
        }
        else
        {
            bottom = (float)WindowRoot.ActualHeight;
        }

        var clip = (0, (int)Math.Floor(top * scale), (int)Math.Ceiling(WindowRoot.ActualWidth * scale), (int)Math.Ceiling(bottom * scale));
        if (clip == _clip)
        {
            return;
        }
        _clip = clip;
        PopupWindow.Clip(_hwnd, clip.Item1, clip.Item2, clip.Item3, clip.Item4);
    }

    // Animates the panel's translation from wherever it is now; hides the window once a
    // close finishes, unless it was reopened in the meantime. Instant with animations off.
    private void Slide(Vector3 to, TimeSpan duration, Vector2 control1, Vector2 control2)
    {
        var animation = _compositor.CreateVector3KeyFrameAnimation();
        animation.InsertExpressionKeyFrame(0, "this.StartingValue");
        animation.InsertKeyFrame(1, to, _compositor.CreateCubicBezierEasingFunction(control1, control2));
        animation.Duration = MainWindow.AnimationsOn ? duration : TimeSpan.FromMilliseconds(1);

        var slide = _compositor.CreateScopedBatch(CompositionBatchTypes.Animation);
        _panelVisual.StartAnimation("Translation", animation);
        slide.End();
        _slide = slide;

        slide.Completed += (_, _) =>
        {
            if (_slide != slide || _isOpen)
            {
                return;
            }
            AppWindow.Hide();
            _app.SetBackground(true);
        };
    }
}
