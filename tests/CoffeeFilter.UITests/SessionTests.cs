using System.Runtime.InteropServices;
using FlaUI.Core.Tools;
using Microsoft.Win32;

namespace CoffeeFilter.UITests;

/// <summary>
/// Windows shutting down: the app gets the same message Windows sends before a restart, and
/// must put the proxy settings back before it agrees. Then the shutdown is canceled, and
/// blocking must come back as it was. Your settings end as they began.
/// </summary>
[Collection("App")]
public sealed class SessionTests(AppFixture app)
{
    private const uint WmQueryEndSession = 0x0011;
    private const uint WmEndSession      = 0x0016;

    [Fact]
    public void ShutdownPutsTheProxyBackAndCancelResumesBlocking()
    {
        app.GoHome();
        var hidden = FindWindowW("CoffeeFilterShortcuts", null);
        Assert.True(hidden != 0, "the app's hidden window isn't there");
        var blocking = Pac()?.StartsWith("http://127.0.0.1:") == true;

        try
        {
            // Asked to end: the answer is yes, and by then the proxy setting is ours no more
            Assert.Equal(1, SendMessageW(hidden, WmQueryEndSession, 0, 0));
            Assert.False(Pac()?.StartsWith("http://127.0.0.1:") == true, $"the proxy still points at Coffee Filter: {Pac()}");
        }
        finally
        {
            // Canceled (another app said no): the core starts again
            SendMessageW(hidden, WmEndSession, 0, 0);
        }

        if (blocking)
        {
            var back = Retry.WhileFalse(() => Pac()?.StartsWith("http://127.0.0.1:") == true, TimeSpan.FromSeconds(60)).Success;
            Assert.True(back, "blocking didn't come back after the canceled shutdown");
        }
        Retry.WhileNull(() => System.Diagnostics.Process.GetProcessesByName("coffee-filter-core").FirstOrDefault(), TimeSpan.FromSeconds(10));
        Assert.NotEmpty(System.Diagnostics.Process.GetProcessesByName("coffee-filter-core"));
    }

    private static string? Pac()
    {
        using var key = Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Internet Settings");
        return key?.GetValue("AutoConfigURL") as string;
    }

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern nint FindWindowW(string className, string? windowName);

    [DllImport("user32.dll")]
    private static extern nint SendMessageW(nint hwnd, uint message, nint wParam, nint lParam);
}
