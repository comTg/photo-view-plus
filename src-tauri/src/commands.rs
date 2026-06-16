use tauri::State;

use crate::config::Profile;

#[tauri::command]
pub fn ping(profile: State<'_, Profile>) -> String {
    profile.as_str().to_string()
}
