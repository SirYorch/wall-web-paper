mod config;
mod wallpaper;

use anyhow::{Context, Result};
use clap::Parser;
use config::AppConfig;
use gtk4::gdk;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;
use wallpaper::{
    WallpaperInstance, create_wallpaper_instance, enumerate_monitors, install_global_css,
    select_monitors,
};

#[derive(Parser, Debug)]
#[command(
    name = "wall-web-paper",
    version,
    about = "Fondo de pantalla en Wayland que renderiza HTML, CSS y JS mediante wlr-layer-shell y WebKitGTK 6.0"
)]
struct Cli {
    /// Ruta al archivo JSON de configuración
    #[arg(short, long, default_value = "config.json")]
    config: PathBuf,

    /// Lista todos los monitores Wayland detectados con su conector y resolución y termina
    #[arg(short, long)]
    list_monitors: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // Forzar el backend Wayland en GDK4
    if std::env::var_os("GDK_BACKEND").is_none() {
        // SAFETY: Se ejecuta al inicio del hilo principal antes de inicializar GTK/hilos secundarios.
        unsafe {
            std::env::set_var("GDK_BACKEND", "wayland");
        }
    }

    if let Err(err) = gtk4::init() {
        eprintln!(
            "[Error] No se pudo inicializar GTK4 en Wayland: {}\nAsegúrate de estar ejecutando una sesión Wayland activa.",
            err
        );
        return ExitCode::FAILURE;
    }

    let Some(display) = gdk::Display::default() else {
        eprintln!("[Error] No se pudo obtener el Display Wayland por defecto.");
        return ExitCode::FAILURE;
    };

    if cli.list_monitors {
        print_monitors(&display);
        return ExitCode::SUCCESS;
    }

    if let Err(err) = run_app(&cli.config) {
        eprintln!("[Error] {:#}", err);
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

fn print_monitors(display: &gdk::Display) {
    let monitors = enumerate_monitors(display);
    if monitors.is_empty() {
        println!("No se detectaron monitores en el servidor Wayland.");
        return;
    }

    println!("Monitores Wayland detectados ({}):", monitors.len());
    for m in monitors {
        let hz = m.refresh_rate_mhz as f64 / 1000.0;
        println!(
            "  [{}] Conector: {:<10} | Resolución: {}x{} (escala {}x, {:.2} Hz) | {} {}",
            m.index, m.connector, m.width, m.height, m.scale_factor, hz, m.manufacturer, m.model
        );
    }
}

fn run_app(config_path: &Path) -> Result<()> {
    // Validar configuración JSON antes de arrancar el ciclo de vida de la aplicación
    let initial_configs = AppConfig::load_from_file(config_path)?;
    let config_path_abs = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.to_path_buf());

    let app = gtk4::Application::builder()
        .application_id("org.wallwebpaper.WaylandWallpaper")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let active_instances: Rc<RefCell<Vec<WallpaperInstance>>> = Rc::new(RefCell::new(Vec::new()));
    let config_file_monitor: Rc<RefCell<Option<gio::FileMonitor>>> = Rc::new(RefCell::new(None));

    let instances_ref = active_instances.clone();
    let cfg_monitor_ref = config_file_monitor.clone();

    app.connect_activate(move |app| {
        if let Some(display) = gdk::Display::default() {
            install_global_css(&display);
        }

        if let Err(err) = build_wallpapers(app, &initial_configs, &instances_ref) {
            eprintln!("[Error] Falló la creación del fondo de pantalla: {:#}", err);
            app.quit();
            return;
        }

        // Observar cambios en el archivo JSON de configuración para aplicarlos en caliente
        if let Ok(monitor) = watch_config_file(app, &config_path_abs, &instances_ref) {
            *cfg_monitor_ref.borrow_mut() = Some(monitor);
        }
    });

    // Ejecutar GTK sin pasar los argumentos propios del CLI para evitar conflictos con GApplication
    let empty_args: [&str; 0] = [];
    app.run_with_args(&empty_args);

    Ok(())
}

fn build_wallpapers(
    app: &gtk4::Application,
    configs: &[config::ResolvedWallpaperConfig],
    instances: &Rc<RefCell<Vec<WallpaperInstance>>>,
) -> Result<()> {
    // Cerrar instancias previas si se está recargando el archivo JSON
    {
        let mut current = instances.borrow_mut();
        for old in current.drain(..) {
            old.close();
        }
    }

    let display = gdk::Display::default().context("No hay un Display Wayland activo")?;
    let all_monitors = enumerate_monitors(&display);

    let mut new_instances = Vec::new();
    for cfg in configs {
        let matched_monitors = select_monitors(&all_monitors, &cfg.monitor)?;
        for monitor_info in matched_monitors {
            let instance = create_wallpaper_instance(app, &monitor_info, cfg)?;
            new_instances.push(instance);
        }
    }

    *instances.borrow_mut() = new_instances;
    Ok(())
}

fn watch_config_file(
    app: &gtk4::Application,
    config_path: &Path,
    instances: &Rc<RefCell<Vec<WallpaperInstance>>>,
) -> Result<gio::FileMonitor> {
    let file = gio::File::for_path(config_path);
    let monitor = file
        .monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)
        .with_context(|| {
            format!(
                "No se pudo observar el archivo de configuración '{}'",
                config_path.display()
            )
        })?;

    let app_weak = app.downgrade();
    let instances_clone = instances.clone();
    let path_owned = config_path.to_path_buf();
    let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

    monitor.connect_changed(move |_, _, _, event_type| {
        if !matches!(
            event_type,
            gio::FileMonitorEvent::Changed
                | gio::FileMonitorEvent::ChangesDoneHint
                | gio::FileMonitorEvent::Created
        ) {
            return;
        }

        if let Some(id) = pending.borrow_mut().take() {
            id.remove();
        }

        let app_weak_inner = app_weak.clone();
        let instances_inner = instances_clone.clone();
        let path_inner = path_owned.clone();
        let pending_inner = pending.clone();

        let source_id = glib::timeout_add_local_once(Duration::from_millis(300), move || {
            *pending_inner.borrow_mut() = None;
            let Some(app) = app_weak_inner.upgrade() else {
                return;
            };
            println!(
                "[wall-web-paper] Recargando configuración desde '{}'...",
                path_inner.display()
            );
            match AppConfig::load_from_file(&path_inner) {
                Ok(new_configs) => {
                    if let Err(err) = build_wallpapers(&app, &new_configs, &instances_inner) {
                        eprintln!("[Error] Al aplicar la nueva configuración JSON: {:#}", err);
                    }
                }
                Err(err) => {
                    eprintln!("[Error] El archivo JSON modificado no es válido: {:#}", err);
                }
            }
        });

        *pending.borrow_mut() = Some(source_id);
    });

    Ok(monitor)
}
