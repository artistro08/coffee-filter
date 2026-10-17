using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.Windows.AppLifecycle;

namespace CoffeeFilter;

/// <summary>
/// Single instance entry point. A second launch (or a coffee-filter:// link) hands off
/// to the running app, which shows its window, so only one core ever runs.
/// </summary>
public static partial class Program
{
    [System.Runtime.InteropServices.LibraryImport("user32.dll")]
    [return: System.Runtime.InteropServices.MarshalAs(System.Runtime.InteropServices.UnmanagedType.Bool)]
    private static partial bool AllowSetForegroundWindow(uint processId);

    [STAThread]
    private static int Main()
    {
        WinRT.ComWrappersSupport.InitializeComWrappers();

        var instance = AppInstance.FindOrRegisterForKey("CoffeeFilter");
        if (!instance.IsCurrent)
        {
            // This launch is the one Windows lets come to the front; pass that on, so the window
            // (and an install dialog) opens on top instead of behind the browser
            AllowSetForegroundWindow(instance.ProcessId);
            var args = AppInstance.GetCurrent().GetActivatedEventArgs();
            Task.Run(() => instance.RedirectActivationToAsync(args).AsTask()).Wait();
            return 0;
        }
        instance.Activated += (_, e) => App.ActivateFromAnyThread(e);

        Application.Start(p =>
        {
            SynchronizationContext.SetSynchronizationContext(new DispatcherQueueSynchronizationContext(DispatcherQueue.GetForCurrentThread()));
            _ = new App();
        });
        return 0;
    }
}
