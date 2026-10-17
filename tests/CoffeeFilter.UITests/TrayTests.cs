using System.Diagnostics;
using Microsoft.Win32;

namespace CoffeeFilter.UITests;

/// <summary>
/// The tray icon keeps its place across updates: Windows only carries it to the new install
/// folder when the icon's ID stays the same and the exe is signed by the same publisher.
/// Looks only; changes nothing.
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
