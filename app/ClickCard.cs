using CommunityToolkit.WinUI.Controls;
using Microsoft.UI.Xaml.Automation.Peers;
using Microsoft.UI.Xaml.Automation.Provider;

namespace CoffeeFilter;

/// <summary>
/// A clickable settings card that screen readers and other assistive tools can press too.
///
/// The toolkit's SettingsCard tells UI Automation it can be invoked but doesn't implement
/// invoking, so pressing it that way fails ("Specified cast is not valid"). This card's
/// automation peer does implement it and raises <see cref="Invoked"/>, the same thing a click does.
/// </summary>
public partial class ClickCard : SettingsCard
{
    /// <summary>
    /// Pressed through UI Automation (mouse and keyboard still raise Click).
    /// </summary>
    public event EventHandler? Invoked;

    protected override AutomationPeer OnCreateAutomationPeer() => new ClickCardAutomationPeer(this);

    internal void RaiseInvoked() => Invoked?.Invoke(this, EventArgs.Empty);
}

/// <summary>
/// Presents the card as a button named after its header, with a working Invoke.
/// </summary>
public partial class ClickCardAutomationPeer(ClickCard owner) : FrameworkElementAutomationPeer(owner), IInvokeProvider
{
    protected override object GetPatternCore(PatternInterface patternInterface) =>
        patternInterface == PatternInterface.Invoke ? this : base.GetPatternCore(patternInterface);

    protected override AutomationControlType GetAutomationControlTypeCore() => AutomationControlType.Button;

    protected override string GetClassNameCore() => nameof(SettingsCard);

    protected override string GetNameCore() => owner.Header as string ?? base.GetNameCore();

    public void Invoke() => owner.RaiseInvoked();
}
