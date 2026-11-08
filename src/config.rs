use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Configuración principal del programa cargada desde el archivo JSON.
/// Soporta tanto un único objeto de configuración como múltiples monitores.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum AppConfig {
    /// Configuración directa de un fondo (formato principal solicitado)
    Single(WallpaperConfig),
    /// Lista de configuraciones por monitor
    Multiple {
        #[serde(alias = "wallpapers", alias = "pantallas")]
        monitores: Vec<WallpaperConfig>,
    },
}

impl AppConfig {
    /// Carga y valida el archivo JSON de configuración.
    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Vec<ResolvedWallpaperConfig>> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path).with_context(|| {
            format!(
                "No se pudo leer el archivo de configuración JSON en '{}'",
                path.display()
            )
        })?;

        let base_dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));

        Self::parse_and_resolve(&raw, base_dir)
    }

    /// Parsea un string JSON y resuelve las rutas relativas respecto a `base_dir`.
    pub fn parse_and_resolve(raw_json: &str, base_dir: &Path) -> Result<Vec<ResolvedWallpaperConfig>> {
        let parsed: AppConfig = serde_json::from_str(raw_json)
            .context("Error al parsear el archivo JSON de configuración")?;

        let configs = match parsed {
            AppConfig::Single(cfg) => vec![cfg],
            AppConfig::Multiple { monitores } => {
                if monitores.is_empty() {
                    bail!("La lista de 'monitores' en el JSON no puede estar vacía");
                }
                monitores
            }
        };

        configs
            .into_iter()
            .map(|c| c.resolve(base_dir))
            .collect()
    }
}

/// Estructura del archivo JSON para configurar el monitor, resolución,
/// interactividad y carpeta con archivos HTML, CSS y JS.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WallpaperConfig {
    /// Nombre del conector del monitor en Wayland (ej. "DP-1", "HDMI-A-1", "eDP-1", "*" para todos)
    #[serde(default = "default_monitor", alias = "output", alias = "pantalla")]
    pub monitor: String,

    /// Resolución del fondo de pantalla (ej. {"ancho": 1920, "alto": 1080} o "1920x1080" o "auto")
    #[serde(default, alias = "resolution", alias = "tamano", alias = "size")]
    pub resolucion: ResolutionConfig,

    /// Define si el fondo permite interacción con ratón y teclado (true) o deja pasar los clics (false)
    #[serde(default, alias = "interactive", alias = "interactivo")]
    pub interactuable: bool,

    /// Ruta a la carpeta que contiene los archivos HTML, CSS y JS
    #[serde(alias = "folder", alias = "web_dir", alias = "directorio", alias = "path")]
    pub carpeta: PathBuf,

    /// Archivo HTML principal dentro de la carpeta (por defecto "index.html")
    #[serde(default = "default_entry_file", alias = "entry", alias = "index")]
    pub archivo_entrada: String,

    /// Recargar automáticamente el WebView cuando cambien los archivos HTML/CSS/JS
    #[serde(default = "default_true", alias = "hot_reload")]
    pub recarga_automatica: bool,

    /// Habilitar las herramientas de desarrollo (WebInspector)
    #[serde(default, alias = "devtools")]
    pub inspector: bool,

    /// Namespace de Wayland Layer Shell (por defecto "wallpaper", igual que swaybg,
    /// para que compositores como Niri lo ubiquen en el backdrop global con `place-within-backdrop true`)
    #[serde(default = "default_namespace", alias = "layer_namespace")]
    pub namespace: String,
}

fn default_monitor() -> String {
    "*".to_string()
}

fn default_entry_file() -> String {
    "index.html".to_string()
}

fn default_true() -> bool {
    true
}

fn default_namespace() -> String {
    "wallpaper".to_string()
}

/// Representa la resolución configurada en el JSON.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub enum ResolutionConfig {
    /// Usa automáticamente la resolución nativa del monitor reportada por Wayland
    #[default]
    Auto,
    /// Resolución explícita en píxeles lógicos
    Custom { ancho: i32, alto: i32 },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawResolution {
    Object {
        #[serde(alias = "width", alias = "w")]
        ancho: i32,
        #[serde(alias = "height", alias = "h")]
        alto: i32,
    },
    Text(String),
    Array([i32; 2]),
}

impl<'de> Deserialize<'de> for ResolutionConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt = Option::<RawResolution>::deserialize(deserializer)?;
        let Some(raw) = opt else {
            return Ok(ResolutionConfig::Auto);
        };

        match raw {
            RawResolution::Object { ancho, alto } => {
                validate_dims(ancho, alto).map_err(serde::de::Error::custom)
            }
            RawResolution::Array([ancho, alto]) => {
                validate_dims(ancho, alto).map_err(serde::de::Error::custom)
            }
            RawResolution::Text(s) => {
                let trimmed = s.trim().to_lowercase();
                if trimmed.is_empty() || trimmed == "auto" || trimmed == "nativa" || trimmed == "native" {
                    return Ok(ResolutionConfig::Auto);
                }
                let parts: Vec<&str> = trimmed.split(['x', '×', ',', ':']).collect();
                if parts.len() != 2 {
                    return Err(serde::de::Error::custom(format!(
                        "Formato de resolución inválido '{}'. Usa {{\"ancho\": 1920, \"alto\": 1080}}, \"1920x1080\" o \"auto\"",
                        s
                    )));
                }
                let ancho: i32 = parts[0].trim().parse().map_err(|_| {
                    serde::de::Error::custom(format!("Ancho inválido en resolución '{}'", s))
                })?;
                let alto: i32 = parts[1].trim().parse().map_err(|_| {
                    serde::de::Error::custom(format!("Alto inválido en resolución '{}'", s))
                })?;
                validate_dims(ancho, alto).map_err(serde::de::Error::custom)
            }
        }
    }
}

fn validate_dims(ancho: i32, alto: i32) -> std::result::Result<ResolutionConfig, String> {
    if ancho <= 0 || alto <= 0 {
        return Err(format!(
            "La resolución debe tener dimensiones positivas (recibido: {}x{})",
            ancho, alto
        ));
    }
    Ok(ResolutionConfig::Custom { ancho, alto })
}

/// Configuración validada con rutas absolutas listas para cargar en WebKit.
#[derive(Debug, Clone)]
pub struct ResolvedWallpaperConfig {
    pub monitor: String,
    pub resolucion: ResolutionConfig,
    pub interactuable: bool,
    pub carpeta_absoluta: PathBuf,
    pub uri_html: String,
    pub recarga_automatica: bool,
    pub inspector: bool,
    pub namespace: String,
}

impl WallpaperConfig {
    pub fn resolve(self, base_dir: &Path) -> Result<ResolvedWallpaperConfig> {
        let folder_path = if self.carpeta.is_absolute() {
            self.carpeta.clone()
        } else {
            base_dir.join(&self.carpeta)
        };

        let carpeta_absoluta = folder_path.canonicalize().with_context(|| {
            format!(
                "La carpeta de archivos web '{}' no existe o no es accesible",
                folder_path.display()
            )
        })?;

        if !carpeta_absoluta.is_dir() {
            bail!(
                "La ruta especificada en 'carpeta' ('{}') no es un directorio",
                carpeta_absoluta.display()
            );
        }

        let entry_candidate = carpeta_absoluta.join(&self.archivo_entrada);
        let archivo_html_absoluto = entry_candidate.canonicalize().with_context(|| {
            format!(
                "No se encontró el archivo HTML de entrada '{}' dentro de la carpeta '{}'",
                self.archivo_entrada,
                carpeta_absoluta.display()
            )
        })?;

        let uri_html = path_to_file_uri(&archivo_html_absoluto);
        let namespace = if self.namespace.trim().is_empty() {
            default_namespace()
        } else {
            self.namespace.trim().to_string()
        };

        Ok(ResolvedWallpaperConfig {
            monitor: self.monitor.trim().to_string(),
            resolucion: self.resolucion,
            interactuable: self.interactuable,
            carpeta_absoluta,
            uri_html,
            recarga_automatica: self.recarga_automatica,
            inspector: self.inspector,
            namespace,
        })
    }
}

/// Convierte una ruta absoluta de archivo en una URI `file://` válida para WebKit.
pub fn path_to_file_uri(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut encoded = String::with_capacity(raw.len() + 16);
    encoded.push_str("file://");
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'/'
            | b'-'
            | b'_'
            | b'.'
            | b'~' => encoded.push(byte as char),
            _ => encoded.push_str(&format!("%{:02X}", byte)),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_parse_object_resolution_and_spanish_fields() {
        let temp_dir = env::temp_dir().join("wall_web_paper_test_1");
        let _ = fs::create_dir_all(&temp_dir);
        let index_path = temp_dir.join("index.html");
        fs::write(&index_path, "<!DOCTYPE html><html></html>").unwrap();

        let json = r#"{
            "monitor": "DP-1",
            "resolucion": {
                "ancho": 1920,
                "alto": 1080
            },
            "interactuable": true,
            "carpeta": "."
        }"#;

        let resolved = AppConfig::parse_and_resolve(json, &temp_dir).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].monitor, "DP-1");
        assert_eq!(
            resolved[0].resolucion,
            ResolutionConfig::Custom {
                ancho: 1920,
                alto: 1080
            }
        );
        assert!(resolved[0].interactuable);
        assert!(resolved[0].uri_html.starts_with("file:///"));
        assert!(resolved[0].uri_html.ends_with("/index.html"));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_parse_string_resolution() {
        let temp_dir = env::temp_dir().join("wall_web_paper_test_2");
        let _ = fs::create_dir_all(&temp_dir);
        fs::write(temp_dir.join("index.html"), "<html></html>").unwrap();

        let json = r#"{
            "monitor": "HDMI-A-1",
            "resolucion": "2560x1440",
            "interactuable": false,
            "carpeta": "."
        }"#;

        let resolved = AppConfig::parse_and_resolve(json, &temp_dir).unwrap();
        assert_eq!(
            resolved[0].resolucion,
            ResolutionConfig::Custom {
                ancho: 2560,
                alto: 1440
            }
        );
        assert!(!resolved[0].interactuable);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_load_project_config_json() {
        let project_config = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.json");
        let resolved = AppConfig::load_from_file(&project_config).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].monitor, "HDMI-A-1");
        assert_eq!(
            resolved[0].resolucion,
            ResolutionConfig::Custom {
                ancho: 1920,
                alto: 1080
            }
        );
        assert!(resolved[0].interactuable);
        assert!(resolved[0].uri_html.ends_with("/wallpaper/index.html"));
    }
}
