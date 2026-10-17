using System.Text.Json;
using System.Text.Json.Nodes;
using CoffeeFilter;

namespace CoffeeFilter.Tests;

/// <summary>
/// Drives the real coffee-filter-core.exe through CoreClient, the same way the app does,
/// in an isolated temp profile. Never starts the proxy, so no system settings change.
/// </summary>
public sealed class CoreClientTests : IDisposable
{
    private readonly string _profile = Path.Combine(Path.GetTempPath(), $"coffee-filter-tests-{Guid.NewGuid():N}");
    private readonly string? _originalLocalAppData = Environment.GetEnvironmentVariable("LOCALAPPDATA");
    private readonly CoreClient _core;

    public CoreClientTests()
    {
        // Profile with one cached test list (named like the core names it: name plus a hash of
        // its URL) and an empty scriptlet library, so nothing is downloaded and no CA exists
        const string listUrl = "https://invalid.invalid/test.txt";
        var dir  = Path.Combine(_profile, "CoffeeFilter");
        var hash = Convert.ToHexStringLower(System.Security.Cryptography.SHA1.HashData(System.Text.Encoding.UTF8.GetBytes(listUrl)))[..8];
        Directory.CreateDirectory(Path.Combine(dir, "lists"));
        File.WriteAllText(Path.Combine(dir, "lists", $"Test-{hash}.txt"), "||ads.example.com^\n||track.example.com^\n");
        File.WriteAllText(Path.Combine(dir, "scriptlets.json"), """{"version":"0","scriptlets":[]}""");
        File.WriteAllText(Path.Combine(dir, "config.json"), JsonSerializer.Serialize(new
        {
            port       = 18230,
            exclusions = Array.Empty<string>(),
            lists      = new[] { new { name = "Test", url = listUrl, enabled = true, kind = "ads" } },
        }));

        // The child process inherits this, so it reads and writes only the temp profile
        Environment.SetEnvironmentVariable("LOCALAPPDATA", _profile);
        _core = new CoreClient();
    }

    public void Dispose()
    {
        _core.Dispose();
        Environment.SetEnvironmentVariable("LOCALAPPDATA", _originalLocalAppData);
        Directory.Delete(_profile, recursive: true);
    }

    [Fact]
    public async Task ReadyReplyMapsSnakeCaseStatus()
    {
        var reply = await _core.WaitReadyAsync();

        Assert.True(reply.Ok);
        Assert.NotNull(reply.Status);
        Assert.False(reply.Status.Running);
        Assert.False(reply.Status.CertInstalled);
        Assert.Equal(2, reply.Status.Rules);
        Assert.Equal(18230, reply.Status.Port);
        Assert.Equal("ads", Assert.Single(reply.Status.Lists).Kind);
    }

    [Fact]
    public async Task TogglingAListRebuildsRules()
    {
        await _core.WaitReadyAsync();

        var off = await _core.SendAsync("set_list", new JsonObject { ["name"] = "Test", ["enabled"] = false });
        Assert.True(off.Ok);
        Assert.Equal(0, off.Status!.Rules);
        Assert.False(off.Status.Lists[0].Enabled);

        var on = await _core.SendAsync("set_list", new JsonObject { ["name"] = "Test", ["enabled"] = true });
        Assert.Equal(2, on.Status!.Rules);
    }

    [Fact]
    public async Task AddAndRemoveCustomList()
    {
        await _core.WaitReadyAsync();

        var bad = await _core.SendAsync("add_list", new JsonObject { ["name"] = "Bad", ["url"] = "C:\\Windows\\win.ini" });
        Assert.False(bad.Ok);
        Assert.Contains("https://", bad.Error);

        var added = await _core.SendAsync("add_list", new JsonObject { ["name"] = "Mine", ["url"] = "https://invalid.invalid/mine.txt" });
        Assert.True(added.Ok);
        Assert.Contains(added.Status!.Lists, l => l.Name == "Mine" && l.Kind == "custom");

        var duplicate = await _core.SendAsync("add_list", new JsonObject { ["name"] = "Mine", ["url"] = "https://invalid.invalid/other.txt" });
        Assert.False(duplicate.Ok);

        var removed = await _core.SendAsync("remove_list", new JsonObject { ["name"] = "Mine" });
        Assert.DoesNotContain(removed.Status!.Lists, l => l.Name == "Mine");
    }

    [Fact]
    public async Task ExclusionsAreCleanedAndSaved()
    {
        await _core.WaitReadyAsync();

        var reply = await _core.SendAsync("set_exclusions", new JsonObject { ["hosts"] = new JsonArray(" MyBank.com ", "*.work.example", "bad host!", "mybank.com") });

        Assert.Equal(["mybank.com", "work.example"], reply.Status!.Exclusions);
        Assert.False(reply.Ok);
        Assert.Contains("bad host!", reply.Error);
        var saved = File.ReadAllText(Path.Combine(_profile, "CoffeeFilter", "config.json"));
        Assert.Contains("work.example", saved);
    }

    [Fact]
    public async Task UserscriptsNeedHttpsAndExist()
    {
        var ready = await _core.WaitReadyAsync();
        Assert.Empty(ready.Status!.Userscripts);

        var plain = await _core.SendAsync("add_userscript", new JsonObject { ["url"] = "http://example.com/a.user.js" });
        Assert.False(plain.Ok);
        Assert.Contains("https://", plain.Error);

        var unknown = await _core.SendAsync("set_userscript", new JsonObject { ["name"] = "Nope", ["enabled"] = false });
        Assert.False(unknown.Ok);
        Assert.Empty(unknown.Status!.Userscripts);
    }

    [Fact]
    public async Task NonAsciiNamesSurviveTheRoundTrip()
    {
        await _core.WaitReadyAsync();
        const string name = "Café – Liste";

        var added = await _core.SendAsync("add_list", new JsonObject { ["name"] = name, ["url"] = "https://invalid.invalid/cafe.txt" });
        Assert.True(added.Ok, added.Error);
        Assert.Contains(added.Status!.Lists, l => l.Name == name);

        var toggled = await _core.SendAsync("set_list", new JsonObject { ["name"] = name, ["enabled"] = false });
        Assert.True(toggled.Ok, toggled.Error);
    }

    [Fact]
    public async Task LogRepliesWithEntriesOnlyWhenAsked()
    {
        await _core.WaitReadyAsync();

        var log = await _core.SendAsync("log", new JsonObject { ["after"] = 0 });
        Assert.True(log.Ok);
        Assert.NotNull(log.Log);
        Assert.Empty(log.Log);

        Assert.Null((await _core.SendAsync("status")).Log);
        Assert.True((await _core.SendAsync("log_stop")).Ok);
    }

    [Fact]
    public async Task StartWithoutCertificateIsRefused()
    {
        await _core.WaitReadyAsync();

        var reply = await _core.SendAsync("start");

        Assert.False(reply.Ok);
        Assert.Equal("Install the certificate first.", reply.Error);
        Assert.False(reply.Status!.Running);
    }

    [Fact]
    public async Task ExitedFiresWhenCoreEnds()
    {
        await _core.WaitReadyAsync();
        var exited = new TaskCompletionSource();
        _core.Exited += () => exited.TrySetResult();

        _core.Dispose();

        Assert.Same(exited.Task, await Task.WhenAny(exited.Task, Task.Delay(10_000)));
    }

    [Fact]
    public async Task UnknownCommandKeepsCoreAlive()
    {
        await _core.WaitReadyAsync();

        Assert.False((await _core.SendAsync("format_c")).Ok);
        Assert.True((await _core.SendAsync("status")).Ok);
    }
}
