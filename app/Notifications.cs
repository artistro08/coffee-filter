using Microsoft.Windows.AppNotifications;
using Microsoft.Windows.AppNotifications.Builder;

namespace CoffeeFilter;

/// <summary>
/// Windows notifications (Windows App SDK app notifications), adapted from MagicBuds'
/// AppNotificationService. Each one carries the page a click opens ("open" = "main" or
/// "problems"). While the app runs, clicks arrive at <see cref="Invoked"/>; when it isn't
/// running, Windows starts it with the same arguments (see App.ReadLink). If Windows refuses
/// to register, notifications are skipped and everything else keeps working.
/// </summary>
/// <seealso href="https://learn.microsoft.com/windows/apps/develop/notifications/app-notifications/app-notifications-quickstart"/>
public sealed class Notifications
{
    public const string OpenArgument = "open";

    private bool _registered;

    /// <summary>
    /// A notification was clicked, with the page it opens. Raised on a background thread.
    /// </summary>
    public event Action<string>? Invoked;

    public void Register()
    {
        try
        {
            // Handle clicks in this process, so a click doesn't start a second copy
            AppNotificationManager.Default.NotificationInvoked += (_, args) =>
                Invoked?.Invoke(args.Arguments.TryGetValue(OpenArgument, out var page) ? page : "main");
            AppNotificationManager.Default.Register();
            _registered = true;
        }
        catch (Exception ex)
        {
            App.LogError(ex);
        }
    }

    /// <param name="open">Page a click opens: "main" or "problems".</param>
    /// <param name="tag">A newer notification with the same tag replaces the older one.</param>
    public void Show(string title, string message, string open = "main", string? tag = null)
    {
        if (!_registered)
        {
            return;
        }
        try
        {
            var builder = new AppNotificationBuilder()
                .AddArgument(OpenArgument, open)
                .AddText(title);
            if (message.Length > 0)
            {
                builder.AddText(message);
            }
            var notification = builder.BuildNotification();
            if (tag is not null)
            {
                notification.Tag = tag;
            }
            AppNotificationManager.Default.Show(notification);
        }
        catch (Exception ex)
        {
            App.LogError(ex);
        }
    }

    public void Unregister()
    {
        if (_registered)
        {
            AppNotificationManager.Default.Unregister();
            _registered = false;
        }
    }
}
