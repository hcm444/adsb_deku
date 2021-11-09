//! This tui program displays the current ADS-B detected airplanes on a plot with your current
//! position as (0,0) and has the ability to show different information about aircraft locations
//! and testing your coverage.
//!
//! # Tabs
//!
//! ## ADSB
//!
//! Regular display of recently observed aircraft on a lat/long plot
//!
//! ## Coverage
//!
//! Instead of only showing current airplanes, only plot dots for a seen airplane location
//!
//! Instead of using a `HashMap` for only storing an aircraft position for each aircraft, store
//! all aircrafts and only display a dot where detection at the lat/long position. This is for
//! testing the reach of your antenna.
//!
//! ## Airplanes
//!
//! Display all information gathered from observed aircraft

use std::io::{self, BufRead, BufReader};
use std::net::TcpStream;
use std::num::ParseFloatError;
use std::str::FromStr;
use std::time::Duration;

use adsb_deku::adsb::ME;
use adsb_deku::cpr::Position;
use adsb_deku::deku::DekuContainerRead;
use adsb_deku::{Frame, DF};
use apps::Airplanes;
use clap::Parser;
use crossterm::event::{poll, read, Event, KeyCode, KeyEvent};
use crossterm::terminal::enable_raw_mode;
use rayon::prelude::*;
use tui::backend::{Backend, CrosstermBackend};
use tui::layout::{Constraint, Direction, Layout, Rect};
use tui::style::{Color, Modifier, Style};
use tui::symbols::DOT;
use tui::text::{Span, Spans};
use tui::widgets::canvas::{Canvas, Line, Points};
use tui::widgets::{Block, Borders, Row, Table, TableState, Tabs};
use tui::Terminal;

/// Amount of zoom out from your original lat/long position
const MAX_PLOT_HIGH: f64 = 400.0;
const MAX_PLOT_LOW: f64 = MAX_PLOT_HIGH * -1.0;

#[derive(Clone)]
pub struct City {
    name: String,
    lat: f64,
    long: f64,
}

impl FromStr for City {
    type Err = ParseFloatError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let coords: Vec<&str> = s
            .trim_matches(|p| p == '(' || p == ')')
            .split(',')
            .collect();

        let lat_fromstr = coords[1].parse::<f64>()?;
        let long_fromstr = coords[2].parse::<f64>()?;

        Ok(Self {
            name: coords[0].to_string(),
            lat: lat_fromstr,
            long: long_fromstr,
        })
    }
}

#[derive(Parser)]
#[clap(
    version,
    name = "radar",
    author = "wcampbell0x2a",
    about = "TUI Display of ADS-B protocol info from demodulator"
)]
struct Opts {
    #[clap(long, default_value = "localhost")]
    host: String,
    #[clap(long, default_value = "30002")]
    port: u16,
    /// Antenna location latitude
    #[clap(long)]
    lat: f64,
    /// Antenna location longitude
    #[clap(long)]
    long: f64,
    /// Vector of cities [(name, lat, long),..]
    #[clap(long)]
    cities: Vec<City>,
    /// Disable output of latitude and longitude on display
    #[clap(long)]
    disable_lat_long: bool,
}

#[derive(Copy, Clone)]
enum Tab {
    Map       = 0,
    Coverage  = 1,
    Airplanes = 2,
}

impl Tab {
    pub fn next_tab(self) -> Self {
        match self {
            Self::Map => Self::Coverage,
            Self::Coverage => Self::Airplanes,
            Self::Airplanes => Self::Map,
        }
    }
}

/// After parsing from `Opts` contains more settings mutated in program
struct Settings {
    scale: f64,
    lat: f64,
    long: f64,
}

fn main() {
    let opts = Opts::parse();

    // Setup non-blocking TcpStream
    let stream = TcpStream::connect((opts.host.clone(), opts.port)).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_millis(50)))
        .unwrap();
    let mut reader = BufReader::new(stream);

    // empty containers
    let mut input = String::new();
    let mut coverage_airplanes = vec![];
    let mut adsb_airplanes = Airplanes::new();

    // setup tui params
    let stdout = io::stdout();
    let mut backend = CrosstermBackend::new(stdout);
    backend.clear().unwrap();
    let mut terminal = Terminal::new(backend).unwrap();
    enable_raw_mode().unwrap();

    // setup tui variables
    let mut tab_selection = Tab::Map;
    let mut quit = None;
    let original_scale = 1.2;
    let mut airplanes_state = TableState::default();

    let mut settings = Settings {
        scale: original_scale,
        lat: opts.lat,
        long: opts.long,
    };

    loop {
        if let Some(reason) = quit {
            terminal.clear().unwrap();
            println!("{}", reason);
            break;
        }
        input.clear();

        if let Ok(len) = reader.read_line(&mut input) {
            // a length of 0 would indicate a broken pipe/input, quit program
            if len == 0 {
                quit = Some("TCP connection aborted, quitting radar tui");
                continue;
            }
            // convert from string hex -> bytes
            let hex = &mut input.to_string()[1..len - 2].to_string();
            let bytes = if let Ok(bytes) = hex::decode(&hex) {
                bytes
            } else {
                continue;
            };

            // check for all 0's
            if bytes.par_iter().all(|&b| b == 0) {
                continue;
            }

            // decode
            if let Ok((_, frame)) = Frame::from_bytes((&bytes, 0)) {
                if let DF::ADSB(ref adsb) = frame.df {
                    adsb_airplanes.incr_messages(adsb.icao);
                    match &adsb.me {
                        ME::AircraftIdentification(identification) => {
                            adsb_airplanes.add_identification(adsb.icao, identification);
                        },
                        ME::AirborneVelocity(vel) => {
                            adsb_airplanes.add_airborne_velocity(adsb.icao, vel);
                        },
                        ME::AirbornePositionGNSSAltitude(altitude)
                        | ME::AirbornePositionBaroAltitude(altitude) => {
                            adsb_airplanes.add_altitude(adsb.icao, altitude);
                        },
                        _ => {},
                    };
                }
            }
        }

        // tui drawing

        // add lat_long to coverage vector
        //
        // TODO: use this data for all display of all airplanes current data instead of recomputing
        // this multiple times
        let all_lat_long = adsb_airplanes.all_lat_long_altitude();
        coverage_airplanes.extend(all_lat_long.clone());

        input.clear();
        // remove airplanes that timed-out
        adsb_airplanes.prune();

        terminal
            .draw(|f| {
                // create layout
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .margin(1)
                    .constraints([Constraint::Min(3), Constraint::Percentage(100)].as_ref())
                    .split(f.size());

                // render tabs
                let titles = ["Map", "Coverage", "Airplanes"]
                    .iter()
                    .copied()
                    .map(Spans::from)
                    .collect();
                let tab = Tabs::new(titles)
                    .block(
                        Block::default()
                            .title(format!("({},{})", settings.lat, settings.long))
                            .borders(Borders::ALL),
                    )
                    .style(Style::default().fg(Color::White))
                    .highlight_style(Style::default().fg(Color::Green))
                    .select(tab_selection as usize)
                    .divider(DOT);

                f.render_widget(tab, chunks[0]);

                // render the tab depending on the selection
                match tab_selection {
                    Tab::Map => build_tab_map(f, chunks, &settings, &opts, &adsb_airplanes),
                    Tab::Coverage => {
                        build_tab_coverage(f, chunks, &settings, &opts, &coverage_airplanes)
                    },
                    Tab::Airplanes => {
                        build_tab_airplanes(f, chunks, &adsb_airplanes, &mut airplanes_state)
                    },
                }
            })
            .unwrap();

        // handle keyboard events
        if poll(Duration::from_millis(10)).unwrap() {
            if let Event::Key(KeyEvent { code, .. }) = read().unwrap() {
                match (code, tab_selection) {
                    // All Tabs
                    (KeyCode::F(1), _) => tab_selection = Tab::Map,
                    (KeyCode::F(2), _) => tab_selection = Tab::Coverage,
                    (KeyCode::F(3), _) => tab_selection = Tab::Airplanes,
                    (KeyCode::Tab, _) => tab_selection = tab_selection.next_tab(),
                    (KeyCode::Char('q'), _) => quit = Some("user requested quit"),
                    (KeyCode::Char('-'), Tab::Map | Tab::Coverage) => settings.scale += 0.1,
                    (KeyCode::Char('+'), Tab::Map | Tab::Coverage) => {
                        if settings.scale > 0.2 {
                            settings.scale -= 0.1;
                        }
                    },
                    // Map and Coverage
                    (KeyCode::Up, Tab::Map | Tab::Coverage) => settings.lat += 0.005,
                    (KeyCode::Down, Tab::Map | Tab::Coverage) => settings.lat -= 0.005,
                    (KeyCode::Left, Tab::Map | Tab::Coverage) => settings.long -= 0.03,
                    (KeyCode::Right, Tab::Map | Tab::Coverage) => settings.long += 0.03,
                    (KeyCode::Enter, Tab::Map | Tab::Coverage) => {
                        settings.lat = opts.lat;
                        settings.long = opts.long;
                        settings.scale = original_scale;
                    },
                    // Airplanes
                    (KeyCode::Up, Tab::Airplanes) => {
                        if let Some(selected) = airplanes_state.selected() {
                            airplanes_state.select(Some(selected - 1));
                        } else {
                            airplanes_state.select(Some(0));
                        }
                    },
                    (KeyCode::Down, Tab::Airplanes) => {
                        if let Some(selected) = airplanes_state.selected() {
                            airplanes_state.select(Some(selected + 1));
                        } else {
                            airplanes_state.select(Some(0));
                        }
                    },
                    (KeyCode::Enter, Tab::Airplanes) => {
                        if let Some(selected) = airplanes_state.selected() {
                            let key = adsb_airplanes.0.keys().nth(selected).unwrap();
                            let pos = adsb_airplanes.lat_long_altitude(*key);
                            if let Some((position, _)) = pos {
                                settings.lat = position.latitude;
                                settings.long = position.longitude;
                                tab_selection = Tab::Map;
                            }
                        }
                    },
                    _ => (),
                }
            }
        }
    }
}

/// Render Map tab for tui display
fn build_tab_map<A: tui::backend::Backend>(
    f: &mut tui::Frame<A>,
    chunks: Vec<Rect>,
    settings: &Settings,
    opts: &Opts,
    adsb_airplanes: &Airplanes,
) {
    let canvas = Canvas::default()
        .block(Block::default().title("Map").borders(Borders::ALL))
        .x_bounds([MAX_PLOT_LOW, MAX_PLOT_HIGH])
        .y_bounds([MAX_PLOT_LOW, MAX_PLOT_HIGH])
        .paint(|ctx| {
            ctx.layer();

            let (lat_diff, long_diff) = scale_lat_long(settings.scale);

            // draw cities
            draw_cities(
                ctx,
                &opts.cities,
                settings.lat,
                settings.long,
                lat_diff,
                long_diff,
            );

            // draw ADSB tab airplanes
            for key in adsb_airplanes.0.keys() {
                let value = adsb_airplanes.lat_long_altitude(*key);
                if let Some((position, _altitude)) = value {
                    let lat = ((position.latitude - settings.lat) / lat_diff) * MAX_PLOT_HIGH;
                    let long = ((position.longitude - settings.long) / long_diff) * MAX_PLOT_HIGH;

                    // draw dot on location
                    ctx.draw(&Points {
                        coords: &[(long, lat)],
                        color: Color::White,
                    });

                    let name = if opts.disable_lat_long {
                        format!("{}", key).into_boxed_str()
                    } else {
                        format!("{} ({}, {})", key, position.latitude, position.longitude)
                            .into_boxed_str()
                    };

                    // draw plane ICAO name
                    ctx.print(
                        long + 3.0,
                        lat,
                        Span::styled(name.to_string(), Style::default().fg(Color::White)),
                    );
                }
            }

            draw_lines(ctx);
        });
    f.render_widget(canvas, chunks[1]);
}

/// Render Coverage tab for tui display
fn build_tab_coverage<A: tui::backend::Backend>(
    f: &mut tui::Frame<A>,
    chunks: Vec<Rect>,
    settings: &Settings,
    opts: &Opts,
    coverage_airplanes: &[Position],
) {
    let canvas = Canvas::default()
        .block(Block::default().title("Coverage").borders(Borders::ALL))
        .x_bounds([MAX_PLOT_LOW, MAX_PLOT_HIGH])
        .y_bounds([MAX_PLOT_LOW, MAX_PLOT_HIGH])
        .paint(|ctx| {
            ctx.layer();

            let (lat_diff, long_diff) = scale_lat_long(settings.scale);

            // draw cities
            draw_cities(
                ctx,
                &opts.cities,
                settings.lat,
                settings.long,
                lat_diff,
                long_diff,
            );

            // draw ADSB tab airplanes
            for position in coverage_airplanes.iter() {
                let lat = ((position.latitude - settings.lat) / lat_diff) * MAX_PLOT_HIGH;
                let long = ((position.longitude - settings.long) / long_diff) * MAX_PLOT_HIGH;

                // draw dot on location
                ctx.draw(&Points {
                    coords: &[(long, lat)],
                    color: Color::White,
                });
            }

            draw_lines(ctx);
        });
    f.render_widget(canvas, chunks[1]);
}

/// Render Airplanes tab for tui display
fn build_tab_airplanes<A: tui::backend::Backend>(
    f: &mut tui::Frame<A>,
    chunks: Vec<Rect>,
    adsb_airplanes: &Airplanes,
    airplanes_state: &mut TableState,
) {
    let mut rows = vec![];
    // make a vec of all strings to get a total amount of airplanes with
    // position information
    let empty = "".to_string();
    for key in adsb_airplanes.0.keys() {
        let state = adsb_airplanes.0.get(key).unwrap();
        let pos = adsb_airplanes.lat_long_altitude(*key);
        let mut lat = empty.clone();
        let mut lon = empty.clone();
        let mut alt = empty.clone();
        if let Some((position, altitude)) = pos {
            lat = format!("{}", position.latitude);
            lon = format!("{}", position.longitude);
            alt = format!("{}", altitude);
        }
        rows.push(Row::new(vec![
            format!("{}", key),
            state.callsign.as_ref().unwrap_or(&empty).clone(),
            lat,
            lon,
            format!("{:>8}", alt),
            state
                .vert_speed
                .map_or_else(|| "".to_string(), |v| format!("{:>6}", v)),
            state
                .speed
                .map_or_else(|| "".to_string(), |v| format!("{:>5.0}", v)),
            format!("{:>8}", state.num_messages),
        ]));
    }

    let rows_len = rows.len();

    // check the length of selected airplanes
    if let Some(selected) = airplanes_state.selected() {
        if selected > rows_len - 1 {
            airplanes_state.select(Some(rows_len - 1));
        }
    }

    // draw table
    let table = Table::new(rows)
        .style(Style::default().fg(Color::White))
        .header(
            Row::new(vec![
                "ICAO",
                "Call sign",
                "Longitude",
                "Latitude",
                "Altitude",
                "   FPM",
                "Speed",
                "    Msgs",
            ])
            .bottom_margin(1),
        )
        .block(
            Block::default()
                .title(format!("Airplanes({})", rows_len))
                .borders(Borders::ALL),
        )
        .widths(&[
            Constraint::Length(6),
            Constraint::Length(15),
            Constraint::Length(15),
            Constraint::Length(15),
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Length(5),
            Constraint::Length(8),
        ])
        .column_spacing(1)
        .highlight_style(Style::default().add_modifier(Modifier::BOLD))
        .highlight_symbol(">> ");
    f.render_stateful_widget(table, chunks[1], &mut airplanes_state.clone());
}

/// Draw vertical and horizontal lines
fn draw_lines(ctx: &mut tui::widgets::canvas::Context<'_>) {
    ctx.draw(&Line {
        x1: MAX_PLOT_HIGH,
        y1: 0.0,
        x2: MAX_PLOT_LOW,
        y2: 0.0,
        color: Color::White,
    });
    ctx.draw(&Line {
        x1: 0.0,
        y1: MAX_PLOT_HIGH,
        x2: 0.0,
        y2: MAX_PLOT_LOW,
        color: Color::White,
    });
}

/// Draw cities on the map
fn draw_cities(
    ctx: &mut tui::widgets::canvas::Context<'_>,
    cities: &[City],
    local_lat: f64,
    local_long: f64,
    lat_diff: f64,
    long_diff: f64,
) {
    for city in cities {
        let lat = ((city.lat - local_lat) / lat_diff) * MAX_PLOT_HIGH;
        let long = ((city.long - local_long) / long_diff) * MAX_PLOT_HIGH;

        // draw city coor
        ctx.draw(&Points {
            coords: &[(long, lat)],
            color: Color::Green,
        });

        // draw city name
        ctx.print(
            long + 3.0,
            lat,
            Span::styled(city.name.to_string(), Style::default().fg(Color::Green)),
        );
    }
}

fn scale_lat_long(scale: f64) -> (f64, f64) {
    // Difference between each lat point
    let lat_diff = scale;
    // Difference between each long point
    let long_diff = lat_diff * 3.0;

    (lat_diff, long_diff)
}
