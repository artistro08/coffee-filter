// From MagicBuds (src/MagicBuds.Presentation/Shortcuts/Hotkey.cs), by way of Leaf Calendar
// (src/LeafCalendar.Core/Tray/Hotkey.cs). F12, Space, and Delete are allowed, as in MagicBuds; Win is written first.

using System.Globalization;

namespace CoffeeFilter;

/// <summary>
/// Modifier keys of a global shortcut. The values are Win32's <c>MOD_*</c> flags, so the app hands them to <c>RegisterHotKey</c> as they are.
/// </summary>
[Flags]
public enum HotkeyModifiers
{
    None  = 0,
    Alt   = 0x1,
    Ctrl  = 0x2,
    Shift = 0x4,
    Win   = 0x8,
}

/// <summary>
/// A global shortcut such as Ctrl+Alt+J: modifiers plus one key, a Win32 virtual-key code for A–Z, 0–9, F1–F24, Space,
/// or Delete.
/// </summary>
/// <remarks>
/// At least one of Ctrl, Alt, or Win is required, so a shortcut never swallows plain typing (Shift+J is a capital J).
/// Text is always written Win, Ctrl, Alt, Shift, then the key (the order Windows writes its own shortcuts in), which is
/// how settings store it.
/// </remarks>
public readonly record struct Hotkey(HotkeyModifiers Modifiers, int Key)
{
    private const HotkeyModifiers AllModifiers = HotkeyModifiers.Ctrl | HotkeyModifiers.Alt | HotkeyModifiers.Shift | HotkeyModifiers.Win;
    private const HotkeyModifiers Anchors      = HotkeyModifiers.Ctrl | HotkeyModifiers.Alt | HotkeyModifiers.Win;

    /// <summary>
    /// Checks a pressed combination (the shortcut dialog) and makes a shortcut from it.
    /// </summary>
    public static bool TryCreate(HotkeyModifiers modifiers, int key, out Hotkey hotkey)
    {
        hotkey = new Hotkey(modifiers, key);
        return (modifiers & Anchors) != 0
            && (modifiers & ~AllModifiers) == 0
            && KeyName(key) is not null;
    }

    /// <summary>
    /// Reads text such as "Ctrl+Alt+J": any order, any case, "Control" and "Windows" work too.
    /// </summary>
    public static bool TryParse(string? text, out Hotkey hotkey)
    {
        hotkey = default;
        if (string.IsNullOrWhiteSpace(text))
        {
            return false;
        }

        var modifiers = HotkeyModifiers.None;
        int? key      = null;
        foreach (var raw in text.Split('+'))
        {
            var part = raw.Trim().ToUpperInvariant();
            HotkeyModifiers? modifier = part switch
            {
                "CTRL" or "CONTROL" => HotkeyModifiers.Ctrl,
                "ALT" => HotkeyModifiers.Alt,
                "SHIFT" => HotkeyModifiers.Shift,
                "WIN" or "WINDOWS" => HotkeyModifiers.Win,
                _ => null,
            };

            // A Modifier (each only once)
            if (modifier is { } m)
            {
                if ((modifiers & m) != 0)
                {
                    return false;
                }
                modifiers |= m;
                continue;
            }

            // The One Key
            if (key is not null || KeyCode(part) is not { } code)
            {
                return false;
            }
            key = code;
        }

        return key is { } k && TryCreate(modifiers, k, out hotkey);
    }

    /// <summary>
    /// The keys one by one, in order ("Ctrl", "Alt", "J"), for showing each on its own key cap.
    /// </summary>
    public IEnumerable<string> Parts()
    {
        if (Modifiers.HasFlag(HotkeyModifiers.Win))
        {
            yield return "Win";
        }
        if (Modifiers.HasFlag(HotkeyModifiers.Ctrl))
        {
            yield return "Ctrl";
        }
        if (Modifiers.HasFlag(HotkeyModifiers.Alt))
        {
            yield return "Alt";
        }
        if (Modifiers.HasFlag(HotkeyModifiers.Shift))
        {
            yield return "Shift";
        }
        yield return KeyName(Key) ?? "?";
    }

    public override string ToString() => string.Join("+", Parts());

    // A–Z and 0–9 are their own virtual-key codes; F1–F24 are 0x70–0x87
    internal static string? KeyName(int key) => key switch
    {
        0x20 => "Space",
        0x2E => "Delete",
        >= 0x41 and <= 0x5A => new string((char)key, 1),
        >= 0x30 and <= 0x39 => new string((char)key, 1),
        >= 0x70 and <= 0x87 => string.Create(CultureInfo.InvariantCulture, $"F{key - 0x6F}"),
        _ => null,
    };

    private static int? KeyCode(string name)
    {
        // Space And Delete (the name is upper-cased)
        if (name is "SPACE" or "DELETE" or "DEL")
        {
            return name == "SPACE" ? 0x20 : 0x2E;
        }
        if (name.Length == 1 && (char.IsAsciiLetterUpper(name[0]) || char.IsAsciiDigit(name[0])))
        {
            return name[0];
        }
        if (name.Length is 2 or 3 && name[0] == 'F' && int.TryParse(name.AsSpan(1), NumberStyles.None, CultureInfo.InvariantCulture, out var n) && n is >= 1 and <= 24)
        {
            return 0x6F + n;
        }
        return null;
    }
}
