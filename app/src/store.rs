//! profiles.json: settings + per-game profiles. Validated before every write; written atomically.
//! Same rules and messages as src/store.js.

use crate::js::{is_int, utf16_len};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use unicode_normalization::UnicodeNormalization;

pub const MAX_PROFILES: usize = 50;
pub const MAX_PROCESSES: usize = 10;
pub const MAX_ITEMS: usize = 100;

pub fn default_data() -> Value {
    json!({ "version": 1, "scene": "", "animateMs": 400, "defaultProfile": "", "profiles": [] })
}

/// "Elden Ring: Nightreign!" -> "elden-ring-nightreign"; "-2", "-3"... when taken.
pub fn slugify(name: &str, taken: &[String]) -> String {
    let folded: String = name.nfkd().filter(|c| !('\u{300}'..='\u{36f}').contains(c)).collect::<String>().to_lowercase();
    let mut slug = String::new();
    let mut gap = false;
    for c in folded.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if gap && !slug.is_empty() {
                slug.push('-');
            }
            gap = false;
            slug.push(c);
        } else {
            gap = true;
        }
    }
    // JS: replace runs with "-", trim dashes, then slice(0, 32) (which can end on a dash).
    let mut base: String = slug.chars().take(32).collect();
    if base.is_empty() {
        base = "profile".into();
    }
    if !taken.iter().any(|t| *t == base) {
        return base;
    }
    (2..).map(|i| format!("{base}-{i}")).find(|c| !taken.iter().any(|t| t == c)).unwrap_or(base)
}

fn is_id(v: &Value) -> bool {
    let Some(s) = v.as_str() else { return false };
    let b = s.as_bytes();
    !b.is_empty() && b.len() <= 40 && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit()) && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

pub fn is_exe(s: &str) -> bool {
    let n = s.len();
    if n < 5 || !s.is_char_boundary(n - 4) || !s[n - 4..].eq_ignore_ascii_case(".exe") {
        return false;
    }
    let stem = &s[..n - 4];
    (1..=80).contains(&utf16_len(stem)) && !stem.chars().any(|c| "\\/:*?\"<>|".contains(c))
}

fn is_size(v: &Value, a: &str, b: &str, min: f64) -> bool {
    v.is_object() && [&v[a], &v[b]].iter().all(|n| is_int(n) && n.as_f64().is_some_and(|f| f >= min && f <= 16384.0))
}

/// Returns a list of "path: problem" strings; empty when valid.
pub fn validate(d: &Value) -> Vec<String> {
    let mut e = Vec::new();
    let Some(_) = d.as_object() else { return vec!["data must be an object".into()] };
    if !d["scene"].as_str().is_some_and(|s| utf16_len(s) <= 200) {
        e.push("scene: must be a scene name".into());
    }
    if !(is_int(&d["animateMs"]) && d["animateMs"].as_f64().is_some_and(|f| (0.0..=3000.0).contains(&f))) {
        e.push("animateMs: 0-3000".into());
    }
    let Some(profiles) = d["profiles"].as_array().filter(|p| p.len() <= MAX_PROFILES) else {
        e.push(format!("profiles: at most {MAX_PROFILES}"));
        return e;
    };
    let mut ids: Vec<&Value> = Vec::new();
    for (i, p) in profiles.iter().enumerate() {
        let at = format!("profiles[{i}]");
        if !p.is_object() {
            e.push(format!("{at}: must be an object"));
            continue;
        }
        let id = &p["id"];
        if !is_id(id) {
            e.push(format!("{at}.id: invalid"));
        }
        if ids.contains(&id) {
            e.push(format!("{at}.id: duplicate"));
        }
        ids.push(id);
        if !p["name"].as_str().is_some_and(|n| !n.trim().is_empty() && utf16_len(n) <= 60) {
            e.push(format!("{at}.name: 1-60 characters"));
        }
        let procs_ok = p["processes"].as_array().is_some_and(|a| a.len() <= MAX_PROCESSES && a.iter().all(|x| x.as_str().is_some_and(is_exe)));
        if !procs_ok {
            e.push(format!("{at}.processes: up to {MAX_PROCESSES} names like game.exe"));
        }
        if p.get("canvas").is_some() && !is_size(&p["canvas"], "width", "height", 1.0) {
            e.push(format!("{at}.canvas: invalid"));
        }
        match p["items"].as_object() {
            Some(items) if items.len() <= MAX_ITEMS => {
                for (name, it) in items {
                    let bad_item = !it.is_object() || !it["transform"].is_object() || !it["visible"].is_boolean();
                    if bad_item {
                        e.push(format!("{at}.items[\"{name}\"]: invalid"));
                    } else if it.get("size").is_some() && !is_size(&it["size"], "w", "h", 0.0) {
                        e.push(format!("{at}.items[\"{name}\"].size: invalid"));
                    } else if it.get("capture").is_some() && !(it["capture"].is_object() && it["capture"]["kind"].is_string() && it["capture"]["settings"].is_object()) {
                        e.push(format!("{at}.items[\"{name}\"].capture: invalid"));
                    }
                }
            }
            _ => e.push(format!("{at}.items: at most {MAX_ITEMS}")),
        }
    }
    let def = &d["defaultProfile"];
    if def.as_str() != Some("") && !ids.contains(&def) {
        e.push("defaultProfile: unknown profile".into());
    }
    e
}

/// Why an update didn't save.
#[derive(Debug)]
pub enum UpdateError {
    /// The change itself refused (status, message), like an HttpError thrown inside the JS callback.
    Rejected(u16, String),
    Invalid(Vec<String>),
    Io(String),
}

pub fn write_json_atomic(file: &Path, value: &Value) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(value).map_err(std::io::Error::other)? + "\n";
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, file)
}

pub struct Store {
    dir: PathBuf,
    file: PathBuf,
    data: Mutex<Value>,
}

impl Store {
    /// Loads profiles.json. A file that fails validation is moved aside; unreadable JSON is ignored.
    pub fn load(dir: &Path) -> std::io::Result<Store> {
        std::fs::create_dir_all(dir)?;
        let file = dir.join("profiles.json");
        let mut data = default_data();
        match std::fs::read_to_string(&file) {
            Ok(text) => {
                if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
                    let mut d = default_data();
                    if let (Some(o), Some(p)) = (d.as_object_mut(), parsed.as_object()) {
                        for (k, v) in p {
                            o.insert(k.clone(), v.clone());
                        }
                    }
                    if validate(&d).is_empty() {
                        data = d;
                    } else {
                        std::fs::rename(&file, dir.join(format!("profiles.broken-{}.json", crate::js::now_ms())))?;
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {}
            Err(e) => return Err(e),
        }
        Ok(Store { dir: dir.to_path_buf(), file, data: Mutex::new(data) })
    }

    pub fn data(&self) -> Value {
        self.data.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn with<R>(&self, f: impl FnOnce(&Value) -> R) -> R {
        f(&self.data.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn profiles(&self) -> Vec<Value> {
        self.with(|d| d["profiles"].as_array().cloned().unwrap_or_default())
    }

    /// Profile by id.
    pub fn profile(&self, id: &str) -> Option<Value> {
        self.with(|d| d["profiles"].as_array()?.iter().find(|p| p["id"].as_str() == Some(id)).cloned())
    }

    /// Apply f to a copy; saves only if the result is valid.
    pub fn update<R>(&self, f: impl FnOnce(&mut Value) -> Result<R, (u16, String)>) -> Result<R, UpdateError> {
        let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = data.clone();
        let r = f(&mut next).map_err(|(s, m)| UpdateError::Rejected(s, m))?;
        let errors = validate(&next);
        if !errors.is_empty() {
            return Err(UpdateError::Invalid(errors));
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| UpdateError::Io(e.to_string()))?;
        write_json_atomic(&self.file, &next).map_err(|e| UpdateError::Io(e.to_string()))?;
        *data = next;
        Ok(r)
    }
}

/// d.profiles as a mutable list (it always is one after validation).
pub fn profiles_mut(d: &mut Value) -> &mut Vec<Value> {
    if !d["profiles"].is_array() {
        d["profiles"] = Value::Array(Vec::new());
    }
    d["profiles"].as_array_mut().expect("array")
}

pub fn obj_mut(v: &mut Value) -> &mut Map<String, Value> {
    if !v.is_object() {
        *v = Value::Object(Map::new());
    }
    v.as_object_mut().expect("object")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_validation_and_slugs() {
        assert!(validate(&default_data()).is_empty());
        let mut bad = default_data();
        bad["defaultProfile"] = json!("x");
        bad["profiles"] = json!([{ "id": "A B", "name": "", "processes": ["notexe"], "items": {} }]);
        assert_eq!(validate(&bad).len(), 4);
        assert_eq!(slugify("Elden Ring: Nightreign!", &[]), "elden-ring-nightreign");
        assert_eq!(slugify("Elden Ring", &["elden-ring".into()]), "elden-ring-2");
        assert_eq!(slugify("¡Pokémon!", &[]), "pokemon");
        assert_eq!(slugify("???", &[]), "profile");
        assert!(is_exe("EldenRing.EXE") && !is_exe(".exe") && !is_exe("bad/name.exe"));
    }

    #[test]
    fn saves_atomically_and_refuses_invalid_data() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::load(dir.path()).unwrap();
        let r = store.update(|d| {
            d["animateMs"] = json!(5000);
            Ok(())
        });
        assert!(matches!(r, Err(UpdateError::Invalid(ref e)) if e == &vec!["animateMs: 0-3000".to_string()]));
        store
            .update(|d| {
                profiles_mut(d).push(json!({ "id": "a", "name": "A", "processes": ["a.exe"], "items": {} }));
                Ok(())
            })
            .unwrap();
        let again = Store::load(dir.path()).unwrap();
        assert_eq!(again.data()["profiles"][0]["id"], "a");
        std::fs::write(dir.path().join("profiles.json"), r#"{"animateMs": -1}"#).unwrap();
        let broken = Store::load(dir.path()).unwrap();
        assert_eq!(broken.data(), default_data());
        assert!(std::fs::read_dir(dir.path()).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().starts_with("profiles.broken-")));
    }
}
