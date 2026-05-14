use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    prelude::*,
    symbols,
    widgets::{Axis, Block, Chart, Dataset, GraphType, Paragraph},
};
use serde::Deserialize;
use std::{
    collections::VecDeque,
    io::{self, Stdout},
    net::IpAddr,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

const BUF_SIZE: usize = 60 * 60;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// IP address of the P1 meter
    #[arg(long, value_parser = parse_ip_addr)]
    ip: IpAddr,
}

fn parse_ip_addr(s: &str) -> Result<IpAddr, String> {
    s.parse::<IpAddr>().map_err(|_| format!("`{}` is not a valid IP address", s))
}

#[derive(Deserialize, Debug, Clone, Copy)]
struct P1Data {
    active_power_w: f64,
}

struct AppState {
    data_points: VecDeque<(f64, f64)>,
    last_error: Option<String>,
    last_value: Option<f64>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let api_url = format!("http://{}/api/v1/data", cli.ip);

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Run app
    let res = run_app(&mut terminal, api_url);

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        println!("{:?}", err);
    }

    Ok(())
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<Stdout>>, api_url: String) -> Result<()> {
    let state = Arc::new(Mutex::new(AppState {
        data_points: VecDeque::with_capacity(BUF_SIZE),
        last_error: None,
        last_value: None,
    }));

    let state_clone = state.clone();
    thread::spawn(move || {
        let client = match reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
        {
            Ok(client) => client,
            Err(e) => {
                let mut state = state_clone.lock().unwrap();
                state.last_error = Some(format!("Failed to build HTTP client: {}", e));
                return; // Exit the thread if client cannot be built
            }
        };
        let url = api_url;
        let mut x_offset = 0.0;

        loop {
            match client.get(&url).send() {
                Ok(resp) => match resp.json::<P1Data>() {
                    Ok(p1_data) => {
                        let mut state = state_clone.lock().unwrap();
                        state.last_error = None;
                        state.last_value = Some(p1_data.active_power_w);

                        if state.data_points.len() >= BUF_SIZE {
                            state.data_points.pop_front();
                        }
                        state
                            .data_points
                            .push_back((x_offset, p1_data.active_power_w));
                        x_offset += 1.0;
                    }
                    Err(e) => {
                        let mut state = state_clone.lock().unwrap();
                        state.last_error = Some(format!("Parse error: {}", e));
                    }
                },
                Err(e) => {
                    let mut state = state_clone.lock().unwrap();
                    state.last_error = Some(format!("Request error: {}", e));
                }
            }
            thread::sleep(Duration::from_secs(1));
        }
    });

    loop {
        terminal.draw(|f| {
            let area = f.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(0), Constraint::Length(3)])
                .split(area);

            let state = state.lock().unwrap();

            // Transform data to be relative to the latest point (which is at 0)
            let raw_data_original: Vec<(f64, f64)> = state.data_points.iter().cloned().collect();
            let latest_x = raw_data_original.last().map(|(x, _)| *x).unwrap_or(0.0);

            let raw_data: Vec<(f64, f64)> = raw_data_original.iter()
                .map(|(x, y)| (x - latest_x, *y))
                .collect();

            // Calculate symmetric Y bounds (0 in middle)
            let max_abs_y = raw_data.iter()
                .map(|(_, y)| y.abs())
                .fold(0.0, f64::max);

            let y_limit = if max_abs_y == 0.0 { 10.0 } else { max_abs_y * 1.1 };
            let y_bounds = [-y_limit, y_limit];

            // Split data into positive - consumption (Red) and negative - injection (Green) points
            let mut pos_data = Vec::new();
            let mut neg_data = Vec::new();

            if !raw_data.is_empty() {
                for (x, y) in raw_data.iter() {
                    if *y >= 0.0 { pos_data.push((*x, *y)); } else { neg_data.push((*x, *y)); }
                }

            }

            // Calculate dynamic X-axis range based on terminal width
            // Subtract 2 for the borders
            let graph_width = chunks[0].width.saturating_sub(2) as f64;
            // Ensure we have at least some range
            let display_width = if graph_width < 10.0 { 10.0 } else { graph_width };

            let x_bounds = [-display_width, 0.0];

            let datasets = vec![
                Dataset::default()
                    .name("Consumption (Red)")
                    .marker(symbols::Marker::Braille)
                    .graph_type(GraphType::Scatter)
                    .style(Style::default().fg(Color::Red))
                    .data(&pos_data),
                Dataset::default()
                    .name("Injection (Green)")
                    .marker(symbols::Marker::Braille)
                    .graph_type(GraphType::Scatter)
                    .style(Style::default().fg(Color::Green))
                    .data(&neg_data),
            ];

            let chart = Chart::new(datasets)
                .block(Block::bordered().title("P1 Meter Active Power"))
                .legend_position(Some(ratatui::widgets::LegendPosition::BottomLeft))
                .x_axis(
                    Axis::default()
                        .title("Time (s)")
                        .style(Style::default().fg(Color::Gray))
                        .bounds(x_bounds)
                        .labels(vec![
                            Span::styled(format!("{:.0}", x_bounds[0]), Style::default().bold()),
                            Span::styled("0", Style::default().bold()),
                        ]),
                )
                .y_axis(
                    Axis::default()
                        .title("Power (W)")
                        .style(Style::default().fg(Color::Gray))
                        .bounds(y_bounds)
                        .labels(vec![
                            Span::styled(format!("{:.0}", y_bounds[0]), Style::default().bold()),
                            Span::styled("0", Style::default().bold()),
                            Span::styled(format!("{:.0}", y_bounds[1]), Style::default().bold()),
                        ]),
                );

            f.render_widget(chart, chunks[0]);

            // Status bar
            let (status_text, status_color) = if let Some(err) = &state.last_error {
                (format!("Error: {}", err), Color::Red)
            } else if let Some(val) = state.last_value {
                let color = if val > 0.0 { Color::Red } else { Color::Green };

                // Calculate stats
                let values: Vec<f64> = state.data_points.iter().map(|(_, v)| *v).collect();
                let min = values.iter().fold(f64::INFINITY, |a, &b| a.min(b));
                let max = values.iter().fold(f64::NEG_INFINITY, |a, &b| a.max(b));
                let avg = if !values.is_empty() {
                    values.iter().sum::<f64>() / values.len() as f64
                } else {
                    0.0
                };

                (format!("Current: {:4.0} W | Min: {:4.0} W | Max: {:4.0} W | Avg: {:4.0} W | Sample window: {:3.0} min",
                    val, min, max, avg, state.data_points.len() / 60), color)
            } else {
                ("Waiting for data...".to_string(), Color::Yellow)
            };

            let status = Paragraph::new(status_text)
                .style(Style::default().fg(status_color))
                .block(Block::bordered().title("Status"));

            f.render_widget(status, chunks[1]);
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press && key.code == KeyCode::Char('q') {
                    return Ok(());
                }
            }
        }
    }
}