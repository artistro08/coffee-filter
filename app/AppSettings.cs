using System.Text.Json;
using System.Text.Json.Serialization;

namespace CoffeeFilter;

/// <summary>
/// App-only preferences, saved next to the core's config in %LOCALAPPDATA%\CoffeeFilter\app.json.
/// </summary>
public class AppSettings
{
    private static readonly string FilePath = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "CoffeeFilter", "app.json");

    public bool ShowTrayIcon { get; set; } = true;

    /// <summary>
    /// Whether blocking was on when last changed, so the next launch starts the same way.
    /// </summary>
    public bool BlockingEnabled { get; set; } = true;

    /// <summary>
    /// Window size you last left it at, scale-independent; null until you close it once.
    /// </summary>
    public double? WindowWidth { get; set; }
    public double? WindowHeight { get; set; }

    /// <summary>
    /// App version that last ran the automatic certificate setup.
    /// </summary>
    public string? CertSetupVersion { get; set; }

    /// <summary>
    /// Global shortcuts you changed, by action name ("OpenFlyout": "Win+Alt+C"); "" means
    /// removed. Actions not listed use their default (see Shortcuts.DefaultFor).
    /// </summary>
    public Dictionary<string, string> Shortcuts { get; set; } = [];

    public static AppSettings Load()
    {
        try
        {
            return JsonSerializer.Deserialize(File.ReadAllText(FilePath), AppSettingsJson.Default.AppSettings) ?? new();
        }
        catch (Exception ex) when (ex is IOException or JsonException or UnauthorizedAccessException)
        {
            return new();
        }
    }

    public void Save()
    {
        // Write a temp file and swap it in, so a crash mid-save can't leave half a file
        Directory.CreateDirectory(Path.GetDirectoryName(FilePath)!);
        var temp = FilePath + ".tmp";
        File.WriteAllText(temp, JsonSerializer.Serialize(this, AppSettingsJson.Default.AppSettings));
        File.Move(temp, FilePath, overwrite: true);
    }
}

/// <summary>
/// Compile-time JSON code, so settings work with Native AOT and trimming.
/// </summary>
[JsonSerializable(typeof(AppSettings))]
internal partial class AppSettingsJson : JsonSerializerContext;
