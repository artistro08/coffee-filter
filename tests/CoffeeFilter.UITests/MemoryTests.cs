using FlaUI.Core.Tools;
using Xunit.Abstractions;

namespace CoffeeFilter.UITests;

/// <summary>
/// Closing the window frees it. Each closed window used to stay in memory (about 12 MB every
/// time you opened and closed it), and the app kept about 160 MB of RAM in the tray after the
/// window had been open once. Leaves the window open again at the end.
/// </summary>
[Collection("App")]
public sealed class MemoryTests(AppFixture app, ITestOutputHelper output)
{
    private const double MB = 1024 * 1024;

    [Fact]
    public void ClosingTheWindowFreesItsMemory()
    {
        try
        {
            // Back in the tray, the app hands its RAM back to Windows
            app.CloseWindow();
            Assert.True(Retry.WhileFalse(() => WorkingSetMB() < 60, TimeSpan.FromSeconds(10)).Success,
                $"the app still uses {WorkingSetMB():N0} MB of RAM in the tray after closing the window");

            // One reopen first: WinUI sets up a few MB the first time a window opens again,
            // once, so it isn't counted as growth
            app.ReopenWindow();
            app.CloseWindow();
            Thread.Sleep(3000);
            var first = PrivateMB();

            // Opening and closing it again doesn't keep adding memory (4 more closed windows
            // used to add about 48 MB)
            List<string> each = [];
            for (var i = 0; i < 4; i++)
            {
                app.ReopenWindow();
                app.CloseWindow();
                Thread.Sleep(3000);
                each.Add($"{PrivateMB() - first:N0}");
            }
            var grown = PrivateMB() - first;
            output.WriteLine($"first {first:N0} MB, growth after each: {string.Join(", ", each)}");
            Assert.True(grown < 20, $"memory grew {grown:N0} MB over 4 more opens and closes of the window (after each: {string.Join(", ", each)})");
        }
        finally
        {
            app.ReopenWindow();
        }
    }

    private double PrivateMB()
    {
        app.AppProcess.Refresh();
        return app.AppProcess.PrivateMemorySize64 / MB;
    }

    private double WorkingSetMB()
    {
        app.AppProcess.Refresh();
        return app.AppProcess.WorkingSet64 / MB;
    }
}
