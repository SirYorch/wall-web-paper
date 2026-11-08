use crate::config::{ResolutionConfig, ResolvedWallpaperConfig};
use anyhow::{Context, Result, bail};
use gtk4::cairo;
use gtk4::gdk;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use webkit6::prelude::*;
use webkit6::{HardwareAccelerationPolicy, LoadEvent, Settings as WebKitSettings, WebView};

/// Información descriptiva de un monitor detectado en la sesión Wayland.
#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub index: usize,
    pub connector: String,
    pub model: String,
    pub manufacturer: String,
    pub width: i32,
    pub height: i32,
    pub scale_factor: i32,
    pub refresh_rate_mhz: i32,
    pub monitor: gdk::Monitor,
}

/// Obtiene todos los monitores activos reportados por el compositor Wayland.
pub fn enumerate_monitors(display: &gdk::Display) -> Vec<MonitorInfo> {
    let list_model = display.monitors();
    let mut monitors = Vec::new();

    for i in 0..list_model.n_items() {
        if let Some(obj) = list_model.item(i)
            && let Ok(monitor) = obj.downcast::<gdk::Monitor>()
        {
            let geom = monitor.geometry();
            let connector = monitor
                .connector()
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("Monitor-{}", i));
            let model = monitor
                .model()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "Desconocido".to_string());
            let manufacturer = monitor
                .manufacturer()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "Genérico".to_string());

            monitors.push(MonitorInfo {
                index: i as usize,
                connector,
                model,
                manufacturer,
                width: geom.width(),
                height: geom.height(),
                scale_factor: monitor.scale_factor(),
                refresh_rate_mhz: monitor.refresh_rate(),
                monitor,
            });
        }
    }

    monitors
}

/// Filtra los monitores que coinciden con el selector del JSON (`monitor`).
pub fn select_monitors(all_monitors: &[MonitorInfo], selector: &str) -> Result<Vec<MonitorInfo>> {
    if all_monitors.is_empty() {
        bail!("No se detectaron monitores activos en el servidor Wayland.");
    }

    let sel = selector.trim();
    let sel_lower = sel.to_lowercase();

    if sel.is_empty()
        || sel == "*"
        || sel_lower == "all"
        || sel_lower == "todos"
        || sel_lower == "auto"
    {
        return Ok(all_monitors.to_vec());
    }

    if sel_lower == "primary" || sel_lower == "principal" {
        return Ok(vec![all_monitors[0].clone()]);
    }

    let exact: Vec<MonitorInfo> = all_monitors
        .iter()
        .filter(|m| m.connector.eq_ignore_ascii_case(sel))
        .cloned()
        .collect();
    if !exact.is_empty() {
        return Ok(exact);
    }

    if let Ok(idx) = sel.parse::<usize>()
        && let Some(m) = all_monitors.get(idx)
    {
        return Ok(vec![m.clone()]);
    }

    let partial: Vec<MonitorInfo> = all_monitors
        .iter()
        .filter(|m| {
            m.connector.to_lowercase().contains(&sel_lower)
                || m.model.to_lowercase().contains(&sel_lower)
        })
        .cloned()
        .collect();
    if !partial.is_empty() {
        return Ok(partial);
    }

    let available: Vec<String> = all_monitors
        .iter()
        .map(|m| format!("{} ({}x{})", m.connector, m.width, m.height))
        .collect();

    bail!(
        "No se encontró el monitor '{}' especificado en el JSON. Monitores disponibles en Wayland: {}",
        sel,
        available.join(", ")
    );
}

/// Aplica estilos CSS globales de GTK4 para asegurar ventanas transparentes sin bordes ni márgenes.
pub fn install_global_css(display: &gdk::Display) {
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(
        "window, window.background, .wwp-transparent-catcher {
            background-color: transparent;
            background: transparent;
            border: none;
            box-shadow: none;
            outline: none;
            margin: 0;
            padding: 0;
        }",
    );
    gtk4::style_context_add_provider_for_display(
        display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

/// Instancia activa de un fondo de pantalla sobre un monitor Wayland.
pub struct WallpaperInstance {
    pub window: gtk4::ApplicationWindow,
    pub input_window: Option<gtk4::ApplicationWindow>,
    pub _webview: WebView,
    pub _file_monitor: Option<gio::FileMonitor>,
}

impl WallpaperInstance {
    pub fn close(&self) {
        if let Some(ref input_win) = self.input_window {
            input_win.close();
        }
        self.window.close();
    }
}

/// Crea la ventana visual en la capa `Background` (con namespace `"wallpaper"` para que Niri
/// la coloque en el backdrop global con `place-within-backdrop true`, igual que `swaybg`) y,
/// cuando `interactuable: true`, crea una capa de entrada 100% transparente que captura los
/// eventos del ratón/teclado en el escritorio y los reenvía al `WebView` del backdrop.
pub fn create_wallpaper_instance(
    app: &gtk4::Application,
    monitor_info: &MonitorInfo,
    cfg: &ResolvedWallpaperConfig,
) -> Result<WallpaperInstance> {
    if !gtk4_layer_shell::is_supported() {
        bail!(
            "El compositor Wayland actual no soporta el protocolo zwlr_layer_shell_v1 (wlr-layer-shell)."
        );
    }

    let (target_width, target_height) = match cfg.resolucion {
        ResolutionConfig::Auto => (monitor_info.width, monitor_info.height),
        ResolutionConfig::Custom { ancho, alto } => (ancho, alto),
    };

    let window = gtk4::ApplicationWindow::builder()
        .application(app)
        .title(format!("wall-web-paper [{}]", monitor_info.connector))
        .decorated(false)
        .default_width(target_width)
        .default_height(target_height)
        .build();

    // 1. Inicializar Wayland Layer Shell ANTES de realizar/mapear la ventana
    window.init_layer_shell();
    // Usar el namespace configurado (por defecto "wallpaper", igual que swaybg,
    // para que la regla `place-within-backdrop true` de Niri lo coloque en el backdrop global)
    window.set_namespace(Some(&cfg.namespace));
    window.set_layer(Layer::Background);
    window.set_monitor(Some(&monitor_info.monitor));
    window.set_exclusive_zone(-1);

    let stretch_horizontal = target_width == monitor_info.width;
    let stretch_vertical = target_height == monitor_info.height;

    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Left, true);
    window.set_anchor(Edge::Right, stretch_horizontal);
    window.set_anchor(Edge::Bottom, stretch_vertical);

    window.set_margin(Edge::Top, 0);
    window.set_margin(Edge::Left, 0);
    window.set_margin(Edge::Right, 0);
    window.set_margin(Edge::Bottom, 0);

    // La ventana visual en el backdrop delega la entrada a `input_window` si es interactuable
    window.set_keyboard_mode(KeyboardMode::None);

    // 2. Configurar el motor WebKitGTK 6.0
    let settings = WebKitSettings::new();
    settings.set_enable_javascript(true);
    settings.set_allow_file_access_from_file_urls(true);
    settings.set_allow_universal_access_from_file_urls(true);
    settings.set_enable_webgl(true);
    settings.set_enable_webaudio(true);
    settings.set_enable_media(true);
    settings.set_enable_mediasource(true);
    settings.set_media_playback_requires_user_gesture(false);
    settings.set_media_playback_allows_inline(true);
    settings.set_enable_html5_local_storage(true);
    settings.set_enable_html5_database(true);
    settings.set_enable_smooth_scrolling(true);
    settings.set_enable_page_cache(false);
    settings.set_enable_write_console_messages_to_stdout(true);
    settings.set_hardware_acceleration_policy(HardwareAccelerationPolicy::Always);
    settings.set_enable_developer_extras(cfg.inspector);

    let webview = WebView::builder()
        .settings(&settings)
        .hexpand(true)
        .vexpand(true)
        .width_request(target_width)
        .height_request(target_height)
        .can_target(cfg.interactuable)
        .can_focus(cfg.interactuable)
        .focus_on_click(cfg.interactuable)
        .sensitive(true)
        .build();

    let transparent = gdk::RGBA::new(0.0, 0.0, 0.0, 0.0);
    webview.set_background_color(&transparent);

    let inspector_enabled = cfg.inspector;
    webview.connect_context_menu(move |_, _, _| !inspector_enabled);

    // Inyectar metadatos y el puente de eventos DOM para que funcione la interactividad
    // incluso cuando la superficie visual está dentro del backdrop de Niri
    let connector_name = monitor_info.connector.clone();
    let is_interactive = cfg.interactuable;
    webview.connect_load_changed(move |view, event| {
        if event == LoadEvent::Finished {
            let js = format!(
                r#"
                window.__WALL_WEB_PAPER__ = Object.freeze({{
                    monitor: {monitor:?},
                    resolucion: {{ ancho: {w}, alto: {h} }},
                    interactuable: {interactive}
                }});
                window.dispatchEvent(new CustomEvent("wall-web-paper:ready", {{
                    detail: window.__WALL_WEB_PAPER__
                }}));
                {bridge_js}
                "#,
                monitor = connector_name,
                w = target_width,
                h = target_height,
                interactive = is_interactive,
                bridge_js = INPUT_BRIDGE_JS,
            );
            view.evaluate_javascript(&js, None, None, gio::Cancellable::NONE, |_| {});
        }
    });

    webview.load_uri(&cfg.uri_html);
    window.set_child(Some(&webview));

    // Vaciar la región de entrada de la ventana del backdrop (la entrada se captura en `input_window`)
    let clear_backdrop_input = move |win: &gtk4::ApplicationWindow| {
        if let Some(surface) = win.surface() {
            let empty_region = cairo::Region::create();
            surface.set_input_region(Some(&empty_region));
        }
    };
    window.connect_realize(clear_backdrop_input);
    window.connect_map(clear_backdrop_input);

    window.present();

    // 3. Si `interactuable: true`, crear la superficie 100% transparente capturadora de eventos
    // en el escritorio (fuera del backdrop para que Niri sí le envíe ratón y teclado)
    let input_window = if cfg.interactuable {
        Some(create_input_forwarder_window(
            app,
            monitor_info,
            target_width,
            target_height,
            stretch_horizontal,
            stretch_vertical,
            &webview,
        ))
    } else {
        None
    };

    // 4. Monitorear cambios en la carpeta HTML/CSS/JS para recarga en vivo (hot-reload)
    let file_monitor = if cfg.recarga_automatica {
        setup_directory_watcher(&cfg.carpeta_absoluta, &webview).ok()
    } else {
        None
    };

    if cfg.inspector
        && let Some(inspector) = webview.inspector()
    {
        inspector.show();
    }

    println!(
        "[wall-web-paper] Activo en monitor '{}' | Namespace: '{}' | Resolución: {}x{} | Interactuable: {} | URI: {}",
        monitor_info.connector,
        cfg.namespace,
        target_width,
        target_height,
        cfg.interactuable,
        cfg.uri_html
    );

    Ok(WallpaperInstance {
        window,
        input_window,
        _webview: webview,
        _file_monitor: file_monitor,
    })
}

/// Crea una superficie Wayland 100% transparente en `Layer::Background` con namespace
/// `"wall-web-paper-input"`. Al ser transparente, no se ve duplicada en los escritorios ni en el
/// Overview de Niri, pero permite capturar todos los movimientos del ratón, clics, rueda y teclado
/// sobre el fondo del escritorio y reenviarlos en tiempo real al `WebView` situado en el backdrop.
fn create_input_forwarder_window(
    app: &gtk4::Application,
    monitor_info: &MonitorInfo,
    target_width: i32,
    target_height: i32,
    stretch_horizontal: bool,
    stretch_vertical: bool,
    webview: &WebView,
) -> gtk4::ApplicationWindow {
    let input_win = gtk4::ApplicationWindow::builder()
        .application(app)
        .title(format!("wall-web-paper-input [{}]", monitor_info.connector))
        .decorated(false)
        .default_width(target_width)
        .default_height(target_height)
        .css_classes(["wwp-transparent-catcher"])
        .build();

    input_win.init_layer_shell();
    input_win.set_namespace(Some("wall-web-paper-input"));
    input_win.set_layer(Layer::Background);
    input_win.set_monitor(Some(&monitor_info.monitor));
    input_win.set_exclusive_zone(-1);

    input_win.set_anchor(Edge::Top, true);
    input_win.set_anchor(Edge::Left, true);
    input_win.set_anchor(Edge::Right, stretch_horizontal);
    input_win.set_anchor(Edge::Bottom, stretch_vertical);
    input_win.set_keyboard_mode(KeyboardMode::OnDemand);

    let area = gtk4::DrawingArea::builder()
        .hexpand(true)
        .vexpand(true)
        .content_width(target_width)
        .content_height(target_height)
        .can_target(true)
        .can_focus(true)
        .focusable(true)
        .focus_on_click(true)
        .css_classes(["wwp-transparent-catcher"])
        .build();

    // Dibujar completamente transparente (alfa = 0.0)
    area.set_draw_func(|_, cr, _, _| {
        cr.set_operator(cairo::Operator::Clear);
        let _ = cr.paint();
    });

    input_win.set_child(Some(&area));

    let set_full_region = move |win: &gtk4::ApplicationWindow| {
        if let Some(surface) = win.surface() {
            let rect = cairo::RectangleInt::new(0, 0, target_width.max(1), target_height.max(1));
            let region = cairo::Region::create_rectangle(&rect);
            surface.set_input_region(Some(&region));
        }
    };
    input_win.connect_realize(set_full_region);
    input_win.connect_map(set_full_region);

    // 1. Controlador de movimiento del puntero (con limitador a ~60 FPS para fluidez sin saturación)
    let motion = gtk4::EventControllerMotion::new();
    let last_move = Rc::new(Cell::new(Instant::now() - Duration::from_millis(50)));
    {
        let webview_weak = webview.downgrade();
        let last_move = last_move.clone();
        motion.connect_motion(move |_, x, y| {
            let now = Instant::now();
            if now.duration_since(last_move.get()) < Duration::from_millis(14) {
                return;
            }
            last_move.set(now);
            if let Some(view) = webview_weak.upgrade() {
                let js = format!(
                    "if(window.__WWP_MOVE__) window.__WWP_MOVE__({:.1}, {:.1});",
                    x, y
                );
                view.evaluate_javascript(&js, None, None, gio::Cancellable::NONE, |_| {});
            }
        });
    }
    {
        let webview_weak = webview.downgrade();
        motion.connect_leave(move |_| {
            if let Some(view) = webview_weak.upgrade() {
                view.evaluate_javascript(
                    "if(window.__WWP_LEAVE__) window.__WWP_LEAVE__();",
                    None,
                    None,
                    gio::Cancellable::NONE,
                    |_| {},
                );
            }
        });
    }
    area.add_controller(motion);

    // 2. Controlador de clics (todos los botones del ratón)
    let click = gtk4::GestureClick::builder().button(0).build();
    {
        let webview_weak = webview.downgrade();
        let area_weak = area.downgrade();
        click.connect_pressed(move |gesture, n_press, x, y| {
            if let Some(a) = area_weak.upgrade() {
                a.grab_focus();
            }
            let gdk_btn = gesture.current_button();
            let dom_btn = match gdk_btn {
                1 => 0,
                2 => 1,
                3 => 2,
                _ => 0,
            };
            if let Some(view) = webview_weak.upgrade() {
                let js = format!(
                    "if(window.__WWP_DOWN__) window.__WWP_DOWN__({}, {}, {:.1}, {:.1});",
                    dom_btn, n_press, x, y
                );
                view.evaluate_javascript(&js, None, None, gio::Cancellable::NONE, |_| {});
            }
        });
    }
    {
        let webview_weak = webview.downgrade();
        click.connect_released(move |gesture, n_press, x, y| {
            let gdk_btn = gesture.current_button();
            let dom_btn = match gdk_btn {
                1 => 0,
                2 => 1,
                3 => 2,
                _ => 0,
            };
            if let Some(view) = webview_weak.upgrade() {
                let js = format!(
                    "if(window.__WWP_UP__) window.__WWP_UP__({}, {}, {:.1}, {:.1});",
                    dom_btn, n_press, x, y
                );
                view.evaluate_javascript(&js, None, None, gio::Cancellable::NONE, |_| {});
            }
        });
    }
    area.add_controller(click);

    // 3. Controlador de rueda de desplazamiento (scroll)
    let scroll = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::BOTH_AXES);
    {
        let webview_weak = webview.downgrade();
        scroll.connect_scroll(move |_, dx, dy| {
            if let Some(view) = webview_weak.upgrade() {
                let js = format!(
                    "if(window.__WWP_SCROLL__) window.__WWP_SCROLL__({:.2}, {:.2});",
                    dx * 40.0,
                    dy * 40.0
                );
                view.evaluate_javascript(&js, None, None, gio::Cancellable::NONE, |_| {});
            }
            glib::Propagation::Proceed
        });
    }
    area.add_controller(scroll);

    // 4. Controlador de teclado para elementos interactivos (inputs, atajos JS, etc.)
    let key_ctrl = gtk4::EventControllerKey::new();
    {
        let webview_weak = webview.downgrade();
        key_ctrl.connect_key_pressed(move |_, keyval, _keycode, _state| {
            if let Some(view) = webview_weak.upgrade() {
                let key_name = keyval
                    .to_unicode()
                    .map(|c| c.to_string())
                    .or_else(|| keyval.name().map(|s| s.to_string()))
                    .unwrap_or_default();
                let js = format!(
                    "if(window.__WWP_KEY__) window.__WWP_KEY__('keydown', {:?});",
                    key_name
                );
                view.evaluate_javascript(&js, None, None, gio::Cancellable::NONE, |_| {});
            }
            glib::Propagation::Proceed
        });
    }
    {
        let webview_weak = webview.downgrade();
        key_ctrl.connect_key_released(move |_, keyval, _keycode, _state| {
            if let Some(view) = webview_weak.upgrade() {
                let key_name = keyval
                    .to_unicode()
                    .map(|c| c.to_string())
                    .or_else(|| keyval.name().map(|s| s.to_string()))
                    .unwrap_or_default();
                let js = format!(
                    "if(window.__WWP_KEY__) window.__WWP_KEY__('keyup', {:?});",
                    key_name
                );
                view.evaluate_javascript(&js, None, None, gio::Cancellable::NONE, |_| {});
            }
        });
    }
    area.add_controller(key_ctrl);

    // 5. Si estamos en Niri ($NIRI_SOCKET), desactivar la captura de entrada mientras el
    // Overview (Mod+O) está abierto para no interferir con los clics del Overview de Niri
    setup_niri_overview_watcher(&input_win, target_width, target_height);

    input_win.present();
    input_win
}

/// Escucha el socket IPC de Niri (`$NIRI_SOCKET`) en un hilo ligero para saber cuándo se abre
/// o cierra el Overview (`Mod+O`) y ajustar la región de entrada de `input_win`.
fn setup_niri_overview_watcher(
    input_win: &gtk4::ApplicationWindow,
    width: i32,
    height: i32,
) {
    let Ok(socket_path) = std::env::var("NIRI_SOCKET") else {
        return;
    };

    let (tx, rx) = mpsc::channel::<bool>();
    std::thread::spawn(move || {
        let Ok(mut stream) = UnixStream::connect(&socket_path) else {
            return;
        };
        if stream.write_all(b"\"EventStream\"\n").is_err() {
            return;
        }
        let _ = stream.flush();

        let reader = BufReader::new(stream);
        for line in reader.lines().map_while(Result::ok) {
            if line.contains("\"OverviewOpenedOrClosed\"") {
                let is_open = line.contains("\"is_open\":true");
                if tx.send(is_open).is_err() {
                    break;
                }
            }
        }
    });

    let win_weak = input_win.downgrade();
    glib::timeout_add_local(Duration::from_millis(50), move || {
        let Some(win) = win_weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        while let Ok(is_open) = rx.try_recv() {
            if let Some(surface) = win.surface() {
                if is_open {
                    let empty = cairo::Region::create();
                    surface.set_input_region(Some(&empty));
                } else {
                    let rect = cairo::RectangleInt::new(0, 0, width.max(1), height.max(1));
                    let region = cairo::Region::create_rectangle(&rect);
                    surface.set_input_region(Some(&region));
                }
            }
        }
        glib::ControlFlow::Continue
    });
}

/// Script JS inyectado en el WebView que recibe los eventos de ratón/teclado desde la ventana
/// capturadora transparente y dispara eventos DOM reales (`pointermove`, `mousemove`, `pointerdown`,
/// `mousedown`, `pointerup`, `mouseup`, `click`, `wheel`, `keydown`, `keyup`) además de simular
/// estados CSS `:hover` y `:active`.
const INPUT_BRIDGE_JS: &str = r#"
(() => {
    if (window.__WWP_BRIDGE_INSTALLED__) return;
    window.__WWP_BRIDGE_INSTALLED__ = true;

    let lastX = window.innerWidth / 2;
    let lastY = window.innerHeight / 2;
    let lastHovered = null;

    // Duplicar reglas CSS :hover y :active a atributos [data-wwp-hover] y [data-wwp-active]
    function patchHoverStyles() {
        for (const sheet of Array.from(document.styleSheets)) {
            let rules;
            try { rules = sheet.cssRules; } catch (_) { continue; }
            if (!rules) continue;
            const len = rules.length;
            for (let i = 0; i < len; i++) {
                const rule = rules[i];
                if (rule.selectorText && (rule.selectorText.includes(":hover") || rule.selectorText.includes(":active"))) {
                    const newSel = rule.selectorText
                        .replace(/:hover/g, "[data-wwp-hover]")
                        .replace(/:active/g, "[data-wwp-active]");
                    try {
                        sheet.insertRule(`${newSel} { ${rule.style.cssText} }`, sheet.cssRules.length);
                    } catch (_) {}
                }
            }
        }
    }
    patchHoverStyles();

    function updateHoverChain(el) {
        for (const prev of document.querySelectorAll("[data-wwp-hover]")) {
            prev.removeAttribute("data-wwp-hover");
        }
        let cur = el;
        while (cur && cur !== document && cur !== document.documentElement) {
            if (cur.setAttribute) cur.setAttribute("data-wwp-hover", "");
            cur = cur.parentElement;
        }
    }

    function updateActiveChain(el, active) {
        for (const prev of document.querySelectorAll("[data-wwp-active]")) {
            prev.removeAttribute("data-wwp-active");
        }
        if (!active) return;
        let cur = el;
        while (cur && cur !== document && cur !== document.documentElement) {
            if (cur.setAttribute) cur.setAttribute("data-wwp-active", "");
            cur = cur.parentElement;
        }
    }

    window.__WWP_MOVE__ = (x, y) => {
        lastX = x;
        lastY = y;
        const target = document.elementFromPoint(x, y) || document.body || window;

        if (target !== lastHovered) {
            if (lastHovered) {
                lastHovered.dispatchEvent(new PointerEvent("pointerout", { bubbles: true, clientX: x, clientY: y }));
                lastHovered.dispatchEvent(new MouseEvent("mouseout", { bubbles: true, clientX: x, clientY: y }));
            }
            if (target && target.dispatchEvent) {
                target.dispatchEvent(new PointerEvent("pointerover", { bubbles: true, clientX: x, clientY: y }));
                target.dispatchEvent(new MouseEvent("mouseover", { bubbles: true, clientX: x, clientY: y }));
            }
            updateHoverChain(target);
            lastHovered = target;
        }

        const ptrEv = new PointerEvent("pointermove", {
            bubbles: true,
            cancelable: true,
            clientX: x,
            clientY: y,
            screenX: x,
            screenY: y,
            pointerId: 1,
            isPrimary: true
        });
        const mouseEv = new MouseEvent("mousemove", {
            bubbles: true,
            cancelable: true,
            clientX: x,
            clientY: y,
            screenX: x,
            screenY: y
        });
        target.dispatchEvent(ptrEv);
        target.dispatchEvent(mouseEv);
    };

    window.__WWP_LEAVE__ = () => {
        updateHoverChain(null);
        updateActiveChain(null, false);
        lastHovered = null;
        window.dispatchEvent(new PointerEvent("pointerleave", { bubbles: false }));
        window.dispatchEvent(new MouseEvent("mouseleave", { bubbles: false }));
    };

    window.__WWP_DOWN__ = (button, nPress, x, y) => {
        lastX = x;
        lastY = y;
        const target = document.elementFromPoint(x, y) || document.body || window;
        updateHoverChain(target);
        updateActiveChain(target, true);

        const buttonsMask = button === 0 ? 1 : button === 2 ? 2 : 4;
        target.dispatchEvent(new PointerEvent("pointerdown", {
            bubbles: true,
            cancelable: true,
            button,
            buttons: buttonsMask,
            clientX: x,
            clientY: y,
            screenX: x,
            screenY: y,
            pointerId: 1,
            isPrimary: true
        }));
        target.dispatchEvent(new MouseEvent("mousedown", {
            bubbles: true,
            cancelable: true,
            button,
            buttons: buttonsMask,
             detail: nPress,
            clientX: x,
            clientY: y,
            screenX: x,
            screenY: y
        }));
    };

    window.__WWP_UP__ = (button, nPress, x, y) => {
        lastX = x;
        lastY = y;
        const target = document.elementFromPoint(x, y) || document.body || window;
        updateActiveChain(target, false);

        target.dispatchEvent(new PointerEvent("pointerup", {
            bubbles: true,
            cancelable: true,
            button,
            buttons: 0,
            clientX: x,
            clientY: y,
            screenX: x,
            screenY: y,
            pointerId: 1,
            isPrimary: true
        }));
        target.dispatchEvent(new MouseEvent("mouseup", {
            bubbles: true,
            cancelable: true,
            button,
            buttons: 0,
            detail: nPress,
            clientX: x,
            clientY: y,
            screenX: x,
            screenY: y
        }));

        if (button === 0) {
            target.dispatchEvent(new MouseEvent("click", {
                bubbles: true,
                cancelable: true,
                button: 0,
                buttons: 1,
                detail: nPress,
                clientX: x,
                clientY: y,
                screenX: x,
                screenY: y
            }));
            if (target && typeof target.focus === "function") {
                target.focus();
            }
        } else if (button === 2) {
            target.dispatchEvent(new MouseEvent("contextmenu", {
                bubbles: true,
                cancelable: true,
                button: 2,
                clientX: x,
                clientY: y
            }));
        }
    };

    window.__WWP_SCROLL__ = (dx, dy) => {
        const target = document.elementFromPoint(lastX, lastY) || document.body || window;
        target.dispatchEvent(new WheelEvent("wheel", {
            bubbles: true,
            cancelable: true,
            deltaX: dx,
            deltaY: dy,
            clientX: lastX,
            clientY: lastY
        }));
    };

    window.__WWP_KEY__ = (type, key) => {
        const target = document.activeElement || document.body || window;
        target.dispatchEvent(new KeyboardEvent(type, {
            bubbles: true,
            cancelable: true,
            key: key
        }));
        if (type === "keydown" && (target.tagName === "INPUT" || target.tagName === "TEXTAREA")) {
            if (key === "BackSpace" || key === "Backspace") {
                target.value = target.value.slice(0, -1);
                target.dispatchEvent(new Event("input", { bubbles: true }));
            } else if (key.length === 1) {
                target.value += key;
                target.dispatchEvent(new Event("input", { bubbles: true }));
            }
        }
    };
})();
"#;

/// Configura un observador `gio::FileMonitor` sobre la carpeta del wallpaper para recargar
/// automáticamente el WebView cuando se editen archivos HTML, CSS o JS.
fn setup_directory_watcher(
    folder: &std::path::Path,
    webview: &WebView,
) -> Result<gio::FileMonitor> {
    let dir_file = gio::File::for_path(folder);
    let monitor = dir_file
        .monitor_directory(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)
        .with_context(|| {
            format!(
                "No se pudo iniciar el monitor de archivos en '{}'",
                folder.display()
            )
        })?;

    let webview_weak = webview.downgrade();
    let pending_reload: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

    monitor.connect_changed(move |_, file, _, event_type| {
        if !matches!(
            event_type,
            gio::FileMonitorEvent::Changed
                | gio::FileMonitorEvent::ChangesDoneHint
                | gio::FileMonitorEvent::Created
                | gio::FileMonitorEvent::Moved
        ) {
            return;
        }

        if let Some(path) = file.path()
            && let Some(ext) = path.extension().and_then(|e| e.to_str())
        {
            let ext_lower = ext.to_lowercase();
            let relevant = matches!(
                ext_lower.as_str(),
                "html"
                    | "htm"
                    | "css"
                    | "js"
                    | "mjs"
                    | "json"
                    | "svg"
                    | "png"
                    | "jpg"
                    | "jpeg"
                    | "webp"
                    | "glsl"
            );
            if !relevant {
                return;
            }
        }

        if let Some(source_id) = pending_reload.borrow_mut().take() {
            source_id.remove();
        }

        let webview_weak_clone = webview_weak.clone();
        let pending_clone = pending_reload.clone();
        let source_id = glib::timeout_add_local_once(Duration::from_millis(250), move || {
            *pending_clone.borrow_mut() = None;
            if let Some(view) = webview_weak_clone.upgrade() {
                println!("[wall-web-paper] Cambios detectados en la carpeta web, recargando...");
                view.reload_bypass_cache();
            }
        });

        *pending_reload.borrow_mut() = Some(source_id);
    });

    Ok(monitor)
}
