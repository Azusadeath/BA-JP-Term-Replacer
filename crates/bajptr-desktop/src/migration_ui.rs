use crate::{app::*, dialogs::window, state::*};
use eframe::egui::{self, RichText};
use serde_json::json;

fn conflict_text(ui: &mut egui::Ui, value: &str, other: &str) {
    let mut prefix = 0;
    for (a, _) in value.chars().zip(other.chars()).take_while(|(a, b)| a == b) {
        prefix += a.len_utf8();
    }
    let suffix: usize = value[prefix..]
        .chars()
        .rev()
        .zip(other[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(ch, _)| ch.len_utf8())
        .sum();
    let end = value.len() - suffix;
    let normal = egui::TextFormat::simple(
        egui::TextStyle::Body.resolve(ui.style()),
        ui.visuals().text_color(),
    );
    let mut job = egui::text::LayoutJob::default();
    job.append(&value[..prefix], 0., normal.clone());
    job.append(
        &value[prefix..end],
        0.,
        egui::TextFormat {
            background: SOFT_BLUE,
            ..normal.clone()
        },
    );
    job.append(&value[end..], 0., normal);
    ui.add(egui::Label::new(job).wrap());
}

fn comparison(ui: &mut egui::Ui, original: &str, replacement: &str) {
    ui.columns(2, |cols| {
        cols[0].colored_label(MUTED, "旧版原文");
        cols[0].add(egui::Label::new(original).wrap().selectable(true));
        cols[1].colored_label(MUTED, "旧版修改结果");
        cols[1].add(
            egui::Label::new(RichText::new(replacement).color(BLUE))
                .wrap()
                .selectable(true),
        );
    });
}

impl Desktop {
    pub fn migration_dialogs(&mut self, ctx: &egui::Context) {
        let Some(mut form) = self.migration_form.take() else {
            return;
        };
        if !form.open {
            self.migration_form = Some(form);
            return;
        }
        let busy = self.busy.is_some();
        let mut open = true;
        let visible_count = form.visible().len();
        window(
            "迁移旧版修改 · 核对文本",
            ctx,
            if visible_count == 0 {
                520.
            } else if visible_count == 1 {
                640.
            } else {
                780.
            },
        )
        .id(egui::Id::unique("migration-review-responsive"))
        .min_width(if visible_count == 0 {
            380.
        } else if visible_count == 1 {
            500.
        } else {
            680.
        })
        .max_width(if visible_count == 0 {
            520.
        } else {
            (ctx.content_rect().width() - 32.).max(680.)
        })
        .open(&mut open)
        .show(ctx, |ui| {
            ui.set_height(if visible_count == 0 {
                220.
            } else {
                (ctx.content_rect().height() - 130.).clamp(360., 620.)
            });
            let total = form.pending_count();
            let selected_count = form.selected_count();
            if total > 0 {
                ui.label(format!("待处理 {total} 段"));
            }
            if total > 1 {
                ui.add(
                    egui::TextEdit::singleline(&mut form.filter)
                        .hint_text("搜索文本或文本表")
                        .desired_width(ui.available_width()),
                );
            }
            form.sync_selection();
            egui::Panel::bottom("migration-actions")
                .frame(
                    egui::Frame::new()
                        .fill(egui::Color32::WHITE)
                        .inner_margin(egui::Margin::symmetric(0, 8)),
                )
                .show(ui, |ui| {
                    if !form.error.is_empty() {
                        ui.colored_label(RED, &form.error);
                    }
                    ui.horizontal(|ui| {
                        if visible_count > 1 && secondary(ui, !busy, "下一组").clicked() {
                            form.next();
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if primary(
                                ui,
                                !busy && selected_count > 0,
                                &format!("保存 {selected_count} 段"),
                            )
                            .clicked()
                            {
                                self.submit(
                                    "migration_commit",
                                    form.payload(),
                                    "正在保存迁移记录…",
                                );
                            }
                        });
                    });
                });
            let area = ui.available_rect_before_wrap();
            let visible = form.visible();
            let show_list = visible.len() > 1;
            let left_width = (area.width() * 0.28).clamp(170., 240.);
            let left = egui::Rect::from_min_max(
                area.min,
                egui::pos2(area.left() + left_width, area.bottom()),
            );
            let right = if show_list {
                egui::Rect::from_min_max(egui::pos2(left.right() + 16., area.top()), area.max)
            } else {
                area
            };
            if show_list {
                let mut list_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(left)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                );
                list_ui.shrink_clip_rect(left);
                let visible = form.visible();
                egui::ScrollArea::vertical()
                    .id_salt(("migration-groups", &form.filter))
                    .min_scrolled_height(0.)
                    .auto_shrink([false, false])
                    .show_rows(&mut list_ui, 60., visible.len(), |ui, range| {
                        for position in range {
                            let index = visible[position];
                            let group = &form.groups[index];
                            let caption = if !group.chosen.is_empty() {
                                format!("已选 {} 处", group.chosen.len())
                            } else {
                                format!(
                                    "记录 {} · {} 个候选",
                                    group.item["row_id"].as_str().map(str::to_owned)
                                        .unwrap_or_else(|| group.item["row_id"].to_string()),
                                    group.targets.len()
                                )
                            };
                            let summary = text(&group.item, "original").replace(['\r', '\n'], " ");
                            let (rect, response) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), 60.),
                                egui::Sense::click(),
                            );
                            if form.selected == Some(index) {
                                ui.painter().rect_filled(rect, 6, SOFT_BLUE);
                            } else if response.hovered() {
                                ui.painter().rect_filled(rect, 6, BG);
                            }
                            response.widget_info(|| {
                                egui::WidgetInfo::selected(
                                    egui::Role::Button,
                                    ui.is_enabled(),
                                    form.selected == Some(index),
                                    format!("{caption} {summary}"),
                                )
                            });
                            for (offset, label) in [
                                (7., RichText::new(caption).small().color(MUTED)),
                                (30., RichText::new(summary)),
                            ] {
                                let mut line = ui.new_child(
                                    egui::UiBuilder::new()
                                        .max_rect(egui::Rect::from_min_size(
                                            rect.min + egui::vec2(10., offset),
                                            egui::vec2(rect.width() - 20., 22.),
                                        ))
                                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                                );
                                let response = line.add(
                                    egui::Label::new(label)
                                        .selectable(false)
                                        .truncate()
                                        .show_tooltip_when_elided(false),
                                );
                                if offset == 30. {
                                    response.on_hover_text(text(&group.item, "original"));
                                }
                            }
                            if response.clicked() {
                                form.selected = Some(index);
                            }
                        }
                    });
            }
            let mut detail = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(right)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            detail.shrink_clip_rect(right);
            if let Some(index) = form.selected {
                let group = &form.groups[index];
                let item = group.item.clone();
                let old_count = group.members.len();
                let candidates: Vec<_> = (0..group.targets.len())
                    .filter(|&candidate| !form.automatically_selected(index, candidate))
                    .collect();
                let new_count = candidates.len();

                egui::ScrollArea::vertical()
                    .id_salt(("migration-detail", index))
                    .min_scrolled_height(0.)
                    .auto_shrink([false, false])
                    .show(&mut detail, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.colored_label(
                                MUTED,
                                bajptr_project::model::title(&text(&item, "table")),
                            );
                            if new_count > 1 {
                                ui.label(format!("{new_count} 个候选"));
                            }
                        });
                        ui.add_space(6.);
                        comparison(ui, &text(&item, "original"), &text(&item, "replacement"));
                        ui.add_space(10.);
                        ui.separator();
                        if let Some(reason) = match item["kind"].as_str() {
                            Some("occupied") => Some("对应位置已有新版修改。"),
                            Some("missing_table") => Some("新版底稿中找不到原文本表。"),
                            Some("unsupported_table") => Some("原文本表无法自动核对。"),
                            Some("unavailable") => Some("未找到候选文本。"),
                            _ => None,
                        } {
                            ui.colored_label(ORANGE, reason);
                        }
                        let mut search = |ui: &mut egui::Ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.add_enabled(!busy, egui::TextEdit::singleline(&mut form.keyword)
                                    .hint_text("输入新版原文中的文字").desired_width(205.));
                                let available = !busy && !form.keyword.trim().is_empty();
                                if secondary(ui, available, "查找新版文本").clicked() {
                                    let item = form.groups[index].members[0];
                                    self.submit("migration_find", json!({"nonce":form.plan["nonce"],"revision":form.plan["revision"],"item":item,"keyword":form.keyword.trim()}), "正在查找新版文本…");
                                }
                            });
                            if !form.search_message.is_empty() {
                                ui.colored_label(MUTED, &form.search_message);
                            }
                        };
                        if new_count == 0 {
                            search(ui);
                        } else {
                            ui.collapsing("查找其他位置", search);
                        }
                        if old_count > 1 && new_count > 0 {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(format!("最多选择 {old_count} 处新版文本。"));
                                if form.groups[index].can_select_all()
                                    && candidates.iter().all(|&candidate| !form.reserved(index, candidate))
                                {
                                    let mut all = form.groups[index].chosen.len() == new_count;
                                    if ui.add_enabled(!busy, egui::Checkbox::new(&mut all, "全选本组")).changed() {
                                        form.toggle_context(index, &candidates, all);
                                    }
                                }
                            });
                        }
                        let page = &mut form.groups[index].candidate_page;
                        let page_count = new_count.div_ceil(20);
                        if page_count > 1 {
                            ui.horizontal(|ui| {
                                if secondary(ui, *page > 0, "上一页").clicked() { *page -= 1; }
                                ui.label(format!("第 {} / {page_count} 页", *page + 1));
                                if secondary(ui, *page + 1 < page_count, "下一页").clicked() { *page += 1; }
                            });
                        }
                        let start = (*page).min(page_count.saturating_sub(1)) * 20;
                        for &candidate in candidates.iter().skip(start).take(20) {
                                let entry = &item["candidates"][candidate];
                                let row = &entry["row"];
                                let proposal = &item["proposals"][candidate];
                                let reserved = form.reserved(index, candidate);
                                let mut checked = form.groups[index].chosen.contains(&candidate);
                                let selectable = !busy && !reserved
                                    && (checked || old_count == 1 || form.groups[index].chosen.len() < old_count);
                                let old_location = row[0] == item["table"] && row[1] == item["row_id"] && row[2] == item["slot"];
                                let label = format!("{} · 记录 {} · 字段 {}{}", bajptr_project::model::title(row[0].as_str().unwrap_or_default()),
                                    row[1].as_str().map(str::to_owned).unwrap_or_else(|| row[1].to_string()), row[2],
                                    if old_location { " · 原位置" } else { "" });
                                egui::Frame::new().fill(BG).corner_radius(6).inner_margin(10).show(ui, |ui| {
                                    ui.set_min_width(ui.available_width());
                                    ui.push_id(candidate, |ui| {
                                        if ui.add_enabled(selectable, egui::Checkbox::new(&mut checked, &label)).changed() {
                                            form.toggle_context(index, &[candidate], checked);
                                        }
                                        ui.add(egui::Label::new(row[3].as_str().unwrap_or_default()).wrap().selectable(true));
                                        if reserved {
                                            ui.colored_label(ORANGE, "已分配给另一组修改。");
                                        }
                                        if !form.groups[index].chosen.contains(&candidate) {
                                            return;
                                        }
                                        let group = &mut form.groups[index];
                                        let mine = proposal["mine"]["result"].as_str();
                                        let game = proposal["game"]["result"].as_str();
                                        if let (Some(mine), Some(game)) = (mine, game) {
                                            ui.columns(2, |columns| {
                                                for (column, (button, value, other)) in columns.iter_mut().zip([
                                                    ("用我的改法", mine, game),
                                                    ("用新版改法", game, mine),
                                                ]) {
                                                    if secondary(column, !busy, button).clicked() {
                                                        group.edits.insert(candidate, value.to_owned());
                                                    }
                                                    conflict_text(column, value, other);
                                                }
                                            });
                                        }
                                        let default = if proposal.is_null() {
                                            item["replacement"].as_str()
                                        } else {
                                            proposal["replacement"].as_str()
                                        };
                                        if let Some(result) = group.edits.get_mut(&candidate) {
                                            ui.colored_label(MUTED, "最终文字");
                                            ui.add_enabled(!busy, egui::TextEdit::multiline(result)
                                                .desired_rows(2).desired_width(ui.available_width()));
                                            let batch_retained = proposal["batch_old"].as_str().is_some_and(|old| {
                                                default == Some(result.as_str()) ||
                                                    ["mine", "game"].into_iter().any(|name| {
                                                        proposal[name]["result"].as_str() == Some(result.as_str())
                                                            && proposal[name]["input"].as_str().is_some_and(|input| input.contains(old))
                                                    })
                                            });
                                            if proposal["batch_old"].is_string() && !batch_retained {
                                                ui.colored_label(MUTED, "保存为单条修改");
                                            }
                                        } else {
                                            if let Some(result) = default {
                                                ui.colored_label(MUTED, "最终文字");
                                                ui.add(egui::Label::new(RichText::new(result).color(BLUE)).wrap().selectable(true));
                                                let reason = text(proposal, "reason");
                                                if !reason.is_empty() {
                                                    ui.colored_label(ORANGE, reason);
                                                    if secondary(ui, !busy, "采用结果").clicked() {
                                                        group.edits.insert(candidate, result.to_owned());
                                                    }
                                                }
                                            }
                                            if !proposal.is_null() && secondary(ui, !busy, "编辑结果").clicked() {
                                                let initial = default.or(game).or_else(|| row[3].as_str()).unwrap_or_default();
                                                group.edits.insert(candidate, initial.to_owned());
                                            }
                                        }
                                    });
                                });
                                ui.add_space(5.);
                        }
                    });
            } else {
                detail.vertical_centered(|ui| {
                    ui.add_space(6.);
                    if !form.filter.is_empty() {
                        ui.label("没有匹配的文本。");
                    } else if form.automatic_count() == 0 {
                        ui.label("没有需要核对的修改。");
                    }
                });
            }
            ui.advance_cursor_after_rect(area);
        });
        form.open = open || self.write_busy();
        self.migration_form = Some(form);
    }
}
