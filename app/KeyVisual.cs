// Copyright (c) Microsoft Corporation
// The Microsoft Corporation licenses this file to you under the MIT license.
// See the LICENSE file in the project root for more information.
//
// Ported from PowerToys: src/common/Common.UI.Controls/Controls/KeyVisual/KeyVisual.xaml.cs
// (https://github.com/microsoft/PowerToys), by way of Leaf Calendar and MagicBuds, whose changes are kept (Native AOT: nothing from the
// template is read back through a typed cast): the presenter's style and content are this control's PresenterStyle and
// PresenterContent, which the template binds, instead of being set on the template part; Update also runs when Content
// changes. Coffee Filter: keys always draw as glyphs where they have one, and only the codes HotkeySettings.GetKeysList
// sends are handled.

using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Windows.System;

namespace CoffeeFilter;

/// <summary>One key of a shortcut drawn as a key cap (PowerToys' KeyVisual): text, a glyph for Shift and the arrows, or the Windows logo.</summary>
[TemplateVisualState(Name = NormalState, GroupName = "CommonStates")]
[TemplateVisualState(Name = DisabledState, GroupName = "CommonStates")]
[TemplateVisualState(Name = InvalidState, GroupName = "CommonStates")]
[TemplateVisualState(Name = WarningState, GroupName = "CommonStates")]
public sealed partial class KeyVisual : Control
{
    private const string NormalState = "Normal";
    private const string DisabledState = "Disabled";
    private const string InvalidState = "Invalid";
    private const string WarningState = "Warning";

    /// <summary>The key: its name as text, or a virtual-key code (92 is Win, 16 is Shift, 37–40 the arrows).</summary>
    public object Content
    {
        get => (object)GetValue(ContentProperty);
        set => SetValue(ContentProperty, value);
    }

    /// <summary>Identifies <see cref="Content"/>.</summary>
    public static readonly DependencyProperty ContentProperty = DependencyProperty.Register(nameof(Content), typeof(object), typeof(KeyVisual), new PropertyMetadata(default(string), OnContentChanged));

    /// <summary>Normal, Error (the combination can't be used), or Warning (it's taken).</summary>
    public KeyVisualState State
    {
        get => (KeyVisualState)GetValue(StateProperty);
        set => SetValue(StateProperty, value);
    }

    /// <summary>Identifies <see cref="State"/>.</summary>
    public static readonly DependencyProperty StateProperty = DependencyProperty.Register(nameof(State), typeof(KeyVisualState), typeof(KeyVisual), new PropertyMetadata(KeyVisualState.Normal, OnStateChanged));

    /// <summary>The presenter's style (text, glyph, or the Windows logo), for the template.</summary>
    public Style PresenterStyle
    {
        get => (Style)GetValue(PresenterStyleProperty);
        private set => SetValue(PresenterStyleProperty, value);
    }

    /// <summary>Identifies <see cref="PresenterStyle"/>.</summary>
    public static readonly DependencyProperty PresenterStyleProperty = DependencyProperty.Register(nameof(PresenterStyle), typeof(Style), typeof(KeyVisual), new PropertyMetadata(null));

    /// <summary>What the presenter shows (the key's text or glyph), for the template.</summary>
    public object PresenterContent
    {
        get => (object)GetValue(PresenterContentProperty);
        private set => SetValue(PresenterContentProperty, value);
    }

    /// <summary>Identifies <see cref="PresenterContent"/>.</summary>
    public static readonly DependencyProperty PresenterContentProperty = DependencyProperty.Register(nameof(PresenterContent), typeof(object), typeof(KeyVisual), new PropertyMetadata(default(string)));

    /// <summary>Creates the key.</summary>
    public KeyVisual()
    {
        this.DefaultStyleKey = typeof(KeyVisual);
    }

    /// <inheritdoc />
    protected override void OnApplyTemplate()
    {
        IsEnabledChanged -= KeyVisual_IsEnabledChanged;
        PresenterStyle ??= Resource("DefaultKeyCharPresenterStyle");
        Update();
        SetVisualStates();
        IsEnabledChanged += KeyVisual_IsEnabledChanged;
        base.OnApplyTemplate();
    }

    private static void OnContentChanged(DependencyObject d, DependencyPropertyChangedEventArgs e)
    {
        ((KeyVisual)d).Update();
        ((KeyVisual)d).SetVisualStates();
    }

    private static void OnStateChanged(DependencyObject d, DependencyPropertyChangedEventArgs e)
    {
        ((KeyVisual)d).SetVisualStates();
    }

    private void SetVisualStates()
    {
        if (State == KeyVisualState.Error)
        {
            VisualStateManager.GoToState(this, InvalidState, true);
        }
        else if (State == KeyVisualState.Warning)
        {
            VisualStateManager.GoToState(this, WarningState, true);
        }
        else if (!IsEnabled)
        {
            VisualStateManager.GoToState(this, DisabledState, true);
        }
        else
        {
            VisualStateManager.GoToState(this, NormalState, true);
        }
    }

    // Text keys (Ctrl, Alt, letters, Enter, ...) as their name; Shift and the arrows as Segoe Fluent glyphs; Win as the logo
    private void Update()
    {
        if (Content == null)
        {
            return;
        }

        if (Content is string key)
        {
            PresenterContent = key;
            PresenterStyle = Resource("DefaultKeyCharPresenterStyle");
            return;
        }

        if (Content is int keyCode)
        {
            switch ((VirtualKey)keyCode)
            {
                case VirtualKey.Shift:
                    SetGlyph("");
                    break;

                case VirtualKey.Up:
                    SetGlyph("");
                    break;

                case VirtualKey.Down:
                    SetGlyph("");
                    break;

                case VirtualKey.Left:
                    SetGlyph("");
                    break;

                case VirtualKey.Right:
                    SetGlyph("");
                    break;

                case VirtualKey.RightWindows:
                    PresenterStyle = Resource("WindowsKeyCharPresenterStyle");
                    break;
            }
        }
    }

    private void SetGlyph(string glyph)
    {
        PresenterContent = glyph;
        PresenterStyle = Resource("GlyphKeyCharPresenterStyle");
    }

    private static Style Resource(string key) => (Style)Application.Current.Resources[key];

    private void KeyVisual_IsEnabledChanged(object sender, DependencyPropertyChangedEventArgs e)
    {
        SetVisualStates();
    }
}

/// <summary>How a <see cref="KeyVisual"/> is drawn (PowerToys' <c>State</c>).</summary>
public enum KeyVisualState
{
    /// <summary>The usual key cap.</summary>
    Normal,

    /// <summary>The combination can't be used.</summary>
    Error,

    /// <summary>The combination is taken.</summary>
    Warning,
}


/// <summary>What's printed on a <see cref="KeyVisual"/>: text, a glyph, or the Windows logo, by its style.</summary>
public sealed partial class KeyCharPresenter : Control
{
    /// <summary>Creates the presenter.</summary>
    public KeyCharPresenter()
    {
        DefaultStyleKey = typeof(KeyCharPresenter);
    }

    /// <summary>The text or glyph.</summary>
    public object Content
    {
        get => (object)GetValue(ContentProperty);
        set => SetValue(ContentProperty, value);
    }

    /// <summary>Identifies <see cref="Content"/>.</summary>
    public static readonly DependencyProperty ContentProperty = DependencyProperty.Register(nameof(Content), typeof(object), typeof(KeyCharPresenter), new PropertyMetadata(default(string)));
}
