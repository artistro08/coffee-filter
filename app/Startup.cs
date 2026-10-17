using Windows.ApplicationModel;

namespace CoffeeFilter;

/// <summary>
/// Start with Windows: the MSIX startup task declared in Package.appxmanifest, adapted from
/// MagicBuds' StartupTaskService. At sign-in Windows starts the app with the StartupTask
/// activation kind, and it opens in the tray without a window. On by default: the first run
/// turns it on once, and after that it's whatever you set (here, or in Task Manager's Startup
/// apps, which wins: once you turn it off there, only you can turn it back on there).
/// </summary>
/// <seealso href="https://learn.microsoft.com/uwp/api/windows.applicationmodel.startuptask"/>
public static class Startup
{
    private const string TaskId = "CoffeeFilterStartup";

    /// <summary>
    /// Whether it's on, and whether this app may change it (false when it was turned off in
    /// Task Manager, or by policy). Null when Windows can't say.
    /// </summary>
    public static async Task<(bool On, bool CanChange)?> GetAsync()
    {
        try
        {
            var task = await StartupTask.GetAsync(TaskId);
            return task.State switch
            {
                StartupTaskState.Enabled => (true, true),
                StartupTaskState.Disabled => (false, true),
                StartupTaskState.EnabledByPolicy => (true, false),
                _ => (false, false),
            };
        }
        catch (Exception ex)
        {
            App.LogError(ex);
            return null;
        }
    }

    /// <summary>
    /// Turn it on or off; returns the state Windows ended up with.
    /// </summary>
    public static async Task<(bool On, bool CanChange)?> SetAsync(bool on)
    {
        try
        {
            var task = await StartupTask.GetAsync(TaskId);
            if (on)
            {
                await task.RequestEnableAsync();
            }
            else
            {
                task.Disable();
            }
        }
        catch (Exception ex)
        {
            App.LogError(ex);
        }
        return await GetAsync();
    }

    /// <summary>
    /// The first run turns it on, once; later runs leave your choice alone.
    /// </summary>
    public static async Task EnableOnFirstRunAsync(AppSettings settings)
    {
        if (settings.StartupSet)
        {
            return;
        }
        settings.StartupSet = true;
        settings.Save();
        if (await GetAsync() is (false, true))
        {
            await SetAsync(true);
        }
    }
}
