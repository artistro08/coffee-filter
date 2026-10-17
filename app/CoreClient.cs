using System.Diagnostics;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.Json.Serialization;

namespace CoffeeFilter;

/// <summary>
/// Filter list as reported by the core.
/// </summary>
public class FilterList
{
    public string Name { get; set; } = "";
    public string Url { get; set; } = "";
    public bool Enabled { get; set; }
    public string Kind { get; set; } = "custom";
}

/// <summary>
/// Userscript as reported by the core; the name comes from its @name.
/// </summary>
public class Userscript
{
    public string Name { get; set; } = "";
    public string Url { get; set; } = "";
    public bool Enabled { get; set; }
}

/// <summary>
/// An app whose connections are never decrypted or filtered: a program (Path) or a
/// Microsoft Store app (Package, its package family name).
/// </summary>
public class ExcludedApp
{
    public string Name { get; set; } = "";
    public string? Path { get; set; }
    public string? Package { get; set; }

    /// <summary>
    /// How the core names it in remove_excluded_app.
    /// </summary>
    public string Id => Package ?? Path ?? "";
}

/// <summary>
/// A blocked request from the live log.
/// </summary>
public class LogEntry
{
    public ulong Id { get; set; }

    /// <summary>
    /// Unix time in milliseconds.
    /// </summary>
    public long Time { get; set; }
    public string Url { get; set; } = "";
    public string Kind { get; set; } = "";

    /// <summary>
    /// Site the request came from, if known.
    /// </summary>
    public string? Page { get; set; }
}

/// <summary>
/// A host where Coffee Filter broke a connection (an app refused its certificate).
/// </summary>
public class Problem
{
    public string Host { get; set; } = "";
    public string Kind { get; set; } = "";
    public string Detail { get; set; } = "";
    public ulong Count { get; set; }
    public long Last { get; set; }
}

/// <summary>
/// Snapshot of the core's state, sent back with every reply.
/// </summary>
public class CoreStatus
{
    public bool Running { get; set; }
    public bool CertInstalled { get; set; }

    /// <summary>
    /// Why the certificate should be renewed (expiring, or from an older version); null if it's fine.
    /// </summary>
    public string? CertRenewal { get; set; }
    public ulong Blocked { get; set; }
    public ulong Allowed { get; set; }
    public int Rules { get; set; }
    public int Port { get; set; }
    public List<string> Exclusions { get; set; } = [];
    public List<ExcludedApp> ExcludedApps { get; set; } = [];
    public List<FilterList> Lists { get; set; } = [];
    public List<Userscript> Userscripts { get; set; } = [];
    public List<Problem> Problems { get; set; } = [];
}

public class CoreReply
{
    public bool Ok { get; set; }
    public string? Error { get; set; }
    public CoreStatus? Status { get; set; }

    /// <summary>
    /// Only on replies to the "log" command.
    /// </summary>
    public List<LogEntry>? Log { get; set; }

    /// <summary>
    /// "ready" on the first line; "problem" on an announcement that isn't a reply.
    /// </summary>
    public string? Event { get; set; }

    /// <summary>
    /// The host that just started having problems, on a "problem" announcement.
    /// </summary>
    public Problem? Problem { get; set; }
}

/// <summary>
/// Runs coffee-filter-core.exe as a child process and talks to it over stdin/stdout,
/// one JSON object per line. No network control port is opened. If this app exits
/// or crashes, the core sees stdin close, stops the proxy, and restores proxy settings.
/// If the core exits on its own, <see cref="Exited"/> fires so the app can restart it.
/// Between replies the core can announce a new connection problem, which raises
/// <see cref="ProblemFound"/> instead of being taken for a reply.
/// </summary>
public sealed class CoreClient : IDisposable
{
    private readonly Process _process;
    private readonly SemaphoreSlim _lock = new(1, 1);
    private readonly System.Threading.Channels.Channel<CoreReply> _replies = System.Threading.Channels.Channel.CreateUnbounded<CoreReply>();

    /// <summary>
    /// The core process ended (on a thread pool thread).
    /// </summary>
    public event Action? Exited;

    /// <summary>
    /// A host just started having connection problems (on a thread pool thread).
    /// </summary>
    public event Action<Problem>? ProblemFound;

    /// <param name="exited">Called when the core ends; wired before it starts, so even an instant exit is seen.</param>
    public CoreClient(Action? exited = null)
    {
        // Absolute path next to the app, never resolved through PATH
        var exe = Path.Combine(AppContext.BaseDirectory, "coffee-filter-core.exe");

        Exited  += exited;
        _process = new Process
        {
            StartInfo = new ProcessStartInfo(exe)
            {
                UseShellExecute        = false,
                CreateNoWindow         = true,
                RedirectStandardInput  = true,
                RedirectStandardOutput = true,
                RedirectStandardError  = false,
                // The core speaks UTF-8; without this a GUI process uses the ANSI code page and
                // names like "Café" or "–" arrive garbled (and can't be toggled or removed)
                StandardInputEncoding  = new System.Text.UTF8Encoding(false),
                StandardOutputEncoding = new System.Text.UTF8Encoding(false),
            },
            EnableRaisingEvents = true,
        };
        _process.Exited += (_, _) => Exited?.Invoke();
        if (!_process.Start())
        {
            throw new InvalidOperationException("Could not start coffee-filter-core.exe");
        }
        _ = Task.Run(ReadLinesAsync);
    }

    /// <summary>
    /// Reads every line the core writes: announcements go to their event, replies wait in
    /// order for the command that asked. Ends when the core does.
    /// </summary>
    private async Task ReadLinesAsync()
    {
        try
        {
            while (await _process.StandardOutput.ReadLineAsync() is { } line)
            {
                var reply = JsonSerializer.Deserialize(line, CoreJson.Default.CoreReply)!;
                if (reply is { Event: "problem", Problem: { } problem })
                {
                    ProblemFound?.Invoke(problem);
                    continue;
                }
                _replies.Writer.TryWrite(reply);
            }
        }
        catch (Exception ex) when (ex is IOException or ObjectDisposedException or InvalidOperationException or JsonException)
        {
            // Pipe closed or garbled: same as the core stopping
        }
        _replies.Writer.TryComplete();
    }

    /// <summary>
    /// Waits for the core's "ready" line, sent once filter lists are loaded.
    /// </summary>
    public async Task<CoreReply> WaitReadyAsync() => await ReadReplyAsync();

    /// <summary>
    /// Sends one command (e.g. "start", "set_list") with optional snake_case args and returns the reply.
    /// </summary>
    public async Task<CoreReply> SendAsync(string cmd, JsonObject? args = null)
    {
        var payload = args?.DeepClone().AsObject() ?? [];
        payload["cmd"] = cmd;

        await _lock.WaitAsync();
        try
        {
            await _process.StandardInput.WriteLineAsync(payload.ToJsonString());
            await _process.StandardInput.FlushAsync();
            return await ReadReplyAsync();
        }
        finally
        {
            _lock.Release();
        }
    }

    private async Task<CoreReply> ReadReplyAsync()
    {
        try
        {
            return await _replies.Reader.ReadAsync();
        }
        catch (System.Threading.Channels.ChannelClosedException)
        {
            throw new InvalidOperationException("The filter core stopped unexpectedly.");
        }
    }

    /// <summary>
    /// Closing stdin tells the core to stop cleanly and restore proxy settings.
    /// </summary>
    public void Dispose()
    {
        try
        {
            _process.StandardInput.Close();
            if (!_process.WaitForExit(5000))
            {
                _process.Kill();
            }
        }
        catch (Exception ex) when (ex is InvalidOperationException or IOException)
        {
            // Already exited (its pipe may be closed too)
        }
        _process.Dispose();
    }
}

/// <summary>
/// Compile-time JSON code for core replies (snake_case), so it works with Native AOT and trimming.
/// </summary>
[JsonSourceGenerationOptions(PropertyNamingPolicy = JsonKnownNamingPolicy.SnakeCaseLower)]
[JsonSerializable(typeof(CoreReply))]
internal partial class CoreJson : JsonSerializerContext;
