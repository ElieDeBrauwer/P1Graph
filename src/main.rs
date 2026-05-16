
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

use gtk4::prelude::*;
use gtk4::{self as gtk, gdk::cairo};
use glib;

const BUF_SIZE: usize = 60 * 60;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// IP address of the P1 meter
    #[arg(long, value_parser = parse_ip_addr)]
    ip: IpAddr,

    /// Force text-based UI
    #[arg(long, default_value_t = false)]
    text_ui: bool,
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
    next_x: f64,
}

fn spawn_data_fetcher(api_url: String, state: Arc<Mutex<AppState>>) {
    thread::spawn(move || {
        let client = match reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
        {
            Ok(client) => client,
            Err(e) => {
                let mut state = state.lock().unwrap();
                state.last_error = Some(format!("Failed to build HTTP client: {}", e));
                return;
            }
        };

        loop {
            match client.get(&api_url).send() {
                Ok(resp) => match resp.json::<P1Data>() {
                    Ok(p1_data) => {
                        let mut state = state.lock().unwrap();
                        state.last_error = None;
                        state.last_value = Some(p1_data.active_power_w);

                        if state.data_points.len() >= BUF_SIZE {
                            state.data_points.pop_front();
                        }
                        let x = state.next_x;
                        state
                            .data_points
                            .push_back((x, p1_data.active_power_w));
                        state.next_x += 1.0;
                    }
                    Err(e) => {
                        let mut state = state.lock().unwrap();
                        state.last_error = Some(format!("Parse error: {}", e));

                    }
                },
                Err(e) => {
                    let mut state = state.lock().unwrap();
                    state.last_error = Some(format!("Request error: {}", e));
                }
            }
            thread::sleep(Duration::from_secs(1));
        }
    });
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let api_url = format!("http://{}/api/v1/data", cli.ip);

    let use_gtk = if cli.text_ui {
        false
    } else {
        std::env::var("DISPLAY").is_ok() || std::env::var("WAYLAND_DISPLAY").is_ok()
    };

    if use_gtk {
        run_gtk_app(api_url, cli.ip)?;
    } else {
        run_tui_app(api_url, cli.ip)?;
    }

    Ok(())
}

fn run_tui_app(api_url: String, _ip: IpAddr) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run_app(&mut terminal, api_url);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        println!("{:?}", err);
    }

    Ok(())
}

fn run_gtk_app(api_url: String, _ip: IpAddr) -> Result<()> {
    let application = gtk::Application::builder()
        .application_id("com.example.P1Graph")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let app_state = Arc::new(Mutex::new(AppState {
        data_points: VecDeque::with_capacity(BUF_SIZE),
        last_error: None,
        last_value: None,
        next_x: 0.0,
    }));

    spawn_data_fetcher(api_url.clone(), app_state.clone());

    application.connect_activate(move |app| {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("P1Graph")
            .default_width(800)
            .default_height(600)
            .build();

        let vbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();

        // Chart area
        let drawing_area = gtk::DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .build();
        vbox.append(&drawing_area);

        // Status bar
        let status_label = gtk::Label::builder()
            .label(&format!("Waiting for data from: {}", api_url))
            .halign(gtk::Align::Start)
            .use_markup(true)
            .build();
        vbox.append(&status_label);

        window.set_child(Some(&vbox));
        window.present();

        let key_controller = gtk::EventControllerKey::new();
        let window_clone = window.clone();
        key_controller.connect_key_pressed(move |_, keyval, _, _| {
            if keyval == gtk::gdk::Key::q || keyval == gtk::gdk::Key::Escape {
                window_clone.close();
            }
            glib::Propagation::Proceed
        });
        window.add_controller(key_controller);

        let app_state_for_draw = app_state.clone();
        drawing_area.set_draw_func(move |_, cr, width, height| {
            let state = app_state_for_draw.lock().unwrap();

            let width = width as f64;
            let height = height as f64;

            // Clear the drawing area
            cr.set_source_rgb(0.0, 0.0, 0.0); // Black background
            cr.paint().expect("Failed to paint background");

            // Define chart area with padding
            let padding_x = 50.0;
            let padding_y = 30.0;
            let chart_width = width - 2.0 * padding_x;
            let chart_height = height - 2.0 * padding_y;

            if chart_width <= 0.0 || chart_height <= 0.0 {
                return; // Not enough space to draw
            }

            // Transform data to be relative to the latest point (which is at chart_width)
            let raw_data_original: Vec<(f64, f64)> = state.data_points.iter().cloned().collect();
            let latest_x = raw_data_original.last().map(|(x, _)| *x).unwrap_or(0.0);

            let raw_data: Vec<(f64, f64)> = raw_data_original.iter()
                .map(|(x, y)| (x - latest_x + chart_width, *y))
                .collect();

            // Calculate symmetric Y bounds (0 in middle)
            let max_abs_y = raw_data_original.iter()
                .map(|(_, y)| y.abs())
                .fold(0.0, f64::max);

            let y_limit = if max_abs_y == 0.0 { 10.0 } else { max_abs_y * 1.1 };
            let y_bounds = [-y_limit, y_limit];

            // Scaling factors
            let scale_y = chart_height / (y_bounds[1] - y_bounds[0]);

            // Helper to transform user coordinates to screen coordinates
            let to_screen = |x: f64, y: f64| {
                (padding_x + x, padding_y + chart_height / 2.0 - y * scale_y)
            };

            // Draw X-axis
            cr.set_source_rgb(0.5, 0.5, 0.5); // Gray
            cr.set_line_width(1.0);
            let (ax1, ay1) = to_screen(0.0, 0.0);
            let (ax2, ay2) = to_screen(chart_width, 0.0);
            cr.move_to(ax1, ay1);
            cr.line_to(ax2, ay2);
            cr.stroke().expect("Failed to draw X-axis");

            // Draw Y-axis (on the left)
            cr.set_source_rgb(0.5, 0.5, 0.5); // Gray
            let (ay_start_x, ay_start_y) = to_screen(0.0, y_bounds[0]);
            let (ay_end_x, ay_end_y) = to_screen(0.0, y_bounds[1]);
            cr.move_to(ay_start_x, ay_start_y);
            cr.line_to(ay_end_x, ay_end_y);
            cr.stroke().expect("Failed to draw Y-axis");

            // Draw data points as a continuous line with color transitions
            if raw_data.len() >= 2 {
                cr.set_line_width(2.0);
                
                for i in 0..raw_data.len() - 1 {
                    let (x1, y1) = raw_data[i];
                    let (x2, y2) = raw_data[i+1];
                    let (sx1, sy1) = to_screen(x1, y1);
                    let (sx2, sy2) = to_screen(x2, y2);
                    
                    if (y1 >= 0.0) == (y2 >= 0.0) {
                        // Same sign
                        if y1 >= 0.0 { cr.set_source_rgb(1.0, 0.0, 0.0); } else { cr.set_source_rgb(0.0, 1.0, 0.0); }
                        cr.move_to(sx1, sy1);
                        cr.line_to(sx2, sy2);
                        cr.stroke().expect("Failed to stroke");
                    } else {
                        // Crossing the X-axis
                        let xi = x1 + (0.0 - y1) * (x2 - x1) / (y2 - y1);
                        let (sxi, syi) = to_screen(xi, 0.0);
                        
                        // First segment
                        if y1 >= 0.0 { cr.set_source_rgb(1.0, 0.0, 0.0); } else { cr.set_source_rgb(0.0, 1.0, 0.0); }
                        cr.move_to(sx1, sy1);
                        cr.line_to(sxi, syi);
                        cr.stroke().expect("Failed to stroke");
                        
                        // Second segment
                        if y2 >= 0.0 { cr.set_source_rgb(1.0, 0.0, 0.0); } else { cr.set_source_rgb(0.0, 1.0, 0.0); }
                        cr.move_to(sxi, syi);
                        cr.line_to(sx2, sy2);
                        cr.stroke().expect("Failed to stroke");
                    }
                }
            }

            // Draw axis labels
            cr.set_source_rgb(1.0, 1.0, 1.0); // White text
            cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
            cr.set_font_size(12.0);

            // X-axis labels
            cr.move_to(padding_x, height - padding_y + 15.0);
            cr.show_text(&format!("{:.0}s", -chart_width)).expect("Failed to draw X-axis label"); // Start time

            cr.move_to(width - padding_x - 10.0, height - padding_y + 15.0);
            cr.show_text("0s").expect("Failed to draw X-axis label"); // Current time

            // X-axis title
            let x_title = "Time (s)";
            let x_extents = cr.text_extents(x_title).expect("Failed to get text extents");
            cr.move_to(padding_x + (chart_width - x_extents.width()) / 2.0, height - 5.0);
            cr.show_text(x_title).expect("Failed to draw X-axis title");

            // Y-axis labels
            cr.move_to(padding_x - 40.0, padding_y);
            cr.show_text(&format!("{:.0}W", y_bounds[1])).expect("Failed to draw Y-axis label"); // Max power

            cr.move_to(padding_x - 40.0, padding_y + chart_height / 2.0 + 5.0);
            cr.show_text("0W").expect("Failed to draw Y-axis label"); // Zero power

            cr.move_to(padding_x - 40.0, height - padding_y - 10.0);
            cr.show_text(&format!("{:.0}W", y_bounds[0])).expect("Failed to draw Y-axis label"); // Min power

            // Y-axis title
            let y_title = "Power (W)";
            cr.move_to(padding_x, padding_y - 10.0);
            cr.show_text(y_title).expect("Failed to draw Y-axis title");
        });

        // Periodic UI update via glib timeout (runs on GTK main thread)
        let app_state_for_timeout = app_state.clone();
        glib::timeout_add_local(Duration::from_secs(1), move || {
            let state = app_state_for_timeout.lock().unwrap();
            if let Some(err) = &state.last_error {
                status_label.set_label(&format!("Error: {}", err));
            } else if let Some(val) = state.last_value {
                // Calculate stats
                let values: Vec<f64> = state.data_points.iter().map(|(_, v)| *v).collect();
                let min = values.iter().fold(f64::INFINITY, |a, &b| a.min(b));
                let max = values.iter().fold(f64::NEG_INFINITY, |a, &b| a.max(b));
                let avg = if !values.is_empty() {
                    values.iter().sum::<f64>() / values.len() as f64
                } else {
                    0.0
                };

                let color = if val > 0.0 { "#FF0000" } else { "#00FF00" };
                status_label.set_markup(&format!(
                    "<span foreground='{}'>Current: {:4.0} W | Min: {:4.0} W | Max: {:4.0} W | Avg: {:4.0} W | Sample window: {:3.0} min</span>",
                    color, val, min, max, avg, state.data_points.len() / 60
                ));
            }

            drawing_area.queue_draw();
            glib::ControlFlow::Continue
        });
    });

    application.run_with_args(&[std::env::args().next().unwrap_or_else(|| "P1Graph".to_string())]);
    Ok(())
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<Stdout>>, api_url: String) -> Result<()> {
    let state = Arc::new(Mutex::new(AppState {
        data_points: VecDeque::with_capacity(BUF_SIZE),
        last_error: None,
        last_value: None,
        next_x: 0.0,
    }));

    spawn_data_fetcher(api_url, state.clone());

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
                if key.kind == KeyEventKind::Press && (key.code == KeyCode::Char('q') || key.code == KeyCode::Esc) {
                    return Ok(());
                }
            }
        }
    }
}