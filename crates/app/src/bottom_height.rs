//! Numbered bottom-panel sizes, separately for each docked/detached layout.
use crate::{settings, AppWindow, BottomHeight};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::rc::Rc;

const KEY: &str = "bottom_panel_height_steps";
fn saved(value: &serde_json::Value) -> Vec<i32> {
    (0..10)
        .map(|i| value[i].as_i64().unwrap_or(3).clamp(1, 10) as i32)
        .collect()
}

pub fn install(app: &AppWindow) {
    app.global::<BottomHeight>()
        .set_levels(ModelRc::from(Rc::new(VecModel::from(saved(
            &settings::load()[KEY],
        )))));
    let weak = app.as_weak();
    app.global::<BottomHeight>()
        .on_set_level(move |profile, level| {
            let Some(app) = weak.upgrade() else { return };
            if !(0..10).contains(&profile) {
                return;
            }
            let ui = app.global::<BottomHeight>();
            let levels = ui.get_levels();
            levels.set_row_data(profile as usize, level.clamp(1, 10));
            ui.set_changes(ui.get_changes().wrapping_add(1));
            settings::set(KEY, serde_json::json!(levels.iter().collect::<Vec<_>>()));
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profiles_default_and_clamp_independently() {
        assert_eq!(saved(&serde_json::Value::Null), vec![3; 10]);
        assert_eq!(
            saved(&serde_json::json!([0, 5, 99, "bad", 1, 8])),
            vec![1, 5, 10, 3, 1, 8, 3, 3, 3, 3]
        );
    }
}
