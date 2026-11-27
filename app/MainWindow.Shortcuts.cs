using CommunityToolkit.WinUI.Controls;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;

namespace CoffeeFilter;

/// <summary>
/// The Shortcuts group: one picker per global shortcut.
/// </summary>
public sealed partial class MainWindow
{
    // One row per shortcut: its card (for the warning) and its picker
    private readonly Dictionary<ShortcutAction, (SettingsCard Card, ShortcutControl Control)> _shortcutRows = [];

    // Why a shortcut isn't what was picked (the new one was taken and the old one is back); cleared on the next change
    private readonly Dictionary<ShortcutAction, string> _shortcutNotes = [];

    // The shortcut when its dialog opened, to restore if the new one turns out taken
    private string? _shortcutBefore;

    /// <summary>
    /// One card per shortcut with PowerToys' shortcut picker, as in MagicBuds and Leaf Calendar:
    /// the keys as key caps (or Assign shortcut), and a dialog to press new ones in (Reset picks
    /// the default, Clear turns it off). A combination Windows or another app holds gets a
    /// warning in place of the card's description.
    /// </summary>
    private void BuildShortcuts()
    {
        if (_shortcutRows.Count == 0)
        {
            var items = new List<object>();
            foreach (var action in Shortcuts.All)
            {
                var control = new ShortcutControl();
                var card    = Row(new SettingsCard { Header = Shortcuts.NameFor(action), Content = control });
                WireShortcut(control, action);
                _shortcutRows[action] = (card, control);
                items.Add(card);
            }
            ReplaceItems(ShortcutsCard, items);
        }

        foreach (var (action, (card, control)) in _shortcutRows)
        {
            var text = Shortcuts.Get(_app.Settings, action);
            control.HotkeySettings = Hotkey.TryParse(text, out var hotkey) ? HotkeySettings.FromHotkey(hotkey) : new HotkeySettings();

            var taken   = _app.Shortcuts.IsTaken(action);
            var message = _shortcutNotes.TryGetValue(action, out var note) ? note
                : taken ? $"Windows or another app is using {text}. Pick a different shortcut."
                : null;
            control.HasConflict = taken;
            control.Tooltip     = message;
            card.Description    = message is null ? null! : new TextBlock
            {
                Text         = message,
                Foreground   = (Brush)Application.Current.Resources["SystemFillColorCautionBrush"],
                TextWrapping = TextWrapping.Wrap,
            };
        }
    }

    // Wires A Picker To Its Shortcut: the default for Reset, the conflict check, and saving
    private void WireShortcut(ShortcutControl control, ShortcutAction action)
    {
        control.ButtonName            = Shortcuts.NameFor(action);
        control.ButtonAutomationId    = $"Shortcut{action}";
        control.DefaultHotkeySettings = Hotkey.TryParse(Shortcuts.DefaultFor(action), out var fallback) ? HotkeySettings.FromHotkey(fallback) : new HotkeySettings();
        control.CheckConflict         = hotkey => ShortcutConflict(action, hotkey);

        // Coffee Filter's own shortcuts let go while the dialog listens (so pressing them reaches it)
        control.DialogOpening += (_, _) =>
        {
            _shortcutBefore = Shortcuts.Get(_app.Settings, action);
            _shortcutNotes.Remove(action);
            _app.Shortcuts.Suspend();
        };
        control.DialogClosed += (_, _) => ShortcutDialogClosed(action);
        control.HotkeySettingsChanged += (_, _) =>
        {
            // A New Combination, The Default (Reset), Or "" (cleared)
            var picked = control.HotkeySettings is { } settings && !settings.IsEmpty() ? settings.ToHotkey()?.ToString() : "";
            if (picked is null)
            {
                return;
            }
            _app.Settings.Shortcuts[action.ToString()] = picked;
            _app.Settings.Save();
        };
    }

    // Another shortcut has it, or Windows or another app holds it (a RegisterHotKey probe)
    private string? ShortcutConflict(ShortcutAction action, Hotkey hotkey)
    {
        var other = Shortcuts.All.FirstOrDefault(each => each != action && Hotkey.TryParse(Shortcuts.Get(_app.Settings, each), out var used) && used == hotkey);
        return other != default ? $"“{Shortcuts.NameFor(other)}” already uses {hotkey}."
            : !_app.Shortcuts.IsFree(hotkey) ? $"Windows or another app is using {hotkey}. Try a different one."
            : null;
    }

    // The shortcuts come back from the settings after, whichever way the dialog closed. A new combination another
    // app took since the dialog checked it gives way to the one that worked before.
    private void ShortcutDialogClosed(ShortcutAction action)
    {
        _app.Shortcuts.Apply(_app.Settings);

        var saved    = Shortcuts.Get(_app.Settings, action);
        var previous = _shortcutBefore ?? saved;
        if (_app.Shortcuts.IsTaken(action) && saved != previous)
        {
            _shortcutNotes[action] = $"{saved} was taken by another app, so {(previous.Length == 0 ? "no shortcut" : previous)} is back.";
            _app.Settings.Shortcuts[action.ToString()] = previous;
            _app.Settings.Save();
            _app.Shortcuts.Apply(_app.Settings);
        }
        BuildShortcuts();
    }
}
