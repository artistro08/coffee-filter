using FlaUI.Core.AutomationElements;
using FlaUI.Core.Definitions;
using FlaUI.Core.Tools;

namespace CoffeeFilter.UITests;

/// <summary>
/// Walks through the whole window the way a person would: every section, both sub-pages,
/// every group, and the Add app dialogs (always canceled).
/// </summary>
[Collection("App")]
public sealed class WindowTests(AppFixture app)
{
    // =========================================================================
    // MAIN PAGE
    // =========================================================================

    [Fact]
    public void MainPageShowsEverySection()
    {
        app.GoHome();
        foreach (var section in new[] { "Protection", "Filter lists", "Userscripts", "Exclusions", "App", "About" })
        {
            Assert.NotNull(app.Find(section));
        }
        foreach (var card in new[] { "Ad blocking", "Certificate", "Connection problems", "Update lists", "Add a filter list", "Show tray icon", "Quit Coffee Filter" })
        {
            Assert.NotNull(app.Find(card));
        }
    }

    [Fact]
    public void CardsKeepTheirButtonOnTheSameRowAtTheSmallestWidth()
    {
        app.GoHome();

        // Asking for a tiny size leaves the window at its minimum width
        var before = app.Window.BoundingRectangle;
        app.Window.Patterns.Transform.Pattern.Resize(100, before.Height);
        Thread.Sleep(800);
        try
        {
            foreach (var (header, button) in new[] { ("Update lists", "Update now"), ("Add a filter list", "Add list"), ("Certificate", "Remove") })
            {
                var text    = app.Window.FindAllDescendants(c => c.ByName(header)).First(e => e.ControlType == ControlType.Text);
                var control = app.Window.FindAllDescendants(c => c.ByName(button).And(c.ByControlType(ControlType.Button))).FirstOrDefault()
                    ?? app.Window.FindAllDescendants(c => c.ByName("Install").Or(c.ByName("Renew")).And(c.ByControlType(ControlType.Button))).First();
                text.Patterns.ScrollItem.PatternOrDefault?.ScrollIntoView();
                Thread.Sleep(200);

                // Side by side: the button starts right of the text and overlaps it vertically
                var t = text.BoundingRectangle;
                var b = control.BoundingRectangle;
                Assert.True(b.Left > t.Right, $"{button} should be right of \"{header}\" (text {t}, button {b})");
                Assert.True(b.Top < t.Bottom + 24 && b.Bottom > t.Top - 24, $"{button} should be on the same row as \"{header}\" (text {t}, button {b})");
            }
        }
        finally
        {
            app.Window.Patterns.Transform.Pattern.Resize(before.Width, before.Height);
        }
    }

    [Fact]
    public void EveryButtonAndSwitchHasASpokenName()
    {
        app.GoHome();
        app.Expand("General");
        app.Expand("Excluded apps");

        var controls = app.Window.FindAllDescendants(c => c.ByControlType(ControlType.Button))
            .Where(e => e.Properties.IsOffscreen.ValueOrDefault == false || e.Properties.ClassName.ValueOrDefault == "ToggleSwitch")
            .ToList();
        var unnamed = controls.Where(e => e.Properties.Name.IsSupported && string.IsNullOrWhiteSpace(e.Properties.Name.ValueOrDefault)).Select(e => e.Properties.ClassName.ValueOrDefault).ToList();
        Assert.True(unnamed.Count == 0, $"controls a screen reader can't name: {string.Join(", ", unnamed)}");
        Assert.NotNull(app.FindButton("Add app"));
    }

    // =========================================================================
    // SUB-PAGES
    // =========================================================================

    [Fact]
    public void AdBlockingOpensTheLiveLogAndBackReturns()
    {
        app.GoHome();
        app.FindButton("Ad blocking").Patterns.Invoke.Pattern.Invoke();

        Assert.NotNull(app.Find("Blocked requests show up here as they happen, newest first."));
        Assert.NotNull(app.FindButton("Clear"));
        Assert.Null(app.Window.FindFirstDescendant(c => c.ByName("Filter lists")));

        app.GoHome();
        Assert.NotNull(app.Find("Filter lists"));
    }

    [Fact]
    public void ConnectionProblemsOpensItsPageAndBackReturns()
    {
        app.GoHome();
        app.FindButton("Connection problems").Patterns.Invoke.Pattern.Invoke();

        Assert.NotNull(app.Find("Apps that couldn't connect"));
        var problems = app.Window.FindFirstDescendant(c => c.ByName("No connection problems"))
            ?? app.Window.FindFirstDescendant(c => c.ByName("Exclude").And(c.ByControlType(ControlType.Button)));
        Assert.NotNull(problems);

        app.GoHome();
        Assert.NotNull(app.Find("Protection"));
    }

    // =========================================================================
    // GROUPS
    // =========================================================================

    [Fact]
    public void FilterListGroupsShowTheirLists()
    {
        app.GoHome();
        var group = app.Expand("General");
        var lists = Retry.WhileEmpty(() => group.FindAllDescendants(c => c.ByControlType(ControlType.Button).And(c.ByClassName("ToggleSwitch"))), TimeSpan.FromSeconds(5)).Result ?? [];
        Assert.NotEmpty(lists);
    }

    [Fact]
    public void ExcludedSitesHasBulkAddInsideTheGroup()
    {
        app.GoHome();
        var group = app.Expand("Excluded sites");
        Assert.NotNull(group.FindFirstDescendant(c => c.ByName("Add several sites")));
        Assert.NotNull(group.FindFirstDescendant(c => c.ByName("Bulk add").And(c.ByControlType(ControlType.Button))));
        Assert.NotNull(app.FindButton("Add site"));
    }

    [Fact]
    public void ExcludedAppsShowsItsListOrEmptyState()
    {
        app.GoHome();
        var group = app.Expand("Excluded apps");
        var any   = Retry.WhileNull(
            () => group.FindFirstDescendant(c => c.ByName("No excluded apps"))
                ?? group.FindAllDescendants(c => c.ByControlType(ControlType.Button)).FirstOrDefault(b => b.Properties.Name.ValueOrDefault?.StartsWith("Delete ") == true),
            TimeSpan.FromSeconds(5)).Result;
        Assert.NotNull(any);

        // Excluded apps show their real icons (a Store app's logo, a program's file icon), not just a symbol
        if (any.Properties.Name.ValueOrDefault?.StartsWith("Delete ") == true)
        {
            var icon = Retry.WhileNull(() => group.FindFirstDescendant(c => c.ByControlType(ControlType.Image)), TimeSpan.FromSeconds(5)).Result;
            Assert.True(icon is { BoundingRectangle.Width: > 0 }, "no excluded app shows its icon");
        }
    }

    [Fact]
    public void AboutNamesTheAuthor()
    {
        app.GoHome();
        app.Expand("Coffee Filter");
        Assert.NotNull(app.Find("Built by Devin Green (Artistro08)"));
    }

    // =========================================================================
    // ADD APP DIALOGS
    // =========================================================================

    [Fact]
    public void StoreAppListShowsNamedAppsWithIcons()
    {
        app.GoHome();
        app.Expand("Excluded apps");
        OpenAddAppMenu();
        Retry.WhileNull(() => FindAnywhere("Microsoft Store app…"), TimeSpan.FromSeconds(5)).Result!.Patterns.Invoke.Pattern.Invoke();

        var dialog = Retry.WhileNull(() => FindAnywhere("Exclude a Microsoft Store app"), TimeSpan.FromSeconds(10)).Result;
        Assert.NotNull(dialog);
        try
        {
            var list  = Retry.WhileNull(() => app.Window.FindFirstDescendant(c => c.ByName("Installed apps")), TimeSpan.FromSeconds(25)).Result!;
            var items = Retry.WhileTrue(() => list.FindAllChildren(c => c.ByControlType(ControlType.ListItem)).Length < 3, TimeSpan.FromSeconds(25)).Success
                ? list.FindAllChildren(c => c.ByControlType(ControlType.ListItem))
                : [];
            Assert.True(items.Length >= 3, $"expected several installed apps, got {items.Length}");
            Thread.Sleep(800);
            items = list.FindAllChildren(c => c.ByControlType(ControlType.ListItem));

            // Every visible row has a name, and the rows show icons
            foreach (var item in items.Take(8))
            {
                Assert.False(string.IsNullOrWhiteSpace(item.Properties.Name.ValueOrDefault), "a row has no name");
                var text = item.FindFirstDescendant(c => c.ByControlType(ControlType.Text));
                Assert.False(string.IsNullOrWhiteSpace(text?.Properties.Name.ValueOrDefault), $"row \"{item.Properties.Name.ValueOrDefault}\" shows no name");
            }
            Retry.WhileTrue(() => items.Take(8).Count(i => i.FindFirstDescendant(c => c.ByControlType(ControlType.Image)) is { BoundingRectangle.Width: > 0 }) == 0, TimeSpan.FromSeconds(5));
            Assert.True(items.Take(8).Any(i => i.FindFirstDescendant(c => c.ByControlType(ControlType.Image)) is { BoundingRectangle.Width: > 0 }), "no row shows an icon");

            // Only apps you can open from Start, like the Settings app picker: Windows' hidden parts stay out
            var names = list.FindAllChildren(c => c.ByControlType(ControlType.ListItem)).Select(i => i.Properties.Name.ValueOrDefault).ToList();
            Assert.DoesNotContain("App Resolver", names);
            Assert.DoesNotContain("AsyncTextService", names);

            // Choosing an app turns on Exclude (canceled below, so nothing is saved)
            items[0].Patterns.SelectionItem.Pattern.Select();
            var exclude = Retry.WhileNull(() => FindAnywhere("Exclude"), TimeSpan.FromSeconds(5)).Result;
            Assert.True(Retry.WhileFalse(() => exclude?.IsEnabled == true, TimeSpan.FromSeconds(5)).Success, "Exclude stays off after choosing an app");
            Assert.NotNull(app.Window.FindFirstDescendant(c => c.ByName("Installed apps")));

            // Scrolling builds more rows without trouble
            var scroll = list.Patterns.Scroll.PatternOrDefault;
            if (scroll is { VerticallyScrollable.Value: true })
            {
                scroll.SetScrollPercent(-1, 100);
                Thread.Sleep(600);
                scroll.SetScrollPercent(-1, 0);
                Thread.Sleep(400);
            }

            // Search narrows the list
            var first  = items[0].Properties.Name.ValueOrDefault ?? "";
            var search = app.Window.FindFirstDescendant(c => c.ByControlType(ControlType.Edit))!.AsTextBox();
            search.Text = first;
            Retry.WhileTrue(() => list.FindAllChildren(c => c.ByControlType(ControlType.ListItem)).Length >= items.Length, TimeSpan.FromSeconds(5));
            Assert.Contains(list.FindAllChildren(c => c.ByControlType(ControlType.ListItem)), i => i.Properties.Name.ValueOrDefault == first);
        }
        finally
        {
            CancelDialog();
        }
    }

    [Fact]
    public void AddSiteDialogOpensAndCancels()
    {
        app.GoHome();
        app.FindButton("Add site").Patterns.Invoke.Pattern.Invoke();
        Assert.NotNull(Retry.WhileNull(() => FindAnywhere("Add a site"), TimeSpan.FromSeconds(5)).Result);
        CancelDialog();
    }

    [Fact]
    public void AddUserscriptDialogOpensAndCancels()
    {
        app.GoHome();
        app.FindButton("Add userscript").Patterns.Invoke.Pattern.Invoke();
        Assert.NotNull(Retry.WhileNull(() => FindAnywhere("Web address of the .user.js file"), TimeSpan.FromSeconds(5)).Result);
        CancelDialog();
    }

    // =========================================================================
    // HELPERS
    // =========================================================================

    /// <summary>
    /// Menus and dialogs open in popups that UI Automation lists under the desktop, not the window.
    /// </summary>
    private AutomationElement? FindAnywhere(string name) =>
        app.Window.FindFirstDescendant(c => c.ByName(name)) ?? app.Window.Automation.GetDesktop().FindFirstDescendant(c => c.ByName(name));

    private void OpenAddAppMenu()
    {
        var button = app.FindButton("Add app");
        if (button.Patterns.ExpandCollapse.IsSupported)
        {
            button.Patterns.ExpandCollapse.Pattern.Expand();
        }
        else
        {
            button.Patterns.Invoke.Pattern.Invoke();
        }
        Thread.Sleep(400);
    }

    private void CancelDialog()
    {
        var cancel = Retry.WhileNull(() => app.Window.FindFirstDescendant(c => c.ByName("Cancel").And(c.ByControlType(ControlType.Button))), TimeSpan.FromSeconds(5)).Result;
        cancel?.Patterns.Invoke.Pattern.Invoke();
        Thread.Sleep(500);
    }
}
