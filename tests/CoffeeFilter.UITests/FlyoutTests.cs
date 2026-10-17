using System.Text.Json;
using FlaUI.Core.AutomationElements;
using FlaUI.Core.Definitions;
using FlaUI.Core.Input;
using FlaUI.Core.Tools;
using FlaUI.Core.WindowsAPI;

namespace CoffeeFilter.UITests;

/// <summary>
/// The tray flyout, opened with its global shortcut the way a person would, and the Keyboard
/// shortcuts settings. Nothing is changed: the picker is canceled, the flyout's switch isn't touched.
/// </summary>
[Collection("App")]
public sealed class FlyoutTests(AppFixture app)
{
    [Fact]
    public void ShortcutsGroupListsEveryShortcut()
    {
        app.GoHome();
        app.Expand("Keyboard shortcuts");
        foreach (var (name, id) in new[] { ("Open the tray flyout", "OpenFlyout"), ("Turn ad blocking on or off", "ToggleBlocking"), ("Update filter lists", "UpdateLists"), ("Open Coffee Filter", "OpenApp") })
        {
            Assert.NotNull(app.Find(name));
            var picker = Picker(id);
            Assert.Equal(name, picker.Name);
        }
    }

    [Fact]
    public void ShortcutPickerOpensAndCancels()
    {
        app.GoHome();
        app.Expand("Keyboard shortcuts");
        Picker("UpdateLists").Patterns.Invoke.Pattern.Invoke();

        // PowerToys' dialog: titled after the setting, the keys area, Reset and Clear, Save and Cancel
        Assert.NotNull(Retry.WhileNull(() => app.Window.FindFirstDescendant(c => c.ByAutomationId("ShortcutPreview")), TimeSpan.FromSeconds(5)).Result);
        Assert.NotNull(app.Find("Update filter lists shortcut"));
        Assert.NotNull(app.Window.FindFirstDescendant(c => c.ByAutomationId("ShortcutReset")));
        Assert.NotNull(app.Window.FindFirstDescendant(c => c.ByAutomationId("ShortcutClear")));
        Assert.NotNull(app.FindButton("Save"));

        // Pressed keys show up as they're pressed (the keys area is named after them); canceled, so nothing is saved
        using (Keyboard.Pressing(VirtualKeyShort.CONTROL, VirtualKeyShort.SHIFT))
        {
            Keyboard.Type(VirtualKeyShort.KEY_X);
        }
        var shown = Retry.WhileFalse(() => app.Window.FindFirstDescendant(c => c.ByAutomationId("ShortcutPreview"))?.Name == "Ctrl+Shift+X", TimeSpan.FromSeconds(3)).Success;
        Assert.True(shown, $"the picker didn't show Ctrl+Shift+X (it shows \"{app.Window.FindFirstDescendant(c => c.ByAutomationId("ShortcutPreview"))?.Name}\")");

        app.FindButton("Cancel").Patterns.Invoke.Pattern.Invoke();
        Assert.True(Retry.WhileNotNull(() => app.Window.FindFirstDescendant(c => c.ByAutomationId("ShortcutPreview")), TimeSpan.FromSeconds(5)).Success, "the picker didn't close");
    }

    [Fact]
    public void ShortcutOpensTheFlyoutAndSettingsBringsTheWindowBack()
    {
        app.GoHome();
        var flyout = OpenFlyout();

        // Only what was asked for: the switch, Update lists, Excluded sites, and Settings
        Assert.NotNull(flyout.FindFirstDescendant(c => c.ByName("Turn ad blocking on or off")));
        Assert.NotNull(flyout.FindFirstDescendant(c => c.ByName("Update now").And(c.ByControlType(ControlType.Button))));
        var settings = flyout.FindFirstDescendant(c => c.ByName("Settings").And(c.ByControlType(ControlType.Button)));
        Assert.NotNull(settings);

        // Its switch matches the window's
        var mine   = flyout.FindFirstDescendant(c => c.ByName("Turn ad blocking on or off")).Patterns.Toggle.Pattern.ToggleState.Value;
        var window = app.Window.FindFirstDescendant(c => c.ByName("Turn ad blocking on or off")).Patterns.Toggle.Pattern.ToggleState.Value;
        Assert.Equal(window, mine);

        // Settings closes the flyout and brings the window to the front
        settings.Patterns.Invoke.Pattern.Invoke();
        Assert.True(Retry.WhileNotNull(FindFlyout, TimeSpan.FromSeconds(3)).Success, "the flyout didn't close");
        Assert.True(Retry.WhileFalse(() => app.Window.Properties.NativeWindowHandle.Value == GetForegroundWindow(), TimeSpan.FromSeconds(3)).Success, "the window didn't come to the front");
    }

    [Fact]
    public void AddSiteOpensItsBoxInTheFlyout()
    {
        var flyout = OpenFlyout();
        Assert.NotNull(flyout.FindFirstDescendant(c => c.ByName("Excluded sites")));
        flyout.FindFirstDescendant(c => c.ByName("Add site").And(c.ByControlType(ControlType.Button))).Patterns.Invoke.Pattern.Invoke();

        // Opened, not used: nothing is excluded
        var box = Retry.WhileNull(() => flyout.FindFirstDescendant(c => c.ByName("Site to exclude")), TimeSpan.FromSeconds(3)).Result;
        Assert.NotNull(box);
        Assert.NotNull(flyout.FindFirstDescendant(c => c.ByName("Add").And(c.ByControlType(ControlType.Button))));

        // While it's open, Add site is Cancel, which closes the box
        var cancel = flyout.FindFirstDescendant(c => c.ByName("Cancel").And(c.ByControlType(ControlType.Button)));
        Assert.NotNull(cancel);
        cancel.Patterns.Invoke.Pattern.Invoke();
        Assert.True(Retry.WhileNotNull(() => flyout.FindFirstDescendant(c => c.ByName("Site to exclude")), TimeSpan.FromSeconds(3)).Success, "Cancel didn't close the box");
        Assert.NotNull(flyout.FindFirstDescendant(c => c.ByName("Add site").And(c.ByControlType(ControlType.Button))));

        // Esc with the box open closes only the box
        flyout.FindFirstDescendant(c => c.ByName("Add site").And(c.ByControlType(ControlType.Button))).Patterns.Invoke.Pattern.Invoke();
        Retry.WhileNull(() => flyout.FindFirstDescendant(c => c.ByName("Site to exclude")), TimeSpan.FromSeconds(3));
        Keyboard.Type(VirtualKeyShort.ESCAPE);
        Assert.True(Retry.WhileNotNull(() => flyout.FindFirstDescendant(c => c.ByName("Site to exclude")), TimeSpan.FromSeconds(3)).Success, "Esc didn't close the box");
        Assert.NotNull(FindFlyout());

        Keyboard.Type(VirtualKeyShort.ESCAPE);
        Retry.WhileNotNull(FindFlyout, TimeSpan.FromSeconds(3));
    }

    [Fact]
    public void EscClosesTheFlyout()
    {
        OpenFlyout();
        Keyboard.Type(VirtualKeyShort.ESCAPE);
        Assert.True(Retry.WhileNotNull(FindFlyout, TimeSpan.FromSeconds(3)).Success, "Esc didn't close the flyout");
    }

    // =========================================================================
    // HELPERS
    // =========================================================================

    /// <summary>
    /// Press the saved Open the tray flyout shortcut and wait for the flyout.
    /// </summary>
    private AutomationElement OpenFlyout()
    {
        var text = FlyoutShortcut();
        Assert.True(Hotkey.TryParse(text, out var hotkey), "Set a shortcut for \"Open the tray flyout\" in Coffee Filter to run this test.");

        // The reopen guard ignores a press right after the flyout closed
        Thread.Sleep(400);
        var modifiers = new List<VirtualKeyShort>();
        if (hotkey.Modifiers.HasFlag(HotkeyModifiers.Win))
        {
            modifiers.Add(VirtualKeyShort.LWIN);
        }
        if (hotkey.Modifiers.HasFlag(HotkeyModifiers.Ctrl))
        {
            modifiers.Add(VirtualKeyShort.CONTROL);
        }
        if (hotkey.Modifiers.HasFlag(HotkeyModifiers.Alt))
        {
            modifiers.Add(VirtualKeyShort.ALT);
        }
        if (hotkey.Modifiers.HasFlag(HotkeyModifiers.Shift))
        {
            modifiers.Add(VirtualKeyShort.SHIFT);
        }
        using (Keyboard.Pressing([.. modifiers]))
        {
            Keyboard.Type((VirtualKeyShort)hotkey.Key);
        }

        return Retry.WhileNull(FindFlyout, TimeSpan.FromSeconds(5)).Result
            ?? throw new InvalidOperationException($"{text} didn't open the flyout.");
    }

    /// <summary>
    /// A shortcut's picker button, by its automation ID ("Shortcut" and the action).
    /// </summary>
    private AutomationElement Picker(string action) =>
        Retry.WhileNull(() => app.Window.FindFirstDescendant(c => c.ByAutomationId($"Shortcut{action}")), TimeSpan.FromSeconds(5)).Result
        ?? throw new InvalidOperationException($"No picker for {action}.");

    /// <summary>
    /// The flyout's window, while it's showing (a hidden window isn't listed).
    /// </summary>
    private AutomationElement? FindFlyout() =>
        app.Window.Automation.GetDesktop().FindFirstChild(c => c.ByName("Coffee Filter flyout"));

    /// <summary>
    /// The saved shortcut, or the default when it was never changed.
    /// </summary>
    private static string FlyoutShortcut()
    {
        var file = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "CoffeeFilter", "app.json");
        using var json = JsonDocument.Parse(File.ReadAllText(file));
        return json.RootElement.TryGetProperty("Shortcuts", out var shortcuts) && shortcuts.TryGetProperty("OpenFlyout", out var saved)
            ? saved.GetString() ?? ""
            : "Win+Alt+C";
    }

    [System.Runtime.InteropServices.DllImport("user32.dll")]
    private static extern nint GetForegroundWindow();
}
