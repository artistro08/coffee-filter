using System.Diagnostics;
using System.Text.Json.Nodes;
using CommunityToolkit.WinUI.Controls;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Media.Imaging;

namespace CoffeeFilter;

/// <summary>
/// Exclusions: sites that are never decrypted or filtered (added here, in bulk, or from the
/// blocked page's Exclude button) and apps whose connections never are (a program file, or a
/// Microsoft Store app from a searchable list).
/// </summary>
public sealed partial class MainWindow
{
    private string? _exclusionsKey;
    private string? _appsKey;
    private List<string> _exclusions = [];

    /// <summary>
    /// A program or Store app picker is open. Only one can be at a time: a second Store app
    /// dialog would fail, and a second file picker would stack on the first.
    /// </summary>
    private bool _pickingApp;

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
        var items = new List<object>();

        // Bulk Add
        var bulk = new Button { Content = "Bulk add" };
        bulk.Click += OnBulkExclusionsClicked;
        items.Add(Row(new SettingsCard { Header = "Add several sites", Description = "Paste one site per line.", Content = bulk }));

        // Empty State
        if (exclusions.Count == 0)
        {
            items.Add(Row(new SettingsCard { Header = "No excluded sites" }));
        }

        foreach (var site in exclusions)
        {
            var delete = new Button { Content = "Delete" };
            AutomationProperties.SetName(delete, $"Delete {site}");
            delete.Click += async (_, _) => await SaveExclusionsAsync(_exclusions.Where(h => h != site));
            items.Add(Row(new SettingsCard { Header = site, Content = delete }));
        }
        ReplaceItems(ExclusionsCard, items);
    }

    private async Task SaveExclusionsAsync(IEnumerable<string> hosts)
    {
        ExclusionsCard.IsExpanded = true;
        await _app.RunAsync("set_exclusions", new JsonObject { ["hosts"] = new JsonArray([.. hosts.Select(h => (JsonNode)h)]) }, lists: true);
    }

    private async void OnAddExclusionClicked(object sender, RoutedEventArgs e) => await AddSitesAsync(bulk: false);

    private async void OnBulkExclusionsClicked(object sender, RoutedEventArgs e) => await AddSitesAsync(bulk: true);

    /// <summary>
    /// Ask for sites in a dialog (one site, or one per line for bulk adding) and add them.
    /// </summary>
    private Task AddSitesAsync(bool bulk) => GuardAsync(async () =>
    {
        var box = new TextBox
        {
            Header          = bulk ? "One site per line" : "Site",
            PlaceholderText = bulk ? "mybank.com\nwork.example.com" : "mybank.com",
            AcceptsReturn   = bulk,
            MinHeight       = bulk ? 160 : 0,
            MinWidth        = 400,
        };
        var dialog = Dialog(bulk ? "Add sites" : "Add a site", box, "Add");
        if (await Dialogs.ShowAsync(dialog) != ContentDialogResult.Primary)
        {
            return;
        }

        var sites = box.Text.Split(['\r', '\n'], StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries);
        if (sites.Length > 0)
        {
            await SaveExclusionsAsync(_exclusions.Concat(sites));
        }
    });

    /// <summary>
    /// Ask to exclude a site, from the blocked page's Exclude button. Like an install link, the
    /// window jumps in front of what you were doing, so Exclude is held back for a moment.
    /// </summary>
    public async Task ExcludeSiteAsync(string host)
    {
        await ShowInFrontAsync();

        var message = new TextBlock { Text = $"{host} will never be decrypted or filtered, so its ads and trackers will load.", TextWrapping = TextWrapping.Wrap, MaxWidth = 400 };
        var dialog  = Dialog("Exclude this site?", message, "Exclude");
        HoldPrimary(dialog);

        // Only one dialog can be open at a time
        var result = ContentDialogResult.None;
        await GuardAsync(async () => result = await Dialogs.ShowAsync(dialog));
        if (result != ContentDialogResult.Primary)
        {
            return;
        }

        // Added by the core to its saved list, so a list this window hasn't loaded yet isn't lost
        ExclusionsCard.IsExpanded = true;
        await _app.RunAsync("add_exclusion", new JsonObject { ["host"] = host }, lists: true);
    }

    // =========================================================================
    // EXCLUDED APPS
    // =========================================================================

    /// <summary>
    /// One card per excluded app (its name, and its path or Store package), each with Delete.
    /// </summary>
    private void BuildExcludedApps(List<ExcludedApp> apps)
    {
        var key = string.Join("\n", apps.Select(a => a.Id));
        if (key == _appsKey)
        {
            return;
        }
        _appsKey = key;
        var items = new List<object>();

        // Empty State
        if (apps.Count == 0)
        {
            items.Add(Row(new SettingsCard { Header = "No excluded apps" }));
            ReplaceItems(ExcludedAppsCard, items);
            return;
        }

        foreach (var app in apps)
        {
            var delete = new Button { Content = "Delete" };
            AutomationProperties.SetName(delete, $"Delete {app.Name}");
            delete.Click += async (_, _) => await _app.RunAsync("remove_excluded_app", new JsonObject { ["id"] = app.Id }, lists: true);
            var card = new SettingsCard
            {
                Header      = app.Name,
                Description = app.Package is null ? app.Path ?? "" : $"Microsoft Store app · {app.Package}",
                HeaderIcon  = new FontIcon { Glyph = app.Package is null ? "" : "" },
                Content     = delete,
            };
            items.Add(Row(card));
            _ = ShowAppIconAsync(card, app);
        }
        ReplaceItems(ExcludedAppsCard, items);
    }

    /// <summary>
    /// Swap a row's placeholder symbol for the app's real icon: a Store app's package logo, or
    /// the icon Windows shows for a program file. Both lookups are asynchronous, so the window
    /// doesn't wait on them; a missing app or icon just keeps the symbol.
    /// </summary>
    private static async Task ShowAppIconAsync(SettingsCard card, ExcludedApp app)
    {
        try
        {
            ImageSource? icon = null;
            if (app.Package is { } family)
            {
                var logo = await Task.Run(() => new Windows.Management.Deployment.PackageManager().FindPackagesForUser("", family).FirstOrDefault()?.Logo);
                if (logo is not null)
                {
                    icon = new BitmapImage(logo) { DecodePixelWidth = 40 };
                }
            }
            else if (app.Path is { } path)
            {
                var file = await Windows.Storage.StorageFile.GetFileFromPathAsync(path);
                using var thumbnail = await file.GetThumbnailAsync(Windows.Storage.FileProperties.ThumbnailMode.SingleItem, 40);
                if (thumbnail is not null)
                {
                    var bitmap = new BitmapImage();
                    await bitmap.SetSourceAsync(thumbnail);
                    icon = bitmap;
                }
            }
            if (icon is not null)
            {
                card.HeaderIcon = new ImageIcon { Source = icon, Width = 20, Height = 20 };
            }
        }
        catch (Exception ex)
        {
            // Uninstalled, unreadable, or an odd image: the placeholder symbol stays
            if (ex is not (IOException or UnauthorizedAccessException or ArgumentException or System.Runtime.InteropServices.COMException))
            {
                App.LogError(ex);
            }
        }
    }

    /// <summary>
    /// Add app from the tray flyout: the main page, Excluded apps opened, and the picker.
    /// </summary>
    public async Task PickExcludedAppAsync(bool store)
    {
        await ShowInFrontAsync();

        // A picker already open (asked for twice from the flyout) stays the one on screen
        if (_pickingApp)
        {
            return;
        }
        GoBack();
        ExcludedAppsCard.IsExpanded = true;
        ExcludedAppsCard.StartBringIntoView();
        await (store ? PickStoreAppAsync() : PickProgramAsync());
    }

    private async Task AddExcludedAppAsync(string name, string? path, string? package)
    {
        ExcludedAppsCard.IsExpanded = true;
        await _app.RunAsync("add_excluded_app", new JsonObject { ["name"] = name, ["path"] = path, ["package"] = package }, lists: true);
    }

    /// <summary>
    /// Pick a program file. Its name comes from the file's own description (what Task Manager shows).
    /// </summary>
    private async void OnAddProgramClicked(object sender, RoutedEventArgs e) => await PickProgramAsync();

    private async Task PickProgramAsync()
    {
        if (_pickingApp)
        {
            return;
        }
        _pickingApp = true;

        try
        {
            await GuardAsync(async () =>
            {
                var picker = new Microsoft.Windows.Storage.Pickers.FileOpenPicker(AppWindow.Id)
                {
                    SuggestedStartLocation = Microsoft.Windows.Storage.Pickers.PickerLocationId.ComputerFolder,
                    CommitButtonText       = "Exclude",
                };
                picker.FileTypeFilter.Add(".exe");

                var file = await picker.PickSingleFileAsync();
                if (file is null)
                {
                    return;
                }
                var description = FileVersionInfo.GetVersionInfo(file.Path).FileDescription;
                var name        = string.IsNullOrWhiteSpace(description) ? Path.GetFileNameWithoutExtension(file.Path) : description.Trim();
                await AddExcludedAppAsync(name, file.Path, null);
            });
        }
        finally
        {
            _pickingApp = false;
        }
    }

    /// <summary>
    /// Pick from your installed Microsoft Store (MSIX) apps, with search. Apps are matched by
    /// package family name, so the exclusion keeps working after the app updates.
    ///
    /// The dialog opens right away with a loading circle; reading every installed package can
    /// take a few seconds and runs off the UI thread, with a time limit in case one package hangs.
    /// Each app's row is built once in code and added to the list as is: list items made from
    /// data (templates, bound or plain names) came out blank or crashed in the AOT build.
    /// </summary>
    private async void OnAddStoreAppClicked(object sender, RoutedEventArgs e) => await PickStoreAppAsync();

    private async Task PickStoreAppAsync()
    {
        if (_pickingApp)
        {
            return;
        }

        var search  = new TextBox { PlaceholderText = "Search apps", IsEnabled = false };
        AutomationProperties.SetName(search, "Search apps");
        var loading = new ProgressRing { IsActive = true, Width = 32, Height = 32, Margin = new Thickness(0, 48, 0, 48) };
        var message = new TextBlock { TextWrapping = TextWrapping.Wrap, Visibility = Visibility.Collapsed, Margin = new Thickness(0, 24, 0, 24) };
        var list    = new ListView
        {
            Height        = 360,
            SelectionMode = ListViewSelectionMode.Single,
            Visibility    = Visibility.Collapsed,
        };
        AutomationProperties.SetName(list, "Installed apps");

        var panel = new StackPanel { Spacing = 8, Width = 420 };
        panel.Children.Add(search);
        panel.Children.Add(loading);
        panel.Children.Add(message);
        panel.Children.Add(list);

        var dialog = Dialog("Exclude a Microsoft Store app", panel, "Exclude");
        dialog.IsPrimaryButtonEnabled = false;

        // One row per installed app (icon and name), made once and reused by the search; the
        // list holds these rows directly, so there's no data to translate for the list control
        var rows = new List<(StoreApp App, ListViewItem Row)>();
        void Show(string filter)
        {
            list.Items.Clear();
            foreach (var (app, row) in rows.Where(r => r.App.Name.Contains(filter, StringComparison.OrdinalIgnoreCase)))
            {
                list.Items.Add(row);
            }
        }
        StoreApp? Chosen() => list.SelectedItem is ListViewItem { Tag: string family } ? rows.FirstOrDefault(r => r.App.Family == family).App : null;

        // Kept Up To Date As You Pick
        // The list's selection is gone once the dialog has closed, so it's read while the dialog is open
        StoreApp? chosen = null;

        ListViewItem MakeRow(StoreApp app)
        {
            var icon = new Image { Width = 24, Height = 24 };
            if (app.Logo is not null)
            {
                // Decodes in the background, at the size shown
                icon.Source = new BitmapImage(app.Logo) { DecodePixelWidth = 48 };
            }
            var content = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 12, Padding = new Thickness(0, 4, 0, 4) };
            content.Children.Add(icon);
            content.Children.Add(new TextBlock { Text = app.Name, VerticalAlignment = VerticalAlignment.Center });

            var row = new ListViewItem { Content = content, Tag = app.Family };
            AutomationProperties.SetName(row, app.Name);
            return row;
        }
        list.SelectionChanged += (_, _) => dialog.IsPrimaryButtonEnabled = (chosen = Chosen()) is not null;

        // Load While Open
        dialog.Opened += async (_, _) =>
        {
            var load = Task.Run(InstalledStoreApps);
            if (await Task.WhenAny(load, Task.Delay(TimeSpan.FromSeconds(20))) != load || load.IsFaulted)
            {
                loading.IsActive   = false;
                loading.Visibility = Visibility.Collapsed;
                message.Text       = "Windows didn't return your installed apps. Try again, or add the app's program file instead.";
                message.Visibility = Visibility.Visible;
                return;
            }
            rows.AddRange(load.Result.Select(app => (app, MakeRow(app))));
            Show("");
            loading.IsActive   = false;
            loading.Visibility = Visibility.Collapsed;
            list.Visibility    = Visibility.Visible;
            search.IsEnabled   = true;
            search.Focus(FocusState.Programmatic);
        };
        search.TextChanged += (_, _) => Show(search.Text);

        // Only one dialog can be open at a time
        _pickingApp = true;
        try
        {
            await GuardAsync(async () =>
            {
                if (await Dialogs.ShowAsync(dialog) != ContentDialogResult.Primary || chosen is null)
                {
                    return;
                }
                await AddExcludedAppAsync(chosen.Name, null, chosen.Family);
            });
        }
        finally
        {
            _pickingApp = false;
        }
    }

    private sealed record StoreApp(string Name, string Family, Uri? Logo);

    /// <summary>
    /// Installed Store apps for this account that you can open from Start (no frameworks,
    /// resource packs, add-on bundles, or hidden Windows parts), one per package family, sorted by name.
    /// </summary>
    private static List<StoreApp> InstalledStoreApps()
    {
        var found = new Dictionary<string, StoreApp>();
        foreach (var package in new Windows.Management.Deployment.PackageManager().FindPackagesForUser(""))
        {
            try
            {
                if (package.IsFramework || package.IsResourcePackage || package.IsBundle)
                {
                    continue;
                }

                // Only apps you can open from Start, like the app picker in Windows Settings (this
                // leaves out Windows' own parts, such as App Resolver or AsyncTextService)
                if (package.GetAppListEntries().Count == 0)
                {
                    continue;
                }
                var name = package.DisplayName;
                if (string.IsNullOrWhiteSpace(name) || name.StartsWith("ms-resource:", StringComparison.Ordinal))
                {
                    continue;
                }
                found[package.Id.FamilyName] = new StoreApp(name, package.Id.FamilyName, package.Logo);
            }
            catch (Exception ex) when (ex is ArgumentException or System.Runtime.InteropServices.COMException or FileNotFoundException)
            {
                // A package that's mid-install or broken: leave it out
            }
        }
        return [.. found.Values.OrderBy(a => a.Name, StringComparer.CurrentCultureIgnoreCase)];
    }
}
