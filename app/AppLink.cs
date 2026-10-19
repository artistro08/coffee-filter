namespace CoffeeFilter;

/// <summary>
/// What a coffee-filter:// link or a notification asks the app to open: Add userscript
/// ("install", with the script's address), Exclude this site ("exclude", with the site name),
/// or a page by name (notifications). Kept free of WinUI so the rules can be tested.
/// </summary>
public sealed record AppLink(string Action, string Value)
{
    /// <summary>
    /// Read coffee-filter://install?url=… or coffee-filter://exclude?host=…; null for anything
    /// else. Any web page can make these links, so a site name must look like one before the
    /// consent dialog shows it (no sentences, no pages of text). The first value counts when a
    /// name repeats.
    /// </summary>
    /// <param name="action">The link's host part: "install" or "exclude".</param>
    /// <param name="query">The link's query, with or without the leading "?".</param>
    public static AppLink? Parse(string action, string query)
    {
        var key = action switch
        {
            "install" => "url",
            "exclude" => "host",
            _ => null,
        };
        if (key is null || string.IsNullOrEmpty(query))
        {
            return null;
        }
        var value = System.Web.HttpUtility.ParseQueryString(query).GetValues(key)?.FirstOrDefault();
        if (string.IsNullOrWhiteSpace(value))
        {
            return null;
        }

        if (action == "exclude")
        {
            value = value.Trim().TrimStart('*', '.').TrimEnd('.').ToLowerInvariant();
            if (value.Length is 0 or > 253 || !value.Contains('.') || !value.All(c => char.IsAsciiLetterOrDigit(c) || c is '.' or '-'))
            {
                return null;
            }
        }
        return new AppLink(action, value);
    }
}
