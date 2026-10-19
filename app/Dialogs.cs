using Microsoft.UI.Xaml.Controls;

namespace CoffeeFilter;

/// <summary>
/// Opening dialogs safely. WinUI allows only one ContentDialog at a time, and one that was
/// just closed still counts while it animates away, so opening another right after fails with
/// "Only a single ContentDialog can be open at any time". Every dialog in the app opens
/// through here.
/// </summary>
public static class Dialogs
{
    /// <summary>
    /// Open a dialog, waiting out one that's still closing for up to two seconds, then passing
    /// the error on.
    /// </summary>
    public static async Task<ContentDialogResult> ShowAsync(ContentDialog dialog)
    {
        const int AsyncNotStarted = unchecked((int)0x80000019);
        for (var attempt = 0; ; attempt++)
        {
            try
            {
                return await dialog.ShowAsync();
            }
            catch (System.Runtime.InteropServices.COMException ex) when (ex.HResult == AsyncNotStarted && attempt < 20)
            {
                await Task.Delay(100);
            }
        }
    }
}
