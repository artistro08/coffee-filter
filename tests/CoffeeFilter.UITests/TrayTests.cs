using System.Diagnostics;
using System.Runtime.InteropServices;
using FlaUI.Core.Definitions;
using FlaUI.Core.Tools;
using Microsoft.Win32;

namespace CoffeeFilter.UITests;

/// <summary>
/// The tray icon keeps its place across updates: Windows only carries it to the new install
/// folder when the icon's ID stays the same and the exe is signed by the same publisher.
/// Also checks the Show tray icon switch really hides and shows it, putting it back after.
/// </summary>
[Collection("App")]
public sealed class TrayTests(AppFixture app)
{
    private const string TrayIconId = "{2DB25E80-4F5F-404A-B185-79860FEAB53E}";

    [Fact]
    public void InstalledExeIsSignedAndTheIconUsesTheFixedId()
    {
        Assert.NotNull(app.Window);
        var folder = InstallLocation();

        // Signed by the package's publisher
#pragma warning disable SYSLIB0057 // Only reads who signed it
        var signer = System.Security.Cryptography.X509Certificates.X509Certificate.CreateFromSignedFile(Path.Combine(folder, "CoffeeFilter.exe"));
#pragma warning restore SYSLIB0057
        Assert.Equal("CN=Devin Green", signer.Subject);

        // Windows saved the running version's icon under the fixed ID
        using var settings = Registry.CurrentUser.OpenSubKey(@"Control Panel\NotifyIconSettings")!;
        var installed = Path.GetFileName(folder);
        var entry     = settings.GetSubKeyNames()
            .Select(name => settings.OpenSubKey(name))
            .FirstOrDefault(key => (key?.GetValue("ExecutablePath") as string)?.Contains(installed, StringComparison.OrdinalIgnoreCase) == true);
        Assert.NotNull(entry);
        Assert.Equal(TrayIconId, entry.GetValue("IconGuid") as string, ignoreCase: true);
    }

    [Fact]
    public void TurningTheTrayIconOffHidesItAndOnBringsItBack()
    {
        app.GoHome();
        var toggle = Retry.WhileNull(() => app.Window.FindAllDescendants(c => c.ByName("Show tray icon")).FirstOrDefault(e => e.Patterns.Toggle.IsSupported), TimeSpan.FromSeconds(5)).Result
            ?? throw new InvalidOperationException("No Show tray icon switch.");
        bool On() => toggle.Patterns.Toggle.Pattern.ToggleState.Value == ToggleState.On;
        Assert.True(On(), "Show tray icon is off; AppFixture turns it on for the run");
        Assert.True(IconShown(), "the tray icon isn't showing before the test");
        try
        {
            toggle.Patterns.Toggle.Pattern.Toggle();
            Assert.True(Retry.WhileFalse(() => !IconShown(), TimeSpan.FromSeconds(3)).Success, "the tray icon is still showing after turning it off");
        }
        finally
        {
            // Back the way it was
            if (!On())
            {
                toggle.Patterns.Toggle.Pattern.Toggle();
            }
        }
        Assert.True(Retry.WhileFalse(IconShown, TimeSpan.FromSeconds(3)).Success, "the tray icon didn't come back after turning it on");
    }

    // Asks Windows where the icon is; that fails while it's hidden or gone
    private static bool IconShown()
    {
        var id = new NotifyIconIdentifier { Size = (uint)Marshal.SizeOf<NotifyIconIdentifier>(), Guid = Guid.Parse(TrayIconId) };
        return Shell_NotifyIconGetRect(ref id, out _) == 0;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct NotifyIconIdentifier
    {
        public uint   Size;
        public IntPtr Window;
        public uint   Id;
        public Guid   Guid;
    }

    [DllImport("shell32.dll")]
    private static extern int Shell_NotifyIconGetRect(ref NotifyIconIdentifier id, out Rect rect);

    [StructLayout(LayoutKind.Sequential)]
    private struct Rect
    {
        public int Left, Top, Right, Bottom;
    }

    private static string InstallLocation()
    {
        var info = new ProcessStartInfo("powershell.exe", "-NoProfile -Command \"(Get-AppxPackage Artistro08.CoffeeFilter).InstallLocation\"")
        {
            RedirectStandardOutput = true,
            UseShellExecute        = false,
            CreateNoWindow         = true,
        };
        using var process = Process.Start(info)!;
        var folder = process.StandardOutput.ReadToEnd().Trim();
        process.WaitForExit();
        return folder;
    }
}
