using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;

namespace CoffeeFilter;

/// <summary>
/// What a global shortcut does. The values are the hotkey IDs registered with Windows.
/// </summary>
public enum ShortcutAction
{
    OpenFlyout     = 1,
    ToggleBlocking = 2,
    UpdateLists    = 3,
    OpenApp        = 4,
}

/// <summary>
/// Global shortcuts, registered with <c>RegisterHotKey</c> on a hidden message-only window whose
/// <c>WM_HOTKEY</c> raises <see cref="Pressed"/>. Adapted from MagicBuds' and Leaf Calendar's
/// GlobalShortcuts. A combination Windows or another app holds can't be taken: it's remembered
/// as taken, and Settings warns about it. <c>MOD_NOREPEAT</c> keeps a held combination from
/// firing over and over. Use from the UI thread.
/// </summary>
/// <author>Devin Green (Artistro08)</author>
/// <seealso href="https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-registerhotkey"/>
public sealed partial class Shortcuts
{
    public static readonly ShortcutAction[] All = [ShortcutAction.OpenFlyout, ShortcutAction.ToggleBlocking, ShortcutAction.UpdateLists, ShortcutAction.OpenApp];

    /// <summary>
    /// The setting's name, as Settings and the picker's dialog show it.
    /// </summary>
    public static string NameFor(ShortcutAction action) => action switch
    {
        ShortcutAction.OpenFlyout     => "Open the tray flyout",
        ShortcutAction.ToggleBlocking => "Turn ad blocking on or off",
        ShortcutAction.UpdateLists    => "Update filter lists",
        _                             => "Open Coffee Filter",
    };

    /// <summary>
    /// Only the flyout has a shortcut out of the box; the rest are yours to pick.
    /// </summary>
    public static string DefaultFor(ShortcutAction action) => action == ShortcutAction.OpenFlyout ? "Win+Alt+C" : "";

    // A spare ID for checking whether a combination is free
    private const int ProbeId = 0x7FFF;

    private static Shortcuts? s_instance;
    private readonly HashSet<ShortcutAction> _registered = [];
    private readonly HashSet<ShortcutAction> _taken      = [];
    private readonly nint _hwnd;

    /// <summary>
    /// A shortcut was pressed.
    /// </summary>
    public event Action<ShortcutAction>? Pressed;

    public unsafe Shortcuts()
    {
        s_instance = this;
        fixed (char* name = "CoffeeFilterShortcuts")
        {
            var windowClass = new WNDCLASSEXW
            {
                cbSize        = (uint)sizeof(WNDCLASSEXW),
                lpfnWndProc   = (nint)(delegate* unmanaged[Stdcall]<nint, uint, nint, nint, nint>)&WndProc,
                hInstance     = GetModuleHandleW(null),
                lpszClassName = (nint)name,
            };
            RegisterClassExW(windowClass);
            _hwnd = CreateWindowExW(0, "CoffeeFilterShortcuts", "", 0, 0, 0, 0, 0, HwndMessage, 0, windowClass.hInstance, 0);
        }
    }

    /// <summary>
    /// The saved combination for an action: the default until changed, "" once removed.
    /// </summary>
    public static string Get(AppSettings settings, ShortcutAction action) =>
        settings.Shortcuts.TryGetValue(action.ToString(), out var text) ? text : DefaultFor(action);

    /// <summary>
    /// Registers the saved shortcuts again (on start, after a change, after the picker closes).
    /// </summary>
    public void Apply(AppSettings settings)
    {
        Suspend();
        _taken.Clear();

        // Without its window nothing could receive a shortcut
        if (_hwnd == 0)
        {
            return;
        }
        foreach (var action in All)
        {
            if (!Hotkey.TryParse(Get(settings, action), out var hotkey))
            {
                continue;
            }
            if (RegisterHotKey(_hwnd, (int)action, Modifiers(hotkey), (uint)hotkey.Key))
            {
                _registered.Add(action);
            }
            else
            {
                _taken.Add(action);
            }
        }
    }

    /// <summary>
    /// Lets go of every shortcut (while the picker listens for keys, so it can pick one of ours).
    /// </summary>
    public void Suspend()
    {
        foreach (var action in _registered)
        {
            UnregisterHotKey(_hwnd, (int)action);
        }
        _registered.Clear();
    }

    /// <summary>
    /// True when Windows or another app holds the saved combination for this action.
    /// </summary>
    public bool IsTaken(ShortcutAction action) => _taken.Contains(action);

    /// <summary>
    /// True when Windows would let Coffee Filter register this combination now.
    /// </summary>
    public bool IsFree(Hotkey hotkey)
    {
        if (!RegisterHotKey(_hwnd, ProbeId, Modifiers(hotkey), (uint)hotkey.Key))
        {
            return false;
        }
        UnregisterHotKey(_hwnd, ProbeId);
        return true;
    }

    private static uint Modifiers(Hotkey hotkey) => (uint)hotkey.Modifiers | ModNoRepeat;

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvStdcall)])]
    private static nint WndProc(nint hwnd, uint message, nint wParam, nint lParam)
    {
        if (message == WmHotkey && Enum.IsDefined((ShortcutAction)(int)wParam))
        {
            // No exception may leave a native callback; it would end the app
            try
            {
                s_instance?.Pressed?.Invoke((ShortcutAction)(int)wParam);
            }
            catch (Exception ex)
            {
                App.LogError(ex);
            }
            return 0;
        }
        return DefWindowProcW(hwnd, message, wParam, lParam);
    }

    // =========================================================================
    // WIN32
    // =========================================================================

    private const uint WmHotkey    = 0x0312;
    private const uint ModNoRepeat = 0x4000;
    private const nint HwndMessage = -3;

    [StructLayout(LayoutKind.Sequential)]
    private struct WNDCLASSEXW
    {
        public uint cbSize;
        public uint style;
        public nint lpfnWndProc;
        public int cbClsExtra;
        public int cbWndExtra;
        public nint hInstance;
        public nint hIcon;
        public nint hCursor;
        public nint hbrBackground;
        public nint lpszMenuName;
        public nint lpszClassName;
        public nint hIconSm;
    }

    [LibraryImport("user32.dll")]
    private static partial ushort RegisterClassExW(in WNDCLASSEXW windowClass);

    [LibraryImport("user32.dll", StringMarshalling = StringMarshalling.Utf16)]
    private static partial nint CreateWindowExW(uint exStyle, string className, string windowName, uint style, int x, int y, int width, int height, nint parent, nint menu, nint instance, nint param);

    [LibraryImport("user32.dll")]
    private static partial nint DefWindowProcW(nint hwnd, uint message, nint wParam, nint lParam);

    [LibraryImport("kernel32.dll", StringMarshalling = StringMarshalling.Utf16)]
    private static partial nint GetModuleHandleW(string? name);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool RegisterHotKey(nint hwnd, int id, uint modifiers, uint key);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool UnregisterHotKey(nint hwnd, int id);
}

/// <summary>
/// A low-level keyboard hook (<c>WH_KEYBOARD_LL</c>) for the shortcut picker. While it runs,
/// every key the callback takes is swallowed, so Windows and other apps never act on it
/// (Win+letter, Alt+Tab). Ported from PowerToys' KeyboardHook by way of MagicBuds and Leaf
/// Calendar: a static [UnmanagedCallersOnly] procedure (no delegate for the GC to collect),
/// one hook at a time, and no exception may leave it (a failing callback lets the key through).
/// Runs on the UI thread that started it.
/// </summary>
/// <seealso href="https://github.com/microsoft/PowerToys/blob/main/src/common/interop/KeyboardHook.cpp"/>
public static unsafe partial class KeyCapture
{
    private static nint s_hook;

    /// <summary>
    /// Gets each key press and release (virtual-key code, down, injected by software, the
    /// sender's extra info); returns true to swallow it.
    /// </summary>
    private static Func<int, bool, bool, nuint, bool>? s_callback;

    /// <summary>
    /// Starts listening (replacing a hook already running). False when Windows refused it.
    /// </summary>
    public static bool Start(Func<int, bool, bool, nuint, bool> callback)
    {
        Stop();
        s_callback = callback;
        s_hook     = SetWindowsHookExW(WhKeyboardLl, &HookProc, GetModuleHandleW(0), 0);
        return s_hook != 0;
    }

    public static void Stop()
    {
        if (s_hook != 0)
        {
            UnhookWindowsHookEx(s_hook);
            s_hook = 0;
        }
        s_callback = null;
    }

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvStdcall)])]
    private static nint HookProc(int code, nint wParam, nint lParam)
    {
        try
        {
            if (code == 0 && s_callback is { } callback)
            {
                var info = (KBDLLHOOKSTRUCT*)lParam;
                var down = wParam is WmKeyDown or WmSysKeyDown;
                if (callback((int)info->vkCode, down, (info->flags & LlkhfInjected) != 0, info->dwExtraInfo))
                {
                    return 1;
                }
            }
        }
        catch (Exception ex)
        {
            App.LogError(ex);
        }
        return CallNextHookEx(0, code, wParam, lParam);
    }

    private const int WhKeyboardLl  = 13;
    private const nint WmKeyDown    = 0x0100;
    private const nint WmSysKeyDown = 0x0104;
    private const uint LlkhfInjected = 0x10;

    [StructLayout(LayoutKind.Sequential)]
    private struct KBDLLHOOKSTRUCT
    {
        public uint vkCode;
        public uint scanCode;
        public uint flags;
        public uint time;
        public nuint dwExtraInfo;
    }

    [LibraryImport("user32.dll")]
    private static partial nint SetWindowsHookExW(int hookId, delegate* unmanaged[Stdcall]<int, nint, nint, nint> proc, nint module, uint threadId);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool UnhookWindowsHookEx(nint hook);

    [LibraryImport("user32.dll")]
    private static partial nint CallNextHookEx(nint hook, int code, nint wParam, nint lParam);

    [LibraryImport("kernel32.dll")]
    private static partial nint GetModuleHandleW(nint name);
}
