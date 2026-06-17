// SPDX-License-Identifier: MIT OR Apache-2.0

//! File-based theming: schema, loader, validation, and DOM application.
//!
//! Theme files override a small set of public semantic CSS custom properties.
//! Unknown keys are ignored; invalid values are skipped. If parsing fails
//! entirely, zero overrides are applied and the CSS fallback wins.

use serde::Deserialize;

// ── Schema ───────────────────────────────────────────────────────────────────

/// Top-level theme file.
#[derive(Debug, Deserialize)]
pub struct ThemeFile {
    pub version: u32,
    #[allow(dead_code)]
    pub name: Option<String>,
    pub color: Option<ColorTokens>,
}

#[derive(Debug, Deserialize)]
pub struct ColorTokens {
    pub surface: Option<SurfaceTokens>,
    pub border: Option<BorderTokens>,
    pub text: Option<TextTokens>,
    pub brand: Option<BrandTokens>,
    pub status: Option<StatusTokens>,
    pub focus: Option<FocusTokens>,
}

#[derive(Debug, Deserialize)]
pub struct SurfaceTokens {
    pub base: Option<ModeValue>,
    pub raised: Option<ModeValue>,
    pub elevated: Option<ModeValue>,
}

#[derive(Debug, Deserialize)]
pub struct BorderTokens {
    pub default: Option<ModeValue>,
    pub emphasis: Option<ModeValue>,
}

#[derive(Debug, Deserialize)]
pub struct TextTokens {
    pub primary: Option<ModeValue>,
    pub secondary: Option<ModeValue>,
    pub error: Option<ModeValue>,
}

#[derive(Debug, Deserialize)]
pub struct BrandTokens {
    pub accent: Option<ModeValue>,
    #[serde(rename = "accent-hover")]
    pub accent_hover: Option<ModeValue>,
}

#[derive(Debug, Deserialize)]
pub struct StatusTokens {
    pub success: Option<ModeValue>,
    pub warning: Option<ModeValue>,
    pub error: Option<ModeValue>,
}

#[derive(Debug, Deserialize)]
pub struct FocusTokens {
    pub ring: Option<ModeValue>,
}

/// Per-token dark/light pair.
#[derive(Debug, Deserialize)]
pub struct ModeValue {
    pub dark: Option<String>,
    pub light: Option<String>,
}

// ── Resolved variant ─────────────────────────────────────────────────────────

/// Which colour-scheme variant to apply (already resolved from Theme + OS).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResolvedVariant {
    Dark,
    Light,
}

impl ResolvedVariant {
    /// Parse from the string that `apply_theme_to_dom` already computes.
    pub fn from_resolved(s: &str) -> Self {
        if s == "light" {
            Self::Light
        } else {
            Self::Dark
        }
    }
}

// ── Allowlist (security boundary) ────────────────────────────────────────────

/// Each entry maps (extractor-fn on ThemeFile, CSS variable name).
type Extractor = fn(&ThemeFile, ResolvedVariant) -> Option<&String>;

fn extract_surface_base(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.surface.as_ref()?.base.as_ref()?, v)
}
fn extract_surface_raised(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.surface.as_ref()?.raised.as_ref()?, v)
}
fn extract_surface_elevated(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.surface.as_ref()?.elevated.as_ref()?, v)
}
fn extract_border_default(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.border.as_ref()?.default.as_ref()?, v)
}
fn extract_border_emphasis(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.border.as_ref()?.emphasis.as_ref()?, v)
}
fn extract_text_primary(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.text.as_ref()?.primary.as_ref()?, v)
}
fn extract_text_secondary(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.text.as_ref()?.secondary.as_ref()?, v)
}
fn extract_text_error(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.text.as_ref()?.error.as_ref()?, v)
}
fn extract_brand_accent(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.brand.as_ref()?.accent.as_ref()?, v)
}
fn extract_brand_accent_hover(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.brand.as_ref()?.accent_hover.as_ref()?, v)
}
fn extract_status_success(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.status.as_ref()?.success.as_ref()?, v)
}
fn extract_status_warning(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.status.as_ref()?.warning.as_ref()?, v)
}
fn extract_status_error(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.status.as_ref()?.error.as_ref()?, v)
}
fn extract_focus_ring(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.focus.as_ref()?.ring.as_ref()?, v)
}

fn mode_pick(mv: &ModeValue, v: ResolvedVariant) -> Option<&String> {
    match v {
        ResolvedVariant::Dark => mv.dark.as_ref(),
        ResolvedVariant::Light => mv.light.as_ref(),
    }
}

/// The complete allowlist. Only these CSS vars can ever be set by a theme file.
const ALLOWLIST: &[(&str, Extractor)] = &[
    ("--bg", extract_surface_base as Extractor),
    ("--surface", extract_surface_raised),
    ("--surface-elevated", extract_surface_elevated),
    ("--border", extract_border_default),
    ("--border-emphasis", extract_border_emphasis),
    ("--text-primary", extract_text_primary),
    ("--text-secondary", extract_text_secondary),
    ("--accent", extract_brand_accent),
    ("--accent-hover", extract_brand_accent_hover),
    ("--success", extract_status_success),
    ("--warning", extract_status_warning),
    ("--error", extract_status_error),
    ("--error-text", extract_text_error),
    ("--focus-ring", extract_focus_ring),
];

// ── Constants ────────────────────────────────────────────────────────────────

/// Maximum size of a user-imported theme JSON blob.
pub const MAX_THEME_JSON_BYTES: usize = 64 * 1024;

/// Maximum length of any single color value string.
pub const MAX_COLOR_VALUE_LEN: usize = 128;

// ── Validation ───────────────────────────────────────────────────────────────

/// Lightweight format check: hex (#rgb/#rrggbb/#rrggbbaa), rgb()/rgba(), hsl()/hsla().
///
/// Security: this is the guard ahead of user-imported theme files. It uses a
/// *positive* grammar rather than a blocklist — a value must be either a hex
/// literal or a single rgb/rgba/hsl/hsla function call with no nested function.
/// Rejecting any second `(` defeats url(), var(), expression(), image-set(),
/// image(), -webkit-image-set(), attr(), etc. in one rule, case-insensitively,
/// without enumerating names.
fn is_valid_color_value(s: &str) -> bool {
    // Length cap (applied to raw input before trimming).
    if s.len() > MAX_COLOR_VALUE_LEN {
        return false;
    }

    let trimmed = s.trim();

    // Hex literals.
    if let Some(hex) = trimmed.strip_prefix('#') {
        let len = hex.len();
        return (len == 3 || len == 4 || len == 6 || len == 8)
            && hex.chars().all(|c| c.is_ascii_hexdigit());
    }

    // Functional notation — lowercase for case-insensitive structural checks.
    let lower = trimmed.to_ascii_lowercase();
    let is_color_fn = lower.starts_with("rgb(")
        || lower.starts_with("rgba(")
        || lower.starts_with("hsl(")
        || lower.starts_with("hsla(");
    if !is_color_fn {
        return false;
    }

    // Locate the function's opening paren; there must be no further `(` after
    // it (no nested function), and the value must close with `)`.
    let open = match lower.find('(') {
        Some(i) => i,
        None => return false,
    };
    let inner = &lower[open + 1..];
    if inner.contains('(') {
        return false;
    }

    // The LAST character of the trimmed string must be `)` — the close of
    // the function. Anything between the args close-paren and end-of-string
    // (trailing junk) is rejected.
    if !trimmed.ends_with(')') {
        return false;
    }

    // Extract the content between the FIRST `(` and the FINAL `)` and
    // validate the inner grammar: only digits, `.`, `,`, `%`, `/`, and
    // ASCII whitespace are allowed (rgb/rgba/hsl/hsla numeric args never
    // need letters).
    let args_end = trimmed.len() - 1; // index of the final ')'
    let args_start = trimmed.find('(').unwrap() + 1;
    let args = &trimmed[args_start..args_end];
    for ch in args.chars() {
        if !matches!(ch, '0'..='9' | '.' | ',' | '%' | '/' | ' ' | '\t') {
            return false;
        }
    }

    !trimmed.contains('{')
        && !trimmed.contains('}')
        && !trimmed.contains(';')
        && !trimmed.contains("/*")
}

// ── Parse + resolve ──────────────────────────────────────────────────────────

/// Errors from theme file parsing.
#[derive(Debug)]
pub enum ThemeFileError {
    Json(serde_json::Error),
    UnsupportedVersion(u32),
    TooLarge,
    InvalidValue,
    StorageFull,
}

impl std::fmt::Display for ThemeFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(e) => write!(f, "theme JSON parse error: {e}"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported theme version: {v}"),
            Self::TooLarge => write!(
                f,
                "theme file exceeds maximum size ({} KB)",
                MAX_THEME_JSON_BYTES / 1024
            ),
            Self::InvalidValue => write!(f, "theme contains an unsupported color value"),
            Self::StorageFull => write!(f, "storage is full"),
        }
    }
}

/// Parse and validate a theme file from JSON.
pub fn parse_theme_file(json: &str) -> Result<ThemeFile, ThemeFileError> {
    let file: ThemeFile = serde_json::from_str(json).map_err(ThemeFileError::Json)?;
    if file.version != 1 {
        return Err(ThemeFileError::UnsupportedVersion(file.version));
    }
    Ok(file)
}

/// Resolve a parsed theme file into a list of (CSS-var-name, validated-value) pairs.
pub fn validate_and_resolve(
    file: &ThemeFile,
    variant: ResolvedVariant,
) -> Vec<(&'static str, String)> {
    let mut pairs = Vec::new();
    for &(css_var, extractor) in ALLOWLIST {
        if let Some(value) = extractor(file, variant) {
            if is_valid_color_value(value) {
                pairs.push((css_var, value.clone()));
            } else {
                log::warn!("theme_file: skipping invalid color value for {css_var}: {value:?}");
            }
        }
    }
    pairs
}

/// Validate an imported theme JSON blob (pure, host-testable — no web_sys).
///
/// Enforces size limit, version, and strict color-value validation for ALL
/// present values across both dark and light variants.
pub fn validate_theme_json(json: &str) -> Result<ThemeFile, ThemeFileError> {
    if json.len() > MAX_THEME_JSON_BYTES {
        return Err(ThemeFileError::TooLarge);
    }
    let file = parse_theme_file(json)?;
    // Strict whole-file value check: every present value must pass validation.
    for &(_css_var, extractor) in ALLOWLIST {
        for variant in [ResolvedVariant::Dark, ResolvedVariant::Light] {
            if let Some(value) = extractor(&file, variant) {
                if !is_valid_color_value(value) {
                    return Err(ThemeFileError::InvalidValue);
                }
            }
        }
    }
    Ok(file)
}

// ── Active theme source ──────────────────────────────────────────────────────

/// Returns the bundled default theme JSON (compile-time embedded).
fn bundled_default_json() -> &'static str {
    include_str!("../static/themes/default.json")
}

/// Storage key for the user-imported custom theme JSON.
const CUSTOM_THEME_STORAGE_KEY: &str = "vc_theme_custom";

/// Load and validate the custom theme from localStorage.
///
/// Returns `None` when no custom theme is stored, or when the stored blob
/// fails validation (in which case the corrupt key is removed — self-heal).
pub fn load_validated_custom_theme_json() -> Option<String> {
    let storage = web_sys::window().and_then(|w| w.local_storage().ok().flatten())?;
    let raw = storage.get_item(CUSTOM_THEME_STORAGE_KEY).ok().flatten()?;
    match validate_theme_json(&raw) {
        Ok(_) => Some(raw),
        Err(_) => {
            let _ = storage.remove_item(CUSTOM_THEME_STORAGE_KEY);
            None
        }
    }
}

/// Persist a validated custom theme JSON to localStorage.
pub fn persist_custom_theme_json(json: &str) -> Result<(), ThemeFileError> {
    validate_theme_json(json)?;
    let storage = web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .ok_or(ThemeFileError::StorageFull)?;
    storage
        .set_item(CUSTOM_THEME_STORAGE_KEY, json)
        .map_err(|_| ThemeFileError::StorageFull)?;
    Ok(())
}

/// Remove the custom theme from localStorage.
pub fn clear_custom_theme() {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.remove_item(CUSTOM_THEME_STORAGE_KEY);
    }
}

/// Maximum number of characters shown for a custom theme's display name.
/// Bounds layout overflow from an adversarially long `name` field so the
/// reset escape-hatch can never be pushed off-screen.
pub const MAX_DISPLAY_NAME_CHARS: usize = 64;

/// Get the display name of the active custom theme, if any.
///
/// The name is truncated to [`MAX_DISPLAY_NAME_CHARS`] so a hostile file
/// cannot overflow the label and hide the reset control.
pub fn custom_theme_display_name() -> Option<String> {
    let raw = load_validated_custom_theme_json()?;
    let file = parse_theme_file(&raw).ok()?;
    let name = file.name.unwrap_or_else(|| "Custom Theme".to_string());
    Some(name.chars().take(MAX_DISPLAY_NAME_CHARS).collect())
}

/// App-controlled gradient backdrop used when a custom theme is active.
///
/// This is a compile-time constant that references already-validated CSS custom
/// property tokens via `var(--...)`. It is NEVER derived from user-provided file
/// content. The gradient replaces the decorative PNG so the page background
/// visibly reflects the imported theme's palette.
///
/// Security invariant: this string must contain NO `url(` — it uses only
/// `var()`, `color-mix()`, `radial-gradient()`, and `linear-gradient()`.
pub const CUSTOM_THEME_BACKDROP_GRADIENT: &str = "\
radial-gradient(900px 600px at 50% -10%, color-mix(in oklch, var(--accent) 22%, transparent), transparent 70%), \
radial-gradient(700px 500px at 85% 110%, color-mix(in oklch, var(--accent-hover) 18%, transparent), transparent 65%), \
linear-gradient(160deg, var(--bg) 0%, var(--surface) 100%)";

// ── DOM application ──────────────────────────────────────────────────────────

/// Remove all managed CSS custom properties from documentElement inline style.
/// Also removes the `--bg-image` inline override to ensure the stylesheet PNG
/// wins when no custom theme is active.
fn clear_theme_overrides() {
    let style = match document_element_style() {
        Some(s) => s,
        None => return,
    };
    for &(var_name, _) in ALLOWLIST {
        let _ = style.remove_property(var_name);
    }
    // Always clear the backdrop override so the bundled PNG is restored.
    let _ = style.remove_property("--bg-image");
}

/// Apply the active theme file's tokens for the given resolved variant.
/// Called from `apply_theme_to_dom` after setting `data-theme`.
///
/// On any parse/load failure, clears all inline overrides so the CSS fallback
/// remains authoritative.
///
/// When a custom (user-imported) theme is active, also sets `--bg-image` to
/// [`CUSTOM_THEME_BACKDROP_GRADIENT`] so the page background visibly reflects
/// the theme palette. When the bundled default is active, `--bg-image` inline
/// override is absent and the stylesheet PNG wins.
pub fn apply_theme_file_tokens(resolved_variant_str: &str) {
    // Always clear first — prevents stale dark values shadowing light (or vice-versa).
    // Also clears any inline --bg-image from a previous custom theme.
    clear_theme_overrides();

    // Single localStorage read: decide the active source once, then reuse it
    // for both token selection and the backdrop-gradient decision.
    let custom_json = load_validated_custom_theme_json();
    let is_custom = custom_json.is_some();
    let json = custom_json.unwrap_or_else(|| bundled_default_json().to_string());
    let file = match parse_theme_file(&json) {
        Ok(f) => f,
        Err(e) => {
            log::warn!("theme_file: failed to parse active theme, using CSS fallback: {e}");
            return;
        }
    };

    let variant = ResolvedVariant::from_resolved(resolved_variant_str);
    let pairs = validate_and_resolve(&file, variant);

    let style = match document_element_style() {
        Some(s) => s,
        None => return,
    };
    for (var_name, value) in pairs {
        let _ = style.set_property(var_name, &value);
    }

    // If a user-imported custom theme is active, replace the decorative PNG
    // with the app-controlled gradient that references the themed tokens.
    if is_custom {
        let _ = style.set_property("--bg-image", CUSTOM_THEME_BACKDROP_GRADIENT);
    }
    // Otherwise: clear_theme_overrides already removed --bg-image, so the
    // stylesheet PNG (dark or light) remains authoritative.
}

/// Helper: get the CSSStyleDeclaration of documentElement.
fn document_element_style() -> Option<web_sys::CssStyleDeclaration> {
    use wasm_bindgen::JsCast;
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
        .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
        .map(|el| el.style())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bundled_default() {
        let file = parse_theme_file(bundled_default_json()).expect("bundled default must parse");
        assert_eq!(file.version, 1);

        let dark_pairs = validate_and_resolve(&file, ResolvedVariant::Dark);
        assert!(!dark_pairs.is_empty());
        // All 14 tokens should resolve for the bundled default.
        assert_eq!(dark_pairs.len(), 14);

        let light_pairs = validate_and_resolve(&file, ResolvedVariant::Light);
        assert_eq!(light_pairs.len(), 14);
    }

    #[test]
    fn rejects_invalid_version() {
        let json = r#"{"version": 99, "color": {}}"#;
        assert!(matches!(
            parse_theme_file(json),
            Err(ThemeFileError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn rejects_css_injection() {
        let json = r##"{
            "version": 1,
            "color": {
                "surface": {
                    "base": {"dark": "red; } html { display:none", "light": "#fff"}
                }
            }
        }"##;
        let file = parse_theme_file(json).unwrap();
        let pairs = validate_and_resolve(&file, ResolvedVariant::Dark);
        // The dark value is rejected, only light would resolve (but we asked for dark).
        assert!(pairs.is_empty());
    }

    #[test]
    fn valid_color_formats() {
        assert!(is_valid_color_value("#fff"));
        assert!(is_valid_color_value("#ffffff"));
        assert!(is_valid_color_value("#ffffffaa"));
        assert!(is_valid_color_value("#abcdef"));
        assert!(is_valid_color_value("rgb(1, 2, 3)"));
        assert!(is_valid_color_value("rgba(1, 2, 3, 0.5)"));
        assert!(is_valid_color_value("hsl(120, 50%, 50%)"));
        assert!(is_valid_color_value("hsla(120, 50%, 50%, 0.8)"));
    }

    #[test]
    fn invalid_color_formats() {
        assert!(!is_valid_color_value("red"));
        assert!(!is_valid_color_value("not-a-color"));
        assert!(!is_valid_color_value("#gg"));
        assert!(!is_valid_color_value("rgb(1,2,3};html{display:none"));
    }

    #[test]
    fn rejects_functional_notation_smuggling() {
        assert!(!is_valid_color_value("rgba(0, url(https://evil/x), 0, 1)"));
        assert!(!is_valid_color_value("rgb(var(--x), 0, 0)"));
        assert!(!is_valid_color_value("rgba(0,0,0,1) /* x */"));
        assert!(!is_valid_color_value("rgb(expression(alert(1)), 0, 0)"));
        // Case-insensitive: uppercase / mixed-case function names must not bypass.
        assert!(!is_valid_color_value("rgb(0,0,0) URL(https://evil/x)"));
        assert!(!is_valid_color_value("rgb(0,0,0) Url(https://evil/x)"));
        assert!(!is_valid_color_value("rgb(0,0,0) uRl(https://evil/x)"));
        // Image-fetching function family must not bypass (was not enumerated).
        assert!(!is_valid_color_value(
            "rgb(0,0,0) image-set('https://evil/x')"
        ));
        assert!(!is_valid_color_value(
            "rgb(0,0,0) IMAGE-SET('https://evil/x')"
        ));
        assert!(!is_valid_color_value("rgb(0,0,0) image('https://evil/x')"));
        assert!(!is_valid_color_value(
            "rgb(0,0,0) -webkit-image-set('https://evil/x')"
        ));
        // attr() / any nested function.
        assert!(!is_valid_color_value("rgb(0,0,0) attr(data-x)"));
    }

    #[test]
    fn garbage_json_yields_error() {
        assert!(parse_theme_file("not json at all").is_err());
        assert!(parse_theme_file("").is_err());
        assert!(parse_theme_file("{}").is_err()); // missing version
    }

    // ── New tests for validate_theme_json and hardened is_valid_color_value ──

    #[test]
    fn validate_theme_json_accepts_valid() {
        let json = bundled_default_json();
        let file = validate_theme_json(json).expect("bundled default must validate");
        assert_eq!(file.version, 1);
    }

    #[test]
    fn validate_theme_json_rejects_oversize() {
        // 65 KB of spaces + minimal valid JSON structure
        let big = " ".repeat(MAX_THEME_JSON_BYTES + 1);
        assert!(matches!(
            validate_theme_json(&big),
            Err(ThemeFileError::TooLarge)
        ));
    }

    #[test]
    fn validate_theme_json_rejects_version_0() {
        let json = r#"{"version": 0, "color": {}}"#;
        assert!(matches!(
            validate_theme_json(json),
            Err(ThemeFileError::UnsupportedVersion(0))
        ));
    }

    #[test]
    fn validate_theme_json_rejects_version_2() {
        let json = r#"{"version": 2, "color": {}}"#;
        assert!(matches!(
            validate_theme_json(json),
            Err(ThemeFileError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn validate_theme_json_rejects_smuggled_values() {
        // @token-exempt: the strings below are hostile validation inputs, not real CSS colors.
        // url()
        let json = r##"{"version": 1, "color": {"surface": {"base": {"dark": "url(https://evil)", "light": "#fff"}}}}"##;
        assert!(matches!(
            validate_theme_json(json),
            Err(ThemeFileError::InvalidValue)
        ));

        // var()
        let json = r##"{"version": 1, "color": {"surface": {"base": {"dark": "var(--x)", "light": "#fff"}}}}"##;
        assert!(matches!(
            validate_theme_json(json),
            Err(ThemeFileError::InvalidValue)
        ));

        // nested paren inside function
        let json = r##"{"version": 1, "color": {"surface": {"base": {"dark": "rgb(expression(1), 0, 0)", "light": "#fff"}}}}"##;
        assert!(matches!(
            validate_theme_json(json),
            Err(ThemeFileError::InvalidValue)
        ));

        // semicolon breakout
        let json = r##"{"version": 1, "color": {"surface": {"base": {"dark": "rgb(0,0,0); body{display:none}", "light": "#fff"}}}}"##;
        assert!(matches!(
            validate_theme_json(json),
            Err(ThemeFileError::InvalidValue)
        ));
    }

    #[test]
    fn is_valid_color_value_rejects_over_max_len() {
        let long = format!("#{}", "a".repeat(MAX_COLOR_VALUE_LEN));
        assert!(!is_valid_color_value(&long));
    }

    #[test]
    fn is_valid_color_value_rejects_trailing_junk() {
        // @token-exempt: trailing-junk rejection inputs, not real CSS colors.
        // Trailing junk after close paren — must be rejected
        assert!(!is_valid_color_value("rgba(0,0,0,1) anything)"));
        assert!(!is_valid_color_value("rgb(0,0,0) extra"));
    }

    // ── Backdrop gradient security invariant tests ───────────────────────────

    #[test]
    fn backdrop_gradient_is_nonempty_and_references_tokens() {
        assert!(!CUSTOM_THEME_BACKDROP_GRADIENT.is_empty());
        assert!(CUSTOM_THEME_BACKDROP_GRADIENT.contains("var(--bg)"));
        assert!(CUSTOM_THEME_BACKDROP_GRADIENT.contains("var(--accent)"));
        assert!(CUSTOM_THEME_BACKDROP_GRADIENT.contains("var(--surface)"));
        assert!(CUSTOM_THEME_BACKDROP_GRADIENT.contains("var(--accent-hover)"));
    }

    #[test]
    fn backdrop_gradient_contains_no_url() {
        // Security: the gradient must never embed a url() — it is a static
        // app-controlled string referencing only validated token vars.
        let lower = CUSTOM_THEME_BACKDROP_GRADIENT.to_ascii_lowercase();
        assert!(
            !lower.contains("url("),
            "backdrop gradient must not contain url()"
        );
    }
}
