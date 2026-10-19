using CoffeeFilter;

namespace CoffeeFilter.Tests;

/// <summary>
/// coffee-filter:// links, which any web page can make: only well-formed ones open a dialog,
/// and a site name has to look like one before it's shown.
/// </summary>
public sealed class AppLinkTests
{
    [Theory]
    [InlineData("?host=example.com", "example.com")]
    [InlineData("?host=*.Example.COM.", "example.com")]
    [InlineData("host=%20news.example.org%20", "news.example.org")]
    [InlineData("?host=a-b.example.co.uk", "a-b.example.co.uk")]
    [InlineData("?host=first.com&host=second.com", "first.com")]
    public void ExcludeTakesASiteName(string query, string expected)
    {
        Assert.Equal(new AppLink("exclude", expected), AppLink.Parse("exclude", query));
    }

    [Theory]
    [InlineData("?host=localhost")]
    [InlineData("?host=Click%20OK%20to%20continue.com")]
    [InlineData("?host=evil.com/path")]
    [InlineData("?host=evil.com%3Cscript%3E")]
    [InlineData("?host=")]
    [InlineData("?site=example.com")]
    [InlineData("")]
    public void ExcludeRefusesAnythingElse(string query)
    {
        Assert.Null(AppLink.Parse("exclude", query));
    }

    [Fact]
    public void ExcludeRefusesOverlongNames()
    {
        Assert.Null(AppLink.Parse("exclude", "?host=" + new string('a', 250) + ".com"));
    }

    [Fact]
    public void InstallKeepsTheAddressAsIs()
    {
        var url = "https://example.com/My Script.user.js?v=1";
        Assert.Equal(new AppLink("install", url), AppLink.Parse("install", "?url=" + Uri.EscapeDataString(url)));
        Assert.Null(AppLink.Parse("install", "?url="));
    }

    [Theory]
    [InlineData("install_cert")]
    [InlineData("problems")]
    [InlineData("")]
    public void OtherLinksOpenNothing(string action)
    {
        Assert.Null(AppLink.Parse(action, "?url=https://example.com/a.user.js&host=example.com"));
    }
}
