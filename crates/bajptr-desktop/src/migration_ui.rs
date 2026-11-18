use crate::{app::*, dialogs::window, state::*};
use eframe::egui::{self, RichText};
use serde_json::json;

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
        if self.migration_notice {
            let mut open = true;
            let mut close = false;
            window("迁移旧版修改", ctx, 520.)
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label("检测到旧版单条或批量修改，是否迁移到当前底稿？");
                    ui.add_space(8.);
                    ui.horizontal(|ui| {
                        if primary(ui, self.busy.is_none(), "开始迁移").clicked() {
                            self.submit(
                                "migration_start",
                                json!({"revision":self.revision()}),
                                "正在核对旧版修改…",
                            );
                            close = true;
                        }
                        if ui.button("暂不迁移").clicked() {
                            close = true;
                        }
                    });
                });
            if !open || close {
                self.migration_notice = false;
            }
        }
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
            let total = slice(&form.plan, "items").len();
            let confirmed = form.confirmed_count();
            let pending = form.groups.iter().filter(|g| !g.confirmed).count();
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("{} 段 · {} 组修改", total, form.groups.len()));
                ui.colored_label(MUTED, format!("已确认 {} 段 · 未保存", confirmed));
            });
            ui.horizontal(|ui| {
                ui.selectable_value(
                    &mut form.show_confirmed,
                    false,
                    format!("待核对 {pending} 组"),
                );
                ui.selectable_value(
                    &mut form.show_confirmed,
                    true,
                    format!("已确认 {} 组", form.groups.len() - pending),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut form.filter)
                        .hint_text("搜索文本或文本表")
                        .desired_width(ui.available_width()),
                );
            });
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
                        if let Some(index) = form.selected {
                            if form.groups[index].confirmed {
                                if secondary(ui, !busy, "重新核对这组").clicked() {
                                    form.groups[index].confirmed = false;
                                    form.show_confirmed = false;
                                }
                            } else {
                                let amount = if form.groups[index].equal_count() {
                                    form.groups[index].members.len()
                                } else {
                                    form.groups[index].chosen.len()
                                };
                                if secondary(
                                    ui,
                                    !busy && form.can_confirm(index),
                                    &format!("确认本组（{amount} 处）"),
                                )
                                .clicked()
                                {
                                    form.confirm(index);
                                }
                                if secondary(ui, !busy && visible_count > 1, "暂时跳过").clicked()
                                {
                                    form.next();
                                }
                            }
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if primary(
                                ui,
                                !busy && confirmed > 0,
                                &format!("保存已确认（{confirmed} 段）"),
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
                    ui.add_space(3.);
                    ui.colored_label(MUTED, "保存时自动备份，可撤销。");
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
                    .id_salt(("migration-groups", form.show_confirmed, &form.filter))
                    .min_scrolled_height(0.)
                    .auto_shrink([false, false])
                    .show_rows(&mut list_ui, 60., visible.len(), |ui, range| {
                        for position in range {
                            let index = visible[position];
                            let group = &form.groups[index];
                            let caption = if group.confirmed {
                                format!("已确认 {} 处", group.chosen.len())
                            } else {
                                format!(
                                    "原 {} 处 → 新 {} 处",
                                    group.members.len(),
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
                                line.add(
                                    egui::Label::new(label)
                                        .selectable(false)
                                        .truncate()
                                        .show_tooltip_when_elided(false),
                                );
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
                let new_count = group.targets.len();
                let equal = group.equal_count();
                let competing = form.has_competing(index);
                let is_confirmed = group.confirmed;

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
                            ui.label(format!("原 {old_count} 处 → 新 {new_count} 处"));
                        });
                        ui.add_space(6.);
                        comparison(ui, &text(&item, "original"), &text(&item, "replacement"));
                        ui.add_space(10.);
                        ui.separator();
                        if competing {
                            ui.colored_label(ORANGE, "这段新版文本也出现在其他组的候选中。");
                        }
                        if let Some(reason) = match item["kind"].as_str() {
                            Some("occupied") => Some("对应位置已有新版修改。"),
                            Some("missing_table") => Some("新版底稿中找不到原文本表。"),
                            Some("unsupported_table") => Some("原文本表无法自动核对。"),
                            Some("unavailable") => Some("未找到候选文本。"),
                            _ => None,
                        } {
                            ui.colored_label(ORANGE, reason);
                        }
                        if !is_confirmed {
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
                        }
                        if new_count > 0 && !equal {
                            ui.label(format!("最多选择 {old_count} 处新版文本。"));
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
                        let start = *page * 20;
                        for candidate in start..new_count.min(start + 20) {
                                let entry = &item["candidates"][candidate];
                                let row = &entry["row"];
                                let context = text(entry, "context");
                                let proposal = &item["proposals"][candidate];
                                let reserved = form.reserved(index, candidate);
                                let mut checked = form.groups[index].chosen.contains(&candidate);
                                let selectable = !busy && !is_confirmed && !reserved
                                    && (checked || form.groups[index].chosen.len() < old_count);
                                let label = format!("{} · 记录 {} · 字段 {}", bajptr_project::model::title(row[0].as_str().unwrap_or_default()),
                                    row[1].as_str().map(str::to_owned).unwrap_or_else(|| row[1].to_string()), row[2]);
                                egui::Frame::new().fill(BG).corner_radius(6).inner_margin(10).show(ui, |ui| {
                                    ui.set_min_width(ui.available_width());
                                    ui.push_id(candidate, |ui| {
                                        if equal {
                                            ui.colored_label(MUTED, &label);
                                        } else if ui.add_enabled(selectable, egui::Checkbox::new(&mut checked, &label)).changed() {
                                            form.toggle_context(index, &[candidate], checked);
                                        }
                                        ui.add(egui::Label::new(row[3].as_str().unwrap_or_default()).wrap().selectable(true));
                                        if !context.is_empty() {
                                            ui.add(egui::Label::new(RichText::new(context).color(MUTED)).wrap().selectable(true));
                                        }
                                        if reserved {
                                            ui.colored_label(ORANGE, "已分配给另一组修改。");
                                        }
                                        if proposal.is_null() { return; }
                                        let reason = text(proposal, "reason");
                                        if !reason.is_empty() { ui.add(egui::Label::new(RichText::new(reason).color(ORANGE)).wrap()); }
                                        let group = &mut form.groups[index];
                                        if checked && proposal["editable"] == true {
                                            let mut input = group.edits.get(&candidate).cloned().unwrap_or_else(|| text(proposal, "input"));
                                            if ui.add_enabled(!busy && !is_confirmed, egui::TextEdit::multiline(&mut input)
                                                .desired_rows(2).desired_width(ui.available_width()).hint_text("输入迁移后的文字")).changed() {
                                                group.edits.insert(candidate, input);
                                            }
                                            if proposal["input"].is_null() && !group.edits.contains_key(&candidate)
                                                && secondary(ui, !busy && !is_confirmed, "使用空文本").clicked() {
                                                group.edits.insert(candidate, String::new());
                                            }
                                        }
                                        let replaced = if let Some(input) = group.edits.get(&candidate) {
                                            if let Some(old) = proposal["batch_old"].as_str() {
                                                input.contains(old).then(|| input.replace(old, &text(proposal, "batch_new")))
                                            } else { Some(input.clone()) }
                                        } else { proposal["replacement"].as_str().map(str::to_owned) };
                                        if let Some(result) = replaced {
                                            ui.colored_label(MUTED, "迁移后文本");
                                            ui.add(egui::Label::new(RichText::new(if result.is_empty() { "（空文本）".into() } else { result }).color(BLUE)).wrap().selectable(true));
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
                    } else if !form.show_confirmed && pending == 0 {
                        ui.heading("核对完成");
                        ui.label(format!("{confirmed} 段修改等待保存"));
                    } else {
                        ui.label("尚未确认任何修改。");
                    }
                });
            }
            ui.advance_cursor_after_rect(area);
        });
        form.open = open || self.write_busy();
        self.migration_form = Some(form);
    }
}
