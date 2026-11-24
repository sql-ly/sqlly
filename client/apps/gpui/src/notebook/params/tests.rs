//! Windowed tests for notebook parameters: every widget type, validation,
//! presets on a fixed clock, personal versus shared storage, Apply gating,
//! linked and query-filled options, and sensitive values.

use std::cell::RefCell;
use std::rc::Rc;

use chrono::NaiveDate;
use gpui::{
    AppContext as _, Entity, ParentElement as _, Styled as _, TestAppContext, VisualTestContext,
    px, size,
};
use serde_json::{Value, json};
use sqlly_services::notebook::cells::parameter::{
    self as spec, DEFAULT_OPTIONS_LIMIT, MAX_OPTIONS_LIMIT, OptionItem, OptionsPage, OptionsQuery,
    ParameterCellSpec,
};
use sqlly_services::notebook::params::{
    AllowedValues, DatePreset, FixedClock, NamedPreset, ParamError, ParamScope, ParamType,
    ParamValue, ParameterDef, Preset, Reactivity,
};
use sqlly_services::notebook::{Cell, CellEvent, CellKind, NotebookDoc, RunMode};

use super::editor::ParamEditorEvent;
use super::store::SecretCatalog;
use super::widget::{Edit, ParamWidgetEvent, PresetPick, Surface};
use super::{PARAM_DEBOUNCE, ParamOptionsRequest};
use crate::notebook::view::{NotebookEvent, NotebookOpening, NotebookView};

struct DataRoot(tempfile::TempDir);

impl DataRoot {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        crate::notebook::autosave::set_test_root(Some(dir.path().to_path_buf()));
        Self(dir)
    }

    fn personal_file(&self, id: &str) -> Option<String> {
        std::fs::read_to_string(
            self.0
                .path()
                .join("notebooks")
                .join("personal")
                .join(format!("{id}.json")),
        )
        .ok()
    }
}

impl Drop for DataRoot {
    fn drop(&mut self) {
        crate::notebook::autosave::set_test_root(None);
    }
}

#[derive(Default)]
struct FakeSecrets(RefCell<Vec<(String, String)>>);

impl SecretCatalog for FakeSecrets {
    fn names(&self) -> Vec<String> {
        self.0.borrow().iter().map(|(n, _)| n.clone()).collect()
    }

    fn add(&self, name: &str, value: &str) -> Result<(), String> {
        self.0.borrow_mut().push((name.into(), value.into()));
        Ok(())
    }
}

fn clock(y: i32, m: u32, d: u32) -> Rc<FixedClock> {
    Rc::new(FixedClock(
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(10, 0, 0)
            .unwrap(),
    ))
}

fn day(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn init(cx: &mut TestAppContext) {
    cx.update(|cx| {
        if cx.try_global::<gpui_component::Theme>().is_none() {
            gpui_component::init(cx);
        }
        crate::editor::register_keybindings(cx);
    });
}

fn notebook(params: Vec<ParameterDef>, cells: &[(&str, CellKind, &str)]) -> NotebookDoc {
    let mut nb = NotebookDoc::empty("nb_params", "Parameters");
    for (id, kind, source) in cells {
        let mut cell = Cell::new(*id, kind.clone(), Default::default());
        cell.source = source.to_string();
        nb.cells.push(cell);
    }
    nb.parameters = params;
    nb
}

fn param_cell(id: &str, name: &str) -> Cell {
    let mut cell = Cell::new(id, CellKind::Parameter, Default::default());
    cell.spec = ParameterCellSpec::for_parameter(name).to_value();
    cell
}

/// Open `model` in a window, recording every event the view emits from
/// its first render on.
fn open(
    cx: &mut TestAppContext,
    model: NotebookDoc,
    opening: NotebookOpening,
) -> (
    Entity<NotebookView>,
    Rc<RefCell<Vec<NotebookEvent>>>,
    &mut VisualTestContext,
) {
    init(cx);
    let events = Rc::new(RefCell::new(Vec::new()));
    let seen = events.clone();
    let (root, vcx) = cx.add_window_view(move |window, cx| {
        let nb = cx.new(|cx| NotebookView::with_choices(model, opening, Rc::new(Vec::new), cx));
        cx.subscribe(&nb, move |_, _, event: &NotebookEvent, _| {
            seen.borrow_mut().push(event.clone());
        })
        .detach();
        gpui_component::Root::new(nb, window, cx)
    });
    let view = vcx.update(|_, cx| {
        root.read(cx)
            .view()
            .clone()
            .downcast::<NotebookView>()
            .expect("root hosts the notebook")
    });
    (view, events, vcx)
}

fn draw(view: &Entity<NotebookView>, cx: &mut VisualTestContext) {
    let view = view.clone();
    cx.draw(
        gpui::Point::default(),
        size(px(1400.0), px(900.0)),
        move |_, _| gpui::div().size_full().child(view.clone()),
    );
}

fn run_cells(events: &Rc<RefCell<Vec<NotebookEvent>>>) -> Vec<String> {
    events
        .borrow()
        .iter()
        .filter_map(|e| match e {
            NotebookEvent::RunCell { cell_id } => Some(cell_id.clone()),
            _ => None,
        })
        .collect()
}

fn option_requests(events: &Rc<RefCell<Vec<NotebookEvent>>>) -> Vec<ParamOptionsRequest> {
    events
        .borrow()
        .iter()
        .filter_map(|e| match e {
            NotebookEvent::ParamOptionsRequested(r) => Some((**r).clone()),
            _ => None,
        })
        .collect()
}

fn value(
    view: &Entity<NotebookView>,
    cx: &mut VisualTestContext,
    name: &str,
) -> Option<ParamValue> {
    view.update(cx, |v, _| v.doc().resolved_params().get(name).cloned())
}

fn edit(view: &Entity<NotebookView>, cx: &mut VisualTestContext, name: &str, edit: Edit) {
    view.update(cx, |v, cx| v.edit_param(name, edit, None, cx));
}

fn widget_value(
    view: &Entity<NotebookView>,
    cx: &mut VisualTestContext,
    surface: Surface,
    name: &str,
) -> (Option<ParamValue>, Option<String>) {
    view.update(cx, |v, cx| {
        let widget = v.param_ui.widget(&surface, name).expect("widget");
        let status = widget.read(cx).status();
        (status.value.clone(), status.error.clone())
    })
}

fn saved_json(view: &Entity<NotebookView>, cx: &mut VisualTestContext) -> String {
    view.update(cx, |v, _| v.model_for_save().to_canonical_json())
}

fn listed(def: ParameterDef, values: &[&str]) -> ParameterDef {
    let mut def = def;
    def.allowed_values = Some(AllowedValues::Static(
        values.iter().map(|v| Value::from(*v)).collect(),
    ));
    def
}

fn with_query(mut def: ParameterDef, sql: &str) -> ParameterDef {
    OptionsQuery::store(&mut def, Some(OptionsQuery::new(sql)));
    def
}

fn every_type() -> Vec<ParameterDef> {
    vec![
        ParameterDef::new("s", ParamType::String),
        ParameterDef::new("i", ParamType::Integer),
        ParameterDef::new("d", ParamType::Decimal),
        ParameterDef::new("b", ParamType::Boolean),
        ParameterDef::new("dt", ParamType::Date),
        ParameterDef::new(
            "ts",
            ParamType::Datetime {
                tz: Some("UTC".into()),
            },
        ),
        ParameterDef::new("dr", ParamType::DateRange),
        listed(ParameterDef::new("e", ParamType::Enum), &["EU", "US"]),
        listed(
            ParameterDef::new("m", ParamType::MultiSelect),
            &["EU", "US", "APAC"],
        ),
        listed(
            ParameterDef::new("q", ParamType::Search),
            &["Alpha", "Beta"],
        ),
        ParameterDef::new("lk", ParamType::EntityLookup),
        ParameterDef::new(
            "sl",
            ParamType::Slider {
                min: 0.0,
                max: 100.0,
                step: 5.0,
            },
        ),
        ParameterDef::new("sec", ParamType::SecretRef),
        ParameterDef::new(
            "id",
            ParamType::Identifier {
                allowlist: vec!["dbo.Orders".into(), "dbo.Customers".into()],
            },
        ),
    ]
}

#[gpui::test]
fn every_type_gets_a_widget_in_its_cell_and_the_rail_and_values_round_trip(
    cx: &mut TestAppContext,
) {
    let root = DataRoot::new();
    let defs = every_type();
    let mut nb = notebook(defs.clone(), &[]);
    for d in &defs {
        nb.cells
            .push(param_cell(&format!("pc_{}", d.name), &d.name));
    }
    let (view, _events, cx) = open(cx, nb, NotebookOpening::New);
    let secrets = Rc::new(FakeSecrets::default());
    secrets.add("prod_key", "s3cr3t").unwrap();
    view.update(cx, |v, _| v.param_ui.secrets = secrets.clone());
    draw(&view, cx);
    view.update(cx, |v, _| {
        for d in &defs {
            assert!(
                v.param_ui.widget(&Surface::Rail, &d.name).is_some(),
                "{}",
                d.name
            );
            assert!(
                v.param_ui
                    .widget(&Surface::Cell(format!("pc_{}", d.name)), &d.name)
                    .is_some(),
                "{}",
                d.name
            );
        }
    });
    let at = NaiveDate::from_ymd_opt(2026, 11, 3)
        .unwrap()
        .and_hms_opt(14, 30, 0)
        .unwrap();
    let cases: Vec<(&str, Edit, ParamValue)> = vec![
        (
            "s",
            Edit::Text("hello".into()),
            ParamValue::Text("hello".into()),
        ),
        ("i", Edit::Text("42".into()), ParamValue::Integer(42)),
        (
            "d",
            Edit::Text("12.50".into()),
            ParamValue::Decimal("12.50".into()),
        ),
        (
            "b",
            Edit::Value(ParamValue::Boolean(true)),
            ParamValue::Boolean(true),
        ),
        (
            "dt",
            Edit::Value(ParamValue::Date(day(2026, 11, 3))),
            ParamValue::Date(day(2026, 11, 3)),
        ),
        (
            "ts",
            Edit::Value(ParamValue::DateTime(at)),
            ParamValue::DateTime(at),
        ),
        (
            "dr",
            Edit::Value(ParamValue::DateRange {
                start: day(2026, 10, 1),
                end: day(2026, 10, 31),
            }),
            ParamValue::DateRange {
                start: day(2026, 10, 1),
                end: day(2026, 10, 31),
            },
        ),
        (
            "e",
            Edit::Choice(vec![json!("US")]),
            ParamValue::Text("US".into()),
        ),
        (
            "m",
            Edit::Choice(vec![json!("EU"), json!("APAC")]),
            ParamValue::List(vec![
                ParamValue::Text("EU".into()),
                ParamValue::Text("APAC".into()),
            ]),
        ),
        (
            "q",
            Edit::Text("Alp".into()),
            ParamValue::Text("Alp".into()),
        ),
        ("lk", Edit::Choice(vec![json!(7)]), ParamValue::Integer(7)),
        ("sl", Edit::Text("15".into()), ParamValue::Integer(15)),
        (
            "sec",
            Edit::Choice(vec![json!("prod_key")]),
            ParamValue::SecretRef("prod_key".into()),
        ),
        (
            "id",
            Edit::Choice(vec![json!("dbo.Orders")]),
            ParamValue::Identifier("dbo.Orders".into()),
        ),
    ];
    for (name, change, expected) in cases.clone() {
        edit(&view, cx, name, change);
        assert_eq!(value(&view, cx, name), Some(expected.clone()), "{name}");
        for surface in [Surface::Rail, Surface::Cell(format!("pc_{name}"))] {
            assert_eq!(
                widget_value(&view, cx, surface, name),
                (Some(expected.clone()), None),
                "{name}"
            );
        }
    }
    // Shared values live in the file and come back on reopen; the secret
    // pick lives in the personal side file.
    let text = saved_json(&view, cx);
    assert!(!text.contains("prod_key"));
    assert!(
        root.personal_file("nb_params")
            .unwrap()
            .contains("prod_key")
    );
    let loaded =
        sqlly_services::notebook::load(None, &text, &mut sqlly_services::notebook::RandomIds, None)
            .unwrap()
            .doc;
    let resolved = sqlly_services::notebook::params::resolve_all(
        &loaded.parameters,
        &Default::default(),
        &FixedClock(at),
    );
    for (name, _, expected) in &cases {
        if *name != "sec" {
            assert_eq!(resolved.get(name), Some(expected), "{name}");
        }
    }
}

#[gpui::test]
fn controls_report_edits_through_the_widget(cx: &mut TestAppContext) {
    let _root = DataRoot::new();
    let defs = vec![
        ParameterDef::new("i", ParamType::Integer),
        ParameterDef::new("b", ParamType::Boolean),
        listed(ParameterDef::new("e", ParamType::Enum), &["EU", "US"]),
        listed(
            ParameterDef::new("m", ParamType::MultiSelect),
            &["EU", "US", "APAC"],
        ),
    ];
    let (view, _events, cx) = open(cx, notebook(defs, &[]), NotebookOpening::New);
    draw(&view, cx);
    let widget = |cx: &mut VisualTestContext, name: &str| {
        view.update(cx, |v, _| {
            v.param_ui.widget(&Surface::Rail, name).unwrap().clone()
        })
    };
    // Typing is debounced: nothing is taken until the pause.
    let input = widget(cx, "i").read_with(cx, |w, _| w.input().unwrap());
    cx.update(|window, cx| input.update(cx, |s, cx| s.replace_all("4", window, cx)));
    cx.run_until_parked();
    cx.update(|window, cx| input.update(cx, |s, cx| s.replace_all("42", window, cx)));
    cx.run_until_parked();
    assert_eq!(value(&view, cx, "i"), None);
    cx.executor().advance_clock(PARAM_DEBOUNCE * 2);
    cx.run_until_parked();
    assert_eq!(value(&view, cx, "i"), Some(ParamValue::Integer(42)));
    let toggle = widget(cx, "b");
    toggle.update(cx, |w, cx| w.toggle(true, cx));
    cx.run_until_parked();
    assert_eq!(value(&view, cx, "b"), Some(ParamValue::Boolean(true)));
    let select = widget(cx, "e");
    select.update(cx, |w, cx| w.pick(1, cx));
    cx.run_until_parked();
    assert_eq!(value(&view, cx, "e"), Some(ParamValue::Text("US".into())));
    let multi = widget(cx, "m");
    multi.update(cx, |w, cx| w.pick(2, cx));
    cx.run_until_parked();
    multi.update(cx, |w, cx| w.pick(0, cx));
    cx.run_until_parked();
    assert_eq!(
        value(&view, cx, "m"),
        Some(ParamValue::List(vec![
            ParamValue::Text("EU".into()),
            ParamValue::Text("APAC".into())
        ]))
    );
    multi.update(cx, |w, cx| w.pick(0, cx));
    cx.run_until_parked();
    assert_eq!(
        value(&view, cx, "m"),
        Some(ParamValue::List(vec![ParamValue::Text("APAC".into())]))
    );
}

#[gpui::test]
fn invalid_values_show_inline_and_keep_the_last_good_value(cx: &mut TestAppContext) {
    let _root = DataRoot::new();
    let mut n = ParameterDef::new("n", ParamType::Integer);
    n.validation.min = Some(json!(1));
    n.validation.max = Some(json!(10));
    n.default = Some(json!(5));
    let mut code = ParameterDef::new("code", ParamType::String);
    code.validation.required = true;
    code.validation.max_length = Some(3);
    code.default = Some(json!("abc"));
    let e = listed(ParameterDef::new("e", ParamType::Enum), &["EU"]);
    let (view, _events, cx) = open(cx, notebook(vec![n, code, e], &[]), NotebookOpening::New);
    draw(&view, cx);
    let message = |e: ParamError| crate::localization::message(e.message());
    edit(&view, cx, "n", Edit::Text("500".into()));
    assert_eq!(value(&view, cx, "n"), Some(ParamValue::Integer(5)));
    assert_eq!(
        widget_value(&view, cx, Surface::Rail, "n").1,
        Some(message(ParamError::OutOfRange))
    );
    edit(&view, cx, "n", Edit::Text("five".into()));
    assert_eq!(
        widget_value(&view, cx, Surface::Rail, "n").1,
        Some(message(ParamError::InvalidValue {
            expected: "integer".into()
        }))
    );
    edit(&view, cx, "n", Edit::Text("7".into()));
    assert_eq!(
        widget_value(&view, cx, Surface::Rail, "n"),
        (Some(ParamValue::Integer(7)), None)
    );
    edit(&view, cx, "code", Edit::Clear);
    assert_eq!(
        widget_value(&view, cx, Surface::Rail, "code").1,
        Some(message(ParamError::Required))
    );
    edit(&view, cx, "code", Edit::Text("abcd".into()));
    assert_eq!(
        widget_value(&view, cx, Surface::Rail, "code").1,
        Some(message(ParamError::TooLong))
    );
    edit(&view, cx, "e", Edit::Choice(vec![json!("XX")]));
    assert_eq!(
        widget_value(&view, cx, Surface::Rail, "e").1,
        Some(message(ParamError::NotAllowed))
    );
}

#[gpui::test]
fn date_presets_follow_the_injected_clock_and_stay_relative(cx: &mut TestAppContext) {
    let _root = DataRoot::new();
    let mut period = ParameterDef::new("period", ParamType::DateRange);
    period.presets = vec![
        Preset::Date(DatePreset::Last7Days),
        Preset::Date(DatePreset::PreviousMonth),
        Preset::Date(DatePreset::CurrentQuarter),
    ];
    let mut region = ParameterDef::new("region", ParamType::String);
    region.presets = vec![Preset::Named(NamedPreset {
        id: "eu".into(),
        label: "Europe".into(),
        value: json!("EU"),
    })];
    let (view, _events, cx) = open(
        cx,
        notebook(vec![period, region], &[]),
        NotebookOpening::New,
    );
    view.update(cx, |v, _| v.set_param_clock(clock(2026, 11, 18)));
    draw(&view, cx);
    let menu: Vec<PresetPick> = view.update(cx, |v, cx| {
        let w = v.param_ui.widget(&Surface::Rail, "period").unwrap();
        w.read(cx)
            .status()
            .presets
            .iter()
            .map(|p| p.pick.clone())
            .collect()
    });
    assert_eq!(
        menu,
        [
            PresetPick::Date(DatePreset::Last7Days),
            PresetPick::Date(DatePreset::PreviousMonth),
            PresetPick::Date(DatePreset::CurrentQuarter),
        ]
    );
    let pick = |cx: &mut VisualTestContext, name: &str, pick: PresetPick| {
        let w = view.update(cx, |v, _| {
            v.param_ui.widget(&Surface::Rail, name).unwrap().clone()
        });
        w.update(cx, |_, cx| cx.emit(ParamWidgetEvent::Preset(pick)));
        cx.run_until_parked();
    };
    pick(cx, "period", PresetPick::Date(DatePreset::Last7Days));
    assert_eq!(
        value(&view, cx, "period"),
        Some(ParamValue::DateRange {
            start: day(2026, 11, 12),
            end: day(2026, 11, 18)
        })
    );
    assert!(saved_json(&view, cx).contains("\"preset\": \"last_7_days\""));
    view.update(cx, |v, _| v.set_param_clock(clock(2026, 12, 3)));
    assert_eq!(
        value(&view, cx, "period"),
        Some(ParamValue::DateRange {
            start: day(2026, 11, 27),
            end: day(2026, 12, 3)
        })
    );
    pick(cx, "period", PresetPick::Date(DatePreset::PreviousMonth));
    assert_eq!(
        value(&view, cx, "period"),
        Some(ParamValue::DateRange {
            start: day(2026, 11, 1),
            end: day(2026, 11, 30)
        })
    );
    pick(cx, "period", PresetPick::Date(DatePreset::CurrentQuarter));
    assert_eq!(
        value(&view, cx, "period"),
        Some(ParamValue::DateRange {
            start: day(2026, 10, 1),
            end: day(2026, 12, 31)
        })
    );
    pick(cx, "region", PresetPick::Named("eu".into()));
    assert_eq!(
        value(&view, cx, "region"),
        Some(ParamValue::Text("EU".into()))
    );
}

#[gpui::test]
fn personal_values_and_sets_never_reach_the_saved_notebook(cx: &mut TestAppContext) {
    let root = DataRoot::new();
    let mut owner = ParameterDef::new("owner", ParamType::String);
    owner.scope = ParamScope::Personal;
    owner.default = Some(json!("nobody"));
    let region = ParameterDef::new("region", ParamType::String);
    let nb = notebook(vec![owner, region], &[]);
    let (view, _events, cx) = open(cx, nb, NotebookOpening::New);
    draw(&view, cx);
    edit(&view, cx, "owner", Edit::Text("matt".into()));
    edit(&view, cx, "region", Edit::Text("EMEA".into()));
    view.update(cx, |v, cx| {
        v.save_param_set("Mine", true, cx);
        v.save_param_set("Team", false, cx);
    });
    let text = saved_json(&view, cx);
    assert!(text.contains("EMEA"));
    assert!(text.contains("\"Team\""));
    assert!(!text.contains("matt"));
    assert!(!text.contains("\"Mine\""));
    let personal = root.personal_file("nb_params").unwrap();
    assert!(personal.contains("matt") && personal.contains("\"Mine\""));
    assert!(!personal.contains("\"Team\""));
    let saved = view.update(cx, |v, _| v.model_for_save());
    let mut app = cx.cx.clone();
    let (again, _again_events, cx2) = open(&mut app, saved, NotebookOpening::New);
    draw(&again, cx2);
    assert_eq!(
        value(&again, cx2, "owner"),
        Some(ParamValue::Text("matt".into()))
    );
    assert_eq!(
        value(&again, cx2, "region"),
        Some(ParamValue::Text("EMEA".into()))
    );
    let sets: Vec<(String, bool)> = again.update(cx2, |v, _| {
        v.param_sets()
            .into_iter()
            .map(|(s, p)| (s.name, p))
            .collect()
    });
    assert_eq!(
        sets,
        [("Team".to_string(), false), ("Mine".to_string(), true)]
    );
}

#[gpui::test]
fn sets_apply_several_values_at_once(cx: &mut TestAppContext) {
    let _root = DataRoot::new();
    let defs = vec![
        ParameterDef::new("region", ParamType::String),
        ParameterDef::new("tier", ParamType::Integer),
    ];
    let (view, _events, cx) = open(cx, notebook(defs, &[]), NotebookOpening::New);
    draw(&view, cx);
    edit(&view, cx, "region", Edit::Text("EU".into()));
    edit(&view, cx, "tier", Edit::Text("2".into()));
    view.update(cx, |v, cx| v.save_param_set("EU gold", false, cx));
    edit(&view, cx, "region", Edit::Text("US".into()));
    edit(&view, cx, "tier", Edit::Text("9".into()));
    let id = view.update(cx, |v, _| v.param_sets()[0].0.id.clone());
    view.update(cx, |v, cx| v.apply_param_set(&id, false, cx));
    assert_eq!(
        value(&view, cx, "region"),
        Some(ParamValue::Text("EU".into()))
    );
    assert_eq!(value(&view, cx, "tier"), Some(ParamValue::Integer(2)));
    view.update(cx, |v, cx| v.delete_param_set(&id, false, cx));
    assert!(view.update(cx, |v, _| v.param_sets().is_empty()));
}

#[gpui::test]
fn remote_cells_wait_for_apply_in_reactive_mode(cx: &mut TestAppContext) {
    let _root = DataRoot::new();
    let mut region = ParameterDef::new("region", ParamType::String);
    region.default = Some(json!("EU"));
    let mut tier = ParameterDef::new("tier", ParamType::String);
    tier.default = Some(json!("gold"));
    tier.reactivity = Reactivity::Apply;
    let mut nb = notebook(
        vec![region, tier],
        &[
            (
                "cell_1",
                CellKind::Sql,
                "SELECT * FROM sales WHERE region = :region",
            ),
            (
                "cell_2",
                CellKind::Sql,
                "SELECT * FROM sales WHERE tier = :tier",
            ),
            ("cell_3", CellKind::Markdown, "Region {{param.region}}"),
        ],
    );
    nb.settings.run_mode = RunMode::Reactive;
    let (view, events, cx) = open(cx, nb, NotebookOpening::New);
    draw(&view, cx);
    edit(&view, cx, "region", Edit::Text("US".into()));
    cx.run_until_parked();
    assert!(run_cells(&events).is_empty(), "no remote run before Apply");
    view.update(cx, |v, _| {
        assert!(v.param_ui.has_pending());
        assert_eq!(v.param_apply_preview(), ["cell_1"]);
        assert_eq!(
            v.doc().interpolate_markdown("Region {{param.region}}"),
            "Region US"
        );
    });
    edit(&view, cx, "tier", Edit::Text("silver".into()));
    view.update(cx, |v, _| {
        assert_eq!(v.param_ui.pending_names(), ["tier"]);
        assert_eq!(v.param_apply_preview(), ["cell_1", "cell_2"]);
        assert!(v.doc().sql_for_run("cell_2").unwrap().contains("N'gold'"));
    });
    assert_eq!(
        widget_value(&view, cx, Surface::Rail, "tier").0,
        Some(ParamValue::Text("silver".into()))
    );
    assert!(run_cells(&events).is_empty());
    view.update(cx, |v, cx| v.apply_params(cx));
    cx.run_until_parked();
    assert_eq!(run_cells(&events), ["cell_1", "cell_2"]);
    view.update(cx, |v, _| {
        assert!(!v.param_ui.has_pending());
        assert!(v.doc().sql_for_run("cell_2").unwrap().contains("N'silver'"));
    });
}

#[gpui::test]
fn manual_mode_changes_mark_cells_stale_without_running(cx: &mut TestAppContext) {
    let _root = DataRoot::new();
    let mut region = ParameterDef::new("region", ParamType::String);
    region.default = Some(json!("EU"));
    let nb = notebook(
        vec![region],
        &[(
            "cell_1",
            CellKind::Sql,
            "SELECT * FROM sales WHERE region = :region",
        )],
    );
    let (view, events, cx) = open(cx, nb, NotebookOpening::New);
    draw(&view, cx);
    view.update(cx, |v, _| {
        let doc = v.doc_mut_for_test();
        let fp = doc.cell_fingerprint("cell_1").unwrap();
        doc.apply("cell_1", CellEvent::Queued);
        doc.apply(
            "cell_1",
            CellEvent::Started {
                run_id: "r1".into(),
            },
        );
        doc.complete("cell_1", "r1", fp, 3);
    });
    assert!(!view.update(cx, |v, _| v.is_stale("cell_1")));
    edit(&view, cx, "region", Edit::Text("US".into()));
    cx.run_until_parked();
    assert!(run_cells(&events).is_empty());
    view.update(cx, |v, _| {
        assert!(v.is_stale("cell_1"));
        assert!(!v.param_ui.has_pending());
    });
}

#[gpui::test]
fn linked_parameters_reload_their_options_and_drop_stale_picks(cx: &mut TestAppContext) {
    let _root = DataRoot::new();
    let mut region = listed(ParameterDef::new("region", ParamType::Enum), &["EU", "US"]);
    region.default = Some(json!("EU"));
    let store = with_query(
        ParameterDef::new("store", ParamType::Enum),
        "SELECT id, name FROM stores WHERE region = :region ORDER BY name",
    );
    let nb = notebook(
        vec![region, store],
        &[(
            "cell_1",
            CellKind::Sql,
            "SELECT * FROM sales WHERE store = :store",
        )],
    );
    let (view, events, cx) = open(cx, nb, NotebookOpening::New);
    draw(&view, cx);
    let first = option_requests(&events);
    assert_eq!(first.len(), 1);
    assert_eq!(
        first[0].bound.sql,
        "SELECT id, name FROM stores WHERE region = N'EU' ORDER BY name"
    );
    assert_eq!(first[0].bound.linked, ["region"]);
    let page = |items: &[(i64, &str)]| OptionsPage {
        items: items
            .iter()
            .map(|(id, name)| OptionItem {
                value: json!(id),
                label: name.to_string(),
            })
            .collect(),
        truncated: false,
    };
    view.update(cx, |v, cx| {
        v.param_options_loaded(
            "store",
            &first[0].fingerprint,
            Ok(page(&[(1, "Lyon"), (2, "Paris")])),
            cx,
        )
    });
    edit(&view, cx, "store", Edit::Choice(vec![json!(2)]));
    assert_eq!(
        value(&view, cx, "store"),
        Some(ParamValue::Text("2".into()))
    );
    view.update(cx, |v, _| {
        let doc = v.doc_mut_for_test();
        let fp = doc.cell_fingerprint("cell_1").unwrap();
        doc.apply("cell_1", CellEvent::Queued);
        doc.apply(
            "cell_1",
            CellEvent::Started {
                run_id: "r1".into(),
            },
        );
        doc.complete("cell_1", "r1", fp, 1);
    });
    edit(&view, cx, "region", Edit::Choice(vec![json!("US")]));
    let second = option_requests(&events);
    assert_eq!(second.len(), 2);
    assert!(second[1].bound.sql.contains("N'US'"));
    assert_ne!(second[1].fingerprint, first[0].fingerprint);
    // A late answer to the old request changes nothing.
    view.update(cx, |v, cx| {
        v.param_options_loaded("store", &first[0].fingerprint, Ok(page(&[(9, "Old")])), cx)
    });
    assert_eq!(
        value(&view, cx, "store"),
        Some(ParamValue::Text("2".into()))
    );
    view.update(cx, |v, cx| {
        v.param_options_loaded(
            "store",
            &second[1].fingerprint,
            Ok(page(&[(7, "Boston")])),
            cx,
        )
    });
    assert_eq!(value(&view, cx, "store"), None);
    assert!(view.update(cx, |v, _| v.is_stale("cell_1")));
    let options: Vec<String> = view.update(cx, |v, cx| {
        let w = v.param_ui.widget(&Surface::Rail, "store").unwrap();
        w.read(cx)
            .status()
            .options
            .iter()
            .map(|o| o.label.clone())
            .collect()
    });
    assert_eq!(options, ["Boston"]);
    // Going back to EU is served from the cache.
    edit(&view, cx, "region", Edit::Choice(vec![json!("EU")]));
    assert_eq!(option_requests(&events).len(), 2);
}

#[gpui::test]
fn options_queries_run_read_only_limited_and_only_once_trusted(cx: &mut TestAppContext) {
    let _root = DataRoot::new();
    let mut wide = with_query(
        ParameterDef::new("wide", ParamType::MultiSelect),
        "SELECT code FROM regions",
    );
    let mut q = OptionsQuery::of(&wide).unwrap();
    q.limit = Some(50_000);
    OptionsQuery::store(&mut wide, Some(q));
    let writes = with_query(
        ParameterDef::new("writes", ParamType::Enum),
        "DELETE FROM regions",
    );
    let plain = with_query(
        ParameterDef::new("plain", ParamType::Enum),
        "SELECT code FROM regions",
    );
    let nb = notebook(vec![wide, writes, plain], &[]);
    let (view, events, cx) = open(cx, nb, NotebookOpening::File);
    draw(&view, cx);
    assert!(
        option_requests(&events).is_empty(),
        "untrusted notebooks run nothing"
    );
    let untrusted = crate::localization::text("notebook.params.options.untrusted");
    let state = |cx: &mut VisualTestContext, name: &str| {
        view.update(cx, |v, cx| {
            let w = v.param_ui.widget(&Surface::Rail, name).unwrap();
            w.read(cx).status().options_state.clone()
        })
    };
    assert_eq!(
        state(cx, "plain"),
        super::widget::OptionsState::Failed(untrusted)
    );
    view.update(cx, |v, cx| {
        v.trust_notebook(cx);
        v.confirm_grant(cx);
    });
    draw(&view, cx);
    let requests = option_requests(&events);
    let names: Vec<&str> = requests.iter().map(|r| r.param.as_str()).collect();
    assert_eq!(names, ["wide", "plain"]);
    for r in &requests {
        assert!(r.target.read_only);
        assert_eq!(
            r.target.timeout_seconds,
            Some(super::OPTIONS_TIMEOUT_SECONDS)
        );
        let mut tab = sqlly_core::ConnectionProfile::sql_login(
            "tab",
            "localhost",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "k".into(),
            },
        );
        tab.block_all_but_select = false;
        tab.enforce_read_only = false;
        let profile =
            crate::notebook::connection::profile_for_target(&r.target, |_| None, Some(&tab))
                .unwrap();
        assert!(profile.block_all_but_select && profile.enforce_read_only);
    }
    assert_eq!(requests[0].bound.limit, MAX_OPTIONS_LIMIT);
    assert_eq!(requests[1].bound.limit, DEFAULT_OPTIONS_LIMIT);
    assert_eq!(
        state(cx, "writes"),
        super::widget::OptionsState::Failed(crate::localization::message(
            spec::OptionsError::NotReadOnly.message()
        ))
    );
    view.update(cx, |v, cx| {
        v.param_options_loaded(
            "plain",
            &requests[1].fingerprint,
            Ok(OptionsPage {
                items: vec![OptionItem {
                    value: json!("EU"),
                    label: "Europe".into(),
                }],
                truncated: true,
            }),
            cx,
        )
    });
    assert_eq!(
        state(cx, "plain"),
        super::widget::OptionsState::Loaded { truncated: true }
    );
    view.update(cx, |v, cx| v.request_options("plain", false, false, cx));
    assert_eq!(option_requests(&events).len(), 2, "cached");
    let w = view.update(cx, |v, _| {
        v.param_ui.widget(&Surface::Rail, "plain").unwrap().clone()
    });
    w.update(cx, |_, cx| cx.emit(ParamWidgetEvent::RefreshOptions));
    cx.run_until_parked();
    assert_eq!(option_requests(&events).len(), 3, "refresh runs it again");
}

#[gpui::test]
fn sensitive_values_are_masked_and_kept_out_of_files_history_and_links(cx: &mut TestAppContext) {
    let root = DataRoot::new();
    let mut pin = ParameterDef::new("pin", ParamType::String);
    pin.sensitive = true;
    pin.scope = ParamScope::Personal;
    let mut tier = listed(
        ParameterDef::new("tier", ParamType::Enum),
        &["gold", "silver"],
    );
    tier.sensitive = true;
    let region = ParameterDef::new("region", ParamType::String);
    let nb = notebook(
        vec![pin, tier, region],
        &[
            (
                "cell_1",
                CellKind::Sql,
                "SELECT * FROM accounts WHERE pin = :pin AND region = :region",
            ),
            (
                "cell_2",
                CellKind::Markdown,
                "PIN {{param.pin}}, tier {{param.tier}}, region {{param.region}}",
            ),
            ("cell_3", CellKind::Sql, "SELECT * FROM {{cell_1}}"),
        ],
    );
    let (view, _events, cx) = open(cx, nb, NotebookOpening::New);
    draw(&view, cx);
    edit(&view, cx, "pin", Edit::Text("8675309".into()));
    edit(&view, cx, "tier", Edit::Choice(vec![json!("gold")]));
    edit(&view, cx, "region", Edit::Text("EU".into()));
    assert_eq!(
        value(&view, cx, "pin"),
        Some(ParamValue::Text("8675309".into()))
    );
    let secret = "8675309";
    let text = saved_json(&view, cx);
    assert!(!text.contains(secret), "{text}");
    view.update(cx, |v, _| {
        let saved = v.model_for_save();
        assert!(
            saved
                .parameters
                .iter()
                .all(|p| !p.sensitive || p.default.is_none())
        );
    });
    let ipynb = view.update(cx, |v, _| {
        sqlly_services::notebook::to_ipynb(&v.model_for_save())
    });
    assert!(!ipynb.contains(secret));
    assert!(
        root.personal_file("nb_params")
            .is_none_or(|t| !t.contains(secret))
    );
    for cell in ["cell_1", "cell_3"] {
        let request = view.update(cx, |v, _| v.run_request(cell)).unwrap();
        assert!(
            request.sql.contains(secret),
            "{cell}: the database still gets the value"
        );
        let history = request.history_sql.expect("redacted history");
        assert!(!history.contains(secret), "{history}");
        assert!(
            history.contains(":pin") && history.contains("N'EU'"),
            "{history}"
        );
    }
    view.update(cx, |v, cx| {
        let shown = v.doc().interpolate_markdown(
            "PIN {{param.pin}}, tier {{param.tier}}, region {{param.region}}",
        );
        assert_eq!(shown, "PIN ••••••, tier ••••••, region EU");
        let resolved = v.doc().resolved_params();
        let shareable = spec::shareable_values(v.doc().param_defs(), &resolved);
        assert_eq!(shareable.keys().collect::<Vec<_>>(), ["region"]);
        let recorded = sqlly_services::notebook::run_record::redact_params(
            v.doc().param_defs(),
            &resolved,
            &["pin", "tier", "region"],
        );
        assert!(
            recorded
                .iter()
                .filter(|p| p.name != "region")
                .all(|p| p.redacted && p.value.is_none())
        );
        assert!(v.activity.iter().all(|e| !e.message.contains(secret)));
        let tier = v.param_ui.widget(&Surface::Rail, "tier").unwrap();
        assert!(tier.read(cx).is_masked());
        let set = spec::capture_set(
            "s",
            "s",
            v.doc().param_defs(),
            &resolved.values,
            spec::SetTarget::Personal,
        );
        assert_eq!(set.values.keys().collect::<Vec<_>>(), ["region"]);
    });
}

#[gpui::test]
fn secret_parameters_pick_by_name_and_refuse_to_be_written_into_sql(cx: &mut TestAppContext) {
    let root = DataRoot::new();
    let nb = notebook(
        vec![ParameterDef::new("api_key", ParamType::SecretRef)],
        &[(
            "cell_1",
            CellKind::Sql,
            "SELECT * FROM calls WHERE key = :api_key",
        )],
    );
    let (view, _events, cx) = open(cx, nb, NotebookOpening::New);
    let secrets = Rc::new(FakeSecrets::default());
    view.update(cx, |v, _| v.param_ui.secrets = secrets.clone());
    draw(&view, cx);
    let w = view.update(cx, |v, _| {
        v.param_ui
            .widget(&Surface::Rail, "api_key")
            .unwrap()
            .clone()
    });
    w.update(cx, |_, cx| {
        cx.emit(ParamWidgetEvent::AddSecret {
            name: "prod_key".into(),
            value: "tok-123".into(),
        })
    });
    cx.run_until_parked();
    assert_eq!(secrets.names(), ["prod_key"]);
    assert_eq!(
        value(&view, cx, "api_key"),
        Some(ParamValue::SecretRef("prod_key".into()))
    );
    let error = view.update(cx, |v, _| v.run_request("cell_1")).unwrap_err();
    assert_eq!(
        error,
        crate::localization::message(
            sqlly_services::notebook::params::BindError::SecretNeedsNativeBinding {
                name: "api_key".into()
            }
            .message()
        )
    );
    assert!(!error.contains("tok-123"));
    assert!(!saved_json(&view, cx).contains("prod_key"));
    let personal = root.personal_file("nb_params").unwrap();
    assert!(personal.contains("prod_key") && !personal.contains("tok-123"));
}

#[gpui::test]
fn the_editor_adds_renames_and_deletes_definitions(cx: &mut TestAppContext) {
    let _root = DataRoot::new();
    let mut nb = notebook(
        vec![ParameterDef::new("region", ParamType::String)],
        &[("cell_1", CellKind::Sql, "SELECT * FROM t WHERE r = :region")],
    );
    nb.cells
        .push(Cell::new("pc", CellKind::Parameter, Default::default()));
    let (view, _events, cx) = open(cx, nb, NotebookOpening::New);
    draw(&view, cx);
    cx.update(|window, cx| view.update(cx, |v, cx| v.new_param_for_cell("pc", window, cx)));
    let editor = view.update(cx, |v, _| v.param_ui.editor().unwrap().clone());
    let mut limit = ParameterDef::new("max_rows", ParamType::Integer);
    limit.default = Some(json!(10));
    editor.update(cx, |_, cx| {
        cx.emit(ParamEditorEvent::Save {
            original: None,
            def: Box::new(limit),
        })
    });
    cx.run_until_parked();
    view.update(cx, |v, _| {
        assert!(v.param_ui.editor().is_none());
        let cell = v.doc().model().cell("pc").unwrap();
        assert_eq!(ParameterCellSpec::of(cell).parameter, "max_rows");
    });
    draw(&view, cx);
    assert!(view.update(cx, |v, _| {
        v.param_ui
            .widget(&Surface::Cell("pc".into()), "max_rows")
            .is_some()
    }));
    // A clashing name keeps the editor open with the error.
    cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.open_param_editor(Some("max_rows".into()), window, cx)
        })
    });
    let editor = view.update(cx, |v, _| v.param_ui.editor().unwrap().clone());
    editor.update(cx, |_, cx| {
        cx.emit(ParamEditorEvent::Save {
            original: Some("max_rows".into()),
            def: Box::new(ParameterDef::new("region", ParamType::Integer)),
        })
    });
    cx.run_until_parked();
    assert!(view.update(cx, |v, _| v.param_ui.editor().is_some()));
    // Renaming rewrites the SQL that uses it and the cells that show it.
    let mut area = ParameterDef::new("area", ParamType::String);
    area.id = "region".into();
    cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.open_param_editor(Some("region".into()), window, cx)
        })
    });
    let editor = view.update(cx, |v, _| v.param_ui.editor().unwrap().clone());
    editor.update(cx, |_, cx| {
        cx.emit(ParamEditorEvent::Save {
            original: Some("region".into()),
            def: Box::new(area),
        })
    });
    cx.run_until_parked();
    view.update(cx, |v, _| {
        assert_eq!(
            v.doc().model().cell("cell_1").unwrap().source,
            "SELECT * FROM t WHERE r = :area"
        );
        assert!(v.doc().param_defs().iter().any(|d| d.name == "area"));
    });
    cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.open_param_editor(Some("area".into()), window, cx)
        })
    });
    let editor = view.update(cx, |v, _| v.param_ui.editor().unwrap().clone());
    editor.update(cx, |_, cx| cx.emit(ParamEditorEvent::Delete("area".into())));
    cx.run_until_parked();
    view.update(cx, |v, _| {
        assert!(v.doc().param_defs().iter().all(|d| d.name != "area"));
        assert!(v.param_ui.editor().is_none());
    });
    draw(&view, cx);
}

#[gpui::test]
fn the_param_fixture_notebooks_open_and_bind(cx: &mut TestAppContext) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../scripts/docs-shots/fixtures/notebooks");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if !name.starts_with("params-") {
            continue;
        }
        seen += 1;
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\r\n", "\n");
        let text = text.replace("{P_DEV}", "0c3d2b1a-5f4e-4d3c-8b2a-1f0e9d8c7b6a");
        let loaded = sqlly_services::notebook::load(
            Some(&path.to_string_lossy()),
            &text,
            &mut sqlly_services::notebook::RandomIds,
            None,
        )
        .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert!(loaded.migrated_from.is_none(), "{name}");
        for cell in &loaded.doc.cells {
            if let Some(p) = super::cell_parameter(cell) {
                assert!(loaded.doc.parameter(&p).is_some(), "{name}: {p}");
            }
        }
        let resolved = sqlly_services::notebook::params::resolve_all(
            &loaded.doc.parameters,
            &Default::default(),
            clock(1998, 2, 1).as_ref(),
        );
        for def in &loaded.doc.parameters {
            assert_eq!(
                resolved.error(&def.name).is_some(),
                def.default.is_none(),
                "{name}: {}",
                def.name
            );
        }
    }
    assert!(seen >= 4, "{seen}");
    let _ = cx;
}

#[gpui::test]
fn options_can_come_from_another_cells_output(cx: &mut TestAppContext) {
    let _root = DataRoot::new();
    let mut region = ParameterDef::new("region", ParamType::Enum);
    region.allowed_values = Some(AllowedValues::FromCell {
        from_cell: "regions".into(),
        value_column: "code".into(),
        label_column: Some("name".into()),
        limit: None,
    });
    let mut nb = notebook(
        vec![region],
        &[("cell_1", CellKind::Sql, "SELECT code, name FROM regions")],
    );
    nb.cells[0].name = Some("regions".into());
    let (view, _events, cx) = open(cx, nb, NotebookOpening::New);
    draw(&view, cx);
    let labels = |cx: &mut VisualTestContext| -> Vec<String> {
        view.update(cx, |v, cx| {
            let w = v.param_ui.widget(&Surface::Rail, "region").unwrap();
            w.read(cx)
                .status()
                .options
                .iter()
                .map(|o| o.label.clone())
                .collect()
        })
    };
    assert!(labels(cx).is_empty());
    view.update(cx, |v, _| {
        let column = |name: &str| sqlly_core::ResultColumn {
            name: name.into(),
            data_type: "text".into(),
            nullable: false,
        };
        let text = |s: &str| sqlly_core::CellValue::String(s.into());
        v.outputs.publish(
            "cell_1",
            sqlly_services::notebook::CellOutput {
                columns: vec![column("code"), column("name")],
                rows: std::sync::Arc::new(vec![
                    vec![text("EU"), text("Europe")],
                    vec![text("US"), text("United States")],
                ]),
                completeness: sqlly_services::notebook::Completeness::Complete,
                row_count: 2,
                elapsed_ms: 1,
                run_id: "r1".into(),
                fingerprint: sqlly_services::notebook::Fingerprint(String::new()),
                provenance: Default::default(),
                freshness: Default::default(),
            },
        );
    });
    draw(&view, cx);
    assert_eq!(labels(cx), ["Europe", "United States"]);
    edit(&view, cx, "region", Edit::Choice(vec![json!("US")]));
    assert_eq!(
        value(&view, cx, "region"),
        Some(ParamValue::Text("US".into()))
    );
}

#[gpui::test]
fn renaming_a_parameter_updates_open_editors_so_typing_keeps_it(cx: &mut TestAppContext) {
    let _root = DataRoot::new();
    let nb = notebook(
        vec![ParameterDef::new("region", ParamType::String)],
        &[("cell_1", CellKind::Sql, "SELECT * FROM t WHERE r = :region")],
    );
    let (view, _events, cx) = open(cx, nb, NotebookOpening::New);
    let target = view.downgrade();
    cx.update(|_, cx| {
        cx.subscribe(&view, move |_, event: &NotebookEvent, cx| {
            if let NotebookEvent::NeedCellEntities { cell_ids } = event {
                let ids = cell_ids.clone();
                let target = target.clone();
                cx.defer(move |cx| {
                    let pairs: Vec<_> = ids
                        .into_iter()
                        .map(|id| {
                            (
                                id,
                                cx.new(crate::editor::SqlEditor::new),
                                cx.new(crate::results::ResultsView::new),
                            )
                        })
                        .collect();
                    target
                        .update(cx, |nb, cx| nb.attach_cell_entities(pairs, cx))
                        .ok();
                });
            }
        })
        .detach();
    });
    view.update(cx, |v, cx| v.edit_cell("cell_1", cx));
    cx.run_until_parked();
    let editor = view.read_with(cx, |v, _| v.cell_entities("cell_1").unwrap().0);

    let mut area = ParameterDef::new("area", ParamType::String);
    area.id = "region".into();
    view.update(cx, |v, cx| v.save_param_def(Some("region"), area, cx))
        .unwrap();
    assert_eq!(
        editor.read_with(cx, |e, _| e.text().to_string()),
        "SELECT * FROM t WHERE r = :area",
        "the open editor shows the rename"
    );
    editor.update(cx, |e, cx| {
        let text = format!("{} ", e.text());
        e.set_text(&text, cx);
        cx.emit(crate::editor::SqlEditorEvent::Change);
    });
    cx.run_until_parked();
    assert_eq!(
        view.read_with(cx, |v, _| v
            .doc()
            .model()
            .cell("cell_1")
            .unwrap()
            .source
            .clone()),
        "SELECT * FROM t WHERE r = :area ",
        "the next keystroke keeps the new name"
    );
}
