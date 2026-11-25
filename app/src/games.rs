//! Which game is running: poll the Windows process list and pick the profile whose game started last.
//! Cheap (one process snapshot every few seconds), no admin rights, no injection. Same rules as
//! src/games.js; the process list comes from the Windows API instead of `tasklist`.

use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|c| *c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

/// Lower-case executable names of every running process.
pub fn list_processes() -> std::io::Result<HashSet<String>> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
    let mut names = HashSet::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut entry) != 0;
        while ok {
            let name = wide_to_string(&entry.szExeFile).to_lowercase();
            if !name.is_empty() {
                names.insert(name);
            }
            ok = Process32NextW(snap, &mut entry) != 0;
        }
        CloseHandle(snap);
    }
    Ok(names)
}

/// Apps with a visible, titled main window (for the "pick the game" list): [{ exe, title }] by exe.
pub fn list_windowed_apps() -> Vec<Value> {
    use windows_sys::Win32::Foundation::{BOOL, CloseHandle, HWND, LPARAM};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, WS_EX_TOOLWINDOW,
    };

    unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let found = unsafe { &mut *(lparam as *mut Vec<(u32, String)>) };
        unsafe {
            if IsWindowVisible(hwnd) == 0 || !GetWindow(hwnd, GW_OWNER).is_null() || (GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW) != 0 {
                return 1;
            }
            let mut title = [0u16; 512];
            let n = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32);
            if n <= 0 {
                return 1;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            found.push((pid, String::from_utf16_lossy(&title[..n as usize])));
        }
        1
    }

    let mut found: Vec<(u32, String)> = Vec::new();
    unsafe { EnumWindows(Some(each), &mut found as *mut _ as LPARAM) };
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for (pid, title) in found {
        let exe = unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                continue;
            }
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0;
            CloseHandle(h);
            if !ok {
                continue;
            }
            let path = String::from_utf16_lossy(&buf[..len as usize]);
            path.rsplit(['\\', '/']).next().unwrap_or("").to_lowercase()
        };
        if exe.is_empty() || seen.contains_key(&exe) {
            continue;
        }
        seen.insert(exe.clone(), crate::js::utf16_take(&title, 80));
        order.push(exe);
    }
    order.sort();
    order
        .into_iter()
        .map(|exe| {
            let title = seen[&exe].clone();
            json!({ "exe": exe, "title": title })
        })
        .collect()
}

/// The profile whose game started most recently; None when none is running. Window focus plays no part,
/// so alt-tabbing to an idle game left open in the background never switches anything. Ties (e.g.
/// everything already running when the app starts) go to the profile listed first.
pub fn match_profile<'a>(profiles: &'a [Value], running: &HashSet<String>, first_seen: &HashMap<String, u64>) -> Option<&'a Value> {
    let mut best = None;
    let mut best_at: Option<u64> = None;
    for p in profiles {
        let seen: Vec<String> = p["processes"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).map(str::to_lowercase).filter(|exe| running.contains(exe)).collect();
        if seen.is_empty() {
            continue;
        }
        let at = seen.iter().map(|exe| first_seen.get(exe).copied().unwrap_or(0)).max().unwrap_or(0);
        if best_at.is_none_or(|b| at > b) {
            best = Some(p);
            best_at = Some(at);
        }
    }
    best
}

pub type ListFn = Arc<dyn Fn() -> std::io::Result<HashSet<String>> + Send + Sync>;
pub type ProfilesFn = Arc<dyn Fn() -> Vec<Value> + Send + Sync>;
type Listener = Arc<dyn Fn(Option<String>) + Send + Sync>;

struct WatchState {
    /// None until the first poll (JS: undefined), then the matching profile id or None.
    current: Option<Option<String>>,
    first_seen: HashMap<String, u64>,
    polls: u64,
}

/// Calls its listeners with the matching profile id (or None) whenever it changes.
pub struct GameWatcher {
    get_profiles: ProfilesFn,
    list: ListFn,
    interval: Duration,
    st: Mutex<WatchState>,
    listeners: Mutex<Vec<Listener>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl GameWatcher {
    pub fn new(get_profiles: ProfilesFn, list: ListFn, interval: Duration) -> Arc<GameWatcher> {
        Arc::new(GameWatcher {
            get_profiles,
            list,
            interval,
            st: Mutex::new(WatchState { current: None, first_seen: HashMap::new(), polls: 0 }),
            listeners: Mutex::new(Vec::new()),
            task: Mutex::new(None),
        })
    }

    pub fn on_change(&self, f: impl Fn(Option<String>) + Send + Sync + 'static) {
        self.listeners.lock().unwrap_or_else(|e| e.into_inner()).push(Arc::new(f));
    }

    /// One poll. The process list is read on a blocking thread.
    pub async fn tick(&self) {
        let list = self.list.clone();
        let Ok(Ok(running)) = tokio::task::spawn_blocking(move || list()).await else {
            return; // a failed poll keeps the current profile
        };
        let changed = {
            let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
            // Remember when each program appeared. Whatever already runs at the first poll counts as "old" (0).
            let now = if st.polls == 0 { 0 } else { crate::js::now_ms() };
            st.polls += 1;
            for exe in &running {
                st.first_seen.entry(exe.clone()).or_insert(now);
            }
            st.first_seen.retain(|exe, _| running.contains(exe));
            let profiles = (self.get_profiles)();
            let id = match_profile(&profiles, &running, &st.first_seen).and_then(|p| p["id"].as_str()).map(String::from);
            if st.current.as_ref() != Some(&id) {
                st.current = Some(id.clone());
                Some(id)
            } else {
                None
            }
        };
        if let Some(id) = changed {
            let listeners = self.listeners.lock().unwrap_or_else(|e| e.into_inner()).clone();
            for f in listeners {
                f(id.clone());
            }
        }
    }

    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        let task = tokio::spawn(async move {
            loop {
                me.tick().await;
                tokio::time::sleep(me.interval).await;
            }
        });
        *self.task.lock().unwrap_or_else(|e| e.into_inner()) = Some(task);
    }

    pub fn stop(&self) {
        if let Some(t) = self.task.lock().unwrap_or_else(|e| e.into_inner()).take() {
            t.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn matching_is_case_insensitive_and_ties_go_to_the_first_profile() {
        let profiles = vec![json!({ "id": "a", "processes": ["EldenRing.exe"] }), json!({ "id": "b", "processes": ["valorant.exe", "eldenring.exe"] })];
        let none = HashMap::new();
        assert_eq!(match_profile(&profiles, &set(&["eldenring.exe", "valorant.exe"]), &none).unwrap()["id"], "a");
        assert_eq!(match_profile(&profiles, &set(&["valorant.exe"]), &none).unwrap()["id"], "b");
        assert!(match_profile(&profiles, &set(&["explorer.exe"]), &none).is_none());
    }

    #[tokio::test]
    async fn the_game_started_last_wins_over_an_idle_game_in_the_background() {
        let profiles = Arc::new(Mutex::new(vec![json!({ "id": "poe", "processes": ["pathofexilesteam.exe"] }), json!({ "id": "idle", "processes": ["idlegame.exe"] })]));
        let running = Arc::new(Mutex::new(set(&["idlegame.exe"])));
        let (p, r) = (profiles.clone(), running.clone());
        let w = GameWatcher::new(Arc::new(move || p.lock().unwrap().clone()), Arc::new(move || Ok(r.lock().unwrap().clone())), Duration::from_secs(3));
        let changes = Arc::new(Mutex::new(Vec::new()));
        let c = changes.clone();
        w.on_change(move |id| c.lock().unwrap().push(id));
        w.tick().await; // idle game was already open when the app started
        tokio::time::sleep(Duration::from_millis(5)).await;
        *running.lock().unwrap() = set(&["idlegame.exe", "pathofexilesteam.exe"]);
        w.tick().await; // PoE starts after it
        profiles.lock().unwrap().reverse();
        w.tick().await; // list order does not matter once start times differ: still PoE
        *running.lock().unwrap() = set(&["idlegame.exe"]);
        w.tick().await; // PoE closes: back to the idle game's layout
        let got: Vec<Option<String>> = changes.lock().unwrap().clone();
        assert_eq!(got, vec![Some("idle".into()), Some("poe".into()), Some("idle".into())]);
    }

    #[test]
    fn the_real_process_list_includes_this_program() {
        let names = list_processes().unwrap();
        let me = std::env::current_exe().unwrap().file_name().unwrap().to_string_lossy().to_lowercase();
        assert!(names.contains(&me), "{me} not in the list");
    }
}
