use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Flex, HorizontalAlignment as Alignment, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Gauge, HighlightSpacing, Padding, Paragraph, Row, Table, TableState,
};
use ratatui::{DefaultTerminal, Frame};
use tokio::runtime::Runtime;

use super::{format_cost, format_tokens, format_usd, usage_bar};
use crate::config::{Config, Credentials};
use crate::harness::{self, HarnessPaths};
use crate::litellm::budget::fetch_budget;
use crate::litellm::{BudgetInfo, Client, ModelSpec, PriceTier, Pricing};

struct HarnessStatus {
    name: &'static str,
    detected: bool,
    installed: bool,
}

struct Data {
    models: Result<Vec<ModelSpec>, String>,
    budget: Result<Option<BudgetInfo>, String>,
    harnesses: Vec<HarnessStatus>,
}

struct App {
    creds: Credentials,
    data: Data,
    table: TableState,
    ping: bool,
}

pub fn dashboard(rt: &Runtime) -> Result<()> {
    let creds = super::require_credentials(&Config::load()?)?;
    let data = load(rt, &creds, false);
    let mut app = App {
        creds,
        data,
        table: TableState::default().with_selected(Some(0)),
        ping: false,
    };
    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app, rt);
    ratatui::restore();
    result
}

fn load(rt: &Runtime, creds: &Credentials, ping: bool) -> Data {
    let fetched = Client::new(creds)
        .map_err(|e| format!("{e:#}"))
        .map(|client| {
            rt.block_on(async {
                let models = client.models().await.map_err(|e| format!("{e:#}"));
                let budget = fetch_budget(&client, models.as_deref().unwrap_or(&[]), ping)
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
            KeyCode::Char('r') => reload(terminal, app, rt)?,
            KeyCode::Char('p') if matches!(app.data.budget, Ok(None)) => {
                app.ping = true;
                reload(terminal, app, rt)?;
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

fn reload(terminal: &mut DefaultTerminal, app: &mut App, rt: &Runtime) -> Result<()> {
    terminal.draw(|frame| {
        draw(frame, app);
        let area = frame.area();
        frame.render_widget(
            Paragraph::new(" refreshing… ").reversed(),
            Rect::new(area.x + 2, area.bottom().saturating_sub(1), 14, 1),
        );
    })?;
    app.data = load(rt, &app.creds, app.ping);
    Ok(())
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
    let ping = if matches!(app.data.budget, Ok(None)) {
        " · p check budget"
    } else {
        ""
    };
    frame.render_widget(
        Line::from(format!(
            " q quit · r refresh{ping} · ↑/↓ scroll · liteton install to configure harnesses"
        ))
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

fn draw_usage(frame: &mut Frame, area: Rect, budget: &Result<Option<BudgetInfo>, String>) {
    let outer = block("Budget");
    let inner = outer.inner(area);
    frame.render_widget(outer, area);
    let info = match budget {
        Ok(Some(info)) => info,
        Ok(None) => {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from("This key may not read its own budget."),
                    Line::from(vec![
                        Span::raw("Press "),
                        "p".cyan().bold(),
                        Span::raw(" to read it from the headers of a 1-token request."),
                    ])
                    .dark_gray(),
                ]),
                inner,
            );
            return;
        }
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
    let mut columns = model_columns(models);
    let available = area.width.saturating_sub(6);
    let needed = |columns: &[Column]| -> u16 {
        columns
            .iter()
            .map(|column| column.width() + COLUMN_SPACING)
            .sum()
    };
    if needed(&columns) > available {
        columns.retain(|column| !column.optional);
    }

    let header = Row::new(columns.iter().map(|column| {
        Line::from(column.title)
            .alignment(column.align)
            .cyan()
            .bold()
    }))
    .bottom_margin(1);
    let rows = (0..models.len()).map(|i| {
        Row::new(
            columns
                .iter()
                .map(|column| Cell::from(column.cells[i].clone().alignment(column.align))),
        )
    });
    let widths: Vec<Constraint> = columns
        .iter()
        .map(|column| Constraint::Length(column.width()))
        .collect();

    let title = format!("Models ({})", models.len());
    let mut legend = vec![Span::raw(" USD per 1M tokens")];
    if models.iter().any(|m| m.long_context().is_some()) {
        legend.extend([
            Span::raw(" · "),
            "yellow".yellow(),
            Span::raw(" applies once the prompt passes "),
            "Long".yellow(),
        ]);
    }
    legend.push(Span::raw(" "));
    let block = block(&title).title_bottom(Line::from(legend).dark_gray());

    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(COLUMN_SPACING)
        .flex(Flex::Start)
        .highlight_symbol(Line::from("› ").cyan().bold())
        .highlight_spacing(HighlightSpacing::Always)
        .row_highlight_style(Style::new().bold())
        .block(block);
    frame.render_stateful_widget(table, area, state);
}

const COLUMN_SPACING: u16 = 2;

struct Column {
    title: &'static str,
    align: Alignment,
    optional: bool,
    cells: Vec<Line<'static>>,
}

impl Column {
    fn width(&self) -> u16 {
        self.cells
            .iter()
            .map(Line::width)
            .chain([self.title.len()])
            .max()
            .unwrap_or(0) as u16
    }
}

/// Same columns and colors as `liteton models`.
fn model_columns(models: &[ModelSpec]) -> Vec<Column> {
    let column = |title, align, optional, cell: &dyn Fn(&ModelSpec) -> Line<'static>| Column {
        title,
        align,
        optional,
        cells: models.iter().map(cell).collect(),
    };
    let price = |pick: fn(&Pricing) -> Option<f64>| {
        move |m: &ModelSpec| {
            price_line(
                pick(&m.pricing),
                m.long_context().and_then(|tier| pick(&tier.pricing)),
            )
        }
    };
    use Alignment::{Center, Left, Right};
    vec![
        column("Model", Left, false, &|m| Line::from(m.id.clone()).bold()),
        column("Context", Right, false, &|m| {
            Line::from(format_tokens(m.context_window))
        }),
        column("Max out", Right, false, &|m| {
            Line::from(format_tokens(m.max_output_tokens))
        }),
        column("Long", Right, false, &|m| long_label(m.long_context())),
        column("Input", Right, false, &price(|p| p.input)),
        column("Output", Right, false, &price(|p| p.output)),
        column("Cache read", Right, true, &price(|p| p.cache_read)),
        column("Cache write", Right, true, &price(|p| p.cache_write)),
        column("Tools", Center, false, &|m| mark(m.tool_calling)),
        column("Vision", Center, false, &|m| mark(m.vision)),
        column("Reasoning", Center, false, &|m| mark(m.reasoning)),
    ]
}

fn mark(on: bool) -> Line<'static> {
    if on {
        Line::from("✓").green()
    } else {
        Line::from("·").dark_gray()
    }
}

fn long_label(long: Option<&PriceTier>) -> Line<'static> {
    match long {
        Some(tier) => Line::from(format!(">{}", format_tokens(Some(tier.above_tokens)))).yellow(),
        None => Line::from("—").dark_gray(),
    }
}

/// Normal rate in the terminal foreground, long-context rate in its yellow.
fn price_line(base: Option<f64>, long: Option<f64>) -> Line<'static> {
    match long {
        Some(long) => Line::from(vec![
            Span::raw(format_cost(base)),
            Span::styled(" → ", Style::new().dark_gray()),
            Span::styled(format_cost(Some(long)), Style::new().yellow()),
        ]),
        None => Line::from(format_cost(base)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::litellm::budget::BudgetSource;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(budget: Result<Option<BudgetInfo>, String>) -> String {
        let models = vec![crate::harness::tests::spec("azure/gpt-5.4-nano", true)];
        render_with(110, models, budget)
    }

    fn render_with(
        width: u16,
        models: Vec<ModelSpec>,
        budget: Result<Option<BudgetInfo>, String>,
    ) -> String {
        let mut app = App {
            creds: Credentials {
                base_url: "https://llm.example.com".into(),
                api_key: "k".into(),
            },
            data: Data {
                models: Ok(models),
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
            ping: false,
        };
        let mut terminal = Terminal::new(TestBackend::new(width, 20)).unwrap();
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
        let screen = render(Ok(Some(BudgetInfo {
            source: BudgetSource::Key,
            spend: 10.0,
            max_budget: Some(20.0),
            budget_duration: None,
            budget_reset_at: None,
            key_alias: Some("me".into()),
        })));
        assert!(screen.contains("$10.00 / $20.00  (50%)"), "{screen}");
        assert!(screen.contains("$10.00 remaining · key \"me\""));
        assert!(screen.contains("azure/gpt-5.4-nano"));
        assert!(screen.contains("VSCode configured"));
        assert!(screen.contains("Cursor not found"));
        assert!(!screen.contains("p check budget"));
    }

    #[test]
    fn renders_errors_in_place() {
        let screen = render(Err("403 Forbidden".into()));
        assert!(
            screen.contains("Could not load the budget: 403 Forbidden"),
            "{screen}"
        );
    }

    #[test]
    fn drops_cache_columns_when_narrow() {
        let mut luna = crate::harness::tests::spec("gpt-6-luna", true);
        luna.pricing.cache_read = Some(0.01);
        luna.pricing.cache_write = Some(0.125);
        luna.tiers = vec![PriceTier {
            above_tokens: 272_000,
            pricing: Pricing {
                input: Some(0.2),
                output: Some(0.75),
                cache_read: Some(0.02),
                cache_write: Some(0.25),
            },
        }];
        let wide = render_with(160, vec![luna.clone()], Ok(None));        assert!(wide.contains("Cache write"), "{wide}");
        assert!(wide.contains("$0.12 → $0.25"));
        assert!(wide.contains("yellow applies once the prompt passes Long"));

        let narrow = render_with(100, vec![luna], Ok(None));
        assert!(!narrow.contains("Cache"), "{narrow}");
        assert!(narrow.contains("$0.05 → $0.20") && narrow.contains("Reasoning"));
    }

    #[test]
    fn offers_a_ping_when_the_budget_is_hidden() {
        let screen = render(Ok(None));
        assert!(
            screen.contains("This key may not read its own budget."),
            "{screen}"
        );
        assert!(screen.contains("Press p to read it"));
        assert!(screen.contains("p check budget"));
    }
}
