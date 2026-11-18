use crate::{app::*, state::*};
use eframe::egui;
use serde_json::json;

#[test]
fn combined_row_shows_one_summary_and_keeps_later_rule_editing_correct() {
    let ctx = egui::Context::default();
    crate::theme::install(&ctx);
    let mut app = Desktop::initial();
    app.loaded = true;
    let state = json!({"revision":0,"version":"current","rules":[
        {"rule":{"old":"任务","new":"委托","mode":"sub","table":"A"}},
        {"rule":{"old":"任务","new":"委托","mode":"sub","table":"B"}},
        {"rule":{"old":"其他原词","new":"其他译词","mode":"exact","table":"A"}}
    ],"batches":[{"id":"batch","old":"任务","new":"委托","active":true,"version":"current","count":5}]});
    app.apply_state(state.clone());
    app.promotion_result =
        json!({"review_nonce":"report","items":[{"review_item":0,"missing":3,"attention":3} ]});
    app.rule_selection = Some(1);
    let mut frame = 0;
    let mut draw = |app: &mut Desktop, events| {
        frame += 1;
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900., 620.),
                )),
                time: Some(f64::from(frame) / 60.),
                events,
                ..Default::default()
            },
            |ui| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().inner_margin(18))
                    .show(ui, |ui| app.replacement(ui));
            },
        );
        output.textures_delta.clear();
        output
            .shapes
            .into_iter()
            .filter_map(|shape| match shape.shape {
                egui::Shape::Text(text) => Some((
                    text.galley.text().to_string(),
                    egui::Rect::from_min_size(text.pos, text.galley.size()),
                    shape.clip_rect,
                )),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let mut labels = Vec::new();
    for _ in 0..3 {
        labels = draw(&mut app, vec![]);
    }
    assert_eq!(
        labels
            .iter()
            .filter(|(label, _, _)| label == "任务")
            .count(),
        1
    );
    let (_, rect, clip) = labels
        .iter()
        .find(|(label, _, _)| label == "整表 2 · 指定文本 5 条")
        .unwrap();
    assert!(clip.contains_rect(*rect));
    assert!(labels.iter().any(|(label, _, _)| label == "生效范围"));
    let single_button = labels
        .iter()
        .find(|(label, _, _)| label == "单条修改")
        .unwrap()
        .1;
    let (_, promotion_button, promotion_clip) = labels
        .iter()
        .find(|(label, _, _)| label == "整理批量修改")
        .unwrap();
    assert!(promotion_button.left() > single_button.right());
    assert!(promotion_clip.contains_rect(*promotion_button));
    let (_, entry, clip) = labels
        .iter()
        .find(|(label, _, _)| label == "查看遗漏并补改…")
        .unwrap();
    assert!(clip.contains_rect(*entry));
    assert!(
        labels
            .iter()
            .any(|(label, _, _)| label.contains("3 段匹配文本"))
    );
    let mut sorted = state.clone();
    sorted["replacement_sort"] = json!("alphabetical");
    app.apply_state(sorted);
    assert_eq!(app.rule_selection, Some(0));
    for _ in 0..3 {
        labels = draw(&mut app, vec![]);
    }
    let pos = labels
        .iter()
        .find(|(label, _, _)| label == "编辑")
        .unwrap()
        .1
        .center();
    for pressed in [true, false] {
        draw(
            &mut app,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: Default::default(),
                },
            ],
        );
    }
    assert_eq!(app.rule_form.as_ref().unwrap().index, Some(2));
    assert_eq!(app.rule_form.as_ref().unwrap().old, "其他原词");
    let mut next = state;
    next["version"] = json!("next");
    app.apply_state(next);
    assert!(app.rule_selection.is_none());
}

#[test]
fn combined_editor_keeps_actions_visible_and_range_changes_are_only_a_draft() {
    let ctx = egui::Context::default();
    crate::theme::install(&ctx);
    let mut app = Desktop::initial();
    app.loaded = true;
    let members=(0..45).map(|index|json!({"index":index*10+10,"label":"文本表","edit":{"original":format!("任务{index}"),"replacement":format!("委托{index}"),"table":"A","batch_operation":{"before":format!("任务{index}")}}})).collect::<Vec<_>>();
    app.tables = vec![
        json!({"name":"A","label":"界面与系统提示"}),
        json!({"name":"B","label":"任务文本"}),
    ];
    app.combined_form = Some(CombinedForm::new(
        json!({"nonce":"test","old":"任务","new":"委托","tables":["A","B"],"members":members}),
    ));
    let mut frame = 0;
    let mut draw = |app: &mut Desktop, mut events: Vec<egui::Event>| {
        frame += 1;
        let modifiers = events
            .iter()
            .find_map(|event| match event {
                egui::Event::PointerButton { modifiers, .. } => Some(*modifiers),
                _ => None,
            })
            .unwrap_or_default();
        events.insert(0, egui::Event::ModifiersChanged(modifiers));
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900., 620.),
                )),
                time: Some(f64::from(frame) / 60.),
                events,
                ..Default::default()
            },
            |ui| app.combined_dialog(ui.ctx()),
        );
        output.textures_delta.clear();
        output
            .shapes
            .into_iter()
            .filter_map(|shape| match shape.shape {
                egui::Shape::Text(text) => Some((
                    text.galley.text().to_string(),
                    egui::Rect::from_min_size(text.pos, text.galley.size()),
                    shape.clip_rect,
                )),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let mut labels = Vec::new();
    for _ in 0..3 {
        labels = draw(&mut app, vec![]);
    }
    for label in ["保存", "选择文本表…", "移除"] {
        let (_, rect, clip) = labels.iter().find(|(text, _, _)| text == label).unwrap();
        assert!(
            rect.bottom() < 620.,
            "{label} is outside viewport: {rect:?}"
        );
        assert!(clip.contains_rect(*rect), "{label} is clipped");
    }
    let visible_members = labels
        .iter()
        .filter(|(text, rect, clip)| {
            text.starts_with("任务") && clip.width() <= 280. && clip.contains_rect(*rect)
        })
        .count();
    assert!(
        visible_members >= 4,
        "only {visible_members} list entries are visible"
    );
    let list_clip = labels
        .iter()
        .find(|(text, _, _)| text == "任务1")
        .unwrap()
        .2;
    assert!(
        !labels
            .iter()
            .any(|(text, _, clip)| text.starts_with("委托") && *clip == list_clip),
        "replacement results belong in the detail pane, not the summary list"
    );
    let (_, _, detail_clip) = labels
        .iter()
        .rev()
        .find(|(text, _, _)| text == "委托0")
        .unwrap();
    assert!(
        detail_clip.height() >= 80.,
        "full text is squeezed to one line: {detail_clip:?}"
    );
    let mut click = |app: &mut Desktop,
                     labels: &[(String, egui::Rect, egui::Rect)],
                     label: &str,
                     modifiers: egui::Modifiers| {
        let (_, rect, clip) = labels
            .iter()
            .filter(|(text, _, _)| text == label)
            .min_by(|a, b| a.2.width().total_cmp(&b.2.width()))
            .unwrap();
        assert!(clip.contains_rect(*rect), "{label} is clipped");
        let pos = rect.center();
        for pressed in [true, false] {
            draw(
                app,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers,
                    },
                ],
            );
        }
        let mut result = Vec::new();
        for _ in 0..3 {
            result = draw(app, vec![]);
        }
        result
    };
    labels = click(&mut app, &labels, "任务1", egui::Modifiers::CTRL);
    assert_eq!(
        app.combined_form.as_ref().unwrap().selection.rows,
        std::collections::BTreeSet::from([10, 20])
    );
    labels = click(&mut app, &labels, "任务3", egui::Modifiers::SHIFT);
    assert_eq!(
        app.combined_form.as_ref().unwrap().selection.rows,
        std::collections::BTreeSet::from([20, 30, 40])
    );
    labels = click(&mut app, &labels, "任务0", egui::Modifiers::CTRL);
    labels = click(&mut app, &labels, "任务2", egui::Modifiers::CTRL);
    assert_eq!(
        app.combined_form.as_ref().unwrap().selection.rows,
        std::collections::BTreeSet::from([10, 20, 40])
    );
    labels = click(&mut app, &labels, "移除（3）", egui::Modifiers::NONE);
    assert_eq!(
        app.combined_form.as_ref().unwrap().payload()["removed"],
        json!([10, 20, 40])
    );
    assert!(
        app.combined_form
            .as_ref()
            .unwrap()
            .selection
            .rows
            .is_empty()
    );
    assert!(
        !labels
            .iter()
            .any(|(text, _, _)| matches!(text.as_str(), "任务0" | "任务1" | "任务3"))
    );
    assert!(app.pending.is_empty());
    app.combined_form.as_mut().unwrap().new = "差事".into();
    labels = click(&mut app, &labels, "任务2", egui::Modifiers::NONE);
    labels = click(&mut app, &labels, "任务5", egui::Modifiers::SHIFT);
    assert_eq!(
        app.combined_form.as_ref().unwrap().selection.rows,
        std::collections::BTreeSet::from([30, 50, 60])
    );
    labels = click(&mut app, &labels, "移除（3）", egui::Modifiers::NONE);
    assert_eq!(
        app.combined_form.as_ref().unwrap().payload()["removed"],
        json!([10, 20, 30, 40, 50, 60])
    );
    assert!(labels.iter().any(|(text, _, _)| text == "指定文本 · 39 条"));
    assert!(labels.iter().any(|(text, _, _)| text.starts_with("差事")));
    assert_eq!(
        app.combined_form.as_ref().unwrap().payload()["tables"],
        json!(["A", "B"])
    );
    assert!(app.pending.is_empty());
}

#[test]
fn combined_summary_wraps_only_before_text_into_two_lines() {
    let ctx = egui::Context::default();
    crate::theme::install(&ctx);
    let before = "任务说明很长，需要折行查看完整的上下文。".repeat(8);
    let after = before.replace("任务", "委托");
    let mut app = Desktop::initial();
    app.loaded = true;
    app.combined_form = Some(CombinedForm::new(json!({
        "nonce":"test", "old":"任务", "new":"委托", "tables":["A"],
        "members":[{"index":10,"label":"文本表","edit":{"original":before,"replacement":after,"table":"A","batch_operation":{"before":before}}}]
    })));
    let mut output = None;
    for frame in 0..3 {
        let mut current = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900., 620.),
                )),
                time: Some(f64::from(frame) / 60.),
                ..Default::default()
            },
            |ui| app.combined_dialog(ui.ctx()),
        );
        current.textures_delta.clear();
        output = Some(current);
    }
    let shapes = &output.unwrap().shapes;
    let summary_clip = shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            egui::Shape::Text(text)
                if text.galley.text() == before && text.galley.rows.len() == 2 =>
            {
                Some(shape.clip_rect)
            }
            _ => None,
        })
        .unwrap();
    let list_text = shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if shape.clip_rect == summary_clip => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>();
    let summary = list_text
        .iter()
        .find(|text| text.galley.text() == before)
        .unwrap();
    assert_eq!(summary.galley.rows.len(), 2);
    assert!(
        list_text
            .iter()
            .all(|text| !text.galley.text().contains("委托"))
    );
    assert!(shapes.iter().any(
        |shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text() == after)
    ));
}
