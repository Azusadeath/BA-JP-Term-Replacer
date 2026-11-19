use crate::{app::*, state::*};
use eframe::egui::{self, Color32};
use serde_json::{Value, json};
use std::path::PathBuf;

impl Desktop {
    pub fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            self.navigation(ui);
            if self.loaded && self.game.trim().is_empty() && self.tab != 2 {
                ui.separator();
                ui.colored_label(ORANGE, "尚未配置游戏目录");
            }
        });
        self.recovery_notices(ui);
    }
    pub fn recovery_notices(&mut self, ui: &mut egui::Ui) {
        if let Some(warning) = self.settings_warning.clone() {
            egui::Frame::new()
                .fill(Color32::from_rgb(255, 244, 218))
                .corner_radius(8)
                .inner_margin(10)
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.colored_label(ORANGE, "设置读取失败，当前使用默认设置");
                        if secondary(ui, true, "关闭警告").clicked() {
                            self.settings_warning = None;
                        }
                    });
                    ui.add(egui::Label::new(warning).wrap());
                    ui.label("请重新选择游戏目录并保存设置；原设置文件保留到保存时。");
                });
        }
    }
    fn directory_controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("游戏目录").size(14.).color(MUTED));
            let reserve = action_width(ui, "选择文件夹")
                + action_width(ui, "保存")
                + ui.spacing().item_spacing.x * 2.;
            let width = (ui.available_width() - reserve).max(180.);
            let response = ui
                .add_enabled_ui(self.loaded && !self.write_busy(), |ui| {
                    ui.add_sized(
                        [width, 36.],
                        egui::TextEdit::singleline(&mut self.game)
                            .font(egui::TextStyle::Body)
                            .margin(egui::Margin::symmetric(10, 8))
                            .desired_width(width),
                    )
                })
                .inner;
            if response.changed() {
                self.game_path_changed();
            }
            if secondary(ui, self.loaded && !self.write_busy(), "选择文件夹").clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .set_title("选择日服游戏安装目录")
                    .pick_folder()
            {
                self.game = path.to_string_lossy().into();
                self.game_path_changed();
            }
            if secondary(
                ui,
                self.loaded
                    && self.path_dirty
                    && !self.write_busy()
                    && !self.game.trim().is_empty(),
                "保存",
            )
            .clicked()
            {
                self.save_settings();
            }
        });
    }
    fn check_button(&mut self, ui: &mut egui::Ui) {
        if secondary(
            ui,
            self.loaded && !self.path_dirty && !self.write_busy() && !self.game.trim().is_empty(),
            "检查底稿",
        )
        .clicked()
        {
            self.submit("check", json!({}), "正在检查游戏资源与底稿…");
        }
    }
    fn navigation(&mut self, ui: &mut egui::Ui) {
        for (index, title) in ["替换", "查找", "工具"].iter().enumerate() {
            if action(
                ui,
                true,
                title,
                if self.tab == index {
                    ButtonKind::Soft
                } else {
                    ButtonKind::Quiet
                },
            )
            .clicked()
            {
                if self.tab != index {
                    self.search_more_wheel = SearchMoreWheel::default();
                }
                self.tab = index;
                self.restore_context_status();
            }
        }
    }
    pub fn replacement(&mut self, ui: &mut egui::Ui) {
        let state = self.state.clone();
        let rules = slice(&state, "rules");
        let batches = slice(&state, "batches");
        let display_rows = replacement_rows(&state);
        let selected = self
            .rule_selection
            .and_then(|index| display_rows.get(index));
        let selected_rule = match selected {
            Some(ReplacementRow::Rule(index)) => Some(*index),
            _ => None,
        };
        let selected_batch = self
            .rule_selection
            .and_then(|i| display_rows.get(i))
            .and_then(|row| match row {
                ReplacementRow::Batch(index) => batches.get(*index),
                _ => None,
            });
        let selected_combined = match selected {
            Some(ReplacementRow::Combined { rules: indices, .. }) => rules.get(indices[0]),
            _ => None,
        };
        let can_edit = self.loaded && self.healthy() && !self.write_busy();
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(can_edit, ToolButton::new("添加词条"))
                .clicked()
            {
                self.rule_form = Some(RuleForm::new(None, self.revision(), None));
            }
            if ui
                .add_enabled(
                    can_edit && self.state["can_undo"] == true,
                    ToolButton::new("撤销"),
                )
                .clicked()
            {
                self.submit("history", json!({"undo":true}), "正在撤销…");
            }
            if ui
                .add_enabled(
                    can_edit && self.state["can_redo"] == true,
                    ToolButton::new("重做"),
                )
                .clicked()
            {
                self.submit("history", json!({"undo":false}), "正在重做…");
            }
            ui.separator();
            if ui
                .add_enabled(
                    can_edit
                        && (selected_combined.is_some() || selected_rule.is_some()
                            || selected_batch.is_some_and(|entry| entry["active"] == true)),
                    ToolButton::new("编辑"),
                )
                .clicked()
            {
                if let Some(entry) = selected_combined {
                    self.open_combined(&entry["rule"]);
                } else if let Some(i) = selected_rule {
                    self.rule_form = Some(RuleForm::new(Some(i), self.revision(), rules.get(i)));
                } else if let Some(entry) = selected_batch {
                    self.open_batch(entry);
                }
            }
            if ui
                .add_enabled(
                    can_edit && (selected_combined.is_some() || selected_rule.is_some() || selected_batch.is_some()),
                    ToolButton::new("移除"),
                )
                .clicked()
            {
                if let Some(entry) = selected_combined {
                    self.submit(
                        "combined_remove",
                        json!({"revision":self.revision(),"old":entry["rule"]["old"],"new":entry["rule"]["new"]}),
                        "正在移除整表与指定文本替换…",
                    );
                } else if let Some(i) = selected_rule {
                    self.submit(
                        "remove_rule",
                        json!({"index":i,"revision":self.revision()}),
                        "正在移除词条…",
                    );
                } else if let Some(entry) = selected_batch {
                    self.submit(
                        if entry["id"].is_string() {
                            "remove_batch"
                        } else {
                            "remove_edit"
                        },
                        json!({"id":entry["id"],"index":entry["index"],"revision":self.revision()}),
                        "正在移除批量修改…",
                    );
                }
                self.rule_selection = None;
            }
            ui.separator();
            if ui
                .add_enabled(
                    can_edit,
                    ToolButton::new("单条修改"),
                )
                .clicked()
            {
                self.show_edits = true;
            }
            let can_promote = can_edit && self.connected && self.sources_ready
                && !self.path_dirty && self.busy.is_none()
                && slice(&self.state, "edits").iter().any(|edit| edit["active"] == true);
            if ui.add_enabled(can_promote,ToolButton::new("整理批量修改"))
                .clicked() {
                self.submit("promotion_start",json!({"revision":self.revision()}),"正在核对并整理批量修改…");
            }
        });
        ui.add_space(6.);
        egui::Panel::bottom("replace-actions")
            .frame(egui::Frame::new().inner_margin(egui::Margin {
                top: 14,
                ..Default::default()
            }))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let ready = self.connected
                            && self.sources_ready
                            && self.healthy()
                            && self.busy.is_none();
                        if primary(ui, ready, "预览并应用").clicked() {
                            self.submit("preview", json!({}), "正在比较本次最终变化…");
                        }
                    });
                });
            });
        let retained = slice(&self.promotion_result, "items");
        if !retained.is_empty() {
            let attention: u64 = retained.iter().map(|item| count(item, "attention")).sum();
            let retained_count = retained.len();
            let direct = retained_count == 1 && attention > 0;
            let item = retained[0]["review_item"].clone();
            let ready = can_edit
                && self.connected
                && self.sources_ready
                && !self.path_dirty
                && self.busy.is_none();
            ui.horizontal_wrapped(|ui| {
                if attention > 0 {
                    ui.colored_label(ORANGE, format!("新版有 {attention} 段匹配文本可供核对。"));
                } else {
                    ui.colored_label(MUTED, format!("保留 {retained_count} 个指定范围。"));
                }
                let label = if direct {
                    "查看遗漏并补改…"
                } else {
                    "查看整理结果…"
                };
                if ui.add_enabled(ready, egui::Button::new(label)).clicked() {
                    self.promotion_error.clear();
                    if direct {
                        self.preview_promotion_missing(item);
                    } else {
                        self.show_promotion_result = true;
                    }
                }
            });
            ui.add_space(6.);
        }
        let width = ui.available_width() - 12.;
        let widths = [(width - 350.) * 0.5, (width - 350.) * 0.5, 110., 240.];
        table_header(ui, &["原文字", "替换为", "匹配方式", "生效范围"], &widths);
        egui::Frame::new()
            .fill(Color32::WHITE)
            .corner_radius(10)
            .stroke(egui::Stroke::new(1., LINE))
            .show(ui, |ui| {
                if rules.is_empty() && batches.is_empty() {
                    ui.allocate_ui_with_layout(
                        ui.available_size(),
                        egui::Layout::centered_and_justified(egui::Direction::TopDown),
                        |ui| {
                            let message = if !self.loaded {
                                if self.busy.is_some() {
                                    "正在读取词表，请稍候…"
                                } else {
                                    "词表尚未读取，请重新连接后重试。"
                                }
                            } else if !self.healthy() {
                                "记录文件读取失败，请修复损坏文件后重新打开工具。"
                            } else if self.write_busy() {
                                "正在处理当前任务，完成后可添加词条或恢复备份。"
                            } else {
                                "点击「添加词条」开始，或恢复已有备份。"
                            };
                            ui.colored_label(MUTED, message);
                        },
                    );
                } else {
                    let mut scroll = egui::ScrollArea::vertical()
                        .id_salt("replacement-rows")
                        .auto_shrink([false, false]);
                    if let Some(index) = self.reveal_rule.take() {
                        scroll = scroll.vertical_scroll_offset(
                            index as f32 * (ROW_HEIGHT + ui.spacing().item_spacing.y),
                        );
                    }
                    scroll.show_rows(ui, ROW_HEIGHT, display_rows.len(), |ui, range| {
                        for i in range {
                            if let ReplacementRow::Combined {
                                rules: indices,
                                batches: batch_indices,
                            } = &display_rows[i]
                            {
                                let rule = &rules[indices[0]]["rule"];
                                let tables = indices
                                    .iter()
                                    .flat_map(|&index| {
                                        bajptr_project::model::scopes(&text(
                                            &rules[index]["rule"],
                                            "table",
                                        ))
                                        .ok()
                                        .flatten()
                                        .unwrap_or_default()
                                    })
                                    .collect::<std::collections::BTreeSet<_>>();
                                let members = batch_indices
                                    .iter()
                                    .map(|&index| count(&batches[index], "count"))
                                    .sum::<u64>();
                                let values = [
                                    text(rule, "old"),
                                    if text(rule, "new").is_empty() {
                                        "（删除匹配文字）".into()
                                    } else {
                                        text(rule, "new")
                                    },
                                    "包含文字".into(),
                                    format!("整表 {} · 指定文本 {} 条", tables.len(), members),
                                ];
                                let response = data_row(
                                    ui,
                                    i,
                                    self.rule_selection == Some(i),
                                    &values,
                                    &widths,
                                );
                                if response.clicked() {
                                    self.rule_selection = Some(i);
                                }
                                if response.double_clicked() && can_edit {
                                    self.open_combined(rule);
                                }
                                continue;
                            }
                            if let ReplacementRow::Batch(index) = display_rows[i] {
                                let entry = &batches[index];
                                let replacement = text(entry, "new");
                                let grouped = entry["id"].is_string();
                                let values = [
                                    text(entry, "old").replace(['\r', '\n'], " "),
                                    if replacement.is_empty() {
                                        if grouped {
                                            "（删除匹配文字）"
                                        } else {
                                            "（清空文本）"
                                        }
                                        .into()
                                    } else {
                                        replacement.replace(['\r', '\n'], " ")
                                    },
                                    if grouped {
                                        "包含文字"
                                    } else {
                                        "整段相同"
                                    }
                                    .into(),
                                    if entry["active"] == true {
                                        "批量修改"
                                    } else {
                                        "批量修改（版本失效）"
                                    }
                                    .into(),
                                ];
                                let response = data_row(
                                    ui,
                                    i,
                                    self.rule_selection == Some(i),
                                    &values,
                                    &widths,
                                );
                                if response.clicked() {
                                    self.rule_selection = Some(i);
                                }
                                if response.double_clicked() && can_edit && entry["active"] == true
                                {
                                    self.open_batch(entry);
                                }
                                continue;
                            }
                            let ReplacementRow::Rule(index) = display_rows[i] else {
                                unreachable!()
                            };
                            let row = &rules[index];
                            let new = text(row, "display_new");
                            let values = [
                                text(row, "display_old"),
                                if new.is_empty() {
                                    "（删除匹配文字）".into()
                                } else {
                                    new
                                },
                                if !row["draft"].is_null() {
                                    mode_name("re").into()
                                } else {
                                    mode_name(&text(&row["rule"], "mode")).into()
                                },
                                text(row, "scope"),
                            ];
                            let response =
                                data_row(ui, i, self.rule_selection == Some(i), &values, &widths);
                            if response.clicked() {
                                self.rule_selection = Some(i);
                            }
                            if response.double_clicked() && can_edit {
                                self.rule_form =
                                    Some(RuleForm::new(Some(index), self.revision(), Some(row)));
                            }
                        }
                    });
                }
            });
    }
    pub fn search_page(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing.y = 6.;
        let wide = ui.available_width() >= 960.;
        let source_label = self.search_source_label();
        let source_width = (ui
            .painter()
            .layout_no_wrap(
                source_label.clone(),
                egui::TextStyle::Button.resolve(ui.style()),
                TEXT,
            )
            .size()
            .x
            + 42.)
            .clamp(145., 220.);
        let mut run = false;
        ui.horizontal(|ui| {
            let reserved = action_width(ui, "查找")
                + ui.spacing().item_spacing.x
                + if wide {
                    source_width + ui.spacing().item_spacing.x
                } else {
                    0.
                };
            let id = egui::Id::unique("search-keyword");
            let focused = ui.memory(|m| m.has_focus(id));
            let enabled = self.connected
                && self.busy.is_none()
                && !self.modal()
                && !self.keyword.trim().is_empty();
            let enter = ui.input(|i| self.search_input.submit(&i.events, focused, enabled));
            let previous_keyword = self.keyword.clone();
            let input = ui.add_sized(
                [(ui.available_width() - reserved).max(160.), 36.],
                egui::TextEdit::singleline(&mut self.keyword)
                    .id(id)
                    .return_key(None)
                    .font(egui::TextStyle::Body)
                    .hint_text("输入要查找的文字"),
            );
            if input.changed() && self.keyword != previous_keyword {
                self.search_input_changed();
            }
            run |= primary(
                ui,
                self.connected && self.busy.is_none() && !self.keyword.trim().is_empty(),
                "查找",
            )
            .clicked()
                || enter;
            if wide {
                run |= self.search_source_control(ui, &source_label, source_width);
            }
        });
        if !wide {
            ui.horizontal(|ui| {
                run |= self.search_source_control(ui, &source_label, source_width);
            });
        }
        if run && self.busy.is_none() {
            self.selection.clear();
            self.submit(
                "search",
                json!({"keyword":self.keyword.trim(),"source":self.search_source,"offset":0}),
                "正在查找…",
            );
        }
        let search = self.search.clone();
        let rows = slice(&search, "rows");
        let has_more = !rows.is_empty() && rows.len() < count(&search, "total") as usize;
        let can_load = has_more && self.connected && self.busy.is_none() && !self.modal();
        let mut load_more = false;
        let mut wheel_bottom = None;
        let toolbar_position = ui.cursor().min + egui::vec2(8., 0.);
        if !rows.is_empty() {
            ui.add_space(44.);
        }
        let width = ui.available_width() - 12.;
        let widths = [width - 180., 180.];
        table_header(ui, &["本次文本", "文本表"], &widths);
        let table_height = ui.available_height();
        egui::Frame::new()
            .fill(Color32::WHITE)
            .corner_radius(10)
            .stroke(egui::Stroke::new(1., LINE))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.set_min_height(table_height);
                if rows.is_empty() {
                    ui.allocate_ui_with_layout(
                        ui.available_size(),
                        egui::Layout::centered_and_justified(egui::Direction::TopDown),
                        |ui| {
                            if !self.search.is_null() {
                                ui.colored_label(MUTED, "未找到匹配文本");
                            } else {
                                ui.colored_label(MUTED, "输入关键词查找游戏文本");
                            }
                        },
                    );
                } else {
                    let list = egui::ScrollArea::vertical()
                        .id_salt(("search-rows", text(&self.search, "nonce")))
                        .auto_shrink([false, false])
                        .max_height(ui.available_height().max(80.))
                        .show_rows(
                            ui,
                            ROW_HEIGHT,
                            rows.len() + usize::from(has_more),
                            |ui, range| {
                                for i in range {
                                    if has_more && i == rows.len() {
                                        ui.allocate_ui_with_layout(
                                            egui::vec2(ui.available_width(), ROW_HEIGHT),
                                            egui::Layout::left_to_right(egui::Align::Center),
                                            |ui| {
                                                let inset = (ui.available_width()
                                                    - action_width(ui, "加载更多"))
                                                    * 0.5;
                                                ui.add_space(inset.max(0.));
                                                load_more |=
                                                    secondary(ui, can_load, "加载更多").clicked();
                                            },
                                        );
                                        continue;
                                    }
                                    let row = &rows[i];
                                    let values = [
                                        text(row, "text").replace(['\r', '\n'], " "),
                                        text(row, "label"),
                                    ];
                                    let response = data_row(
                                        ui,
                                        i,
                                        self.selection.rows.contains(&i),
                                        &values,
                                        &widths,
                                    );
                                    if response.clicked() {
                                        let modifiers = ui.input(|i| i.modifiers);
                                        self.selection.click(i, modifiers.ctrl, modifiers.shift);
                                    }
                                    if response.double_clicked()
                                        && self.connected
                                        && self.busy.is_none()
                                    {
                                        self.open_text(i);
                                    }
                                }
                            },
                        );
                    let end = (list.content_size.y - list.inner_rect.height()).max(0.);
                    if can_load
                        && self.busy.is_none()
                        && !self.modal()
                        && !self.show_logs
                        && !egui::Popup::is_any_open(ui.ctx())
                        && ui.is_enabled()
                        && ui.rect_contains_pointer(list.inner_rect)
                        && list.state.clamped_offset().y >= end - 1.
                    {
                        wheel_bottom = Some((list.id, rows.len()));
                    }
                }
            });
        let wheel_load = ui.input(|input| self.search_more_wheel.update(wheel_bottom, input));
        if (load_more || wheel_load) && self.busy.is_none() && !self.modal() {
            self.search_more_wheel = SearchMoreWheel::default();
            self.submit(
                "search_more",
                json!({
                    "keyword":text(&search,"keyword"),
                    "source":text(&search,"source"),
                    "search_nonce":text(&search,"nonce"),
                    "offset":rows.len()
                }),
                "正在加载更多搜索结果…",
            );
        }
        if !rows.is_empty()
            && !self.selection.rows.is_empty()
            && !self.modal()
            && !self.show_logs
            && !egui::Popup::is_any_open(ui.ctx())
            && ui.is_enabled()
        {
            let selected = self.selection.rows.len();
            let labels: &[&str] = if selected == 1 {
                &["编辑文本", "替换选中文本", "用此表添加词条", "文本表详情"]
            } else {
                &["替换选中文本"]
            };
            let toolbar_width = labels
                .iter()
                .map(|label| action_width(ui, label))
                .sum::<f32>()
                + ui.painter()
                    .layout_no_wrap(
                        format!("已选 {selected} 条"),
                        egui::FontId::proportional(14.),
                        BLUE,
                    )
                    .size()
                    .x
                + ui.spacing().item_spacing.x * labels.len() as f32
                + 8.;
            egui::Area::new(egui::Id::unique("search-selection-toolbar"))
                .default_width(toolbar_width)
                .order(egui::Order::Foreground).fixed_pos(toolbar_position).movable(false)
                .show(ui.ctx(), |ui| {
                    egui::Frame::new().fill(Color32::WHITE).stroke(egui::Stroke::new(1., LINE))
                        .corner_radius(8).inner_margin(4).shadow(egui::epaint::Shadow {
                            offset: [0, 2], blur: 8, spread: 0, color: Color32::from_black_alpha(12)
                        }).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let selected = self.selection.rows.len();
                                if selected == 1 && secondary(ui, self.connected && self.busy.is_none(), "编辑文本").clicked() {
                                    self.open_text(*self.selection.rows.first().unwrap());
                                }
                                if secondary(ui, self.connected && self.busy.is_none(), "替换选中文本").clicked() {
                                    self.batch_form = Some(BatchForm { group_id: String::new(), indices: self.selection.rows.iter().copied().collect(), old: text(&self.search, "keyword"), new: String::new(), nonce: String::new(), changes: Vec::new(), error: String::new() });
                                }
                                if selected == 1 {
                                    let row = &rows[*self.selection.rows.first().unwrap()];
                                    if secondary(ui, !self.write_busy(), "用此表添加词条").clicked() {
                                        let keyword = text(&self.search, "keyword");
                                        let value = json!({"rule":{"old":keyword,"new":keyword,"mode":"sub","source":"-","table":row["table"]}});
                                        self.rule_form = Some(RuleForm::new(None, self.revision(), Some(&value)));
                                    }
                                    if secondary(ui, true, "文本表详情").clicked() {
                                        self.table_detail = Some(self.tables.iter().find(|v|v["name"]==row["table"]).cloned().unwrap_or(json!({"name":row["table"],"label":row["table"]})));
                                    }
                                }
                                ui.label(egui::RichText::new(format!("已选 {selected} 条")).size(14.).color(MUTED));
                            });
                        });
                });
        }
    }
    fn search_source_label(&self) -> String {
        if self.search_source.is_empty() {
            "全部文本表".into()
        } else {
            self.tables
                .iter()
                .chain(slice(&self.search, "source_tables"))
                .find(|v| text(v, "name") == self.search_source)
                .map(|v| text(v, "label"))
                .unwrap_or_else(|| self.search_source.clone())
        }
    }
    fn search_source_control(&mut self, ui: &mut egui::Ui, selected: &str, width: f32) -> bool {
        let mut changed = false;
        ui.add_enabled_ui(self.busy.is_none(), |ui| {
            ui.set_max_width(width);
            egui::ComboBox::from_id_salt("search-source")
                .width(width)
                .truncate()
                .selected_text(selected)
                .show_ui(ui, |ui| {
                    let sources = search_sources(&self.tables, &self.search, &self.keyword);
                    let font = egui::TextStyle::Button.resolve(ui.style());
                    let longest = std::iter::once("全部文本表".to_string())
                        .chain(sources.iter().map(|table| text(table, "label")))
                        .map(|label| {
                            ui.painter()
                                .layout_no_wrap(label, font.clone(), TEXT)
                                .size()
                                .x
                        })
                        .fold(0., f32::max);
                    let menu_width = (longest + ui.spacing().button_padding.x * 2. + 16.)
                        .min((ui.ctx().content_rect().width() - 48.).max(width));
                    ui.set_width(menu_width);
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    changed |= ui
                        .selectable_value(&mut self.search_source, String::new(), "全部文本表")
                        .changed();
                    for table in sources {
                        changed |= ui
                            .selectable_value(
                                &mut self.search_source,
                                text(table, "name"),
                                text(table, "label"),
                            )
                            .changed();
                    }
                });
        });
        changed && self.connected && !self.keyword.trim().is_empty()
    }
    fn export_text_button(&mut self, ui: &mut egui::Ui) {
        if ui
            .add_enabled(
                self.connected && self.busy.is_none(),
                ToolButton::new("导出底稿文本"),
            )
            .clicked()
            && let Some(path) = rfd::FileDialog::new()
                .set_title("选择导出文件夹")
                .pick_folder()
        {
            self.submit("export_text", json!({"path":path}), "正在导出底稿文本…");
        }
    }
    pub fn tools_page(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .id_salt("tools-page")
            .auto_shrink([false, true])
            .show(ui, |ui| {
                tool_section(ui, "游戏与底稿", |ui| {
                    self.directory_controls(ui);
                    ui.horizontal_wrapped(|ui| {
                        ui.colored_label(MUTED, "初始底稿");
                        let verified = !self.baseline.is_null();
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(14., 20.), egui::Sense::hover());
                        ui.painter().circle_filled(
                            rect.center(),
                            5.,
                            if verified {
                                Color32::from_rgb(37, 147, 91)
                            } else {
                                Color32::from_rgb(160, 169, 181)
                            },
                        );
                        if verified && secondary(ui, true, "打开底稿位置").clicked() {
                            Self::open_folder(&PathBuf::from(text(&self.baseline, "folder")));
                        }
                        self.check_button(ui);
                        if !self.baseline.is_null()
                            && secondary(ui, self.connected && self.busy.is_none(), "恢复初始底稿")
                                .clicked()
                        {
                            self.confirm_restore = true;
                        }
                    });
                    if !self.baseline.is_null() {
                        egui::CollapsingHeader::new("底稿详情").show(ui, |ui| {
                            egui::Grid::new("baseline-metadata")
                                .num_columns(2)
                                .spacing(egui::vec2(20., 6.))
                                .show(ui, |ui| {
                                    for (label, key) in [
                                        ("资源版本", "version"),
                                        ("建立时间", "created"),
                                        ("底稿位置", "folder"),
                                        ("资源标识", "resource"),
                                        ("版本标识", "version_id"),
                                    ] {
                                        let value = text(&self.baseline, key);
                                        if !value.is_empty() {
                                            ui.colored_label(MUTED, label);
                                            ui.add(egui::Label::new(value).wrap().selectable(true));
                                            ui.end_row();
                                        }
                                    }
                                });
                        });
                    }
                    if self.migration_count > 0
                        && secondary(ui, self.connected && self.busy.is_none(), "迁移旧版修改…")
                            .clicked()
                    {
                        self.open_migration();
                    }
                });
                ui.add_space(4.);
                tool_section(ui, "工作区", |ui| {
                    let can_edit = self.loaded && self.healthy() && !self.write_busy();
                    ui.horizontal(|ui| {
                        ui.colored_label(MUTED, "来源表排序");
                        let saved = self.state["replacement_sort"].as_str().unwrap_or("added");
                        let mut selected = saved.to_string();
                        ui.add_enabled_ui(self.loaded && self.busy.is_none(), |ui| {
                            egui::ComboBox::from_id_salt("replacement-sort")
                                .selected_text(match selected.as_str() {
                                    "alphabetical" => "按首字母",
                                    "modified" => "按修改时间",
                                    _ => "按添加时间",
                                })
                                .show_ui(ui, |ui| {
                                    for (mode, label) in [
                                        ("alphabetical", "按首字母"),
                                        ("modified", "按修改时间"),
                                        ("added", "按添加时间"),
                                    ] {
                                        ui.selectable_value(&mut selected, mode.to_string(), label);
                                    }
                                });
                        });
                        if selected != saved {
                            self.submit(
                                "sort_settings",
                                json!({"revision":self.revision(),"value":selected}),
                                "正在保存排序…",
                            );
                        }
                    });
                    let response = egui::Frame::new()
                        .fill(Color32::WHITE)
                        .stroke(egui::Stroke::new(1., LINE))
                        .corner_radius(7)
                        .inner_margin(egui::Margin::symmetric(10, 0))
                        .show(ui, |ui| {
                            ui.spacing_mut().icon_width = 20.;
                            ui.spacing_mut().icon_width_inner = 12.;
                            ui.add_enabled(
                                can_edit && !self.path_dirty,
                                egui::Checkbox::new(
                                    &mut self.skip,
                                    egui::RichText::new("跳过含日文假名的文本").size(14.5),
                                ),
                            )
                        })
                        .inner;
                    if response.changed() {
                        self.search = Value::Null.into();
                        self.selection.clear();
                        self.status = "跳过假名选项已更新，请重新查找".into();
                        self.save_settings();
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.colored_label(MUTED, "备份");
                        if secondary(ui, can_edit && !self.path_dirty, "导出").clicked()
                            && let Some(path) = rfd::FileDialog::new()
                                .add_filter("BA-JPTR 备份", &["json"])
                                .set_file_name("BA-JPTR备份.json")
                                .save_file()
                        {
                            self.submit(
                                "backup_create",
                                json!({"path":path,"revision":self.revision()}),
                                "正在保存备份…",
                            );
                        }
                        if secondary(ui, self.loaded && !self.write_busy(), "恢复").clicked()
                            && let Some(path) = rfd::FileDialog::new()
                                .add_filter("BA-JPTR 备份", &["json"])
                                .pick_file()
                        {
                            self.submit(
                                "backup_read",
                                json!({"path":path,"revision":self.revision()}),
                                "正在读取备份…",
                            );
                        }
                    });
                });
                ui.add_space(4.);
                tool_section(ui, "文件输出", |ui| {
                    ui.horizontal_wrapped(|ui| {
                        self.export_text_button(ui);
                        if secondary(ui, true, "打开输出目录").clicked() {
                            match std::fs::create_dir_all(&self.output_dir) {
                                Ok(()) => Self::open_folder(&self.output_dir),
                                Err(error) => {
                                    let output = self.output_dir.clone();
                                    self.report_file_error("open_directory", &output, error.into());
                                }
                            }
                        }
                    });
                });
                if action(ui, true, "关于 BA-JPTR", ButtonKind::Quiet).clicked() {
                    self.show_about = true;
                }
            });
    }
}
