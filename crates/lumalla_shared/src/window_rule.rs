/// How to match a window title against a pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleMatchKind {
    Equals,
    Contains,
    StartsWith,
    EndsWith,
}

impl TitleMatchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Equals => "equals",
            Self::Contains => "contains",
            Self::StartsWith => "starts_with",
            Self::EndsWith => "ends_with",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "equals" => Some(Self::Equals),
            "contains" => Some(Self::Contains),
            "starts_with" => Some(Self::StartsWith),
            "ends_with" => Some(Self::EndsWith),
            _ => None,
        }
    }
}

/// Title pattern used by a window rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleMatcher {
    pub kind: TitleMatchKind,
    pub pattern: String,
}

impl TitleMatcher {
    pub fn matches(&self, title: &str) -> bool {
        match self.kind {
            TitleMatchKind::Equals => title == self.pattern,
            TitleMatchKind::Contains => title.contains(&self.pattern),
            TitleMatchKind::StartsWith => title.starts_with(&self.pattern),
            TitleMatchKind::EndsWith => title.ends_with(&self.pattern),
        }
    }
}

/// Default placement for windows matching an application id and/or title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowRule {
    /// Application id to match (`xdg_toplevel.app_id`). Exact equality when set.
    pub app_id: Option<String>,
    /// Optional title matcher.
    pub title: Option<TitleMatcher>,
    /// Optional zone to join when the rule matches.
    pub zone: Option<String>,
    /// Default x position.
    pub x: Option<i32>,
    /// Default y position.
    pub y: Option<i32>,
    /// Default width.
    pub width: Option<i32>,
    /// Default height.
    pub height: Option<i32>,
}

impl WindowRule {
    /// Geometry fields carried by this rule.
    pub fn geometry(&self) -> super::WindowGeometryUpdate {
        super::WindowGeometryUpdate {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
    }

    /// Whether this rule matches the given window identity.
    ///
    /// Present matchers are ANDed. A rule with neither matcher never matches.
    pub fn matches(&self, app_id: &str, title: &str) -> bool {
        if self.app_id.is_none() && self.title.is_none() {
            return false;
        }
        if let Some(rule_app_id) = self.app_id.as_deref()
            && rule_app_id != app_id
        {
            return false;
        }
        if let Some(title_matcher) = &self.title
            && !title_matcher.matches(title)
        {
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(
        app_id: Option<&str>,
        title: Option<(TitleMatchKind, &str)>,
    ) -> WindowRule {
        WindowRule {
            app_id: app_id.map(String::from),
            title: title.map(|(kind, pattern)| TitleMatcher {
                kind,
                pattern: String::from(pattern),
            }),
            zone: None,
            x: None,
            y: None,
            width: None,
            height: None,
        }
    }

    #[test]
    fn title_matcher_kinds() {
        let equals = TitleMatcher {
            kind: TitleMatchKind::Equals,
            pattern: String::from("Exact"),
        };
        assert!(equals.matches("Exact"));
        assert!(!equals.matches("exact"));

        let contains = TitleMatcher {
            kind: TitleMatchKind::Contains,
            pattern: String::from("Tube"),
        };
        assert!(contains.matches("YouTube Music"));
        assert!(!contains.matches("You"));

        let starts = TitleMatcher {
            kind: TitleMatchKind::StartsWith,
            pattern: String::from("Git"),
        };
        assert!(starts.matches("GitHub"));
        assert!(!starts.matches("My GitHub"));

        let ends = TitleMatcher {
            kind: TitleMatchKind::EndsWith,
            pattern: String::from(".rs"),
        };
        assert!(ends.matches("main.rs"));
        assert!(!ends.matches("main.rs.bak"));
    }

    #[test]
    fn rule_matches_app_id_only() {
        let r = rule(Some("firefox"), None);
        assert!(r.matches("firefox", "anything"));
        assert!(!r.matches("chrome", "anything"));
    }

    #[test]
    fn rule_matches_title_only() {
        let r = rule(None, Some((TitleMatchKind::Contains, "YouTube")));
        assert!(r.matches("", "Watch YouTube"));
        assert!(!r.matches("firefox", "Other"));
    }

    #[test]
    fn rule_matches_app_id_and_title() {
        let r = rule(
            Some("firefox"),
            Some((TitleMatchKind::StartsWith, "GitHub")),
        );
        assert!(r.matches("firefox", "GitHub - lumalla"));
        assert!(!r.matches("firefox", "GitLab"));
        assert!(!r.matches("chrome", "GitHub - lumalla"));
    }

    #[test]
    fn empty_rule_never_matches() {
        let r = rule(None, None);
        assert!(!r.matches("firefox", "title"));
    }
}
