//! Phone navigation state: which surface is in front, and what the drawer's
//! filter/search ask for.
//!
//! The shell has exactly three top-level screens and two overlay stacks:
//!
//! ```text
//! Screen::Chat ──drawer──▶ Screen::Sessions  (a drawer, not a route)
//!      │
//!      ├──sheet──▶ Sheet::{Model, Session(id), Actions}
//!      └──route──▶ Screen::Settings(SettingsScreen)
//! ```
//!
//! Renderers translate transitions into animations; the model only records
//! the destination so a half-finished glide never leaves the state ambiguous
//! (unlike a bool per overlay, which can end up with two "open" flags).

/// Top-level surface.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum Screen {
    /// Transcript + composer.
    #[default]
    Chat,
    /// Settings page stack; the drawer stays closed while it is front.
    Settings(SettingsScreen),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SettingsScreen {
    #[default]
    Root,
    Appearance,
    General,
    Models,
    About,
}

impl SettingsScreen {
    pub fn title(self) -> &'static str {
        match self {
            SettingsScreen::Root => "设置",
            SettingsScreen::Appearance => "外观",
            SettingsScreen::General => "通用",
            SettingsScreen::Models => "模型",
            SettingsScreen::About => "关于",
        }
    }

    /// `Root` closes the settings surface; every other page pops back to it.
    pub fn back(self) -> Option<SettingsScreen> {
        match self {
            SettingsScreen::Root => None,
            _ => Some(SettingsScreen::Root),
        }
    }
}

/// Bottom sheet kinds. `Session` carries the conversation it acts on.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Sheet {
    Model,
    Session(String),
    Actions,
}

impl Sheet {
    pub fn title(&self) -> &'static str {
        match self {
            Sheet::Model => "切换模型",
            Sheet::Session(_) => "会话操作",
            Sheet::Actions => "快捷操作",
        }
    }
}

/// Drawer filter. `Attention` is "waiting for you", not "unread": a run that
/// stopped on an approval must show up there even after the user read it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Filter {
    #[default]
    All,
    Running,
    Attention,
    Unread,
}

impl Filter {
    pub const ALL: [Filter; 4] = [Filter::All, Filter::Running, Filter::Attention, Filter::Unread];

    pub fn label(self) -> &'static str {
        match self {
            Filter::All => "全部",
            Filter::Running => "运行中",
            Filter::Attention => "待确认",
            Filter::Unread => "未读",
        }
    }
}

/// The whole shell navigation state.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Nav {
    screen: Screen,
    drawer_open: bool,
    sheet: Option<Sheet>,
    filter: Filter,
    query: String,
}

impl Nav {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    pub fn drawer_open(&self) -> bool {
        self.drawer_open
    }

    pub fn sheet(&self) -> Option<&Sheet> {
        self.sheet.as_ref()
    }

    pub fn filter(&self) -> Filter {
        self.filter
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    // --- drawer -----------------------------------------------------------

    /// Opening the drawer closes any sheet: one overlay at a time keeps the
    /// scrims from stacking and Escape unambiguous.
    pub fn open_drawer(&mut self) {
        self.drawer_open = true;
        self.sheet = None;
    }

    pub fn close_drawer(&mut self) {
        self.drawer_open = false;
    }

    pub fn toggle_drawer(&mut self) {
        if self.drawer_open {
            self.close_drawer();
        } else {
            self.open_drawer();
        }
    }

    pub fn set_filter(&mut self, filter: Filter) {
        self.filter = filter;
    }

    pub fn set_query(&mut self, query: impl Into<String>) {
        self.query = query.into();
    }

    // --- sheets -----------------------------------------------------------

    /// Raising a sheet also closes the drawer (the sheet is raised from it).
    pub fn open_sheet(&mut self, sheet: Sheet) {
        self.drawer_open = false;
        self.sheet = Some(sheet);
    }

    pub fn close_sheet(&mut self) {
        self.sheet = None;
    }

    pub fn toggle_sheet(&mut self, sheet: Sheet) {
        if self.sheet.as_ref() == Some(&sheet) {
            self.close_sheet();
        } else {
            self.open_sheet(sheet);
        }
    }

    // --- screens ----------------------------------------------------------

    /// Push the settings surface. Opening settings always dismisses overlays.
    pub fn open_settings(&mut self, screen: SettingsScreen) {
        self.drawer_open = false;
        self.sheet = None;
        self.screen = Screen::Settings(screen);
    }

    /// Pop one level: a sub-page returns to the settings root, the root closes
    /// settings back to the transcript. Returns true while still in settings.
    pub fn settings_back(&mut self) -> bool {
        match self.screen.clone() {
            Screen::Settings(root) => match root.back() {
                Some(parent) => {
                    self.screen = Screen::Settings(parent);
                    true
                }
                None => {
                    self.screen = Screen::Chat;
                    false
                }
            },
            Screen::Chat => false,
        }
    }

    pub fn open_chat(&mut self) {
        self.screen = Screen::Chat;
        self.drawer_open = false;
        self.sheet = None;
    }

    // --- gestures ---------------------------------------------------------

    /// Result of a horizontal drag that started near the left edge.
    pub fn edge_swipe(&mut self, dx: f32) -> bool {
        if dx > 48.0 && !self.drawer_open && self.sheet.is_none() {
            self.open_drawer();
            return true;
        }
        false
    }

    /// Result of a horizontal drag on the open drawer panel.
    pub fn drawer_swipe(&mut self, dx: f32) -> bool {
        if dx < -48.0 && self.drawer_open {
            self.close_drawer();
            return true;
        }
        false
    }

    /// Escape/first back press: dismiss the topmost surface. Returns true when
    /// something was dismissed, false when the caller should decide (e.g.
    /// stop the running turn).
    pub fn dismiss_top(&mut self) -> bool {
        if self.sheet.is_some() {
            self.close_sheet();
            return true;
        }
        if self.drawer_open {
            self.close_drawer();
            return true;
        }
        if matches!(self.screen, Screen::Settings(_)) {
            self.settings_back();
            return true;
        }
        false
    }
}

/// Filter predicate shared by both renderers.
pub fn matches_filter(filter: Filter, status: &SessionStatus) -> bool {
    match filter {
        Filter::All => true,
        Filter::Running => status.running,
        Filter::Attention => status.waiting_approval,
        Filter::Unread => status.unread,
    }
}

/// What a session row advertises, independent of how the app computes it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionStatus {
    pub running: bool,
    pub waiting_approval: bool,
    pub unread: bool,
}

/// Case-insensitive title/id match for the drawer search box.
pub fn matches_query(query: &str, title: &str, id: &str) -> bool {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }
    title.to_lowercase().contains(&needle) || id.to_lowercase().contains(&needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_the_drawer_closes_a_sheet_and_vice_versa() {
        let mut nav = Nav::new();
        nav.open_sheet(Sheet::Model);
        assert_eq!(nav.sheet(), Some(&Sheet::Model));

        nav.open_drawer();
        assert!(nav.drawer_open());
        assert_eq!(nav.sheet(), None, "one overlay at a time");

        nav.open_sheet(Sheet::Actions);
        assert_eq!(nav.sheet(), Some(&Sheet::Actions));
        assert!(!nav.drawer_open(), "raising a sheet closes the drawer");
    }

    #[test]
    fn settings_pop_one_level_then_close() {
        let mut nav = Nav::new();
        nav.open_settings(SettingsScreen::Appearance);
        assert!(matches!(
            nav.screen(),
            Screen::Settings(SettingsScreen::Appearance)
        ));

        assert!(nav.settings_back());
        assert!(matches!(nav.screen(), Screen::Settings(SettingsScreen::Root)));

        assert!(!nav.settings_back());
        assert!(matches!(nav.screen(), Screen::Chat));
    }

    #[test]
    fn dismiss_top_prefers_sheet_then_drawer_then_settings() {
        let mut nav = Nav::new();
        nav.open_settings(SettingsScreen::General);
        nav.open_sheet(Sheet::Actions);
        assert!(nav.dismiss_top());
        assert_eq!(nav.sheet(), None);
        assert!(nav.dismiss_top(), "settings page pops");
        assert!(nav.dismiss_top(), "root closes settings");
        assert!(!nav.dismiss_top(), "nothing left to dismiss");
    }

    #[test]
    fn swipes_need_distance_and_direction() {
        let mut nav = Nav::new();
        assert!(!nav.edge_swipe(20.0));
        assert!(nav.edge_swipe(60.0));
        assert!(nav.drawer_open());

        assert!(!nav.drawer_swipe(40.0));
        assert!(nav.drawer_swipe(-60.0));
        assert!(!nav.drawer_open());
    }

    #[test]
    fn filters_map_onto_session_status() {
        let idle = SessionStatus::default();
        let running = SessionStatus {
            running: true,
            ..Default::default()
        };
        let attention = SessionStatus {
            waiting_approval: true,
            ..Default::default()
        };
        assert!(matches_filter(Filter::All, &idle));
        assert!(!matches_filter(Filter::Running, &idle));
        assert!(matches_filter(Filter::Running, &running));
        assert!(matches_filter(Filter::Attention, &attention));
        assert!(!matches_filter(Filter::Unread, &attention));
    }

    #[test]
    fn search_matches_title_or_id_case_insensitively() {
        assert!(matches_query("", "anything", "x"));
        assert!(matches_query("READ", "readme notes", "x"));
        assert!(matches_query("abc", "title", "abc-123"));
        assert!(!matches_query("zzz", "title", "abc-123"));
    }
}
