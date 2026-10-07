use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Gauge, Padding, Paragraph, Row, Table, TableState,
};
use ratatui::{DefaultTerminal, Frame};
use tokio::runtime::Runtime;

use super::{format_cost, format_tokens, format_usd, usage_bar};
use crate::config::{Config, Credentials};
use crate::harness::{self, HarnessPaths};
use crate::litellm::budget::fetch_budget;
use crate::litellm::{BudgetInfo, Client, ModelSpec};

struct HarnessStatus {
    name: &'static str,
    detected: bool,
    installed: bool,
}

struct Data {
    models: Result<Vec<ModelSpec>, String>,
    budget: Result<BudgetInfo, String>,
    harnesses: Vec<HarnessStatus>,
}

struct App {
    creds: Credentials,
    data: Data,
    table: TableState,
}

pub fn dashboard(rt: &Runtime) -> Result<()> {
    let creds = super::require_credentials(&Config::load()?)?;
    let data = load(rt, &creds);
    let mut app = App {
        creds,
        data,
        table: TableState::default().with_selected(Some(0)),
    };
    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app, rt);
    ratatui::restore();
    result
}

fn load(rt: &Runtime, creds: &Credentials) -> Data {
    let fetched = Client::new(creds)
        .map_err(|e| format!("{e:#}"))
        .map(|client| {
            rt.block_on(async {
                let models = client.models().await.map_err(|e| format!("{e:#}"));
                let budget = fetch_budget(&client, models.as_deref().unwrap_or(&[]), false)
                    .await
                    .map_err(|e| format!("{e:#}"));
                (models, budget)
            })
        });
    let (models, budget) = match fetched {
        Ok(pair) => pair,
        Err(e) => (Err(e.clone()), Err(e)),
    };
    let paths = HarnessPaths::default();
    let harnesses = harness::all(&paths)
        .iter()
        .map(|h| HarnessStatus {
            name: h.id().display_name(),
            detected: h.detect(),
            installed: h.is_installed(),
        })
        .collect();
    Data {
        models,
        budget,
        harnesses,
    }
}

fn run(terminal: &mut DefaultTerminal, app: &mut App, rt: &Runtime) -> Result<()> {
    loop {
        terminal.draw(|frame| draw(frame, app))?;
        if !event::poll(Duration::from_millis(500))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let rows = app.data.models.as_ref().map(Vec::len).unwrap_or(0);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
            KeyCode::Char('r') => {
                terminal.draw(|frame| {
                    draw(frame, app);
                    let area = frame.area();
                    frame.render_widget(
                        Paragraph::new(" refreshing… ").reversed(),
                        Rect::new(area.x + 2, area.bottom().saturating_sub(1), 14, 1),
                    );
                })?;
                app.data = load(rt, &app.creds);
            }
            KeyCode::Down | KeyCode::Char('j') if rows > 0 => app
                .table
                .select(Some((app.table.selected().unwrap_or(0) + 1).min(rows - 1))),
            KeyCode::Up | KeyCode::Char('k') => app
                .table
                .select(Some(app.table.selected().unwrap_or(0).saturating_sub(1))),
            KeyCode::PageDown if rows > 0 => app
                .table
                .select(Some((app.table.selected().unwrap_or(0) + 10).min(rows - 1))),
            KeyCode::PageUp => app
                .table
                .select(Some(app.table.selected().unwrap_or(0).saturating_sub(10))),
            _ => {}
        }
    }
}

fn draw(frame: &mut Frame, app: &mut App) {
    let [header, usage, harnesses, models, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(5),
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    frame.render_widget(
        Line::from(vec![
            " liteton ".bold().black().on_cyan(),
            Span::raw("  "),
            Span::raw(&app.creds.base_url).dark_gray(),
        ]),
        header,
    );
    draw_usage(frame, usage, &app.data.budget);
    draw_harnesses(frame, harnesses, &app.data.harnesses);
    draw_models(frame, models, &app.data.models, &mut app.table);
    frame.render_widget(
        Line::from(" q quit · r refresh · ↑/↓ scroll · liteton install to configure harnesses")
            .dark_gray(),
        footer,
    );
}

fn block(title: &str) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(Line::from(format!(" {title} ")).bold())
}

fn draw_usage(frame: &mut Frame, area: Rect, budget: &Result<BudgetInfo, String>) {
    let outer = block("Budget");
    let inner = outer.inner(area);
    frame.render_widget(outer, area);
    let info = match budget {
        Ok(info) => info,
        Err(e) => {
            frame.render_widget(
                Paragraph::new(format!("Could not load the budget: {e}")).red(),
                inner,
            );
            return;
        }
    };
    let [gauge_area, detail_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(2)]).areas(inner);
    match (info.max_budget, info.ratio()) {
        (Some(max), ratio) => {
            let ratio = ratio.unwrap_or(1.0);
            let color = match usage_bar::level_color(ratio) {
                crossterm::style::Color::Red => Color::Red,
                crossterm::style::Color::Yellow => Color::Yellow,
                _ => Color::Green,
            };
            frame.render_widget(
                Gauge::default()
                    .ratio(ratio)
                    .gauge_style(Style::new().fg(color).bg(Color::DarkGray))
                    .label(format!(
                        "{} / {}  ({:.0}%)",
                        format_usd(info.spend),
                        format_usd(max),
                        ratio * 100.0
                    ))
                    .use_unicode(true),
                gauge_area,
            );
            let mut detail = format!("{} remaining", format_usd(info.remaining().unwrap_or(0.0)));
            if let Some(reset) = usage_bar::reset_text(info, Utc::now()) {
                detail.push_str(&format!(" · {reset}"));
            }
            if let Some(alias) = &info.key_alias {
                detail.push_str(&format!(" · key \"{alias}\""));
            }
            frame.render_widget(Paragraph::new(detail).dark_gray(), detail_area);
        }
        (None, _) => {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(vec![format_usd(info.spend).bold(), Span::raw(" spent")]),
                    Line::from("No budget limit on this key").dark_gray(),
                ]),
                inner,
            );
        }
    }
}

fn draw_harnesses(frame: &mut Frame, area: Rect, harnesses: &[HarnessStatus]) {
    let mut spans = Vec::new();
    for h in harnesses {
        let (symbol, label, style) = match (h.installed, h.detected) {
            (true, _) => ("●", "configured", Style::new().green()),
            (false, true) => ("○", "not configured", Style::new().yellow()),
            (false, false) => ("·", "not found", Style::new().dark_gray()),
        };
        spans.push(Span::styled(format!("{symbol} "), style));
        spans.push(Span::raw(h.name).bold());
        spans.push(Span::styled(
            format!(" {label}    "),
            style.add_modifier(Modifier::DIM),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(block("Harnesses")),
        area,
    );
}

fn draw_models(
    frame: &mut Frame,
    area: Rect,
    models: &Result<Vec<ModelSpec>, String>,
    state: &mut TableState,
) {
    let models = match models {
        Ok(models) => models,
        Err(e) => {
            frame.render_widget(
                Paragraph::new(format!("Could not load models: {e}"))
                    .red()
                    .block(block("Models")),
                area,
            );
            return;
        }
    };
    let flag = |b: bool| if b { "✓" } else { "" };
    let rows = models.iter().map(|m| {
        Row::new(vec![
            Cell::from(m.id.clone()),
            Cell::from(Line::from(format_tokens(m.context_window)).right_aligned()),
            Cell::from(Line::from(format_tokens(m.max_output_tokens)).right_aligned()),
            Cell::from(Line::from(format_cost(m.input_cost)).right_aligned()),
            Cell::from(Line::from(format_cost(m.output_cost)).right_aligned()),
            Cell::from(Line::from(flag(m.tool_calling)).centered()),
            Cell::from(Line::from(flag(m.vision)).centered()),
            Cell::from(Line::from(flag(m.reasoning)).centered()),
        ])
    });
    let header = Row::new(
        [
            "Model",
            "Context",
            "Max out",
            "In $/1M",
            "Out $/1M",
            "Tools",
            "Vision",
            "Reasoning",
        ]
        .map(|h| Cell::from(h).bold()),
    )
    .bottom_margin(0);
    let title = format!("Models ({})", models.len());
    let table = Table::new(
        rows,
        [
            Constraint::Fill(1),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(6),
            Constraint::Length(7),
            Constraint::Length(10),
        ],
    )
    .header(header)
    .row_highlight_style(Style::new().reversed())
    .block(block(&title));
    frame.render_stateful_widget(table, area, state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::litellm::budget::BudgetSource;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(budget: Result<BudgetInfo, String>) -> String {
        let mut app = App {
            creds: Credentials {
                base_url: "https://llm.example.com".into(),
                api_key: "k".into(),
            },
            data: Data {
                models: Ok(vec![crate::harness::tests::spec(
                    "azure/gpt-5.4-nano",
                    true,
                )]),
                budget,
                harnesses: vec![
                    HarnessStatus {
                        name: "VSCode",
                        detected: true,
                        installed: true,
                    },
                    HarnessStatus {
                        name: "Cursor",
                        detected: false,
                        installed: false,
                    },
                ],
            },
            table: TableState::default().with_selected(Some(0)),
        };
        let mut terminal = Terminal::new(TestBackend::new(110, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_budget_models_and_harnesses() {
        let screen = render(Ok(BudgetInfo {
            source: BudgetSource::Key,
            spend: 10.0,
            max_budget: Some(20.0),
            budget_duration: None,
            budget_reset_at: None,
            key_alias: Some("me".into()),
        }));
        assert!(screen.contains("$10.00 / $20.00  (50%)"), "{screen}");
        assert!(screen.contains("$10.00 remaining · key \"me\""));
        assert!(screen.contains("azure/gpt-5.4-nano"));
        assert!(screen.contains("VSCode configured"));
        assert!(screen.contains("Cursor not found"));
    }

    #[test]
    fn renders_errors_in_place() {
        let screen = render(Err("403 Forbidden".into()));
        assert!(
            screen.contains("Could not load the budget: 403 Forbidden"),
            "{screen}"
        );
    }
}
