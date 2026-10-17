using System.Diagnostics;
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
/// The window goes back to its original size afterward.
/// </summary>
public sealed class AppFixture : IDisposable
{
    private readonly UIA3Automation _automation = new();
    private readonly Application _app;
    private readonly System.Drawing.Rectangle _originalBounds;

    public Window Window { get; }

    public AppFixture()
    {
        // Launching again while it runs just brings the window back (single instance)
        var family = PackageFamilyName();
        Process.Start(new ProcessStartInfo("explorer.exe", $"shell:AppsFolder\\{family}!App") { UseShellExecute = true })?.WaitForExit();

        var process = Retry.WhileNull(() => Process.GetProcessesByName("CoffeeFilter").FirstOrDefault(), TimeSpan.FromSeconds(20)).Result
            ?? throw new InvalidOperationException("Coffee Filter didn't start.");
        _app   = Application.Attach(process);
        Window = Retry.WhileNull(() => _app.GetMainWindow(_automation), TimeSpan.FromSeconds(20)).Result
            ?? throw new InvalidOperationException("Coffee Filter's window didn't open.");

        // Wait for the filter lists to load (the main page shows its sections then)
        Retry.WhileNull(() => Window.FindFirstDescendant(c => c.ByName("Protection")), TimeSpan.FromSeconds(60), throwOnTimeout: true);
        _originalBounds = Window.BoundingRectangle;
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
