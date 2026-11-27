using System.Text.Json.Nodes;
using CommunityToolkit.WinUI.Controls;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;

namespace CoffeeFilter;

/// <summary>
/// Filter lists (grouped by kind, each with an on/off switch) and userscripts.
/// </summary>
public sealed partial class MainWindow
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

    // Which lists and userscripts are on screen (added or removed ones rebuild their cards)
    private string _listsKey   = "";
    private string _scriptsKey = "";

    // The latest lists, and each group's expander, so a group opened later shows them as they are now
    private List<FilterList> _lists = [];
    private readonly Dictionary<string, SettingsExpander> _listGroups = [];

    // On/off switches by name, so turning one on or off updates it in place
    private readonly Dictionary<string, ToggleSwitch> _listSwitches   = [];
    private readonly Dictionary<string, ToggleSwitch> _scriptSwitches = [];

    // =========================================================================
    // FILTER LIST CARDS
    // =========================================================================

    /// <summary>
    /// One expander per group. A group's cards are only created when it's opened, so the 60
    /// list cards don't exist until someone looks at them. Only a list added or removed rebuilds
    /// the groups; turning one on or off updates its switch and its group's count in place, so
    /// the switch you pressed stays where it is, with keyboard focus.
    /// </summary>
    private void BuildListGroups(List<FilterList> lists)
    {
        _lists = lists;
        var key = string.Join("\n", lists.Select(l => $"{l.Kind}|{l.Name}|{l.Url}"));
        if (key == _listsKey)
        {
            foreach (var (kind, expander) in _listGroups)
            {
                expander.Description = OnCount(GroupMembers(lists, kind));
            }
            SyncSwitches(_listSwitches, lists.Select(l => (l.Name, l.Enabled)));
            return;
        }
        _listsKey = key;
        _listGroups.Clear();
        _listSwitches.Clear();
        ListGroups.Children.Clear();

        foreach (var (kind, label, glyph) in Groups)
        {
            var members = GroupMembers(lists, kind);
            if (members.Count == 0)
            {
                continue;
            }

            var expander = new SettingsExpander
            {
                Header      = label,
                Description = OnCount(members),
                HeaderIcon  = new FontIcon { Glyph = glyph },
            };
            OneRow(expander);
            void Fill()
            {
                if (expander.Items.Count > 0)
                {
                    return;
                }
                ReplaceItems(expander, [.. GroupMembers(_lists, kind).Select(list => (object)ListCard(list.Name, list.Url, list.Enabled, kind == "custom", "set_list", "remove_list", _listSwitches))]);
            }
            expander.Expanded  += (_, _) => { Expanded.Add(kind); Fill(); };
            expander.Collapsed += (_, _) => Expanded.Remove(kind);

            if (Expanded.Contains(kind))
            {
                Fill();
                expander.IsExpanded = true;
            }
            _listGroups[kind] = expander;
            ListGroups.Children.Add(expander);
        }
    }

    /// <summary>
    /// A group's lists; kinds without a group of their own go under Custom.
    /// </summary>
    private static List<FilterList> GroupMembers(List<FilterList> lists, string kind) =>
        [.. lists.Where(l => l.Kind == kind || (kind == "custom" && !Groups.Any(g => g.Kind == l.Kind)))];

    private static string OnCount(List<FilterList> members) => $"{members.Count(l => l.Enabled)} of {members.Count} on";

    /// <summary>
    /// Set each switch already on screen to the core's state, without sending it back.
    /// </summary>
    private void SyncSwitches(Dictionary<string, ToggleSwitch> switches, IEnumerable<(string Name, bool Enabled)> states)
    {
        _updating = true;
        foreach (var (name, enabled) in states)
        {
            if (switches.TryGetValue(name, out var toggle) && toggle.IsOn != enabled)
            {
                toggle.IsOn = enabled;
            }
        }
        _updating = false;
    }

    /// <summary>
    /// A filter list or userscript: on/off switch, plus a remove button for ones the user added.
    /// </summary>
    private SettingsCard ListCard(string name, string url, bool enabled, bool removable, string setCommand, string removeCommand, Dictionary<string, ToggleSwitch> switches)
    {
        var toggle = new ToggleSwitch { IsOn = enabled };
        AutomationProperties.SetName(toggle, name);
        toggle.Toggled += async (_, _) =>
        {
            if (_updating)
            {
                return;
            }
            await _app.RunAsync(setCommand, new JsonObject { ["name"] = name, ["enabled"] = toggle.IsOn }, lists: true);
        };
        switches[name] = toggle;

        var content = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        if (removable)
        {
            var remove = new Button { Content = new FontIcon { Glyph = "", FontSize = 14 } };
            ToolTipService.SetToolTip(remove, "Remove");
            AutomationProperties.SetName(remove, $"Remove {name}");
            remove.Click += async (_, _) => await _app.RunAsync(removeCommand, new JsonObject { ["name"] = name }, lists: true);
            content.Children.Add(remove);
        }
        content.Children.Add(toggle);

        return Row(new SettingsCard { Header = name, Description = url, Content = content });
    }

    private async void OnUpdateClicked(object sender, RoutedEventArgs e) =>
        await Busy.RunAsync(UpdateButton, UpdateRing, () => _app.RunAsync("update_lists", lists: true));

    private async void OnAddListClicked(object sender, RoutedEventArgs e) => await GuardAsync(async () =>
    {
        var name  = new TextBox { Header = "Name", PlaceholderText = "My list" };
        var url   = new TextBox { Header = "Web address", PlaceholderText = "https://example.com/list.txt" };
        var panel = new StackPanel { Spacing = 12, MinWidth = 400 };
        panel.Children.Add(name);
        panel.Children.Add(url);

        var dialog = Dialog("Add a filter list", panel, "Add");
        if (await Dialogs.ShowAsync(dialog) == ContentDialogResult.Primary)
        {
            Expanded.Add("custom");
            await Busy.RunAsync(AddListButton, AddListRing, () => _app.RunAsync("add_list", new JsonObject { ["name"] = name.Text, ["url"] = url.Text }, lists: true));
        }
    });

    // =========================================================================
    // USERSCRIPT CARDS
    // =========================================================================

    /// <summary>
    /// One card per userscript. Like the filter lists, only a script added or removed rebuilds them.
    /// </summary>
    private void BuildUserscripts(List<Userscript> scripts)
    {
        var key = string.Join("\n", scripts.Select(s => $"{s.Name}|{s.Url}"));
        if (key == _scriptsKey)
        {
            SyncSwitches(_scriptSwitches, scripts.Select(s => (s.Name, s.Enabled)));
            return;
        }
        _scriptsKey = key;
        _scriptSwitches.Clear();
        var items = new List<object>();

        foreach (var script in scripts)
        {
            items.Add(ListCard(script.Name, script.Url, script.Enabled, true, "set_userscript", "remove_userscript", _scriptSwitches));
        }
        ReplaceItems(UserscriptsCard, items);
    }

    private async void OnAddUserscriptClicked(object sender, RoutedEventArgs e) => await AddUserscriptAsync("", fromLink: false);

    /// <summary>
    /// Ask to add a userscript, with the address filled in when it came from a site's Install button.
    /// From a link, Add is held back for a moment (see HoldPrimary).
    /// </summary>
    public async Task AddUserscriptAsync(string address, bool fromLink)
    {
        await ShowInFrontAsync();

        var url = new TextBox { Header = "Web address of the .user.js file", PlaceholderText = "https://greasyfork.org/scripts/…/script.user.js", Text = address, MinWidth = 400 };
        var dialog = Dialog("Add a userscript", url, "Add");
        if (fromLink)
        {
            HoldPrimary(dialog);
        }

        // Only one dialog can be open at a time
        var result = ContentDialogResult.None;
        await GuardAsync(async () => result = await Dialogs.ShowAsync(dialog));
        if (result != ContentDialogResult.Primary)
        {
            return;
        }
        await Busy.RunAsync(AddScriptButton, AddScriptRing, () => _app.RunAsync("add_userscript", new JsonObject { ["url"] = url.Text }, lists: true));
    }
}
