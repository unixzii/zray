//! Colors of the search UI, addressed by what they are used for.
//!
//! The UI never names a color directly; it asks the theme for a token.
//! [`Theme::default`] is the built-in look and `$ZRAY_THEME` overrides
//! individual tokens on top of it.

use ratatui::style::Color;

/// Environment variable holding theme overrides, e.g.
/// `match=#ffb454,marker=blue,muted=244`.
pub const THEME_ENV: &str = "ZRAY_THEME";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    /// Candidate commands.
    pub text: Color,
    /// The selected candidate's text.
    pub selected: Color,
    /// Background of the selected candidate's row.
    pub selected_bg: Color,
    /// Characters of a candidate that the query matched.
    pub matched: Color,
    /// The bar marking the selected row.
    pub marker: Color,
    /// Secondary information: ages and the truncation ellipsis.
    pub muted: Color,
    /// The text typed into the search box.
    pub query: Color,
}

impl Default for Theme {
    /// Uses only the terminal's own palette, so it follows the terminal's
    /// color scheme. `Reset` means the terminal's default color.
    fn default() -> Self {
        Theme {
            text: Color::Reset,
            selected: Color::Reset,
            selected_bg: Color::Reset,
            matched: Color::Red,
            marker: Color::Cyan,
            muted: Color::DarkGray,
            query: Color::Reset,
        }
    }
}

impl Theme {
    /// Token names as written in `$ZRAY_THEME`, in display order.
    pub const TOKENS: [&'static str; 7] = [
        "text",
        "selected",
        "selected-bg",
        "match",
        "marker",
        "muted",
        "query",
    ];

    fn slot(&mut self, token: &str) -> Option<&mut Color> {
        Some(match token {
            "text" => &mut self.text,
            "selected" => &mut self.selected,
            "selected-bg" => &mut self.selected_bg,
            "match" => &mut self.matched,
            "marker" => &mut self.marker,
            "muted" => &mut self.muted,
            "query" => &mut self.query,
            _ => return None,
        })
    }

    /// The color of `token`, which must be one of [`Theme::TOKENS`].
    pub fn get(&self, token: &str) -> Option<Color> {
        self.clone().slot(token).map(|c| *c)
    }

    /// Applies `token=color` overrides, separated by commas or whitespace,
    /// to the default theme. Entries that cannot be understood are skipped
    /// and described in the returned list, so one typo does not discard the
    /// rest of the theme.
    pub fn parse(spec: &str) -> (Theme, Vec<String>) {
        let mut theme = Theme::default();
        let mut problems = Vec::new();
        for entry in spec.split(|c: char| c == ',' || c.is_whitespace()) {
            if entry.is_empty() {
                continue;
            }
            let Some((token, value)) = entry.split_once('=') else {
                problems.push(format!("`{entry}`: expected token=color"));
                continue;
            };
            let Some(slot) = theme.slot(&token.to_ascii_lowercase().replace('_', "-")) else {
                problems.push(format!(
                    "`{token}`: unknown token (known: {})",
                    Self::TOKENS.join(", ")
                ));
                continue;
            };
            match parse_color(value) {
                Some(color) => *slot = color,
                None => problems.push(format!(
                    "`{value}`: not a color (use #rrggbb, #rgb, a name like `yellow` or \
                     `bright-blue`, an index 0-255, or `default`)"
                )),
            }
        }
        (theme, problems)
    }

    /// The theme from `$ZRAY_THEME`, with any problems found in it.
    pub fn from_env() -> (Theme, Vec<String>) {
        match std::env::var(THEME_ENV) {
            Ok(spec) => Self::parse(&spec),
            Err(_) => (Theme::default(), Vec::new()),
        }
    }
}

/// `#rrggbb`, `#rgb`, an ANSI color name, a 256-color index, or `default`
/// for the terminal's own default color.
fn parse_color(value: &str) -> Option<Color> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("default") || value.eq_ignore_ascii_case("none") {
        return Some(Color::Reset);
    }
    if let Some(hex) = value.strip_prefix('#') {
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let digits: Vec<u8> = hex
            .chars()
            .filter_map(|c| c.to_digit(16))
            .map(|d| d as u8)
            .collect();
        return match digits[..] {
            [r, g, b] => Some(Color::Rgb(r * 17, g * 17, b * 17)),
            [r1, r0, g1, g0, b1, b0] => Some(Color::Rgb(r1 * 16 + r0, g1 * 16 + g0, b1 * 16 + b0)),
            _ => None,
        };
    }
    if let Ok(index) = value.parse::<u8>() {
        return Some(Color::Indexed(index));
    }
    let name = value.to_ascii_lowercase().replace(['_', ' '], "-");
    let name = match name.as_str() {
        "gray" | "grey" => "bright-black",
        other => other,
    };
    ANSI_NAMES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, color)| *color)
}

/// The 16 ANSI colors under their standard names. ratatui calls ANSI white
/// `Gray` and bright white `White`; these names hide that.
const ANSI_NAMES: [(&str, Color); 16] = [
    ("black", Color::Black),
    ("red", Color::Red),
    ("green", Color::Green),
    ("yellow", Color::Yellow),
    ("blue", Color::Blue),
    ("magenta", Color::Magenta),
    ("cyan", Color::Cyan),
    ("white", Color::Gray),
    ("bright-black", Color::DarkGray),
    ("bright-red", Color::LightRed),
    ("bright-green", Color::LightGreen),
    ("bright-yellow", Color::LightYellow),
    ("bright-blue", Color::LightBlue),
    ("bright-magenta", Color::LightMagenta),
    ("bright-cyan", Color::LightCyan),
    ("bright-white", Color::White),
];

/// `color` in the notation [`Theme::parse`] accepts.
pub fn color_name(color: Color) -> String {
    match color {
        Color::Reset => "default".to_string(),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(i) => i.to_string(),
        named => ANSI_NAMES
            .iter()
            .find(|(_, c)| *c == named)
            .map_or_else(|| named.to_string(), |(n, _)| n.to_string()),
    }
}

/// SGR parameters selecting `color` as the foreground (`background` false)
/// or background, for printing samples outside the TUI.
pub fn sgr(color: Color, background: bool) -> String {
    let offset = if background { 10 } else { 0 };
    let basic = |n: u8| (n + offset).to_string();
    match color {
        Color::Reset => basic(39),
        Color::Black => basic(30),
        Color::Red => basic(31),
        Color::Green => basic(32),
        Color::Yellow => basic(33),
        Color::Blue => basic(34),
        Color::Magenta => basic(35),
        Color::Cyan => basic(36),
        Color::Gray => basic(37),
        Color::DarkGray => basic(90),
        Color::LightRed => basic(91),
        Color::LightGreen => basic(92),
        Color::LightYellow => basic(93),
        Color::LightBlue => basic(94),
        Color::LightMagenta => basic(95),
        Color::LightCyan => basic(96),
        Color::White => basic(97),
        Color::Indexed(i) => format!("{};5;{i}", 38 + offset),
        Color::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", 38 + offset),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_spec_is_the_default_theme() {
        assert_eq!(Theme::parse(""), (Theme::default(), vec![]));
    }

    #[test]
    fn overrides_apply_on_top_of_the_default() {
        let (theme, problems) =
            Theme::parse("match=#ffb454, marker=bright-blue selected_bg=#234\nmuted=244");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(theme.matched, Color::Rgb(0xff, 0xb4, 0x54));
        assert_eq!(theme.marker, Color::LightBlue);
        assert_eq!(theme.selected_bg, Color::Rgb(0x22, 0x33, 0x44));
        assert_eq!(theme.muted, Color::Indexed(244));
        assert_eq!(theme.text, Theme::default().text);
    }

    #[test]
    fn default_resets_a_token() {
        let (theme, problems) = Theme::parse("match=default");
        assert!(problems.is_empty());
        assert_eq!(theme.matched, Color::Reset);
    }

    #[test]
    fn bad_entries_are_reported_and_skipped() {
        let (theme, problems) = Theme::parse("match=#12345,nope=red,marker,muted=#zzz,query=green");
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert_eq!(theme.matched, Theme::default().matched);
        assert_eq!(theme.muted, Theme::default().muted);
        assert_eq!(theme.query, Color::Green);
    }

    #[test]
    fn every_token_is_addressable() {
        let theme = Theme::default();
        for token in Theme::TOKENS {
            assert!(theme.get(token).is_some(), "{token}");
        }
    }

    #[test]
    fn ansi_names_follow_the_ansi_standard() {
        assert_eq!(parse_color("white"), Some(Color::Gray));
        assert_eq!(parse_color("Bright White"), Some(Color::White));
        assert_eq!(parse_color("grey"), Some(Color::DarkGray));
        assert_eq!(parse_color("bright_black"), Some(Color::DarkGray));
        assert_eq!(sgr(parse_color("white").unwrap(), false), "37");
        assert_eq!(parse_color("lightblue"), None);
    }

    #[test]
    fn color_names_round_trip() {
        for value in ["default", "#ffb454", "244", "white", "bright-black", "cyan"] {
            assert_eq!(color_name(parse_color(value).unwrap()), value);
        }
    }

    #[test]
    fn sgr_codes() {
        assert_eq!(sgr(Color::Yellow, false), "33");
        assert_eq!(sgr(Color::DarkGray, true), "100");
        assert_eq!(sgr(Color::Rgb(1, 2, 3), false), "38;2;1;2;3");
        assert_eq!(sgr(Color::Indexed(244), true), "48;5;244");
    }
}
