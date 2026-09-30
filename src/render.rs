use crate::App;
use crate::parser::FileChange;
use crate::restore::{RestoreAction, RestoreJob};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, Paragraph, Wrap},
};

pub fn render_file_list(f: &mut Frame, area: Rect, app: &mut App) {
    let available_width = area.width.saturating_sub(4) as usize; // Account for borders and padding

    // Get current items based on search mode
    let current_items = app.get_current_file_tree_items();

    let items: Vec<ListItem> = current_items
        .iter()
        .enumerate()
        .map(|(i, tree_item)| {
            let is_selected = i == app.selected_index;
            let bg_style = if is_selected {
                Style::default().bg(app.theme.colors.tree_selected_bg.0)
            } else {
                Style::default()
            };

            // Build tree structure with styled spans
            let mut spans = Vec::new();

            // Build tree prefix using diffnav-style logic
            let mut tree_parts = Vec::new();

            // Add vertical lines for ancestor levels
            // For each ancestor level, show │ if that ancestor is NOT the last child
            // diffnav uses 2 characters per level
            for i in 0..tree_item.depth {
                if i < tree_item.parent_is_last.len() {
                    if tree_item.parent_is_last[i] {
                        tree_parts.push("  "); // Ancestor was last child, no vertical line (2 spaces)
                    } else {
                        tree_parts.push("│ "); // Ancestor has siblings below, show vertical line + space
                    }
                } else {
                    tree_parts.push("  "); // Default to 2 spaces
                }
            }

            // Add connector for current level (with 1 space padding like diffnav)
            if tree_item.depth > 0 {
                if tree_item.is_last_child {
                    tree_parts.push("╰ "); // Final branch connector + space
                } else {
                    tree_parts.push("├ "); // Branch connector + space
                }
            }

            let tree_prefix = tree_parts.join("");

            // Add tree prefix with tree line color
            if !tree_prefix.is_empty() {
                spans.push(Span::styled(
                    tree_prefix.clone(),
                    Style::default().fg(app.theme.colors.tree_line.0),
                ));
            }

            // Add checkbox for files (not directories)
            if !tree_item.is_directory {
                let is_checked = app.checked_files.contains(&tree_item.full_path);
                let checkbox_char = if is_checked { '☑' } else { '☐' };
                let checkbox_style = if is_selected {
                    Style::default().fg(app.theme.colors.tree_selected_fg.0)
                } else {
                    Style::default().fg(app.theme.colors.text_primary.0)
                };
                spans.push(Span::styled(format!("{checkbox_char} "), checkbox_style));
            }

            // Get icon based on item type
            let icon = if tree_item.is_directory {
                crate::icons::get_directory_icon(tree_item.is_expanded)
            } else {
                // File - use file_diff icon or default
                tree_item
                    .file_diff
                    .as_ref()
                    .map(|fd| fd.get_file_icon())
                    .unwrap_or(crate::icons::get_file_icon(""))
            };

            // Apply color to directory icon
            if tree_item.is_directory {
                let icon_style = if is_selected {
                    Style::default().fg(app.theme.colors.tree_selected_fg.0)
                } else {
                    Style::default().fg(app.theme.colors.tree_directory.0)
                };
                spans.push(Span::styled(format!("{icon} "), icon_style));
            } else {
                spans.push(Span::raw(format!("{icon} ")));
            }

            // Add file/directory name with appropriate color
            let name_style = if is_selected {
                Style::default().fg(app.theme.colors.tree_selected_fg.0)
            } else if tree_item.is_directory {
                Style::default().fg(app.theme.colors.tree_directory.0)
            } else {
                // Check if file is checked to dim the color
                let is_checked = app.checked_files.contains(&tree_item.full_path);
                if is_checked {
                    // Dim the file color for checked files
                    Style::default()
                        .fg(app.theme.colors.tree_file.0)
                        .add_modifier(ratatui::style::Modifier::DIM)
                } else {
                    Style::default().fg(app.theme.colors.tree_file.0)
                }
            };

            // Add stats for files or collapsed directories, with an arrow replacing the line
            // counts when files are marked to be copied
            let restore_indicator = app.restore_indicator(tree_item);
            let line_counts = |added: usize, removed: usize| {
                [
                    Span::styled(
                        format!("+{added} "),
                        Style::default().fg(app.theme.colors.status_added.0),
                    ),
                    Span::styled(
                        format!("-{removed}"),
                        Style::default().fg(app.theme.colors.status_removed.0),
                    ),
                ]
            };
            let mut stats = Vec::new();
            if tree_item.is_directory && !tree_item.is_expanded && tree_item.dir_file_count > 0 {
                // Show directory statistics when collapsed, with the number of marked files.
                // Compared files have no line counts.
                if let Some(indicator) = &restore_indicator {
                    stats.push(Span::styled(
                        indicator.marked.to_string(),
                        Style::default().fg(app.theme.colors.status_modified.0),
                    ));
                    stats.push(Span::raw("/"));
                }
                stats.push(Span::raw(format!("{} files ", tree_item.dir_file_count)));
                if restore_indicator.is_none() && !app.is_path_compare() {
                    stats.extend(line_counts(
                        tree_item.dir_added_lines,
                        tree_item.dir_removed_lines,
                    ));
                }
            } else if let Some(file_diff) = &tree_item.file_diff {
                if restore_indicator.is_none() {
                    match file_diff.change {
                        Some(change) => stats.push(change_span(change, app)),
                        None => stats
                            .extend(line_counts(file_diff.added_lines, file_diff.removed_lines)),
                    }
                }
            }
            if let Some(indicator) = restore_indicator {
                stats.push(Span::styled(
                    indicator.arrow.to_string(),
                    Style::default().fg(restore_color(indicator.action, app)),
                ));
            }

            // Calculate available space for the name
            let tree_prefix_width = tree_prefix.chars().count();
            let checkbox_width = if !tree_item.is_directory { 2 } else { 0 }; // Checkbox + space for files only
            let icon_width = 2; // Icon + space
            let stats_width: usize = stats.iter().map(|span| span.content.chars().count()).sum();
            // Leave a space between the name and any stats
            let used_width = tree_prefix_width
                + checkbox_width
                + icon_width
                + stats_width
                + usize::from(stats_width > 0);
            let available_name_width = available_width.saturating_sub(used_width);

            // Truncate name if too long
            let display_name = if tree_item.name.chars().count() > available_name_width
                && available_name_width > 3
            {
                let truncated_width = available_name_width.saturating_sub(3);
                let truncated: String = tree_item.name.chars().take(truncated_width).collect();
                format!("{truncated}...")
            } else {
                tree_item.name.clone()
            };

            spans.push(Span::styled(display_name.clone(), name_style));

            if !stats.is_empty() {
                let current_width = tree_prefix.chars().count() +
                                   checkbox_width + // checkbox width (0 for directories, 2 for files)
                                   2 + // icon width
                                   display_name.chars().count();
                // Keep at least one space between the name and the stats
                if current_width + stats_width < available_width {
                    let padding = available_width - current_width - stats_width;
                    spans.push(Span::raw(" ".repeat(padding)));
                    spans.extend(stats);
                }
            }

            ListItem::new(Line::from(spans)).style(bg_style)
        })
        .collect();

    // Create title based on search mode
    let title = if app.search_mode {
        if app.search_query.is_empty() {
            format!(
                " Search Mode - Type to filter ({} items)",
                current_items.len()
            )
        } else {
            format!(
                " Search: '{}' ({} items)",
                app.search_query,
                current_items.len()
            )
        }
    } else {
        format!(" Files & Directories ({} items)", current_items.len())
    };

    let file_list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .style(Style::default().fg(app.theme.colors.border.0)),
        )
        .style(Style::default().fg(app.theme.colors.text_primary.0));

    f.render_stateful_widget(file_list, area, &mut app.file_list_state);
}

pub fn render_diff_content(f: &mut Frame, area: Rect, app: &mut App) {
    // Clamp scroll values before rendering
    app.clamp_scroll(area.height, area.width);

    // Check if we need to refresh diff with current width for side-by-side display
    // Use actual diff area width for maximum utilization
    if !matches!(
        app.config.get_diff_command_type(),
        crate::config::DiffCommandType::GitDefault
    ) && should_refresh_diff_width(app, area.width)
    {
        // Pass both terminal width and actual area width for flexible template calculation
        if let Ok((terminal_width, _)) = crossterm::terminal::size() {
            app.refresh_diff_with_area_width(area.width, terminal_width);
        } else {
            app.refresh_diff_with_width(area.width);
        }
    }

    let text_content = app.diff_cache().text.clone();

    let diff_content = Paragraph::new(text_content)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(
                    "Diff Content (using {}) - [h/l: scroll, j/k: files, g/G: jump{}]",
                    app.config.get_diff_display_name(),
                    if app.restore_targets.is_some() {
                        ", r/R: mark restore ←/→, P: preview, X: run"
                    } else {
                        ""
                    }
                ))
                .style(Style::default().fg(app.theme.colors.border.0)),
        )
        .scroll((app.vertical_scroll, app.horizontal_scroll))
        .wrap(Wrap { trim: false });

    f.render_widget(diff_content, area);
}

/// Check if we should refresh the diff with new width
fn should_refresh_diff_width(_app: &App, current_width: u16) -> bool {
    // Only refresh if width has changed significantly (by more than 5 characters)
    // to avoid constant re-rendering
    static mut LAST_WIDTH: u16 = 0;
    unsafe {
        if LAST_WIDTH == 0 || (current_width as i16 - LAST_WIDTH as i16).abs() > 5 {
            LAST_WIDTH = current_width;
            true
        } else {
            false
        }
    }
}

pub fn render_status_line(f: &mut Frame, area: Rect, app: &App) {
    let current_items = app.get_current_file_tree_items();
    let status_spans = if let Some(tree_item) = current_items.get(app.selected_index) {
        let mut spans = Vec::new();

        if tree_item.is_directory {
            spans.push(Span::raw(" : "));
            spans.push(Span::styled(
                tree_item.full_path.clone(),
                Style::default().fg(app.theme.colors.tree_directory.0),
            ));
            spans.push(Span::raw(" | Directory | "));
        } else if let Some(file_diff) = &tree_item.file_diff {
            spans.push(Span::raw(format!(" {}: ", file_diff.get_file_icon())));
            spans.push(Span::styled(
                tree_item.full_path.clone(),
                Style::default().fg(app.theme.colors.tree_file.0),
            ));
            spans.push(Span::raw(" | "));

            // Add colored diff stats, or how a compared file changed, or where it will be copied
            let stats_string = if let Some(mark) = app.restore_marks.get(&tree_item.full_path) {
                spans.push(Span::styled(
                    format!("{} {}", mark.direction.arrow(), mark.description()),
                    Style::default().fg(restore_color(mark.action, app)),
                ));
                String::new()
            } else if let Some(change) = file_diff.change {
                spans.push(change_span(change, app));
                String::new()
            } else {
                file_diff.diff_stats()
            };
            let stats_parts: Vec<&str> = stats_string.split_whitespace().collect();
            for (i, part) in stats_parts.iter().enumerate() {
                if part.starts_with('+') {
                    spans.push(Span::styled(
                        part.to_string(),
                        Style::default().fg(app.theme.colors.status_added.0),
                    ));
                } else if part.starts_with('-') {
                    spans.push(Span::styled(
                        part.to_string(),
                        Style::default().fg(app.theme.colors.status_removed.0),
                    ));
                } else {
                    spans.push(Span::raw(part.to_string()));
                }
                if i < stats_parts.len() - 1 {
                    spans.push(Span::raw(" "));
                }
            }
            spans.push(Span::raw(" | "));
        } else {
            spans.push(Span::raw(format!(
                " : {} | No diff | ",
                tree_item.full_path
            )));
        }

        spans.push(Span::raw(format!(
            "Scroll: {},{}",
            app.vertical_scroll, app.horizontal_scroll
        )));
        spans
    } else {
        vec![Span::raw(" No item selected")]
    };
    let status_spans = match &app.status_message {
        Some(message) => vec![Span::styled(
            format!(" {message}"),
            Style::default().fg(app.theme.colors.status_modified.0),
        )],
        None => status_spans,
    };

    let status = Paragraph::new(Line::from(status_spans))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Status")
                .style(Style::default().fg(app.theme.colors.border_focused.0)),
        )
        .style(Style::default().fg(app.theme.colors.status_bar_fg.0))
        .wrap(Wrap { trim: false });

    f.render_widget(status, area);
}

pub fn render_search_box(f: &mut Frame, area: Rect, app: &App) {
    let (search_text, title) = if app.search_input_mode {
        // Currently typing in search
        let text = if app.search_query.is_empty() {
            "Filter files 󰬛 ".to_string()
        } else {
            format!("󰬛 {}", app.search_query)
        };
        (text, " Search (/: search, Enter: confirm, ESC: exit)")
    } else {
        // Search confirmed, showing filtered results
        let text = if app.search_query.is_empty() {
            "󰬛 All files".to_string()
        } else {
            format!("󰬛 Filtered: '{}'", app.search_query)
        };
        (text, " Search Results (/: new search, ESC: exit)")
    };

    let search_style = if app.search_query.is_empty() && app.search_input_mode {
        Style::default()
            .fg(app.theme.colors.text_primary.0)
            .add_modifier(ratatui::style::Modifier::DIM)
    } else {
        Style::default().fg(app.theme.colors.text_primary.0)
    };

    let border_style = if app.search_input_mode {
        Style::default().fg(app.theme.colors.border_focused.0)
    } else {
        Style::default().fg(app.theme.colors.border.0)
    };

    let search_box = Paragraph::new(search_text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .style(border_style),
        )
        .style(search_style);

    f.render_widget(search_box, area);
}

/// Colored label for how a compared file changed
fn change_span(change: FileChange, app: &App) -> Span<'static> {
    let color = match change {
        FileChange::Added => app.theme.colors.status_added.0,
        FileChange::Deleted => app.theme.colors.status_removed.0,
        FileChange::Modified => app.theme.colors.status_modified.0,
    };
    Span::styled(change.label(), Style::default().fg(color))
}

/// Red for deletions, yellow for copies
fn restore_color(action: RestoreAction, app: &App) -> Color {
    match action {
        RestoreAction::Copy => app.theme.colors.status_modified.0,
        RestoreAction::Delete => app.theme.colors.status_removed.0,
    }
}

/// Area of a given size centered in another, shrunk to fit
fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

pub fn render_popup(f: &mut Frame, app: &mut App) {
    let Some(popup) = &mut app.popup else {
        return;
    };
    let full = f.area();
    let area = centered_rect(full, full.width * 9 / 10, full.height * 8 / 10);

    let text: Vec<Line> = popup
        .lines
        .iter()
        .map(|line| Line::raw(line.as_str()))
        .collect();
    let paragraph = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(popup.title.as_str())
                .style(Style::default().fg(app.theme.colors.border_focused.0)),
        )
        .style(Style::default().fg(app.theme.colors.text_primary.0))
        .wrap(Wrap { trim: false });

    // Stop scrolling once the last line is at the bottom
    let line_count = u16::try_from(paragraph.line_count(area.width)).unwrap_or(u16::MAX);
    popup.max_scroll = line_count.saturating_sub(area.height);
    popup.scroll = popup.scroll.min(popup.max_scroll);
    let paragraph = paragraph.scroll((popup.scroll, 0));

    f.render_widget(Clear, area);
    f.render_widget(paragraph, area);
}

pub fn render_restore_progress(f: &mut Frame, job: &RestoreJob, app: &App) {
    let full = f.area();
    let area = centered_rect(full, full.width * 6 / 10, 3);
    let (completed, total) = job.progress();
    let title = if job.is_cancelled() {
        " Restoring files - cancelling... "
    } else {
        " Restoring files (Esc: cancel) "
    };
    let gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .style(Style::default().fg(app.theme.colors.border_focused.0)),
        )
        .gauge_style(Style::default().fg(app.theme.colors.status_modified.0))
        .ratio(if total == 0 {
            1.0
        } else {
            completed as f64 / total as f64
        })
        .label(format!("{completed} / {total}"));

    f.render_widget(Clear, area);
    f.render_widget(gauge, area);
}
