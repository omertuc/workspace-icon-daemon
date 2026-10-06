use std::sync::{Arc, Mutex};

use image::{Rgba, RgbaImage};
use serde_json::json;

use super::*;
use crate::assets::BASE_FONT;
use crate::raster;

#[derive(Clone, Default)]
struct FakeIpc {
    tree: Arc<Mutex<serde_json::Value>>,
    commands: Arc<Mutex<Vec<String>>>,
}

impl FakeIpc {
    fn set_tree(&self, tree: serde_json::Value) {
        *self.tree.lock().unwrap() = tree;
    }

    fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }
}

impl Ipc for FakeIpc {
    fn get_tree(&mut self) -> Result<Node> {
        Ok(serde_json::from_value(self.tree.lock().unwrap().clone())?)
    }

    fn command(&mut self, command: &str) -> Result<()> {
        self.commands.lock().unwrap().push(command.to_string());
        Ok(())
    }

    fn get_config(&mut self) -> Result<String> {
        Ok("font pango:monospace 10\n".to_string())
    }
}

fn workspace(num: i32, name: &str, nodes: serde_json::Value) -> serde_json::Value {
    json!({"id": 1, "type": "root", "nodes": [{"id": 2, "type": "output", "nodes": [
        {"id": 3, "type": "workspace", "num": num, "name": name, "nodes": nodes}
    ]}]})
}

fn window(id: i64, app_id: &str, x: i32, y: i32) -> serde_json::Value {
    json!({"id": id, "type": "con", "app_id": app_id, "rect": {"x": x, "y": y}, "nodes": []})
}

static NOTIFICATIONS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn record_notification(summary: &str, _body: &str) {
    NOTIFICATIONS.lock().unwrap().push(summary.to_string());
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    icon: PathBuf,
    ipc: FakeIpc,
    daemon: Daemon,
}

impl Fixture {
    /// A daemon with one mapped program ("app"), whose font is installed.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let icon = root.join("app.png");
        raster::save_png(
            &RgbaImage::from_pixel(32, 32, Rgba([0, 0, 255, 255])),
            &icon,
        )
        .unwrap();
        let mut map = ProgramIconMap::load(&root.join("map.yaml")).unwrap();
        map.add_program("app", Some(&icon)).unwrap();
        map.save().unwrap();
        let ipc = FakeIpc::default();
        ipc.set_tree(json!({"id": 1, "type": "root", "nodes": []}));
        let settings = Settings {
            compositor: Compositor::Sway,
            program_icon_map_path: map.filepath.clone(),
            base_font: BASE_FONT,
            font_output_path: root.join("cache/cached.ttf"),
            font_family_name: DEFAULT_FONT_FAMILY_NAME.to_string(),
            unique_icons_mode: UniqueIconsMode::NumbersSubscript,
            use_placeholder_icon: true,
            workspace_icons: true,
            titlebar_icons: false,
            title_text_size: None,
            fonts_dir: root.join("fonts"),
        };
        let mut daemon = Daemon::new(Box::new(ipc.clone()), settings).unwrap();
        daemon.notifier = record_notification;
        create_icon_font(
            &map.icons(),
            BASE_FONT,
            &daemon.settings.font_output_path,
            DEFAULT_FONT_FAMILY_NAME,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("fonts")).unwrap();
        std::fs::copy(
            &daemon.settings.font_output_path,
            root.join("fonts/cached.ttf"),
        )
        .unwrap();
        Self {
            _dir: dir,
            root,
            icon,
            ipc,
            daemon,
        }
    }

    fn installed_font(&self) -> PathBuf {
        self.root.join("fonts/cached.ttf")
    }

    fn snapshot(&mut self) -> bool {
        let font = self.installed_font();
        self.daemon.snapshot_active_font(&font)
    }
}

#[test]
fn font_rebuild_preserves_codepoints_after_icon_removal() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut map = ProgramIconMap::load(&root.join("programs.yaml")).unwrap();
    for (name, color) in [
        ("removed", [255, 0, 0, 255]),
        ("retained", [0, 0, 255, 255]),
    ] {
        let path = root.join(format!("{name}.png"));
        raster::save_png(&RgbaImage::from_pixel(32, 32, Rgba(color)), &path).unwrap();
        map.add_program(name, Some(&path)).unwrap();
    }
    map.add_program("iconless", None).unwrap();
    map.save().unwrap();
    std::fs::remove_file(root.join("removed.png")).unwrap();

    let mut restored = ProgramIconMap::load(&map.filepath).unwrap();
    assert!(restored.modified_at_load);
    assert_eq!(restored.get_unicode_id("retained"), Some(0xEC02));
    // Shared icon paths must still receive their own glyphs.
    restored
        .add_program("another", Some(&root.join("retained.png")))
        .unwrap();
    let output = root.join("font.ttf");
    create_icon_font(&restored.icons(), BASE_FONT, &output, "TestIcons").unwrap();

    let info = font_builder::read_font_info(&output).unwrap();
    assert_eq!(info.family.as_deref(), Some("TestIcons"));
    assert_eq!(info.version.as_deref(), Some(FONT_LAYOUT_VERSION));
    let pua: HashSet<u32> = info
        .bitmap_codepoints
        .iter()
        .copied()
        .filter(|cp| (0xE000..=0xF8FF).contains(cp))
        .collect();
    assert_eq!(pua, HashSet::from([0xEC00, 0xEC02, 0xEC03]));
    let font = std::fs::read(&output).unwrap();
    let expected = raster::collect_image(&root.join("retained.png"), 109).unwrap();
    assert_eq!(font_builder::glyph_png(&font, 0xEC02).unwrap(), expected);
    assert_eq!(font_builder::glyph_png(&font, 0xEC03).unwrap(), expected);
    // Stacking glyphs and layout lines are included.
    let (top, bottom, middle) = stacked_codepoints(0xEC02).unwrap();
    for cp in [
        top,
        bottom,
        middle,
        TAB_UNDERLINE_CODEPOINT,
        SPLIT_LINE_CODEPOINT,
    ] {
        assert!(info.bitmap_codepoints.contains(&cp), "missing U+{cp:X}");
    }
}

#[test]
fn stacked_codepoints_cover_programs_and_favicons() {
    assert_eq!(
        stacked_codepoints(0xEC00),
        Some((0x108000, 0x10B000, 0x10E000))
    );
    assert_eq!(
        stacked_codepoints(0x100002),
        Some((0x108402, 0x10B402, 0x10E402))
    );
    assert_eq!(stacked_codepoints(0xE000), None);
    assert_eq!(stacked_codepoints(0x100000 + STACK_SLOTS), None);
}

#[test]
fn collects_native_and_xwayland_windows_in_layout_order() {
    let mut fixture = Fixture::new();
    let xwayland = json!({"id": 6, "type": "con", "window_properties": {"class": "Firefox"},
                          "rect": {"x": 0, "y": 3}, "nodes": []});
    fixture.ipc.set_tree(workspace(
        2,
        "2",
        json!([window(5, "foot", 500, 0), xwayland]),
    ));
    let tree = fixture.daemon.ipc.get_tree().unwrap();
    let workspaces = fixture.daemon.programs_by_workspace(&tree);
    assert_eq!(workspaces[0].programs, ["Firefox", "foot"]);
    assert_eq!(workspaces[0].num, 2);
}

#[test]
fn icon_count_modes_and_workspace_names() {
    let mut fixture = Fixture::new();
    let icons = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let processed = fixture.daemon.process_icons(icons(&["a", "b", "a"]));
    assert_eq!(processed, ["a₂", "b"]);
    fixture.daemon.settings.unique_icons_mode = UniqueIconsMode::NumbersSuperscript;
    assert_eq!(
        fixture.daemon.process_icons(icons(&["a", "a", "a"])),
        ["a³"]
    );
    fixture.daemon.settings.unique_icons_mode = UniqueIconsMode::Unique;
    assert_eq!(
        fixture.daemon.process_icons(icons(&["a", "b", "a"])),
        ["a", "b"]
    );
    fixture.daemon.settings.unique_icons_mode = UniqueIconsMode::Nonunique;
    assert_eq!(
        fixture.daemon.process_icons(icons(&["a", "b", "a"])),
        ["a", "b", "a"]
    );

    assert_eq!(
        construct_workspace_name(2, &icons(&["a₂", "b"]), None),
        "2: a₂b"
    );
    assert_eq!(
        construct_workspace_name(2, &icons(&["a"]), Some("2:")),
        "2: a"
    );
    assert_eq!(
        construct_workspace_name(3, &icons(&["a"]), Some("3: mail")),
        "3: mail a"
    );
    assert_eq!(construct_workspace_name(3, &[], Some("3: mail")), "3: mail");
    assert_eq!(construct_workspace_name(-1, &[], Some("web")), "web");

    let app = char::from_u32(0xEC01).unwrap();
    let placeholder = char::from_u32(PLACEHOLDER_CODEPOINT).unwrap();
    let daemon = &fixture.daemon;
    assert_eq!(
        daemon.workspace_base_name(&format!("3: mail {app}₂{placeholder}")),
        "3: mail"
    );
    assert_eq!(daemon.workspace_base_name("3: mail  "), "3: mail  ");
    assert_eq!(daemon.workspace_base_name(&format!("1: {app}")), "1:");
}

#[test]
fn workspaces_are_renamed_with_loaded_icons() {
    let mut fixture = Fixture::new();
    assert!(fixture.snapshot());
    fixture.ipc.set_tree(workspace(
        1,
        "1",
        json!([
            window(5, "app", 0, 0),
            window(6, "app", 100, 0),
            window(7, "unknown", 200, 0)
        ]),
    ));
    fixture.daemon.update_workspace_names().unwrap();
    let expected = format!(
        "rename workspace \"1\" to \"1: {}₂{}\"",
        char::from_u32(0xEC01).unwrap(),
        char::from_u32(PLACEHOLDER_CODEPOINT).unwrap()
    );
    assert_eq!(fixture.ipc.commands(), [expected]);

    fixture.daemon.settings.workspace_icons = false;
    fixture.daemon.update_workspace_names().unwrap();
    assert_eq!(fixture.ipc.commands().len(), 1);
}

#[test]
fn titlebar_icons_use_scoped_font_and_mapped_codepoint() {
    let mut fixture = Fixture::new();
    fixture.daemon.settings.use_placeholder_icon = false;
    assert!(fixture.snapshot());
    fixture.ipc.set_tree(workspace(
        1,
        "1",
        json!([window(41, "app", 0, 0), window(43, "unmapped", 0, 0)]),
    ));
    fixture.daemon.update_window_titles().unwrap();
    assert!(fixture.ipc.commands().is_empty(), "titlebar icons are off");

    fixture.daemon.settings.titlebar_icons = true;
    fixture.daemon.update_window_titles().unwrap();
    assert_eq!(
        fixture.ipc.commands(),
        [
            "[con_id=41] title_format \"&#x200B;<span font_family='WorkspaceIconDaemon' size='14pt'>&#xEC01;</span> %title\""
        ]
    );
    fixture.daemon.update_window_titles().unwrap();
    assert_eq!(
        fixture.ipc.commands().len(),
        1,
        "unchanged titles are not resent"
    );

    fixture.daemon.settings.title_text_size = Some(9.0);
    fixture.daemon.titlebar_icon_codepoints.clear();
    fixture.daemon.update_window_titles().unwrap();
    assert!(fixture.ipc.commands()[1].ends_with("</span> <span size='9pt'>%title</span>\""));
}

#[test]
fn split_containers_show_their_layout() {
    let mut fixture = Fixture::new();
    fixture.daemon.settings.titlebar_icons = true;
    assert!(fixture.snapshot());
    let split = json!({"id": 10, "type": "con", "layout": "splith", "nodes": [
        window(11, "app", 0, 0),
        {"id": 12, "type": "con", "layout": "splitv", "nodes": [
            window(13, "app", 0, 0), {"id": 14, "type": "con", "app_id": "app", "focused": true, "nodes": []}
        ]}
    ]});
    fixture.ipc.set_tree(workspace(1, "1", json!([split])));
    fixture.daemon.update_window_titles().unwrap();
    let commands = fixture.ipc.commands();
    let split_title = commands
        .iter()
        .find(|c| c.starts_with("[con_id=10]"))
        .unwrap();
    let (top, _, _) = stacked_codepoints(0xEC01).unwrap();
    let (_, bottom, _) = stacked_codepoints(0xEC01).unwrap();
    assert!(split_title.contains("<span foreground='#719cd6' weight='bold' size='10pt'>|</span>"));
    assert!(split_title.contains(&format!(
        "<span background='{FOCUS_HIGHLIGHT}'>{}{}{}</span>",
        glyph(top),
        glyph(SPLIT_LINE_CODEPOINT),
        glyph(bottom)
    )));
    assert!(split_title.contains("size='13pt'"));
    let nested = commands
        .iter()
        .find(|c| c.starts_with("[con_id=12]"))
        .unwrap();
    assert!(nested.contains(&glyph(top)));
}

#[test]
fn second_startup_uses_preinstalled_font_without_rebuilding() {
    let mut fixture = Fixture::new();
    // No desktop entries, so discovery adds nothing.
    let empty = fixture.root.join("empty");
    let installed_before = std::fs::read(fixture.installed_font()).unwrap();
    let started =
        desktop::tests::with_xdg(&empty, &[&empty], || fixture.daemon.ensure_startup_font());
    assert!(started.unwrap());
    assert_eq!(
        std::fs::read(fixture.installed_font()).unwrap(),
        installed_before
    );
    assert_eq!(fixture.daemon.active_unicode_id("app"), Some(0xEC01));
    assert!(fixture.daemon.stacking_available);
}

#[test]
fn first_startup_builds_notifies_and_exits_without_renaming() {
    let mut fixture = Fixture::new();
    std::fs::remove_file(fixture.installed_font()).unwrap();
    let empty = fixture.root.join("empty");
    let started =
        desktop::tests::with_xdg(&empty, &[&empty], || fixture.daemon.ensure_startup_font());
    assert!(!started.unwrap());
    if fixture.installed_font().exists() {
        // fc-cache is available, so the font was installed.
        assert!(font_builder::read_font_info(&fixture.installed_font()).is_ok());
    }
    assert!(fixture.ipc.commands().is_empty());
    assert!(
        NOTIFICATIONS
            .lock()
            .unwrap()
            .iter()
            .any(|n| n.contains("Icon font installed"))
    );
}

#[test]
fn new_program_is_installed_but_uses_loaded_placeholder() {
    let mut fixture = Fixture::new();
    let data = fixture.root.join("data");
    let apps = data.join("applications");
    std::fs::create_dir_all(&apps).unwrap();
    std::fs::write(
        apps.join("new-app.desktop"),
        format!("[Desktop Entry]\nIcon={}\n", fixture.icon.display()),
    )
    .unwrap();
    // Startup discovers the installed app first, so start without it.
    let empty = fixture.root.join("empty");
    desktop::tests::with_xdg(&empty, &[&empty], || {
        assert!(fixture.daemon.ensure_startup_font().unwrap())
    });

    fixture
        .ipc
        .set_tree(workspace(1, "1", json!([window(42, "new-app", 0, 0)])));
    desktop::tests::with_xdg(&data, &[&data], || {
        fixture.daemon.on_window_event("new", None).unwrap()
    });
    assert_eq!(
        fixture.daemon.active_unicode_id("new-app"),
        Some(PLACEHOLDER_CODEPOINT)
    );
    let assigned = fixture
        .daemon
        .program_icon_map
        .get_unicode_id("new-app")
        .unwrap();
    assert_ne!(assigned, PLACEHOLDER_CODEPOINT);
    assert_eq!(
        fixture.daemon.program_icon_map.get_icon_path("new-app"),
        Some(fixture.icon.as_path())
    );
    let built = font_builder::read_font_info(&fixture.daemon.settings.font_output_path).unwrap();
    assert!(built.bitmap_codepoints.contains(&assigned));
    assert!(
        NOTIFICATIONS
            .lock()
            .unwrap()
            .iter()
            .any(|n| n.contains("New application"))
    );
}

#[test]
fn installed_desktop_entries_and_startup_class_are_prebuilt() {
    let mut fixture = Fixture::new();
    let data = fixture.root.join("data");
    std::fs::create_dir_all(data.join("applications")).unwrap();
    std::fs::create_dir_all(data.join("icons/hicolor/128x128/apps")).unwrap();
    std::fs::copy(
        &fixture.icon,
        data.join("icons/hicolor/128x128/apps/example.png"),
    )
    .unwrap();
    std::fs::write(
        data.join("applications/org.example.App.desktop"),
        "[Desktop Entry]\nIcon=example\nStartupWMClass=ExampleClass\n",
    )
    .unwrap();
    fixture.daemon.program_icon_map =
        ProgramIconMap::load(&fixture.root.join("desktop-map.yaml")).unwrap();
    let added = desktop::tests::with_xdg(&data, &[&data], || {
        fixture.daemon.discover_installed_programs()
    });
    assert!(added.unwrap());
    let map = &fixture.daemon.program_icon_map;
    assert!(map.get_unicode_id("org.example.App").is_some());
    assert!(map.get_unicode_id("ExampleClass").is_some());
    assert_eq!(
        map.get_icon_path("ExampleClass"),
        Some(
            data.join("icons/hicolor/128x128/apps/example.png")
                .as_path()
        )
    );
}

#[test]
fn reset_restores_names_and_titles() {
    let mut fixture = Fixture::new();
    fixture.daemon.settings.titlebar_icons = true;
    let name = format!("1: {}", char::from_u32(0xEC01).unwrap());
    fixture
        .ipc
        .set_tree(workspace(1, &name, json!([window(5, "app", 0, 0)])));
    fixture.daemon.reset_desktop_state().unwrap();
    assert_eq!(
        fixture.ipc.commands(),
        [
            "[con_id=5] title_format \"%title\"".to_string(),
            format!("rename workspace \"{name}\" to \"1\"")
        ]
    );
}

#[test]
fn formats_numbers_like_python() {
    assert_eq!(format_g(14.000000000000002), "14");
    assert_eq!(format_g(10.0 * 1.4), "14");
    assert_eq!(format_g(13.0), "13");
    assert_eq!(format_g(19.6), "19.6");
    assert_eq!(format_g(10.5), "10.5");
    assert_eq!(escape("a'b\"<&>"), "a&#x27;b&quot;&lt;&amp;&gt;");
}
