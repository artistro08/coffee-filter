using CoffeeFilter;

namespace CoffeeFilter.Tests;

/// <summary>
/// The text global shortcuts are saved as, read back and written out.
/// </summary>
public sealed class HotkeyTests
{
    [Theory]
    [InlineData("Win+Alt+C", "Win+Alt+C")]
    [InlineData("alt + ctrl + j", "Ctrl+Alt+J")]
    [InlineData("Control+Shift+F12", "Ctrl+Shift+F12")]
    [InlineData("Windows+Space", "Win+Space")]
    [InlineData("alt+win+c", "Win+Alt+C")]
    [InlineData("Ctrl+Del", "Ctrl+Delete")]
    public void ReadsAndWritesInOneOrder(string text, string expected)
    {
        Assert.True(Hotkey.TryParse(text, out var hotkey));
        Assert.Equal(expected, hotkey.ToString());
    }

    [Theory]
    [InlineData("")]
    [InlineData("J")]
    [InlineData("Shift+J")]
    [InlineData("Ctrl+Ctrl+J")]
    [InlineData("Ctrl+J+K")]
    [InlineData("Ctrl+F25")]
    [InlineData("Ctrl+Tab")]
    public void RefusesWhatCantBeAShortcut(string text) => Assert.False(Hotkey.TryParse(text, out _));

    [Fact]
    public void PartsAreOneKeyCapEach()
    {
        Assert.True(Hotkey.TryParse("Ctrl+Alt+Shift+Win+5", out var hotkey));
        Assert.Equal(["Win", "Ctrl", "Alt", "Shift", "5"], hotkey.Parts());
    }
}
