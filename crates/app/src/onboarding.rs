//! The setup guide's presets ("Where are you coming from?") and the folder handling choice.
//!
//! A preset is a list of existing Arke settings set to the defaults of another file manager, so
//! someone moving from it finds Arke working the way they are used to. It sets exactly the fields
//! of Settings that the same options in Settings set: nothing here is a setting of its own.
//! Where each value comes from is in `docs/onboarding-presets.md`; only values the program's
//! documentation, its source code or its well-known out-of-the-box behavior back are mapped, and
//! where Directory Opus and XYplorer (or Total Commander and Double Commander) may differ, the
//! setting is left alone.
//!
//! The guide itself (the corner card) is `workspace_welcome.rs`.

use std::borrow::Cow;

use rust_i18n::t;

use crate::explorer::ViewMode;
use crate::settings::{
    FolderOrder, NavScope, NewTabAction, Settings, SortBy, StartLocation, TabLayout,
};

/// The file manager someone is coming from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// Windows File Explorer (Windows 11).
    Explorer,
    /// Total Commander and Double Commander.
    Commander,
    /// Directory Opus and XYplorer.
    Opus,
    /// macOS Finder.
    Finder,
}

impl Source {
    pub(crate) const ALL: [Self; 4] = [Self::Explorer, Self::Commander, Self::Opus, Self::Finder];

    /// The program names on its tile.
    pub(crate) fn label(self) -> Cow<'static, str> {
        match self {
            Self::Explorer => t!("onboarding.from_explorer"),
            // Product names: the same in every language.
            Self::Commander => Cow::Borrowed("Total / Double Commander"),
            Self::Opus => Cow::Borrowed("Directory Opus / XYplorer"),
            Self::Finder => Cow::Borrowed("macOS Finder"),
        }
    }

    /// The settings this preset sets, in the order they are listed.
    pub(crate) const fn items(self) -> &'static [Item] {
        match self {
            Self::Explorer => EXPLORER,
            Self::Commander => COMMANDER,
            Self::Opus => OPUS,
            Self::Finder => FINDER,
        }
    }

    /// Sets this preset's settings.
    pub(crate) fn apply(self, s: &mut Settings) {
        for item in self.items() {
            item.apply(s);
        }
    }

    /// What applying it would change: the items that differ from `s`.
    pub(crate) fn changes(self, s: &Settings) -> Vec<Item> {
        self.items()
            .iter()
            .copied()
            .filter(|item| !item.is_set(s))
            .collect()
    }
}

/// Windows 11 File Explorer. Extensions stay shown: Explorer hides them out of the box, but
/// that is left out on purpose (`docs/onboarding-presets.md`).
const EXPLORER: &[Item] = &[
    Item::SingleClick(false),
    Item::Folders(FolderOrder::First),
    Item::Sort(SortBy::Name, false),
    Item::View(ViewMode::Details),
    Item::ShowHidden(false),
    Item::ShowExtensions(true),
    Item::FullDates(true),
    Item::DecimalSizes(false),
    Item::Tabs(TabLayout::Horizontal),
    Item::NewTab(NewTabAction::Home),
    Item::Start(StartLocation::HomePage),
    Item::NavScope(NavScope::WithinTab),
    Item::SidebarFollow(false),
    Item::Checkboxes(false),
    Item::ConfirmRecycle(false),
];

/// Total Commander and Double Commander: the name without its extension, which has a column
/// of its own, and the attributes column; no Type column.
const COMMANDER: &[Item] = &[
    Item::SingleClick(false),
    Item::Folders(FolderOrder::AlwaysFirst),
    Item::Sort(SortBy::Name, false),
    Item::View(ViewMode::Details),
    Item::ShowHidden(false),
    Item::ShowExtensions(false),
    Item::ExtensionColumn(true),
    Item::TypeColumn(false),
    Item::AttributesColumn(true),
    Item::FullDates(true),
    Item::DecimalSizes(false),
    Item::Tabs(TabLayout::Horizontal),
    Item::NewTab(NewTabAction::CurrentFolder),
    Item::ConfirmRecycle(true),
];

/// Directory Opus and XYplorer: only what both do out of the box.
const OPUS: &[Item] = &[
    Item::SingleClick(false),
    Item::Folders(FolderOrder::AlwaysFirst),
    Item::Sort(SortBy::Name, false),
    Item::View(ViewMode::Details),
    Item::ShowExtensions(true),
    Item::FullDates(true),
    Item::Tabs(TabLayout::Horizontal),
];

/// macOS Finder: icons, folders mixed with files, extensions and dot files hidden, sizes in
/// base 1000, no folder sizes, new windows and tabs on Recents (Arke's Home page).
const FINDER: &[Item] = &[
    Item::SingleClick(false),
    Item::Folders(FolderOrder::Mixed),
    Item::Sort(SortBy::Name, false),
    Item::View(ViewMode::Icons),
    Item::ShowHidden(false),
    Item::HideDotfiles(true),
    Item::ShowExtensions(false),
    Item::FullDates(false),
    Item::DecimalSizes(true),
    Item::FolderSizes(false),
    Item::Tabs(TabLayout::Horizontal),
    Item::NewTab(NewTabAction::Home),
    Item::Start(StartLocation::HomePage),
    Item::ConfirmRecycle(false),
];

/// One setting a preset sets, with its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Item {
    SingleClick(bool),
    Folders(FolderOrder),
    /// Column and descending.
    Sort(SortBy, bool),
    View(ViewMode),
    ShowHidden(bool),
    HideDotfiles(bool),
    ShowExtensions(bool),
    ExtensionColumn(bool),
    TypeColumn(bool),
    AttributesColumn(bool),
    FullDates(bool),
    DecimalSizes(bool),
    FolderSizes(bool),
    Tabs(TabLayout),
    NewTab(NewTabAction),
    Start(StartLocation),
    NavScope(NavScope),
    SidebarFollow(bool),
    Checkboxes(bool),
    ConfirmRecycle(bool),
}

impl Item {
    /// Sets it, as its control in Settings does.
    pub(crate) const fn apply(self, s: &mut Settings) {
        match self {
            Self::SingleClick(v) => s.single_click = v,
            Self::Folders(v) => s.folders = v,
            Self::Sort(by, descending) => {
                s.sort_by = by;
                s.sort_descending = descending;
            }
            Self::View(v) => s.default_view = v,
            Self::ShowHidden(v) => s.show_hidden = v,
            Self::HideDotfiles(v) => s.hide_dotfiles = v,
            Self::ShowExtensions(v) => s.show_extensions = v,
            Self::ExtensionColumn(v) => s.show_extension = v,
            Self::TypeColumn(v) => s.show_type = v,
            Self::AttributesColumn(v) => s.show_attributes = v,
            Self::FullDates(v) => s.full_dates = v,
            Self::DecimalSizes(v) => s.decimal_sizes = v,
            Self::FolderSizes(v) => s.folder_sizes = v,
            Self::Tabs(v) => s.tab_layout = v,
            Self::NewTab(v) => s.new_tab = v,
            Self::Start(v) => s.start_location = v,
            Self::NavScope(v) => s.nav_scope = v,
            Self::SidebarFollow(v) => s.sidebar_follow = v,
            Self::Checkboxes(v) => s.show_checkboxes = v,
            Self::ConfirmRecycle(v) => s.confirm_recycle = v,
        }
    }

    /// Whether `s` already has this value.
    pub(crate) fn is_set(self, s: &Settings) -> bool {
        match self {
            Self::SingleClick(v) => s.single_click == v,
            Self::Folders(v) => s.folders == v,
            Self::Sort(by, descending) => s.sort_by == by && s.sort_descending == descending,
            Self::View(v) => s.default_view == v,
            Self::ShowHidden(v) => s.show_hidden == v,
            Self::HideDotfiles(v) => s.hide_dotfiles == v,
            Self::ShowExtensions(v) => s.show_extensions == v,
            Self::ExtensionColumn(v) => s.show_extension == v,
            Self::TypeColumn(v) => s.show_type == v,
            Self::AttributesColumn(v) => s.show_attributes == v,
            Self::FullDates(v) => s.full_dates == v,
            Self::DecimalSizes(v) => s.decimal_sizes == v,
            Self::FolderSizes(v) => s.folder_sizes == v,
            Self::Tabs(v) => s.tab_layout == v,
            Self::NewTab(v) => s.new_tab == v,
            Self::Start(v) => s.start_location == v,
            Self::NavScope(v) => s.nav_scope == v,
            Self::SidebarFollow(v) => s.sidebar_follow == v,
            Self::Checkboxes(v) => s.show_checkboxes == v,
            Self::ConfirmRecycle(v) => s.confirm_recycle == v,
        }
    }

    /// The setting's name, as Settings shows it.
    pub(crate) fn name(self) -> Cow<'static, str> {
        match self {
            Self::SingleClick(_) => t!("settings.single_click"),
            Self::Folders(_) => t!("settings.folders"),
            Self::Sort(..) => t!("settings.sort_by"),
            Self::View(_) => t!("settings.default_view"),
            Self::ShowHidden(_) => t!("settings.show_hidden"),
            Self::HideDotfiles(_) => t!("settings.hide_dotfiles"),
            Self::ShowExtensions(_) => t!("settings.show_extensions"),
            Self::ExtensionColumn(_) => column(t!("col.extension")),
            Self::TypeColumn(_) => column(t!("col.type")),
            Self::AttributesColumn(_) => column(t!("col.attributes")),
            Self::FullDates(_) => t!("settings.dates"),
            Self::DecimalSizes(_) => t!("settings.size_units"),
            Self::FolderSizes(_) => t!("settings.folder_sizes"),
            Self::Tabs(_) => t!("settings.tabs"),
            Self::NewTab(_) => t!("settings.new_tab"),
            Self::Start(_) => t!("settings.start"),
            Self::NavScope(_) => t!("settings.nav_scope"),
            Self::SidebarFollow(_) => t!("settings.sidebar_follow"),
            Self::Checkboxes(_) => t!("settings.checkboxes"),
            Self::ConfirmRecycle(_) => t!("settings.confirm_recycle"),
        }
    }

    /// The value, in the words of its choice in Settings.
    pub(crate) fn value(self) -> Cow<'static, str> {
        match self {
            Self::SingleClick(v)
            | Self::ShowHidden(v)
            | Self::HideDotfiles(v)
            | Self::ShowExtensions(v)
            | Self::ExtensionColumn(v)
            | Self::TypeColumn(v)
            | Self::AttributesColumn(v)
            | Self::FolderSizes(v)
            | Self::SidebarFollow(v)
            | Self::Checkboxes(v)
            | Self::ConfirmRecycle(v) => on_off(v),
            Self::Folders(FolderOrder::First) => t!("settings.folders_first"),
            Self::Folders(FolderOrder::AlwaysFirst) => t!("settings.folders_always_first"),
            Self::Folders(FolderOrder::Mixed) => t!("settings.folders_mixed"),
            Self::Sort(by, descending) => Cow::Owned(format!(
                "{}, {}",
                sort_name(by),
                if descending {
                    t!("settings.descending")
                } else {
                    t!("settings.ascending")
                }
            )),
            Self::View(ViewMode::Details) => t!("settings.view_details"),
            Self::View(ViewMode::Icons) => t!("settings.view_icons"),
            Self::View(ViewMode::Columns) => t!("settings.view_columns"),
            Self::FullDates(true) => t!("settings.dates_full"),
            Self::FullDates(false) => t!("settings.dates_short"),
            Self::DecimalSizes(true) => t!("settings.size_units_decimal"),
            Self::DecimalSizes(false) => t!("settings.size_units_binary"),
            Self::Tabs(TabLayout::Horizontal) => t!("settings.tabs_top"),
            Self::Tabs(TabLayout::Vertical) => t!("settings.tabs_side"),
            Self::NewTab(NewTabAction::GoToFolder) => t!("settings.new_tab_box"),
            Self::NewTab(NewTabAction::Home) => t!("settings.new_tab_home"),
            Self::NewTab(NewTabAction::CurrentFolder) => t!("settings.new_tab_here"),
            Self::Start(StartLocation::HomePage) => t!("home.title"),
            Self::Start(StartLocation::Home) => t!("settings.start_home"),
            Self::Start(StartLocation::LastFolder) => t!("settings.start_last"),
            Self::Start(StartLocation::LastSession) => t!("settings.start_session"),
            Self::NavScope(NavScope::AcrossTabs) => t!("settings.nav_scope_across"),
            Self::NavScope(NavScope::WithinTab) => t!("settings.nav_scope_within"),
        }
    }
}

fn column(name: Cow<'static, str>) -> Cow<'static, str> {
    Cow::Owned(t!("onboarding.column", name = name).into_owned())
}

fn on_off(on: bool) -> Cow<'static, str> {
    if on {
        t!("onboarding.on")
    } else {
        t!("onboarding.off")
    }
}

fn sort_name(by: SortBy) -> Cow<'static, str> {
    match by {
        SortBy::Name => t!("col.name"),
        SortBy::Modified => t!("col.modified"),
        SortBy::Type => t!("col.type"),
        SortBy::Size => t!("col.size"),
        SortBy::Created => t!("col.created"),
        SortBy::Extension => t!("col.extension"),
        SortBy::Attributes => t!("col.attributes"),
    }
}

/// How Arke takes over from File Explorer (the guide's third step). The same three switches as
/// Settings › Startup, with the same rules: taking over needs Arke to keep running in the
/// background, and Win+E only means something while Arke opens folders.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Takeover {
    /// File Explorer keeps opening folders; nothing is registered.
    #[default]
    Off,
    /// "Open folders in Arke": folders other programs open come to Arke.
    Folders,
    /// Also Win+E and the File Explorer icon.
    FoldersAndWinE,
}

impl Takeover {
    pub(crate) const ALL: [Self; 3] = [Self::Off, Self::Folders, Self::FoldersAndWinE];

    /// What the settings have now.
    pub(crate) const fn of(s: &Settings) -> Self {
        match (s.open_folders_in_arke, s.win_e_opens_arke) {
            (false, _) => Self::Off,
            (true, false) => Self::Folders,
            (true, true) => Self::FoldersAndWinE,
        }
    }

    /// Whether Arke has to keep running in the background for it.
    pub(crate) const fn needs_background(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// The guide's third step, held until it is confirmed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Connect {
    pub(crate) takeover: Takeover,
    pub(crate) keep_running: bool,
    /// `None` where the installer made its own shortcut and the row isn't offered.
    pub(crate) start_menu: Option<bool>,
    pub(crate) autostart: bool,
}

impl Connect {
    /// The current settings; the Start menu shortcut is proposed (on) until it has been offered
    /// once, as the first-run card did.
    pub(crate) const fn from_settings(s: &Settings, offer_start_menu: bool) -> Self {
        Self {
            takeover: Takeover::of(s),
            keep_running: s.keep_running,
            start_menu: if offer_start_menu {
                Some(s.start_menu || !s.start_menu_offered)
            } else {
                None
            },
            autostart: s.autostart,
        }
    }

    /// Whether Arke will keep running in the background: as ticked, and always while it takes
    /// over from File Explorer.
    pub(crate) const fn background(&self) -> bool {
        self.keep_running || self.takeover.needs_background()
    }

    /// Writes it to the settings, as the switches in Settings › Startup would.
    pub(crate) const fn apply(&self, s: &mut Settings) {
        s.keep_running = self.background();
        s.open_folders_in_arke = self.takeover.needs_background();
        s.win_e_opens_arke = matches!(self.takeover, Takeover::FoldersAndWinE);
        if let Some(start_menu) = self.start_menu {
            s.start_menu = start_menu;
            s.start_menu_offered = true;
        }
        s.autostart = self.autostart;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The keys of settings.json whose values differ.
    fn changed_keys(before: &Settings, after: &Settings) -> Vec<String> {
        let (serde_json::Value::Object(a), serde_json::Value::Object(b)) = (
            serde_json::to_value(before).unwrap(),
            serde_json::to_value(after).unwrap(),
        ) else {
            panic!("settings are an object");
        };
        let mut keys: Vec<String> = a
            .iter()
            .filter(|(k, v)| b.get(*k) != Some(v))
            .map(|(k, _)| k.clone())
            .collect();
        keys.sort();
        keys
    }

    /// The settings.json keys an item writes.
    fn keys_of(item: Item) -> &'static [&'static str] {
        match item {
            Item::SingleClick(_) => &["single_click"],
            Item::Folders(_) => &["folders"],
            Item::Sort(..) => &["sort_by", "sort_descending"],
            Item::View(_) => &["default_view"],
            Item::ShowHidden(_) => &["show_hidden"],
            Item::HideDotfiles(_) => &["hide_dotfiles"],
            Item::ShowExtensions(_) => &["show_extensions"],
            Item::ExtensionColumn(_) => &["show_extension"],
            Item::TypeColumn(_) => &["show_type"],
            Item::AttributesColumn(_) => &["show_attributes"],
            Item::FullDates(_) => &["full_dates"],
            Item::DecimalSizes(_) => &["decimal_sizes"],
            Item::FolderSizes(_) => &["folder_sizes"],
            Item::Tabs(_) => &["tab_layout"],
            Item::NewTab(_) => &["new_tab"],
            Item::Start(_) => &["start_location"],
            Item::NavScope(_) => &["nav_scope"],
            Item::SidebarFollow(_) => &["sidebar_follow"],
            Item::Checkboxes(_) => &["show_checkboxes"],
            Item::ConfirmRecycle(_) => &["confirm_recycle"],
        }
    }

    /// Settings as every preset in turn left them: a start that differs from the defaults in
    /// most of the fields the presets set.
    fn opposite() -> Settings {
        let mut s = Settings::default();
        for source in Source::ALL {
            for item in source.items() {
                item.apply(&mut s);
            }
        }
        s
    }

    #[test]
    fn each_preset_sets_exactly_its_settings() {
        for source in Source::ALL {
            for start in [Settings::default(), opposite()] {
                let mut after = start.clone();
                source.apply(&mut after);
                let mut expected: Vec<String> = source
                    .changes(&start)
                    .into_iter()
                    .flat_map(|item| keys_of(item).iter().map(|k| (*k).to_owned()))
                    .collect();
                expected.sort();
                expected.dedup();
                // Some keys may already match within an item (Sort's direction).
                let changed = changed_keys(&start, &after);
                for key in &changed {
                    assert!(expected.contains(key), "{source:?} changed {key}");
                }
                for item in source.items() {
                    assert!(item.is_set(&after), "{source:?}: {item:?}");
                }
                assert!(source.changes(&after).is_empty(), "{source:?} applies once");
            }
        }
    }

    #[test]
    fn a_preset_lists_each_setting_once() {
        for source in Source::ALL {
            let mut keys: Vec<&str> = source
                .items()
                .iter()
                .flat_map(|item| keys_of(*item).iter().copied())
                .collect();
            let all = keys.len();
            keys.sort_unstable();
            keys.dedup();
            assert_eq!(keys.len(), all, "{source:?} sets a setting twice");
        }
    }

    #[test]
    fn the_presets_differ_where_the_programs_do() {
        let applied = |source: Source| {
            let mut s = Settings::default();
            source.apply(&mut s);
            s
        };
        let explorer = applied(Source::Explorer);
        assert!(explorer.show_extensions, "kept shown on purpose");
        assert!(!explorer.show_hidden);
        assert_eq!(explorer.folders, FolderOrder::First);
        assert_eq!(explorer.tab_layout, TabLayout::Horizontal);
        assert_eq!(explorer.nav_scope, NavScope::WithinTab);
        let commander = applied(Source::Commander);
        assert!(commander.show_extension && commander.show_attributes && !commander.show_type);
        assert!(commander.confirm_recycle);
        assert_eq!(commander.folders, FolderOrder::AlwaysFirst);
        assert_eq!(commander.new_tab, NewTabAction::CurrentFolder);
        let opus = applied(Source::Opus);
        assert!(opus.show_extensions && opus.full_dates);
        assert_eq!(opus.folders, FolderOrder::AlwaysFirst);
        let finder = applied(Source::Finder);
        assert_eq!(finder.default_view, ViewMode::Icons);
        assert_eq!(finder.folders, FolderOrder::Mixed);
        assert!(finder.decimal_sizes && finder.hide_dotfiles && !finder.folder_sizes);
    }

    #[test]
    fn every_value_has_words() {
        for source in Source::ALL {
            assert!(!source.label().is_empty());
            for item in source.items() {
                assert!(!item.name().starts_with("settings."), "{item:?}");
                assert!(!item.value().starts_with("settings."), "{item:?}");
            }
        }
    }

    #[test]
    fn connect_follows_the_startup_rules() {
        let mut s = Settings::default();
        let mut c = Connect::from_settings(&s, true);
        assert_eq!(c.takeover, Takeover::Off);
        assert_eq!(c.start_menu, Some(true), "proposed until offered");
        c.keep_running = false;
        c.takeover = Takeover::FoldersAndWinE;
        c.apply(&mut s);
        assert!(s.open_folders_in_arke && s.win_e_opens_arke && s.keep_running);
        assert!(s.start_menu && s.start_menu_offered);
        assert_eq!(Takeover::of(&s), Takeover::FoldersAndWinE);

        c.takeover = Takeover::Off;
        c.start_menu = Some(false);
        c.apply(&mut s);
        assert!(!s.open_folders_in_arke && !s.win_e_opens_arke && !s.keep_running);
        assert!(!s.start_menu && s.start_menu_offered);
        assert_eq!(Connect::from_settings(&s, true).start_menu, Some(false));
        assert_eq!(Connect::from_settings(&s, false).start_menu, None);
    }
}
