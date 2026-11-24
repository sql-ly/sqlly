//! Application menu bar construction. Actions remain stable across languages;
//! only their labels come from the active catalog.

use gpui::{Menu, MenuItem, OsAction, SystemMenuType};

use crate::app::{
    About, AddFirewallRule, AppStoreEdition, AskSchema, AzureSqlConnection, BringAllToFront,
    CancelActiveQuery, ChartDashboardTool, CheckForUpdates, CloseSplit, CloseTab, Connect, Copy,
    Cut, Disconnect, EstimatedPlanViewer, ExecuteActiveQuery, ExecuteActiveQueryKeepResults,
    ExecutionPlanViewer, ExportConnections, ExportCredentials, ExportForAzureDataStudio,
    ExportForDataGrip, ExportForDbGate, ExportForDbeaver, ExportForPgAdmin, ExportForPgService,
    FormatActiveQuery, FormatDirectivesReference, GenerateSql, ImportConnections,
    ImportConnectionsFromTools, ImportFromClipboard, ManageWorkspaces, MinifyActiveQuery,
    MinimizeWindow, NewQueryTab, NewWindow, NextEditorTab, ObjectBrowserTool, OpenFolder,
    OpenKeybindings, OpenObjectProperties, OpenPreferences, OpenProjectFile, OpenSourceLicenses,
    OpenSql, OpenSqlFile, OpenSqllyExport, OpenWorkspace, Paste, PreviousEditorTab, PrivacyPolicy,
    QuickOpen, Quit, Redo, RefreshObjectExplorer, ReopenClosedTab, ResultSnapshots, RunOnTargets,
    RunSqlFile, SaveResultsCsv, SaveSql, SaveToLibrary, SaveWorkspaceAs, SavedCharts, SearchSchema,
    SelectAll, SendFeedback, ShowConnectionManagement, ShowEntityManagement, ShowSqlLibrary,
    SplitHorizontal, SplitSameBufferHorizontal, SplitSameBufferVertical, SplitVertical, SqllyHelp,
    TemplateExplorer, ThemeDark, ThemeHighContrast, ThemeLight, ThemeSystem, TipOfTheDay,
    ToggleAiPanel, ToggleCommandPalette, ToggleIntelliSenseDebug, ToggleLogConsole,
    ToggleResultsPane, ToggleSidebar, Undo, UnlockSqlly, ValidateActiveQuery, ZoomEditorIn,
    ZoomEditorOut, ZoomEditorReset, ZoomWindow,
};
use crate::app::{
    ConvertNotebookToScript, NewNotebook, OpenAsNotebook, OpenNotebook, RunAllNotebookCells,
    RunNotebookCell, SaveNotebook, StopNotebookRun,
};
use crate::editor::{
    AddCursorAbove, AddCursorBelow, AddCursorForNextOccurrence, AddLineComment,
    ConvertDelimitedList, End, Find, FindNext, FindPrev, FormatDocument, Home,
    InsertDistinctValuesAsInList, PageDown, PageUp, PasteAsInList, RemoveLineComment, Replace,
    SelectAllOccurrences, SelectEnd, SelectHome, SelectPageDown, SelectPageUp, ToggleBlockComment,
    ToggleLineComment, TriggerCompletions, TriggerValueLookup,
};
use crate::theme::ThemeKind;

macro_rules! action {
    ($key:expr, $action:expr) => {
        MenuItem::action(crate::localization::text($key), $action)
    };
}

macro_rules! os_action {
    ($key:literal, $action:expr, $os_action:expr) => {
        MenuItem::os_action(crate::localization::text($key), $action, $os_action)
    };
}

pub fn build_menus(connected: bool) -> Vec<Menu> {
    if cfg!(target_os = "macos") {
        build_macos_menus(connected)
    } else {
        build_windows_menus(connected)
    }
}

fn menu(name: &'static str, items: Vec<MenuItem>) -> Menu {
    Menu {
        name: crate::localization::text(name).into(),
        disabled: false,
        items,
    }
}

fn build_macos_menus(connected: bool) -> Vec<Menu> {
    vec![
        menu(
            "menu.application.title",
            vec![
                action!("menu.application.about", About),
                if crate::distribution::checks_for_updates() {
                    action!("menu.application.check_updates", CheckForUpdates)
                } else {
                    action!("menu.application.unlock", UnlockSqlly)
                },
                MenuItem::separator(),
                action!("menu.application.preferences", OpenPreferences),
                MenuItem::separator(),
                MenuItem::os_submenu(
                    crate::localization::text("menu.application.services"),
                    SystemMenuType::Services,
                ),
                MenuItem::separator(),
                action!("menu.application.quit", Quit),
            ],
        ),
        file_menu(FileMenuStyle::MacOs),
        edit_menu(),
        view_menu(ThemeMenuState::current()),
        query_menu(),
        tools_menu(connected),
        window_menu(WindowMenuStyle::MacOs),
        help_menu(HelpMenuStyle::MacOs),
    ]
}

fn build_windows_menus(connected: bool) -> Vec<Menu> {
    vec![
        file_menu(FileMenuStyle::Windows),
        edit_menu(),
        view_menu(ThemeMenuState::current()),
        query_menu(),
        tools_menu(connected),
        window_menu(WindowMenuStyle::Windows),
        help_menu(HelpMenuStyle::Windows),
    ]
}

#[derive(PartialEq)]
enum FileMenuStyle {
    MacOs,
    Windows,
}

fn file_menu(style: FileMenuStyle) -> Menu {
    let mut items = vec![
        action!("menu.file.new_query", NewQueryTab),
        action!("menu.file.new_window", NewWindow),
        action!("notebook.shell.menu.new_notebook", NewNotebook),
        MenuItem::separator(),
        action!("menu.file.open_sql", OpenSql),
        action!("menu.file.open_sql_file", OpenSqlFile),
        action!("notebook.shell.menu.open_notebook", OpenNotebook),
        #[cfg(not(target_arch = "wasm32"))]
        action!("menu.file.run_sql_file", RunSqlFile),
        action!("menu.file.open_project_file", OpenProjectFile),
        action!("menu.file.open_export", OpenSqllyExport),
        #[cfg(not(target_arch = "wasm32"))]
        action!("menu.file.export_connections", ExportConnections),
        #[cfg(not(target_arch = "wasm32"))]
        action!("menu.file.export_credentials", ExportCredentials),
        #[cfg(not(target_arch = "wasm32"))]
        export_for_other_tools_menu(),
        #[cfg(not(target_arch = "wasm32"))]
        action!("menu.file.import_connections", ImportConnections),
        #[cfg(not(target_arch = "wasm32"))]
        action!("menu.file.import_other_tools", ImportConnectionsFromTools),
        action!("menu.file.open_folder", OpenFolder),
        MenuItem::separator(),
        action!("menu.file.open_workspace", OpenWorkspace),
        action!("menu.file.save_workspace_as", SaveWorkspaceAs),
        action!("menu.file.manage_workspaces", ManageWorkspaces),
        MenuItem::separator(),
        action!("menu.file.save_sql", SaveSql),
        action!("notebook.shell.menu.save_notebook", SaveNotebook),
        action!("menu.file.save_library", SaveToLibrary),
        action!("menu.file.save_results", SaveResultsCsv),
        MenuItem::separator(),
        action!("menu.file.close", CloseTab),
    ];
    if style == FileMenuStyle::Windows {
        items.push(MenuItem::separator());
        items.push(action!("menu.file.exit", Quit));
    }
    menu("menu.file.title", items)
}

/// File ▸ Export for Other Tools: one item per tool, each writing the
/// picked connections in the file format that tool imports.
#[cfg(not(target_arch = "wasm32"))]
fn export_for_other_tools_menu() -> MenuItem {
    MenuItem::submenu(menu(
        "menu.file.export_other_tools",
        vec![
            action!("menu.file.export_other_tools_pgadmin", ExportForPgAdmin),
            action!(
                "menu.file.export_other_tools_azure_data_studio",
                ExportForAzureDataStudio
            ),
            action!("menu.file.export_other_tools_dbeaver", ExportForDbeaver),
            action!("menu.file.export_other_tools_datagrip", ExportForDataGrip),
            action!("menu.file.export_other_tools_dbgate", ExportForDbGate),
            action!(
                "menu.file.export_other_tools_pg_service",
                ExportForPgService
            ),
        ],
    ))
}

fn edit_menu() -> Menu {
    menu(
        "menu.edit.title",
        vec![
            os_action!("menu.edit.undo", Undo, OsAction::Undo),
            os_action!("menu.edit.redo", Redo, OsAction::Redo),
            MenuItem::separator(),
            os_action!("menu.edit.cut", Cut, OsAction::Cut),
            os_action!("menu.edit.copy", Copy, OsAction::Copy),
            os_action!("menu.edit.paste", Paste, OsAction::Paste),
            os_action!("menu.edit.select_all", SelectAll, OsAction::SelectAll),
            MenuItem::separator(),
            action!("menu.edit.find", Find),
            action!("menu.edit.replace", Replace),
            action!("menu.edit.find_next", FindNext),
            action!("menu.edit.find_previous", FindPrev),
            MenuItem::separator(),
            action!(
                "menu.edit.add_cursor_next_occurrence",
                AddCursorForNextOccurrence
            ),
            action!("menu.edit.select_all_occurrences", SelectAllOccurrences),
            action!("menu.edit.add_cursor_above", AddCursorAbove),
            action!("menu.edit.add_cursor_below", AddCursorBelow),
            MenuItem::separator(),
            action!("menu.edit.trigger_suggest", TriggerCompletions),
            action!("menu.edit.value_autocomplete", TriggerValueLookup),
            action!(
                "menu.edit.insert_distinct_in_list",
                InsertDistinctValuesAsInList
            ),
            action!("menu.edit.paste_in_list", PasteAsInList),
            action!("menu.edit.convert_delimited_list", ConvertDelimitedList),
            MenuItem::separator(),
            action!("menu.edit.format_document", FormatDocument),
            MenuItem::separator(),
            action!("menu.edit.toggle_line_comment", ToggleLineComment),
            action!("menu.edit.add_line_comment", AddLineComment),
            action!("menu.edit.remove_line_comment", RemoveLineComment),
            action!("menu.edit.toggle_block_comment", ToggleBlockComment),
            MenuItem::separator(),
            action!("menu.edit.move_end", End),
            action!("menu.edit.move_beginning", Home),
            action!("menu.edit.select_end", SelectEnd),
            action!("menu.edit.select_beginning", SelectHome),
            action!("menu.edit.page_down", PageDown),
            action!("menu.edit.page_up", PageUp),
            action!("menu.edit.select_page_down", SelectPageDown),
            action!("menu.edit.select_page_up", SelectPageUp),
        ],
    )
}

fn view_menu(theme: ThemeMenuState) -> Menu {
    menu(
        "menu.view.title",
        vec![
            action!("menu.view.command_palette", ToggleCommandPalette),
            action!("menu.view.go_to_object", QuickOpen),
            action!("menu.view.toggle_explorer", ToggleSidebar),
            action!("menu.view.show_sql_library", ShowSqlLibrary),
            action!("menu.view.search_schema", SearchSchema),
            action!("menu.view.toggle_results", ToggleResultsPane),
            action!(
                "menu.view.toggle_intellisense_debug",
                ToggleIntelliSenseDebug
            ),
            action!("menu.view.toggle_log", ToggleLogConsole),
            MenuItem::separator(),
            action!("menu.view.split_vertical", SplitVertical),
            action!("menu.view.split_horizontal", SplitHorizontal),
            action!("menu.view.split_same_vertical", SplitSameBufferVertical),
            action!("menu.view.split_same_horizontal", SplitSameBufferHorizontal),
            action!("menu.view.close_split", CloseSplit),
            MenuItem::separator(),
            action!("menu.view.zoom_in", ZoomEditorIn),
            action!("menu.view.zoom_out", ZoomEditorOut),
            action!("menu.view.actual_size", ZoomEditorReset),
            MenuItem::separator(),
            theme_menu(theme),
        ],
    )
}

/// What View ▸ Theme checks: the chosen mode, and whether the palette that
/// mode paints right now is High Contrast. Also the fingerprint the
/// workbench compares to know the installed menus have gone stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ThemeMenuState {
    pub mode: ThemeKind,
    pub high_contrast: bool,
}

impl ThemeMenuState {
    pub(crate) fn current() -> Self {
        use crate::theme::ColorTheme;
        let painted = match crate::theme::resolved_kind() {
            ThemeKind::Light => crate::theme::light_theme(),
            _ => crate::theme::dark_theme(),
        };
        Self {
            // Legacy High Contrast mode paints as Dark + High Contrast Dark.
            mode: match crate::theme::kind() {
                ThemeKind::HighContrast => ThemeKind::Dark,
                kind => kind,
            },
            high_contrast: matches!(
                painted,
                ColorTheme::HighContrastLight | ColorTheme::HighContrastDark
            ),
        }
    }
}

/// View ▸ Theme: the three modes, then the High Contrast palette, which
/// layers on whichever mode is chosen.
fn theme_menu(theme: ThemeMenuState) -> MenuItem {
    MenuItem::submenu(menu(
        "menu.view.theme",
        vec![
            action!("menu.view.theme_system", ThemeSystem).checked(theme.mode == ThemeKind::System),
            action!("menu.view.theme_light", ThemeLight).checked(theme.mode == ThemeKind::Light),
            action!("menu.view.theme_dark", ThemeDark).checked(theme.mode == ThemeKind::Dark),
            MenuItem::separator(),
            action!("menu.view.theme_high_contrast", ThemeHighContrast)
                .checked(theme.high_contrast),
        ],
    ))
}

fn query_menu() -> Menu {
    menu(
        "menu.query.title",
        vec![
            action!("menu.query.execute", ExecuteActiveQuery),
            action!(
                "menu.query.execute_new_result_tab",
                ExecuteActiveQueryKeepResults
            ),
            action!("menu.query.run_multiple", RunOnTargets),
            action!("menu.query.explain_actual", ExecutionPlanViewer),
            action!("menu.query.explain_estimated", EstimatedPlanViewer),
            MenuItem::separator(),
            action!("notebook.shell.menu.run_cell", RunNotebookCell),
            action!("notebook.shell.menu.run_all", RunAllNotebookCells),
            action!("notebook.shell.menu.stop", StopNotebookRun),
            action!("notebook.shell.menu.open_as_notebook", OpenAsNotebook),
            action!(
                "notebook.shell.menu.convert_to_script",
                ConvertNotebookToScript
            ),
            MenuItem::separator(),
            action!("menu.query.generate_sql", GenerateSql),
            action!("menu.query.ask_schema", AskSchema),
            action!("menu.query.toggle_ai", ToggleAiPanel),
            MenuItem::separator(),
            action!("menu.query.validate", ValidateActiveQuery),
            action!("menu.query.format", FormatActiveQuery),
            action!("menu.query.minify", MinifyActiveQuery),
            action!("menu.query.cancel", CancelActiveQuery),
        ],
    )
}

fn tools_menu(connected: bool) -> Menu {
    let connection_item = if connected {
        action!("menu.tools.disconnect", Disconnect)
    } else {
        action!("menu.tools.connect", Connect)
    };
    menu(
        "menu.tools.title",
        vec![
            action!("menu.tools.refresh_explorer", RefreshObjectExplorer),
            action!("menu.tools.go_to_object", QuickOpen),
            MenuItem::separator(),
            connection_item,
            action!("menu.tools.connection_management", ShowConnectionManagement),
            action!("menu.tools.azure_connection", AzureSqlConnection),
            action!("menu.tools.add_firewall_ip", AddFirewallRule),
            MenuItem::separator(),
            action!("menu.tools.manage_entities", ShowEntityManagement),
            MenuItem::separator(),
            action!("menu.tools.template_explorer", TemplateExplorer),
            action!("menu.tools.object_properties", OpenObjectProperties),
            action!("menu.tools.import_clipboard", ImportFromClipboard),
            action!("menu.tools.object_browser", ObjectBrowserTool),
            action!("menu.tools.result_snapshots", ResultSnapshots),
            action!("menu.tools.saved_charts", SavedCharts),
            action!("menu.tools.chart_dashboard", ChartDashboardTool),
            MenuItem::separator(),
            action!(
                if cfg!(target_os = "macos") {
                    "menu.tools.preferences"
                } else {
                    "menu.tools.options"
                },
                OpenPreferences
            ),
            action!("menu.tools.keyboard_shortcuts", OpenKeybindings),
        ],
    )
}

#[derive(PartialEq)]
enum WindowMenuStyle {
    MacOs,
    Windows,
}

fn window_menu(style: WindowMenuStyle) -> Menu {
    let mut items = vec![
        action!("menu.window.minimize", MinimizeWindow),
        action!(
            if style == WindowMenuStyle::MacOs {
                "menu.window.zoom"
            } else {
                "menu.window.maximize"
            },
            ZoomWindow
        ),
        MenuItem::separator(),
        action!("menu.window.close_tab", CloseTab),
        action!("menu.window.reopen_closed_tab", ReopenClosedTab),
        action!("menu.window.next_tab", NextEditorTab),
        action!("menu.window.previous_tab", PreviousEditorTab),
    ];
    if style == WindowMenuStyle::MacOs {
        items.push(MenuItem::separator());
        items.push(action!("menu.window.bring_all_front", BringAllToFront));
    }
    menu("menu.window.title", items)
}

#[derive(PartialEq)]
enum HelpMenuStyle {
    MacOs,
    Windows,
}

fn help_menu(style: HelpMenuStyle) -> Menu {
    let mut items = vec![
        action!("menu.help.sqlly_help", SqllyHelp),
        action!("menu.help.show_commands", ToggleCommandPalette),
        action!("menu.help.formatting_directives", FormatDirectivesReference),
        action!("menu.help.send_feedback", SendFeedback),
        action!("menu.help.tip_of_day", TipOfTheDay),
        action!("menu.help.privacy_policy", PrivacyPolicy),
        MenuItem::separator(),
    ];
    match style {
        HelpMenuStyle::MacOs => {
            if crate::distribution::is_app_store() {
                items.push(action!("menu.help.app_store_edition", AppStoreEdition));
            }
            items.push(action!("menu.help.about", About));
            items.push(action!(
                "menu.help.open_source_licenses",
                OpenSourceLicenses
            ));
        }
        HelpMenuStyle::Windows => {
            items.push(action!("menu.help.check_updates", CheckForUpdates));
            items.push(MenuItem::separator());
            items.push(action!(
                "menu.help.open_source_licenses",
                OpenSourceLicenses
            ));
            items.push(MenuItem::separator());
            items.push(action!("menu.help.about", About));
        }
    }
    menu("menu.help.title", items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action_names(menu: &Menu) -> Vec<String> {
        menu.items
            .iter()
            .filter_map(|item| match item {
                MenuItem::Action { name, .. } => Some(name.to_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn help_information_actions_have_the_same_labels_on_every_platform() {
        for style in [HelpMenuStyle::MacOs, HelpMenuStyle::Windows] {
            let menu = help_menu(style);
            for (action, key) in [
                (Box::new(About) as Box<dyn gpui::Action>, "menu.help.about"),
                (
                    Box::new(OpenSourceLicenses),
                    "menu.help.open_source_licenses",
                ),
            ] {
                let labels: Vec<_> = menu
                    .items
                    .iter()
                    .filter_map(|item| match item {
                        MenuItem::Action {
                            name,
                            action: candidate,
                            ..
                        } if candidate.as_ref().type_id() == action.as_ref().type_id() => {
                            Some(name.to_string())
                        }
                        _ => None,
                    })
                    .collect();
                assert_eq!(labels, vec![crate::localization::text(key)]);
            }
        }
    }

    #[test]
    fn file_menu_has_close_and_open_sql_commands() {
        let names = action_names(&file_menu(FileMenuStyle::MacOs));
        for key in [
            "menu.file.close",
            "menu.file.open_sql",
            "menu.file.open_sql_file",
            "menu.file.open_project_file",
            "menu.file.open_export",
            "menu.file.save_sql",
            "menu.file.save_library",
        ] {
            assert!(
                names.contains(&crate::localization::text(key)),
                "{key}: {names:?}"
            );
        }
    }

    #[test]
    fn file_and_query_menus_carry_the_notebook_items() {
        for style in [FileMenuStyle::MacOs, FileMenuStyle::Windows] {
            let names = action_names(&file_menu(style));
            for key in [
                "notebook.shell.menu.new_notebook",
                "notebook.shell.menu.open_notebook",
                "notebook.shell.menu.save_notebook",
            ] {
                assert!(
                    names.contains(&crate::localization::text(key)),
                    "{key}: {names:?}"
                );
            }
        }
        let names = action_names(&query_menu());
        for key in [
            "notebook.shell.menu.run_cell",
            "notebook.shell.menu.run_all",
            "notebook.shell.menu.stop",
            "notebook.shell.menu.open_as_notebook",
            "notebook.shell.menu.convert_to_script",
        ] {
            assert!(
                names.contains(&crate::localization::text(key)),
                "{key}: {names:?}"
            );
        }
    }

    #[test]
    fn view_menu_has_sql_library() {
        let names = action_names(&view_menu(ThemeMenuState {
            mode: ThemeKind::System,
            high_contrast: false,
        }));
        assert!(names.contains(&crate::localization::text("menu.view.show_sql_library")));
    }

    /// The checks follow the painted palette: High Contrast is on only when
    /// the palette the resolved mode paints is a High Contrast one, System
    /// follows the OS, and the legacy High Contrast mode reads as Dark.
    #[test]
    fn theme_menu_state_follows_the_painted_palette() {
        use crate::theme::{
            ColorTheme, THEME_LOCK, set_dark_theme, set_kind, set_light_theme, set_system_dark,
        };
        let _guard = THEME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state = |mode, high_contrast| ThemeMenuState {
            mode,
            high_contrast,
        };

        set_light_theme(ColorTheme::Light);
        set_dark_theme(ColorTheme::HighContrastDark);
        set_kind(ThemeKind::Light);
        assert_eq!(
            ThemeMenuState::current(),
            state(ThemeKind::Light, false),
            "a Dark High Contrast palette is not painted in Light"
        );
        set_kind(ThemeKind::Dark);
        assert_eq!(ThemeMenuState::current(), state(ThemeKind::Dark, true));

        set_kind(ThemeKind::System);
        set_system_dark(true);
        assert_eq!(ThemeMenuState::current(), state(ThemeKind::System, true));
        set_system_dark(false);
        assert_eq!(ThemeMenuState::current(), state(ThemeKind::System, false));

        set_kind(ThemeKind::HighContrast);
        assert_eq!(ThemeMenuState::current(), state(ThemeKind::Dark, true));

        set_dark_theme(ColorTheme::Dark);
        set_kind(ThemeKind::Dark);
    }

    /// The four theme items live in one View ▸ Theme submenu, not loose in
    /// View, and only the chosen mode (plus High Contrast when on) is checked.
    #[test]
    fn theme_choices_are_one_submenu_checking_the_current_theme() {
        let _guard = crate::localization::test_language(crate::localization::Language::English);
        let theme = |mode, high_contrast| {
            let view = view_menu(ThemeMenuState {
                mode,
                high_contrast,
            });
            let loose = action_names(&view);
            assert!(
                !loose
                    .iter()
                    .any(|n| n == &crate::localization::text("menu.view.theme_dark")),
                "{loose:?}"
            );
            let submenus: Vec<_> = view
                .items
                .iter()
                .filter_map(|item| match item {
                    MenuItem::Submenu(submenu) => Some(submenu),
                    _ => None,
                })
                .filter(|submenu| submenu.name == crate::localization::text("menu.view.theme"))
                .collect();
            assert_eq!(submenus.len(), 1);
            submenus[0]
                .items
                .iter()
                .filter_map(|item| match item {
                    MenuItem::Action { name, checked, .. } => Some((name.to_string(), *checked)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let row = |label: &str, checked: bool| (label.to_string(), checked);
        assert_eq!(
            theme(ThemeKind::System, false),
            vec![
                row("System", true),
                row("Light", false),
                row("Dark", false),
                row("High Contrast", false),
            ]
        );
        assert_eq!(
            theme(ThemeKind::Dark, true),
            vec![
                row("System", false),
                row("Light", false),
                row("Dark", true),
                row("High Contrast", true),
            ]
        );
    }

    #[test]
    fn tools_menu_leads_with_the_connection_items() {
        let names = action_names(&tools_menu(false));
        let connect = names
            .iter()
            .position(|n| n == &crate::localization::text("menu.tools.connect"))
            .unwrap();
        let management = names
            .iter()
            .position(|n| n == &crate::localization::text("menu.tools.connection_management"))
            .unwrap();
        let browser = names
            .iter()
            .position(|n| n == &crate::localization::text("menu.tools.object_browser"))
            .unwrap();
        assert!(connect < management);
        assert!(management < browser);
    }

    #[test]
    fn connection_item_matches_connection_state() {
        let disconnected = action_names(&tools_menu(false));
        assert!(disconnected.contains(&crate::localization::text("menu.tools.connect")));
        assert!(!disconnected.contains(&crate::localization::text("menu.tools.disconnect")));
        let connected = action_names(&tools_menu(true));
        assert!(connected.contains(&crate::localization::text("menu.tools.disconnect")));
        assert!(!connected.contains(&crate::localization::text("menu.tools.connect")));
    }

    #[test]
    fn menu_labels_follow_the_active_language_without_changing_structure() {
        let _guard = crate::localization::test_language(crate::localization::Language::English);
        let english = build_windows_menus(false);
        crate::localization::activate(crate::localization::Language::Spanish, None).unwrap();
        let spanish = build_windows_menus(false);
        assert_eq!(english.len(), spanish.len());
        assert_ne!(english[0].name, spanish[0].name);
        crate::localization::activate(crate::localization::Language::English, None).unwrap();
    }
    /// Menu label without a trailing ellipsis, so "Foo..." and "Foo…" compare
    /// equal.
    fn bare(label: &str) -> &str {
        label.trim_end_matches("...").trim_end_matches('…')
    }

    /// The server- and database-scoped tools moved to the explorer's
    /// right-click Tools submenu, where the node supplies the connection. They
    /// must not reappear here, opening against whatever is active.
    #[test]
    fn node_scoped_tools_are_not_in_the_application_menu() {
        for connected in [false, true] {
            let names = action_names(&tools_menu(connected));
            for moved in [
                "Backup / Restore",
                "Server Dashboard",
                "Activity Monitor",
                "SQL Agent",
                "Security",
                "Designers",
                "Import / Export",
                "Schema Compare",
                "Data Compare",
                "Data Transfer",
                "Query Insights",
                "Disk Usage",
                "Index Analyzer",
                "Test Data Generator",
                "Relationship Graph",
                "Query Builder",
                "Connection Health",
                "Document Schema (Markdown)",
                "Tenant Scoping",
                "Query History",
                "Extended Events",
            ] {
                assert!(
                    !names.iter().any(|n| bare(n) == moved),
                    "{moved} belongs to the explorer Tools submenu now: {names:?}"
                );
            }
        }
    }

    #[test]
    fn connection_preview_stays_removed() {
        // The no-op Connection Preview item was removed in v0.42.0; make sure
        // it never quietly returns in either connection state.
        for connected in [false, true] {
            let names = action_names(&tools_menu(connected));
            assert!(
                !names.iter().any(|n| n.contains("Connection Preview")),
                "{names:?}"
            );
        }
    }

    #[test]
    fn help_menu_links_the_formatting_directives_reference() {
        for style in [HelpMenuStyle::MacOs, HelpMenuStyle::Windows] {
            let names = action_names(&help_menu(style));
            assert!(
                names.iter().any(|n| bare(n) == "Formatting Directives"),
                "{names:?}"
            );
        }
    }
}
