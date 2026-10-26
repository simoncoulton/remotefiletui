//! Rendering. Pure functions of `App` state; the only mutation is writing
//! layout facts (page size, table scroll offset) back into the panes.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Cell, Clear, Gauge, List, ListItem, Paragraph, Row, Table, Wrap},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    app::{
        App, BrowserState, ConnectState, Direction, FilterFocus, Focus, FolderChoice, LogLevel,
        Modal, Pane, SPINNER, Side,
    },
    backend::{Entry, EntryKind, Protocol},
    config::{COLORS, Filter, Profile, TreeRow},
    theme::Theme,
    util::{format_rate, format_size, format_time, sanitize, sanitize_text},
};

const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 20;
/// Height of the activity log panel, borders included.
const LOG_ROWS: u16 = 7;

fn key_hint(th: &Theme, key: &str, label: &str) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            key.to_string(),
            Style::new().fg(th.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {label}  "), th.dim_style()),
    ]
}

/// A line of key hints with each key in the accent colour, like the footer.
fn hints_line(th: &Theme, items: &[(&str, &str)]) -> Line<'static> {
    Line::from(
        items
            .iter()
            .flat_map(|(k, l)| key_hint(th, k, l))
            .collect::<Vec<_>>(),
    )
}

/// Clear `rect` and repaint the theme's background (plain `Clear` would reset it).
fn clear(frame: &mut Frame, rect: Rect, th: &Theme) {
    frame.render_widget(Clear, rect);
    frame.render_widget(Block::new().style(th.base()), rect);
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let th = app.theme.clone();
    frame.render_widget(Block::new().style(th.base()), area);
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        let msg = format!(
            "Terminal too small\n{}x{} — need at least {MIN_WIDTH}x{MIN_HEIGHT}",
            area.width, area.height
        );
        let y = area.height.saturating_sub(2) / 2;
        frame.render_widget(
            Paragraph::new(msg).alignment(Alignment::Center),
            Rect {
                y: area.y + y,
                height: area.height.min(2),
                ..area
            },
        );
        return;
    }

    let tick = app.tick;
    // Keep the help scroll position within the real content height.
    let in_browser = app.browser.is_some();
    if let Some(Modal::Help { scroll }) = &mut app.modal {
        let lines = help_entries(in_browser).len() as u16;
        let visible = (lines + 2).min(area.height).saturating_sub(2);
        *scroll = (*scroll).min(lines.saturating_sub(visible));
    }
    let filter = Filter::new(&app.form.filter_text, app.form.filter_tags.iter());
    let rows = app.config.tree(&app.form.collapsed, &filter);
    let editing = app.editing().cloned();
    let modified = app.form_modified();

    // The log takes what is left over once the connect form (16 rows) fits.
    let spare = area.height.saturating_sub(3 + 16 + 1);
    let log_h = if spare >= 3 { spare.min(LOG_ROWS) } else { 0 };
    app.log_visible = log_h > 0;
    if log_h == 0 && app.log_focus {
        app.log_focus = false; // the terminal shrank under it
    }
    let [head, body, log_area, foot] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(log_h),
        Constraint::Length(1),
    ])
    .areas(area);
    draw_header(frame, head, app, editing.as_ref(), modified);

    let hints = if let Some(browser) = app.browser.as_mut() {
        draw_browser(frame, body, browser, tick, &th, app.log_focus)
    } else {
        draw_connect(
            frame,
            body,
            &th,
            &app.form,
            SavedView {
                rows: &rows,
                config: &app.config,
                editing: editing.as_ref(),
                modified,
                any_selected: app.selection().0,
                profile_selected: app.selection().1,
            },
        )
    };
    if log_h > 0 {
        draw_log(frame, log_area, app);
    }
    let hints = if app.log_focus {
        vec![
            ("j/k", "move"),
            ("gg/G", "top/bottom"),
            ("Tab", "next"),
            ("Esc", "back"),
            ("?", "help"),
            ("q", "quit"),
        ]
    } else {
        hints
    };
    footer(frame, &th, foot, app.notice.as_ref(), &hints);
    draw_modal(frame, area, app);
}

/// The activity log above the footer: connections, navigation and operations,
/// newest at the bottom. J / K (PageUp / PageDown while typing) scroll it.
fn draw_log(frame: &mut Frame, area: Rect, app: &mut App) {
    let inner_h = area.height.saturating_sub(2) as usize;
    let len = app.log.len();
    let max_scroll = len.saturating_sub(inner_h);
    app.log_scroll = app.log_scroll.min(max_scroll);
    let focused = app.log_focus && len > 0;
    if focused {
        // Keep the highlighted line inside the window.
        app.log_sel = app.log_sel.min(len - 1);
        let end = len - app.log_scroll;
        let start = end.saturating_sub(inner_h);
        if app.log_sel < start {
            app.log_scroll = len - (app.log_sel + inner_h).min(len);
        } else if app.log_sel >= end {
            app.log_scroll = len - 1 - app.log_sel;
        }
    }
    let th = &app.theme;
    let end = len - app.log_scroll;
    let start = end.saturating_sub(inner_h);
    let width = area.width.saturating_sub(2) as usize;

    let lines: Vec<Line> = app
        .log
        .iter()
        .enumerate()
        .skip(start)
        .take(end - start)
        .map(|(i, e)| {
            let color = match e.level {
                LogLevel::Info => th.fg,
                LogLevel::Ok => th.success,
                LogLevel::Warn => th.warning,
                LogLevel::Error => th.error,
            };
            let text = ellipsize_right(&sanitize(&e.text), width.saturating_sub(e.time.len() + 1));
            let line = Line::from(vec![
                Span::styled(format!("{} ", e.time), th.dim_style()),
                Span::styled(text, Style::new().fg(color)),
            ]);
            if focused && i == app.log_sel {
                // Whole row highlighted, text colours kept.
                let pad = width.saturating_sub(line.width());
                let mut spans = line.spans;
                spans.push(Span::raw(" ".repeat(pad)));
                Line::from(spans).patch_style(th.selection())
            } else {
                line
            }
        })
        .collect();
    let title = if focused {
        format!(" Log · {}/{len} ", app.log_sel + 1)
    } else if app.log_scroll > 0 {
        format!(" Log · {} newer (J) ", app.log_scroll)
    } else if max_scroll > 0 {
        " Log · K scrolls back ".to_string()
    } else {
        " Log ".to_string()
    };
    let (border, title_style) = if app.log_focus {
        (Style::new().fg(th.accent), Style::new().fg(th.accent))
    } else {
        (th.dim_style(), th.dim_style())
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(Span::styled(title, title_style));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Top bar: what is connected (or being edited), at a glance.
fn draw_header(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    editing: Option<&Profile>,
    modified: bool,
) {
    let th = &app.theme;
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(th.dim_style())
        .title(Line::styled(
            " remotefiletui ",
            Style::new().fg(th.accent).add_modifier(Modifier::BOLD),
        ));
    let mut left: Vec<Span> = Vec::new();
    let mut right: Vec<Span> = Vec::new();

    if let Some(b) = &app.browser {
        let dot = b
            .color
            .as_deref()
            .and_then(|c| th.conn_color(c))
            .unwrap_or(th.success);
        left.push(Span::styled("● ", Style::new().fg(dot)));
        left.push(Span::styled(
            sanitize(&b.name),
            Style::new().add_modifier(Modifier::BOLD),
        ));
        left.push(Span::styled(
            format!("  {}", sanitize(&b.detail)),
            th.dim_style(),
        ));
        let count = |p: &Pane| p.view.iter().filter(|e| !e.is_parent_marker()).count();
        right.push(Span::styled(
            format!("remote {}  local {}", count(&b.remote), count(&b.local)),
            th.dim_style(),
        ));
        block = block.title_bottom(
            Line::styled(
                format!(" {} ", ellipsize_left(&sanitize(&b.remote.cwd), 60)),
                th.dim_style(),
            )
            .right_aligned(),
        );
    } else {
        let browsing = app.form.focus == Focus::Profiles;
        match editing {
            _ if browsing && app.selected_profile().is_some() => {
                let p = app.selected_profile().expect("checked above");
                left.push(Span::styled(
                    sanitize(&p.name),
                    Style::new().add_modifier(Modifier::BOLD),
                ));
                left.push(Span::styled(
                    format!("  {}", sanitize(&profile_detail(p))),
                    th.dim_style(),
                ));
            }
            Some(p) => {
                left.push(Span::styled("✎ Editing ", Style::new().fg(th.accent)));
                left.push(Span::styled(
                    sanitize(&p.name),
                    Style::new().add_modifier(Modifier::BOLD),
                ));
                if modified {
                    left.push(Span::styled(" (modified)", Style::new().fg(th.warning)));
                }
            }
            None => left.push(Span::styled(
                "New connection",
                Style::new().add_modifier(Modifier::BOLD),
            )),
        }
        let detail = if browsing && app.selected_profile().is_some() {
            String::new()
        } else {
            form_detail(&app.form)
        };
        if !detail.is_empty() {
            left.push(Span::styled(
                format!("  {}", sanitize(&detail)),
                th.dim_style(),
            ));
        }
        // On the top border, so it never squeezes the connection details.
        let mut title = vec![Span::raw(" ")];
        match app.config.password_manager {
            Some(m) => {
                title.push(Span::styled("Password Manager: ", th.dim_style()));
                title.push(Span::styled(
                    m.label(),
                    Style::new().fg(th.accent).add_modifier(Modifier::BOLD),
                ));
            }
            None => title.push(Span::styled(
                "⚠ No password manager: passwords aren't saved (Ctrl-P)",
                Style::new().fg(th.warning),
            )),
        }
        title.push(Span::raw(" "));
        block = block.title(Line::from(title).right_aligned());
        if !app.config_hint.is_empty() {
            block = block.title_bottom(
                Line::styled(format!(" {} ", app.config_hint), th.dim_style()).right_aligned(),
            );
        }
    }

    let inner = block.inner(area);
    frame.render_widget(block, area);
    let right_w = right.iter().map(|s| s.content.width()).sum::<usize>() as u16;
    let [l, r] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(right_w + 1)]).areas(inner);
    frame.render_widget(Paragraph::new(Line::from(left)), l);
    frame.render_widget(
        Paragraph::new(Line::from(right)).alignment(Alignment::Right),
        r,
    );
}

/// `SFTP  user@host:22  /path` for a saved connection.
fn profile_detail(p: &Profile) -> String {
    let scheme = p.protocol.scheme().to_uppercase();
    let who = if p.username.is_empty() {
        String::new()
    } else {
        format!("{}@", p.username)
    };
    let mut out = if p.protocol == Protocol::S3 {
        let mut parts = vec![scheme];
        parts.extend(p.region.clone());
        if !p.host.is_empty() {
            parts.push(p.host.clone());
        }
        parts.extend(p.bucket.clone());
        parts.join("  ")
    } else {
        format!("{scheme}  {who}{}:{}", p.host, p.port)
    };
    if let Some(path) = &p.path {
        out.push_str(&format!("  {path}"));
    }
    if !p.tags.is_empty() {
        out.push_str(&format!("  Tags: {}", p.tags.join(" ")));
    }
    out
}

/// `SFTP  user@host:22` from what is typed in the form.
fn form_detail(form: &ConnectState) -> String {
    let v = form.values();
    let scheme = v.protocol.scheme().to_uppercase();
    let host = v.host.trim();
    if v.protocol == Protocol::S3 {
        let mut parts = vec![scheme];
        for part in [v.region.trim(), host, v.bucket.trim()] {
            if !part.is_empty() {
                parts.push(part.to_string());
            }
        }
        return parts.join("  ");
    }
    if host.is_empty() {
        return String::new();
    }
    let who = match v.user.trim() {
        "" => String::new(),
        u => format!("{u}@"),
    };
    let port = v.port.trim();
    let port = if port.is_empty() {
        String::new()
    } else {
        format!(":{port}")
    };
    format!("{scheme}  {who}{host}{port}")
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// Truncate with a trailing ellipsis.
fn ellipsize_right(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 1; // the ellipsis
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > max {
            break;
        }
        w += cw;
        out.push(c);
    }
    out.push('…');
    out
}

/// Keep the end of a long path (the interesting part), eliding the start.
fn ellipsize_left(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let mut out: Vec<char> = Vec::new();
    let mut w = 1; // the ellipsis
    for c in s.chars().rev() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > max {
            break;
        }
        w += cw;
        out.push(c);
    }
    out.push('…');
    out.iter().rev().collect()
}

/// One row: the status message if there is one, else the key hints. Hints are
/// ranked by their order; those that don't fit are dropped from the end.
fn footer(
    frame: &mut Frame,
    th: &Theme,
    area: Rect,
    notice: Option<&crate::app::Notice>,
    hints: &[(&str, &str)],
) {
    let line = match notice {
        Some(n) => {
            let color = if n.is_error { th.warning } else { th.success };
            Line::styled(sanitize(&n.text), Style::new().fg(color))
        }
        None => {
            // The last two hints (help, quit) always stay; the others are
            // dropped from the end when the width runs out.
            let width = |(k, l): &(&str, &str)| k.width() + l.width() + 3;
            let split = hints.len().saturating_sub(2);
            let (head, tail) = hints.split_at(split);
            let mut room = (area.width as usize + 2).saturating_sub(tail.iter().map(width).sum());
            let mut spans = Vec::new();
            for h in head {
                if width(h) > room {
                    break;
                }
                room -= width(h);
                spans.extend(key_hint(th, h.0, h.1));
            }
            for (k, l) in tail {
                spans.extend(key_hint(th, k, l));
            }
            Line::from(spans)
        }
    };
    frame.render_widget(Paragraph::new(line), area);
}

// ───────────────────────────── connect screen ─────────────────────────────

const LABEL_W: u16 = 14;

/// What the connect screen shows besides the form itself.
struct SavedView<'a> {
    rows: &'a [TreeRow],
    config: &'a crate::config::Config,
    /// The saved connection whose values are in the form, and whether they differ.
    editing: Option<&'a Profile>,
    modified: bool,
    /// A row is selected in the list / it is a connection (not a folder).
    any_selected: bool,
    profile_selected: bool,
}

fn draw_connect(
    frame: &mut Frame,
    area: Rect,
    th: &Theme,
    form: &ConnectState,
    saved: SavedView,
) -> Vec<(&'static str, &'static str)> {
    let SavedView {
        rows: rows_tree,
        config,
        editing,
        modified,
        any_selected,
        profile_selected,
    } = saved;
    let body = area;
    let [left, right] =
        Layout::horizontal([Constraint::Length(40), Constraint::Min(30)]).areas(body);

    // Saved connections
    let focused = form.focus == Focus::Profiles;
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(if focused {
            Style::new().fg(th.accent)
        } else {
            th.dim_style()
        })
        .title(if !form.has_filter() {
            " [S]aved Connections ".to_string()
        } else {
            format!(
                " [S]aved · {} ",
                ellipsize_right(&sanitize(&form.filter_summary()), 24)
            )
        });
    let list_w = left.width.saturating_sub(2) as usize;
    let items: Vec<ListItem> = rows_tree
        .iter()
        .map(|row| match row {
            TreeRow::Folder {
                name,
                depth,
                count,
                collapsed,
            } => {
                let suffix = format!(" ({count})");
                let indent = "  ".repeat(*depth);
                let room = list_w.saturating_sub(2 + indent.len() + suffix.width());
                let leaf = crate::config::Config::folder_leaf(name);
                ListItem::new(Line::from(vec![
                    Span::raw(indent),
                    Span::styled(
                        if *collapsed { "▸ " } else { "▾ " },
                        Style::new().fg(th.accent),
                    ),
                    Span::styled(
                        ellipsize_right(&sanitize(leaf), room),
                        Style::new().fg(th.accent).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(suffix, th.dim_style()),
                ]))
            }
            TreeRow::Profile { index, depth } => {
                let p = &config.profiles[*index];
                let indent = "  ".repeat(*depth);
                // Colour dot (or a blank of the same width so names stay aligned).
                let dot = match p.color.as_deref().and_then(|c| th.conn_color(c)) {
                    Some(c) => Span::styled("● ", Style::new().fg(c)),
                    None => Span::raw("  "),
                };
                let name =
                    ellipsize_right(&sanitize(&p.name), list_w.saturating_sub(indent.len() + 2));
                ListItem::new(Line::from(vec![Span::raw(indent), dot, Span::raw(name)]))
            }
        })
        .collect();
    if items.is_empty() {
        let text = if !form.has_filter() {
            "No saved connections.\n\nFill in the form and\npress Ctrl-S to save it.\n\nPress f (in this list)\nto create a folder.".to_string()
        } else {
            format!(
                "Nothing matches\n{}\n\nEsc clears the filter.",
                ellipsize_right(&sanitize(&form.filter_summary()), 30)
            )
        };
        frame.render_widget(
            Paragraph::new(text).style(th.dim_style()).block(block),
            left,
        );
    } else {
        let highlight = if focused {
            th.selection()
        } else {
            Style::new().add_modifier(Modifier::BOLD)
        };
        let mut state = form.profiles;
        frame.render_stateful_widget(
            List::new(items).block(block).highlight_style(highlight),
            left,
            &mut state,
        );
    }

    // Form
    let in_form = form.focus != Focus::Profiles;
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(if in_form {
            Style::new().fg(th.accent)
        } else {
            th.dim_style()
        })
        .title(match editing {
            Some(p) => format!(
                " [C]onnections · editing {}{} ",
                ellipsize_right(&sanitize(&p.name), 28),
                if modified { " *" } else { "" }
            ),
            None => " [C]onnections ".to_string(),
        });
    let inner = block.inner(right);
    frame.render_widget(block, right);

    // Row layout: 0 name, 1 protocol, 2 blank, 3..=9 fields, 10 saved-entry info,
    // 11..=12 note, 11..=13 validation error (replacing the note).
    let rows = Layout::vertical([Constraint::Length(1); 14]).split(inner);
    let secret_label = if form.key.value().trim().is_empty() {
        "Password"
    } else {
        "Passphrase"
    };

    let text_field = |frame: &mut Frame,
                      row: usize,
                      label: &str,
                      input: &crate::input::TextInput,
                      masked: bool,
                      focus: Focus,
                      placeholder: &str| {
        let r = rows[row];
        draw_field_label(frame, th, r, label, form.focus == focus);
        let field = Rect {
            x: r.x + LABEL_W,
            width: r.width.saturating_sub(LABEL_W + 1),
            ..r
        };
        let (shown, col) = input.view(field.width, masked);
        let (text, style) = if shown.is_empty() && form.focus != focus {
            (placeholder.to_string(), th.dim_style())
        } else {
            (shown, Style::new())
        };
        frame.render_widget(
            Paragraph::new(text).style(style.add_modifier(Modifier::UNDERLINED)),
            field,
        );
        if form.focus == focus {
            frame.set_cursor_position((field.x + col, field.y));
        }
    };

    // Protocol selector is drawn separately (not a text field).
    {
        let r = rows[1];
        let focused = form.focus == Focus::Protocol;
        draw_field_label(frame, th, r, "Protocol", focused);
        let label = form.protocol.label();
        let text = if focused {
            format!("◀ {label} ▶")
        } else {
            label.to_string()
        };
        let style = if focused {
            Style::new().add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        frame.render_widget(
            Paragraph::new(text).style(style),
            Rect {
                x: r.x + LABEL_W,
                width: r.width.saturating_sub(LABEL_W),
                ..r
            },
        );
    }

    // Optional name (row 0, above the protocol): defaults to the host.
    text_field(
        frame,
        0,
        "Name",
        &form.name,
        false,
        Focus::Name,
        if form.protocol == Protocol::S3 {
            "default: the bucket"
        } else {
            "default: the host"
        },
    );

    let host_hint = if config.password_manager.is_some() {
        "optional: can come from the password manager"
    } else {
        "example.com"
    };
    type Field<'a> = (&'a str, &'a crate::input::TextInput, bool, Focus, &'a str);
    let fields: Vec<Field> = match form.protocol {
        Protocol::S3 => vec![
            (
                "Endpoint",
                &form.host,
                false,
                Focus::Host,
                "empty = AWS  (or https://minio.local:9000)",
            ),
            (
                "Region",
                &form.region,
                false,
                Focus::Region,
                "empty = from AWS config",
            ),
            (
                "Bucket",
                &form.bucket,
                false,
                Focus::Bucket,
                "empty = list all buckets",
            ),
            (
                "Access key ID",
                &form.user,
                false,
                Focus::User,
                "empty = AWS credential chain",
            ),
            ("Secret key", &form.pass, true, Focus::Pass, ""),
            (
                "AWS profile",
                &form.aws_profile,
                false,
                Focus::AwsProfile,
                "optional",
            ),
        ],
        Protocol::Sftp => vec![
            ("Host", &form.host, false, Focus::Host, host_hint),
            ("Port", &form.port, false, Focus::Port, ""),
            ("Username", &form.user, false, Focus::User, ""),
            (secret_label, &form.pass, true, Focus::Pass, ""),
            (
                "Key file",
                &form.key,
                false,
                Focus::Key,
                "optional (default: ssh-agent, ~/.ssh keys)",
            ),
        ],
        Protocol::Ftp | Protocol::Ftps => vec![
            ("Host", &form.host, false, Focus::Host, host_hint),
            ("Port", &form.port, false, Focus::Port, ""),
            (
                "Username",
                &form.user,
                false,
                Focus::User,
                "anonymous if empty",
            ),
            ("Password", &form.pass, true, Focus::Pass, ""),
        ],
    };
    let mut fields = fields;
    fields.push((
        "Default path",
        &form.path,
        false,
        Focus::Path,
        "optional: opened after connecting",
    ));
    for (i, (label, input, masked, focus, placeholder)) in fields.into_iter().enumerate() {
        text_field(frame, 3 + i, label, input, masked, focus, placeholder);
    }

    // The selected saved connection's tags (its colour is the dot in the list).
    if let Some(p) = editing {
        let r = rows[10];
        frame.render_widget(
            Paragraph::new("Tags:"),
            Rect {
                width: LABEL_W.min(r.width),
                ..r
            },
        );
        let spans: Vec<Span> = if p.tags.is_empty() {
            vec![Span::styled("none", th.dim_style())]
        } else {
            p.tags
                .iter()
                .map(|t| Span::styled(format!("{} ", sanitize(t)), Style::new().fg(th.accent)))
                .collect()
        };
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect {
                x: r.x + LABEL_W,
                width: r.width.saturating_sub(LABEL_W),
                ..r
            },
        );
    }

    // Notes and validation errors at the bottom of the form (wrapped over two rows).
    let note_area = Rect {
        height: rows[12].bottom() - rows[11].y,
        ..rows[11]
    };
    let (note, note_style) = match form.protocol {
        Protocol::Ftp => (
            "⚠ Plain FTP sends passwords and files unencrypted.",
            Style::new().fg(th.warning),
        ),
        Protocol::Sftp => ("New host keys ask for confirmation first.", th.dim_style()),
        Protocol::Ftps => (
            "Server certificate must be trusted by this system.",
            th.dim_style(),
        ),
        Protocol::S3 => (
            "Empty keys use the AWS credential chain (env, ~/.aws, SSO). The secret key is never saved.",
            th.dim_style(),
        ),
    };
    frame.render_widget(
        Paragraph::new(note)
            .wrap(Wrap { trim: true })
            .style(note_style),
        note_area,
    );
    if let Some(err) = &form.error {
        // Errors can be long (e.g. ssh_config problems): wrap over the spare rows,
        // replacing the note.
        let area = Rect {
            height: rows[13].bottom() - rows[11].y,
            ..rows[11]
        };
        clear(frame, area, th);
        frame.render_widget(
            Paragraph::new(sanitize(err))
                .wrap(Wrap { trim: true })
                .style(Style::new().fg(th.error)),
            area,
        );
    }

    // Most important first: narrow terminals drop hints from the end.
    match form.focus {
        Focus::Profiles => {
            // Keys that act on a connection only show when one is selected.
            let mut h = vec![];
            if profile_selected {
                h.push(("⏎", "connect"));
                h.push(("e", "edit"));
            }
            h.push(("n", "new"));
            if form.has_filter() {
                h.push(("Esc", "clear filter"));
            }
            h.push(("/", "filter"));
            if profile_selected {
                h.extend([("y", "duplicate"), ("c", "colour"), ("t", "tags")]);
            }
            h.push(("f", "folder"));
            if any_selected {
                h.extend([("m", "move"), ("r", "rename"), ("d", "delete")]);
            }
            h.extend([("?", "help"), ("q", "quit")]);
            h
        }
        Focus::Protocol => {
            let mut h = vec![("←→", "protocol"), ("⏎", "connect"), ("n", "new")];
            if editing.is_some() {
                h.push(("Esc", "stop editing"));
            }
            h.extend([
                ("Ctrl-S", "save"),
                ("j/k", "field"),
                ("S", "saved"),
                ("?", "help"),
                ("q", "quit"),
            ]);
            h
        }
        _ => vec![
            ("⏎", "connect"),
            ("Tab", "next"),
            ("Ctrl-S", "save"),
            ("Esc", "leave field"),
            ("Ctrl-B", "saved"),
            ("F1", "help"),
            ("Ctrl-C", "quit"),
        ],
    }
}

fn draw_field_label(frame: &mut Frame, th: &Theme, r: Rect, label: &str, focused: bool) {
    let style = if focused {
        Style::new().fg(th.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    frame.render_widget(
        Paragraph::new(label.to_string()).style(style),
        Rect {
            width: LABEL_W.min(r.width),
            ..r
        },
    );
}

// ───────────────────────────── browser screen ─────────────────────────────

fn draw_browser(
    frame: &mut Frame,
    area: Rect,
    b: &mut BrowserState,
    tick: usize,
    th: &Theme,
    log_focus: bool,
) -> Vec<(&'static str, &'static str)> {
    let transfer_h = if b.transfer.is_some() { 3 } else { 0 };
    let [panes, xfer] =
        Layout::vertical([Constraint::Min(5), Constraint::Length(transfer_h)]).areas(area);
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(panes);

    let active = b.active;
    let label = b.label.clone();
    let conn = b.color.as_deref().and_then(|c| th.conn_color(c));
    draw_pane(
        frame,
        left,
        &mut b.local,
        PaneView {
            label: "Local",
            active: active == Side::Local && !log_focus,
            tick,
            conn: None,
        },
        th,
    );
    draw_pane(
        frame,
        right,
        &mut b.remote,
        PaneView {
            label: &label,
            active: active == Side::Remote && !log_focus,
            tick,
            conn,
        },
        th,
    );

    if let Some(t) = &b.transfer {
        let (done, total) = (t.ctl.done(), t.ctl.total());
        let ratio = if total > 0 {
            (done as f64 / total as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let secs = t.started.elapsed().as_secs_f64().max(0.001);
        let arrow = match t.plan.direction {
            Direction::Upload => "↑",
            Direction::Download => "↓",
        };
        let total_s = if total > 0 {
            format_size(total)
        } else {
            "?".into()
        };
        let text = format!(
            "{arrow} {}   {} / {}   {}",
            sanitize(&t.plan.name),
            format_size(done),
            total_s,
            format_rate(done as f64 / secs),
        );
        frame.render_widget(
            Gauge::default()
                .block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .border_style(Style::new().fg(th.accent))
                        .title(" Transfer — Esc to cancel "),
                )
                .gauge_style(Style::new().fg(th.accent).bg(th.gauge_track()))
                .ratio(ratio)
                .label(text),
            xfer,
        );
    }

    vec![
        ("⏎", "open"),
        ("c", "copy"),
        ("Tab", "pane"),
        ("h", "up"),
        ("m", "mkdir"),
        ("r", "rename"),
        ("x", "delete"),
        (".", "hidden"),
        ("?", "help"),
        ("q", "disconnect"),
    ]
}

fn entry_cells(e: &Entry, wide: bool, with_mode: bool, th: &Theme) -> Row<'static> {
    let name = sanitize(&e.name);
    let (text, style) = if e.is_parent_marker() {
        (
            "../".to_string(),
            Style::new().fg(th.info).add_modifier(Modifier::BOLD),
        )
    } else {
        match (e.kind, e.is_link) {
            (EntryKind::Dir, true) => (format!("{name}@/"), Style::new().fg(th.special)),
            (EntryKind::Dir, false) => (
                format!("{name}/"),
                Style::new().fg(th.info).add_modifier(Modifier::BOLD),
            ),
            (_, true) => (format!("{name}@"), Style::new().fg(th.special)),
            (EntryKind::Other, _) => (name, th.dim_style()),
            _ => (name, Style::new()),
        }
    };
    let size = if e.is_parent_marker() || e.is_dir() {
        String::new()
    } else {
        format_size(e.size)
    };
    let mut cells = vec![
        Cell::from(text).style(style),
        Cell::from(Line::from(size).alignment(Alignment::Right)),
    ];
    if wide {
        let time = e.modified.map(format_time).unwrap_or_default();
        cells.push(Cell::from(time).style(th.dim_style()));
    }
    if with_mode {
        cells.push(Cell::from(e.mode.clone().unwrap_or_default()).style(th.dim_style()));
    }
    Row::new(cells)
}

/// How a pane should be drawn.
struct PaneView<'a> {
    label: &'a str,
    active: bool,
    tick: usize,
    /// Colour of the saved connection this pane belongs to.
    conn: Option<Color>,
}

fn draw_pane(frame: &mut Frame, area: Rect, pane: &mut Pane, view: PaneView, th: &Theme) {
    let PaneView {
        label,
        active,
        tick,
        conn,
    } = view;
    let spinner = if pane.loading {
        format!(" {}", SPINNER[tick % SPINNER.len()])
    } else {
        String::new()
    };
    let max_path = (area.width as usize).saturating_sub(label.width() + 8);
    let title = format!(
        " {label}: {}{spinner} ",
        ellipsize_left(&sanitize(&pane.cwd), max_path)
    );
    // A saved connection's colour marks its pane even when it isn't focused, so
    // it is hard to forget which environment you're in.
    let border = match (conn, active) {
        (Some(c), true) => Style::new().fg(c),
        (Some(c), false) => Style::new().fg(c).add_modifier(Modifier::DIM),
        (None, true) => Style::new().fg(th.accent),
        (None, false) => th.dim_style(),
    };
    let title_line = match conn {
        Some(c) => Line::from(vec![
            Span::styled(" ●", Style::new().fg(c)),
            Span::styled(title, Style::new().fg(c).add_modifier(Modifier::BOLD)),
        ]),
        None => Line::from(title),
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(title_line);
    let inner = block.inner(area);
    pane.page = inner.height.saturating_sub(1) as usize;

    let wide = inner.width >= 46;
    let with_mode = inner.width >= 62;
    let rows: Vec<Row> = pane
        .view
        .iter()
        .map(|e| entry_cells(e, wide, with_mode, th))
        .collect();
    let mut header = vec!["Name", "Size"];
    let mut widths = vec![Constraint::Min(8), Constraint::Length(10)];
    if wide {
        header.push("Modified");
        widths.push(Constraint::Length(16));
    }
    if with_mode {
        header.push("Mode");
        widths.push(Constraint::Length(10));
    }
    let highlight = if active {
        th.selection()
    } else {
        th.selection_inactive()
    };
    let table = Table::new(rows, widths)
        .header(Row::new(header).style(th.dim_style().add_modifier(Modifier::BOLD)))
        .block(block)
        .column_spacing(1)
        .row_highlight_style(highlight);
    frame.render_stateful_widget(table, area, &mut pane.state);

    if pane.view.is_empty() {
        let msg = if pane.loading {
            "Loading…"
        } else {
            "(empty)"
        };
        let r = Rect {
            y: inner.y + 1,
            height: 1,
            ..inner
        };
        frame.render_widget(
            Paragraph::new(msg)
                .style(th.dim_style())
                .alignment(Alignment::Center),
            r,
        );
    }
}

// ───────────────────────────── modals ─────────────────────────────

/// Help content for the current screen: `(keys, description)`; an empty
/// description makes a section heading.
fn help_entries(browser: bool) -> Vec<(&'static str, &'static str)> {
    let mut v: Vec<(&'static str, &'static str)> = Vec::new();
    if browser {
        v.extend([
            ("File browser", ""),
            (
                "Tab  Shift-Tab",
                "switch pane: local, remote, then the log (j k gg G move in it)",
            ),
            ("j k  ↑ ↓", "move"),
            ("gg  G", "top / bottom"),
            ("Ctrl-d  Ctrl-u", "half page down / up"),
            ("Ctrl-f  Ctrl-b  PgDn PgUp", "page down / up"),
            ("l  ⏎  →", "open directory"),
            ("h  -  ⌫  ←", "parent directory"),
            ("c  F5", "copy file to the other pane"),
            ("m  F7", "make directory"),
            ("r  F2", "rename"),
            ("x  Del  F8", "delete file / empty directory (asks first)"),
            (".", "show / hide dotfiles"),
            ("R  Ctrl-R", "refresh"),
            ("Esc", "cancel running transfer"),
            ("q", "disconnect"),
        ]);
    } else {
        v.extend([
            ("[S]aved Connections   S  Ctrl-B", ""),
            ("j k  ↑ ↓", "move"),
            ("gg  G", "top / bottom"),
            ("Ctrl-d  Ctrl-u", "half page down / up"),
            ("h l  ← →  Space", "collapse / expand a folder"),
            ("⏎", "connect (or open / close a folder)"),
            (
                "n",
                "new connection (blank form; Ctrl-L does this from any field)",
            ),
            ("y", "duplicate the connection under a new name"),
            ("e", "edit: moves into the form; Ctrl-S saves your changes"),
            ("r", "rename connection or folder"),
            ("f", "new folder (inside the selected folder)"),
            ("m", "move a connection or folder into another folder"),
            ("c", "colour (shown as a dot, and on the connected pane)"),
            ("t", "edit tags"),
            ("/", "filter: search text, and pick tags"),
            ("Esc", "clear the filter"),
            ("d  Del", "delete (asks first)"),
            ("[C]onnections   C  Ctrl-N", ""),
            ("Tab  Shift-Tab  ↑ ↓", "move between fields"),
            ("← →  (on Protocol)", "change protocol"),
            ("⏎", "connect"),
            (
                "Ctrl-S",
                "save: updates the connection being edited, else asks for a name",
            ),
            (
                "Default path",
                "opened after connecting; if it fails you stay connected",
            ),
            (
                "Ctrl-P",
                "choose a password manager (1Password): empty details are filled from an item named after the connection, and Host becomes optional",
            ),
            ("Esc", "leave a text field (then j k i ? S C q work)"),
        ]);
    }
    v.extend([
        ("", ""),
        ("Anywhere", ""),
        ("?  F1", "this help (F1 also works while typing)"),
        (
            "K  J",
            "scroll the activity log back / forward (PageUp PageDown while typing)",
        ),
        ("t  Ctrl-T", "choose a colour theme"),
        if browser {
            ("Ctrl-C  Ctrl-Q", "quit (asks first)")
        } else {
            ("q  Ctrl-C  Ctrl-Q", "quit (asks first; q not while typing)")
        },
        ("b  (in theme picker)", "transparent background on / off"),
    ]);
    v
}

fn modal_block(th: &Theme, title: &str, color: Color) -> Block<'static> {
    Block::bordered()
        .style(th.base())
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .title(format!(" {title} "))
}

/// Height needed for `text` wrapped to `width` columns.
fn wrapped_height(text: &str, width: u16) -> u16 {
    let width = width.max(1) as usize;
    text.lines()
        .map(|l| l.width().max(1).div_ceil(width))
        .sum::<usize>()
        .max(1) as u16
}

fn draw_modal(frame: &mut Frame, area: Rect, app: &App) {
    let Some(modal) = &app.modal else { return };
    let th = &app.theme;
    match modal {
        Modal::Help { scroll } => {
            let entries = help_entries(app.browser.is_some());
            let h = (entries.len() as u16 + 2).min(area.height);
            let rect = centered(area, 66, h);
            clear(frame, rect, th);
            let rows: Vec<Line> = entries
                .iter()
                .map(|(k, v)| {
                    if v.is_empty() {
                        Line::styled(
                            k.to_string(),
                            Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                        )
                    } else {
                        Line::from(vec![
                            Span::styled(format!("  {k:<18}"), Style::new().fg(th.accent)),
                            Span::raw(v.to_string()),
                        ])
                    }
                })
                .collect();
            let overflow = entries.len() as u16 + 2 > area.height;
            let title = if overflow {
                "Help — ↑↓ scroll, Esc closes"
            } else {
                "Help — Esc closes"
            };
            frame.render_widget(
                Paragraph::new(rows)
                    .scroll((*scroll, 0))
                    .block(modal_block(th, title, th.accent)),
                rect,
            );
        }
        Modal::Colors { profile, index } => {
            let n = COLORS.len() as u16 + 1;
            let rect = centered(area, 40, (n + 4).min(area.height));
            clear(frame, rect, th);
            let mut lines = Vec::new();
            for i in 0..=COLORS.len() {
                let (label, color) = match i.checked_sub(1) {
                    None => ("no colour".to_string(), None),
                    Some(c) => (COLORS[c].to_string(), th.conn_color(COLORS[c])),
                };
                let mut spans = vec![Span::styled(
                    format!("{} ", if i == *index { "▸" } else { " " }),
                    Style::new(),
                )];
                spans.push(match color {
                    Some(c) => Span::styled("● ", Style::new().fg(c)),
                    None => Span::raw("  "),
                });
                spans.push(Span::raw(format!("{label:<28}")));
                let line = Line::from(spans);
                lines.push(if i == *index {
                    line.patch_style(th.selection())
                } else {
                    line
                });
            }
            frame.render_widget(
                Paragraph::new(lines).block(
                    modal_block(
                        th,
                        &format!("Colour for '{}'", ellipsize_right(&sanitize(profile), 18)),
                        th.accent,
                    )
                    .padding(ratatui::widgets::Padding::horizontal(1)),
                ),
                rect,
            );
        }
        Modal::Filter {
            text,
            tags,
            selected,
            index,
            focus,
        } => {
            const MAX_TAG_ROWS: usize = 10;
            let tag_rows = tags.len().clamp(1, MAX_TAG_ROWS);
            // border, search, blank, "Tags", tag rows, blank, hints, border
            let rect = centered(area, 52, (tag_rows as u16 + 7).min(area.height));
            clear(frame, rect, th);
            let block = modal_block(th, "Filter connections", th.accent)
                .padding(ratatui::widgets::Padding::horizontal(1));
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            let label = |text: &'static str, on: bool| {
                Span::styled(
                    text,
                    if on {
                        Style::new().fg(th.accent).add_modifier(Modifier::BOLD)
                    } else {
                        th.dim_style()
                    },
                )
            };

            // Text field above the tag list.
            let search = Rect { height: 1, ..inner };
            let field = Rect {
                x: search.x + 8,
                width: search.width.saturating_sub(8),
                ..search
            };
            frame.render_widget(
                Paragraph::new(Line::from(label("Search ", *focus == FilterFocus::Text))),
                search,
            );
            let (shown, col) = text.view(field.width, false);
            let (shown, style) = if shown.is_empty() && *focus != FilterFocus::Text {
                ("name, host, user…".to_string(), th.dim_style())
            } else {
                (shown, Style::new())
            };
            frame.render_widget(
                Paragraph::new(shown).style(style.add_modifier(Modifier::UNDERLINED)),
                field,
            );
            if *focus == FilterFocus::Text {
                frame.set_cursor_position((field.x + col, field.y));
            }

            let mut lines = vec![
                Line::raw(""),
                Line::from(vec![
                    label("Tags ", *focus == FilterFocus::Tags),
                    Span::styled("(all selected must match)", th.dim_style()),
                ]),
            ];
            if tags.is_empty() {
                lines.push(Line::styled(
                    "  No tags yet. Press t on a connection to add some.",
                    th.dim_style(),
                ));
            }
            let start = (*index + 1).saturating_sub(tag_rows);
            for (i, (tag, count)) in tags.iter().enumerate().skip(start).take(tag_rows) {
                let mark = if selected.contains(tag) { "[x]" } else { "[ ]" };
                let cursor = i == *index && *focus == FilterFocus::Tags;
                let line = Line::from(format!(
                    "{} {mark} {:<33} {count:>3}",
                    if cursor { "▸" } else { " " },
                    ellipsize_right(&sanitize(tag), 32)
                ));
                lines.push(if cursor {
                    line.patch_style(th.selection())
                } else {
                    line
                });
            }
            lines.push(Line::raw(""));
            lines.push(match focus {
                FilterFocus::Text => {
                    hints_line(th, &[("Tab", "tags"), ("⏎", "apply"), ("Esc", "cancel")])
                }
                FilterFocus::Tags => hints_line(
                    th,
                    &[
                        ("Space", "toggle"),
                        ("/", "search"),
                        ("c", "clear"),
                        ("⏎", "apply"),
                        ("Esc", "cancel"),
                    ],
                ),
            });
            frame.render_widget(
                Paragraph::new(lines),
                Rect {
                    y: inner.y + 1,
                    height: inner.height.saturating_sub(1),
                    ..inner
                },
            );
        }
        Modal::Folders {
            choices,
            index,
            profile,
            folder,
        } => {
            let rect = centered(area, 50, (choices.len() as u16 + 4).min(area.height));
            clear(frame, rect, th);
            let lines: Vec<Line> = choices
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let label = match c {
                        FolderChoice::TopLevel => "(top level, no folder)".to_string(),
                        FolderChoice::Folder(f) => format!(
                            "{}▾ {}",
                            "  ".repeat(crate::config::Config::folder_depth(f)),
                            sanitize(crate::config::Config::folder_leaf(f))
                        ),
                        FolderChoice::New => "+ New folder…".to_string(),
                    };
                    let text = format!("{} {label:<44}", if i == *index { "▸" } else { " " });
                    Line::styled(
                        text,
                        if i == *index {
                            th.selection()
                        } else {
                            Style::new()
                        },
                    )
                })
                .collect();
            frame.render_widget(
                Paragraph::new(lines).block(
                    modal_block(
                        th,
                        &format!(
                            "Move '{}' to…",
                            ellipsize_right(
                                &sanitize(if *folder {
                                    crate::config::Config::folder_leaf(profile)
                                } else {
                                    profile
                                }),
                                24
                            )
                        ),
                        th.accent,
                    )
                    .padding(ratatui::widgets::Padding::horizontal(1)),
                ),
                rect,
            );
        }
        Modal::Error { title, message } => {
            let message = sanitize_text(message);
            let w = 70.min(area.width);
            let h = (wrapped_height(&message, w.saturating_sub(4)) + 4).min(area.height);
            let rect = centered(area, w, h);
            clear(frame, rect, th);
            let mut lines: Vec<Line> = message.lines().map(|l| Line::from(l.to_string())).collect();
            lines.push(Line::raw(""));
            lines.push(hints_line(th, &[("Enter", "dismiss")]));
            frame.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                    modal_block(th, &sanitize(title), th.error)
                        .padding(ratatui::widgets::Padding::horizontal(1)),
                ),
                rect,
            );
        }
        Modal::Connecting { label } => {
            let rect = centered(area, 56, 5);
            clear(frame, rect, th);
            let spin = SPINNER[app.tick % SPINNER.len()];
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(format!("{spin} Connecting to {}…", sanitize(label))),
                    Line::raw(""),
                    hints_line(th, &[("Esc", "cancel")]),
                ])
                .alignment(Alignment::Center)
                .block(modal_block(th, "Connecting", th.accent)),
                rect,
            );
        }
        Modal::HostKey(p) => {
            let rect = centered(area, 72, 11);
            clear(frame, rect, th);
            let lines = vec![
                Line::from(format!(
                    "The authenticity of host '{}:{}' can't be established.",
                    sanitize(&p.host),
                    p.port
                )),
                Line::raw(""),
                Line::from(format!("{} key fingerprint:", sanitize(&p.algorithm))),
                Line::styled(
                    sanitize(&p.fingerprint),
                    Style::new().add_modifier(Modifier::BOLD),
                ),
                Line::raw(""),
                Line::from("Trust this host and add it to known_hosts?"),
                Line::from(vec![
                    Span::styled(
                        "y",
                        Style::new().fg(th.success).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" trust    "),
                    Span::styled("n", Style::new().fg(th.error).add_modifier(Modifier::BOLD)),
                    Span::raw(" reject"),
                ]),
            ];
            frame.render_widget(
                Paragraph::new(lines).block(
                    modal_block(th, "Unknown host key", th.warning)
                        .padding(ratatui::widgets::Padding::horizontal(1)),
                ),
                rect,
            );
        }
        Modal::Input {
            title,
            input,
            error,
            ..
        } => {
            let rect = centered(area, 56, 7);
            clear(frame, rect, th);
            let block = modal_block(th, title, th.accent);
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            let field = Rect {
                x: inner.x + 1,
                y: inner.y + 1,
                width: inner.width.saturating_sub(2),
                height: 1,
            };
            let (shown, col) = input.view(field.width, false);
            frame.render_widget(
                Paragraph::new(shown).style(Style::new().add_modifier(Modifier::UNDERLINED)),
                field,
            );
            frame.set_cursor_position((field.x + col, field.y));
            let hint = match error {
                Some(e) => Line::styled(sanitize(e), Style::new().fg(th.error)),
                None => hints_line(th, &[("Enter", "confirm"), ("Esc", "cancel")]),
            };
            // Errors can be long ("'name' is already used by …"): wrap onto two lines.
            frame.render_widget(
                Paragraph::new(hint).wrap(Wrap { trim: true }),
                Rect {
                    x: inner.x + 1,
                    y: inner.y + 3,
                    width: inner.width.saturating_sub(2),
                    height: 2,
                },
            );
        }
        Modal::Themes { index, .. } => {
            // border, blank, "b" line, border; the list scrolls to fit.
            let want = app.themes.len() as u16 + 6;
            let rect = centered(area, 50, want.min(area.height));
            clear(frame, rect, th);
            let visible = (rect.height as usize).saturating_sub(6).max(1);
            let start = (*index + 1).saturating_sub(visible);
            let lines: Vec<Line> = app
                .themes
                .iter()
                .enumerate()
                .skip(start)
                .take(visible)
                .map(|(i, t)| {
                    let selected = i == *index;
                    let mut spans = vec![Span::styled(
                        format!(
                            "{} {:<26}",
                            if selected { "▸" } else { " " },
                            ellipsize_right(&sanitize(&t.name), 25)
                        ),
                        if selected {
                            th.selection()
                        } else {
                            Style::new()
                        },
                    )];
                    spans.push(Span::raw(" "));
                    for c in t.clone().quantized(app.truecolor).swatch() {
                        spans.push(Span::styled("■", Style::new().fg(c)));
                    }
                    Line::from(spans)
                })
                .collect();
            let mut lines = lines;
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled(
                    "b ",
                    Style::new().fg(th.accent).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(
                    "[{}] transparent background",
                    if app.transparent { "x" } else { " " }
                )),
            ]));
            frame.render_widget(
                Paragraph::new(lines).block(
                    modal_block(th, "Theme — ⏎ save, Esc cancel", th.accent)
                        .padding(ratatui::widgets::Padding::horizontal(1)),
                ),
                rect,
            );
        }
        Modal::PasswordManager { index } => {
            let rect = centered(area, 62, 10.min(area.height));
            clear(frame, rect, th);
            let mut rows: Vec<(String, &str)> = vec![(
                "None".to_string(),
                "passwords are typed each time and never saved",
            )];
            rows.extend(crate::config::PasswordManager::ALL.iter().map(|m| {
                (
                    m.label().to_string(),
                    "fills empty details from an item named after the connection",
                )
            }));
            let mut lines: Vec<Line> = rows
                .iter()
                .enumerate()
                .map(|(i, (name, note))| {
                    let line = Line::from(vec![
                        Span::raw(format!(
                            "{} {name:<10}",
                            if i == *index { "▸" } else { " " }
                        )),
                        Span::styled(format!(" {note}"), th.dim_style()),
                    ]);
                    if i == *index {
                        line.patch_style(th.selection())
                    } else {
                        line
                    }
                })
                .collect();
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "With a manager the Host is optional: it can come from the item.",
                th.dim_style(),
            ));
            lines.push(hints_line(th, &[("⏎", "choose"), ("Esc", "cancel")]));
            frame.render_widget(
                Paragraph::new(lines).block(
                    modal_block(th, "Password manager", th.accent)
                        .padding(ratatui::widgets::Padding::horizontal(1)),
                ),
                rect,
            );
        }
        Modal::Unsaved { message, .. } => {
            let w = 60.min(area.width);
            let h = (wrapped_height(message, w.saturating_sub(4)) + 4).min(area.height);
            let rect = centered(area, w, h);
            clear(frame, rect, th);
            let lines = vec![
                Line::from(message.clone()),
                Line::raw(""),
                Line::from(vec![
                    Span::styled(
                        "y",
                        Style::new().fg(th.success).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" save    "),
                    Span::styled("n", Style::new().fg(th.error).add_modifier(Modifier::BOLD)),
                    Span::raw(" discard    "),
                    Span::styled(
                        "Esc",
                        Style::new().fg(th.accent).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" cancel"),
                ]),
            ];
            frame.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                    modal_block(th, "Unsaved changes", th.warning)
                        .padding(ratatui::widgets::Padding::horizontal(1)),
                ),
                rect,
            );
        }
        Modal::Confirm { message, .. } => {
            let w = 60.min(area.width);
            let h = (wrapped_height(message, w.saturating_sub(4)) + 4).min(area.height);
            let rect = centered(area, w, h);
            clear(frame, rect, th);
            let lines = vec![
                Line::from(message.clone()),
                Line::raw(""),
                Line::from(vec![
                    Span::styled(
                        "y",
                        Style::new().fg(th.success).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" yes    "),
                    Span::styled("n", Style::new().fg(th.error).add_modifier(Modifier::BOLD)),
                    Span::raw(" no"),
                ]),
            ];
            frame.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                    modal_block(th, "Confirm", th.warning)
                        .padding(ratatui::widgets::Padding::horizontal(1)),
                ),
                rect,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ellipsizes_from_the_left() {
        assert_eq!(ellipsize_left("/a/b/c", 10), "/a/b/c");
        let s = ellipsize_left("/very/long/path/name", 8);
        assert!(s.starts_with('…'));
        assert!(s.width() <= 8);
        assert!(s.ends_with("name"));
    }

    #[test]
    fn ellipsizes_from_the_right() {
        assert_eq!(ellipsize_right("short", 10), "short");
        let s = ellipsize_right("sftp://someone@host.example:2222", 12);
        assert!(s.ends_with('…'));
        assert!(s.width() <= 12);
        assert!(s.starts_with("sftp://"));
    }

    #[test]
    fn wrapped_height_counts_lines() {
        assert_eq!(wrapped_height("", 10), 1);
        assert_eq!(wrapped_height("abcdefghij", 5), 2);
        assert_eq!(wrapped_height("a\nb\nc", 10), 3);
    }
}
