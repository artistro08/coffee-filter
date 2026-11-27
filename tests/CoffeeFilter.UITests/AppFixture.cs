using System.Diagnostics;
using System.Text.Json;
using FlaUI.Core;
using FlaUI.Core.AutomationElements;
using FlaUI.Core.Definitions;
using FlaUI.Core.Tools;
using FlaUI.UIA3;

namespace CoffeeFilter.UITests;

/// <summary>
/// Opens the installed Coffee Filter app (or uses the running one) and hands its main window
/// to the tests. The tests look, open pages and groups, and cancel dialogs. One test excludes
/// a Store app you haven't excluded and deletes it again, so your settings end as they began.
/// The tray and flyout tests need Show tray icon and Use tray flyout on; whichever you turned off
/// is turned on for the run and off again afterward. The window goes back to its original size.
/// </summary>
public sealed class AppFixture : IDisposable
{
    /// <summary>
    /// Switches the tests need on, with the app.json setting each one saves to.
    /// </summary>
    private static readonly (string Name, string Setting)[] NeededOn = [("Show tray icon", "ShowTrayIcon"), ("Use tray flyout", "UseFlyout")];

    private readonly UIA3Automation _automation = new();
    private readonly Application _app;
    private readonly System.Drawing.Rectangle _originalBounds;
    private readonly List<string> _turnedOn = [];

    public Window Window { get; }

    public AppFixture()
    {
        // Launching again while it runs just brings the window back (single instance)
        var family = PackageFamilyName();
        void Launch() => Process.Start(new ProcessStartInfo("explorer.exe", $"shell:AppsFolder\\{family}!App") { UseShellExecute = true })?.WaitForExit();
        Launch();

        var process = Retry.WhileNull(() => Process.GetProcessesByName("CoffeeFilter").FirstOrDefault(), TimeSpan.FromSeconds(20)).Result
            ?? throw new InvalidOperationException("Coffee Filter didn't start.");
        _app = Application.Attach(process);

        // By name: the tray flyout is a window of its own. Started fresh, it opens in the tray
        // (unless Show window at launch is on), so open it again for the window
        Window? MainWindow(TimeSpan wait) =>
            Retry.WhileNull(() => _app.GetAllTopLevelWindows(_automation).FirstOrDefault(w => w.Name == "Coffee Filter"), wait).Result;
        var window = MainWindow(TimeSpan.FromSeconds(5));
        if (window is null)
        {
            Launch();
            window = MainWindow(TimeSpan.FromSeconds(20));
        }
        Window = window ?? throw new InvalidOperationException("Coffee Filter's window didn't open.");

        // Wait for the filter lists to load (the main page shows its sections then)
        Retry.WhileNull(() => Window.FindFirstDescendant(c => c.ByName("Protection")), TimeSpan.FromSeconds(60), throwOnTimeout: true);
        _originalBounds = Window.BoundingRectangle;

        // Turn On What the Tests Need
        foreach (var (name, setting) in NeededOn)
        {
            if (SetSwitch(name, setting, on: true))
            {
                _turnedOn.Add(name);
            }
        }
    }

    /// <summary>
    /// Sets a settings switch and waits until app.json has it. Returns whether it had to change.
    /// </summary>
    private bool SetSwitch(string name, string setting, bool on)
    {
        var toggle = Retry.WhileNull(() => Window.FindAllDescendants(c => c.ByName(name)).FirstOrDefault(e => e.Patterns.Toggle.IsSupported), TimeSpan.FromSeconds(5)).Result
            ?? throw new InvalidOperationException($"No {name} switch.");
        if ((toggle.Patterns.Toggle.Pattern.ToggleState.Value == ToggleState.On) == on)
        {
            return false;
        }
        toggle.Patterns.Toggle.Pattern.Toggle();
        bool Saved()
        {
            using var json = ReadAppSettings();
            return json.RootElement.TryGetProperty(setting, out var value) && value.GetBoolean() == on;
        }
        if (!Retry.WhileFalse(Saved, TimeSpan.FromSeconds(3)).Success)
        {
            throw new InvalidOperationException($"{name} didn't save as {(on ? "on" : "off")}.");
        }
        return true;
    }

    /// <summary>
    /// The installed package's family name, from PowerShell (no extra dependencies).
    /// </summary>
    private static string PackageFamilyName()
    {
        var info = new ProcessStartInfo("powershell.exe", "-NoProfile -Command \"(Get-AppxPackage Artistro08.CoffeeFilter).PackageFamilyName\"")
        {
            RedirectStandardOutput = true,
            UseShellExecute        = false,
            CreateNoWindow         = true,
        };
        using var process = Process.Start(info)!;
        var family = process.StandardOutput.ReadToEnd().Trim();
        process.WaitForExit();
        return string.IsNullOrEmpty(family) ? throw new InvalidOperationException("Coffee Filter isn't installed.") : family;
    }

    /// <summary>
    /// Find an element by name anywhere in the window, waiting for it to appear.
    /// </summary>
    public AutomationElement Find(string name, TimeSpan? wait = null) =>
        Retry.WhileNull(() => Window.FindFirstDescendant(c => c.ByName(name)), wait ?? TimeSpan.FromSeconds(10)).Result
        ?? throw new InvalidOperationException($"\"{name}\" isn't on screen.");

    /// <summary>
    /// Find a clickable settings card (or other button-like element) by name.
    /// </summary>
    public AutomationElement FindButton(string name, TimeSpan? wait = null) =>
        Retry.WhileNull(() => Window.FindFirstDescendant(c => c.ByName(name).And(c.ByControlType(ControlType.Button))), wait ?? TimeSpan.FromSeconds(10)).Result
        ?? throw new InvalidOperationException($"No \"{name}\" button.");

    /// <summary>
    /// Open a settings group (SettingsExpander) by its header.
    /// </summary>
    public AutomationElement Expand(string name)
    {
        var expander = Retry.WhileNull(
                () => Window.FindAllDescendants(c => c.ByName(name)).FirstOrDefault(e => e.Patterns.ExpandCollapse.IsSupported),
                TimeSpan.FromSeconds(10)).Result
            ?? throw new InvalidOperationException($"No \"{name}\" group.");
        expander.Patterns.ScrollItem.PatternOrDefault?.ScrollIntoView();
        if (expander.Patterns.ExpandCollapse.Pattern.ExpandCollapseState.Value != ExpandCollapseState.Expanded)
        {
            expander.Patterns.ExpandCollapse.Pattern.Expand();
        }
        Thread.Sleep(400);
        return expander;
    }

    /// <summary>
    /// The app's saved settings (app.json). The app swaps in a new file when a setting
    /// changes; read without blocking the swap, and try again if it lands in the middle of one.
    /// </summary>
    public static JsonDocument ReadAppSettings()
    {
        var file = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "CoffeeFilter", "app.json");
        var text = Retry.WhileException(() =>
        {
            using var stream = new FileStream(file, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete);
            using var reader = new StreamReader(stream);
            return reader.ReadToEnd();
        }, TimeSpan.FromSeconds(2), throwOnTimeout: true).Result!;
        return JsonDocument.Parse(text);
    }

    /// <summary>
    /// Back to the main page, whatever page is open.
    /// </summary>
    public void GoHome()
    {
        var back = Window.FindFirstDescendant(c => c.ByName("Back").And(c.ByControlType(ControlType.Button)));
        if (back is not null && back.IsEnabled && !back.IsOffscreen)
        {
            back.Patterns.Invoke.Pattern.Invoke();
            Thread.Sleep(500);
        }
    }

    public void Dispose()
    {
        GoHome();

        // Back Off, the Way You Had Them
        foreach (var (name, setting) in NeededOn.Where(s => _turnedOn.Contains(s.Name)))
        {
            SetSwitch(name, setting, on: false);
        }

        var transform = Window.Patterns.Transform.PatternOrDefault;
        if (transform is not null && _originalBounds.Width > 0)
        {
            transform.Resize(_originalBounds.Width, _originalBounds.Height);
            transform.Move(_originalBounds.X, _originalBounds.Y);
        }
        _automation.Dispose();
    }
}

[CollectionDefinition("App")]
public sealed class AppCollection : ICollectionFixture<AppFixture>;
