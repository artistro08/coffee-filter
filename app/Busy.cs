using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace CoffeeFilter;

/// <summary>
/// Slow work behind a button, like Windows Settings: the button is off and a loading circle
/// spins beside it until the work is done. Used by the window and the tray flyout.
/// </summary>
public static class Busy
{
    public static async Task RunAsync(Button button, ProgressRing ring, Func<Task> work)
    {
        button.IsEnabled = false;
        ring.Visibility  = Visibility.Visible;
        ring.IsActive    = true;
        try
        {
            await work();
        }
        finally
        {
            ring.IsActive    = false;
            ring.Visibility  = Visibility.Collapsed;
            button.IsEnabled = true;
        }
    }
}
